# MultiView Daemon — Rust Implementation Plan

## TLA+ Refinement Mapping to Rust

```
GoalState == TvMultiviewDaemon deployed as standalone Rust binary
InitState == No Rust implementation exists

Plan == [PlanPhase, CargoInit, StateModule, HttpModule, TvModule, MainLoop, BuildDeploy]
```

## Architecture

```
tv-multiview/
├── Cargo.toml
└── src/
    ├── main.rs          # tokio runtime, signal handling, service init
    ├── state.rs         # TvDaemonState struct + TLA+ transitions
    ├── http.rs          # axum routes: /enter, /multiview/*, /status, /health
    └── tv.rs            # bscpylgtvcommand subprocess wrapper (like LG_Buddy)
```

## TLA+ → Rust Mapping

| TLA+ Variable | Rust |
|---|---|
| `tv_mode` | `state.tv_mode: TvMode` |
| `tv_input` | `state.tv_input: Option<Input>` |
| `cursor` | Not modeled (external input) |
| `capture` | Not modeled (lan-mouse concern) |
| `daemon_healthy` | `state.healthy: bool` |
| `pending_switch` | `state.pending: Option<Input>` |
| `reconnect_count` | `state.reconnect_count: u32` |

| TLA+ Transition | Rust Method |
|---|---|
| `EnterOtherHost` | `state.enter_other_host(input)` |
| `SwitchComplete` | After `set_input` returns |
| `EnterMultiView` | `state.enter_multiview()` |
| `ExitMultiView` | `state.exit_multiview()` |
| `EnterMultiViewHost` | `state.enter_multiview_host()` |
| `ReturnToLinux` | `state.return_to_linux()` |
| `DaemonDies` | `state.mark_dead()` |
| `ReconnectFails` | `state.reconnect_failed()` |
| `DaemonReconnects` | `state.mark_healthy()` |
| `TvRemoteOverride` | `state.remote_override(mode)` |

## Dependencies

- `tokio` — async runtime
- `axum` — HTTP server
- `serde` / `serde_json` — JSON logging + /status endpoint
- `tracing` / `tracing-subscriber` — structured logging
- `thiserror` — error types
- `tokio::process` — spawn bscpylgtvcommand subprocess

No bscpylgtv Python dependency — the Rust binary shells out to
`/usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand` for TV operations,
exactly like LG_Buddy does. Shares the same `.aiopylgtv.sqlite`
key file via `WorkingDirectory` (set in systemd unit).

## TV Operations

All TV communication goes through `bscpylgtvcommand` subprocess:

```
set_input     → bscpylgtvcommand {ip} set_input HDMI_X
splitscreen   → bscpylgtvcommand {ip} set_system_settings commercial {"splitscreenEnable":"on"}
get_sw_info   → bscpylgtvcommand {ip} get_current_sw_info  (heartbeat)
subscribe     → bscpylgtvcommand ... get_system_settings option '["multiViewStatus"]'
```

Subscribe is the tricky one — `bscpylgtvcommand` is a one-shot CLI, but we need
a persistent subscription. Options:
1. Poll `multiViewStatus` every N seconds via periodic `bscpylgtvcommand` calls
2. Use the Python bscpylgtv library directly via PyO3 (adds Python dependency)
3. Implement WebOS SSAP WebSocket in Rust (complex)

Recommended: option 1 (poll). Poll every 5s. Simpler, no Python runtime needed,
and 5s latency on SXS mode detection is acceptable.

## Implementation Phases

### Phase 0: Cargo scaffold + state types

| Step | Action |
|---|---|
| R0.1 | `cargo init tv-multiview` in lan-mouse workspace |
| R0.2 | Define `TvMode`, `Input` enums, `TvDaemonState` struct |
| R0.3 | Implement `Default` for `TvDaemonState` (mirrors TLA+ `Init`) |
| R0.4 | Implement transition methods on `TvDaemonState` |

### Phase 1: TV subprocess module

| Step | Action |
|---|---|
| R1.1 | `tv.rs`: `BscpylgtvClient` wrapping `tokio::process::Command` |
| R1.2 | `set_input(ip, input)` method |
| R1.3 | `set_splitscreen(ip, enable)` method |
| R1.4 | `get_sw_info(ip)` method (heartbeat) |
| R1.5 | `poll_multiview_status(ip)` method |

### Phase 2: HTTP server

| Step | Action |
|---|---|
| R2.1 | `http.rs`: axum router with shared `Arc<RwLock<TvDaemonState>>` |
| R2.2 | `GET /health` → 200 |
| R2.3 | `GET /status` → JSON state |
| R2.4 | `GET /enter/{target}` → TLA+ enter logic |
| R2.5 | `GET /multiview/on` → `set_splitscreen(true)` |
| R2.6 | `GET /multiview/off` → `set_splitscreen(false)` |

### Phase 3: Main loop + connection health

| Step | Action |
|---|---|
| R3.1 | `main.rs`: tokio main, signal handlers |
| R3.2 | Background poll loop: poll multiViewStatus every 5s |
| R3.3 | Health tracking: heartbeat via `get_sw_info` every 30s |
| R3.4 | Reconnect logic with exponential backoff |
| R3.5 | `tracing` subscriber for JSON-line structured logging |

### Phase 4: Build + deploy

| Step | Action |
|---|---|
| R4.1 | `cargo build --release` produces `tv-multiview` binary |
| R4.2 | Systemd unit: ExecStart=/usr/local/bin/tv-multiview |
| R4.3 | Ansible integration: build from source or download pre-built |
