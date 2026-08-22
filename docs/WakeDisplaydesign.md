# Generic WakeDisplay Design for lan-mouse and a Shared HDMI Display

## 1. Purpose

This document defines a general display-wake protocol for any lan-mouse client
connected to a shared OLED, television, monitor, KVM, or HDMI switch.

It refines the fenced two-phase switch and host-wake behavior in
[fullscreenmultiviewswitchdesign.md](fullscreenmultiviewswitchdesign.md). It
does not replace the existing input-ownership protocol.

The motivating failure is not specific to Windows:

1. A remote lan-mouse client remains online and reports keyboard and pointer
   emulation ready.
2. Its display pipeline has entered display sleep and no longer produces an
   HDMI signal.
3. The shared display selects that client's HDMI input.
4. The controller cannot verify a signal, so it returns the display to the
   lan-mouse server host.
5. Physical activity on the remote host wakes its display and makes a later
   switch succeed.

The protocol must wake the selected client's display pipeline without moving
the user's keyboard or pointer away from the lan-mouse server host. It must be
host-neutral and must not encode Windows, macOS, Linux, or a fixed host name in
the coordinator.

## 2. Terminology and Ownership Boundaries

### 2.1 Hosts

- `SERVER_HOST`: the host running the lan-mouse server/hub. This is the normal
  and always-available input fallback. It is Linux only in the current
  deployment.
- `RemoteHosts`: all configured lan-mouse clients other than `SERVER_HOST`.
- `target`: the remote host selected by the current fenced enter request.

### 2.2 Independent Availability Dimensions

Every remote host has three independent runtime states:

```text
PeerOnline[host]
InputBundleReady[host]
DisplaySignalPresent[host]
```

Their meanings are deliberately separate:

- `PeerOnline` means the lan-mouse process is reachable through the authenticated
  peer transport.
- `InputBundleReady` means the current peer process can emulate both keyboard
  and pointer for the matching process/session epoch.
- `DisplaySignalPresent` means the shared display controller has freshly
  observed a physical signal on the host's configured display input.

An online and input-ready process does not prove that its GPU or display output
is awake.

### 2.3 Two Different Wake Operations

`HostWake` and `DisplayWake` solve different states and must remain separate:

```text
HostWake:
    PeerOnline[target] = FALSE
    Uses an out-of-band host-power mechanism such as Wake-on-LAN.

DisplayWake:
    PeerOnline[target] = TRUE
    InputBundleReady[target] = TRUE
    DisplaySignalPresent[target] = FALSE
    Uses the already-running lan-mouse client.
```

`HostWake` preserves the existing C11 behavior. `DisplayWake` is the new
refinement specified here.

### 2.4 Shared Display Selection Versus Input Ownership

The shared display input and the user's input ownership are separate state:

```text
display_input     = which HDMI/source the shared display currently shows
keyboard_owner    = which host receives user keyboard events
pointer_owner     = which host receives user pointer events
```

For a shared OLED/HDMI sink, the controller first selects the target display
input. The target may require being the selected sink before its operating
system and GPU activate the display pipeline. During this phase, keyboard and
pointer ownership remain on `SERVER_HOST`.

## 3. Goals and Non-Goals

### 3.1 Goals

1. Support any lan-mouse client that advertises a display-wake adapter.
2. Select the target on the shared display before requesting display wake.
3. Keep keyboard and pointer ownership together on `SERVER_HOST` until fresh
   target signal verification and the existing fenced grant/commit complete.
4. Treat the physical display signal as authoritative. A client acknowledgement
   alone never proves display availability.
5. Make retries idempotent and reject stale acknowledgements by epoch.
6. Converge every failure to verified `SERVER_HOST` fallback.
7. Preserve bounded queues, one active request, one wake operation, and one
   signal observation in flight.
8. Produce concrete failure reasons and correlated persistent logs.

### 3.2 Non-Goals

1. Do not keep a remote display awake indefinitely.
2. Do not bypass an OS lock screen or authentication policy.
3. Do not send arbitrary commands to remote clients.
4. Do not transfer only keyboard or only pointer.
5. Do not treat a synthetic user key as the generic wake protocol.
6. Do not infer signal availability from process health, acknowledgement, or
   configured intent.
7. Do not hardcode host names or operating-system branches in tv-multiview.

## 4. Architecture

### 4.1 Actors

#### tv-multiview coordinator

The coordinator remains the single owner of the display transaction state. It:

- serializes display commands and signal observations;
- owns request, switch, grant, and wake epochs;
- selects the target display input;
- decides whether display wake is required;
- waits for fresh target signal evidence;
- issues the existing fenced grant only after verification;
- initiates verified fallback on failure.

#### Display adapter

The existing SSAP/display adapter:

- selects the configured shared-display input;
- observes the active input;
- observes physical signal presence for each configured input;
- reports observations tagged with the current switch epoch.

#### lan-mouse server

The lan-mouse server:

- starts and polls the fenced enter request;
- publishes peer readiness and display-wake capability to tv-multiview;
- receives a typed `display_wake_required` request status;
- sends one epoch-tagged `WakeDisplay` control message to the target peer;
- reports the peer acknowledgement or rejection to tv-multiview;
- keeps the capture gate closed until the ordinary grant/commit phase.

#### lan-mouse client

Every client exposes the same adapter contract. The adapter implementation is
platform-specific, but the wire protocol and controller state machine are not.

```rust
trait DisplayWakeAdapter {
    fn supported(&self) -> bool;
    fn wake_display(&mut self) -> Result<(), DisplayWakeError>;
}
```

Examples of adapter internals may include a native Windows display-power API,
a macOS display-wake API, a Linux compositor/logind API, or a future platform
backend. Unsupported clients advertise `supported = false`.

## 5. Generic Protocol

### 5.1 Capability Advertisement

Peer readiness gains a display-wake capability bit:

```text
Readiness {
    keyboard_ready: bool,
    pointer_ready: bool,
    display_wake_ready: bool,
    session_epoch: u64,
}
```

The capability is scoped to the same lan-mouse process/session epoch as input
readiness. A process restart invalidates all cached readiness, wake requests,
and grants from the previous session.

tv-multiview receives the capability through the existing authenticated
internal peer-readiness update. The coordinator evaluates capability state, not
the target operating system.

### 5.2 Peer Wire Messages

The lan-mouse peer protocol gains two authenticated control messages:

```text
WakeDisplay {
    wake_epoch: u64,
}

WakeDisplayAck {
    wake_epoch: u64,
    result: accepted | unsupported | failed,
}
```

Only `wake_epoch` must travel on the peer wire. The lan-mouse server associates
it with the active request epoch, switch epoch, target handle, and peer session
epoch. The authenticated peer connection and current session provide the rest
of the identity boundary.

The target client deduplicates repeated `WakeDisplay` messages with the same
wake epoch. It returns the same result without invoking the native adapter a
second time.

`accepted` means only that the native adapter call completed successfully. It
does not authorize an input grant.

### 5.3 Coordinator API Extension

The existing fenced enter request remains the transaction root. Its poll result
adds a typed status:

```json
{
  "status": "display_wake_required",
  "request_id": "request-...",
  "request_epoch": 42,
  "switch_epoch": 87,
  "wake_epoch": 6,
  "target": "host-id",
  "deadline_ms": 123456
}
```

The lan-mouse server sends the peer message once for this identity and reports
the result through an authenticated internal endpoint:

```text
POST /internal/enter/request/{request_id}/display-wake-result
```

The request body contains `request_epoch`, `switch_epoch`, `wake_epoch`, peer
session epoch, and the typed result. tv-multiview rejects any stale or
conflicting identity.

The coordinator begins or continues signal observation independently of the
acknowledgement. A fresh target signal may arrive because of physical keyboard
activity, another system service, or the wake adapter. All are valid because
the signal, not the cause, is authoritative.

## 6. State Machine

### 6.1 Successful Display-Wake Path

```text
Idle on SERVER_HOST
  -> CaptureCandidate(target)
  -> verify PeerOnline(target)
  -> verify InputBundleReady(target)
  -> reserve keyboard+pointer bundle
  -> command shared display input = target
     keyboard_owner = SERVER_HOST
     pointer_owner = SERVER_HOST
  -> observe active display input = target
  -> observe DisplaySignalPresent(target) = FALSE
  -> verify DisplayWakeSupported(target)
  -> DisplayWaking(target, wake_epoch)
  -> lan-mouse server sends WakeDisplay(wake_epoch)
  -> client executes its native display-wake adapter
  -> client sends WakeDisplayAck(accepted)
  -> tv-multiview observes fresh target HDMI signal = TRUE
  -> issue fenced grant
  -> lan-mouse commits capture
  -> keyboard_owner = target and pointer_owner = target atomically
  -> RemoteOwned(target)
```

### 6.2 Signal Already Present

If a fresh target signal is already present after display selection, the
protocol skips `DisplayWaking` and proceeds directly to the existing grant and
commit path. A screen saver that continues producing HDMI follows this path.

### 6.3 Unsupported Display Wake

If signal is absent and `DisplayWakeSupported(target) = FALSE`, the coordinator
does not fabricate availability and does not send a user input event. It waits
only within the bounded verification policy, then restores `SERVER_HOST` and
reports `target_signal_absent_display_wake_unsupported`.

### 6.4 Host Offline

If `PeerOnline(target) = FALSE`, the protocol uses the existing `HostWake`
state before selecting the target display. A later process/session readiness
update may restart the same fenced request only if every request and session
identity still matches.

### 6.5 Failure and Fallback

Any wake rejection, peer loss, display-control loss, stale identity, wake
deadline, signal deadline, cancellation, or shutdown performs these steps:

1. Release keyboard and pointer ownership locally if necessary.
2. Invalidate the active grant and wake epoch.
3. Command the shared display to `SERVER_HOST`.
4. Obtain a fresh active-input and server-signal observation.
5. Declare recovery only after the normal server fallback state is verified.

During `DisplayWaking`, step 1 is already satisfied because ownership never left
`SERVER_HOST`.

## 7. TLA+ Refinement Contract

The following is a refinement contract for integration into the existing
unified model. It is not a second independent source of truth.

```tla
RemoteHosts == Host \ {SERVER_HOST}
ActiveInput == Host \cup {"unknown"}

DisplayWakePhase == {
    "none",
    "selecting_display",
    "verifying_display",
    "display_waking",
    "grant_pending",
    "remote_owned",
    "fallback_command_pending",
    "fallback_verifying"
}

DisplayWakeResult == {"none", "accepted", "unsupported", "failed"}

VARIABLES
    phase,
    pending_host,
    commanded_display_input,
    observed_display_input,
    display_signal,
    display_signal_epoch,
    peer_online,
    input_bundle_ready,
    display_wake_supported,
    display_wake_pending,
    display_wake_result,
    wake_epoch,
    wake_timer,
    switch_epoch,
    keyboard_owner,
    pointer_owner,
    input_owner,
    fallback_required

FreshSignal(host) ==
    /\ display_signal[host]
    /\ display_signal_epoch[host] = switch_epoch

RemoteReady(host) ==
    /\ peer_online[host]
    /\ input_bundle_ready[host]

DisplayWakeTypeOK ==
    /\ phase \in DisplayWakePhase
    /\ pending_host \in ({"none"} \cup RemoteHosts)
    /\ commanded_display_input \in Host
    /\ observed_display_input \in ActiveInput
    /\ display_signal \in [Host -> BOOLEAN]
    /\ display_signal_epoch \in [Host -> Nat]
    /\ peer_online \in [RemoteHosts -> BOOLEAN]
    /\ input_bundle_ready \in [RemoteHosts -> BOOLEAN]
    /\ display_wake_supported \in [RemoteHosts -> BOOLEAN]
    /\ display_wake_pending \in ({"none"} \cup RemoteHosts)
    /\ display_wake_result \in DisplayWakeResult
    /\ wake_epoch \in Nat
    /\ wake_timer \in Nat
    /\ switch_epoch \in Nat
    /\ keyboard_owner \in Host
    /\ pointer_owner \in Host
    /\ input_owner \in Host
    /\ fallback_required \in BOOLEAN

InputOwnershipAtomic ==
    /\ keyboard_owner = pointer_owner
    /\ input_owner = keyboard_owner

DisplayWakeKeepsInputLocal ==
    (phase = "display_waking") =>
        /\ input_owner = SERVER_HOST
        /\ keyboard_owner = SERVER_HOST
        /\ pointer_owner = SERVER_HOST

DisplayWakeSelectsTarget ==
    (phase = "display_waking") =>
        /\ pending_host \in RemoteHosts
        /\ commanded_display_input = pending_host
        /\ observed_display_input = pending_host

DisplayWakeRequiresCapability ==
    \A host \in RemoteHosts :
        (phase = "display_waking" /\ pending_host = host) =>
            /\ display_wake_supported[host]
            /\ RemoteReady(host)

WakeAckCannotGrant ==
    (phase = "display_waking"
      /\ display_wake_result = "accepted") =>
        input_owner = SERVER_HOST

RemoteOwnershipRequiresDisplay ==
    (input_owner \in RemoteHosts) =>
        /\ phase = "remote_owned"
        /\ observed_display_input = input_owner
        /\ FreshSignal(input_owner)
        /\ RemoteReady(input_owner)

FallbackOwnsInputLocally ==
    fallback_required =>
        /\ input_owner = SERVER_HOST
        /\ keyboard_owner = SERVER_HOST
        /\ pointer_owner = SERVER_HOST

WakeIdentityBound ==
    (display_wake_pending \in RemoteHosts) =>
        /\ display_wake_pending = pending_host
        /\ phase = "display_waking"
        /\ wake_epoch > 0
```

The action guards refine the prose protocol:

```text
RequestDisplayWake(target)
  requires:
    phase = verifying_display
    pending_host = target
    observed_display_input = target
    fresh target signal observation is false
    RemoteReady(target)
    DisplayWakeSupported(target)
    no wake request is already pending

DisplayWakeAccepted(epoch)
  requires:
    phase = display_waking
    epoch = wake_epoch
  effect:
    records accepted only; ownership is unchanged

DisplaySignalReady(target)
  requires:
    phase in {verifying_display, display_waking}
    observed_display_input = target
    FreshSignal(target)
    RemoteReady(target)
  effect:
    phase = grant_pending

CommitInput(target)
  requires:
    phase = grant_pending
    FreshSignal(target)
    RemoteReady(target)
    valid request, grant, lease, and peer session epochs
  effect:
    keyboard_owner = target
    pointer_owner = target
    input_owner = target
    phase = remote_owned

DisplayWakeTimeout
  requires:
    phase = display_waking
    wake_timer expired
  effect:
    invalidate wake and grant identities
    keep all input ownership on SERVER_HOST
    begin verified display fallback to SERVER_HOST
```

Required fairness for liveness includes weak fairness for wake timer progress,
wake timeout, signal observation completion, fallback command, and fallback
verification. A frozen timer or permanently unconsumed signal observation must
not make eventual fallback vacuous.

## 8. Safety Properties

### S1. Keyboard and pointer never split

Every transition moves keyboard and pointer together. Display-wake control
messages do not modify either owner.

### S2. No user input is routed to an unverified display

Until the shared display has freshly observed the target input and physical
signal, `input_owner = SERVER_HOST`.

### S3. Acknowledgement is not signal evidence

`WakeDisplayAck(accepted)` cannot issue a grant. Only fresh display-controller
observation may advance to `grant_pending`.

### S4. Stale wake events cannot affect a newer request

The lan-mouse server and tv-multiview both validate request, switch, wake, lease,
and peer session identities. A late acknowledgement is ignored.

### S5. Unsupported clients fail closed

No fallback synthetic key or pointer event is sent when the adapter is absent.
The shared display returns to `SERVER_HOST`.

### S6. The server host is the universal fallback

The design is not Linux-specific. Whatever host runs the lan-mouse server is
the input and display fallback for every failed transition.

## 9. Failure Matrix

| Failure | Detection | Input ownership | Display recovery | Reason |
|---|---|---|---|---|
| Target peer offline | Peer heartbeat/session | `SERVER_HOST` | Keep server input while HostWake runs | `peer_offline` |
| Target input bundle unavailable | Readiness tuple | `SERVER_HOST` | No target display command | `peer_bundle_not_ready` |
| Display command rejected | Typed display command result | `SERVER_HOST` | Verify or restore server input | `display_select_failed` |
| Target selected, signal absent, wake unsupported | Capability + fresh signal | `SERVER_HOST` | Restore server input | `display_wake_unsupported` |
| Wake control transport failed | lan-mouse peer send result | `SERVER_HOST` | Restore server input | `display_wake_transport_failed` |
| Client rejected wake | Typed wake acknowledgement | `SERVER_HOST` | Restore server input | `display_wake_rejected` |
| Client accepted wake but signal never appeared | Wake/signal deadline | `SERVER_HOST` | Restore server input | `target_signal_absent_after_wake` |
| Peer restarted during wake | Session epoch change | `SERVER_HOST` | Restore server input | `peer_session_changed_during_wake` |
| Signal appeared before wake acknowledgement | Fresh signal observation | `SERVER_HOST` until grant | Continue grant; ignore late ack | none |
| User physically wakes target | Fresh signal observation | `SERVER_HOST` until grant | Continue grant | none |
| Signal disappears before commit | Fresh verification | `SERVER_HOST` | Restore server input | `target_signal_lost_before_commit` |
| TV transport disconnects | SSAP lifecycle | immediately `SERVER_HOST` | Deferred verified fallback after reconnect | `display_transport_disconnected` |
| Server-host HDMI signal unavailable during fallback | Fresh server observation | `SERVER_HOST` | Remain honestly degraded and retry | `server_signal_unavailable` |

## 10. Native Adapter Contract

### 10.1 Required Semantics

A native adapter must:

1. Request display activation once.
2. Return promptly with accepted, unsupported, or failed.
3. Avoid changing persistent power policy.
4. Avoid transferring lan-mouse input ownership.
5. Avoid arbitrary shell execution or arbitrary remote commands.
6. Be safe to invoke repeatedly for the same deduplicated wake epoch.
7. Log the adapter, wake epoch, result, and native error category.

### 10.2 Platform Isolation

Platform modules implement the adapter. The coordinator, HTTP API, peer
protocol state machine, and formal model use only capabilities and typed
results. They must not contain logic such as:

```text
if host == "windows" ...
if target_os == "macos" ...
```

### 10.3 No Synthetic Enter as the Protocol

A physical Enter key proves that local user activity can wake a particular
machine, but the generic protocol must not encode Enter as its wake contract.
An injected Enter may dismiss a dialog, submit text, or act on the foreground
application. Platform adapters should use a native display-power facility when
one exists.

If a future platform has no safer facility and requires synthetic activity,
that behavior belongs inside its adapter and must be explicitly documented and
capability-gated. The coordinator still sees only `WakeDisplay`.

## 11. Timing, Performance, and Deadlock Constraints

1. There is at most one active fenced enter request.
2. There is at most one display-wake request for that enter request.
3. There is at most one display signal observation in flight.
4. Poll deadlines are consumed or advanced when work starts; an expired poll
   deadline must not remain the minimum deadline while an observation is in
   flight.
5. Wake and signal retries use bounded timers and bounded queues.
6. Native adapter execution must not block the tv-multiview coordinator.
7. Wake polling must not run as an unbounded task per cursor event.
8. Repeated edge motion must not create repeated wake operations. The request
   and wake epochs deduplicate the whole transaction.
9. Safety/fallback commands retain priority over ordinary observation and wake
   work.
10. Timeout values are configuration, not hardcoded host-specific constants.
    Their values require measured hardware behavior and must preserve the
    request/lease deadline ordering.

Required deadline ordering:

```text
display command deadline
    < display wake deadline
    < request/lease expiry
```

The grant deadline begins only after fresh target signal verification. Time
spent waking the display must not consume an already-issued grant.

## 12. Observability

Every log record must include the identities available at that layer:

```text
request_id
request_epoch
switch_epoch
wake_epoch
lease_epoch
peer_session_epoch
target
phase
```

Required events:

```json
{"event":"display_wake_required","target":"host-id","wake_epoch":6,"signal":false}
{"event":"display_wake_sent","target":"host-id","wake_epoch":6}
{"event":"display_wake_ack","target":"host-id","wake_epoch":6,"result":"accepted"}
{"event":"display_signal_observed","target":"host-id","switch_epoch":87,"signal":true}
{"event":"display_wake_completed","target":"host-id","wake_epoch":6,"latency_ms":842}
{"event":"display_wake_failed","target":"host-id","wake_epoch":6,"reason":"target_signal_absent_after_wake"}
```

Notifications are emitted only by `SERVER_HOST`. They distinguish:

- peer offline;
- input bundle unavailable;
- display wake unsupported;
- display wake rejected;
- wake accepted but no HDMI signal appeared;
- shared-display command failure;
- verified fallback failure.

A notification must not claim that an operating system is asleep solely
because HDMI signal is absent.

## 13. Security

1. `WakeDisplay` is accepted only from an authenticated, configured lan-mouse
   peer over the existing protected transport.
2. Exact protocol/build compatibility is required for the new message and
   readiness field. No compatibility branch is added.
3. The command carries no executable string, key code, path, or arbitrary
   payload.
4. Wake epochs and peer session epochs prevent replay across requests or
   process restarts.
5. The client reports only a typed result and does not expose a general remote
   execution API.
6. tv-multiview internal wake-result endpoints retain bearer authentication and
   full identity validation.

## 14. Implementation Boundaries

### lan-mouse-proto

- Add `display_wake_ready` to readiness.
- Add `WakeDisplay` and `WakeDisplayAck` event types.
- Append event identifiers without renumbering existing events.
- Add round-trip, stale identity, and deduplication tests.

### input-emulation

- Add the synchronous display-wake adapter method with unsupported as the
  default.
- Implement platform modules independently.
- Add the platform API feature declarations required by each native backend.

### lan-mouse client receive path

- Authenticate the requesting peer before dispatch.
- Deduplicate wake epoch.
- Execute the native adapter without changing the normal input handle or input
  ownership.
- Return the typed acknowledgement.

### lan-mouse server request path

- Publish peer display-wake capability with current readiness/session epoch.
- Interpret `display_wake_required` from the fenced request poll.
- Send one wake request for each wake identity.
- Report typed result to tv-multiview.
- Keep the capture gate closed until the ordinary grant is valid.

### tv-multiview

- Add `DisplayWaking` protocol phase and wake identity state.
- Select the target shared-display input before entering `DisplayWaking`.
- Keep all input ownership local throughout display wake.
- Poll fresh signal state while wake is pending.
- Allow fresh signal to advance independently of wake acknowledgement.
- Add typed wake failure and timeout reasons.
- Preserve verified `SERVER_HOST` fallback.

### Deployment

- Pin one exact lan-mouse revision.
- Build and deploy all native clients because the peer protocol changes.
- Deploy tv-multiview after its model and tests pass.
- Preserve persistent logs and native permission requirements on every host.

## 15. Verification Requirements

### 15.1 Formal Safety Scenarios

The model must cover at least:

1. Signal already present, no wake required.
2. Target selected, signal absent, wake accepted, signal appears.
3. Wake accepted, signal never appears.
4. Wake rejected.
5. Wake unsupported.
6. Duplicate wake request and duplicate acknowledgement.
7. Stale acknowledgement after a newer request begins.
8. Peer restart during display wake.
9. TV disconnect during display wake.
10. Physical user activity causes signal before acknowledgement.
11. Signal appears and disappears before commit.
12. Fallback server signal is temporarily absent.

Every trace must preserve `InputOwnershipAtomic`,
`DisplayWakeKeepsInputLocal`, `WakeAckCannotGrant`, and
`FallbackOwnsInputLocally`.

### 15.2 Rust Tests

- Protocol serialization round trips.
- Capability/session invalidation.
- One wake send per wake epoch.
- Duplicate acknowledgement idempotence.
- Accepted acknowledgement does not grant ownership.
- Fresh signal can grant after wake.
- Signal timeout falls back with both owners local.
- Coordinator deadline remains in the future while observation is in flight.
- Native unsupported and failure results remain typed.

### 15.3 Native Runtime Check

For each adapter implementation:

1. Let the client remain online while its display output sleeps.
2. Cross the server edge.
3. Confirm the shared display selects the target input.
4. Confirm keyboard and pointer remain local while the target has no signal.
5. Confirm one display-wake request is received.
6. Confirm the shared display observes target signal.
7. Confirm the fenced commit moves keyboard and pointer together.

This is a focused validation of the implemented adapter, not a requirement to
pre-test every theoretical failure before normal use.

## 16. Architecture Decisions

### ADR-WD-001: Model host wake and display wake separately

Decision: `PeerOnline = FALSE` uses HostWake; online peer with absent display
signal uses DisplayWake.

Rationale: process reachability, input readiness, and physical HDMI signal are
independent facts.

### ADR-WD-002: Select the shared display target before display wake

Decision: command the shared OLED/HDMI display to the target while keyboard and
pointer remain on `SERVER_HOST`, then request display wake if signal is absent.

Rationale: a target GPU may require the shared sink to be selected before its
display pipeline activates. The current two-phase protocol already permits
display selection without input transfer.

### ADR-WD-003: Keep display wake separate from user input

Decision: use a typed control message and native adapter, not an ordinary Enter
or pointer event.

Rationale: user input must follow ownership. A control-plane wake operation can
run while user input remains safely local.

### ADR-WD-004: Physical signal is authoritative

Decision: a wake acknowledgement never grants input. Fresh target signal and
the existing readiness/lease checks are required.

Rationale: successful API execution does not prove that the cable, GPU, HDMI
port, or shared display is producing and receiving a usable signal.

### ADR-WD-005: Capability, not operating-system identity

Decision: clients advertise display-wake support in readiness. The coordinator
contains no OS-specific branch.

Rationale: the same protocol must support every current and future lan-mouse
client and any configured shared-display input.

### ADR-WD-006: SERVER_HOST remains the universal fallback

Decision: every display-wake failure keeps or restores keyboard, pointer, and
the shared display to the host running the lan-mouse server.

Rationale: fallback ownership is a deployment role, not an operating-system
name.

## 17. Final Protocol Summary

```text
Remote peer offline
  -> HostWake
  -> wait for current process/session readiness

Remote peer online and input-ready
  -> reserve keyboard+pointer together
  -> select target on shared display
  -> keep keyboard+pointer on SERVER_HOST

Fresh target signal present
  -> fenced grant
  -> atomic input commit

Fresh target signal absent and display wake supported
  -> WakeDisplay(target, wake_epoch)
  -> wait for fresh physical signal
  -> fenced grant only after signal

Wake unsupported, rejected, stale, timed out, or no signal
  -> invalidate request and wake identities
  -> keep input on SERVER_HOST
  -> command and freshly verify shared display on SERVER_HOST
  -> notify with the concrete failure reason
```

The central rule is:

```text
The shared display may select a remote host before verification.
The user's keyboard and pointer may not leave SERVER_HOST until the target
display signal, peer input bundle, request identity, grant, and lease are all
fresh and valid.
```
