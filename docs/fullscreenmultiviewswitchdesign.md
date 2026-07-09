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

\* --- TYPE DEFINITIONS ---

Host == { "linux", "mac", "windows" }
SERVER_HOST == "linux"  \* current deployment: the lan-mouse hub/server host
RemoteHosts == Host \ {SERVER_HOST}

TVMode == { "fullscreen", "multiview", "transitioning" }
ActiveInput == Host \cup { "unknown" }
CursorLocation == Host
\* The server host never captures its own local input. Non-server hosts use
\* HostCapture below; if SERVER_HOST changes, the same invariant still holds.
CaptureState == { "idle", "capturing_linux", "capturing_mac", "capturing_windows" }
PendingValues == {"none", "multiview_on", "multiview_off"} \cup (ActiveInput \ {"unknown"})
SSAPState == { "disconnected", "connecting", "registering", "connected" }
InputOwner == Host  \* keyboard + mouse as one atomic unit
InputCapabilities == { "keyboard", "pointer" }

\* --- CONSTANTS ---

SWITCH_TIMEOUT == 5     \* seconds before declaring no-signal
WAKE_TIMEOUT == 60      \* seconds before cancelling a wake attempt
RECONNECT_CAP == 30     \* max reconnects before process exit

HostCapture == [
    linux |-> "capturing_linux",
    mac |-> "capturing_mac",
    windows |-> "capturing_windows"
]

CaptureFor(host) ==
    IF host = SERVER_HOST THEN "idle" ELSE HostCapture[host]

\* --- VARIABLES ---

VARIABLES
    tv_mode,            \* "fullscreen" | "multiview" | "transitioning"
    tv_input,           \* last commanded input (informational — NOT used as gate)
    cursor,             \* cursor location
    capture,            \* lan-mouse capture state
    input_owner,         \* atomic keyboard+mouse owner; never split
    ws_state,           \* SSAP WebSocket lifecycle
    subscribe_active,   \* is multiViewStatus subscription live
    daemon_healthy,     \* daemon + SSAP combined health (commands accepted)
    pending_switch,     \* in-flight command (debounce gate)
    reconnect_count,    \* consecutive failed reconnect attempts
    switch_timer,       \* countdown for no-signal detection after switch
    wake_timer,         \* countdown for host wake attempt
    input_signal,       \* per-input HDMI signal presence (authoritative, from TV)
    remote_online,      \* per-remote-host lan-mouse spoke connectivity
    remote_input_ready, \* per-host keyboard+pointer injection readiness
    wake_pending        \* host being woken via WoL before retry ("none" or RemoteHosts)

\* --- INVARIANTS ---

TypeInvariant ==
    /\ tv_mode \in TVMode
    /\ tv_input \in ActiveInput
    /\ cursor \in CursorLocation
    /\ capture \in CaptureState
    /\ input_owner \in InputOwner
    /\ ws_state \in SSAPState
    /\ subscribe_active \in BOOLEAN
    /\ daemon_healthy \in BOOLEAN
    /\ pending_switch \in PendingValues
    /\ reconnect_count \in 0..RECONNECT_CAP
    /\ switch_timer \in 0..SWITCH_TIMEOUT
    /\ wake_timer \in 0..WAKE_TIMEOUT
    /\ input_signal \in [ActiveInput -> BOOLEAN]
    /\ remote_online \in [RemoteHosts -> BOOLEAN]
    \* String keys are intentional: InputCapabilities is a set of string
    \* values, so each per-host readiness map is a total function over them.
    /\ remote_input_ready \in [RemoteHosts -> [InputCapabilities -> BOOLEAN]]
    /\ wake_pending \in ({"none"} \cup RemoteHosts)

\* Keyboard and mouse are never independently switched. The local user's
\* physical input is one unit: pointer motion, pointer buttons, scroll, and
\* keyboard events must be owned by the same host at every visible state.
InputOwnershipAtomic ==
    /\ (input_owner = SERVER_HOST) =>
         (cursor = SERVER_HOST /\ capture = "idle")
    /\ \A host \in RemoteHosts :
         (input_owner = host) => (cursor = host /\ capture = CaptureFor(host))

RemoteReadyForControl(host) ==
    /\ host \in RemoteHosts
    /\ remote_online[host] = TRUE
    /\ remote_input_ready[host]["keyboard"] = TRUE
    /\ remote_input_ready[host]["pointer"] = TRUE

\* If input is owned by a remote host in fullscreen, the visible input must
\* be that same host. Server-host ownership can temporarily coexist with a
\* remote TV input during manual TV override/resync, but remote ownership
\* cannot be invisible.
DisplayMatchesInputOwner ==
    (tv_mode = "fullscreen" /\ input_owner \in RemoteHosts) =>
        tv_input = input_owner

\* When a failed/aborted switch has settled and input is back on the
\* lan-mouse server host, fullscreen display must also be back on that same
\* host. SERVER_HOST is Linux only in the current deployment; the invariant is
\* about the machine that runs the lan-mouse hub/server.
ServerHostNormalFallback ==
    (daemon_healthy /\ tv_mode = "fullscreen" /\ input_owner = SERVER_HOST
     /\ pending_switch = "none" /\ switch_timer = 0) =>
        /\ tv_input = SERVER_HOST
        /\ cursor = SERVER_HOST
        /\ capture = "idle"

\* No command in flight when daemon is dead.
NoPendingWhenDead ==
    (~daemon_healthy) => pending_switch = "none"

\* CENTRAL AVAILABILITY INVARIANT:
\* If a remote host is selected but has no signal or is offline,
\* the system must revert to the lan-mouse server host.
\* switch_timer > 0 means revert is already in progress.
ServerHostAlwaysAvailable ==
    (daemon_healthy /\ tv_mode = "fullscreen" /\ tv_input \in RemoteHosts) =>
        (input_signal[tv_input] = TRUE /\ RemoteReadyForControl(tv_input)
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
    /\ tv_input = SERVER_HOST
    /\ cursor = SERVER_HOST
    /\ capture = "idle"
    /\ input_owner = SERVER_HOST
    /\ ws_state = "disconnected"
    /\ subscribe_active = FALSE
    /\ daemon_healthy = FALSE    \* SSAP not yet connected
    /\ pending_switch = "none"
    /\ reconnect_count = 0
    /\ switch_timer = 0
    /\ wake_timer = 0
    /\ input_signal = [h \in ActiveInput |-> h = SERVER_HOST]
    /\ remote_online = [h \in RemoteHosts |-> FALSE]
    /\ remote_input_ready = [
         h \in RemoteHosts |-> ["keyboard" |-> FALSE, "pointer" |-> FALSE]
       ]
    /\ wake_pending = "none"

\* All variables tuple (used by Spec for stuttering).
vars == <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
          subscribe_active, daemon_healthy, pending_switch, reconnect_count,
          switch_timer, wake_timer, input_signal, remote_online,
          remote_input_ready, wake_pending>>

\* =====================================================================
\* SSAP LIFECYCLE (persistent wss:// connection, replaces subprocess-per-command)
\* =====================================================================

\* TCP + TLS handshake to wss://TV_IP:3001/.
SSAPConnecting ==
    /\ ws_state = "disconnected"
    /\ ws_state' = "connecting"
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* SSAP register handshake: send client-key, receive registration confirmation.
SSAPRegistering ==
    /\ ws_state = "connecting"
    /\ ws_state' = "registering"
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* Registration complete. SSAP socket is connected, but commands are not
\* accepted until subscription is live.
SSAPRegistered ==
    /\ ws_state = "registering"
    /\ ws_state' = "connected"
    /\ daemon_healthy' = FALSE
    /\ reconnect_count' = 0
    \* Can't know TV state during disconnect; resync from subscribe + signal query.
    /\ tv_mode' \in {"fullscreen", "multiview"}
    /\ tv_input' \in ActiveInput
    \* Query signal status on reconnect to resolve stale state.
    /\ input_signal' \in [ActiveInput -> BOOLEAN]
    /\ UNCHANGED <<cursor, capture, input_owner, subscribe_active,
                   pending_switch, switch_timer, wake_timer, remote_online,
                   remote_input_ready, wake_pending>>

\* Subscribe to multiViewStatus push updates from TV.
\* daemon_healthy only becomes TRUE after subscription is live
\* (HealthDefinition: connected AND subscribed).
SSAPSubscribe ==
    /\ ws_state = "connected"
    /\ ~subscribe_active
    /\ subscribe_active' = TRUE
    /\ daemon_healthy' = TRUE
    /\ IF tv_mode = "fullscreen" /\ input_owner = SERVER_HOST /\ tv_input # SERVER_HOST THEN
           /\ tv_mode' = "transitioning"
           /\ tv_input' = SERVER_HOST
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
       ELSE
           /\ UNCHANGED <<tv_mode, tv_input, pending_switch, switch_timer>>
    /\ UNCHANGED <<cursor, capture, input_owner, ws_state,
                   reconnect_count, wake_timer,
                   input_signal, remote_online, remote_input_ready, wake_pending>>

\* WebSocket drops (TV reboot, WiFi blip, TV off). TV commands are unavailable
\* until reconnect, so display may remain stale. lan-mouse input still falls
\* back immediately to SERVER_HOST; SSAPSubscribe resyncs display after the
\* persistent TV control path is healthy again.
SSAPDisconnect ==
    /\ ws_state = "connected"
    /\ ws_state' = "disconnected"
    /\ subscribe_active' = FALSE
    /\ daemon_healthy' = FALSE
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ UNCHANGED <<tv_mode, tv_input,
                   reconnect_count, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* =====================================================================
\* SUBSCRIPTION CALLBACK (push updates from TV)
\* =====================================================================

\* TV reports multiViewStatus change via subscribe callback.
\* Can fire at any time while subscribed — handles race with pending_switch.
\* C4 fix: remote can only set fullscreen or multiview, never transitioning.
SubscriptionFires ==
    /\ subscribe_active
    /\ \E reported_mode \in {tv_mode} \cup ({"fullscreen", "multiview"} \ {tv_mode}) :
       \E reported_input \in ActiveInput :
       IF (reported_mode = "fullscreen"
           /\ input_owner = SERVER_HOST
           /\ reported_input \in RemoteHosts) THEN
           \* TV remote/manual override moved display away while lan-mouse input
           \* is on the server host. Treat it as a resync event and return the
           \* display to SERVER_HOST.
           /\ tv_mode' = "transitioning"
           /\ tv_input' = SERVER_HOST
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
       ELSE
           /\ tv_mode' = reported_mode
           /\ tv_input' = reported_input
           /\ IF pending_switch /= "none" THEN
                  /\ pending_switch' = "none"  \* C4: remote override clears pending
              ELSE
                  /\ UNCHANGED pending_switch
           /\ switch_timer' = 0                \* cancel any in-flight timeout
    /\ UNCHANGED <<cursor, capture, input_owner, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* =====================================================================
\* SWITCH TRANSITIONS (EnterOtherHost → SwitchComplete | SwitchFailed | SwitchTimeout)
\* =====================================================================

\* =====================================================================
\* TWO-PHASE SWITCH PROTOCOL (atomicity invariant: cursor never moves to dead host)
\*
\* Phase 1 (EnterOtherHost): TV input switches. Cursor + capture STAY on
\*   SERVER_HOST.
\*   The daemon issues set_input(target) and awaits two confirmations:
\*   (a) SSAP response: set_input() returned success.
\*   (b) Signal verification: target input has HDMI signal present.
\*   If (a) fails → SwitchFailed. If (b) fails within SWITCH_TIMEOUT → SwitchTimeout.
\*   In both failure cases, cursor never left SERVER_HOST — user is never trapped.
\*
\* Phase 2 (SwitchComplete): Both confirmations received. NOW move cursor + capture
\*   to target. The HTTP response returns "fullscreen" to lan-mouse, which then
\*   switches its own capture to the remote host. Cursor only moves AFTER the
\*   display is verified working.
\*
\* This eliminates the trap scenario: switch to sleeping/shutdown Windows →
\*   display shows nothing → keyboard/mouse captured by dead host → stuck.
\* =====================================================================

\* Phase 1: Cursor crosses edge. TV input switches. Cursor + capture STAY on
\* SERVER_HOST.
\* APPROACH 1: ALWAYS issues set_input() — no stale-state no-op guard.
\* switch_timer starts countdown for signal verification.
\* GUARD: target host must be online and ready for BOTH keyboard and pointer.
\*   If offline, SendWoL fires instead — wake the host, then auto-retry.
\*   If online but missing keyboard/pointer capability, reject the enter and
\*   keep input_owner=SERVER_HOST; do not split keyboard from mouse.
EnterOtherHost(host) ==
    /\ host \in RemoteHosts
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"          \* debounce: only one switch at a time
    /\ cursor = SERVER_HOST
    /\ input_owner = SERVER_HOST        \* keyboard+mouse still local
    /\ daemon_healthy                   \* requires connected + subscribed
    /\ ws_state = "connected"
    /\ RemoteReadyForControl(host)      \* online + keyboard + pointer ready
    /\ tv_mode' = "transitioning"
    /\ tv_input' = host
\* INPUT STAYS ON SERVER_HOST — keyboard and mouse move only after verification.
    /\ pending_switch' = host
    /\ switch_timer' = SWITCH_TIMEOUT   \* start signal-verification countdown
    /\ UNCHANGED <<cursor, capture, input_owner, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* Phase 2a: set_input() acknowledged, target signal is present, and the
\* pending remote target is still online and ready for both keyboard+pointer.
\* NOW move keyboard+mouse ownership to target. Return success to lan-mouse.
SwitchComplete ==
    /\ tv_mode = "transitioning"
    /\ pending_switch \in RemoteHosts
    /\ tv_input = pending_switch
    /\ input_signal[tv_input] = TRUE    \* authoritative signal check
    /\ RemoteReadyForControl(tv_input)  \* re-check spoke readiness at commit
    /\ tv_mode' = "fullscreen"
    /\ input_owner' = tv_input
    /\ cursor' = input_owner'           \* mouse follows keyboard atomically
    /\ capture' = CaptureFor(input_owner')
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ UNCHANGED <<tv_input, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* Server-host fallback/return completion. This is intentionally separate from
\* remote SwitchComplete so the remote completion path cannot accidentally fire
\* during ReturnToServerHost or failure recovery.
ServerHostSwitchComplete ==
    /\ tv_mode = "transitioning"
    /\ pending_switch = SERVER_HOST
    /\ tv_input = SERVER_HOST
    /\ input_signal[SERVER_HOST] = TRUE
    /\ tv_mode' = "fullscreen"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ UNCHANGED <<tv_input, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* Phase 2b: set_input() returned SSAP error.
\* Cursor never left SERVER_HOST. Revert/confirm server-host display and keep
\* keyboard+mouse local.
SwitchFailed ==
    /\ tv_mode = "transitioning"
    /\ ws_state = "connected"
    /\ tv_mode' = "fullscreen"
    /\ tv_input' = SERVER_HOST
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy, reconnect_count,
                   wake_timer, input_signal, remote_online, remote_input_ready,
                   wake_pending>>

\* Phase 2b: Timer expired — set_input() succeeded but no signal on target.
\* Cursor never left SERVER_HOST. Revert/confirm server-host display and keep
\* keyboard+mouse local.
SwitchTimeout ==
    /\ tv_mode = "transitioning"
    /\ switch_timer = 1                \* last tick before expiry
    /\ ~(pending_switch \in RemoteHosts /\ tv_input = pending_switch
          /\ input_signal[tv_input] = TRUE /\ RemoteReadyForControl(tv_input))
    /\ ~(pending_switch = SERVER_HOST /\ tv_input = SERVER_HOST
          /\ input_signal[SERVER_HOST] = TRUE)
    /\ switch_timer' = 0
    /\ tv_mode' = "fullscreen"
    /\ tv_input' = SERVER_HOST
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = "none"
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy, reconnect_count,
                   wake_timer, input_signal, remote_online, remote_input_ready,
                   wake_pending>>

\* Timer tick — models time passing during switch.
TimerTick ==
    /\ switch_timer > 1
    /\ switch_timer' = switch_timer - 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

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
    /\ IF tv_mode = "fullscreen" /\ tv_input \in RemoteHosts
          /\ input_signal'[tv_input] = FALSE THEN
           /\ switch_timer' = SWITCH_TIMEOUT
       ELSE
           /\ UNCHANGED switch_timer
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, wake_timer, remote_online,
                   remote_input_ready, wake_pending>>

\* Signal-loss revert: fullscreen on remote, signal just went dead, timer expired.
\* Revert display to SERVER_HOST and force keyboard+mouse back to local ownership.
SignalLossRevert ==
    /\ tv_mode = "fullscreen"
    /\ tv_input \in RemoteHosts
    /\ ~input_signal[tv_input]
    /\ switch_timer = 1
    /\ switch_timer' = 0
    /\ tv_mode' = "transitioning"
    /\ tv_input' = SERVER_HOST
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = SERVER_HOST
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy, reconnect_count,
                   wake_timer, input_signal, remote_online, remote_input_ready,
                   wake_pending>>

\* =====================================================================
\* REMOTE HOST HEALTH (lan-mouse spoke connectivity)
\* =====================================================================

\* Remote host is offline — instead of failing the switch, wake it first.
\* C11: Send WoL, set wake_pending, and return "waking" to lan-mouse.
\* The user's keyboard+mouse stay on SERVER_HOST; no capture switch happens.
\* When RemoteHostOnline fires → WakeAndRetry automatically re-enters.
SendWoL(host) ==
    /\ host \in RemoteHosts
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"
    /\ cursor = SERVER_HOST
    /\ input_owner = SERVER_HOST
    /\ daemon_healthy
    /\ ws_state = "connected"
    /\ ~remote_online[host]            \* host is asleep/offline
    /\ wake_pending = "none"
    /\ wake_pending' = host
    /\ wake_timer' = WAKE_TIMEOUT
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, input_signal, remote_online,
                   remote_input_ready>>
                   \* wake_pending and wake_timer set explicitly above

\* Time passes while waiting for a host to wake.
WakeTimerTick ==
    /\ wake_pending \in RemoteHosts
    /\ wake_timer > 1
    /\ wake_timer' = wake_timer - 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* Host did not wake. Cancel the pending wake and settle display/input on
\* SERVER_HOST.
WakeTimeout ==
    /\ wake_pending \in RemoteHosts
    /\ wake_timer = 1
    /\ wake_timer' = 0
    /\ wake_pending' = "none"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ IF daemon_healthy /\ ws_state = "connected" THEN
           /\ tv_mode' = "fullscreen"
           /\ tv_input' = SERVER_HOST
       ELSE
           /\ UNCHANGED <<tv_mode, tv_input>>
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy,
                   reconnect_count, input_signal, remote_online,
                   remote_input_ready>>

\* Now that the host is online and both keyboard+pointer paths are ready,
\* EnterOtherHost(host) becomes enabled.
WakeAndRetry(host) ==
    /\ host \in RemoteHosts
    /\ wake_pending = host
    /\ RemoteReadyForControl(host)     \* host woke up and input paths are ready
    /\ wake_pending' = "none"
    /\ wake_timer' = 0
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, input_signal, remote_online,
                   remote_input_ready>>
                   \* wake_pending and wake_timer set explicitly above

\* Online spoke readiness can change independently from connectivity.
\* Both keyboard and pointer must be ready before remote control is allowed.
RemoteInputReadinessUpdate(host) ==
    /\ host \in RemoteHosts
    /\ remote_online[host] = TRUE
    /\ \E next_ready \in [RemoteHosts -> [InputCapabilities -> BOOLEAN]] :
       /\ remote_input_ready' = next_ready
       /\ IF (input_owner = host \/ pending_switch = host)
             /\ ~(next_ready[host]["keyboard"] /\ next_ready[host]["pointer"]) THEN
             /\ input_owner' = SERVER_HOST
             /\ cursor' = SERVER_HOST
             /\ capture' = "idle"
             /\ IF daemon_healthy /\ ws_state = "connected"
                   /\ tv_mode \in {"fullscreen", "transitioning"} THEN
                   /\ tv_mode' = "transitioning"
                   /\ tv_input' = SERVER_HOST
                   /\ pending_switch' = SERVER_HOST
                   /\ switch_timer' = SWITCH_TIMEOUT
                ELSE
                   /\ UNCHANGED <<tv_mode, tv_input, pending_switch, switch_timer>>
          ELSE
             /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                            pending_switch, switch_timer>>
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy,
                   reconnect_count, wake_timer, input_signal, remote_online,
                   wake_pending>>

\* Remote host is online but cannot currently accept keyboard+pointer as
\* one atomic control stream. Reject the enter; do not wake and do not capture.
RemoteInputNotReadyReject(host) ==
    /\ host \in RemoteHosts
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"
    /\ cursor = SERVER_HOST
    /\ input_owner = SERVER_HOST
    /\ daemon_healthy
    /\ ws_state = "connected"
    /\ remote_online[host] = TRUE
    /\ ~RemoteReadyForControl(host)
    /\ tv_input' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ input_owner' = SERVER_HOST
    /\ capture' = "idle"
    /\ switch_timer' = 0
    /\ UNCHANGED <<tv_mode, ws_state, subscribe_active, daemon_healthy,
                   pending_switch, reconnect_count, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* Remote host disconnects (power off, crash, network loss).
\* If currently displaying or switching to that host, initiate revert to
\* SERVER_HOST. If multiView owns input for that host, release input to
\* SERVER_HOST even though the TV remains in multiView.
RemoteHostOffline(host) ==
    /\ host \in RemoteHosts
    /\ remote_online[host] = TRUE
    /\ remote_online' = [remote_online EXCEPT ![host] = FALSE]
    /\ IF tv_mode \in {"fullscreen", "transitioning"}
          /\ (tv_input = host \/ pending_switch = host) THEN
           /\ tv_mode' = "transitioning"
           /\ tv_input' = SERVER_HOST
           /\ input_owner' = SERVER_HOST
           /\ cursor' = SERVER_HOST
           /\ capture' = "idle"
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
       ELSE IF input_owner = host THEN
           /\ input_owner' = SERVER_HOST
           /\ cursor' = SERVER_HOST
           /\ capture' = "idle"
           /\ UNCHANGED <<tv_mode, tv_input, pending_switch, switch_timer>>
       ELSE
           /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                          pending_switch, switch_timer>>
    /\ remote_input_ready' = [remote_input_ready EXCEPT ![host] = ["keyboard" |-> FALSE, "pointer" |-> FALSE]]
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy, reconnect_count,
                   wake_timer, input_signal, wake_pending>>

RemoteHostOnline(host) ==
    /\ host \in RemoteHosts
    /\ remote_online[host] = FALSE
    /\ remote_online' = [remote_online EXCEPT ![host] = TRUE]
    /\ remote_input_ready' \in [RemoteHosts -> [InputCapabilities -> BOOLEAN]]
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
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
    /\ UNCHANGED <<tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, reconnect_count, switch_timer, wake_timer,
                   input_signal, remote_online, remote_input_ready, wake_pending>>

\* M→F: Disable multiView, return to fullscreen.
\* Atomic SSAP call. Restore TV input to the host that owns both keyboard and mouse.
ExitMultiView ==
    /\ tv_mode = "multiview"
    /\ pending_switch = "none"
    /\ daemon_healthy
    /\ ws_state = "connected"
    /\ tv_mode' = "fullscreen"
    /\ tv_input' = CASE input_owner \in Host -> input_owner
                     [] OTHER -> SERVER_HOST
    /\ pending_switch' = "none"
    /\ UNCHANGED <<cursor, capture, input_owner, ws_state, subscribe_active,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* Cursor crosses edge while in multiView: update keyboard+mouse owner only.
\* Do NOT switch TV input — it's showing multiple sources already.
EnterMultiViewHost(host) ==
    /\ host \in RemoteHosts
    /\ tv_mode = "multiview"
    /\ pending_switch = "none"
    /\ RemoteReadyForControl(host)
    /\ input_owner' = host
    /\ cursor' = host
    /\ capture' = CaptureFor(host)
    /\ UNCHANGED <<tv_mode, tv_input, ws_state, subscribe_active,
                   daemon_healthy, pending_switch, reconnect_count,
                   switch_timer, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* =====================================================================
\* RETURN TO SERVER HOST
\* =====================================================================

\* Keyboard+mouse come back to the lan-mouse server host as one atomic unit.
\* Fullscreen: switch TV input back to SERVER_HOST (through transitioning).
\* MultiView or transitioning: leave TV alone, just release capture.
ReturnToServerHost ==
    /\ input_owner \in RemoteHosts
    /\ pending_switch = "none"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ IF tv_mode = "fullscreen" THEN
           /\ daemon_healthy
           /\ ws_state = "connected"
           /\ tv_mode' = "transitioning"
           /\ tv_input' = SERVER_HOST
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
       ELSE
           /\ UNCHANGED <<tv_mode, tv_input, pending_switch, switch_timer,
                          daemon_healthy, ws_state, wake_pending>>
    /\ UNCHANGED <<subscribe_active, reconnect_count,
                   wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* =====================================================================
\* RECONNECT LIFECYCLE
\* =====================================================================

\* Reconnect attempt failed. Exponential backoff external to spec.
ReconnectFails ==
    /\ ~daemon_healthy
    /\ reconnect_count < RECONNECT_CAP
    /\ reconnect_count' = reconnect_count + 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   switch_timer, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* Retry cap reached. This is an explicit stuttering terminal state for the
\* daemon process; systemd Restart=on-failure gives a fresh start with
\* reconnect_count=0 (Init) outside this spec behavior.
DaemonExits ==
    /\ ~daemon_healthy
    /\ reconnect_count = RECONNECT_CAP
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

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
    \/ \E host \in RemoteHosts : EnterOtherHost(host)
    \/ \E host \in RemoteHosts : SendWoL(host)
    \/ WakeTimerTick
    \/ WakeTimeout
    \/ \E host \in RemoteHosts : WakeAndRetry(host)
    \/ SwitchComplete
    \/ ServerHostSwitchComplete
    \/ SwitchFailed
    \/ SwitchTimeout
    \/ TimerTick
    \/ SignalUpdate
    \/ SignalLossRevert
    \/ \E host \in RemoteHosts : RemoteInputReadinessUpdate(host)
    \/ \E host \in RemoteHosts : RemoteInputNotReadyReject(host)
    \/ \E host \in RemoteHosts : RemoteHostOffline(host)
    \/ \E host \in RemoteHosts : RemoteHostOnline(host)
    \/ EnterMultiView
    \/ ExitMultiView
    \/ \E host \in RemoteHosts : EnterMultiViewHost(host)
    \/ ReturnToServerHost
    \/ ReconnectFails
    \/ DaemonExits

Spec == Init /\ [][Next]_vars
          /\ WF_vars(SSAPConnecting)
          /\ WF_vars(SSAPRegistering)
          /\ WF_vars(ReturnToServerHost)
          /\ WF_vars(SSAPRegistered)
          /\ WF_vars(SSAPSubscribe)
          /\ WF_vars(ReconnectFails)
          /\ WF_vars(TimerTick)
          /\ WF_vars(SwitchComplete)
          /\ WF_vars(ServerHostSwitchComplete)
          /\ WF_vars(SwitchTimeout)
          /\ WF_vars(SignalLossRevert)
          /\ WF_vars(WakeTimerTick)
          /\ WF_vars(WakeTimeout)

\* =====================================================================
\* LIVENESS
\* =====================================================================

\* Daemon eventually reconnects or reaches the retry cap.
\* This depends on weak fairness for SSAPConnecting, SSAPRegistering,
\* SSAPRegistered, SSAPSubscribe, and ReconnectFails.
EventuallyReconnect ==
    (~daemon_healthy /\ ws_state = "disconnected")
        ~> (daemon_healthy /\ ws_state = "connected" \/ reconnect_count = RECONNECT_CAP)

\* If a remote host is selected but unusable, eventually revert to SERVER_HOST.
\* This depends on weak fairness for TimerTick, SwitchTimeout, SignalLossRevert,
\* and ServerHostSwitchComplete; otherwise a model can stutter forever with an
\* enabled recovery action.
EventuallyRevert ==
    (tv_mode = "fullscreen" /\ tv_input \in RemoteHosts
     /\ (~input_signal[tv_input] \/ ~RemoteReadyForControl(tv_input)))
        ~> (tv_input = SERVER_HOST /\ input_owner = SERVER_HOST
            /\ cursor = SERVER_HOST /\ capture = "idle")

\* A wake attempt must either complete with remote input readiness or cancel.
\* This depends on weak fairness for WakeTimerTick and WakeTimeout.
EventuallyWakeSettles ==
    (wake_pending \in RemoteHosts)
        ~> (wake_pending = "none")

\* =====================================================================
\* DESIGN DECISIONS
\* =====================================================================

\* C1 (stuck pending): EnterMultiView/ExitMultiView clear pending directly
\*     (atomic SSAP calls). ReturnToServerHost routes through transitioning
\*     like EnterOtherHost. SwitchFailed/SwitchTimeout clear pending.
\* C2 (DisplayMatchesInputOwner violations): Resolved by SwitchFailed,
\*     SwitchTimeout, SignalLossRevert, and RemoteHostOffline — all revert
\*     to SERVER_HOST when the display is unusable. SubscriptionFires handles
\*     TvRemoteOverride races.
\* C3 (deadlock at cap): DaemonExits → systemd restart → fresh Init with
\*     reconnect_count = 0.
\* C4 (TvRemoteOverride): SubscriptionFires clears pending_switch.
\*     tv_mode' restricted to {"fullscreen","multiview"} — remote can't
\*     set transitioning.
\* C5 (liveness violated by user): EventuallyReturn dropped (user may
\*     never return input). Instead: EventuallyRevert ensures the
\*     system recovers from host failure. EventuallyReconnect covers the
\*     SSAP health path. SSAPConnecting, SSAPRegistering, SSAPRegistered,
\*     SSAPSubscribe, ReconnectFails, TimerTick, SwitchTimeout,
\*     SignalLossRevert, ServerHostSwitchComplete, WakeTimerTick, and
\*     WakeTimeout have weak fairness so these liveness properties are
\*     non-vacuous.
\* C6 (stale-state no-op): REMOVED. Approach 1: always issue set_input(),
\*     never skip based on cached tv_input. set_input() is idempotent.
\*     tv_input is now informational-only — not a gating condition.
\* C7 (stale state elimination): tv_input is no longer the authority on
\*     what the TV displays. input_signal (from TV SSAP query) and
\*     remote_online plus remote_input_ready (from lan-mouse spoke) are
\*     the authoritative sources.
\* C8 (always-availability): ServerHostAlwaysAvailable and
\*     ServerHostNormalFallback are enforced by SwitchFailed, SwitchTimeout,
\*     SignalLossRevert, RemoteHostOffline, WakeTimeout, and
\*     RemoteInputNotReadyReject. SSAPDisconnect immediately releases
\*     keyboard+mouse to SERVER_HOST; SSAPSubscribe resyncs the display to
\*     SERVER_HOST after TV control is healthy again. The system never gets
\*     stuck on a dead display or a half-ready input host.
\* C9 (unified design): SSAP lifecycle (ws_state, subscribe_active) and
\*     daemon state machine (tv_mode, tv_input, input_owner, cursor,
\*     capture) are
\*     modeled in one spec. The HealthDefinition invariant ties them
\*     together: daemon_healthy iff connected AND subscribed.
\* C10 (two-phase atomic enter): EnterOtherHost switches only the TV input.
\*     Keyboard and mouse ownership stay on SERVER_HOST during Phase 1.
\*     SwitchComplete moves both keyboard and mouse to the target only after
\*     signal is present, pending_switch still names the target, and
\*     RemoteReadyForControl(target) is re-checked at commit time. SwitchFailed
\*     or SwitchTimeout leaves keyboard+mouse on SERVER_HOST. This prevents the
\*     transition race where a lan-mouse spoke dies after EnterOtherHost but
\*     before SwitchComplete, plus the split-input trap (keyboard remote,
\*     pointer local, or pointer remote, keyboard local).
\* C11 (pre-switch wake): EnterOtherHost requires
\*     RemoteReadyForControl(target) = TRUE for remote hosts. If the
\*     host is asleep/offline, SendWoL fires instead — sends Wake-on-LAN,
\*     sets wake_pending, returns "waking" to lan-mouse. When the host comes
\*     online and reports keyboard+pointer readiness (RemoteHostOnline plus
\*     RemoteInputReadinessUpdate),
\*     WakeAndRetry clears wake_pending, and EnterOtherHost becomes
\*     enabled again (auto-retry). The user never needs to re-trigger
\*     the edge crossing. If the host never wakes, WakeTimeout clears
\*     wake_pending and keeps keyboard+mouse on SERVER_HOST.
\* C12 (synchronous capture gate): lan-mouse must pause capture before the
\*     enter hook and wait for an explicit allow result from tv-multiview.
\*     The old asynchronous "spawn hook after ClientEntered" contract cannot
\*     enforce C10, because input may already be captured/sent before display
\*     and readiness are verified.

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
                   │  │ REMOTE   │  RemoteHostOffline ──→ server fallback│
                   │  │ HOST     │                                       │
                   │  │ OFFLINE  │                                       │
                   │  └──────────┘                                       │
                   └─────────────────────────────────────────────────────┘

Pending switch gating: pending_switch != "none" blocks all new transitions
that produce a TV command (natural debounce).

Failure is always recoverable: SwitchFailed, SwitchTimeout, SignalLossRevert,
WakeTimeout, RemoteInputNotReadyReject, RemoteHostOffline, and
SSAPDisconnect plus SSAPSubscribe recovery all converge to
`tv_input = SERVER_HOST`, `input_owner = SERVER_HOST`,
`cursor = SERVER_HOST`, `capture = "idle"` when TV control is healthy. During
TV-control outage, keyboard+mouse still fall back immediately to SERVER_HOST.
```

### Reliability Design

#### 0. lan-mouse Integration Contract

The TV switch daemon cannot enforce the two-phase protocol by itself. The
lan-mouse side must provide a synchronous enter gate:

1. Pointer crosses the screen edge.
2. lan-mouse pauses remote capture and sends no keyboard, pointer-motion,
   pointer-button, or scroll events to the target yet.
3. lan-mouse calls `/enter/{target}` and waits for an explicit allow/deny
   result.
4. Only an allow result after display signal and remote input readiness are
   verified may move input ownership to the remote host.
5. Any other result (`waking`, `multiview`, `not_ready`, 4xx/5xx, timeout,
   hook crash) keeps keyboard and mouse on the lan-mouse server host as one
   unit.

This is a hard contract. An asynchronous best-effort `enter_hook` that starts
after capture has already begun does not satisfy C10.

Input ownership is atomic: keyboard, pointer motion, pointer buttons, and
scroll events are switched together. The design must never allow a state where
keyboard is sent to a remote host while the pointer remains on the server host,
or where the pointer is captured remotely while keyboard remains local.

#### 1. SSAP Lifecycle (persistent wss://)

The daemon holds one persistent WebSocket connection to the TV.
This eliminates the subprocess-spawn overhead of running a Python command per
operation (previously `bscpylgtvcommand` every 5s). Earlier local observation
showed about 28% CPU in that polling path; the implementation artifact must
record the exact measurement command and workload before treating that number
as a benchmark.

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
curl → /enter/{target}:                               [keyboard+mouse still on SERVER_HOST]
  1. If daemon_healthy=false → 503
  2. If pending_switch != none → 200 current_mode  (debounce)
  3. If mode=multiview → 200 "multiview"           (skip, don't disturb multiView)
  4. If mode=fullscreen → CHECK TARGET ONLINE + INPUT READY:
     a. If remote_online[target] = FALSE:
        → send WoL, set wake_pending=target         [C11: pre-switch wake]
        → return 202 "waking" to lan-mouse
        → if wake_timer expires, clear wake_pending and keep input on SERVER_HOST
        → when host comes online and keyboard+pointer are ready → auto-retry step 4
     b. If remote_online[target] = TRUE but keyboard or pointer is not ready:
        → return 409 "not_ready" to lan-mouse
        → keyboard+mouse stay on SERVER_HOST; no partial switch
     c. If remote_online[target] = TRUE and both keyboard+pointer are ready:
        → TWO-PHASE ENTER:
        i.   set_input(target) always  (APPROACH 1: no stale-state no-op)
        ii.  Await SSAP response:
             ├─ error → SwitchFailed  → revert to SERVER_HOST → 502
             └─ success → Proceed
        iii. Query signal status and re-check remote input readiness:
             ├─ signal present and keyboard+pointer still ready
             │  → SwitchComplete → keyboard+mouse move → 200 "fullscreen"
             └─ no signal or readiness lost
                → SwitchTimeout / readiness fallback → SERVER_HOST → 502
        [CRITICAL: keyboard+mouse only move in step (iii) AFTER signal and
         input readiness are re-verified at commit time. If signal is absent
         or input readiness is incomplete, input never leaves SERVER_HOST —
         user is never trapped.]
```

The key change from the previous design (C6): step 4 no longer checks
`tv_input == target`. It always issues `set_input()`. The TV's actual
state is determined by `input_signal` (from SSAP query), `remote_online`,
and `remote_input_ready` (from lan-mouse spoke), not by cached `tv_input`.

The second key change (C10): keyboard and mouse ownership do NOT move to
the target until signal and remote keyboard+pointer readiness are verified
again at `SwitchComplete`. This prevents the race where a host is ready at
`EnterOtherHost`, its HDMI signal remains present, but its lan-mouse spoke
dies before ownership moves.

The third key change (C11): if the target host is offline, the daemon
sends Wake-on-LAN and defers the switch. When the host wakes up and its
lan-mouse spoke connects and reports keyboard+pointer readiness, the daemon
auto-retries the enter. If wake times out, the daemon clears `wake_pending`
and keeps input on SERVER_HOST. The user never needs to manually re-cross the
screen edge for a successful wake, and is never trapped on a failed wake.

#### 3. Failure Recovery Paths

All failures converge to the same recovery state:
`tv_input = SERVER_HOST`, `input_owner = SERVER_HOST`,
`cursor = SERVER_HOST`, `capture = "idle"`.

| Failure | Detection | Recovery Time | Transition |
|---|---|---|---|
| set_input() SSAP error | Immediate (response code) | <1s | SwitchFailed |
| Source has no HDMI signal | switch_timer expires (5s) | 5s | SwitchTimeout |
| Signal drops after stable connection | SignalUpdate periodic poll | 5s (SWITCH_TIMEOUT) | SignalLossRevert |
| Remote host input readiness missing before enter | lan-mouse hub readiness report | immediate | RemoteInputNotReadyReject |
| Remote host input readiness lost while pending/owned | lan-mouse hub readiness update | immediate fallback or switch timeout | RemoteInputReadinessUpdate → ServerHost fallback |
| Wake attempt never completes | wake_timer expires | 60s | WakeTimeout |
| Remote host spoke disconnects | lan-mouse hub event | <3s | RemoteHostOffline |
| WebSocket disconnects | ping timeout (~15s) | input immediate; display after reconnect | SSAPDisconnect → SSAPSubscribe resync |
| TV reboot | ping timeout → reconnect | input immediate; display after reconnect | SSAPDisconnect → reconnect → SSAPRegistered → SSAPSubscribe resync |

#### 4. Signal Status Tracking

The daemon periodically queries the TV for per-input signal presence
(via SSAP `getExternalInputList` or equivalent endpoint). This is the
**authoritative** source for whether a display is actually usable — it
replaces the old approach of trusting the cached `tv_input` variable.

Query frequency: once after each switch (to confirm signal), then every
10s while a remote host is selected (to detect mid-session signal loss).
No query while on SERVER_HOST (zero overhead for the always-available
baseline).

#### 5. Remote Host Health and Input Readiness Tracking

The daemon monitors lan-mouse spoke connectivity to determine whether
remote hosts are online. If a spoke disconnects while that host's input
is selected, the daemon reverts to SERVER_HOST.

Connectivity is not sufficient for control. Each remote host must also report
that both input paths are ready:

- keyboard injection/receive path
- pointer motion/button/scroll injection/receive path

`RemoteReadyForControl(host)` is true only when the spoke is online and both
input capabilities are available. A host that is online but lacks pointer
capacity, keyboard capacity, or permission for either path is not eligible for
`SwitchComplete`.

This covers:
- macOS powered off after being selected.
- Windows crash/reboot while selected.
- Network loss to the remote host.

#### 6. Observability

**Operational log availability invariant:** every host participating in the
switch path must expose a persistent, known log source before the design can be
considered debuggable. Runtime state seen over SSH is not enough; failure
analysis must be able to reconstruct the previous switch attempt after the
fact.

- Current Linux SERVER_HOST lan-mouse:
  `journalctl --user -u lan-mouse.service`.
- Current Linux SERVER_HOST tv-multiview:
  `journalctl --user -u tv-multiview.service`.
- macOS lan-mouse: `~/Library/Logs/lan-mouse.log` and
  `~/Library/Logs/lan-mouse.err.log`.
- Windows lan-mouse: the scheduled task must redirect stdout and stderr to
  fixed files under `%LOCALAPPDATA%\lan-mouse\`, for example
  `%LOCALAPPDATA%\lan-mouse\lan-mouse.log` and
  `%LOCALAPPDATA%\lan-mouse\lan-mouse.err.log`. The deploy must not rely on
  transient console output or an unverified Event Log source.

**Structured JSON logging (stdout, one object per line):**
```json
{"ts":"...","event":"ssap_connecting","tv_ip":"192.0.2.20"}
{"ts":"...","event":"ssap_registered","client_key_present":true}
{"ts":"...","event":"subscribed","topic":"multiViewStatus"}
{"ts":"...","event":"enter","target":"mac","action":"switch","input":"HDMI_3"}
{"ts":"...","event":"switch_complete","input":"mac","signal":true}
{"ts":"...","event":"switch_timeout","target":"mac","action":"revert_to_server_host","server_host":"linux"}
{"ts":"...","event":"switch_failed","target":"windows","error":"timeout","action":"revert_to_server_host","server_host":"linux"}
{"ts":"...","event":"signal_loss","input":"mac","action":"revert_to_server_host","server_host":"linux"}
{"ts":"...","event":"remote_offline","host":"mac","action":"revert_to_server_host","server_host":"linux"}
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
- Eliminates Python subprocess startup from the heartbeat path (previously
  `bscpylgtvcommand` every 5s). The earlier ~28% CPU observation must be
  tied to a recorded measurement command/workload before it is used as a
  benchmark.
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

### ADR-006: Always-availability — revert to lan-mouse server host on any failure

**Decision:** If a remote host is selected but becomes unusable, the daemon
automatically reverts display and input ownership to the host running the
lan-mouse hub/server. In the current deployment `SERVER_HOST = "linux"`, but
the invariant is `ServerHostAlwaysAvailable`, not Linux-specific.

**Rationale:**
- The user's desktop must always have a usable display
- If macOS shows "No Signal" or is powered off, the user is stuck
  (can't move cursor back because screen edge is unreachable)
- Detect via: SSAP signal query (no HDMI signal), lan-mouse spoke
  disconnect (host offline), SSAP command failure (set_input error)
- All failure paths converge to the same recovery: server-host display,
  server-host input, server-host cursor, idle capture

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
