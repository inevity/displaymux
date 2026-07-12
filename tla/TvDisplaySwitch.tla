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
