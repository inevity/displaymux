# Lan Mouse Protocol Compatibility and Capability Negotiation Design

## 1. Status and Scope

This document replaces Lan Mouse's exact Git-commit runtime compatibility gate
with an explicit, authenticated protocol negotiation for the DTLS input and
control channel.

It refines the version-compatibility findings in
`reviesionandclipboradexplore.md` against the current input and clipboard
implementation.

The design is normative for:

- protocol compatibility between Lan Mouse peers
- capability advertisement and requirements
- connection-scoped negotiation and fencing
- keyboard and pointer readiness authorization
- mutating input/control message admission
- compatibility failure reporting
- rollout from the current exact-commit implementation

The design is host-neutral:

- `SERVER_HOST` is the host running the Lan Mouse ownership authority.
- A peer may run Linux, macOS, Windows, or another supported operating system.
- Git commit identity is diagnostic metadata, not an authorization predicate.
- Keyboard and pointer remain one atomic input bundle.
- Backend availability remains runtime readiness and is not a protocol
  capability.
- Clipboard keeps its existing independent TLS protocol version and capability
  negotiation. Clipboard capability is not duplicated in the DTLS input
  protocol.

There is no compatibility path for binaries that predate this negotiation.
The initial deployment is one coordinated cutover. A peer that does not send
the new offer remains online-but-unnegotiated and cannot receive or control
input.

## 2. Exact Semantics

### 2.1 Build Identity

Build identity answers:

> Which source and build produced this binary?

The current eight-byte `Hello.commit` remains available for logs, status, and
diagnostics. A commit mismatch may produce an informational diagnostic, but it
must never directly make a peer eligible or ineligible for input.

### 2.2 Protocol Epoch

The protocol epoch answers:

> Does this peer implement the same baseline wire semantics?

The epoch is an unsigned integer with exact-match semantics. It changes only
when an existing mandatory wire encoding or baseline semantic changes in a way
that cannot be represented by adding a capability.

Package versions and Git commits do not determine the protocol epoch.

### 2.3 Protocol Capability

A protocol capability answers:

> Which immutable protocol behavior does this process implement?

Capabilities describe compiled protocol semantics. They do not describe
whether an OS permission is currently granted, whether an emulation backend
successfully started, whether a display is awake, or whether a peer is
reachable.

Each offer contains:

- `offered_capabilities`: behavior implemented by the sender
- `required_capabilities`: behavior the sender requires from the peer

Two offers are compatible exactly when:

```text
local.protocol_epoch == peer.protocol_epoch
AND local.required_capabilities is a subset of peer.offered_capabilities
AND peer.required_capabilities is a subset of local.offered_capabilities
```

The effective capability set is:

```text
effective_capabilities =
    local.offered_capabilities AND peer.offered_capabilities
```

### 2.4 Runtime Readiness

Runtime readiness answers:

> Can this process instance currently receive both keyboard and pointer input?

Readiness remains:

```text
{ keyboard_ready, pointer_ready, session_epoch }
```

It is valid only on the same authenticated DTLS connection after mutual
protocol negotiation is established. A capability-compatible peer with a
failed Windows `SendInput`, missing macOS Accessibility permission, or failed
Linux emulation backend remains ineligible because runtime readiness is false.

Transport liveness and backend readiness are separate. Receipt of a valid
heartbeat response proves that the authenticated connection is online.
`Pong(bool)` currently mixes those meanings by carrying an emulation-available
boolean; after cutover, that boolean is diagnostic only and cannot set
`TransportOnline` false. The negotiated `Readiness` event is the authoritative
backend state.

### 2.5 Control Eligibility

For a configured peer and its active outbound connection:

```text
ControlEligible(peer) ==
    TransportOnline(peer)
    AND NegotiationEstablished(peer.active_connection)
    AND RequiredCapabilitiesSatisfied(peer.active_connection)
    AND peer.keyboard_ready
    AND peer.pointer_ready
    AND peer.readiness_session_epoch != 0
```

No TV command, input lease, capture permit, `Enter`, or input event may be
derived from build identity alone.

## 3. Proven Current Baseline

The current implementation has five relevant properties.

1. `lan-mouse-proto/src/lib.rs` uses append-only `EventType` values and fixed
   datagram encoding. Unknown event types are ignored without dropping the DTLS
   connection.
2. `ProtoEvent::Hello` carries an eight-byte short Git commit. The connect side
   sends it after DTLS authentication and the listen side echoes its own value.
3. `ClientManager::peer_protocol_compatible` currently requires exact commit
   equality. `peer_input_readiness` masks both readiness bits when commits
   differ.
4. The receive-side `ListenTask` currently processes `Enter`, `Input`,
   `Leave`, and release messages without first checking the peer's compatibility
   state. Sender-side gating is therefore not a complete admission boundary.
5. The clipboard transport already has a separate framed TLS protocol with
   `PROTOCOL_VERSION`, `ClipboardHello`, process-session fencing, and
   `CLIPBOARD_TEXT_V1`. It must remain isolated from DTLS input compatibility.

The existing `Hello` comments describe build mismatch as a soft warning, while
the current `ClientManager` treats it as a hard control gate. This design makes
the soft diagnostic semantics real and moves hard authorization to an explicit
negotiation state.

## 4. Goals and Non-Goals

### 4.1 Goals

- Permit different Git commits to exchange input when their protocol epoch and
  required capabilities are compatible.
- Preserve the server host as the input and display fallback on every
  negotiation failure.
- Require mutual negotiation before any mutating input or control message.
- Preserve atomic keyboard-and-pointer ownership.
- Fence stale offers, results, readiness, disconnects, and datagrams to the
  exact authenticated connection.
- Recover from a lost offer or result using the existing heartbeat cycle.
- Produce exact incompatibility reasons without treating a build difference as
  a protocol failure.
- Allow a future implementation-only Windows, macOS, or Linux change to deploy
  only to the affected host.
- Avoid additional background tasks, unbounded queues, or state locks held
  across I/O.

### 4.2 Non-Goals

- Accepting the current commit-only handshake after cutover
- Compatibility with upstream Lan Mouse, Deskflow, Synergy, Barrier, or Input
  Leap protocols
- Negotiating native backend permissions or current backend availability
- Using Cargo package SemVer as a wire-compatibility decision
- Allowing configuration to disable mandatory atomic-input safety capabilities
- Renegotiating capabilities in place during one DTLS connection
- Moving clipboard payloads or clipboard negotiation onto the DTLS input path
- Inferring which operating systems need deployment from changed source paths

## 5. Safety and Availability Invariants

### 5.1 Server Fallback

```text
Not ControlEligible(target)
    => keyboard_owner == SERVER_HOST
       AND pointer_owner == SERVER_HOST
       AND (
           display is verified on SERVER_HOST
           OR verified fallback to SERVER_HOST is active
       )
```

A compatibility failure never transfers only one input path and never leaves a
remote lease valid.

### 5.2 Mutual Negotiation Before Mutation

```text
AcceptedMutatingMessage(connection)
    => NegotiationEstablished(connection)
```

Mutating messages are:

- `Enter`
- `Leave`
- `Ack` when it advances an enter/leave transaction
- `Input`
- `Readiness`
- `ReleaseRequest`
- `ReleaseAck`

Bootstrap and liveness messages are:

- `Ping`
- `Pong`
- build `Hello`
- `ProtocolOffer`
- `ProtocolResult`

### 5.3 Build Independence

```text
Compatible(local, peer)
```

depends only on protocol epoch and capability sets. Changing either build
commit while leaving those values unchanged cannot change compatibility.

### 5.4 Required Capability Soundness

```text
NegotiationEstablished(connection)
    => local.required is a subset of peer.offered
       AND peer.required is a subset of local.offered
       AND effective == local.offered intersection peer.offered
```

### 5.5 Connection Fencing

```text
MessageChangesState(message)
    => message.connection_id == active_connection_id
```

An old connection's delayed result, readiness, acknowledgement, or disconnect
cannot authorize or clear a replacement connection.

### 5.6 Readiness Binding

```text
ReadinessAccepted(connection)
    => NegotiationEstablished(connection)
```

Readiness received before negotiation is ignored and cannot be replayed after a
replacement connection is established.

### 5.7 Atomic Input Bundle

```text
keyboard_owner == pointer_owner
```

The initial required capability set explicitly includes atomic
keyboard-pointer semantics. Negotiation cannot downgrade to keyboard-only or
pointer-only control.

### 5.8 Receiver Enforcement

Sender-side checks are not sufficient. Every receiving connection independently
rejects mutating messages until its own negotiation state is established.

## 6. Protocol Domain Model

The following types are logical types even where their wire representation is
an integer:

```rust
struct ProtocolEpoch(u32);
struct CapabilitySet(u64);
struct NegotiationId(u64);
struct ConnectionId(u64);

struct ProtocolOffer {
    negotiation_id: NegotiationId,
    protocol_epoch: ProtocolEpoch,
    offered_capabilities: CapabilitySet,
    required_capabilities: CapabilitySet,
}

enum ProtocolDecision {
    Accept,
    EpochMismatch,
    MissingCapabilities,
}

struct ProtocolResult {
    negotiation_id: NegotiationId,
    decision: ProtocolDecision,
    effective_capabilities: CapabilitySet,
}
```

`ProtocolResult.negotiation_id` echoes the identifier from the offer being
answered. It does not identify the result sender's own offer.

Each authenticated connection owns:

```rust
struct ConnectionNegotiation {
    connection_id: ConnectionId,
    local_offer: ProtocolOffer,
    peer_offer: Option<ProtocolOffer>,
    local_result: Option<ProtocolResult>,
    peer_result: Option<ProtocolResult>,
    phase: NegotiationPhase,
}

enum NegotiationPhase {
    Negotiating,
    Established {
        peer_epoch: ProtocolEpoch,
        peer_offered: CapabilitySet,
        peer_required: CapabilitySet,
        effective: CapabilitySet,
    },
    Rejected(NegotiationFailure),
}
```

The phase is derived from the retained offer/result facts. It is not updated by
build identity or readiness.

## 7. Wire Protocol

### 7.1 Stable Bootstrap Events

Append two event IDs without renumbering existing IDs:

```text
15 = ProtocolOffer
16 = ProtocolResult
```

All integer fields use the existing canonical big-endian encoding.

`ProtocolOffer`:

```text
event_type             u8    1 byte
negotiation_id         u64   8 bytes
protocol_epoch         u32   4 bytes
offered_capabilities   u64   8 bytes
required_capabilities  u64   8 bytes
                              --------
                              29 bytes
```

`ProtocolResult`:

```text
event_type              u8    1 byte
negotiation_id          u64   8 bytes
decision                u8    1 byte
effective_capabilities  u64   8 bytes
                               --------
                               18 bytes
```

`MAX_EVENT_SIZE` increases from 21 to 29 bytes. This remains far below a normal
network MTU and does not introduce allocation in the DTLS input path.

The bootstrap event layouts and decision values are permanent. A future
protocol epoch changes the semantics of non-bootstrap protocol behavior, not
the ability to decode these two messages.

### 7.2 Build Hello

Existing event ID 11 remains wire-compatible:

```text
Hello { commit: [u8; 8] }
```

It is renamed in prose to `BuildIdentity`, but its event ID and encoding are
unchanged. Receiving it updates diagnostic state only.

### 7.3 Offer Identifier

Each side creates one nonzero random `NegotiationId` when the authenticated
DTLS connection actor starts. It remains immutable for that connection.

- An exact duplicate offer is idempotent.
- A changed offer with the same negotiation ID is a protocol violation.
- A second negotiation ID on the same connection is a protocol violation.
- A new DTLS connection receives a new local `ConnectionId` and negotiation ID.

Capability changes require process restart or connection replacement. They
cannot race an active input lease through in-place renegotiation.

### 7.4 Compatibility Algorithm

For a received peer offer:

```rust
fn evaluate(local: ProtocolOffer, peer: ProtocolOffer) -> ProtocolResult {
    if local.protocol_epoch != peer.protocol_epoch {
        return reject(peer.negotiation_id, EpochMismatch);
    }

    let missing_for_local =
        local.required_capabilities & !peer.offered_capabilities;
    let missing_for_peer =
        peer.required_capabilities & !local.offered_capabilities;

    if missing_for_local != 0 || missing_for_peer != 0 {
        return reject(peer.negotiation_id, MissingCapabilities);
    }

    accept(
        peer.negotiation_id,
        local.offered_capabilities & peer.offered_capabilities,
    )
}
```

An accepted peer result is valid only when:

- it echoes the local offer's negotiation ID
- its decision is `Accept`
- its effective set equals the locally computed intersection
- the retained peer offer is locally acceptable

A result may arrive before the peer offer because DTLS datagrams may be
reordered. It is retained but cannot establish negotiation until the peer offer
also arrives and validates.

### 7.5 Mutual Handshake

For peers A and B on one authenticated connection:

```text
1. A -> B: BuildIdentity(A)
2. A -> B: ProtocolOffer(A)
3. B -> A: BuildIdentity(B)
4. B -> A: ProtocolOffer(B)
5. B -> A: ProtocolResult(offer=A, Accept|Reject)
6. A -> B: ProtocolResult(offer=B, Accept|Reject)
7. Each side enters Established only after:
     - it accepts the peer offer, and
     - it receives a valid Accept for its own offer.
8. A side publishes Readiness only after it enters Established.
```

Readiness is the final practical synchronization barrier. A sender cannot
reserve or commit input until it receives readiness. Therefore, even if A
enters `Established` just before B receives A's result, A still cannot send
`Enter`: B does not publish readiness until B is also established.

The connection has directional runtime roles even though negotiation is
mutual. On A's configured outbound connection to B, B's listener publishes B's
readiness to A. A publishes its own readiness on B's separate outbound
connection to A. This prevents readiness observed on one DTLS connection from
authorizing input sent on another.

### 7.6 Retransmission and Convergence

No new retry timer is added.

- Send the local offer immediately after DTLS authentication.
- Until established or rejected, resend the local offer during the existing
  heartbeat cycle.
- The listener resends its offer while it has not received a valid result for
  that offer, even if the initiator already considers its side established.
- An exact duplicate peer offer causes the same result to be resent.
- After establishment, a duplicate offer/result is answered idempotently.
- A rejected epoch/capability combination remains connected for diagnostics
  and heartbeat, but no readiness or mutating traffic is admitted.

This makes a lost offer or result recover without adding a task, queue, timeout,
or arbitrary retry count.

### 7.7 Malformed Bootstrap Traffic

An authenticated peer that changes an offer in place, sends an invalid decision
value, sends a mismatched effective set, or otherwise violates the bootstrap
format is disconnected. Its readiness is cleared before the disconnect event
is published.

An ordinary epoch or capability mismatch is not malformed. It remains a stable
online-but-incompatible state so status and notifications can report the exact
reason without connection churn.

## 8. Initial Capability Registry

Capability bits are append-only and immutable. A bit is never reused for a
different meaning.

```text
bit 0  INPUT_EVENTS_V1
       Existing Enter, Leave, Ack, keyboard, pointer, Ping, and Pong semantics.

bit 1  ATOMIC_KEYBOARD_POINTER_V1
       Keyboard and pointer are one all-or-nothing ownership bundle. No
       single-path commit or degraded ownership is permitted.

bit 2  READINESS_SESSION_EPOCH_V1
       Readiness carries both backend states and a nonzero emulation-session
       epoch that fences reservation, grant, commit, and renewal.

bit 3  RELEASE_FENCE_V1
       ReleaseRequest and ReleaseAck use a release epoch; stale release
       acknowledgements cannot complete a newer return.

bit 4  CENTER_BEFORE_ENTER_ACK_V1
       The target releases its local capture and centers its pointer before
       acknowledging Enter. Failure or timeout denies entry.
```

The initial constants are:

```text
OFFERED_INPUT_CAPABILITIES =
    INPUT_EVENTS_V1
    | ATOMIC_KEYBOARD_POINTER_V1
    | READINESS_SESSION_EPOCH_V1
    | RELEASE_FENCE_V1
    | CENTER_BEFORE_ENTER_ACK_V1

REQUIRED_INPUT_CAPABILITIES = OFFERED_INPUT_CAPABILITIES
```

The required set is a code-owned safety policy, not a user configuration.
Unit tests require every locally required bit to be locally offered.

Runtime backend state is deliberately absent from this registry:

- Windows input emulation failed -> readiness false
- macOS Accessibility permission missing -> readiness false
- Linux pointer backend unavailable -> pointer readiness false
- protocol implementation lacks atomic bundle semantics -> capability missing

Clipboard uses `lan-mouse-clipboard`'s `PROTOCOL_VERSION` and
`CLIPBOARD_TEXT_V1` on its own TLS connection. A clipboard capability mismatch
skips clipboard transfer and never affects DTLS input eligibility.

## 9. Connection Ownership and Fencing

### 9.1 Outbound Connection Actor

The configured-client connection path owns:

- DTLS connection
- heartbeat
- local and peer offers/results
- peer readiness accepted on this connection
- the connection-local negotiation phase

The current split `ping_pong` and `receive_loop` tasks should be refined into
one connection actor, or use one actor-owned state channel. No shared state
mutex may be held across `send`, `recv`, sleep, or service notification.

`ClientState` receives one coherent snapshot from this actor:

```text
transport_online
active_connection_id
negotiation_phase
peer protocol fields
effective capabilities
keyboard readiness
pointer readiness
readiness session epoch
peer build identity
```

The input sender uses only this outbound connection's negotiation and readiness
to reserve and transmit input.

### 9.2 Incoming Connection Registry

`LanMouseListener` assigns a monotonically increasing local `ConnectionId` to
every accepted DTLS connection and includes it in:

```text
Accept
Msg
Disconnected
```

`ListenTask` stores one negotiation record per connection ID. Socket address is
observability metadata, not the identity fence.

An old disconnect removes state only when its connection ID is still current.
This mirrors the deterministic replacement fencing already used by the
clipboard peer registry.

### 9.3 No Cross-Direction Authorization

An offer seen on an incoming connection cannot authorize an outbound
connection, and an outbound offer cannot authorize a different incoming
connection.

The current incoming `PeerHello` fallback may still update build diagnostics,
but it must not update the outbound negotiation gate. If the outbound
connection is broken, that host cannot carry the sender's input and must remain
ineligible.

## 10. Sender and Receiver Admission

### 10.1 Sender Gate

Replace exact-commit masking in `ClientManager::peer_input_readiness` with:

```text
transport online
AND active outbound connection established
AND all required capabilities effective
AND keyboard ready
AND pointer ready
AND readiness session epoch nonzero
```

`Service::handle_capture_candidate` must evaluate this unified snapshot before:

- bundle reservation
- controller preparation
- TV switching
- capture permit arming

If negotiation is not established, the failure is protocol-specific and the
manual recovery path may be armed for the exact target using the existing
failed-enter policy.

### 10.2 Receiver Gate

`ListenTask` processes bootstrap/liveness traffic before establishment and
drops all mutating traffic.

After establishment:

- accept readiness only on the outbound actor that owns it
- accept `Enter` only for the established incoming connection
- bind pending enter and entered connection state to `ConnectionId`
- accept `Input` only from an established connection that completed Enter
- accept release messages only from the established connection

Build identity never opens this gate.

### 10.3 Negotiation Loss During Active Ownership

Negotiation cannot change in place. It is lost only through disconnect,
replacement, malformed traffic, or process restart.

The actor publishes one atomic transition:

```text
negotiation = not established
keyboard_ready = false
pointer_ready = false
readiness_session_epoch = 0
keyboard_owner = SERVER_HOST
pointer_owner = SERVER_HOST
```

The existing peer-readiness loss path then:

1. revokes reservation/grant/lease
2. disables both remote input paths
3. releases pressed keys and modifiers
4. starts verified display fallback to `SERVER_HOST`

There is no intermediate state where readiness remains true after compatibility
is revoked.

## 11. State Machine

### 11.1 Negotiation States

| State | Meaning | Mutating traffic |
|---|---|---|
| `Negotiating` | DTLS authenticated; mutual offer/result incomplete | denied |
| `Established` | Both offers accepted and local offer acknowledged | allowed subject to readiness/session fences |
| `RejectedEpoch` | Peer epoch differs | denied |
| `RejectedCapabilities` | At least one required set is not satisfied | denied |
| `ProtocolViolation` | Bootstrap message changed or contradicted prior state | denied; disconnect |
| `Disconnected` | Connection no longer active | denied |

### 11.2 Allowed Transitions

```text
Disconnected -> Negotiating
Negotiating  -> Established
Negotiating  -> RejectedEpoch
Negotiating  -> RejectedCapabilities
Negotiating  -> ProtocolViolation
Established  -> ProtocolViolation
Established  -> Disconnected
Rejected*    -> Disconnected
```

There is no `Rejected -> Established` transition on the same connection.
Changed software or policy creates a new process/connection and a fresh
negotiation.

### 11.3 Idempotence

- duplicate identical offer: retain state and resend the same result
- duplicate identical result: retain state
- duplicate readiness with same or newer accepted emulation epoch: existing
  readiness epoch rules apply
- duplicate stale disconnect: ignored by connection ID
- duplicate build identity: diagnostic update only

## 12. Failure Semantics and User Reasons

| Evidence | State | Input/display result | Reason code |
|---|---|---|---|
| Transport not connected | offline | keep/return server | `peer_offline` |
| Connected, no peer offer yet | negotiating | keep server | `peer_protocol_handshake_missing` |
| Epoch differs | rejected | keep server | `peer_protocol_epoch_mismatch` |
| Peer lacks local required bits | rejected | keep server | `peer_missing_required_capabilities` |
| Peer requires unsupported local bits | rejected | keep server | `local_missing_peer_required_capabilities` |
| Offer/result contradiction | violation/disconnect | keep/return server | `peer_protocol_violation` |
| Compatible, readiness session absent | established, not ready | keep server | `peer_readiness_handshake_missing` |
| Compatible, keyboard unavailable | established, not ready | keep server | `peer_keyboard_unavailable` |
| Compatible, pointer unavailable | established, not ready | keep server | `peer_pointer_unavailable` |
| Build commit differs | established or not | no direct effect | diagnostic `peer_build_differs` |

Notifications should include capability names and masks, but not present a Git
commit difference as the failure reason when protocol negotiation succeeded.

## 13. Observability and IPC

Extend `ClientState` or a nested protocol-status record with:

```text
active_connection_id
protocol_phase
peer_protocol_epoch
peer_offered_capabilities
peer_required_capabilities
effective_capabilities
protocol_failure_reason
peer_commit
```

Structured events:

```json
{"event":"protocol_offer_sent","connection_id":17,"epoch":1,"offered":"0x1f","required":"0x1f"}
{"event":"protocol_offer_received","connection_id":17,"peer_epoch":1,"peer_offered":"0x1f","peer_required":"0x1f"}
{"event":"protocol_established","connection_id":17,"effective":"0x1f","peer_build":"56117cf"}
{"event":"protocol_rejected","connection_id":17,"reason":"missing_capabilities","missing_for_local":"0x08","missing_for_peer":"0x00"}
{"event":"peer_build_differs","connection_id":17,"local_build":"56117cf","peer_build":"e20bc6a"}
```

Requirements:

- one transition log per state change, not one warning per heartbeat retry
- capability masks plus decoded names
- no build mismatch system notification after successful negotiation
- status distinguishes transport online, protocol established, and input ready
- disconnect logs the exact connection ID whose state was cleared

## 14. Performance and Deadlock Constraints

### 14.1 Performance

- Offer/result messages are at most 29 bytes.
- Encoding remains stack-based and allocation-free.
- Negotiation uses the existing authenticated DTLS connection.
- Retransmission uses the existing heartbeat cycle.
- State is constant-size per live connection.
- No source commit, Cargo metadata, filesystem access, HTTP request, or TV
  operation occurs in negotiation.
- Build identity formatting is outside the input hot path.

### 14.2 Deadlock Exclusion

Both sides send an offer immediately. Neither side waits to receive before
sending, so symmetric startup cannot deadlock.

The per-connection actor owns negotiation state. It never:

- holds a state mutex while awaiting network I/O
- waits for service ownership while the service waits for its response
- waits for runtime readiness before sending a compatibility result
- waits for TV state during protocol negotiation

An incompatible peer reaches a stable rejected state with server-host input
available. Rejection is not a system deadlock.

### 14.3 Liveness Assumption

Under an authenticated connection that remains alive and eventual delivery of
one retried offer and result in each direction:

```text
Compatible(local, peer) ~> NegotiationEstablished(connection)
```

If both emulation paths eventually become ready:

```text
NegotiationEstablished(connection) ~> ControlEligible(peer)
```

No liveness claim is made under permanent packet loss, process termination, or
backend failure. Safety still holds because input remains or returns to the
server host.

## 15. TLA+ Refinement

The finite model should use:

```tla
VARIABLES
    connected,
    localOffer,
    peerOffer,
    localResult,
    peerResult,
    negotiation,
    readiness,
    activeConnection,
    keyboardOwner,
    pointerOwner,
    acceptedMutations,
    buildIdentity
```

Core definitions:

```tla
Compatible(a, b) ==
    /\ epoch[a] = epoch[b]
    /\ required[a] \subseteq offered[b]
    /\ required[b] \subseteq offered[a]

Effective(a, b) == offered[a] \cap offered[b]

Established(a, b, c) ==
    /\ connected[c]
    /\ negotiation[a][c] = "established"
    /\ negotiation[b][c] = "established"
    /\ Compatible(a, b)

ControlEligible(host, c) ==
    /\ Established(SERVER_HOST, host, c)
    /\ readiness[host].connection = c
    /\ readiness[host].keyboard
    /\ readiness[host].pointer
    /\ readiness[host].sessionEpoch # 0
```

Actions:

```text
Connect
SendOffer
DeliverOffer
SendResult
DeliverResult
PublishReadiness
DeliverReadiness
AcceptMutation
ReplaceConnection
Disconnect
RuntimeReadinessLoss
CommitInputBundle
FallbackToServer
```

The message network is a set or bag so the model may lose, duplicate, delay,
and reorder offer, result, readiness, and disconnect-related events.

Required invariants:

```tla
MutationsRequireNegotiation ==
    \A m \in acceptedMutations :
        Established(m.sender, m.receiver, m.connection)

RequiredCapabilitiesSatisfied ==
    \A c \in EstablishedConnections :
        /\ required[Local(c)] \subseteq offered[Peer(c)]
        /\ required[Peer(c)] \subseteq offered[Local(c)]

BuildIdentityNotInAuthorization ==
    \A h \in RemoteHosts :
        controlAuthorized[h] =
            /\ Established(SERVER_HOST, h, activeConnection[h])
            /\ readiness[h].connection = activeConnection[h]
            /\ readiness[h].keyboard
            /\ readiness[h].pointer
            /\ readiness[h].sessionEpoch # 0

StaleConnectionCannotAuthorize ==
    \A h \in Hosts :
        readiness[h].connection = activeConnection[h]

InputOwnershipAtomic ==
    keyboardOwner = pointerOwner

RemoteOwnershipRequiresEligibility ==
    \A h \in RemoteHosts :
        keyboardOwner = h => ControlEligible(h, activeConnection[h])

IncompatibleFallsBack ==
    keyboardOwner \in RemoteHosts
    /\ ~ControlEligible(keyboardOwner, activeConnection[keyboardOwner])
        => fallbackRequired
```

Liveness properties, under weak fairness for retry and delivery actions:

```tla
CompatibleEventuallyEstablished ==
    CompatibleConnected ~> MutuallyEstablished

ReadyEventuallyEligible ==
    MutuallyEstablished /\ BackendEventuallyReady ~> ControlEligible

FailureEventuallyLocal ==
    ActiveRemote /\ CompatibilityLost ~> OwnersAreServer
```

Counterexamples the model must cover:

1. Different builds, same epoch/capabilities -> establish.
2. Same build, different epoch -> reject.
3. Same epoch, missing atomic-input capability -> reject.
4. Unknown optional offered capability -> establish with the intersection.
5. First offer lost -> heartbeat retry converges.
6. Result arrives before offer -> cannot establish until offer validates.
7. Readiness arrives before establishment -> ignored.
8. Old result arrives after connection replacement -> ignored.
9. Old disconnect arrives after replacement -> new connection remains.
10. Compatibility is lost during remote ownership -> both owners revert.
11. One peer never sends an offer -> no mutation and owners remain local.
12. Receiver gets `Enter` before establishment -> no release, center, or Ack.

## 16. Version Evolution Rules

### 16.1 Implementation-Only Change

Keep epoch and capability sets unchanged.

Result:

- only the affected native host needs a new binary
- build identities differ
- negotiation still succeeds

### 16.2 New Optional Feature

Allocate a new immutable capability bit and append any new event IDs.

- Advertise the bit only when implemented.
- Send feature events only when the bit is in the effective set.
- Do not add the bit to the required set.
- Epoch remains unchanged.

### 16.3 New Mandatory Capability

Use a two-stage rollout:

1. Deploy binaries that offer the capability but do not require it.
2. After every required peer offers it, deploy the policy/binary that adds it
   to the required set.

This avoids an unnecessary coordinated outage while still ending in a
fail-closed mandatory policy.

### 16.4 Breaking Baseline Change

Bump the protocol epoch only when an existing mandatory event encoding or
baseline semantic cannot coexist.

- Stage every host first.
- Activate remote peers while the server host remains the fallback.
- Activate the server host last.
- Do not accept the old epoch.

### 16.5 Registry Rules

- Event IDs are append-only.
- Capability bits are append-only.
- Decision values are append-only.
- Unknown offered bits are harmless and excluded from the effective
  intersection unless locally implemented.
- An unknown required bit causes capability rejection.
- Removed features leave their bit reserved forever.

## 17. Initial Cutover

Because existing binaries do not send `ProtocolOffer`, the first deployment is
coordinated and intentionally has no legacy compatibility mode.

1. Build and test the same initial negotiation implementation for Linux,
   macOS, and Windows.
2. Stage all three binaries without replacing running processes.
3. Verify binary digests and native service paths.
4. Activate remote peers first. During this interval, the old server does not
   authorize them; input remains on `SERVER_HOST`.
5. Activate the server-host binary last.
6. Verify each peer reports:
   - transport online
   - protocol established
   - expected effective capability set
   - nonzero readiness session
   - keyboard and pointer ready
7. Exercise one enter and return for each peer.

After this cutover, compatible implementation-only changes use per-host
deployment. There is no requirement to rebuild unchanged hosts merely because
the source commit changed.

Rollback across the initial cutover is coordinated: either keep all hosts on
the new protocol epoch or restore all affected hosts to the old build. Future
same-epoch implementation changes may roll back per host.

## 18. Implementation Map

### 18.1 `lan-mouse-proto`

- Add `ProtocolOffer`, `ProtocolResult`, decision encoding, and capability
  constants.
- Append event IDs 15 and 16.
- Increase `MAX_EVENT_SIZE` to 29.
- Add exact wire-layout, round-trip, unknown-ID, and non-renumbering tests.
- Keep build `Hello` encoding unchanged.

### 18.2 Outbound Connection Path

- Give each connection an opaque local `ConnectionId`.
- Make one actor own heartbeat, negotiation, receive admission, and readiness.
- Send build identity and offer immediately after DTLS authentication.
- Retransmit offers through the existing heartbeat cycle.
- Derive transport liveness from valid heartbeat traffic, not from the legacy
  `Pong` emulation-available boolean.
- Publish a coherent protocol/readiness snapshot to `ClientManager`.
- Clear negotiation and readiness atomically on replacement/disconnect.

### 18.3 Incoming Listener Path

- Add `ConnectionId` to accept/message/disconnect events.
- Store negotiation per connection ID.
- Reply to offers idempotently.
- Publish readiness only after establishment.
- Reject every mutating event before establishment.
- Bind pending enter, entered connections, and release epochs to connection ID.

### 18.4 Client and IPC State

- Replace `peer_protocol_compatible == exact commit` with explicit negotiation
  state.
- Retain `peer_commit` for diagnostics.
- Add protocol epoch, capability masks, effective set, connection ID, phase,
  and failure reason to IPC state.
- Render capability names in CLI/status output.

### 18.5 Service and Controller Boundary

- Compute peer control eligibility from one coherent snapshot.
- Map protocol failures to the reason codes in Section 12.
- Feed incompatible/not-established readiness to the existing atomic bundle
  gate.
- Preserve existing manual OLED recovery behavior for a user-confirmed failed
  enter.
- Do not change the `tv-multiview` controller API; it continues to receive
  only valid readiness and lease transitions.

### 18.6 Clipboard

- Make no DTLS capability dependency for clipboard.
- Keep clipboard `PROTOCOL_VERSION`, `ClipboardHello`,
  `ProcessSessionId`, and `CLIPBOARD_TEXT_V1`.
- A clipboard protocol mismatch remains clipboard-local and never changes input
  readiness.

### 18.7 Deployment

- Keep Ansible free of a global `lan_mouse_revision` pin.
- Preserve native build/test/install on the explicitly selected hosts.
- Add optional status verification for protocol epoch and effective
  capabilities after service restart.
- Do not infer target hosts from Git paths.
- Do not reinstall unchanged hosts for a same-epoch, capability-compatible
  change.

No new Rust dependency is required for this design.

## 19. Verification Matrix

### 19.1 Protocol Unit Tests

- Offer and result encode/decode at exact lengths.
- Existing event IDs remain unchanged.
- New IDs are append-only.
- Compatibility truth table covers both missing-capability directions.
- Effective mask must equal the intersection.
- Different build commits do not affect compatibility.
- Duplicate exact offer/result is idempotent.
- Changed offer/result on one negotiation ID is rejected.

### 19.2 Connection Actor Tests

- Offer/result loss followed by retry establishes.
- Result-before-offer reordering does not establish early.
- Readiness-before-establishment is ignored.
- Disconnect clears negotiation and readiness together.
- Old disconnect cannot clear replacement connection state.
- Rejected peers remain heartbeat-visible but cannot mutate.

### 19.3 Input Safety Tests

- Sender cannot reserve before established readiness.
- Receiver ignores `Enter`, `Input`, and release messages before establishment.
- Missing `ATOMIC_KEYBOARD_POINTER_V1` denies both paths.
- Negotiation loss during an active lease releases both paths and requests
  server fallback.
- Build mismatch plus compatible negotiation allows enter and return.
- Same build plus incompatible negotiation denies enter and return.

### 19.4 Cross-OS Acceptance

For Linux, macOS, and Windows native binaries:

- establish with different commits but the same epoch/capability policy
- report backend readiness independently of capability negotiation
- switch server -> remote and remote -> server
- preserve key release and pointer centering
- fail closed when the remote service is replaced by an incompatible-epoch
  binary

## 20. Acceptance Criteria

The design is complete when:

1. Git commit equality is absent from every input authorization predicate.
2. Build identity remains visible in logs/status.
3. Both sender and receiver enforce connection-scoped negotiation.
4. Protocol epoch and required-capability mismatch keep both input owners on
   the server host.
5. A different-commit compatible peer can enter and return.
6. An old pre-negotiation peer remains unnegotiated and cannot mutate input.
7. Readiness cannot authorize a different or replacement connection.
8. Compatibility loss during remote ownership converges to verified server
   fallback.
9. Clipboard compatibility remains isolated.
10. A same-epoch implementation-only change can be built, installed, and
    restarted on one native host without rebuilding the others.

## 21. Architecture Decisions

### ADR-001: Build Identity Is Diagnostic

**Decision:** Git commit identity remains observable but never authorizes
input.

**Rationale:** A source change does not imply a wire-semantic change, and an
equal commit does not prove identical build features or runtime readiness.

### ADR-002: Exact Epoch Plus Capability Sets

**Decision:** Baseline compatibility requires an exact protocol epoch; additive
behavior uses offered/required capability sets.

**Rationale:** This gives a small deterministic rule and avoids pretending that
package SemVer describes negotiated runtime behavior.

### ADR-003: Mutual Authenticated Negotiation

**Decision:** Both peers exchange and accept offers on every authenticated DTLS
connection before readiness or mutation.

**Rationale:** Sender-only compatibility checks leave the receiver exposed to
early or incompatible mutating messages.

### ADR-004: Negotiation Is Connection-Scoped

**Decision:** Incoming and outgoing DTLS connections negotiate independently
and use opaque local connection IDs.

**Rationale:** Socket addresses, build identity, and a handshake observed on a
different connection cannot fence delayed readiness, results, or disconnects.

### ADR-005: Readiness Is Published After Establishment

**Decision:** A peer sends readiness only after its local negotiation is fully
established.

**Rationale:** The sender already requires readiness before input reservation.
Using it as the post-negotiation barrier prevents an early `Enter` while the
receiver is still waiting for the reciprocal result.

### ADR-006: No Legacy Mutating Path

**Decision:** Peers without `ProtocolOffer` remain unnegotiated and fail closed.

**Rationale:** A fallback to commit equality or unversioned readiness would
retain two authorization protocols and recreate ambiguous edge cases.

### ADR-007: Clipboard Negotiates Separately

**Decision:** Clipboard keeps its existing TLS version/capability handshake.

**Rationale:** Clipboard is optional, reliable bulk/control traffic. Coupling it
to latency-sensitive DTLS input eligibility would violate clipboard failure
isolation.

### ADR-008: Existing Heartbeat Drives Retry

**Decision:** Offer/result retransmission uses the current heartbeat cycle.

**Rationale:** It provides convergence without a new timer, retry threshold,
task, or unbounded queue.
