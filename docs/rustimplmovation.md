# Rust Implementation Rationale — Replacing `bscpylgtvcommand`

## Context

The current daemons (both Python `tv_multiview_daemon.py.j2` and Rust
`tv-multiview/`) shell out to `bscpylgtvcommand` — a Python CLI tool — for
every TV operation. Every heartbeat, every input switch, every splitscreen
toggle spawns a new Python process that opens a fresh WebSocket to the TV,
authenticates, issues one command, and exits. At a 5-second heartbeat
interval, this burns ~28% CPU and adds 300-500ms latency per operation.

The goal: replace all subprocess calls with a persistent, in-process Rust
SSAP client that holds one wss:// connection open for the lifetime of the
daemon.

---

## Why Not Fork `lg-webos-client`?

The only existing pure-Rust SSAP crate (`kziemianek/lg-webos-client` on
crates.io) is not viable as a starting point:

| Factor | Forking it | Building from scratch |
|--------|-----------|----------------------|
| Age | v0.5.0, January 2023; zero activity since | Fresh |
| TLS | No TLS support — pins old `tokio-tungstenite` 0.18 with no `native-tls` or `rustls` feature. Your G4 TV requires `wss://` on port 3001. | `tokio-tungstenite` 0.24+ with `rustls` built in |
| Subscriptions | Architecture is one-shot request/response only — hardcodes `type: "request"`, resolves a `tokio::oneshot` channel per `id`, then discards. There is no `type: "subscribe"` path. Your daemon needs subscriptions for `multiViewStatus` push updates. | Designed for subscribe from the start with a callback dispatch table |
| Reconnect | No retry/backoff anywhere in `client.rs`. A dropped socket terminates the client object. | Your TLA+ reconnect state machine (`main.rs:69-111`) already exists and plugs in directly |
| Commands needed | `set_input`, `set_splitscreen`, `get_sw_info` — yes, the crate has these | Same 3 commands, ~10 lines each |
| Kept code | ~50 lines (the 25 command wrappers, most of which you never use) | ~200-300 total lines for everything |
| Transitive deps | 20+ stale crates inherited from old `tungstenite` ecosystem | 4 crates: `tokio-tungstenite`, `rustls`, `serde_json`, `tokio` |

**Bottom line**: the only reusable part of `lg-webos-client` is the
`set_input`/`switchInput` wrapper (~15 lines), and even that uses a
hardcoded `client-key` string that may not match your G4 firmware's pairing
handshake. Everything else — TLS, subscribe, reconnect — must be written
anyway. The fork path saves ~15 lines and costs 20+ stale dependencies.

---

## SSAP vs. Python Script Approach

### What the Python Script (`bscpylgtvcommand`) Does

Every time the daemon needs to talk to the TV:

```
bscpylgtvcommand 192.0.2.20 set_input HDMI_3
```

1. **Process launch**: Python interpreter starts, loads `bscpylgtv` package,
   imports `aiohttp`, `sqlite3`, `asyncio`.
2. **WebSocket connect**: Opens a new `wss://` connection to the TV port 3001.
   Full TLS handshake with the TV's self-signed certificate.
3. **SSAP handshake**: Sends a `register` message with pairing credentials
   read from `~/.config/lg-buddy/.aiopylgtv.sqlite`. Receives a `client-key`
   for this session.
4. **Single command**: Sends one JSON request (`ssap://tv/switchInput`).
   Waits for the JSON response.
5. **Disconnect**: WebSocket close + process exit.

Every call repeats steps 1-5 from scratch. At 5-second heartbeat intervals
+ on-demand switches, this adds up.

### What SSAP Is (the Protocol)

**Simple Service Access Protocol** — LG's name for the WebSocket-based JSON-RPC
layer that webOS TVs expose on `wss://{tv_ip}:3001/`.

It is **not** an alternative to `bscpylgtvcommand`. It is the protocol that
`bscpylgtvcommand` (and every other LG TV client library) speaks under the
hood.

Mechanically:

```
1. Open WebSocket to wss://TV_IP:3001/
2. Send register handshake:
   {"type":"register","id":"register_0",
    "payload":{"pairingType":"PROMPT",
               "manifest":{"permissions":[...]},
               "client-key":"<persisted>"}}
   ← server returns {"type":"registered","payload":{"client-key":"<key>"}}
3. Send commands:
   {"type":"request","id":"1","uri":"ssap://tv/switchInput",
    "payload":{"inputId":"HDMI_3"}}
   ← server returns {"type":"response","id":"1","payload":{"returnValue":true}}
4. Subscribe to push updates:
   {"type":"subscribe","id":"2","uri":"ssap://settings/getSystemSettings",
    "payload":{"category":"option","keys":["multiViewStatus"]}}
   ← server pushes {"type":"response","id":"2","payload":{...}} on changes
```

The protocol is ~40 lines of JSON format. It is not complex. Every LG TV
client library (Python `bscpylgtv`, `aiopylgtv`, JavaScript `lgtv2`, Go
`go-webos`, Rust `lg-webos-client`) is just a different language wrapper
around these same JSON messages.

### SSAP (persistent Rust) vs. Python Script (subprocess)

| | Python subprocess (`bscpylgtvcommand`) | Persistent Rust SSAP client |
|---|---|---|
| Connection | New WebSocket per command | One WebSocket for daemon lifetime |
| TLS handshake | Every call | Once at startup |
| SSAP register | Every call | Once at startup (client-key persisted) |
| Heartbeat cost | ~28% CPU (Python startup × 12/min) | ~0% (WebSocket ping handled by OS) |
| Latency per command | 300-500ms | <5ms (already on open socket) |
| Connection health | Discovered via 5s heartbeat + timeout | Immediate (socket error on next write) |
| Reconnect | Implicit (exit + systemd restart) | Explicit (your TLA+ `maintain_connection()` loop) |
| Pairing persistence | `.aiopylgtv.sqlite` read every call | `.aiopylgtv.sqlite` read once at connect time, client-key held in memory |

**The Python script approach is not wrong** — it works and it's battle-tested
in `bscpylgtv`. The cost is CPU and latency from process-per-command. For a
production daemon running 24/7 on a desktop, that overhead is worth
eliminating.

---

## Is the Python Script or `lg-webos-client` Useful for Our Rust Design?

### `bscpylgtv` (Python) — Useful as a Reference, Not as Code

**What's useful**:
- The SSAP command catalog: `bscpylgtv`'s source documents every known
  endpoint URI and payload format. This is the single best reference for what
  commands to implement.
- The register handshake payload: the exact `manifest` structure and
  `client-key` persistence pattern. Copy this verbatim.
- `aiopylgtv.sqlite` file format: your Rust client should read the same file
  (or a copy) so that pairing done once on the TV works for both tools.

**What's NOT useful**:
- The Python code itself cannot be used in Rust (different language).
- The process-per-command architecture is the thing we're replacing.
- The `asyncio`/`aiohttp` layer maps poorly to `tokio`/`tungstenite`.

**How to use it**: Read `bscpylgtv` source to extract:
1. Exact `register` JSON payload shape
2. URI strings for `switchInput`, `getCurrentSWInformation`, `setSystemSettings`
3. `client-key` storage format in sqlite

Then implement these in Rust from scratch.

### `lg-webos-client` (Rust) — Useful as a Reference, Not as a Base

**What's useful**:
- Confirms the Rust `tungstenite` ecosystem can speak SSAP.
- The `SwitchInput`/`SetInput` wrapper shows the exact `serde` struct shape
  for one command — useful as a template.
- The `client-key` handshake confirms the sequence.

**What's NOT useful as a base**:
- No TLS (can't connect to your G4).
- No subscribe capability.
- No reconnect.
- Stale dependencies (pinned to tungstenite 0.18, current is 0.24).
- The 25-command catalog is mostly things you don't need (toasts, 3D toggle,
  browser launch, media transport).

**How to use it**: Skim `src/client.rs` (~150 lines) to confirm the
connect→register→request sequence. Skim `src/commands/` to see the
`serde` struct pattern. Then write your own that:
1. Starts with `tokio_tungstenite::connect_async_tls_with_config` (TLS from
   day one).
2. Implements `type: "subscribe"` alongside `type: "request"`.
3. Plugs into your existing `maintain_connection()` reconnect loop.

---

## Verdict

The SSAP protocol is thin (~40 lines of JSON). The existing tools serve as
excellent references but not as code to build on. The right approach is a
from-scratch Rust client that holds a persistent `wss://` connection —
eliminating the Python process-spawn overhead entirely while keeping the
same protocol the TV already speaks.
