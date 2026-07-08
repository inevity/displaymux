# Stale TV Input Detection & Always-Availability Design

## 0. Problem: Stale `_tv_input` State

### Current Behavior

The daemon (`tv_multiview_daemon.py.j2` and the Rust `tv-multiview/`) tracks a
cached variable `_tv_input` that records what it *thinks* the TV is displaying.
When lan-mouse calls `/enter/{target}`, the handler checks:

```python
if _tv_mode == "fullscreen" and target == _tv_input:
    return "fullscreen"  # no-op, thinks we're already there
```

**The bug**: `_tv_input` can become stale. The daemon sets `_tv_input = "mac"`
when it sends `set_input(HDMI_3)`, assuming success. But the real TV state can
drift from the cached state via:

- User presses a button on the TV remote, switching input directly.
- TV reboot/power cycle resets input to a different port.
- `set_input()` succeeds at the WebOS level but the HDMI source has no signal
  (TV auto-switches to another active input, or shows "No Signal").
- Network blip causes `set_input()` to silently fail.

When `_tv_input` is stale, the daemon rejects `/enter/mac` with a no-op,
thinking the TV is already on macOS — but the display is actually on
something else. The user's cursor moves to the edge of the screen (lan-mouse
sees this as a valid transition), but the TV never switches.

### Rust Daemon (same issue)

The Rust `http.rs:enter()` has the same pattern (`state.rs:enter_other_host()`):

```rust
if *input == target {
    return false; // C6: no-op if already on target
}
```

---

## 1. Approach 1: Always Switch, Ignore Cached State

**Selected fix**: Remove the "already-on-target" no-op guard. Every `/enter/{target}`
request unconditionally issues `set_input()`, regardless of what the daemon
*cached* as `_tv_input`.

### Rationale

- `set_input()` is idempotent — sending "switch to HDMI_3" when already on
  HDMI_3 is harmless.
- It eliminates the single point of staleness: the daemon doesn't need to
  maintain a truthful `_tv_input` at all.
- No additional WebSocket API call needed (avoids adding latency and complexity).
- Simpler state machine: the daemon stops trying to be the authority on what
  the TV is doing.

### What Changes

**Python daemon** (`tv_multiview_daemon.py.j2`):
- Remove lines 96-98 (the `if target == _tv_input: return "fullscreen"` guard).
- `_tv_input` becomes informational only (for `/status` observability), not a
  gating condition.

**Rust daemon** (`state.rs:enter_other_host()`):
- Remove lines 91-93 (the `if *input == target { return false; }` guard).
- Same effect: `tv_input` remains tracked for `/status` but doesn't gate
  the switch.

### Status

**Not yet implemented.** This document records the decision; implementation
will follow separately.

---

## 2. Edge-Case Design: Always-Availability

### 2a. What if the screen has switched to macOS, but the display shows "No Signal"?

*Scenario*: lan-mouse triggers `/enter/mac`. The daemon calls
`set_input(HDMI_3)`. WebOS acknowledges the switch. But the macOS machine is:
- Asleep (display off).
- Powered off.
- DisplayPort/HDMI cable unplugged or loose.
- GPU output not initialized (e.g., during boot).

The TV receives no HDMI signal on HDMI_3 and shows a "No Signal" banner or
black screen.

**Current behavior (both daemons)**: The daemon marks the switch as successful.
The user sees a black/"No Signal" screen and is stuck — lan-mouse can't move
the cursor back because the screen edge is now unreachable.

#### Decision: Revert to Linux on No-Signal Detection

The daemon should detect that the selected source is unusable and
**automatically revert to Linux** (the always-available host).

Detection options:

1. **WebSocket API query for input signal status**: The LG WebOS SSAP API
   exposes signal/present status per HDMI port. Query after each switch.
   - Pro: Authoritative, TV-reported.
   - Con: Adds another SSAP round-trip (but can be done in parallel with the
     switch confirmation).

2. **Heartbeat-based**: After switching to a remote host, start a short timer
   (e.g., 3s). If the remote host's lan-mouse spoke doesn't acknowledge
   cursor arrival within the window, assume the display is dead and revert.
   - Pro: No additional TV API calls.
   - Con: False positives if the network is slow.

3. **Combined**: Try WebSocket signal-status query first; fall back to
   heartbeat timeout if the query is unavailable.

**Recommended: option 1 (WebSocket signal status)**. The LG TV's API reports
per-input signal presence. After `set_input(HDMI_3)`, query
`get_system_info` or the equivalent input-signal endpoint. If "no signal,"
immediately revert to `set_input(HDMI_4)` (Linux).

Revert sequence:
```
/enter/mac → set_input(HDMI_3) → query_input_signal(HDMI_3)
  ├─ signal=present → stay on macOS, normal operation
  └─ signal=absent  → set_input(HDMI_4), log warning, return "fullscreen" (linux)
```

### 2b. Why does the routine `get_software_info` call use 28% CPU?

The Rust daemon (`main.rs:98`) calls `client.get_sw_info()` every 5 seconds
as a heartbeat:

```rust
loop {
    tokio::time::sleep(Duration::from_secs(5)).await;
    if let Err(e) = client.get_sw_info().await { break; }
}
```

And `TvClient::get_sw_info()` (`tv.rs:72`) shells out:

```rust
Command::new("/usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand")
    .arg(self.ip.to_string())
    .args(&["get_software_info"])
    .output()
    .await
```

**Why this is expensive (5s interval × 12 calls/min × 720 calls/hr):**

| Step | Cost |
|------|------|
| Python interpreter startup | ~50-100ms CPU, loads libpython + modules |
| `bscpylgtv` import & init | Loads `aiohttp`, `sqlite3`, WebSocket client, ~150ms CPU |
| Open WebSocket to TV | TCP handshake + TLS + WebSocket upgrade, ~20ms |
| SSAP handshake + auth | Read `.aiopylgtv.sqlite`, send registration, ~30ms |
| `get_software_info` call | SSAP request/response: TV sends full firmware version, model, serial, all installed apps — **large payload**, ~50-200ms |
| Process teardown | Python GC, OS cleanup, ~10ms |

**Total per call**: ~300-500ms of wall-clock time, ~200-400ms CPU time.
At one call every 5 seconds, that's roughly **4-8% of one core on average**.
On a system with other workloads, the observed 28% suggests either:
- Contention with other Python processes using the same venv.
- The `get_software_info` response payload is unusually large (all installed
  apps listed), and JSON deserialization dominates.
- The Python process is also doing I/O wait (network to TV) which inflates
  the process's reported CPU time.

#### Fix: Use a lightweight heartbeat instead

Replace `get_software_info` with any of:

1. **Persistent WebSocket + ping**: Keep the WebSocket connection alive
   (already done in the Python daemon). Send a WebSocket-level ping frame
   (zero CPU, handled at TCP/websocket library level).
   - Pros: Near-zero cost. The WebSocket library handles keepalive.
   - Cons: Requires the daemon to hold a persistent connection (the Rust
     daemon currently doesn't — it shells out for every operation).

2. **Lighter SSAP call**: Use `get_system_info` with restricted keys or
   `get_power_state` instead of `get_software_info`, which returns only
   a few fields vs. the full app catalog.
   - Example: `/usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand 192.0.2.20 get_power_state`
     returns `{"state": "Active Screen On"}` in a few bytes.

3. **TCP connect-only probe**: Just check if the TV's port 3001 (WebOS WS)
   is reachable with a TCP SYN, no SSAP at all:
   ```bash
   timeout 1 bash -c "echo >/dev/tcp/192.0.2.20/3001" 2>/dev/null
   ```
   - Pro: Sub-millisecond, no Python at all.
   - Con: Confirms network reachability, not WebOS readiness. Still an
     improvement over the current approach.

4. **Switch to persistent WebSocket daemon**: The Python daemon already
   holds a persistent connection. The Rust daemon should do the same
   (use `bscpylgtv` as a library, not subprocess). One WebSocket connect
   at startup, then all operations reuse it. Heartbeat is just a library-level
   ping.

**Recommended**: Start with option 2 (lighter SSAP call) as it's a one-line
change in `tv.rs`. Then migrate to option 4 (persistent WebSocket) as a
proper fix.

### 2c. If I switch to macOS, then power off the Mac — what happens?

*Scenario*: User switches to macOS via lan-mouse. Later, physically powers
off the Mac (or it crashes). The TV stays on HDMI_3 showing "No Signal" or
black.

**Current behavior**: The daemon has no awareness that the Mac is offline.
The user is stuck on a dead display. lan-mouse's cursor is trapped on the
right edge (believing it's on macOS). Manual TV remote intervention is needed
to switch back to Linux.

#### Decision: Auto-Revert When Remote Host Disappears

When the daemon detects that the currently-selected remote host is
unreachable, it must **automatically switch back to Linux**.

Detection approaches:

1. **lan-mouse spoke health check**: After switching to macOS, the daemon
   starts monitoring whether the Mac's lan-mouse spoke is still connected
   to the hub. If the spoke disconnects (Mac powered off, lan-mouse process
   dies), trigger a revert.

2. **HDMI signal status** (as in 2a): Query the TV's per-port signal
   presence. If HDMI_3 shows "no signal" for >5 consecutive seconds, revert.

3. **SSH ping**: The daemon (or a companion script) pings the Mac via SSH.
   If unreachable for >10s, revert.

4. **ARP/WoL-based**: Check if the Mac's IP is responding to ARP. If not,
   attempt a WoL wake. If still no response after timeout, revert.

**Recommended approach**: Use **HDMI signal status as primary** (same
mechanism as 2a). Supplement with **lan-mouse spoke connectivity** as a
faster secondary signal (the spoke disconnects immediately on macOS
shutdown). Combined logic:

```
After switch to macOS:
  ┌─ Periodic check (every 3s for the first 30s):
  │   ├─ Is macOS lan-mouse spoke connected? → if yes: normal, stop checking
  │   ├─ Does HDMI_3 have signal? → if yes: normal, stop checking
  │   └─ Both fail for 2 consecutive checks → revert to Linux
  └─ After 30s of stable operation: stop active monitoring.
     └─ Passive: if spoke disconnects later (Mac crash), revert.
```

This ensures:
- If the Mac was already dead at switch time → immediate revert.
- If the Mac dies later → revert within 3-6 seconds.
- No overhead during normal operation once the connection is stable.

---

## Summary of Changes Needed

| # | Change | Priority | File(s) |
|---|--------|----------|---------|
| 1 | Remove no-op guard (always switch) | P0 | `state.rs:91-93`, `tv_multiview_daemon.py.j2:96-98` |
| 2 | Add signal-status verification after switch | P0 | `tv.rs` (new `get_input_signal()`), `http.rs` (revert logic) |
| 3 | Replace `get_software_info` with lighter heartbeat | P1 | `tv.rs:get_sw_info()` → lighter call or persistent connection |
| 4 | Auto-revert to Linux when remote host disappears | P0 | `main.rs` (monitoring loop), `http.rs` (revert on disconnect) |
| 5 | Switch back to Linux on remote host power-off | P0 | Same as #4 |

---
