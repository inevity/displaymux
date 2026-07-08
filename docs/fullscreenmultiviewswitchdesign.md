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
EXTENDS Naturals, Sequences

\* --- TYPE DEFINITIONS ---

TVMode == { "fullscreen", "multiview", "transitioning" }
ActiveInput == { "linux", "mac", "windows", "unknown" }
CursorLocation == { "linux", "mac", "windows" }
CaptureState == { "idle", "capturing_linux", "capturing_mac", "capturing_windows" }
PendingValues == {"none", "multiview_on", "multiview_off"} \cup (ActiveInput \ {"unknown"})
SSAPState == { "disconnected", "connecting", "registering", "connected" }

\* --- CONSTANTS ---

SWITCH_TIMEOUT == 5     \* seconds before declaring no-signal
RECONNECT_CAP == 30     \* max reconnects before process exit

\* --- VARIABLES ---

VARIABLES
    tv_mode,            \* "fullscreen" | "multiview" | "transitioning"
    tv_input,           \* last commanded input (informational — NOT used as gate)
    cursor,             \* cursor location
    capture,            \* lan-mouse capture state
    ws_state,           \* SSAP WebSocket lifecycle
    subscribe_active,   \* is multiViewStatus subscription live
    daemon_healthy,     \* daemon + SSAP combined health (commands accepted)
    pending_switch,     \* in-flight command (debounce gate)
    reconnect_count,    \* consecutive failed reconnect attempts
    switch_timer,       \* countdown for no-signal detection after switch
    input_signal,       \* per-input HDMI signal presence (authoritative, from TV)
    remote_online,      \* per-remote-host lan-mouse spoke connectivity
    wake_pending        \* host being woken via WoL before retry (None|"mac"|"windows")

\* --- INVARIANTS ---

TypeInvariant ==
    /\ tv_mode \in TVMode
    /\ tv_input \in ActiveInput
    /\ cursor \in CursorLocation
    /\ capture \in CaptureState
    /\ ws_state \in SSAPState
    /\ subscribe_active \in BOOLEAN
    /\ daemon_healthy \in BOOLEAN
    /\ pending_switch \in PendingValues
    /\ reconnect_count \in 0..RECONNECT_CAP
    /\ switch_timer \in 0..SWITCH_TIMEOUT
    /\ input_signal \in [ActiveInput -> BOOLEAN]
    /\ remote_online \in [{"mac","windows"} -> BOOLEAN]
    /\ wake_pending \in ({"none"} \cup (ActiveInput \ {"linux","unknown"}))

\* Display matches cursor position.
DisplayMatchesCursor ==
    /\ (tv_mode = "fullscreen" /\ capture = "idle") => tv_input = "linux"
    /\ (tv_mode = "fullscreen" /\ capture = "capturing_linux") => tv_input = "linux"
    /\ (tv_mode = "fullscreen" /\ capture = "capturing_mac") => tv_input = "mac"
    /\ (tv_mode = "fullscreen" /\ capture = "capturing_windows") => tv_input = "windows"

\* No command in flight when daemon is dead.
NoPendingWhenDead ==
    (~daemon_healthy) => pending_switch = "none"

\* CENTRAL AVAILABILITY INVARIANT:
\* If a remote host is selected but has no signal or is offline,
\* the system must revert to Linux (the always-available host).
\* switch_timer > 0 means revert is already in progress.
LinuxAlwaysAvailable ==
    (tv_mode = "fullscreen" /\ tv_input \in {"mac", "windows"}) =>
        (input_signal[tv_input] = TRUE /\ remote_online[tv_input] = TRUE
         \/ switch_timer > 0)

\* Subscription only active when connected.
SubscribeRequiresConnected ==
    subscribe_active => ws_state = "connected"

\* Combined daemon health: SSAP connected AND subscribe active.
\* This ensures the daemon only accepts commands when the TV is fully reachable.
HealthDefinition ==
    daemon_healthy <=> (ws_state = "connected" /\ subscribe_active)

\* --- INITIAL STATE ---

Init ==
    /\ tv_mode = "fullscreen"
    /\ tv_input = "linux"
    /\ cursor = "linux"
    /\ capture = "idle"
    /\ ws_state = "disconnected"
    /\ subscribe_active = FALSE
    /\ daemon_healthy = FALSE    \* SSAP not yet connected
    /\ pending_switch = "none"
    /\ reconnect_count = 0
    /\ switch_timer = 0
    /\ input_signal = [linux |-> TRUE, mac |-> FALSE, windows |-> FALSE, unknown |-> FALSE]
    /\ remote_online = [mac |-> FALSE, windows |-> FALSE]
    /\ wake_pending = "none"

\* All variables tuple (used by Spec for stuttering).
vars == <<tv_mode, tv_input, cursor, capture, ws_state, subscribe_active,
          daemon_healthy, pending_switch, reconnect_count, switch_timer,
          input_signal, remote_online, wake_pending>>

\* =====================================================================
\* SSAP LIFECYCLE (persistent wss:// connection, replaces subprocess-per-command)
\* =====================================================================

\* TCP + TLS handshake to wss://TV_IP:3001/.
SSAPConnecting ==
    /\ ws_state = "disconnected"
    /\ ws_state' = "connecting"
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, subscribe_active,
                   daemon_healthy, pending_switch, reconnect_count,
                   switch_timer, input_signal, remote_online, wake_pending>>

\* SSAP register handshake: send client-key, receive registration confirmation.
SSAPRegistering ==
    /\ ws_state = "connecting"
    /\ ws_state' = "registering"
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, subscribe_active,
                   daemon_healthy, pending_switch, reconnect_count,
                   switch_timer, input_signal, remote_online, wake_pending>>

\* Registration complete. SSAP ready for commands.
\* daemon_healthy transitions to TRUE (via HealthDefinition).
SSAPRegistered ==
    /\ ws_state = "registering"
    /\ ws_state' = "connected"
    /\ daemon_healthy' = TRUE
    /\ reconnect_count' = 0
    \* Can't know TV state during disconnect; resync from subscribe + signal query.
    /\ tv_mode' \in {"fullscreen", "multiview"}
    /\ tv_input' \in ActiveInput
    \* Query signal status on reconnect to resolve stale state.
    /\ input_signal' \in [ActiveInput -> BOOLEAN]
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture,
                   pending_switch, switch_timer, remote_online, wake_pending>>

\* Subscribe to multiViewStatus push updates from TV.
\* daemon_healthy only becomes TRUE after subscription is live
\* (HealthDefinition: connected AND subscribed).
SSAPSubscribe ==
    /\ ws_state = "connected"
    /\ ~subscribe_active
    /\ subscribe_active' = TRUE
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, ws_state,
                   daemon_healthy, pending_switch, reconnect_count,
                   switch_timer, input_signal, remote_online, wake_pending>>

\* WebSocket drops (TV reboot, WiFi blip, TV off).
\* Everything goes dead. pending_switch cleared (invariant).
SSAPDisconnect ==
    /\ ws_state = "connected"
    /\ ws_state' = "disconnected"
    /\ subscribe_active' = FALSE
    /\ daemon_healthy' = FALSE
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture,
                   reconnect_count, input_signal, remote_online, wake_pending>>

\* =====================================================================
\* SUBSCRIPTION CALLBACK (push updates from TV)
\* =====================================================================

\* TV reports multiViewStatus change via subscribe callback.
\* Can fire at any time while subscribed — handles race with pending_switch.
\* C4 fix: remote can only set fullscreen or multiview, never transitioning.
SubscriptionFires ==
    /\ subscribe_active
    /\ tv_mode' \in {tv_mode} \cup ({"fullscreen", "multiview"} \ {tv_mode})
    /\ tv_input' \in ActiveInput        \* remote may change input independently
    /\ IF pending_switch /= "none" THEN
           /\ pending_switch' = "none"  \* C4: remote override clears pending
       ELSE
           /\ UNCHANGED <<pending_switch, wake_pending>>
    /\ switch_timer' = 0                \* cancel any in-flight timeout
    /\ UNCHANGED <<cursor, capture, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, input_signal, remote_online, wake_pending>>

\* =====================================================================
\* SWITCH TRANSITIONS (EnterOtherHost → SwitchComplete | SwitchFailed | SwitchTimeout)
\* =====================================================================

\* =====================================================================
\* TWO-PHASE SWITCH PROTOCOL (atomicity invariant: cursor never moves to dead host)
\*
\* Phase 1 (EnterOtherHost): TV input switches. Cursor + capture STAY on Linux.
\*   The daemon issues set_input(target) and awaits two confirmations:
\*   (a) SSAP response: set_input() returned success.
\*   (b) Signal verification: target input has HDMI signal present.
\*   If (a) fails → SwitchFailed. If (b) fails within SWITCH_TIMEOUT → SwitchTimeout.
\*   In both failure cases, cursor never left Linux — user is never trapped.
\*
\* Phase 2 (SwitchComplete): Both confirmations received. NOW move cursor + capture
\*   to target. The HTTP response returns "fullscreen" to lan-mouse, which then
\*   switches its own capture to the remote host. Cursor only moves AFTER the
\*   display is verified working.
\*
\* This eliminates the trap scenario: switch to sleeping/shutdown Windows →
\*   display shows nothing → keyboard/mouse captured by dead host → stuck.
\* =====================================================================

\* Phase 1: Cursor crosses edge. TV input switches. Cursor + capture STAY on Linux.
\* APPROACH 1: ALWAYS issues set_input() — no stale-state no-op guard.
\* switch_timer starts countdown for signal verification.
\* GUARD: target host must be online (remote_online[target] = TRUE).
\*   If offline, SendWoL fires instead — wake the host, then auto-retry.
EnterOtherHost(host) ==
    LET target == CASE host = "mac" -> "mac"
                    [] host = "windows" -> "windows"
                    [] OTHER -> "linux"
    IN
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"          \* debounce: only one switch at a time
    /\ cursor \in {"linux"}
    /\ daemon_healthy                   \* requires connected + subscribed
    /\ ws_state = "connected"
    /\ remote_online[target] = TRUE     \* C11: target must be online (WoL otherwise)
    /\ tv_mode' = "transitioning"
    /\ tv_input' = target
    \* CURSOR + CAPTURE STAY ON LINUX — do not move until signal verified.
    /\ pending_switch' = target
    /\ switch_timer' = SWITCH_TIMEOUT   \* start signal-verification countdown
    /\ UNCHANGED <<cursor, capture, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, input_signal, remote_online,
                   wake_pending>>

\* Phase 2a: set_input() acknowledged AND signal present on target input.
\* NOW move cursor + capture to target. Return success to lan-mouse.
SwitchComplete ==
    /\ tv_mode = "transitioning"
    /\ input_signal[tv_input] = TRUE    \* authoritative signal check
    /\ tv_mode' = "fullscreen"
    /\ cursor' = tv_input               \* cursor moves NOW — after verification
    /\ capture' = CASE tv_input = "mac" -> "capturing_mac"
                    [] tv_input = "windows" -> "capturing_windows"
                    [] OTHER -> "capturing_linux"
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ UNCHANGED <<tv_input, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, input_signal, remote_online, wake_pending>>

\* Phase 2b: set_input() returned SSAP error.
\* Cursor never left Linux. Just clear pending, return 502 to lan-mouse.
SwitchFailed ==
    /\ tv_mode = "transitioning"
    /\ ws_state = "connected"
    /\ tv_mode' = "fullscreen"
    /\ tv_input' = "linux"
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ UNCHANGED <<cursor, capture, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, input_signal, remote_online, wake_pending>>

\* Phase 2b: Timer expired — set_input() succeeded but no signal on target.
\* Cursor never left Linux. Revert tv_input, clear pending, return failure.
SwitchTimeout ==
    /\ tv_mode = "transitioning"
    /\ switch_timer = 1                \* last tick before expiry
    /\ switch_timer' = 0
    /\ tv_mode' = "fullscreen"
    /\ tv_input' = "linux"
    /\ pending_switch' = "none"
    /\ UNCHANGED <<cursor, capture, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, input_signal, remote_online, wake_pending>>

\* Timer tick — models time passing during switch.
TimerTick ==
    /\ tv_mode = "transitioning"
    /\ switch_timer > 1
    /\ switch_timer' = switch_timer - 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, input_signal, remote_online, wake_pending>>

\* =====================================================================
\* SIGNAL STATUS TRACKING (authoritative TV-reported signal presence)
\* =====================================================================

\* Periodic query to TV: getExternalInputList or equivalent.
\* Returns per-port signal presence. This is the authoritative source
\* for whether a display is actually usable — replaces stale tv_input guess.
\* Fires non-deterministically to model arbitrary timing of periodic poll.
SignalUpdate ==
    /\ ws_state = "connected"
    /\ input_signal' \in [ActiveInput -> BOOLEAN]
    \* If currently on a remote host and signal drops, start revert timer.
    /\ IF tv_mode = "fullscreen" /\ tv_input \in {"mac", "windows"}
          /\ input_signal'[tv_input] = FALSE THEN
           /\ switch_timer' = SWITCH_TIMEOUT
       ELSE
           /\ UNCHANGED <<switch_timer, wake_pending>>
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, remote_online, wake_pending>>

\* Signal-loss revert: fullscreen on remote, signal just went dead, timer expired.
\* Revert to Linux.
SignalLossRevert ==
    /\ tv_mode = "fullscreen"
    /\ tv_input \in {"mac", "windows"}
    /\ ~input_signal[tv_input]
    /\ switch_timer = 1
    /\ switch_timer' = 0
    /\ tv_mode' = "transitioning"
    /\ tv_input' = "linux"
    /\ pending_switch' = "linux"
    /\ UNCHANGED <<cursor, capture, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, input_signal, remote_online, wake_pending>>

\* =====================================================================
\* REMOTE HOST HEALTH (lan-mouse spoke connectivity)
\* =====================================================================

\* Remote host is offline — instead of failing the switch, wake it first.
\* C11: Send WoL, set wake_pending, and return "waking" to lan-mouse.
\* The user's cursor stays on Linux; no capture switch happens.
\* When RemoteHostOnline fires → WakeAndRetry automatically re-enters.
SendWoL(host) ==
    /\ host \in {"mac", "windows"}
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"
    /\ cursor \in {"linux"}
    /\ daemon_healthy
    /\ ws_state = "connected"
    /\ ~remote_online[host]            \* host is asleep/offline
    /\ wake_pending = "none"
    /\ wake_pending' = host
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                    reconnect_count, switch_timer, input_signal, remote_online>>
                                                                       \* wake_pending set explicitly above
\* Now that remote_online[host] = TRUE, EnterOtherHost(host) becomes enabled.
WakeAndRetry(host) ==
    /\ host \in {"mac", "windows"}
    /\ wake_pending = host
    /\ remote_online[host] = TRUE      \* host woke up
    /\ wake_pending' = "none"
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                                       reconnect_count, switch_timer, input_signal, remote_online>>
                                                              \* wake_pending set explicitly above (power off, crash, network loss).
\* If currently displaying that host, initiate revert to Linux.
RemoteHostOffline(host) ==
    /\ host \in {"mac", "windows"}
    /\ remote_online[host] = TRUE
    /\ remote_online' = [remote_online EXCEPT ![host] = FALSE]
    /\ IF tv_mode = "fullscreen" /\ tv_input = host THEN
           /\ tv_mode' = "transitioning"
           /\ tv_input' = "linux"
           /\ pending_switch' = "linux"
           /\ switch_timer' = SWITCH_TIMEOUT
       ELSE
           /\ UNCHANGED <<tv_mode, tv_input, pending_switch, switch_timer, wake_pending>>
    /\ UNCHANGED <<cursor, capture, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, input_signal,
                   wake_pending>>

RemoteHostOnline(host) ==
    /\ host \in {"mac", "windows"}
    /\ remote_online[host] = FALSE
    /\ remote_online' = [remote_online EXCEPT ![host] = TRUE]
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, input_signal,
                   wake_pending>>

\* =====================================================================
\* MULTIVIEW TRANSITIONS (Side-by-side / PIP)
\* =====================================================================

\* F→M: Enable multiView via splitscreenEnable SSAP command.
\* Atomic SSAP call — no settle delay. pending_switch set during call, cleared on return.
EnterMultiView ==
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"
    /\ daemon_healthy
    /\ ws_state = "connected"
    /\ tv_mode' = "multiview"
    /\ pending_switch' = "none"
    /\ UNCHANGED <<tv_input, cursor, capture, ws_state, subscribe_active,
                   reconnect_count, switch_timer, input_signal, remote_online, wake_pending>>

\* M→F: Disable multiView, return to fullscreen.
\* Atomic SSAP call. Restore TV input to the host the cursor is currently capturing.
ExitMultiView ==
    /\ tv_mode = "multiview"
    /\ pending_switch = "none"
    /\ daemon_healthy
    /\ ws_state = "connected"
    /\ tv_mode' = "fullscreen"
    /\ tv_input' = CASE capture = "capturing_mac" -> "mac"
                     [] capture = "capturing_windows" -> "windows"
                     [] OTHER -> "linux"
    /\ pending_switch' = "none"
    /\ UNCHANGED <<cursor, capture, ws_state, subscribe_active,
                   reconnect_count, switch_timer, input_signal, remote_online, wake_pending>>

\* Cursor crosses edge while in multiView: update capture state only.
\* Do NOT switch TV input — it's showing multiple sources already.
EnterMultiViewHost(host) ==
    LET target == CASE host = "mac" -> "mac"
                    [] host = "windows" -> "windows"
                    [] OTHER -> "linux"
    IN
    /\ tv_mode = "multiview"
    /\ pending_switch = "none"
    /\ cursor' = target
    /\ capture' = CASE host = "mac" -> "capturing_mac"
                   [] host = "windows" -> "capturing_windows"
                   [] OTHER -> "capturing_linux"
    /\ UNCHANGED <<tv_mode, tv_input, ws_state, subscribe_active,
                   daemon_healthy, pending_switch, reconnect_count,
                   switch_timer, input_signal, remote_online, wake_pending>>

\* =====================================================================
\* RETURN TO LINUX
\* =====================================================================

\* Cursor comes back to local machine.
\* Fullscreen: switch TV input back to linux (through transitioning).
\* MultiView or transitioning: leave TV alone, just release capture.
ReturnToLinux ==
    /\ cursor \in {"mac", "windows"}
    /\ pending_switch = "none"
    /\ cursor' = "linux"
    /\ capture' = "idle"
    /\ IF tv_mode = "fullscreen" THEN
           /\ daemon_healthy
           /\ ws_state = "connected"
           /\ tv_mode' = "transitioning"
           /\ tv_input' = "linux"
           /\ pending_switch' = "linux"
           /\ switch_timer' = SWITCH_TIMEOUT
       ELSE
           /\ UNCHANGED <<tv_mode, tv_input, pending_switch, switch_timer,
                          daemon_healthy, ws_state, wake_pending>>
    /\ UNCHANGED <<subscribe_active, reconnect_count,
                   input_signal, remote_online, wake_pending>>

\* =====================================================================
\* RECONNECT LIFECYCLE
\* =====================================================================

\* Reconnect attempt failed. Exponential backoff external to spec.
ReconnectFails ==
    /\ ~daemon_healthy
    /\ reconnect_count < RECONNECT_CAP
    /\ reconnect_count' = reconnect_count + 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   switch_timer, input_signal, remote_online, wake_pending>>

\* Retry cap reached. Process exits; systemd Restart=on-failure
\* gives a fresh start with reconnect_count=0 (Init).
DaemonExits ==
    /\ ~daemon_healthy
    /\ reconnect_count = RECONNECT_CAP
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, input_signal, remote_online, wake_pending>>

\* =====================================================================
\* COMPOSITE NEXT
\* =====================================================================

Next ==
    \/ SSAPConnecting
    \/ SSAPRegistering
    \/ SSAPRegistered
    \/ SSAPSubscribe
    \/ SSAPDisconnect
    \/ SubscriptionFires
    \/ \E host \in {"mac", "windows"} : EnterOtherHost(host)
    \/ \E host \in {"mac", "windows"} : SendWoL(host)
    \/ \E host \in {"mac", "windows"} : WakeAndRetry(host)
    \/ SwitchComplete
    \/ SwitchFailed
    \/ SwitchTimeout
    \/ TimerTick
    \/ SignalUpdate
    \/ SignalLossRevert
    \/ \E host \in {"mac", "windows"} : RemoteHostOffline(host)
    \/ \E host \in {"mac", "windows"} : RemoteHostOnline(host)
    \/ EnterMultiView
    \/ ExitMultiView
    \/ \E host \in {"mac", "windows"} : EnterMultiViewHost(host)
    \/ ReturnToLinux
    \/ ReconnectFails
    \/ DaemonExits

Spec == Init /\ [][Next]_vars
          /\ WF_vars(ReturnToLinux)
          /\ WF_vars(SSAPRegistered)

\* =====================================================================
\* LIVENESS
\* =====================================================================

\* Daemon eventually reconnects or reaches the retry cap.
EventuallyReconnect ==
    (~daemon_healthy /\ ws_state = "disconnected")
        ~> (daemon_healthy /\ ws_state = "connected" \/ reconnect_count = RECONNECT_CAP)

\* If a remote host is selected but unusable, eventually revert to Linux.
EventuallyRevert ==
    (tv_mode = "fullscreen" /\ tv_input \in {"mac", "windows"}
     /\ (~input_signal[tv_input] \/ ~remote_online[tv_input]))
        ~> (tv_input = "linux")

\* =====================================================================
\* DESIGN DECISIONS
\* =====================================================================

\* C1 (stuck pending): EnterMultiView/ExitMultiView clear pending directly
\*     (atomic SSAP calls). ReturnToLinux routes through transitioning
\*     like EnterOtherHost. SwitchFailed/SwitchTimeout clear pending.
\* C2 (DisplayMatchesCursor violations): Resolved by SwitchFailed,
\*     SwitchTimeout, SignalLossRevert, and RemoteHostOffline — all revert
\*     to Linux when the display is unusable. SubscriptionFires handles
\*     TvRemoteOverride races.
\* C3 (deadlock at cap): DaemonExits → systemd restart → fresh Init with
\*     reconnect_count = 0.
\* C4 (TvRemoteOverride): SubscriptionFires clears pending_switch.
\*     tv_mode' restricted to {"fullscreen","multiview"} — remote can't
\*     set transitioning.
\* C5 (liveness violated by user): EventuallyReturn dropped (user may
\*     never return cursor). Instead: EventuallyRevert ensures the
\*     system recovers from host failure. EventuallyReconnect covers the
\*     SSAP health path.
\* C6 (stale-state no-op): REMOVED. Approach 1: always issue set_input(),
\*     never skip based on cached tv_input. set_input() is idempotent.
\*     tv_input is now informational-only — not a gating condition.
\* C7 (stale state elimination): tv_input is no longer the authority on
\*     what the TV displays. input_signal (from TV SSAP query) and
\*     remote_online (from lan-mouse spoke) are the authoritative sources.
\* C8 (always-availability): LinuxAlwaysAvailable invariant enforced by
\*     SwitchFailed, SwitchTimeout, SignalLossRevert, and
\*     RemoteHostOffline. The system never gets stuck on a dead display.
\* C9 (unified design): SSAP lifecycle (ws_state, subscribe_active) and
\*     daemon state machine (tv_mode, tv_input, cursor, capture) are
\*     modeled in one spec. The HealthDefinition invariant ties them
\*     together: daemon_healthy iff connected AND subscribed.
\* C10 (two-phase atomic enter): EnterOtherHost switches only the TV input.
\*     Cursor and capture stay on Linux during Phase 1. SwitchComplete
\*     (signal verified) moves cursor+capture to target. SwitchFailed or
\*     SwitchTimeout leaves cursor on Linux. This prevents the trap:
\*     switch to sleeping/shutdown host → keyboard/mouse captured by dead
\*     host → user can't move or see anything. The invariant is: cursor
\*     never moves to a host whose display isn't verified working.
\* C11 (pre-switch wake): EnterOtherHost requires remote_online[target] = TRUE
\*     for remote hosts. If the host is asleep/offline, SendWoL fires
\*     instead — sends Wake-on-LAN, sets wake_pending, returns "waking"
\*     to lan-mouse. When the host comes online (RemoteHostOnline),
\*     WakeAndRetry clears wake_pending, and EnterOtherHost becomes
\*     enabled again (auto-retry). The user never needs to re-trigger
\*     the edge crossing. If the host never wakes, the user can use the
\*     TV remote to switch back manually (TvRemoteOverride via
\*     SubscriptionFires).

=================================================================================
```

## Architecture Design

### Actors

```
┌─────────────┐     enter_hook      ┌──────────────┐    persistent wss://      ┌──────┐
│  lan-mouse  │ ──── curl ────────→ │ tv-multiview  │ ──── SSAP (WebSocket) ─→ │  TV  │
│   (hub)     │                     │   daemon      │                          │ G4   │
└─────────────┘                     │  (Rust)       │ ←── subscribe callback ── │      │
                                    └──────────────┘                          └──────┘
                                           │
                                           │ HTTP API (axum)
                                           ▼
                                    ┌──────────────┐
                                    │  External     │
                                    │  triggers     │
                                    │ (multiView   │
                                    │  toggle)     │
                                    └──────────────┘

The Rust daemon holds one persistent wss:// connection to the TV for its
entire lifetime. No subprocess-per-command. No repeated TLS handshakes.
No repeated SSAP register. All SSAP operations (set_input,
set_splitscreen, get_signal_status, subscribe) flow over the same
long-lived WebSocket.

The daemon uses the same .aiopylgtv.sqlite client-key file as LG_Buddy
(at ~/.config/lg-buddy/), so one TV pairing works for both tools.
```

### API Endpoints

| Method | Path | Purpose | Returns |
|---|---|---|---|
| GET | `/enter/{target}` | lan-mouse enter_hook. Switch to `target`. Always issues set_input() (no stale-state no-op). | TV mode string |
| GET | `/multiview/on` | Enable multiView via `splitscreenEnable` SSAP command. | TV mode string |
| GET | `/multiview/off` | Disable multiView. | TV mode string |
| GET | `/status` | Health + current state + signal status. | JSON |
| GET | `/health` | Liveness probe (always 200 if process alive). | `"ok"` |

### State Machine (Daemon Internal)

```
                         ┌──────────────────────────────────┐
                         │          DISCONNECTED             │
                         │  ws_state = disconnected          │
                         │  daemon_healthy = false           │
                         │  HTTP: 503                        │
                         └──────┬──────────────┬────────────┘
                                │ reconnect    │ disconnect
                                ▼              ▼
                   ┌─────────────────────────────────────────────────────┐
                   │                   CONNECTED                          │
                   │  ws_state = connected, subscribe_active = true       │
                   │                                                     │
                   │  ┌──────────┐   multiview     ┌──────────┐          │
                   │  │FULLSCREEN│ ── on ────────→ │MULTIVIEW │          │
                   │  │          │ ←── off ─────── │(SXS/PIP) │          │
                   │  │          │                 │          │          │
                   │  │ enter→   │                 │ enter→   │          │
                   │  │ TRANS.   │                 │ capture  │          │
                   │  │  ┌───────┼───┐             │ only     │          │
                   │  │  │ SwitchComplete  │       └──────────┘          │
                   │  │  │ SwitchFailed    │                              │
                   │  │  │ SwitchTimeout   │                              │
                   │  │  └───────┼───┘             ┌──────────┐          │
                   │  │          │                 │ SIGNAL   │          │
                   │  │  SignalLossRevert ───────→ │ LOSS     │          │
                   │  │          │                 │ REVERT   │          │
                   │  └──────────┘                 └──────────┘          │
                   │                                                     │
                   │  ┌──────────┐                                       │
                   │  │ REMOTE   │  RemoteHostOffline ──→ revert to linux│
                   │  │ HOST     │                                       │
                   │  │ OFFLINE  │                                       │
                   │  └──────────┘                                       │
                   └─────────────────────────────────────────────────────┘

Pending switch gating: pending_switch != "none" blocks all new transitions
that produce a TV command (natural debounce).

Failure is always recoverable: SwitchFailed, SwitchTimeout, SignalLossRevert,
and RemoteHostOffline all converge to tv_input = "linux", cursor = "linux",
capture = "idle" — the always-available baseline.
```

### Reliability Design

#### 1. SSAP Lifecycle (persistent wss://)

The daemon holds one persistent WebSocket connection to the TV.
This eliminates the ~28% CPU overhead of spawning a Python subprocess
per command (previously `bscpylgtvcommand` every 5s).

**Connect flow:**
1. TCP + TLS handshake to `wss://TV_IP:3001/` (TV self-signed cert, trusted).
2. SSAP register: send client-key (from `~/.config/lg-buddy/.aiopylgtv.sqlite`).
   On first ever connect, TV shows pairing prompt; subsequent connects use
   the persisted client-key with no prompt.
3. Subscribe to `multiViewStatus` push updates.
4. Query signal status (`getExternalInputList` or equivalent) to resolve
   any stale state from while disconnected.
5. daemon_healthy = TRUE only after all of the above succeed.

**Keepalive:** WebSocket-level ping/pong (tokio-tungstenite built-in).
No separate heartbeat command needed. If the TV misses 3 consecutive pongs,
the WebSocket is declared dead and `SSAPDisconnect` fires.

**Reconnect:** Exponential backoff (1s → 2s → 4s → ... → 60s cap).
After RECONNECT_CAP (30) consecutive failures, the process exits.
systemd `Restart=on-failure` gives a fresh start.

#### 2. Enter Hook Logic

```
curl → /enter/{target}:                               [cursor still on linux]
  1. If daemon_healthy=false → 503
  2. If pending_switch != none → 200 current_mode  (debounce)
  3. If mode=multiview → 200 "multiview"           (skip, don't disturb multiView)
  4. If mode=fullscreen → CHECK TARGET ONLINE:
     a. If remote_online[target] = FALSE:
        → send WoL, set wake_pending=target         [C11: pre-switch wake]
        → return 202 "waking" to lan-mouse
        → when host comes online → auto-retry step 4
     b. If remote_online[target] = TRUE → TWO-PHASE ENTER:
        i.   set_input(target) always  (APPROACH 1: no stale-state no-op)
        ii.  Await SSAP response:
             ├─ error → SwitchFailed  → revert to linux → 502
             └─ success → Proceed
        iii. Query signal status on target input:
             ├─ signal present → SwitchComplete → cursor+capture move → 200 "fullscreen"
             └─ no signal      → SwitchTimeout  → revert to linux   → 502
        [CRITICAL: cursor+capture only move in step (iii) AFTER signal verified.
         If signal absent, cursor never left linux — user is never trapped.]
```

The key change from the previous design (C6): step 4 no longer checks
`tv_input == target`. It always issues `set_input()`. The TV's actual
state is determined by `input_signal` (from SSAP query) and `remote_online`
(from lan-mouse spoke), not by cached `tv_input`.

The second key change (C10): cursor and capture do NOT move to the target
until signal is verified (two-phase protocol). This prevents the trap
scenario where the user's keyboard/mouse are captured by a dead host.

The third key change (C11): if the target host is offline, the daemon
sends Wake-on-LAN and defers the switch. When the host wakes up and its
lan-mouse spoke connects, the daemon auto-retries the enter. The user
never needs to manually re-cross the screen edge.

#### 3. Failure Recovery Paths

All failures converge to the same recovery state:
`tv_input = "linux"`, `cursor = "linux"`, `capture = "idle"`.

| Failure | Detection | Recovery Time | Transition |
|---|---|---|---|
| set_input() SSAP error | Immediate (response code) | <1s | SwitchFailed |
| Source has no HDMI signal | switch_timer expires (5s) | 5s | SwitchTimeout |
| Signal drops after stable connection | SignalUpdate periodic poll | 5s (SWITCH_TIMEOUT) | SignalLossRevert |
| Remote host spoke disconnects | lan-mouse hub event | <3s | RemoteHostOffline |
| WebSocket disconnects | ping timeout (~15s) | ~15s + reconnect backoff | SSAPDisconnect → reconnect |
| TV reboot | ping timeout → reconnect | ~20s | SSAPDisconnect → reconnect → SSAPRegistered |

#### 4. Signal Status Tracking

The daemon periodically queries the TV for per-input signal presence
(via SSAP `getExternalInputList` or equivalent endpoint). This is the
**authoritative** source for whether a display is actually usable — it
replaces the old approach of trusting the cached `tv_input` variable.

Query frequency: once after each switch (to confirm signal), then every
10s while a remote host is selected (to detect mid-session signal loss).
No query while on Linux (zero overhead for the always-available baseline).

#### 5. Remote Host Health Tracking

The daemon monitors lan-mouse spoke connectivity to determine whether
remote hosts are online. If a spoke disconnects while that host's input
is selected, the daemon reverts to Linux.

This covers:
- macOS powered off after being selected.
- Windows crash/reboot while selected.
- Network loss to the remote host.

#### 6. Observability

**Structured JSON logging (stdout, one object per line):**
```json
{"ts":"...","event":"ssap_connecting","tv_ip":"192.0.2.20"}
{"ts":"...","event":"ssap_registered","client_key_present":true}
{"ts":"...","event":"subscribed","topic":"multiViewStatus"}
{"ts":"...","event":"enter","target":"mac","action":"switch","input":"HDMI_3"}
{"ts":"...","event":"switch_complete","input":"mac","signal":true}
{"ts":"...","event":"switch_timeout","target":"mac","action":"revert_to_linux"}
{"ts":"...","event":"switch_failed","target":"windows","error":"timeout","action":"revert_to_linux"}
{"ts":"...","event":"signal_loss","input":"mac","action":"revert_to_linux"}
{"ts":"...","event":"remote_offline","host":"mac","action":"revert_to_linux"}
{"ts":"...","event":"ssap_disconnect","reason":"ping_timeout"}
```

**`/status` response:**
```json
{
  "mode": "fullscreen",
  "input": "linux",
  "healthy": true,
  "ws_state": "connected",
  "subscribe_active": true,
  "pending_switch": null,
  "switch_timer": 0,
  "input_signal": {"linux": true, "mac": false, "windows": true},
  "remote_online": {"mac": false, "windows": true},
  "uptime_seconds": 3600,
  "reconnect_total": 0,
  "switch_count": {"linux": 12, "mac": 8, "windows": 5},
  "last_error": null
}
```

#### 7. systemd Integration

```
[Service]
ExecStart=/home/example/.local/bin/tv-multiview
Restart=on-failure
RestartSec=5
StartLimitIntervalSec=60
StartLimitBurst=10
```

Signal handling: SIGTERM → graceful WebSocket close → shutdown.
SIGUSR1 → dump full state to stderr (debugging).

## Architecture Decision Records

### ADR-001: Separate daemon

**Decision:** Run tv-multiview as a standalone HTTP daemon alongside lan-mouse.

**Rationale:**
- lan-mouse `enter_hook` is synchronous (`sh -c "curl ..."`) — no async integration
- TV subscription needs persistent WebSocket — lan-mouse is event-driven
- HTTP API is universal: any OS, any enter_hook
- Separate crash domain: daemon death doesn't crash lan-mouse

### ADR-002: Shared LG_Buddy key file

**Decision:** Read client-key from `~/.config/lg-buddy/.aiopylgtv.sqlite`
(same file LG_Buddy uses). One TV pairing, one key file.

**Rationale:**
- LG_Buddy and our daemon speak the same SSAP protocol to the same TV
- The client-key from the initial pairing prompt is stored in sqlite
- Sharing the key file means the user pairs once, both tools work

### ADR-003: MultiView toggle is standalone, not cursor-driven

**Decision:** `/multiview/on` and `/multiview/off` are independent HTTP endpoints —
NOT embedded in `enter_hook` parameters.

**Rationale:**
- `splitscreenEnable` only toggles mode on/off — cannot select which inputs
  go into multiView
- The user pre-configures the desired layout and input pair once via remote
- Cursor movement should switch inputs in fullscreen, not trigger multiView

### ADR-004: Persistent wss:// (no subprocess-per-command)

**Decision:** The daemon holds one long-lived WebSocket connection to the TV.
All SSAP commands flow over this connection. No Python subprocess, no
per-command TLS handshake.

**Rationale:**
- Eliminates ~28% CPU from Python subprocess startup (previously
  `bscpylgtvcommand` every 5s for heartbeat)
- Reduces per-command latency from 300-500ms to <5ms
- Connection health is immediate (socket error on next write) vs.
  discovered via 5s heartbeat
- Subscriptions (push updates from TV) require a persistent connection
  anyway — can't subscribe over a one-shot subprocess

### ADR-005: Approach 1 — always switch, never skip on stale state

**Decision:** `/enter/{target}` always issues `set_input(target)`.
There is no "already on target" no-op guard. `tv_input` is
informational-only (for `/status`), not a gating condition.

**Rationale:**
- `tv_input` can become stale (user pressed remote, TV rebooted,
  previous switch silently failed)
- `set_input()` is idempotent — sending the same HDMI port when already
  on it is harmless
- Simpler state machine: one less invariant to maintain
- Eliminates the bug where the daemon thinks it's on macOS but the display
  shows something else

### ADR-006: Always-availability — revert to Linux on any failure

**Decision:** If a remote host (mac/windows) is selected but becomes
unusable, the daemon automatically reverts to Linux. This is the
`LinuxAlwaysAvailable` invariant in the TLA+ spec.

**Rationale:**
- The user's desktop must always have a usable display
- If macOS shows "No Signal" or is powered off, the user is stuck
  (can't move cursor back because screen edge is unreachable)
- Detect via: SSAP signal query (no HDMI signal), lan-mouse spoke
  disconnect (host offline), SSAP command failure (set_input error)
- All failure paths converge to the same recovery: linux input,
  linux cursor, idle capture

### ADR-007: SSAP + daemon unified in one TLA+ spec

**Decision:** The TLA+ spec models both the SSAP client lifecycle
(`ws_state`, `subscribe_active`) and the daemon state machine
(`tv_mode`, `tv_input`, `cursor`, `capture`) in one module.

**Rationale:**
- The daemon's state machine depends on SSAP events (disconnect →
  DaemonDies, register → DaemonReconnects, subscribe fires →
  SubscriptionFires)
- The `HealthDefinition` invariant ties them together: daemon_healthy
  iff connected AND subscribed
- Modeling them separately would miss race conditions between SSAP
  events and daemon state transitions
- In code, they are separated by a module boundary (`src/ssap/`) within
  a single Rust crate, not a crate boundary — so the unified spec
  matches the implementation structure
