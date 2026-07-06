# Fullscreen / MultiView Switch Design for lan-mouse

## TLA+ State Model

```
---- MODULE TvDisplaySwitch ----
EXTENDS Naturals

\* The TV display can be in one of three modes
\* plus a disconnected/error state.
TVMode == { "fullscreen", "side_by_side", "transitioning", "dead" }

\* The active input (what the TV is showing in fullscreen)
ActiveInput == { "linux", "mac", "windows", "unknown" }

\* The cursor's current location
CursorLocation == { "linux", "mac", "windows", "edge" }

\* The lan-mouse capture state
CaptureState == { "idle", "capturing_linux", "capturing_mac", "capturing_windows" }

VARIABLES
    tv_mode,            \* current TV display mode
    tv_input,           \* what the TV is currently displaying
    cursor,             \* where the cursor physically is
    capture,            \* lan-mouse capture state
    daemon_healthy,     \* is the daemon connection alive
    pending_switch,     \* an input switch in flight
    reconnect_count     \* count of reconnect attempts

\* --- INVARIANTS ---

\* The TV must never be in an inconsistent state
TypeInvariant ==
    /\ tv_mode \in TVMode
    /\ tv_input \in ActiveInput
    /\ cursor \in CursorLocation
    /\ capture \in CaptureState
    /\ daemon_healthy \in BOOLEAN
    /\ reconnect_count \in 0..30

\* When in fullscreen, TV input must match the captured host
DisplayMatchesCursor ==
    (tv_mode = "fullscreen" /\ capture = "capturing_linux") => tv_input = "linux"
    /\ (tv_mode = "fullscreen" /\ capture = "capturing_mac") => tv_input = "mac"
    /\ (tv_mode = "fullscreen" /\ capture = "capturing_windows") => tv_input = "windows"

\* When daemon is dead, no commands can be in flight
NoPendingWhenDead ==
    (~daemon_healthy) => pending_switch = "none"

\* --- INITIAL STATE ---
Init ==
    /\ tv_mode = "fullscreen"
    /\ tv_input = "linux"
    /\ cursor = "linux"
    /\ capture = "idle"
    /\ daemon_healthy = TRUE
    /\ pending_switch = "none"
    /\ reconnect_count = 0

\* --- TRANSITIONS ---

\* F1 → F2: Cursor crosses edge, fullscreen → fullscreen (different input)
EnterOtherHost(host) ==
    LET target == CASE host = "mac" -> "mac"
                    [] host = "windows" -> "windows"
    IN
    /\ tv_mode = "fullscreen"
    /\ cursor \in {"linux", "edge"}
    /\ cursor' = target
    /\ tv_mode' = "fullscreen"
    /\ tv_input' = target
    /\ capture' = CASE host = "mac" -> "capturing_mac"
                   [] host = "windows" -> "capturing_windows"
    /\ pending_switch' = target
    /\ daemon_healthy' = daemon_healthy
    /\ UNCHANGED reconnect_count

\* F → S: User or hook requests side-by-side mode
EnterSideBySide ==
    /\ tv_mode = "fullscreen"
    /\ daemon_healthy
    /\ tv_mode' = "side_by_side"
    /\ pending_switch' = "sxs_on"
    /\ UNCHANGED <<tv_input, cursor, capture, reconnect_count>>

\* S → F: Exit side-by-side, return to fullscreen (the host being captured)
ExitSideBySide ==
    /\ tv_mode = "side_by_side"
    /\ daemon_healthy
    /\ tv_mode' = "fullscreen"
    \* Restore TV input to the host the cursor is capturing
    /\ tv_input' = CASE capture = "capturing_mac" -> "mac"
                     [] capture = "capturing_windows" -> "windows"
                     [] OTHER -> "linux"
    /\ pending_switch' = tv_input'
    /\ UNCHANGED <<cursor, capture, reconnect_count>>

\* Cursor crosses edge while in SXS mode: update capture state only,
\* do NOT switch TV input (it's showing multiple sources)
EnterSxsHost(host) ==
    LET target == CASE host = "mac" -> "mac"
                    [] host = "windows" -> "windows"
    IN
    /\ tv_mode = "side_by_side"
    /\ cursor' = target
    /\ capture' = CASE host = "mac" -> "capturing_mac"
                   [] host = "windows" -> "capturing_windows"
    /\ UNCHANGED <<tv_mode, tv_input, pending_switch, daemon_healthy, reconnect_count>>

\* Return to linux: cursor comes back to local machine
ReturnToLinux ==
    /\ cursor \in {"mac", "windows", "edge"}
    /\ cursor' = "linux"
    /\ capture' = "idle"
    /\ IF tv_mode = "fullscreen" THEN
           /\ tv_input' = "linux"
           /\ pending_switch' = "linux"
       ELSE
           /\ UNCHANGED <<tv_input, pending_switch>>
    /\ UNCHANGED <<tv_mode, daemon_healthy, reconnect_count>>

\* Daemon health transitions
DaemonDies ==
    /\ daemon_healthy
    /\ daemon_healthy' = FALSE
    /\ reconnect_count' = 0
    /\ tv_mode' = tv_mode  \* stale state, but preserved
    /\ UNCHANGED <<tv_input, cursor, capture, pending_switch>>

DaemonReconnects ==
    /\ ~daemon_healthy
    /\ reconnect_count < 30
    /\ daemon_healthy' = TRUE
    /\ reconnect_count' = 0
    \* Resync tv_mode from TV after reconnect
    /\ UNCHANGED <<tv_input, cursor, capture, pending_switch>>

\* Switch command completes
SwitchComplete ==
    /\ pending_switch /= "none"
    /\ daemon_healthy
    /\ pending_switch' = "none"
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, daemon_healthy, reconnect_count>>

\* TV manually changes mode (user pressed remote button)
TvRemoteOverride ==
    /\ tv_mode' \in TVMode \ {tv_mode}
    \* Resync input from TV subscription
    /\ tv_input' \in ActiveInput
    /\ UNCHANGED <<cursor, capture, daemon_healthy, pending_switch, reconnect_count>>

Next ==
    \/ \E host \in {"mac", "windows"} : EnterOtherHost(host)
    \/ \E host \in {"mac", "windows"} : EnterSxsHost(host)
    \/ EnterSideBySide
    \/ ExitSideBySide
    \/ ReturnToLinux
    \/ DaemonDies
    \/ DaemonReconnects
    \/ SwitchComplete
    \/ TvRemoteOverride

\* --- LIVENESS ---
\* Cursor should eventually return to linux after release bind
EventuallyReturn ==
    (capture /= "idle") ~> (cursor = "linux")

\* Daemon should eventually reconnect
EventuallyReconnect ==
    (~daemon_healthy) ~> (daemon_healthy)

\* --- DEADLOCK CHECK ---
\* Is there a state where no transition is possible?
\* Deadlock = ~ENABLED Next
\* Running model checker would find: daemon dead + cursor on remote host = stuck
\* Resolution: release bind always returns to linux, regardless of daemon state
=================================================================================
```

## Architecture Design

### Actors

```
┌─────────────┐     enter_hook      ┌──────────────┐     set_system_settings    ┌──────┐
│  lan-mouse  │ ──── curl ────────→ │ tv-multiview  │ ─── bscpylgtv/SSAP ────→ │  TV  │
│   (hub)     │                     │   daemon      │                           │      │
└─────────────┘                     │  (Python)     │ ←── subscribe callback ── │      │
                                    └──────────────┘                           └──────┘
                                           │
                                           │ HTTP API
                                           ▼
                                    ┌──────────────┐
                                    │  External     │
                                    │  triggers     │
                                    │ (sxs on/off)  │
                                    └──────────────┘
```

### API Endpoints

| Method | Path | Purpose | Returns |
|---|---|---|---|
| GET | `/enter/{target}` | lan-mouse enter_hook. Switch to `target` input if fullscreen | TV mode string |
| GET | `/sxs/on` | Force side-by-side mode on | TV mode string |
| GET | `/sxs/off` | Force side-by-side mode off (fullscreen) | TV mode string |
| GET | `/status` | Health + current state | JSON with mode, input, healthy, uptime |
| GET | `/health` | Liveness probe (always 200 if process alive) | `"ok"` |

### State Machine (Daemon Internal)

```
                    ┌──────────────────────────────────┐
                    │            DEAD                   │
                    │  daemon_healthy = false           │
                    │  HTTP responds with 503           │
                    └──────┬──────────────┬────────────┘
                           │ reconnect    │ disconnect
                           ▼              ▼
              ┌─────────────────────────────────────────┐
              │              ALIVE                       │
              │  ┌──────────┐   sxs/on    ┌───────────┐ │
              │  │FULLSCREEN│ ──────────→ │ SIDE_BY   │ │
              │  │          │ ←────────── │  _SIDE     │ │
              │  │ switch   │  sxs/off    │  skip      │ │
              │  │ on enter │             │  switch    │ │
              │  └──────────┘             └───────────┘ │
              │       │  ▲                      │  ▲    │
              │       ▼  │                      ▼  │    │
              │  ┌──────────────┐    ┌────────────────┐ │
              │  │TRANSITIONING │    │ SXS_TRANSITION │ │
              │  │ set_input    │    │ splitscreenEn. │ │
              │  │ pending      │    │ pending        │ │
              │  └──────────────┘    └────────────────┘ │
              └─────────────────────────────────────────┘
```

### Reliability Design

#### 1. Connection Lifecycle

```python
class TvDaemonState:
    healthy: bool                    # daemon connected to TV
    tv_mode: str                     # "fullscreen" | "sxs" | "unknown"
    last_mode_change: float          # epoch timestamp
    reconnect_count: int             # exponential backoff counter
    pending_switch: Optional[str]    # in-flight input switch target
    uptime: float                    # daemon process uptime
```

**Connect flow:**
1. Exponential backoff: 1s → 2s → 4s → 8s → 16s → 30s → 60s (cap)
2. After 5 consecutive failures: log warning
3. After 30 consecutive failures: log error, emit metric
4. On reconnect: resubscribe to `multiViewStatus`, reconcile state

**Disconnect detection:**
- `ping_interval=5` (WebSocket keepalive, 3 missed pings = disconnect)
- 30s heartbeat: `get_current_sw_info()` as fallback liveness check
- On disconnect detected: set `healthy=False`, all commands return 503

#### 2. Enter Hook Idempotency

```
curl → /enter/mac:
  1. If healthy=false → 503 "tv disconnected"
  2. If mode=sxs → 200 "sxs" (skip switch, don't change TV)
  3. If mode=fullscreen AND input already mac → 200 "fullscreen" (no-op)
  4. If mode=fullscreen AND input != mac → set_input(HDMI_3) → 200 "fullscreen"
```

#### 3. Observability

**Structured logging (JSON lines to stdout):**
```json
{"ts":"2026-07-07T12:00:00Z","event":"connect","tv_ip":"192.0.2.20","retry":0}
{"ts":"2026-07-07T12:00:01Z","event":"connected","tv_ip":"192.0.2.20"}
{"ts":"2026-07-07T12:00:05Z","event":"mode_change","from":"fullscreen","to":"sxs","source":"subscribe"}
{"ts":"2026-07-07T12:00:10Z","event":"enter","target":"mac","mode":"sxs","action":"skip"}
{"ts":"2026-07-07T12:00:15Z","event":"enter","target":"linux","mode":"fullscreen","action":"switch","input":"HDMI_4"}
{"ts":"2026-07-07T12:01:00Z","event":"disconnect","reason":"ping_timeout","uptime":3600}
{"ts":"2026-07-07T12:01:01Z","event":"reconnect","retry":1}
```

**Metrics (exposed via /status):**
```json
{
  "mode": "fullscreen",
  "input": "linux",
  "healthy": true,
  "uptime_seconds": 3600,
  "reconnect_count_total": 3,
  "switch_count": {"linux": 12, "mac": 8, "windows": 5},
  "last_error": null,
  "subscription_active": true
}
```

#### 4. systemd Integration

```
[Service]
ExecStart=/usr/bin/LG_Buddy_PIP/bin/python3 .../tv_multiview_daemon.py
WorkingDirectory=/usr/bin/LG_Buddy_PIP
Restart=on-failure
RestartSec=5

# Reliability tuning:
StartLimitIntervalSec=60     # don't restart forever in 60s
StartLimitBurst=10           # max 10 restarts in interval
```

**Signal handling:**
- SIGTERM: call `client.disconnect()`, close HTTP server, exit 0
- SIGUSR1: dump current state to stderr (debug trigger)

#### 5. Edge Cases

| Scenario | Behavior |
|---|---|
| TV off when daemon starts | Exponential backoff connect, HTTP returns 503 |
| TV reboot during capture | Daemon detects disconnect, reconnects, resubscribes |
| WiFi blip (silent disconnect) | 30s heartbeat detects, triggers reconnect |
| User presses remote (sxs on) | `subscribe` callback fires, state updates, future enters skip |
| User presses remote (sxs off → fullscreen) | Callback fires, state updates, next enter triggers input switch |
| Two rapid enters (debounce) | Second enter sees `pending_switch != None`, skips (200 + mode) |
| bscpylgtv library crash | Exception caught, daemon logs, systemd restarts |
| TV IP changes | Daemon crashes, systemd restarts, fails to connect, admin intervention needed |
| `splitscreenEnable` fails (unsupported firmware) | Log warning, return 500, degrade gracefully (only read `multiViewStatus`) |

### Transition Completeness Verification

```
States: fullscreen(F), side_by_side(S), transitioning(T), dead(D)
Valid transitions:
  F→F: enter other host (same mode, different input)
  F→T→F: enter other host, switch completes
  F→S: sxs/on request or remote
  F→D: daemon disconnect
  S→F: sxs/off request or remote
  S→S: enter other host (capture updates, no TV change)
  S→D: daemon disconnect
  D→F: daemon reconnect, resync from TV
  D→S: daemon reconnect while TV is in SXS mode
  T→F: switch completes
  T→D: daemon dies mid-switch (pending_switch lost, best-effort)

UNREACHABLE (by design):
  S→T→S: no input switch path from SXS mode
  D→T: daemon reconnects directly to stable state (F or S), never to transitioning

MISSING (design gap):
  No automatic SXS→F on return-to-linux when SXS was entered manually:
    - User manually enters SXS via remote while cursor is on mac
    - Cursor returns to linux
    - TV stays in SXS mode (showing linux + mac)
    - daemon's state says "sxs" but input should be linux fullscreen
    - Resolution: return-to-linux always calls sxs/off to restore fullscreen
```

## Implementation Plan

### Phase 0: Fix Critical Defects (current daemon)

| Task | Fix |
|---|---|
| P0 | Add `ping_interval=5` to `WebOsClient.create()` |
| P0 | Add 30s heartbeat + reconnect loop |
| P0 | Add structured JSON logging |
| P1 | Handle SIGTERM with `client.disconnect()` |
| P1 | Return 503 when `healthy=false` |
| P1 | Return 400 for invalid target |

### Phase 1: State Machine

| Task | Description |
|---|---|
| S1 | Define `TvDaemonState` dataclass with all fields |
| S2 | Implement `DaemonDies` / `DaemonReconnects` transitions |
| S3 | Implement `EnterOtherHost` / `EnterSxsHost` transitions |
| S4 | Implement debounce (skip if `pending_switch is not None`) |

### Phase 2: SXS Control

| Task | Description |
|---|---|
| X1 | Add `/sxs/on` endpoint → `set_system_settings("commercial", {"splitscreenEnable": "on"})` |
| X2 | Add `/sxs/off` endpoint → `set_system_settings("commercial", {"splitscreenEnable": "off"})` |
| X3 | Handle `splitscreenEnable` failure (unsupported firmware, log + degrade) |

### Phase 3: Observability

| Task | Description |
|---|---|
| O1 | Add `/status` endpoint (JSON metrics) |
| O2 | Add `/health` endpoint (liveness probe) |
| O3 | Structured JSON logging for all events |
| O4 | systemd `StartLimitIntervalSec` + `StartLimitBurst` |

### Phase 4: Integration

| Task | Description |
|---|---|
| I1 | Update ansible `tv-multiview.service.j2` with `StartLimitIntervalSec` |
| I2 | Update ansible `tv_multiview_daemon.py.j2` with full implementation |
| I3 | Update `enter_hook` in lan-mouse configs to use new endpoints |
| I4 | Add optional sxs trigger to `enter_hook` (e.g., `/enter/mac?sxs=1`) |

## Architecture Decision Records

### ADR-001: Separate daemon, not embedded in lan-mouse

**Decision:** Run tv-multiview as a standalone HTTP daemon, independent of lan-mouse.

**Rationale:**
- lan-mouse `enter_hook` is synchronous (`sh -c "curl ..."`), no async integration point
- TV state subscription needs a persistent WebSocket — lan-mouse is event-driven, not a daemon
- HTTP API is the simplest integration surface: any `enter_hook` on any OS can `curl` it
- Separate crash domain: daemon crash doesn't take down lan-mouse, systemd restarts the daemon

### ADR-002: LG_Buddy's venv, not our own

**Decision:** Use `/usr/bin/LG_Buddy_PIP` venv (shared with LG_Buddy), add only `aiohttp`.

**Rationale:**
- LG_Buddy already pairs with the TV — one pairing, one key file location
- `bscpylgtv` already installed; only `aiohttp` is missing
- Single update surface: upgrade LG_Buddy, run ansible to re-add `aiohttp`
- `WorkingDirectory` ensures `.aiopylgtv.sqlite` lands next to LG_Buddy's key

### ADR-003: SXS is opt-in, not automatic

**Decision:** Side-by-side mode is NOT auto-triggered by cursor crossing. It's a separate API call.

**Rationale:**
- `splitscreenEnable` toggles mode but can't select inputs — the input pair is manual
- Auto-triggering SXS on enter would disrupt the user's manual SXS configuration
- SXS mode is a user choice (remote), not a cursor-tracking side effect
- If the user wants auto-SXS, they configure it once via remote, then use `/sxs/on` to toggle
