# Fullscreen / MultiView Switch Design for lan-mouse

## Scope and Terminology

**multiView** is the umbrella term used throughout this document. It covers
all WebOS split-layout modes: side-by-side (two panes, left/right),
picture-in-picture (PIP, small overlay), and any future multiView layout the
TV firmware supports.

The LG WebOS API exposes a single boolean:
`multiViewStatus: "on" | "off"`. There is no API distinction between
side-by-side and PIP — they are the same state as far as programmatic
control is concerned. The TV remembers which layout the user last selected
via the remote and uses it whenever `splitscreenEnable` is toggled on.

Implication: the daemon's behavior is identical whether the user chose
side-by-side or PIP. When `multiViewStatus == "on"`, input switching is
suppressed regardless of layout geometry.

## TLA+ State Model

```
---- MODULE TvDisplaySwitch ----
EXTENDS Naturals

\* The TV display mode (what the panel is actually showing).
\* "multiview" covers both side-by-side and picture-in-picture —
\* the TV API reports multiViewStatus: "on" for both.
TVMode == { "fullscreen", "multiview", "transitioning" }

\* The active input (what the TV is displaying in fullscreen)
ActiveInput == { "linux", "mac", "windows", "unknown" }

\* The cursor's current location
CursorLocation == { "linux", "mac", "windows", "edge" }

\* The lan-mouse capture state
CaptureState == { "idle", "capturing_linux", "capturing_mac", "capturing_windows" }

\* Values for pending_switch
PendingValues == {"none", "multiview_on"} \cup ActiveInput

VARIABLES
    tv_mode,            \* current TV display mode
    tv_input,           \* what the TV is currently displaying
    cursor,             \* where the cursor physically is
    capture,            \* lan-mouse capture state
    daemon_healthy,     \* is the daemon connection alive
    pending_switch,     \* an input switch or multiView toggle in flight
    reconnect_count     \* count of failed reconnect attempts

\* --- INVARIANTS ---

TypeInvariant ==
    /\ tv_mode \in TVMode
    /\ tv_input \in ActiveInput
    /\ cursor \in CursorLocation
    /\ capture \in CaptureState
    /\ daemon_healthy \in BOOLEAN
    /\ pending_switch \in PendingValues
    /\ reconnect_count \in 0..30

\* When in fullscreen, TV input must match the captured host.
\* When idle (cursor on linux), TV must show linux.
DisplayMatchesCursor ==
    (tv_mode = "fullscreen" /\ capture = "idle") => tv_input = "linux"
    /\ (tv_mode = "fullscreen" /\ capture = "capturing_linux") => tv_input = "linux"
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

\* F→T: Cursor crosses edge, fullscreen → transitioning (different input).
\* The display update takes ~1-3s (HDCP/EDID relock).
\* pending_switch set to the target to gate rapid re-enters (debounce).
EnterOtherHost(host) ==
    LET target == CASE host = "mac" -> "mac"
                    [] host = "windows" -> "windows"
    IN
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"        \* debounce: don't fire if switch already in flight
    /\ cursor \in {"linux", "edge"}
    /\ daemon_healthy
    /\ tv_mode' = "transitioning"
    /\ tv_input' = target              \* destination known, TV processing async
    /\ cursor' = target
    /\ capture' = CASE host = "mac" -> "capturing_mac"
                   [] host = "windows" -> "capturing_windows"
    /\ pending_switch' = target
    /\ UNCHANGED <<daemon_healthy, reconnect_count>>

\* T→F: Switch completes (TV displays the new input, HDCP settled).
SwitchComplete ==
    /\ tv_mode = "transitioning"
    /\ tv_mode' = "fullscreen"
    /\ pending_switch' = "none"
    /\ UNCHANGED <<tv_input, cursor, capture, daemon_healthy, reconnect_count>>

\* F→M: User or hook enables multiView (side-by-side or PIP).
\* Pending gate prevents overlapping toggles.
EnterMultiView ==
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"
    /\ daemon_healthy
    /\ tv_mode' = "multiview"
    /\ pending_switch' = "multiview_on"
    /\ UNCHANGED <<tv_input, cursor, capture, reconnect_count>>

\* M→F: Exit multiView (side-by-side or PIP), return to fullscreen.
\* Restore TV input to the host the cursor is currently capturing.
ExitMultiView ==
    /\ tv_mode = "multiview"
    /\ pending_switch = "none"
    /\ daemon_healthy
    /\ tv_mode' = "fullscreen"
    /\ tv_input' = CASE capture = "capturing_mac" -> "mac"
                     [] capture = "capturing_windows" -> "windows"
                     [] OTHER -> "linux"
    /\ pending_switch' = tv_input'
    /\ UNCHANGED <<cursor, capture, reconnect_count>>

\* Cursor crosses edge while in multiView mode: update capture state only,
\* do NOT switch TV input (it's showing multiple sources already).
EnterMultiViewHost(host) ==
    LET target == CASE host = "mac" -> "mac"
                    [] host = "windows" -> "windows"
    IN
    /\ tv_mode = "multiview"
    /\ pending_switch = "none"
    /\ cursor' = target
    /\ capture' = CASE host = "mac" -> "capturing_mac"
                   [] host = "windows" -> "capturing_windows"
    /\ UNCHANGED <<tv_mode, tv_input, pending_switch, daemon_healthy, reconnect_count>>

\* Return to linux: cursor comes back to local machine.
\* In fullscreen: switch TV input back to linux.
\* In multiView: leave TV alone (multiView still active), just release capture.
ReturnToLinux ==
    /\ cursor \in {"mac", "windows", "edge"}
    /\ pending_switch = "none"
    /\ cursor' = "linux"
    /\ capture' = "idle"
    /\ IF tv_mode = "fullscreen" THEN
           /\ daemon_healthy
           /\ tv_input' = "linux"
           /\ pending_switch' = "linux"
       ELSE
           /\ UNCHANGED <<tv_input, pending_switch, daemon_healthy>>
    /\ UNCHANGED <<tv_mode, reconnect_count>>

\* Daemon health transitions.
\* If daemon dies mid-switch, clear pending_switch (lost, best-effort).
DaemonDies ==
    /\ daemon_healthy
    /\ daemon_healthy' = FALSE
    /\ pending_switch' = "none"      \* invariant: NoPendingWhenDead
    /\ reconnect_count' = 0
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture>>

ReconnectFails ==
    /\ ~daemon_healthy
    /\ reconnect_count < 30
    /\ reconnect_count' = reconnect_count + 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, daemon_healthy, pending_switch>>

DaemonReconnects ==
    /\ ~daemon_healthy
    /\ reconnect_count < 30
    /\ daemon_healthy' = TRUE
    /\ reconnect_count' = 0
    \* Can't know TV state during disconnect; resync from subscribe callback.
    /\ tv_mode' \in {"fullscreen", "multiview"}
    /\ tv_input' \in ActiveInput
    /\ UNCHANGED <<cursor, capture, pending_switch>>

\* TV manually changes mode (user pressed remote button).
\* Only possible if daemon is connected (subscribe callback is the source).
TvRemoteOverride ==
    /\ daemon_healthy
    /\ tv_mode' \in TVMode \ {tv_mode}
    \* Input may change independently of our capture state (design gap — see below).
    /\ tv_input' \in ActiveInput
    /\ UNCHANGED <<cursor, capture, daemon_healthy, pending_switch, reconnect_count>>

Next ==
    \/ \E host \in {"mac", "windows"} : EnterOtherHost(host)
    \/ \E host \in {"mac", "windows"} : EnterMultiViewHost(host)
    \/ EnterMultiView
    \/ ExitMultiView
    \/ ReturnToLinux
    \/ SwitchComplete
    \/ DaemonDies
    \/ ReconnectFails
    \/ DaemonReconnects
    \/ TvRemoteOverride

Spec == Init /\ [][Next]_<<tv_mode, tv_input, cursor, capture, daemon_healthy, pending_switch, reconnect_count>>

\* --- LIVENESS ---
\* Cursor should eventually return to linux after release bind
EventuallyReturn ==
    (capture /= "idle") ~> (cursor = "linux")

\* Daemon should eventually reconnect after disconnect
EventuallyReconnect ==
    (~daemon_healthy) ~> (daemon_healthy)

\* --- DESIGN GAPS (discovered by model) ---

\* GAP 1 (multiView → return to linux): If multiView (SXS or PIP) was
\* entered manually via remote, returning to linux leaves TV in multiView
\* mode. The user sees the multiView layout with linux already present,
\* which may be acceptable. To return to fullscreen, the user must
\* manually exit multiView via remote or call /multiview/off.

\* GAP 2 (remote input override): TvRemoteOverride allows tv_input' to
\* be anything, even if it contradicts capture state (e.g., cursor on mac
\* but remote switches TV to windows). DisplayMatchesCursor breaks.
\* Resolution: the subscribe callback fires, daemon learns the new input,
\* but capture state doesn't auto-correct — the user's cursor remains on
\* mac while the display shows windows. This is a real-world gap: the
\* daemon cannot force capture to follow a manual TV input change.

=================================================================================
```

## Architecture Design

### Actors

```
┌─────────────┐     enter_hook      ┌──────────────┐    bscpylgtv library     ┌──────┐
│  lan-mouse  │ ──── curl ────────→ │ tv-multiview  │ ──── WebOS SSAP ──────→ │  TV  │
│   (hub)     │                     │   daemon      │                          │      │
└─────────────┘                     │  (Python)     │ ←── subscribe callback ── │      │
                                    └──────────────┘                          └──────┘
                                           │
                                           │ HTTP API (aiohttp)
                                           ▼
                                    ┌──────────────┐
                                    │  External     │
                                    │  triggers     │
                                    │ (multiView   │
                                    │  toggle)     │
                                    └──────────────┘

Note: LG_Buddy (Rust) also talks to the TV via the same bscpylgtv Python
library — it spawns /usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand as a
subprocess per command. Our daemon imports bscpylgtv as a library and
maintains a persistent WebSocket. Both share the same .aiopylgtv.sqlite
key file at ~/.config/lg-buddy/ (set via WorkingDirectory).
```

### API Endpoints

| Method | Path | Purpose | Returns |
|---|---|---|---|
| GET | `/enter/{target}` | lan-mouse enter_hook. Switch to `target` if fullscreen and no pending switch | TV mode string |
| GET | `/multiview/on` | Enable multiView via `splitscreenEnable` toggle (commercial category, pending live-TV verification — see Phase 2/X3) | TV mode string |
| GET | `/multiview/off` | Disable multiView | TV mode string |
| GET | `/status` | Health + current state | JSON |
| GET | `/health` | Liveness probe (always 200 if process alive) | `"ok"` |

### State Machine (Daemon Internal)

```
                    ┌──────────────────────────────────┐
                    │            DISCONNECTED           │
                    │  daemon_healthy = false           │
                    │  HTTP: 503                        │
                    └──────┬──────────────┬────────────┘
                           │ reconnect    │ disconnect
                           ▼              ▼
              ┌─────────────────────────────────────────┐
              │              CONNECTED                   │
              │  ┌──────────┐   multiview   ┌──────────┐ │
              │  │FULLSCREEN│ ── on ──────→ │MULTIVIEW │ │
              │  │          │ ←── off ───── │(SXS/PIP) │ │
              │  │ enter→   │              │ enter→    │ │
              │  │ TRANS.   │              │ capture   │ │
              │  │  (async) │              │ only      │ │
              │  └──────────┘              └──────────┘ │
              └─────────────────────────────────────────┘
```

**Pending switch gating:** `pending_switch != "none"` blocks all new transitions
that would produce a TV command. This provides natural debounce: a second
rapid enter while a switch is in flight returns the current mode string
without issuing a new command.

### Reliability Design

#### 1. Connection Lifecycle

```python
class TvDaemonState:
    healthy: bool
    tv_mode: str              # "fullscreen" | "multiview" | "unknown"
    last_mode_change: float   # epoch
    reconnect_count: int      # exponential backoff: 1,2,4,8,16,30,60s cap
    pending_switch: Optional[str]  # in-flight command; blocks new commands
    uptime: float
```

**Connect flow:**
1. Load existing key from `~/.config/lg-buddy/.aiopylgtv.sqlite`
2. Exponential backoff: 1s → 2s → 4s → 8s → 16s → 30s → 60s (cap)
3. On connect: resubscribe to `multiViewStatus`
4. Max 30 retries (StartLimitBurst=10 per 60s in systemd)

**Disconnect detection:**
- `ping_interval=5` (WebSocket keepalive, 3 missed pings = disconnect)
- 30s heartbeat: `get_current_sw_info()` as fallback
- On disconnect: `healthy=False`, `pending_switch="none"`, all commands return 503

#### 2. Enter Hook Logic

```
curl → /enter/{target}:
  1. If healthy=false → 503
  2. If pending_switch != none → 200 current_mode  (debounce)
  3. If mode=multiview → 200 "multiview"           (skip, don't disturb multiView)
  4. If mode=fullscreen AND input == target → 200 "fullscreen"  (no-op)
  5. If mode=fullscreen AND input != target → set_input → 200 "fullscreen"
```

#### 3. Observability

**Structured JSON logging (stdout, one object per line):**
```json
{"ts":"...","event":"connect","ip":"192.0.2.20","retry":0}
{"ts":"...","event":"connected"}
{"ts":"...","event":"mode_change","from":"fullscreen","to":"multiview","source":"subscribe"}
{"ts":"...","event":"enter","target":"mac","mode":"multiview","action":"skip"}
{"ts":"...","event":"enter","target":"linux","mode":"fullscreen","action":"switch","input":"HDMI_4"}
{"ts":"...","event":"disconnect","reason":"ping_timeout"}
{"ts":"...","event":"reconnect_fail","retry":3}
```

**`/status` response:**
```json
{
  "mode": "fullscreen",
  "input": "linux",
  "healthy": true,
  "pending_switch": null,
  "uptime_seconds": 3600,
  "reconnect_total": 0,
  "switch_count": {"linux": 12, "mac": 8, "windows": 5},
  "last_error": null
}
```

#### 4. systemd Integration

```
[Service]
ExecStart=/usr/bin/LG_Buddy_PIP/bin/python3 .../tv_multiview_daemon.py
WorkingDirectory=$HOME/.config/lg-buddy
Restart=on-failure
RestartSec=5
StartLimitIntervalSec=60
StartLimitBurst=10
```

Signal handling: SIGTERM → `client.disconnect()` → graceful shutdown. SIGUSR1 → dump state to stderr.

#### 5. Edge Cases

| Scenario | Behavior |
|---|---|
| TV off when daemon starts | Exponential backoff, HTTP returns 503 |
| TV reboot during capture | Detect disconnect, reconnect, resubscribe |
| WiFi blip (silent) | 30s heartbeat detects, triggers reconnect |
| Remote: enter multiView (SXS or PIP) | subscribe callback → `mode=multiview` → future enters skip |
| Remote: exit multiView → fullscreen | Callback → `mode=fullscreen` → next enter switches |
| Remote: switch to different input | subscribe callback learns new input; capture state unchanged → `DisplayMatchesCursor` gap |
| Two rapid enters | Second sees `pending_switch != none` → debounce, returns current mode |
| Return to linux while multiView active | TV stays in multiView (linux already present); capture released |
| `splitscreenEnable` fails | Log warning, return 500; read-only `multiViewStatus` still works |
| bscpylgtv library crash | Exception caught, log, systemd restarts |
| TV IP changes | Daemon crashes → systemd restarts → fails to connect → admin must update config |

## Implementation Phases

### Phase 0: Fix Critical Defects (current daemon)

| Task | Description |
|---|---|
| P0 | Add `ping_interval=5` to `WebOsClient.create()` |
| P0 | Add 30s heartbeat + reconnect loop |
| P0 | Implement `pending_switch` gate (debounce rapid enters) |
| P1 | Structured JSON logging |
| P1 | Handle SIGTERM with `client.disconnect()` |
| P1 | Return 503 when `healthy=false` |
| P1 | Return 400 for invalid target |

### Phase 1: State Machine

| Task | Description |
|---|---|
| S1 | `TvDaemonState` dataclass with all fields |
| S2 | `DaemonDies` / `ReconnectFails` / `DaemonReconnects` transitions |
| S3 | `EnterOtherHost` sets `transitioning` → `SwitchComplete` resolves to `fullscreen` |
| S4 | `EnterMultiViewHost` (capture-only, no TV change) |

### Phase 2: MultiView Control (SXS and PIP)

| Task | Description |
|---|---|
| X1 | `/multiview/on` → `set_system_settings("commercial", {"splitscreenEnable": "on"})` (expected API, verify on live TV) |
| X2 | `/multiview/off` → same toggle off |
| X3 | **Verify** on live TV: confirm category, method, and that toggling works before committing |

### Phase 3: Observability

| Task | Description |
|---|---|
| O1 | `/status` endpoint (JSON metrics) |
| O2 | `/health` endpoint (liveness probe) |
| O3 | JSON-line structured logging |
| O4 | systemd `StartLimitIntervalSec` + `StartLimitBurst` |

### Phase 4: Integration

| Task | Description |
|---|---|
| I1 | Update `tv-multiview.service.j2` with `WorkingDirectory=~/.config/lg-buddy` + `StartLimit*` |
| I2 | Update `tv_multiview_daemon.py.j2` with full implementation |
| I3 | `/multiview/on` and `/multiview/off` are standalone — NOT embedded in `enter_hook` |

## Architecture Decision Records

### ADR-001: Separate daemon

**Decision:** Run tv-multiview as a standalone HTTP daemon alongside lan-mouse.

**Rationale:**
- lan-mouse `enter_hook` is synchronous (`sh -c "curl ..."`) — no async integration
- TV subscription needs persistent WebSocket — lan-mouse is event-driven
- HTTP API is universal: any OS, any enter_hook
- Separate crash domain: daemon death doesn't crash lan-mouse

### ADR-002: Shared LG_Buddy key file

**Decision:** `WorkingDirectory={{ ansible_env.HOME }}/.config/lg-buddy` so bscpylgtv
finds the existing `.aiopylgtv.sqlite` key file (same one LG_Buddy uses).

**Rationale:**
- LG_Buddy shells out to `/usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand` — same
  bscpylgtv library, same key file format
- Our daemon imports bscpylgtv as a library — same key file, same pairing
- One TV pairing, one key file, shared between both processes
- Python venv at `/usr/bin/LG_Buddy_PIP/` provides the interpreter + library

### ADR-003: MultiView toggle is standalone, not cursor-driven

**Decision:** `/multiview/on` and `/multiview/off` are independent HTTP endpoints —
NOT embedded in `enter_hook` parameters. MultiView mode is toggled by explicit
call, not by cursor crossing.

**Rationale:**
- `splitscreenEnable` (category `"commercial"`, per G4 firmware settings catalog snapshot —
  pending live-TV verification) only toggles mode on/off — cannot select which inputs go
  into multiView (side-by-side or PIP)
- The user pre-configures the desired layout and input pair once via remote
- Cursor movement should switch inputs in fullscreen, not trigger multiView
- The API endpoint works identically regardless of whether the user chose
  side-by-side or PIP — it's a single `multiViewStatus` toggle
