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
RequestTarget == {"none"} \cup RemoteHosts
ProtocolPhase == {
    "idle",
    "waking",
    "command_pending",
    "verification_pending",
    "grant_pending",
    "remote_owned",
    "multiview_owned",
    "fallback_deferred",
    "fallback_command_pending",
    "fallback_verification_pending"
}

ProtocolType == [
    commanded_input    : ActiveInput,
    phase              : ProtocolPhase,
    switch_epoch       : Nat,
    verified_epoch     : Nat,
    request_target     : RequestTarget,
    request_epoch      : Nat,
    grant_epoch        : Nat,
    reservation_target : RequestTarget,
    reservation_epoch  : Nat,
    keyboard_owner     : Host,
    pointer_owner      : Host,
    fallback_required  : BOOLEAN,
    tv_control_available : BOOLEAN
]

\* --- CONSTANTS ---

CONSTANTS SWITCH_TIMEOUT, WAKE_TIMEOUT, RECONNECT_CAP

ASSUME /\ SWITCH_TIMEOUT \in Nat /\ SWITCH_TIMEOUT > 0
       /\ WAKE_TIMEOUT \in Nat /\ WAKE_TIMEOUT > 0
       /\ RECONNECT_CAP \in Nat /\ RECONNECT_CAP > 0

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
    tv_input,           \* last TV-observed active input; never set by command intent
    cursor,             \* cursor ownership host, not pixel coordinates
    capture,            \* lan-mouse capture state
    input_owner,         \* atomic keyboard+mouse owner; never split
    ws_state,           \* SSAP WebSocket lifecycle
    subscribe_active,   \* is multiViewStatus subscription live
    daemon_healthy,     \* daemon + SSAP combined health (commands accepted)
    pending_switch,     \* in-flight command (debounce gate)
    reconnect_count,    \* consecutive failed reconnect attempts
    switch_timer,       \* countdown for no-signal detection after switch
    wake_timer,         \* countdown for host wake attempt
    input_signal,       \* cached TV observation; epoch proves transaction freshness
    remote_online,      \* per-remote-host lan-mouse spoke connectivity
    remote_input_ready, \* per-host keyboard+pointer injection readiness
    wake_pending,       \* host being woken via WoL before retry ("none" or RemoteHosts)
    protocol            \* command, observation, reservation, grant, and owner state

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
    /\ reconnect_count \in Nat
    /\ switch_timer \in 0..SWITCH_TIMEOUT
    /\ wake_timer \in 0..WAKE_TIMEOUT
    /\ input_signal \in [ActiveInput -> BOOLEAN]
    /\ remote_online \in [RemoteHosts -> BOOLEAN]
    \* String keys are intentional: InputCapabilities is a set of string
    \* values, so each per-host readiness map is a total function over them.
    /\ remote_input_ready \in [RemoteHosts -> [InputCapabilities -> BOOLEAN]]
    /\ wake_pending \in ({"none"} \cup RemoteHosts)
    /\ protocol \in ProtocolType

\* Keyboard and mouse are never independently switched. The local user's
\* physical input is one unit: pointer motion, pointer buttons, scroll, and
\* keyboard events must be owned by the same host at every visible state.
InputOwnershipAtomic ==
    /\ protocol.keyboard_owner = protocol.pointer_owner
    /\ protocol.keyboard_owner = input_owner
    /\ (input_owner = SERVER_HOST) =>
         (cursor = SERVER_HOST /\ capture = "idle")
    /\ \A host \in RemoteHosts :
         (input_owner = host) => (cursor = host /\ capture = CaptureFor(host))

RemoteReadyForControl(host) ==
    /\ host \in RemoteHosts
    /\ remote_online[host] = TRUE
    /\ remote_input_ready[host]["keyboard"] = TRUE
    /\ remote_input_ready[host]["pointer"] = TRUE

ReservationValid(host) ==
    /\ host \in RemoteHosts
    /\ protocol.reservation_target = host
    /\ protocol.reservation_epoch = protocol.request_epoch
    /\ RemoteReadyForControl(host)

FreshRemoteVerification(host) ==
    /\ protocol.phase = "verification_pending"
    /\ protocol.commanded_input = host
    /\ protocol.verified_epoch = protocol.switch_epoch
    /\ tv_input = host
    /\ input_signal[host]

FreshServerVerification ==
    /\ protocol.phase = "fallback_verification_pending"
    /\ protocol.commanded_input = SERVER_HOST
    /\ protocol.verified_epoch = protocol.switch_epoch
    /\ tv_input = SERVER_HOST
    /\ input_signal[SERVER_HOST]

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
     /\ pending_switch = "none" /\ switch_timer = 0
     /\ protocol.phase = "idle" /\ ~protocol.fallback_required) =>
        /\ tv_input = SERVER_HOST
        /\ input_signal[SERVER_HOST]
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
        ((input_signal[tv_input] = TRUE /\ RemoteReadyForControl(tv_input))
         \/ protocol.fallback_required \/ switch_timer > 0)

FallbackReleasesInputImmediately ==
    protocol.fallback_required =>
      /\ input_owner = SERVER_HOST
      /\ protocol.keyboard_owner = SERVER_HOST
      /\ protocol.pointer_owner = SERVER_HOST

GrantIsFresh ==
    (protocol.phase = "grant_pending") =>
      /\ protocol.request_target \in RemoteHosts
      /\ protocol.grant_epoch = protocol.request_epoch
      /\ ReservationValid(protocol.request_target)
      /\ protocol.verified_epoch = protocol.switch_epoch
      /\ tv_input = protocol.request_target
      /\ input_signal[protocol.request_target]

RemoteOwnershipHasLease ==
    \A host \in RemoteHosts :
      (input_owner = host) =>
        /\ ReservationValid(host)
        /\ protocol.phase \in {"remote_owned", "multiview_owned"}

RemoteFullscreenKnownSafe ==
    (tv_mode = "fullscreen" /\ input_owner \in RemoteHosts) =>
      /\ tv_input = input_owner
      /\ ((input_signal[input_owner] /\ ReservationValid(input_owner))
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
         h \in RemoteHosts |-> [cap \in InputCapabilities |-> FALSE]
       ]
    /\ wake_pending = "none"
    /\ protocol = [
         commanded_input |-> SERVER_HOST,
         phase |-> "idle",
         switch_epoch |-> 0,
         verified_epoch |-> 0,
         request_target |-> "none",
         request_epoch |-> 0,
         grant_epoch |-> 0,
         reservation_target |-> "none",
         reservation_epoch |-> 0,
         keyboard_owner |-> SERVER_HOST,
         pointer_owner |-> SERVER_HOST,
         fallback_required |-> FALSE,
         tv_control_available |-> FALSE
       ]

\* All variables tuple (used by Spec for stuttering).
vars == <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
          subscribe_active, daemon_healthy, pending_switch, reconnect_count,
          switch_timer, wake_timer, input_signal, remote_online,
          remote_input_ready, wake_pending, protocol>>

\* =====================================================================
\* SSAP LIFECYCLE (persistent wss:// connection, replaces subprocess-per-command)
\* =====================================================================

\* Environment assumption boundary: this says the TV control path is
\* reachable again; it does not itself complete any lifecycle phase.
TVControlAvailable ==
    /\ ~protocol.tv_control_available
    /\ protocol' = [protocol EXCEPT !.tv_control_available = TRUE]
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* TCP + TLS handshake to wss://TV_IP:3001/.
SSAPConnecting ==
    /\ ws_state = "disconnected"
    /\ ws_state' = "connecting"
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending, protocol>>

\* SSAP register handshake: send client-key, receive registration confirmation.
SSAPRegistering ==
    /\ ws_state = "connecting"
    /\ protocol.tv_control_available
    /\ ws_state' = "registering"
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending, protocol>>

SSAPConnectFailed ==
    /\ ws_state = "connecting"
    /\ ~protocol.tv_control_available
    /\ ws_state' = "disconnected"
    /\ reconnect_count' = reconnect_count + 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                   subscribe_active, daemon_healthy, pending_switch,
                   switch_timer, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending, protocol>>

\* Registration complete. SSAP socket is connected, but commands are not
\* accepted until subscription is live.
SSAPRegistered ==
    /\ ws_state = "registering"
    /\ protocol.tv_control_available
    /\ ws_state' = "connected"
    /\ daemon_healthy' = FALSE
    \* Can't know TV state during disconnect; resync from subscribe + signal query.
    /\ tv_mode' \in {"fullscreen", "multiview"}
    /\ tv_input' \in ActiveInput
    \* Query signal status on reconnect to resolve stale state.
    /\ input_signal' \in [ActiveInput -> BOOLEAN]
    /\ UNCHANGED <<cursor, capture, input_owner, subscribe_active,
                   pending_switch, reconnect_count, switch_timer, wake_timer,
                   remote_online, remote_input_ready, wake_pending, protocol>>

SSAPRegisterFailed ==
    /\ ws_state = "registering"
    /\ ~protocol.tv_control_available
    /\ ws_state' = "disconnected"
    /\ reconnect_count' = reconnect_count + 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                   subscribe_active, daemon_healthy, pending_switch,
                   switch_timer, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending, protocol>>

\* Subscribe to multiViewStatus push updates from TV.
\* daemon_healthy only becomes TRUE after subscription is live
\* (HealthDefinition: connected AND subscribed).
SSAPSubscribe ==
    /\ ws_state = "connected"
    /\ ~subscribe_active
    /\ protocol.tv_control_available
    /\ subscribe_active' = TRUE
    /\ daemon_healthy' = TRUE
    /\ reconnect_count' = 0
    /\ IF protocol.fallback_required
          \/ (tv_mode = "fullscreen" /\ input_owner = SERVER_HOST
              /\ (tv_input # SERVER_HOST
                  \/ ~input_signal[SERVER_HOST])) THEN
           /\ tv_mode' = "transitioning"
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
           /\ protocol' = [protocol EXCEPT
                              !.commanded_input = SERVER_HOST,
                              !.phase = "fallback_command_pending",
                              !.switch_epoch = protocol.switch_epoch + 1,
                              !.reservation_target = "none",
                              !.reservation_epoch = 0,
                              !.grant_epoch = 0,
                              !.keyboard_owner = SERVER_HOST,
                              !.pointer_owner = SERVER_HOST,
                              !.fallback_required = TRUE]
       ELSE
           /\ UNCHANGED <<tv_mode, pending_switch, switch_timer, protocol>>
    /\ UNCHANGED <<cursor, capture, input_owner, ws_state,
                   wake_timer, tv_input, input_signal, remote_online, remote_input_ready,
                   wake_pending>>

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
    /\ wake_timer' = 0
    /\ wake_pending' = "none"
    /\ protocol' = [protocol EXCEPT
                       !.phase = "fallback_deferred",
                       !.request_target = "none",
                       !.reservation_target = "none",
                       !.reservation_epoch = 0,
                       !.grant_epoch = 0,
                       !.keyboard_owner = SERVER_HOST,
                       !.pointer_owner = SERVER_HOST,
                       !.fallback_required = TRUE,
                       !.tv_control_available = FALSE]
    /\ UNCHANGED <<tv_mode, tv_input, reconnect_count, input_signal,
                   remote_online, remote_input_ready>>

\* =====================================================================
\* SUBSCRIPTION CALLBACK (push updates from TV)
\* =====================================================================

\* TV reports multiViewStatus change via subscribe callback.
\* Can fire at any time while subscribed — handles race with pending_switch.
\* A callback matching the active transaction is an expected observation and
\* must not cancel that transaction. Only an unexpected callback is a manual
\* override. TV reports only fullscreen or multiview, never transitioning.
SubscriptionFires ==
    /\ subscribe_active
    /\ \E reported_mode \in {"fullscreen", "multiview"} :
       \E reported_input \in ActiveInput :
       LET expected_remote ==
             protocol.phase \in {
               "command_pending", "verification_pending", "grant_pending",
               "remote_owned"
             }
             /\ reported_mode = "fullscreen"
             /\ reported_input = protocol.request_target
           expected_fallback ==
             protocol.phase \in {
               "fallback_command_pending", "fallback_verification_pending"
             }
             /\ reported_mode = "fullscreen"
             /\ reported_input = SERVER_HOST
           stable_server ==
             protocol.phase = "idle"
             /\ input_owner = SERVER_HOST
             /\ reported_mode = "fullscreen"
             /\ reported_input = SERVER_HOST
             /\ input_signal[SERVER_HOST]
           stable_multiview ==
             tv_mode = "multiview"
             /\ pending_switch = "none"
             /\ reported_mode = "multiview"
       IN
       IF expected_remote \/ expected_fallback
          \/ stable_server \/ stable_multiview THEN
           /\ tv_mode' =
                IF protocol.phase \in {
                     "command_pending", "verification_pending", "grant_pending",
                     "fallback_command_pending", "fallback_verification_pending"
                   }
                   THEN tv_mode
                   ELSE reported_mode
           /\ tv_input' = reported_input
           /\ UNCHANGED <<cursor, capture, input_owner, pending_switch,
                          switch_timer, protocol>>
       ELSE IF reported_mode = "multiview"
               /\ ~protocol.fallback_required THEN
           \* Manual multiView is allowed, but any stale fullscreen grant and
           \* reservation are revoked before exposing the new mode.
           /\ tv_mode' = "multiview"
           /\ tv_input' = reported_input
           /\ input_owner' = SERVER_HOST
           /\ cursor' = SERVER_HOST
           /\ capture' = "idle"
           /\ pending_switch' = "none"
           /\ switch_timer' = 0
           /\ protocol' = [protocol EXCEPT
                              !.phase = "idle",
                              !.request_target = "none",
                              !.grant_epoch = 0,
                              !.reservation_target = "none",
                              !.reservation_epoch = 0,
                              !.keyboard_owner = SERVER_HOST,
                              !.pointer_owner = SERVER_HOST,
                              !.fallback_required = FALSE]
       ELSE
           \* Any unexpected safety-relevant callback revokes remote ownership
           \* and starts or continues verified SERVER_HOST fallback.
           /\ tv_mode' = "transitioning"
           /\ tv_input' = reported_input
           /\ input_owner' = SERVER_HOST
           /\ cursor' = SERVER_HOST
           /\ capture' = "idle"
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
           /\ protocol' = [protocol EXCEPT
                              !.commanded_input = SERVER_HOST,
                              !.phase = "fallback_command_pending",
                              !.switch_epoch = protocol.switch_epoch + 1,
                              !.grant_epoch = 0,
                              !.reservation_target = "none",
                              !.reservation_epoch = 0,
                              !.keyboard_owner = SERVER_HOST,
                              !.pointer_owner = SERVER_HOST,
                              !.fallback_required = TRUE]
    \* A callback that resolves or supersedes a wake request must cancel the
    \* wake timer. Otherwise phase can leave "waking" while wake_pending stays
    \* set forever, disabling WakeTimeout and violating EventuallyWakeSettles.
    /\ IF protocol.phase = "waking" THEN
          /\ wake_timer' = 0
          /\ wake_pending' = "none"
       ELSE
          /\ UNCHANGED <<wake_timer, wake_pending>>
    /\ UNCHANGED <<ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, input_signal,
                   remote_online, remote_input_ready>>

\* =====================================================================
\* SWITCH TRANSITIONS (EnterOtherHost → SwitchComplete | SwitchFailed | SwitchTimeout)
\* =====================================================================

\* =====================================================================
\* TWO-PHASE SWITCH PROTOCOL (atomicity invariant: cursor never moves to dead host)
\*
\* Phase 1 (EnterOtherHost): reserve keyboard+pointer and issue the TV command.
\*   Observed tv_input is unchanged; cursor + capture STAY on SERVER_HOST.
\*   The daemon awaits two separate confirmations:
\*   (a) SSAP response: set_input() returned success.
\*   (b) Signal verification: target input has HDMI signal present.
\*   If (a) fails → SwitchFailed. If (b) fails within SWITCH_TIMEOUT → SwitchTimeout.
\*   In both failure cases, cursor never left SERVER_HOST — user is never trapped.
\*
\* Phase 2 (SwitchComplete): Both confirmations received. Issue an epoch-fenced,
\*   expiring grant to lan-mouse. LanMouseCommitGrant is a separate transition;
\*   only it moves keyboard and pointer ownership together. A delayed response
\*   cannot commit after timeout, cancellation, lease loss, or a newer request.
\*
\* This eliminates the trap scenario: switch to sleeping/shutdown Windows →
\*   display shows nothing → keyboard/mouse captured by dead host → stuck.
\* =====================================================================

\* Phase 1: Cursor crosses edge. Reserve the input bundle and issue the TV
\* command. Observed TV state and input ownership remain unchanged.
\* APPROACH 1: ALWAYS issues set_input() — no stale-state no-op guard.
\* switch_timer starts countdown for signal verification.
\* GUARD: target host must be online and ready for BOTH keyboard and pointer.
\*   If offline, SendWoL preserves one request epoch while lan-mouse polls it.
\*   If online but missing keyboard/pointer capability, reject the enter and
\*   keep input_owner=SERVER_HOST; do not split keyboard from mouse.
EnterOtherHost(host) ==
    /\ host \in RemoteHosts
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"          \* debounce: only one switch at a time
    /\ protocol.phase = "idle"
    /\ ~protocol.fallback_required
    /\ cursor = SERVER_HOST
    /\ input_owner = SERVER_HOST        \* keyboard+mouse still local
    /\ daemon_healthy                   \* requires connected + subscribed
    /\ ws_state = "connected"
    /\ RemoteReadyForControl(host)      \* online + keyboard + pointer ready
    /\ LET request == protocol.request_epoch + 1
           switch == protocol.switch_epoch + 1
       IN
       /\ tv_mode' = "transitioning"
       /\ pending_switch' = host
       /\ switch_timer' = SWITCH_TIMEOUT
       /\ protocol' = [protocol EXCEPT
                          !.commanded_input = host,
                          !.phase = "command_pending",
                          !.switch_epoch = switch,
                          !.request_target = host,
                          !.request_epoch = request,
                          !.grant_epoch = 0,
                          !.reservation_target = host,
                          !.reservation_epoch = request,
                          !.keyboard_owner = SERVER_HOST,
                          !.pointer_owner = SERVER_HOST,
                          !.fallback_required = FALSE]
    \* tv_input and input_signal remain observations; command intent cannot
    \* update them. Input remains on SERVER_HOST until client commit.
    /\ UNCHANGED <<tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, reconnect_count,
                   wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* SSAP acknowledged the command. This does not prove what the TV displays.
SwitchCommandAck ==
    /\ protocol.phase = "command_pending"
    /\ pending_switch \in RemoteHosts
    /\ protocol.commanded_input = pending_switch
    /\ ReservationValid(pending_switch)
    /\ daemon_healthy
    /\ protocol' = [protocol EXCEPT !.phase = "verification_pending"]
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

\* Fresh active-input and signal observations are tagged with the current
\* switch epoch. Cached signal state from an earlier switch cannot satisfy it.
SwitchVerificationObserved ==
    /\ protocol.phase = "verification_pending"
    /\ pending_switch \in RemoteHosts
    /\ protocol.commanded_input = pending_switch
    /\ ReservationValid(pending_switch)
    /\ daemon_healthy
    /\ tv_input' = pending_switch
    /\ input_signal' = [input_signal EXCEPT ![pending_switch] = TRUE]
    /\ protocol' = [protocol EXCEPT
                       !.verified_epoch = protocol.switch_epoch]
    /\ UNCHANGED <<tv_mode, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, remote_online,
                   remote_input_ready, wake_pending>>

\* Phase 2a: issue an expiring grant after command acknowledgement and a fresh
\* matching observation. Ownership remains local until LanMouseCommitGrant.
SwitchComplete ==
    /\ tv_mode = "transitioning"
    /\ pending_switch \in RemoteHosts
    /\ FreshRemoteVerification(pending_switch)
    /\ ReservationValid(pending_switch)
    /\ tv_mode' = "fullscreen"
    /\ switch_timer' = SWITCH_TIMEOUT
    /\ protocol' = [protocol EXCEPT
                       !.phase = "grant_pending",
                       !.grant_epoch = protocol.request_epoch]
    /\ UNCHANGED <<tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

LanMouseCommitGrant ==
    /\ protocol.phase = "grant_pending"
    /\ pending_switch \in RemoteHosts
    /\ protocol.grant_epoch = protocol.request_epoch
    /\ ReservationValid(pending_switch)
    /\ protocol.verified_epoch = protocol.switch_epoch
    /\ tv_input = pending_switch
    /\ input_signal[pending_switch]
    /\ switch_timer > 1
    /\ input_owner' = pending_switch
    /\ cursor' = pending_switch
    /\ capture' = CaptureFor(pending_switch)
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ protocol' = [protocol EXCEPT
                       !.phase = "remote_owned",
                       !.keyboard_owner = pending_switch,
                       !.pointer_owner = pending_switch]
    /\ UNCHANGED <<tv_mode, tv_input, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

GrantTimeout ==
    /\ protocol.phase = "grant_pending"
    /\ (switch_timer = 1
        \/ protocol.grant_epoch # protocol.request_epoch
        \/ ~ReservationValid(protocol.request_target))
    /\ tv_mode' = "transitioning"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = SERVER_HOST
    /\ switch_timer' = SWITCH_TIMEOUT
    /\ protocol' = [protocol EXCEPT
                       !.commanded_input = SERVER_HOST,
                       !.phase = "fallback_command_pending",
                       !.switch_epoch = protocol.switch_epoch + 1,
                       !.grant_epoch = 0,
                       !.reservation_target = "none",
                       !.reservation_epoch = 0,
                       !.keyboard_owner = SERVER_HOST,
                       !.pointer_owner = SERVER_HOST,
                       !.fallback_required = TRUE]
    /\ UNCHANGED <<tv_input, ws_state, subscribe_active, daemon_healthy,
                   reconnect_count, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* Server-host fallback/return completion. This is intentionally separate from
\* remote SwitchComplete so the remote completion path cannot accidentally fire
\* during ReturnToServerHost or failure recovery.
ServerHostSwitchComplete ==
    /\ tv_mode = "transitioning"
    /\ pending_switch = SERVER_HOST
    /\ FreshServerVerification
    /\ tv_mode' = "fullscreen"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = "none"
    /\ switch_timer' = 0
    /\ protocol' = [protocol EXCEPT
                       !.phase = "idle",
                       !.request_target = "none",
                       !.grant_epoch = 0,
                       !.reservation_target = "none",
                       !.reservation_epoch = 0,
                       !.keyboard_owner = SERVER_HOST,
                       !.pointer_owner = SERVER_HOST,
                       !.fallback_required = FALSE]
    /\ UNCHANGED <<tv_input, ws_state, subscribe_active,
                   daemon_healthy, reconnect_count, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

FallbackCommandAck ==
    /\ protocol.phase = "fallback_command_pending"
    /\ pending_switch = SERVER_HOST
    /\ daemon_healthy
    /\ protocol' = [protocol EXCEPT
                       !.phase = "fallback_verification_pending"]
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending>>

FallbackVerificationObserved ==
    /\ protocol.phase = "fallback_verification_pending"
    /\ pending_switch = SERVER_HOST
    /\ daemon_healthy
    /\ tv_input' = SERVER_HOST
    /\ input_signal' = [input_signal EXCEPT ![SERVER_HOST] = TRUE]
    /\ protocol' = [protocol EXCEPT
                       !.verified_epoch = protocol.switch_epoch]
    /\ UNCHANGED <<tv_mode, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, remote_online,
                   remote_input_ready, wake_pending>>

\* Phase 2b: set_input() returned SSAP error.
\* Cursor never left SERVER_HOST. Revert/confirm server-host display and keep
\* keyboard+mouse local.
SwitchFailed ==
    /\ tv_mode = "transitioning"
    /\ protocol.phase = "command_pending"
    /\ pending_switch \in RemoteHosts
    /\ ws_state = "connected"
    /\ tv_mode' = "transitioning"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = SERVER_HOST
    /\ switch_timer' = SWITCH_TIMEOUT
    /\ protocol' = [protocol EXCEPT
                       !.commanded_input = SERVER_HOST,
                       !.phase = "fallback_command_pending",
                       !.switch_epoch = protocol.switch_epoch + 1,
                       !.grant_epoch = 0,
                       !.reservation_target = "none",
                       !.reservation_epoch = 0,
                       !.keyboard_owner = SERVER_HOST,
                       !.pointer_owner = SERVER_HOST,
                       !.fallback_required = TRUE]
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy, reconnect_count,
                   tv_input, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* Phase 2b: Timer expired — set_input() succeeded but no signal on target.
\* Cursor never left SERVER_HOST. Revert/confirm server-host display and keep
\* keyboard+mouse local.
SwitchTimeout ==
    /\ tv_mode = "transitioning"
    /\ protocol.phase \in {"command_pending", "verification_pending"}
    /\ pending_switch \in RemoteHosts
    /\ switch_timer = 1                \* last tick before expiry
    /\ switch_timer' = SWITCH_TIMEOUT
    /\ tv_mode' = "transitioning"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = SERVER_HOST
    /\ protocol' = [protocol EXCEPT
                       !.commanded_input = SERVER_HOST,
                       !.phase = "fallback_command_pending",
                       !.switch_epoch = protocol.switch_epoch + 1,
                       !.grant_epoch = 0,
                       !.reservation_target = "none",
                       !.reservation_epoch = 0,
                       !.keyboard_owner = SERVER_HOST,
                       !.pointer_owner = SERVER_HOST,
                       !.fallback_required = TRUE]
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy, reconnect_count,
                   tv_input, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* Fallback timeout never declares success. It reissues the idempotent server
\* command under a new switch epoch and remains degraded until fresh verify.
FallbackTimeout ==
    /\ protocol.phase \in {
         "fallback_command_pending", "fallback_verification_pending"
       }
    /\ pending_switch = SERVER_HOST
    /\ switch_timer = 1
    /\ daemon_healthy
    /\ switch_timer' = SWITCH_TIMEOUT
    /\ protocol' = [protocol EXCEPT
                       !.commanded_input = SERVER_HOST,
                       !.phase = "fallback_command_pending",
                       !.switch_epoch = protocol.switch_epoch + 1,
                       !.fallback_required = TRUE]
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* Timer tick — models time passing during switch.
TimerTick ==
    /\ switch_timer > 1
    /\ switch_timer' = switch_timer - 1
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending, protocol>>

\* =====================================================================
\* SIGNAL STATUS TRACKING (epoch-tagged TV observations)
\* =====================================================================

\* Periodic single-flight query to TV: getExternalInputList or equivalent.
\* The result is an observation, not permanent truth. The implementation uses
\* a monotonic schedule; repeated bad polls cannot reset an armed deadline.
SignalUpdate ==
    /\ ws_state = "connected"
    /\ input_signal' \in [ActiveInput -> BOOLEAN]
    /\ IF daemon_healthy
          /\ tv_mode = "fullscreen"
          /\ input_owner = SERVER_HOST
          /\ protocol.phase = "idle"
          /\ input_signal'[SERVER_HOST] = FALSE THEN
           /\ tv_mode' = "transitioning"
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
           /\ protocol' = [protocol EXCEPT
                              !.commanded_input = SERVER_HOST,
                              !.phase = "fallback_command_pending",
                              !.switch_epoch = protocol.switch_epoch + 1,
                              !.grant_epoch = 0,
                              !.reservation_target = "none",
                              !.reservation_epoch = 0,
                              !.keyboard_owner = SERVER_HOST,
                              !.pointer_owner = SERVER_HOST,
                              !.fallback_required = TRUE]
       ELSE IF protocol.phase = "grant_pending"
               /\ protocol.request_target \in RemoteHosts
               /\ input_signal'[protocol.request_target] = FALSE THEN
           /\ tv_mode' = "transitioning"
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
           /\ protocol' = [protocol EXCEPT
                              !.commanded_input = SERVER_HOST,
                              !.phase = "fallback_command_pending",
                              !.switch_epoch = protocol.switch_epoch + 1,
                              !.grant_epoch = 0,
                              !.reservation_target = "none",
                              !.reservation_epoch = 0,
                              !.keyboard_owner = SERVER_HOST,
                              !.pointer_owner = SERVER_HOST,
                              !.fallback_required = TRUE]
       ELSE IF tv_mode = "fullscreen" /\ tv_input \in RemoteHosts
          /\ protocol.phase = "remote_owned"
          /\ input_signal'[tv_input] = FALSE THEN
           /\ UNCHANGED <<tv_mode, pending_switch, protocol>>
           /\ IF switch_timer = 0 THEN
                  /\ switch_timer' = SWITCH_TIMEOUT
              ELSE
                  /\ UNCHANGED switch_timer
       ELSE IF protocol.phase = "remote_owned"
               /\ switch_timer > 0
               /\ input_signal'[tv_input] = TRUE THEN
           /\ switch_timer' = 0
           /\ UNCHANGED <<tv_mode, pending_switch, protocol>>
       ELSE
           /\ UNCHANGED switch_timer
           /\ UNCHANGED <<tv_mode, pending_switch, protocol>>
    /\ UNCHANGED <<tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, reconnect_count,
                   wake_timer, remote_online, remote_input_ready, wake_pending>>

\* Signal-loss revert: fullscreen on remote, signal just went dead, timer expired.
\* Revert display to SERVER_HOST and force keyboard+mouse back to local ownership.
SignalLossRevert ==
    /\ tv_mode = "fullscreen"
    /\ tv_input \in RemoteHosts
    /\ protocol.phase = "remote_owned"
    /\ ~input_signal[tv_input]
    /\ switch_timer = 1
    /\ daemon_healthy
    /\ switch_timer' = SWITCH_TIMEOUT
    /\ tv_mode' = "transitioning"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ pending_switch' = SERVER_HOST
    /\ protocol' = [protocol EXCEPT
                       !.commanded_input = SERVER_HOST,
                       !.phase = "fallback_command_pending",
                       !.switch_epoch = protocol.switch_epoch + 1,
                       !.grant_epoch = 0,
                       !.reservation_target = "none",
                       !.reservation_epoch = 0,
                       !.keyboard_owner = SERVER_HOST,
                       !.pointer_owner = SERVER_HOST,
                       !.fallback_required = TRUE]
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy, reconnect_count,
                   tv_input, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

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
    /\ protocol.phase = "idle"
    /\ LET request == protocol.request_epoch + 1 IN
       /\ wake_pending' = host
       /\ wake_timer' = WAKE_TIMEOUT
       /\ protocol' = [protocol EXCEPT
                          !.phase = "waking",
                          !.request_target = host,
                          !.request_epoch = request,
                          !.grant_epoch = 0,
                          !.reservation_target = "none",
                          !.reservation_epoch = 0,
                          !.keyboard_owner = SERVER_HOST,
                          !.pointer_owner = SERVER_HOST]
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
                   remote_input_ready, wake_pending, protocol>>

\* Host did not wake. Cancel the pending wake. Settle only when SERVER_HOST
\* signal is still present; otherwise preserve honest degraded state and enter
\* the same verified fallback transaction used by every other failure.
WakeTimeout ==
    /\ wake_pending \in RemoteHosts
    /\ protocol.phase = "waking"
    /\ wake_timer = 1
    /\ ~RemoteReadyForControl(wake_pending)
    /\ wake_timer' = 0
    /\ wake_pending' = "none"
    /\ input_owner' = SERVER_HOST
    /\ cursor' = SERVER_HOST
    /\ capture' = "idle"
    /\ IF input_signal[SERVER_HOST] THEN
          /\ tv_mode' = "fullscreen"
          /\ pending_switch' = "none"
          /\ switch_timer' = 0
          /\ protocol' = [protocol EXCEPT
                             !.phase = "idle",
                             !.request_target = "none",
                             !.grant_epoch = 0,
                             !.reservation_target = "none",
                             !.reservation_epoch = 0,
                             !.keyboard_owner = SERVER_HOST,
                             !.pointer_owner = SERVER_HOST,
                             !.fallback_required = FALSE]
       ELSE IF daemon_healthy /\ ws_state = "connected"
               /\ protocol.tv_control_available THEN
          /\ tv_mode' = "transitioning"
          /\ pending_switch' = SERVER_HOST
          /\ switch_timer' = SWITCH_TIMEOUT
          /\ protocol' = [protocol EXCEPT
                             !.commanded_input = SERVER_HOST,
                             !.phase = "fallback_command_pending",
                             !.switch_epoch = protocol.switch_epoch + 1,
                             !.request_target = "none",
                             !.grant_epoch = 0,
                             !.reservation_target = "none",
                             !.reservation_epoch = 0,
                             !.keyboard_owner = SERVER_HOST,
                             !.pointer_owner = SERVER_HOST,
                             !.fallback_required = TRUE]
       ELSE
          /\ tv_mode' = "fullscreen"
          /\ pending_switch' = "none"
          /\ switch_timer' = 0
          /\ protocol' = [protocol EXCEPT
                             !.phase = "fallback_deferred",
                             !.request_target = "none",
                             !.grant_epoch = 0,
                             !.reservation_target = "none",
                             !.reservation_epoch = 0,
                             !.keyboard_owner = SERVER_HOST,
                             !.pointer_owner = SERVER_HOST,
                             !.fallback_required = TRUE]
    /\ UNCHANGED tv_input
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy,
                   reconnect_count, input_signal, remote_online,
                   remote_input_ready>>

\* The original request remains live and is polled by lan-mouse using its
\* request epoch. Readiness advances that same request directly into a held
\* bundle reservation and TV command; no uncorrelated auto-switch is allowed.
WakeAndRetry(host) ==
    /\ host \in RemoteHosts
    /\ wake_pending = host
    /\ protocol.phase = "waking"
    /\ protocol.request_target = host
    /\ RemoteReadyForControl(host)     \* host woke up and input paths are ready
    /\ daemon_healthy
    /\ pending_switch = "none"
    /\ LET switch == protocol.switch_epoch + 1 IN
       /\ wake_pending' = "none"
       /\ wake_timer' = 0
       /\ tv_mode' = "transitioning"
       /\ pending_switch' = host
       /\ switch_timer' = SWITCH_TIMEOUT
       /\ protocol' = [protocol EXCEPT
                          !.commanded_input = host,
                          !.phase = "command_pending",
                          !.switch_epoch = switch,
                          !.grant_epoch = 0,
                          !.reservation_target = host,
                          !.reservation_epoch = protocol.request_epoch]
    /\ UNCHANGED <<tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, reconnect_count,
                   input_signal, remote_online, remote_input_ready>>

\* Online spoke readiness can change independently from connectivity.
\* Both keyboard and pointer must be ready before remote control is allowed.
RemoteInputReadinessUpdate(host) ==
    /\ host \in RemoteHosts
    /\ remote_online[host] = TRUE
    /\ \E next_ready \in [InputCapabilities -> BOOLEAN] :
       /\ remote_input_ready' =
            [remote_input_ready EXCEPT ![host] = next_ready]
       /\ IF (input_owner = host \/ pending_switch = host
               \/ protocol.reservation_target = host)
             /\ ~(next_ready["keyboard"] /\ next_ready["pointer"]) THEN
             /\ input_owner' = SERVER_HOST
             /\ cursor' = SERVER_HOST
             /\ capture' = "idle"
             /\ IF daemon_healthy /\ ws_state = "connected"
                   /\ protocol.tv_control_available THEN
                   /\ tv_mode' = "transitioning"
                   /\ pending_switch' = SERVER_HOST
                   /\ switch_timer' = SWITCH_TIMEOUT
                   /\ protocol' = [protocol EXCEPT
                                      !.commanded_input = SERVER_HOST,
                                      !.phase = "fallback_command_pending",
                                      !.switch_epoch = protocol.switch_epoch + 1,
                                      !.grant_epoch = 0,
                                      !.reservation_target = "none",
                                      !.reservation_epoch = 0,
                                      !.keyboard_owner = SERVER_HOST,
                                      !.pointer_owner = SERVER_HOST,
                                      !.fallback_required = TRUE]
                ELSE
                   /\ pending_switch' = "none"
                   /\ switch_timer' = 0
                   /\ protocol' = [protocol EXCEPT
                                      !.phase = "fallback_deferred",
                                      !.grant_epoch = 0,
                                      !.reservation_target = "none",
                                      !.reservation_epoch = 0,
                                      !.keyboard_owner = SERVER_HOST,
                                      !.pointer_owner = SERVER_HOST,
                                      !.fallback_required = TRUE]
                   /\ UNCHANGED tv_mode
             /\ UNCHANGED tv_input
          ELSE
             /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                            pending_switch, switch_timer, protocol>>
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
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   remote_online, remote_input_ready, wake_pending, protocol>>

\* Remote host disconnects (power off, crash, network loss).
\* If displaying, switching to, reserving, or controlling that host, release
\* both input paths and initiate verified fullscreen SERVER_HOST fallback.
RemoteHostOffline(host) ==
    /\ host \in RemoteHosts
    /\ remote_online[host] = TRUE
    /\ remote_online' = [remote_online EXCEPT ![host] = FALSE]
    /\ IF (input_owner = host \/ pending_switch = host
            \/ protocol.reservation_target = host) THEN
           /\ input_owner' = SERVER_HOST
           /\ cursor' = SERVER_HOST
           /\ capture' = "idle"
           /\ IF daemon_healthy /\ ws_state = "connected"
                 /\ protocol.tv_control_available THEN
                /\ tv_mode' = "transitioning"
                /\ pending_switch' = SERVER_HOST
                /\ switch_timer' = SWITCH_TIMEOUT
                /\ protocol' = [protocol EXCEPT
                                   !.commanded_input = SERVER_HOST,
                                   !.phase = "fallback_command_pending",
                                   !.switch_epoch = protocol.switch_epoch + 1,
                                   !.grant_epoch = 0,
                                   !.reservation_target = "none",
                                   !.reservation_epoch = 0,
                                   !.keyboard_owner = SERVER_HOST,
                                   !.pointer_owner = SERVER_HOST,
                                   !.fallback_required = TRUE]
             ELSE
                /\ pending_switch' = "none"
                /\ switch_timer' = 0
                /\ protocol' = [protocol EXCEPT
                                   !.phase = "fallback_deferred",
                                   !.grant_epoch = 0,
                                   !.reservation_target = "none",
                                   !.reservation_epoch = 0,
                                   !.keyboard_owner = SERVER_HOST,
                                   !.pointer_owner = SERVER_HOST,
                                   !.fallback_required = TRUE]
                /\ UNCHANGED tv_mode
           /\ UNCHANGED tv_input
       ELSE
           /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner,
                          pending_switch, switch_timer, protocol>>
    /\ remote_input_ready' =
         [remote_input_ready EXCEPT
            ![host] = [cap \in InputCapabilities |-> FALSE]]
    /\ UNCHANGED <<ws_state, subscribe_active, daemon_healthy, reconnect_count,
                   wake_timer, input_signal, wake_pending>>

RemoteHostOnline(host) ==
    /\ host \in RemoteHosts
    /\ remote_online[host] = FALSE
    /\ remote_online' = [remote_online EXCEPT ![host] = TRUE]
    /\ remote_input_ready' =
         [remote_input_ready EXCEPT
            ![host] = [cap \in InputCapabilities |-> FALSE]]
    /\ UNCHANGED <<tv_mode, tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, pending_switch,
                   reconnect_count, switch_timer, wake_timer, input_signal,
                   wake_pending, protocol>>

\* =====================================================================
\* MULTIVIEW TRANSITIONS (Side-by-side / PIP)
\* =====================================================================

\* F→M: confirmed result of the serialized splitscreenEnable SSAP command.
\* The implementation keeps the command pending until its response/callback;
\* this abstract action represents that confirmed completion.
EnterMultiView ==
    /\ tv_mode = "fullscreen"
    /\ pending_switch = "none"
    /\ input_owner = SERVER_HOST
    /\ protocol.phase = "idle"
    /\ ~protocol.fallback_required
    /\ daemon_healthy
    /\ ws_state = "connected"
    /\ tv_mode' = "multiview"
    /\ pending_switch' = "none"
    /\ UNCHANGED <<tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, reconnect_count,
                   switch_timer, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending, protocol>>

\* M→F: begin the serialized disable + verified SERVER_HOST fallback. Input is
\* first released locally; fullscreen is not declared from command intent.
ExitMultiView ==
    /\ tv_mode = "multiview"
    /\ pending_switch = "none"
    /\ input_owner = SERVER_HOST
    /\ protocol.phase = "idle"
    /\ ~protocol.fallback_required
    /\ daemon_healthy
    /\ ws_state = "connected"
    /\ tv_mode' = "transitioning"
    /\ pending_switch' = SERVER_HOST
    /\ switch_timer' = SWITCH_TIMEOUT
    /\ protocol' = [protocol EXCEPT
                       !.commanded_input = SERVER_HOST,
                       !.phase = "fallback_command_pending",
                       !.switch_epoch = protocol.switch_epoch + 1,
                       !.grant_epoch = 0,
                       !.reservation_target = "none",
                       !.reservation_epoch = 0,
                       !.keyboard_owner = SERVER_HOST,
                       !.pointer_owner = SERVER_HOST,
                       !.fallback_required = TRUE]
    /\ UNCHANGED <<tv_input, cursor, capture, input_owner, ws_state,
                   subscribe_active, daemon_healthy, reconnect_count,
                   wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* Cursor crosses edge while in multiView: update keyboard+mouse owner only.
\* Do NOT switch TV input — it's showing multiple sources already.
EnterMultiViewHost(host) ==
    /\ host \in RemoteHosts
    /\ tv_mode = "multiview"
    /\ pending_switch = "none"
    /\ protocol.phase = "idle"
    /\ ~protocol.fallback_required
    /\ RemoteReadyForControl(host)
    /\ input_signal[host]
    /\ LET request == protocol.request_epoch + 1 IN
       /\ input_owner' = host
       /\ cursor' = host
       /\ capture' = CaptureFor(host)
       /\ protocol' = [protocol EXCEPT
                          !.phase = "multiview_owned",
                          !.request_target = host,
                          !.request_epoch = request,
                          !.grant_epoch = request,
                          !.reservation_target = host,
                          !.reservation_epoch = request,
                          !.keyboard_owner = host,
                          !.pointer_owner = host]
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
           /\ pending_switch' = SERVER_HOST
           /\ switch_timer' = SWITCH_TIMEOUT
           /\ protocol' = [protocol EXCEPT
                              !.commanded_input = SERVER_HOST,
                              !.phase = "fallback_command_pending",
                              !.switch_epoch = protocol.switch_epoch + 1,
                              !.grant_epoch = 0,
                              !.reservation_target = "none",
                              !.reservation_epoch = 0,
                              !.keyboard_owner = SERVER_HOST,
                              !.pointer_owner = SERVER_HOST,
                              !.fallback_required = TRUE]
       ELSE
           /\ protocol' = [protocol EXCEPT
                              !.phase = "idle",
                              !.request_target = "none",
                              !.grant_epoch = 0,
                              !.reservation_target = "none",
                              !.reservation_epoch = 0,
                              !.keyboard_owner = SERVER_HOST,
                              !.pointer_owner = SERVER_HOST]
           /\ UNCHANGED <<tv_mode, pending_switch, switch_timer>>
    /\ UNCHANGED <<tv_input, ws_state, daemon_healthy, subscribe_active,
                   reconnect_count, wake_timer, input_signal, remote_online,
                   remote_input_ready, wake_pending>>

\* =====================================================================
\* RECONNECT LIFECYCLE
\* =====================================================================

\* Reconnect failure is tied to the lifecycle phase that actually failed.
\* RECONNECT_CAP is an alert threshold, not a terminal process state.
ReconnectFails ==
    SSAPConnectFailed \/ SSAPRegisterFailed

ReconnectAlert == reconnect_count >= RECONNECT_CAP

RemoteCommandOutcome == SwitchCommandAck \/ SwitchFailed
RemoteVerificationOutcome == SwitchVerificationObserved \/ SwitchTimeout
GrantOutcome == LanMouseCommitGrant \/ GrantTimeout
FallbackCommandOutcome == FallbackCommandAck \/ FallbackTimeout
FallbackVerificationOutcome ==
    FallbackVerificationObserved \/ FallbackTimeout
WakeRetryOutcome ==
    \E host \in RemoteHosts : WakeAndRetry(host)

\* =====================================================================
\* COMPOSITE NEXT
\* =====================================================================

Next ==
    \/ TVControlAvailable
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
    \/ WakeRetryOutcome
    \/ RemoteCommandOutcome
    \/ RemoteVerificationOutcome
    \/ SwitchComplete
    \/ GrantOutcome
    \/ FallbackCommandOutcome
    \/ FallbackVerificationOutcome
    \/ ServerHostSwitchComplete
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

Spec == Init /\ [][Next]_vars
          /\ WF_vars(SSAPConnecting)
          /\ WF_vars(SSAPRegistering)
          /\ WF_vars(SSAPRegistered)
          /\ WF_vars(SSAPSubscribe)
          /\ WF_vars(ReconnectFails)
          /\ WF_vars(TimerTick)
          /\ WF_vars(RemoteCommandOutcome)
          /\ WF_vars(RemoteVerificationOutcome)
          /\ WF_vars(SwitchComplete)
          /\ WF_vars(GrantOutcome)
          /\ WF_vars(FallbackCommandOutcome)
          /\ WF_vars(FallbackVerificationOutcome)
          /\ WF_vars(ServerHostSwitchComplete)
          /\ WF_vars(SignalLossRevert)
          /\ WF_vars(WakeTimerTick)
          \* Readiness updates can alternately enable timeout and retry without
          \* leaving either continuously enabled. These internal outcomes need
          \* strong fairness so an oscillating environment cannot starve both.
          /\ SF_vars(WakeTimeout)
          /\ SF_vars(WakeRetryOutcome)

\* =====================================================================
\* LIVENESS
\* =====================================================================

\* Reconnection is promised only when the environment eventually leaves the
\* TV control path available. A permanently unavailable TV is not a software
\* liveness failure, and transient failures never terminate the daemon.
EventuallyReconnect ==
    (<>[]protocol.tv_control_available) =>
      ((~daemon_healthy /\ ws_state = "disconnected")
        ~> (daemon_healthy /\ ws_state = "connected"))

\* Revert means freshly verified fullscreen SERVER_HOST, not merely that a
\* server command was issued. Input ownership returns immediately; display
\* convergence depends on restored TV control and server HDMI signal.
EventuallyRevert ==
    (<>[]protocol.tv_control_available
     /\ []<>input_signal[SERVER_HOST]) =>
      (protocol.fallback_required
        ~> (tv_mode = "fullscreen"
            /\ tv_input = SERVER_HOST
            /\ input_signal[SERVER_HOST]
            /\ input_owner = SERVER_HOST
            /\ protocol.keyboard_owner = SERVER_HOST
            /\ protocol.pointer_owner = SERVER_HOST
            /\ protocol.phase = "idle"
            /\ ~protocol.fallback_required
            /\ pending_switch = "none"
            /\ cursor = SERVER_HOST /\ capture = "idle"))

\* A wake attempt must either complete with remote input readiness or cancel.
\* This depends on weak fairness for ticking and strong fairness for timeout
\* versus readiness-driven retry when readiness oscillates around the deadline.
EventuallyWakeSettles ==
    (wake_pending \in RemoteHosts)
        ~> (wake_pending = "none")

EventuallyGrantSettles ==
    (protocol.phase = "grant_pending")
        ~> (protocol.phase # "grant_pending")

\* Finite TLC state constraint. It is not part of Spec and does not change
\* production semantics. The finite checker configuration uses reduced timer
\* constants and this bound to cover one request plus a remote/fallback switch.
TLCFiniteState ==
    /\ reconnect_count <= 1
    /\ protocol.request_epoch <= 1
    /\ protocol.switch_epoch <= 2
    /\ protocol.verified_epoch <= 2
    /\ protocol.grant_epoch <= 1
    /\ protocol.reservation_epoch <= 1

\* =====================================================================
\* DESIGN DECISIONS
\* =====================================================================

\* C1 (stuck pending): every command has an outcome or deadline. A remote
\*     failure starts fallback; it does not clear pending and claim recovery.
\* C2 (DisplayMatchesInputOwner violations): Resolved by SwitchFailed,
\*     SwitchTimeout, SignalLossRevert, and RemoteHostOffline — all revert
\*     to SERVER_HOST when the display is unusable. SubscriptionFires handles
\*     TvRemoteOverride races.
\* C3 (reconnect lifecycle): connecting/registering failures return to
\*     disconnected and increment telemetry. RECONNECT_CAP alerts but does not
\*     stop retries; fatal configuration/authentication errors are separate.
\* C4 (TvRemoteOverride): expected callbacks preserve the matching epoch and
\*     target. Unexpected fullscreen callbacks revoke grants and start fallback.
\* C5 (liveness violated by user): EventuallyReturn dropped (user may
\*     never return input). Instead: EventuallyRevert ensures the
\*     system recovers from host failure. EventuallyReconnect covers the
\*     SSAP health path under an explicit eventual-availability assumption.
\*     Internal command, verification, grant, fallback, and timer outcomes
\*     have fairness so the model cannot hide in an internal phase forever.
\* C6 (stale-state no-op): REMOVED. Approach 1: always issue set_input(),
\*     never skip based on cached tv_input. set_input() is idempotent.
\*     tv_input is observed TV state; commanded_input is separate.
\* C7 (stale state elimination): an observation must carry the current
\*     switch_epoch. Cached tv_input/input_signal values cannot complete a new
\*     transaction. Spoke readiness is converted into a held bundle lease.
\* C8 (always-availability): ServerHostAlwaysAvailable and
\*     ServerHostNormalFallback are enforced by SwitchFailed, SwitchTimeout,
\*     SignalLossRevert, RemoteHostOffline, WakeTimeout, and
\*     RemoteInputNotReadyReject. SSAPDisconnect immediately releases
\*     keyboard+mouse to SERVER_HOST; SSAPSubscribe resumes the preserved
\*     fallback intent. Recovery is not declared until fresh server signal and
\*     active-input verification succeed.
\* C9 (unified design): SSAP lifecycle (ws_state, subscribe_active) and
\*     daemon state machine (tv_mode, tv_input, input_owner, cursor,
\*     capture) are
\*     modeled in one spec. The HealthDefinition invariant ties them
\*     together: daemon_healthy iff connected AND subscribed.
\* C10 (fenced enter): reserve keyboard+pointer, issue TV command, acknowledge,
\*     obtain a fresh epoch-tagged observation, issue an expiring grant, then
\*     let lan-mouse atomically commit both owners. These are separate actions.
\* C11 (pre-switch wake): EnterOtherHost requires
\*     RemoteReadyForControl(target) = TRUE for remote hosts. If the
\*     host is asleep/offline, SendWoL fires instead — sends Wake-on-LAN,
\*     sets wake_pending, returns "waking" to lan-mouse. When the host comes
\*     online and reports keyboard+pointer readiness (RemoteHostOnline plus
\*     RemoteInputReadinessUpdate),
\*     WakeAndRetry advances the same request_epoch. lan-mouse polls that
\*     request and never treats an uncorrelated daemon retry as permission.
\* C12 (client commit gate): lan-mouse pauses both input paths, waits/polls for
\*     a matching grant, validates the lease and epoch, then commits keyboard
\*     and pointer together. Timeout or native client failure keeps both local.
\* C13 (single SSAP owner): one actor serializes writes, correlates responses,
\*     and publishes state events. No state mutex is held across an await.

=================================================================================
```

### TLC Verification

Executable artifacts live in `../tla/`:

- `TvDisplaySwitch.tla`: canonical module extracted from the code block above.
- `TvDisplaySwitch.cfg`: candidate production timer values; the unbounded epoch
  state space is intentionally not claimed as exhaustively checked.
- `TvDisplaySwitchFinite.cfg`: both remote hosts, timers reduced to 2, one
  request epoch, and up to two switch epochs through `TLCFiniteState`.
- `tlc-pre-fix/`: preserved parser, invariant, and liveness check inputs from
  the review that found the defects corrected here.

The finite configuration was checked with TLC 2.19 from
`/home/example/.cache/nvim/tla.nvim/tla2tools.jar`. TLC completed with no error:
36,259,841 states generated, 1,064,650 distinct states, depth 28, all twelve
invariants and all four liveness properties checked. This is bounded validation,
not a proof of the unbounded production specification.

## Architecture Design

### Actors

```
┌──────────────┐  DTLS readiness/release  ┌─────────────┐  authenticated HTTP  ┌──────────────┐
│ mac/windows  │ ←───────────────────────→ │ lan-mouse   │ ───────────────────→ │ tv-multiview │
│ spokes       │                           │ hub/server  │  request/poll/commit │ daemon (Rust)│
└──────────────┘                           └─────────────┘                      └──────┬───────┘
                                                                                     │
                                                        persistent SSAP WebSocket     │
                                                                                     ▼
                                                                               ┌──────────┐
                                                                               │ LG TV G4 │
                                                                               └──────────┘
```

External MultiView triggers use the same authenticated axum API but never enter
the cursor-driven grant path.

The Rust daemon holds one persistent wss:// connection to the TV for its
entire lifetime. No subprocess-per-command. No repeated TLS handshakes.
No repeated SSAP register. All SSAP operations (set_input,
set_splitscreen, get_signal_status, subscribe) flow over the same
long-lived WebSocket.

The daemon uses the same .aiopylgtv.sqlite client-key file as LG_Buddy
(at ~/.config/lg-buddy/), so one TV pairing works for both tools.

One dedicated SSAP actor owns the socket, request IDs, response correlation,
subscription callbacks, and a bounded command queue. HTTP handlers send
messages to that actor and never read/write the WebSocket directly. No shared
state lock may be held while awaiting SSAP I/O or lan-mouse commit.

### API Endpoints

| Method | Path | Purpose | Returns |
|---|---|---|---|
| POST | `/enter/{target}` | Create one fenced enter request; reserve both input paths before issuing `set_input()`. | Typed JSON: request ID, epoch, state, deadline |
| GET | `/enter/request/{id}` | Poll a waking or switching request without creating a second switch. | Typed JSON: pending, grant, denied, or fallback |
| POST | `/enter/request/{id}/commit` | Acknowledge that lan-mouse atomically committed the matching keyboard+pointer grant. | Typed JSON commit result |
| POST | `/internal/enter/request/{id}/cancel` | Invalidate one request/lease and begin verified fallback when required. | Typed JSON request state |
| POST | `/internal/enter/request/{id}/renew` | Renew the current active-session lease after identity and readiness checks. | Typed JSON renewal deadline |
| POST | `/internal/readiness/{host}` | Publish named-host online, keyboard, pointer, and session-epoch readiness. | Typed JSON readiness state |
| POST | `/multiview/on` | Enable multiView through the serialized SSAP actor. | Typed JSON command result |
| POST | `/multiview/off` | Disable multiView, release input locally, then verify fullscreen `SERVER_HOST`. | Typed JSON command result |
| GET | `/status` | Health, protocol phase, command/observation epochs, owners, lease, and signal status. | JSON |
| GET | `/health` | Liveness probe (always 200 if process alive). | `"ok"` |
| GET | `/ready` | Readiness probe; 200 only when SSAP is subscribed and no unresolved fallback exists. | Typed JSON readiness |

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
that produce a TV command. A concurrent request receives an explicit `409
busy` containing the active request ID; it never receives a success-shaped
`200 current_mode` response.

The diagram compresses four distinct protocol transitions: SSAP command
acknowledgement, fresh active-input/signal observation, grant issuance, and
lan-mouse commit. `SwitchComplete` issues a grant; only
`LanMouseCommitGrant` changes data-plane ownership.

Failure is always recoverable: SwitchFailed, SwitchTimeout, SignalLossRevert,
WakeTimeout, RemoteInputNotReadyReject, RemoteHostOffline, and
SSAPDisconnect plus SSAPSubscribe recovery all converge to freshly observed
fullscreen `SERVER_HOST`, server HDMI signal present, keyboard and pointer
owners both on `SERVER_HOST`, and idle capture. During a TV-control outage,
input falls back immediately while display recovery remains explicitly
`fallback_deferred`; it is never reported as completed from command intent.
```

### Reliability Design

#### 0. lan-mouse Integration Contract

The TV switch daemon cannot enforce the protocol by itself. The lan-mouse side
must provide a request-correlated commit gate:

1. Pointer crosses the screen edge.
2. Because native backends report the edge after exclusive capture begins,
   lan-mouse immediately releases that first crossing. It sends no keyboard,
   pointer-motion, pointer-button, or scroll events to the target.
3. lan-mouse creates one `/enter/{target}` request and stores its request ID,
   epoch, and deadline. `409 busy` never means allow.
4. If the request is waking, lan-mouse keeps both input paths local and polls
   that request ID. The daemon cannot switch input later without client commit.
5. Before the TV command, the lan-mouse hub reserves keyboard and pointer
   capacity as one expiring bundle lease. Partial reservation is failure.
6. The daemon issues `set_input`, receives its correlated acknowledgement,
   then obtains a fresh active-input and signal observation tagged with the
   current switch epoch.
7. The daemon returns an expiring grant containing request epoch and lease ID.
   lan-mouse arms it without changing ownership. If the pointer is still
   focused on the same physical edge and the same edge-enter serial is current,
   the native capture backend resumes that crossing; otherwise the next
   matching crossing consumes the grant. The resumed or later crossing
   revalidates the peer session and both deadlines, atomically commits
   keyboard+pointer, and reports commit. A stale or late grant is rejected.
8. On the receiving host, `Enter` releases any outgoing capture and moves the
   native pointer to the center of the display that currently contains it
   before returning `Ack`. The transmitted edge remains only the return-edge
   barrier. Centering failure withholds `Ack`, so remote input forwarding
   cannot begin from an edge coordinate and the enter handshake fails closed.
9. Any other result (`waking`, `multiview`, `not_ready`, `busy`, 4xx/5xx,
   timeout, native controller task failure, lease loss) keeps keyboard and mouse
   on `SERVER_HOST`.

This is a hard contract. The removed asynchronous `enter_hook` path could start
only after capture had already begun and therefore could not satisfy C10. A
daemon-side auto-retry with no matching native client request is equally invalid.

Input ownership is atomic: keyboard, pointer motion, pointer buttons, and
scroll events are switched together. The design must never allow a state where
keyboard is sent to a remote host while the pointer remains on the server host,
or where the pointer is captured remotely while keyboard remains local.
The formal model therefore keeps `keyboard_owner` and `pointer_owner` separate
and checks equality as an invariant instead of assuming one owner variable.
Its abstract `cursor` value records the host that owns the pointer; receiver
pixel placement is the additional implementation invariant
`EnterAck => PointerPosition = Center(CurrentDisplay)`.

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
No separate heartbeat command is needed. The miss threshold and interval are
configuration derived from measured healthy latency and the required fallback
detection budget; three missed pongs is only an initial candidate.

**Reconnect:** Exponential backoff (1s → 2s → 4s → ... → 60s cap).
Transient connection failures retry indefinitely at the bounded backoff.
`RECONNECT_CAP` is an observability alert threshold, not a process-exit gate.
Only fatal local configuration, invalid credentials, or an unrecoverable
protocol incompatibility exits for systemd restart. The counter resets only
after registration, subscription, and initial state synchronization succeed.

#### 2. Native Enter Protocol Logic

```
POST /enter/{target}:                                 [both owners on SERVER_HOST]
  1. daemon_healthy=false or fallback unresolved -> 503 unavailable
  2. another request/command exists -> 409 busy + active request_id
  3. mode=multiview -> use the multiView bundle-commit path; no TV input change
  4. mode=fullscreen:
     a. target offline:
        -> allocate request_id/request_epoch, send WoL, state=waking
        -> return 202 pending; lan-mouse polls this same request_id
        -> timeout/cancel clears request; both owners remain SERVER_HOST
     b. target online but either path unavailable:
        -> 409 not_ready; no request, TV command, or ownership change
     c. both paths available:
        i.   reserve keyboard+pointer as one lease for request_epoch
        ii.  always issue set_input(target) with switch_epoch
        iii. await the correlated SSAP acknowledgement
        iv.  obtain a fresh active-input and signal observation for switch_epoch
        v.   revalidate the lease and issue an expiring grant
        vi.  lan-mouse validates request_epoch + lease, atomically commits both
             owners, and POSTs /enter/request/{id}/commit

  Any command error, negative/freshness-mismatched observation, lease loss,
  grant expiry, client timeout, or commit failure:
        -> release both owners to SERVER_HOST immediately
        -> enter the verified SERVER_HOST fallback transaction
        -> do not report recovery until active input + signal are freshly seen
```

The key change from the previous design (C6): step 4 no longer checks
`tv_input == target`. It always issues `set_input()`. `commanded_input` records
intent; `tv_input` and `input_signal` are observations. Only observations tagged
with the current `switch_epoch`, plus a valid input bundle lease, can authorize
a grant.

The second key change (C10): `SwitchComplete` issues a fenced grant but does not
change ownership. `LanMouseCommitGrant` is the only remote ownership transition,
and it changes `keyboard_owner` and `pointer_owner` together after validating
the request epoch, grant epoch, and held bundle reservation. This closes the
daemon-response delay race and makes split ownership representable in the
model.

The third key change (C11): if the target host is offline, the daemon sends
Wake-on-LAN and preserves the original request ID/epoch. lan-mouse polls that
request while keeping both owners local. Readiness advances the same request;
the daemon cannot perform an uncorrelated auto-switch after returning `202`.
Wake timeout or cancellation invalidates the request and every later response.

#### 2a. Clock Domains and Return-to-Server Release

Daemon deadlines are absolute only inside the daemon's monotonic clock domain;
lan-mouse never compares them directly with its own process-relative clock. Each
typed response carries `server_now_ms`. The client computes
`remaining = remote_deadline - server_now_ms` and anchors that duration at the
local request-start instant. Network and processing time therefore make the
local deadline conservatively earlier, never later. Zero, negative, overflowed,
or missing remaining time fails closed.

Returning from a spoke to `SERVER_HOST` does not create a competing
`/enter/SERVER_HOST` transaction. The spoke sends an epoch-tagged
`ReleaseRequest` to the hub and repeats the current epoch on heartbeat until the
hub returns the matching `ReleaseAck`. The hub releases capture first, then
cancels/cleans up the active daemon request; that cancellation drives the one
verified display fallback transaction. A stale acknowledgement cannot clear a
newer release request, and daemon unavailability cannot prevent local input
release.

#### 3. Failure Recovery Paths

All failures converge to the same recovery state:
freshly observed fullscreen `tv_input = SERVER_HOST`, server signal present,
`input_owner = keyboard_owner = pointer_owner = SERVER_HOST`,
`cursor = SERVER_HOST`, `capture = "idle"`, `pending_switch = none`, and
`fallback_required = false`. Command intent alone never satisfies recovery.

| Failure | Detection | Recovery Time | Transition |
|---|---|---|---|
| `set_input()` SSAP error | Correlated response | Input local immediately; display by fallback deadline | SwitchFailed → fallback command/verify |
| Target active-input mismatch or no HDMI signal | Fresh epoch-tagged query | Input remains local; display by fallback deadline | SwitchTimeout → fallback command/verify |
| Signal drops after stable connection | Single-flight periodic observation | Input local when detected; display by fallback deadline | SignalLossRevert → fallback command/verify |
| Remote host input readiness missing before enter | lan-mouse hub readiness report | immediate | RemoteInputNotReadyReject |
| Bundle reservation or readiness lost while pending/owned | Named-host hub update | input immediate | RemoteInputReadinessUpdate → verified fallback |
| Grant is stale, expires, or is not committed | Request/lease epoch and deadline | input remains local | GrantTimeout → verified fallback |
| Wake attempt never completes | Configured wake deadline | input remains local | WakeTimeout invalidates request epoch |
| Remote host spoke disconnects | lan-mouse hub event | input immediate | RemoteHostOffline → active/deferred fallback |
| Unexpected subscription callback | Target/phase mismatch | input immediate | SubscriptionFires → verified fallback |
| WebSocket disconnects | Configured keepalive budget | input immediate; display after reconnect | SSAPDisconnect → fallback_deferred → resume |
| TV reboot | disconnect/reconnect lifecycle | input immediate; display after verified resync | reconnect → subscribe → fallback command/verify |

If TV control or the physical `SERVER_HOST` HDMI signal remains unavailable,
the system stays degraded with input local and retries display recovery. It
must not fabricate a successful fallback state that the user cannot see.

#### 4. Signal Status Tracking

The daemon periodically queries the TV for per-input signal presence
(via SSAP `getExternalInputList` or equivalent endpoint). Each response is a
time-bounded observation, not permanent truth. `commanded_input`, observed
`tv_input`, observed `input_signal`, and `switch_epoch` are separate. A switch
can complete only from an observation produced for its current epoch.

Query immediately after every command acknowledgement, after reconnect, and
after every fallback command. While a remote host owns input, run one
single-flight poll on a monotonic schedule. The initial 10-second interval is
a candidate to validate against measured TV load and the required detection
budget. A bad poll arms recovery only when no deadline is active; repeated bad
polls cannot postpone fallback. A good poll cancels an armed loss deadline.
No steady poll is required on a freshly verified `SERVER_HOST` baseline.

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
reservation. Before switching the TV, the hub atomically converts readiness
into one expiring keyboard+pointer lease. Capacity cannot be consumed by a
different request while that lease is held. Permission loss, spoke loss, or
lease invalidation releases both owners and starts fallback.
After commit, the reservation becomes the active session lease and is renewed
by the same spoke-health channel; renewal expiry is handled as readiness loss.

Readiness updates are per-host `EXCEPT` updates. A Windows readiness event
cannot alter macOS readiness. `keyboard_owner` and `pointer_owner` remain
separate modeled fields and must be equal in every externally visible state.
An input-capture or emulation error such as `no capacity available` is a bundle
reservation failure: log both capability states, deny/revoke the grant, and
keep or return both owners to `SERVER_HOST`.

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
- Current Linux SERVER_HOST peer state: `lan-mouse list --json` exposes each
  configured target's online state, keyboard/pointer readiness, process session
  epoch, and peer build identity from the hub's local IPC snapshot.
- macOS lan-mouse: `~/Library/Logs/lan-mouse.log` and
  `~/Library/Logs/lan-mouse.err.log`, each with five 10 MiB backups managed by
  the launch wrapper.
- Windows lan-mouse: the scheduled task must redirect stdout and stderr to
  `%LOCALAPPDATA%\lan-mouse\logs\lan-mouse.log` and
  `%LOCALAPPDATA%\lan-mouse\logs\lan-mouse.err.log`, each with five 10 MiB
  backups. The deploy must not rely on transient console output or an
  unverified Event Log source.

**Structured JSON logging (stdout, one object per line):**
```json
{"ts":"...","event":"ssap_connecting","tv_ip":"192.0.2.20"}
{"ts":"...","event":"ssap_registered","client_key_present":true}
{"ts":"...","event":"subscribed","topic":"multiViewStatus"}
{"ts":"...","event":"enter","request_id":"...","request_epoch":41,"target":"mac","phase":"command_pending","commanded_input":"mac"}
{"ts":"...","event":"switch_observed","request_epoch":41,"switch_epoch":87,"observed_input":"mac","signal":true,"observation_age_ms":0}
{"ts":"...","event":"grant_issued","request_epoch":41,"grant_epoch":41,"lease_id":"...","target":"mac"}
{"ts":"...","event":"input_commit","request_epoch":41,"keyboard_owner":"mac","pointer_owner":"mac"}
{"ts":"...","event":"switch_timeout","request_epoch":41,"switch_epoch":87,"target":"mac","phase":"fallback_command_pending","action":"revert_to_server_host","server_host":"linux"}
{"ts":"...","event":"switch_failed","target":"windows","error":"timeout","action":"revert_to_server_host","server_host":"linux"}
{"ts":"...","event":"signal_loss","input":"mac","action":"revert_to_server_host","server_host":"linux"}
{"ts":"...","event":"remote_offline","host":"mac","action":"revert_to_server_host","server_host":"linux"}
{"ts":"...","event":"ssap_disconnect","reason":"ping_timeout"}
```

Every transition log includes request/switch epoch where applicable, previous
and next phase, commanded and observed input, keyboard and pointer owners,
reservation/grant identity, observation age, command latency, and fallback
reason. The logging sink is bounded and non-blocking; rotation or a stalled
file sink must not block the SSAP actor or input-release path.

The production implementation makes that bound concrete in both Rust
processes: at most 1,024 records of at most 16 KiB each may wait for the log
worker (a maximum queued payload of 16 MiB). Producers use non-blocking sends.
An oversized record, a full queue, or a failed sink write increments the
drop count instead of delaying protocol or input service work. tv-multiview
exposes its queue depth, capacity, record bound, and drop count in `/status`.
lan-mouse reports the cumulative dropped-record count to its persistent sink
after that sink accepts writes again. The macOS and Windows wrappers keep one
writer open per stream and rotate only at the configured 10 MiB boundary, so
the wrapper does not reopen and restat each file for every log line.

**`/status` `data` payload (the API envelope also carries `server_now_ms`):**
```json
{
  "mode": "fullscreen",
  "observed_input": "linux",
  "commanded_input": "linux",
  "healthy": true,
  "ready": true,
  "ws_state": "connected",
  "subscribe_active": true,
  "protocol_phase": "idle",
  "request_id": null,
  "request_epoch": 41,
  "switch_epoch": 88,
  "verified_epoch": 88,
  "pending_switch": null,
  "switch_timer": 0,
  "fallback_required": false,
  "keyboard_owner": "linux",
  "pointer_owner": "linux",
  "reservation_target": null,
  "grant_epoch": null,
  "input_signal": {"linux": true, "mac": false, "windows": true},
  "signal_observations": {
    "linux": {"present": true, "switch_epoch": 88, "observed_at_ms": 3599000}
  },
  "remote_online": {"mac": false, "windows": true},
  "peer_readiness": {
    "windows": {
      "online": true,
      "keyboard_ready": true,
      "pointer_ready": true,
      "session_epoch": 17,
      "observed_at_ms": 3599500
    }
  },
  "uptime_seconds": 3600,
  "reconnect_total": 0,
  "switch_count": {"linux": 12, "mac": 8, "windows": 5},
  "last_error": null,
  "dropped_logs": 0,
  "request_history_len": 4,
  "retained_request_limit": 32,
  "queues": {
    "ordinary_commands": {"depth": 0, "capacity": 64},
    "safety_commands": {"depth": 0, "capacity": 16},
    "ordinary_effects": {"depth": 0, "capacity": 64},
    "safety_effects": {"depth": 0, "capacity": 16}
  },
  "runtime": {
    "log_queue_depth": 0,
    "log_queue_capacity": 1024,
    "log_record_max_bytes": 16384,
    "dropped_logs": 0,
    "reconnect_consecutive": 0,
    "reconnect_backoff_ms": 0,
    "retry_alert": false
  },
  "observation_age_ms": 1000,
  "deadline_remaining_ms": null,
  "in_flight_operation": null
}
```

The retry alert becomes true after the configured consecutive reconnect
threshold (initially ten) and resets only after registration, subscription,
and initial synchronization all succeed. It is diagnostic state, never a
process-exit or false-readiness condition. Request history is retained in a
fixed oldest-evicted ring (initially 32 entries) so duplicate create, commit,
and cancellation identities remain deterministic without unbounded memory.

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

Transient TV/network failures are handled inside the daemon and do not cause
process exit. `Restart=on-failure` is reserved for fatal local errors or an
unexpected crash. Persistent logs require rotation on every platform.

Linux, macOS, and Windows build the same immutable lan-mouse git revision from
one git bundle and resolve dependencies from a Cargo vendor archive keyed by the
verified `Cargo.lock` SHA-256. Linux preserves all configured non-GTK input
backends; macOS and Windows use their target-native backends with
`--no-default-features`. A host records the revision, lock hash, toolchain
selector, selected compiler's exact `rustc --version` output, and feature set
only after native tests and installation succeed. Every checkout is also
required to match the bundle's exact pinned head before a build can start.

#### 8. Performance and Deadlock Constraints

- One SSAP actor owns the socket and serializes commands. The queue is bounded;
  duplicate signal polls are coalesced and a fallback command has priority over
  ordinary mode requests.
- HTTP handlers and subscription processing never hold a state mutex while
  awaiting SSAP, lan-mouse, file I/O, or timers. State changes use short critical
  sections or actor messages, excluding the lock/response circular wait.
- There is at most one signal query in flight. Poll scheduling and transaction
  deadlines use monotonic time; a callback cannot extend a deadline unless it
  starts a new epoch.
- A pending enter request owns one bundle lease and one TV command slot. New
  requests receive `409 busy`; they do not allocate unbounded tasks or queue
  duplicate TV commands.
- Wake/request polling is implemented by one bounded native lan-mouse task. It
  never spawns `curl` or another process per poll; cancellation is tied to the
  capture gate and request epoch.
- The 5-second switch, 60-second wake, 10-second poll, ping-miss, and reconnect
  values are initial candidates, not correctness constants. Production values
  require recorded p50/p95/p99 command, observation, wake, and disconnect data
  plus a documented safety margin.
- Persistent WebSocket latency numbers describe transport/request overhead only;
  end-to-end switch latency also includes TV settle, fresh observation, lease
  validation, and lan-mouse commit. No `<5ms` end-to-end claim is permitted
  without measurement.

## Architecture Decision Records

### ADR-001: Separate daemon

**Decision:** Run tv-multiview as a standalone HTTP daemon alongside lan-mouse.

**Rationale:**
- lan-mouse starts the request synchronously, then polls the same request ID
  while capture remains paused; a daemon-side delayed switch is never implicit
- TV subscription needs persistent WebSocket — lan-mouse is event-driven
- The authenticated typed HTTP API is usable by the native client on every OS
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
not encoded in cursor-enter request parameters.

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
- Removes repeated process/TLS/register overhead. Transport/request latency and
  end-to-end switch latency are measured separately; no fixed `<5ms` claim is
  accepted without a recorded workload and percentile data.
- Connection health is immediate (socket error on next write) vs.
  discovered via 5s heartbeat
- Subscriptions (push updates from TV) require a persistent connection
  anyway — can't subscribe over a one-shot subprocess

### ADR-005: Approach 1 — always switch, never skip on stale state

**Decision:** `/enter/{target}` always issues `set_input(target)`.
There is no "already on target" no-op guard. `commanded_input` records intent;
`tv_input` is the latest TV observation. Neither cached field can complete a
request without a current `switch_epoch` observation.

**Rationale:**
- observed `tv_input` can become stale (user pressed remote, TV rebooted,
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
- All failure paths first release keyboard and pointer to the server host, then
  converge through one fallback command/ack/observation transaction
- Recovery is complete only after fullscreen server input and server HDMI signal
  are freshly observed; unavailable TV control remains `fallback_deferred`

### ADR-007: SSAP + daemon unified in one TLA+ spec

**Decision:** The TLA+ spec models both the SSAP client lifecycle
(`ws_state`, `subscribe_active`) and the daemon state machine
(`tv_mode`, observed `tv_input`, `protocol`, owners, reservation, grant,
`cursor`, `capture`) in one module.

**Rationale:**
- The daemon's state machine depends on SSAP events (disconnect → deferred
  fallback, registration/subscription → healthy, callback → expected event or
  manual override)
- The `HealthDefinition` invariant ties them together: daemon_healthy
  iff connected AND subscribed
- Modeling them separately would miss race conditions between SSAP
  events and daemon state transitions
- In code, they are separated by a module boundary (`src/ssap/`) within
  a single Rust crate, not a crate boundary — so the unified spec
  matches the implementation structure

### ADR-008: Fenced request, bundle reservation, and client commit

**Decision:** Every enter attempt has a request epoch. The lan-mouse hub reserves
keyboard and pointer capacity as one lease before any TV command. After a fresh
switch-epoch observation, the daemon issues an expiring grant; lan-mouse validates
the request, grant, and lease epochs and commits both owners atomically.

**Rationale:**
- A readiness snapshot does not reserve capacity and is vulnerable to TOCTOU
- An HTTP response can be delayed past timeout, cancellation, or remote failure
- Separate keyboard/pointer owners let the model detect the observed split-input
  failure instead of assuming it away
- A stale grant is harmless because its epoch or lease cannot commit

### ADR-009: One SSAP actor with bounded work

**Decision:** One actor exclusively owns the WebSocket and a bounded command
queue. It correlates IDs, coalesces duplicate polls, and prioritizes fallback.
No state lock is held across asynchronous I/O.

**Rationale:**
- Multiple HTTP handlers must not interleave writes or consume each other's replies
- Holding shared state while waiting for the reader task creates a lock/response
  circular wait
- Bounded single-flight polling prevents task growth and timer starvation

### ADR-010: Retry transient failures; never fabricate availability

**Decision:** Transient SSAP failures retry indefinitely with bounded backoff.
The reconnect threshold raises an alert but does not exit. Fatal local
configuration/authentication errors may exit. Server fallback retries until a
fresh server-input and HDMI-signal observation succeeds.

**Rationale:**
- Restarting after a transient retry count resets backoff without improving reachability
- A command acknowledgement proves acceptance, not visible display state
- Keeping input local with `fallback_required=true` is honest degraded service;
  declaring an unverified server display would violate the availability contract
