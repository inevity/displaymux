# MultiView Daemon Implementation Plan

## TLA+ Refinement Mapping

```
GoalState == Phase4Complete
InitState == CurrentDaemon (62-line skeleton)

\* Sequence of actions transforming InitState → GoalState
Plan == [
    Phase0,   \* fix critical defects
    Phase1,   \* state machine
    Phase2,   \* multiview endpoints
    Phase3,   \* observability
    Phase4    \* integration (service template done, daemon rewrite)
]
```

## Phase 0: Fix Critical Defects

| Step | Action | TLA+ Transition |
|---|---|---|
| P0.1 | Add `ping_interval=5` to `WebOsClient.create()` | Enables `DaemonDies` detection |
| P0.2 | Add `maintain_connection()` loop with exponential backoff | Implements `ReconnectFails` → `DaemonReconnects` |
| P0.3 | Add `pending_switch` gate on `enter()` | Implements debounce guard on all transitions |
| P0.4 | Return 503 when `healthy=False` | Implements `NoPendingWhenDead` protocol |
| P0.5 | Return 400 for invalid target | Boundary validation |

## Phase 1: State Machine

| Step | Action |
|---|---|
| S1.1 | Define `TvDaemonState` dataclass (healthy, tv_mode, tv_input, pending_switch, reconnect_count, uptime) |
| S1.2 | `on_multiview_change()` updates `tv_mode` (handles `TvRemoteOverride`) |
| S1.3 | `enter()`: implement full decision tree (C1-C6 fixes applied) |
| S1.4 | `ReturnToLinux` path: route through transitioning with `pending_switch` |

## Phase 2: MultiView Endpoints

| Step | Action |
|---|---|
| X2.1 | Add `GET /multiview/on` → `set_system_settings("commercial", {"splitscreenEnable": "on"})` |
| X2.2 | Add `GET /multiview/off` → `set_system_settings("commercial", {"splitscreenEnable": "off"})` |
| X2.3 | Both toggle `tv_mode` atomically (no transitioning — single SSAP call) |

## Phase 3: Observability

| Step | Action |
|---|---|
| O3.1 | JSON-line structured logging for all events |
| O3.2 | `GET /status` endpoint (JSON metrics) |
| O3.3 | `GET /health` endpoint (liveness probe) |
| O3.4 | SIGTERM handler → `client.disconnect()` + clean shutdown |

## Phase 4: Integration

| Step | Action |
|---|---|
| I4.1 | Write complete `tv_multiview_daemon.py.j2` (this task) |
| I4.2 | (already done) `tv-multiview.service.j2` with `WorkingDirectory` + `StartLimit*` |

## Verification Gates

After each phase:
- [ ] No TLA+ invariant violations
- [ ] All edge cases from design doc handled
- [ ] `systemctl --user restart tv-multiview` works
- [ ] `curl localhost:8765/health` returns 200
