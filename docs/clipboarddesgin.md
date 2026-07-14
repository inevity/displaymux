# Clipboard Handoff Design for Lan Mouse

## 1. Status and Scope

This document defines clipboard sharing for the current Lan Mouse input-switch implementation. It refines the evidence collected in `clipboardexplore.md` into a normative protocol, state machine, native-backend contract, failure policy, and implementation map.

The design is host-neutral:

- `SERVER_HOST` means the host running the Lan Mouse ownership authority. It is not inherently Linux.
- A `REMOTE_HOST` may run Linux, macOS, Windows, or another supported operating system.
- Keyboard and pointer remain one atomic input bundle.
- Clipboard follows committed input ownership, but clipboard is never part of input readiness or input commit.

The checked finite abstraction is `../tla/ClipboardHandoff.tla` with its TLC configurations. The existing `TvDisplaySwitch.tla` remains authoritative for display selection, signal verification, input readiness, and server-host fallback. Clipboard composes with that model; it does not replace it.

## 2. Exact User-Visible Semantics

### 2.1 Meaning of Clipboard Sharing

Clipboard sharing means the platform's ordinary user clipboard:

- Windows system clipboard
- macOS general pasteboard
- Wayland clipboard selection
- X11 `CLIPBOARD` selection

Linux X11 `PRIMARY`, selection history, files, rich text, images, and application-private formats are not V1 clipboard data.

### 2.2 Ownership Rule

The normative rule is:

> A committed input-owner transition may hand the previous owner's latest stable supported clipboard value to the new input owner.

Consequences:

1. Only the host holding the current authority-issued ownership token may originate a snapshot.
2. Clipboard changes on inactive hosts are local and are never published in the background.
3. Clipboard is sampled only for a real input-owner transition, not for edge priming, display wake, TV preparation, heartbeat, or reconnect.
4. Input commit never waits for snapshot capture, target preparation, transport, target application, or an acknowledgement.
5. A failed, unsupported, private, oversized, stale, or racing snapshot leaves the destination clipboard unchanged.
6. Explicit empty clipboard is data. Unavailable clipboard is a failure. Only explicit empty data may clear the destination.
7. A target-local clipboard change after destination preparation wins over the incoming snapshot.
8. No inactive host may overwrite the clipboard of the current owner.

This is transition-bound handoff, not continuous multi-master synchronization and not global last-writer-wins mirroring.

## 3. Goals and Non-Goals

### 3.1 Goals

- Copy text on the current input host, switch hosts, and paste the same text on the new input host.
- Preserve atomic keyboard/pointer switching under every clipboard failure.
- Fence stale work across switch aborts, rapid switches, service restarts, peer restarts, delayed packets, and duplicate messages.
- Preserve a destination-local clipboard change made during a handoff.
- Bound CPU, memory, queue depth, native clipboard lock time, and network allocation.
- Isolate native clipboard and clipboard transport failures from the DTLS input path.
- Support the same semantics on a Lan Mouse server running any supported operating system.
- Provide actionable status without logging clipboard contents.

### 3.2 Non-Goals

- Deskflow, Synergy, Input Leap, or Barrier wire compatibility
- Continuous clipboard synchronization while input ownership is unchanged
- Clipboard history synchronization
- File transfer, drag and drop, images, HTML, RTF, or arbitrary MIME data in V1
- Synchronizing X11 `PRIMARY`
- Inferring secrets from clipboard text
- Making clipboard availability a prerequisite for switching input or display
- Persisting clipboard payloads to disk
- Delivering every snapshot under peer failure; availability is best effort

## 4. Proven Current Implementation Baseline

The design attaches to existing transitions rather than inventing a second input-owner state machine.

### 4.1 Input Protocol Boundary

`lan-mouse-proto/src/lib.rs` defines a fixed maximum input event of 21 bytes. `ProtoEvent` carries input, liveness, readiness, enter/leave, release, and build identity. It has no bulk-data framing.

Therefore:

- Clipboard payloads do not enter `ProtoEvent`.
- Clipboard failure cannot corrupt, backpressure, resize, or disconnect the DTLS input stream.
- `clipboard_text_v1` is an optional capability of a separate protocol.

### 4.2 Server-to-Remote Commit

The current path is:

```text
confirmed edge intent
  -> Service::handle_capture_candidate
  -> BundleLeaseManager::reserve
  -> SwitchController::prepare
  -> BundleLeaseManager::arm_grant
  -> Capture::arm one-shot permit
  -> ICaptureEvent::CommitRequested
  -> Service::authorize_client_enter
  -> BundleLeaseManager::commit
  -> capture sends Enter
  -> remote releases local capture, centers pointer, and sends Ack
```

`BundleLeaseManager::commit` changes the local gate to `RemoteOwned` before the asynchronous controller commit acknowledgement. The clipboard target token is tied to that local ownership commit, not to the later controller response.

Clipboard hooks:

- Begin source snapshot and destination preparation only after the edge intent is confirmed and the bundle reservation succeeds.
- Mark target ownership active when `BundleLeaseManager::commit` succeeds.
- Cancel the clipboard handoff from every existing `fail_context`, lease invalidation, peer-readiness loss, and capture-denial path.

### 4.3 Remote-to-Server Return

The current return path is:

```text
remote cursor enters its EnterOnly barrier
  -> remote ICaptureEvent::CaptureBegin
  -> remote sends ReleaseRequest(release_epoch)
  -> server capture receives ReleaseRequest
  -> server releases local capture first
  -> server sends key-up/modifier reset/Leave
  -> server emits ICaptureEvent::ClientReleased
  -> server invalidates the remote lease and cleans up controller state
```

Local input availability is restored before network cleanup. Clipboard must preserve this ordering.

Clipboard hooks:

- The remote source requests a native snapshot before sending `ReleaseRequest`, but never waits for it.
- The server capture path emits a non-blocking `PeerReleaseStarted` event before releasing capture. This starts destination preparation.
- `ClientReleased` commits ownership back to `SERVER_HOST` and activates the prepared target token.
- A missing or late preparation, snapshot, channel, or apply result does not delay release.

### 4.4 Service Scheduling Constraint

`Service::run` is one Tokio `select!` loop. Native clipboard APIs may block, require a platform event loop, or own lazy clipboard data after a write. No native clipboard call may run directly in this service loop.

## 5. Architecture

### 5.1 Ownership Authority

The Lan Mouse server process is the sole clipboard ownership authority. It already decides when the atomic keyboard/pointer bundle leaves or returns to the server.

It owns:

- `authority_session_id`
- current `OwnershipToken`
- current owner host
- at most one active `Handoff`
- per-peer clipboard capabilities and process session
- at most one in-memory source snapshot for the active handoff

It does not own native clipboard APIs on other hosts and does not accept background publication from an inactive host.

### 5.2 Clipboard Coordinator

A new coordinator inside the Lan Mouse service consumes small state events only:

- input handoff started
- destination prepared or preparation failed
- source snapshot ready or skipped
- input ownership committed or aborted
- peer clipboard channel connected or disconnected
- target apply completed or skipped
- authority or peer process restarted

The coordinator never contains payload bytes in the main service event queue. Payload ownership remains in the clipboard transport/actor tasks as a bounded `Arc<[u8]>`.

### 5.3 Native Clipboard Actor

Every Lan Mouse process has one serialized native clipboard actor. The actor owns all platform handles, event loops, and lazy clipboard providers.

Its responsibilities are:

- maintain an opaque monotonic native generation
- snapshot the current clipboard with generation-before/generation-after validation
- identify supported, explicit-empty, private, unsupported, and oversized states
- record a destination baseline before activation
- apply only when the baseline still matches
- suppress its own native change notification after apply
- remain alive while the OS may request lazily owned clipboard data

The actor has a bounded command queue. Snapshot and apply results return through bounded channels. Queue saturation skips clipboard work; it never blocks input.

### 5.4 Clipboard Transport

The server maintains one full-duplex authenticated clipboard connection to each configured peer. A single connection carries:

- authority-to-peer target preparation and activation
- peer-to-authority snapshot offers
- authority-to-peer snapshot delivery
- bounded result/status messages

The transport is independent of `LanMouseConnection` and `LanMouseListener`. Closing it never closes DTLS.

### 5.5 Native Backend

Native backends implement a common actor command/result contract, not a lowest-common-denominator clipboard object. Platform-specific versioning, selection ownership, privacy markers, and event-loop requirements remain inside the backend.

## 6. Identity and Fencing

### 6.1 Process Session

Each process generates a random `process_session_id` at startup. A reconnect from the same certificate but a different process session invalidates messages, target preparations, and cached snapshots from the previous process.

### 6.2 Authority Session

The server generates a random `authority_session_id` at startup. It is never loaded from disk. Every ownership and handoff identity contains this value.

A server restart therefore invalidates all pre-restart work even if a delayed transport frame is delivered later.

### 6.3 Ownership Token

```text
OwnershipToken {
    authority_session_id: u128,
    ownership_epoch: u64,
    owner_host_id: HostId,
}
```

`ownership_epoch` is monotonically allocated by the authority and is never reused, including after an aborted handoff. Exhaustion is fatal configuration/runtime corruption; it must not wrap.

The current token means: this host is authorized to originate a transition snapshot. It does not grant permission to control input; the existing bundle lease remains authoritative for that.

### 6.4 Handoff Identity

```text
HandoffId {
    authority_session_id: u128,
    handoff_epoch: u64,
}

Handoff {
    id: HandoffId,
    source_host: HostId,
    source_token: OwnershipToken,
    target_host: HostId,
    target_token: OwnershipToken,
}
```

The authority allocates both `handoff_epoch` and the target ownership epoch when a real transition begins. Abort consumes those values; later handoffs receive new values.

### 6.5 Snapshot Identity

```text
SnapshotId {
    source_process_session_id: u128,
    sequence: u64,
}
```

The tuple `(HandoffId, SnapshotId)` identifies one payload. Target application is idempotent for that tuple.

### 6.6 Native Generation

`NativeGeneration` is an opaque backend-local version. It is never compared between hosts and is not a wall-clock timestamp.

It supports only:

```text
same generation     -> no observed local change
different generation -> local clipboard changed
```

## 7. Handoff State Machine

### 7.1 States

```text
Idle
PreparingTarget
CapturingSource
Ready
InFlight
Staged
Applied
Skipped(reason)
Canceled
```

Target preparation and source capture run concurrently. The displayed state is the furthest externally relevant phase; implementations retain independent source and target substates.

Terminal states are `Applied`, `Skipped`, and `Canceled`. Only one non-terminal handoff exists per authority.

### 7.2 BeginHandoff(source, target)

Preconditions:

- a real input-owner transition has begun
- `source` equals the current input owner
- `source_token` equals the current authority token
- source and target differ
- a new target ownership token and handoff ID can be allocated

Effects:

- invalidate the prior handoff, stage, and unsent snapshot
- allocate identities without reuse
- request source snapshot under `source_token`
- request target preparation under `target_token`
- leave input owner, keyboard owner, pointer owner, cursor, capture, TV state, and bundle lease unchanged

Missing clipboard capability sets the handoff to `Skipped(capability_missing)` but does not affect the input transition.

### 7.3 PrepareTarget

The target actor records its native generation before target ownership activates:

```text
TargetPreparation {
    handoff_id,
    target_token,
    target_process_session_id,
    baseline_generation,
}
```

Preparation is valid only if:

- the target process session still matches
- the target has not already activated `target_token`
- no newer handoff superseded this one
- the native backend can provide a trustworthy generation

Input does not wait for preparation. If target ownership commits first and no valid preparation exists, the handoff becomes `Skipped(target_not_prepared)`.

This ordering is required. Recording the baseline when payload data arrives would miss target-local changes made between input activation and payload arrival.

### 7.4 CaptureSource

The source actor records generation `g0`, reads bounded supported data, and records generation `g1`.

```text
g0 = g1 -> stable snapshot
g0 != g1 and source_token is still current -> retry within the fixed actor budget
g0 != g1 after ownership changed -> skip
```

The retry budget limits actor work; it is not an input timeout. Budget exhaustion returns `Skipped(source_changed)`.

Snapshot outcomes are:

```text
Text(bytes)
Empty
Unavailable(reason)
```

`Unavailable` is never serialized as clipboard content.

### 7.5 CommitInputOwner

The existing input state machine commits ownership when its existing readiness, lease, grant, and capture conditions pass.

Effects added by this design:

- publish the new `OwnershipToken` to the coordinator
- enqueue `OwnershipActivated(target_token)` to the target actor/peer
- preserve source capture, transport, and target stage work if their identities still match

There is no clipboard precondition. Clipboard cannot reject or roll back input commit.

### 7.6 TransferSnapshot

The source sends a snapshot offer tagged with its source ownership token and snapshot ID. The authority accepts it only when it matches the active handoff and the source token that was current when that handoff began.

The authority forwards the bounded payload with both source and target identities. It does not rewrite, persist, or log the content.

### 7.7 StageSnapshot

The target accepts a payload only when:

- TLS peer identity is authorized
- protocol and capability negotiation succeeded
- frame and payload lengths are within the negotiated limit before allocation
- payload hash and UTF-8 validation pass
- source process session, authority session, handoff ID, source token, and target token match
- a preparation for the same target token exists
- the snapshot has not already been applied

Staging records bytes and the preparation baseline. Staging does not change the native clipboard.

### 7.8 ApplySnapshot

Apply is permitted only when:

- current authority token equals `target_token`
- current input owner equals `target_host`
- target process session still matches
- native generation still equals `baseline_generation`
- the backend remains available
- this `(HandoffId, SnapshotId)` was not already applied

The actor writes `Text` or explicit `Empty`, records the post-write generation and applied ID, and suppresses the matching self-notification.

If any condition is false, the target drops the stage and leaves the native clipboard unchanged.

### 7.9 Abort and Supersession

Switch abort, readiness loss, lease expiry, controller failure, service reconfiguration, a newer handoff, or authority restart cancels the active clipboard handoff.

Cancellation:

- never waits for native or network acknowledgement
- invalidates prepared and staged target state
- drops locally queued payload ownership
- permits already-running bounded native work to finish and have its stale result discarded
- leaves input recovery to the existing input state machine

## 8. Successful Paths

### 8.1 Server Host to Remote Host

```text
1. Confirm deliberate edge intent.
2. Reserve the existing input bundle lease.
3. Allocate Handoff and target OwnershipToken.
4. Start local source snapshot.
5. Send PrepareTarget to the remote clipboard actor.
6. Continue TV preparation, verification, and grant unchanged.
7. Commit the input bundle locally.
8. Send OwnershipActivated(target_token); do not wait.
9. Transfer/stage the snapshot whenever it becomes ready.
10. Target applies only with matching activation and unchanged baseline.
11. ApplyResult updates telemetry only.
```

### 8.2 Remote Host to Server Host

```text
1. Remote enters the server-return barrier.
2. Remote actor starts snapshot under its current OwnershipToken.
3. Remote sends SnapshotOffer when ready; it does not wait.
4. Remote sends the existing ReleaseRequest immediately.
5. Server emits PeerReleaseStarted and prepares its local clipboard baseline.
6. Server releases local capture immediately.
7. ClientReleased commits input owner to SERVER_HOST and allocates/activates target token.
8. Authority associates only the matching old-owner SnapshotOffer with this return.
9. Local server actor applies only if preparation and generation checks pass.
10. Existing TV/controller cleanup proceeds independently.
```

If the remote is dead or its snapshot never arrives, steps 5 through 10 reduce to a skipped clipboard handoff while input still returns to the server.

### 8.3 Remote Host to Another Remote Host

The authority remains the hub:

```text
source remote -> authority -> target remote
```

There is no peer-to-peer clipboard trust or direct target write. The same source and target token checks apply.

## 9. Wire Protocol

### 9.1 Transport

- TCP with TLS 1.3
- same configured numeric port as Lan Mouse DTLS is allowed because TCP and UDP port spaces are independent
- mutual certificate authentication
- reuse the existing certificate and authorized fingerprint policy
- one authority-initiated full-duplex connection per peer
- asynchronous reconnect with bounded backoff
- input DTLS remains usable while this channel is absent

### 9.2 Framing

Use fixed binary headers and network byte order. Do not use JSON or base64 for payload data.

```text
FramePrefix {
    magic: [u8; 4] = "LMCB",
    protocol_version: u16,
    message_type: u16,
    flags: u32,
    header_length: u32,
    payload_length: u64,
}
```

Validation order is mandatory:

1. Read the fixed prefix into a fixed buffer.
2. Validate magic, version, message type, flags, and exact header length.
3. Reject `payload_length > negotiated_max_bytes` before allocation.
4. Allocate at most the negotiated payload size.
5. Read exactly the declared bytes with a transfer deadline and cancellation.
6. Validate SHA-256 and UTF-8 before publishing the snapshot.

Any malformed frame closes only this peer's clipboard TLS connection.

### 9.3 Negotiation

```text
ClipboardHello {
    host_id,
    process_session_id,
    protocol_version,
    offered_capabilities,
    max_receive_bytes,
}
```

V1 capability:

```text
clipboard_text_v1
```

Effective maximum:

```text
min(local_configured_max, peer_advertised_max)
```

A peer without `clipboard_text_v1` remains fully usable for input.

### 9.4 Messages

```text
AuthorityState
PrepareTarget
PrepareResult
OwnershipActivated
SnapshotOffer
SnapshotDeliver
ApplyResult
CancelHandoff
ProtocolError
```

`ApplyResult` and `PrepareResult` are diagnostic. Neither is an input acknowledgement or gate.

### 9.5 Payload Metadata

```text
ClipboardPayload {
    handoff_id,
    source_token,
    target_token,
    snapshot_id,
    kind: Text | Empty,
    payload_length,
    sha256,
    bytes,
}
```

Text is UTF-8 with LF line endings. `Empty` has zero bytes. Compression is not used in V1.

## 10. Bounded Work and Storage

### 10.1 Payload Limit

The proposed default is 3 MiB because it is large enough for normal text and matches the mature Deskflow default observed during research. It is a configuration default, not a protocol constant.

Every layer enforces the effective limit:

- native read before or during accumulation
- source snapshot object
- frame prefix validation
- receiver allocation
- target stage

### 10.2 Queue Limits

- one coordinator handoff
- one source snapshot for that handoff
- one staged target snapshot
- one pending actor command per replaceable command class
- bounded transport control queue
- at most one payload send per peer

Newer handoff identities supersede older replaceable work. Input events never share these queues.

### 10.3 No Persistence

Payload bytes, hashes, and text-derived metadata are memory-only. They are never written to configuration, cache files, crash reports, notifications, or logs.

## 11. Native Backend Contract

### 11.1 Commands

```text
ObserveGeneration
PrepareTarget(handoff_id, target_token)
CaptureSource(handoff_id, source_token, max_bytes)
ActivateTarget(handoff_id, target_token)
Apply(handoff_id, target_token, snapshot_id, baseline, payload)
Cancel(handoff_id)
Shutdown
```

All commands are idempotent by identity. `Cancel` and `Shutdown` do not wait for an operating-system clipboard lock held by another process.

### 11.2 Results

```text
Prepared(baseline_generation)
Captured(snapshot)
Applied(post_write_generation)
Skipped(reason)
BackendUnavailable(reason)
```

No result contains clipboard text in its `Debug`, `Display`, error, tracing, or notification representation.

### 11.3 Actor Rules

- one actor owns all native clipboard access in a process
- no native handle crosses actor threads unless the platform API explicitly permits it
- no OS clipboard lock is held across network I/O or an async await
- actor requests have deadlines and cancellation identities
- actor queue saturation returns a skip result
- backend restart creates a new process/backend session and invalidates old generations

## 12. Platform Design

### 12.1 Windows

- Run in the interactive logged-in user's window station, not Session 0.
- Use one dedicated clipboard thread with a message-only or hidden window.
- Subscribe with `AddClipboardFormatListener` and handle `WM_CLIPBOARDUPDATE`.
- Use `GetClipboardSequenceNumber` as the native generation.
- Serialize `OpenClipboard`; retry only within a short actor-local budget.
- Read `CF_UNICODETEXT` and validate `GlobalSize` before copying.
- Write `CF_UNICODETEXT` and retain ownership according to Win32 rules.
- Do not export content carrying `ExcludeClipboardContentFromMonitorProcessing`.
- Treat lock contention as `Unavailable`, not `Empty`.

The existing scheduled task already runs Lan Mouse in the user session. A Windows service migration would require a separate user-session clipboard agent.

### 12.2 macOS

- Use a dedicated AppKit-compatible actor/run loop.
- Use `NSPasteboard.general` and `changeCount` as the native generation.
- Inspect data length before copying into the transport snapshot.
- Recognize concealed/transient community marker types where available.
- Package the non-GTK process with a stable bundle identifier and `LSUIElement` identity so pasteboard privacy authorization is stable.
- Surface persistent permission denial as backend-unavailable status.

The actor must tolerate the OS denying programmatic reads. Denial skips clipboard only.

### 12.3 Wayland

Backend selection order:

1. `ext-data-control-v1`
2. `wlr-data-control-unstable-v1` for compositors such as the current Hyprland deployment
3. XDG Clipboard portal only when a compatible RemoteDesktop/InputCapture session requested clipboard access before session start
4. unavailable

Requirements:

- maintain a generation counter from selection-owner/change events
- read from the offered file descriptor with `take(max_bytes + 1)` or equivalent bounded accumulation
- reject oversize before retaining the full payload
- keep the actor alive to serve data after setting the clipboard
- ignore the actor's own selection notification using applied identity/generation

XWayland is not a generic fallback for the Wayland-native clipboard.

### 12.4 X11

- Use the `CLIPBOARD` selection only.
- Track selection-owner changes as native generation changes.
- Read UTF-8 text with a deadline.
- Support `INCR` transfer with a hard cumulative byte limit.
- Continue serving `SelectionRequest` while this process owns applied data.
- Treat owner disappearance, timeout, malformed properties, and unsupported targets as unavailable.

### 12.5 Why `arboard` Is Not the Backend Contract

`arboard` is useful reference code and may be usable for selected writes. It is not the V1 abstraction because the checked Wayland path may read to completion before an application-level cap can reject the payload, and the design needs platform generation, private-format inspection, target preparation, and persistent ownership semantics.

## 13. Race and Failure Analysis

### 13.1 Source Changes During Read

Detection: generation before read differs from generation after read.

Result: retry only while the same source ownership token remains current; otherwise skip. Never send a mixed snapshot.

### 13.2 Destination Changes After Preparation

Detection: current target generation differs from the prepared baseline.

Result: drop the stage. The local target value wins.

### 13.3 Input Commits Before Target Preparation

Result: input commits normally; target refuses late preparation and clipboard skips.

### 13.4 Snapshot Arrives Before Input Commit

Result: target may stage under the prepared target token but cannot apply until `OwnershipActivated` matches.

### 13.5 Snapshot Arrives After Input Commit

Result: apply is allowed only while the target token is still current and the target baseline is unchanged.

### 13.6 Switch Aborts After Stage

Result: `CancelHandoff` invalidates the stage. Target token never becomes current, so apply remains disabled even if cancellation is lost.

### 13.7 Rapid A -> B -> A or A -> B -> C

Result: every begin allocates non-reused identities and cancels replaceable work. Delayed old data fails target-token or handoff checks.

### 13.8 Authority Restart

Result: new authority session and server-host input fallback invalidate every old message. Clipboard channel reconnect does not replay payloads.

### 13.9 Peer Restart

Result: process-session mismatch invalidates target preparation, cached snapshot, activation, and apply work for that peer.

### 13.10 Duplicate Delivery

Result: `(HandoffId, SnapshotId)` is applied at most once. A duplicate is acknowledged as duplicate or dropped without a second native write.

### 13.11 Malformed or Oversized Frame

Result: reject before unbounded allocation, close the clipboard TLS connection, keep DTLS input and ownership state unchanged.

### 13.12 Private or Unsupported Clipboard

Result: source returns a skip reason. No placeholder or empty clipboard is sent.

### 13.13 Channel Loss

Result: queued payload is dropped, channel reconnects independently, input continues, and no payload is replayed after a newer handoff.

### 13.14 Native Backend Failure

Result: mark clipboard backend unavailable and skip affected handoffs. Do not alter peer input readiness.

### 13.15 Empty Versus Failure

```text
explicit Empty -> target may clear after all fencing checks
Unavailable    -> target remains unchanged
```

No error path synthesizes `Empty`.

## 14. Failure Matrix

| Failure | Clipboard result | Input/display result |
|---|---|---|
| Capability absent | Skip | Unchanged |
| Source backend unavailable | Skip | Unchanged |
| Source changes during bounded retries | Skip | Unchanged |
| Source content private | Skip | Unchanged |
| Source content oversized | Skip | Unchanged |
| Target preparation unavailable/late | Skip | Unchanged |
| TLS unavailable | Skip | Unchanged |
| TLS disconnect mid-payload | Drop partial payload | Unchanged |
| Invalid length/hash/UTF-8 | Reject and close clipboard channel | Unchanged |
| Target changed after preparation | Preserve target local clipboard | Unchanged |
| Target backend unavailable | Preserve target local clipboard | Unchanged |
| Switch abort/supersession | Cancel/drop | Existing input recovery |
| Authority restart | Reject old sessions | Server-host fallback |
| Peer restart | Reject old peer session | Existing readiness handling |
| Duplicate snapshot | No second write | Unchanged |
| Apply-result loss | No retry after applied identity | Unchanged |

## 15. Safety Invariants

### C1. InputOwnershipAtomic

```text
keyboard_owner = pointer_owner = input_owner
```

Clipboard actions cannot change any of these variables.

### C2. InputIndependence

```text
ClipboardNext => UNCHANGED
    input_owner, keyboard_owner, pointer_owner,
    capture, cursor, TV state, bundle lease
```

Input commit and server fallback have no clipboard guard.

### C3. ActiveSourceOnly

```text
BeginHandoff(source, target)
  => source = current input_owner
  AND source_token = current ownership_token
```

Later completion of a snapshot is authorized by this recorded source token, not by the source's ownership at completion time.

### C4. PreparedBeforeActivation

```text
ApplySnapshot
  => target preparation was recorded before target activation
```

### C5. NoStaleApply

```text
ApplySnapshot
  => target_token = current ownership_token
  AND target_host = current input_owner
  AND authority/process/handoff sessions match
```

### C6. DestinationPreservation

```text
target_generation != prepared_baseline
  => incoming snapshot is not written
```

All failure actions leave native target content unchanged.

### C7. AtMostOnceApply

```text
Applied(handoff_id, snapshot_id)
  => never NativeWrite(handoff_id, snapshot_id) again
```

### C8. BoundedMemory

Every ready, in-flight, or staged payload is at most the negotiated maximum, and every queue has a fixed bound.

### C9. NoPrivateExport

A snapshot marked private/concealed is never represented as `SnapshotOffer` or `SnapshotDeliver`.

### C10. NoFailureClear

Only the explicit `Empty` data variant may clear a target clipboard.

### C11. SessionFreshness

Authority restart and peer process restart invalidate old target preparation, snapshot, and apply identities.

### C12. OneActiveHandoff

At most one non-terminal handoff, one source payload, and one target stage exist under one authority.

## 16. Liveness and Fairness Boundary

Clipboard liveness is conditional:

```text
stable source backend
AND stable target backend
AND stable authenticated channel
AND no target-local change after preparation
AND committed target token remains current
=> eventually Applied or explicitly Skipped
```

Unconditional liveness belongs to input fallback, not clipboard. A permanently denied macOS pasteboard, absent Wayland protocol, sleeping peer, or disconnected channel must settle clipboard to `Skipped`; it must not hold an active internal phase forever.

Internal actor outcomes, transfer completion/failure, stage apply/drop, and cancellation processing require fairness. Environment availability does not.

## 17. Performance and Deadlock Constraints

### 17.1 Input Hot Path

- No clipboard read, hash, allocation, TLS write, lock, or actor acknowledgement in pointer/keyboard forwarding.
- Handoff hooks enqueue fixed-size control records with non-blocking `try_send` semantics.
- Queue-full means clipboard skip.

### 17.2 CPU

- Windows, Wayland, and X11 use event-driven generation updates.
- macOS may poll `changeCount` at a low rate only while its ownership token is active; direct transition snapshot remains the correctness path.
- SHA-256 runs once per accepted source snapshot and once per received snapshot.
- No V1 compression.

### 17.3 Memory

At most one source and one target payload are retained. Implementations should share immutable payload bytes rather than copy between coordinator and transport queues.

### 17.4 Lock and Await Order

Forbidden:

```text
native clipboard lock -> await network
network write lock -> await native actor
service state borrow -> await native actor or transport
input capture release -> await clipboard completion
```

Allowed flow:

```text
service emits identity-only command
actor/transport performs bounded work independently
service validates identity-only completion event
```

### 17.5 Convergent Actor States

Every command reaches one of:

```text
Completed
Skipped(reason)
Canceled
BackendUnavailable(reason)
```

No error leaves a native lock held, a payload queue permanently reserved, or a handoff waiting for an acknowledgement that input does not need.

## 18. Security and Privacy

- Authenticate peers with the existing certificate fingerprint allowlist.
- Bind host identity to the authenticated connection, never to message text alone.
- Do not accept source host IDs that differ from the connection identity.
- Do not transfer recognized private/concealed clipboard formats.
- Do not use text-pattern secret detection; it is unreliable and would inspect user content unnecessarily.
- Do not log payloads, hashes, previews, MIME bodies, or transformed text.
- Enforce negotiated size before allocation and cumulative size during native/stream reads.
- Use TLS deadlines and close malformed clipboard connections.
- Do not persist or replay payloads after process restart.
- A clipboard protocol error cannot revoke or grant input ownership.

## 19. Observability and Notification

### 19.1 Structured Events

```text
clipboard_backend_ready
clipboard_backend_unavailable
clipboard_handoff_started
clipboard_target_prepared
clipboard_snapshot_captured
clipboard_snapshot_skipped
clipboard_transfer_started
clipboard_transfer_completed
clipboard_transfer_rejected
clipboard_apply_completed
clipboard_apply_skipped
clipboard_handoff_canceled
```

Fields:

```text
source_host
target_host
authority_session_short
handoff_epoch
ownership_epoch
snapshot_sequence
bytes
duration_ms
reason
```

Never log payload, digest, clipboard type contents, or source application.

### 19.2 Stable Reason Codes

```text
capability_missing
backend_unavailable
permission_denied
private_content
unsupported_format
oversize
source_changed
target_not_prepared
destination_changed
stale_authority_session
stale_peer_session
stale_handoff
stale_owner_token
duplicate
channel_unavailable
transfer_timeout
protocol_error
integrity_failed
invalid_utf8
canceled
queue_full
```

### 19.3 Notifications

Do not notify for each transient handoff failure. Notify only persistent actionable backend states, such as macOS permission denial or no usable Linux clipboard protocol. A notification states that input switching still works and identifies the platform action required.

## 20. Proposed Configuration

This configuration does not exist yet; it is the implementation contract:

```toml
[clipboard]
enabled = true
max_bytes = 3145728
```

No arbitrary retry, polling, or timeout knobs are initially exposed. Backend-local budgets use implementation constants justified by native API behavior and are observable through reason codes.

Disabling clipboard cancels clipboard handoffs only and leaves all input clients active.

Runtime enable/disable is a clipboard-only state transition. Disabling an
active handoff atomically marks it canceled, clears in-flight and staged
payload ownership, and preserves input ownership, readiness, native clipboard
values, and native generations. Enabling starts no handoff until the next real
input-owner transition. Delayed retired frames remain fenced and cannot be
delivered while clipboard is disabled.

## 21. Current-Code Refinement Map

### 21.1 New Modules

Proposed workspace crate:

```text
lan-mouse-clipboard/
  src/lib.rs
  src/actor.rs
  src/types.rs
  src/backend/windows.rs
  src/backend/macos.rs
  src/backend/wayland.rs
  src/backend/x11.rs
```

Main-process modules:

```text
src/clipboard.rs             coordinator and input-owner refinement
src/clipboard_transport.rs   TCP/TLS peer sessions and binary framing
```

Do not put payload framing into `lan-mouse-proto`; that crate remains the fixed-size input/control event protocol.

### 21.2 `Service`

Add fixed-size coordinator state and event receivers to `Service`. Extend the `select!` loop with clipboard control completions only. Payload movement remains outside the service loop.

Hooks:

```text
handle_capture_candidate after successful reserve
  -> clipboard.begin_handoff(server, target)

authorize_client_enter after BundleLeaseManager::commit
  -> clipboard.activate_target(target_token)

fail_context / release_gate / config change / shutdown
  -> clipboard.cancel_handoff(identity)

PeerReleaseStarted
  -> clipboard.begin_return_handoff(remote, server)

ClientReleased for peer return
  -> clipboard.activate_server(target_token)
```

### 21.3 `Capture`

Add an identity-only `ICaptureEvent::PeerReleaseStarted` before local capture release on a valid `ReleaseRequest`. Sending this event must not await clipboard work.

The remote EnterOnly `CaptureBegin` path asks its local clipboard coordinator to start a source snapshot before it sends `ReleaseRequest`; it still sends `ReleaseRequest` immediately.

### 21.4 `BundleLeaseManager`

Do not add clipboard states or guards to `BundleLeaseManager`. The clipboard coordinator observes successful reserve/commit/invalidate transitions and keeps separate identities.

### 21.5 Peer Connection State

Associate a clipboard TLS peer with the same authenticated fingerprint and configured client handle used by input. A clipboard process-session change invalidates only clipboard state; existing input readiness behavior remains independent.

### 21.6 Native Async Interface

Use Rust native async traits where an async trait boundary is required. Do not introduce the `async-trait` crate. Platform event-loop actors may instead expose bounded channels, which is preferred when native APIs are thread-affine.

## 22. Verification Plan

### 22.1 TLA+/TLC

The finite model must explore:

- input commit before/after target preparation
- snapshot before/after input commit
- source generation change during read
- target generation change before apply
- switch abort before every clipboard phase
- rapid supersession and non-reused epochs
- delayed stale message
- authority restart with old wire data
- peer/backend/channel failure
- private, oversized, malformed, and explicit-empty outcomes
- duplicate delivery
- remote-to-server fallback with clipboard unavailable

Checked safety properties:

- type correctness
- atomic input ownership
- input commit/fallback enabled independently of clipboard
- only the current source token starts a handoff
- stage and apply require fresh identities
- target preparation precedes activation
- destination changes prevent apply
- failure actions preserve native values
- payloads remain bounded and non-private
- at-most-once apply

Checked liveness properties:

- an input transition eventually commits or aborts under internal fairness
- every active clipboard handoff eventually applies, skips, or cancels under internal fairness

### 22.2 Rust Domain Tests

- allocate non-reused handoff/owner epochs across abort
- reject old authority and peer process sessions
- reject source host differing from authenticated peer
- reject inactive source token
- reject late prepare after activation
- reject target baseline mismatch
- duplicate apply is idempotent
- explicit empty clears; unavailable does not
- newer handoff supersedes every replaceable queue item
- clipboard queue full does not change input state

### 22.3 Framing and Transport Tests

- fragmented prefix/header/payload reads
- partial writes and EOF at each byte boundary
- declared length over limit rejected before allocation
- cumulative native/stream limit
- invalid magic/version/type/flags/header length
- hash mismatch and invalid UTF-8
- connection loss during payload
- authenticated host mismatch
- reconnect with changed process session
- malformed clipboard connection leaves DTLS input operational

### 22.4 Fake Backend Integration Tests

- source changes at each capture point
- target changes before preparation, after preparation, and after staging
- input commits with actor blocked forever
- fallback commits with channel blocked forever
- abort and restart while every actor command is outstanding
- applied self-notification does not republish
- actor shutdown releases platform resources

### 22.5 Native Tests

Run on each native OS only after domain/transport tests pass:

- text and explicit empty
- process restart
- lock/contention or permission denial
- target local-copy race
- source change race
- payload at limit and one byte over
- service runs in the intended interactive user session
- actor remains alive and serves clipboard data after apply

## 23. Implementation Order

1. Add domain identities, state machine, fake native actor, and tests.
2. Add binary clipboard protocol and in-memory/fault-injected transport tests.
3. Add coordinator hooks without native backends; prove input tests remain unchanged.
4. Add Linux native actor for the current server environment.
5. Add Windows native actor and interactive-session verification.
6. Add macOS native actor, stable application identity, and permission status.
7. Add Ansible build/deployment inputs only after each native binary builds on its native OS.
8. Deploy normally and diagnose real failures from structured reason codes.

There is no legacy clipboard protocol and no compatibility shim. A peer without the optional capability simply has no clipboard handoff while input remains available.

## 24. Architecture Decisions

### ADR-CB-001: Clipboard follows ownership transitions

Decision: synchronize once per committed owner transition, not continuously.

Rationale: inactive hosts cannot overwrite the active owner, and the behavior matches copy-switch-paste.

### ADR-CB-002: Clipboard never gates input

Decision: all clipboard work is best effort and asynchronous to input reserve, commit, release, and fallback.

Rationale: clipboard convenience cannot weaken the core availability invariant.

### ADR-CB-003: Separate TCP/TLS transport

Decision: keep bulk clipboard framing outside fixed-size input DTLS.

Rationale: reliability, framing, allocation, and failure isolation differ from low-latency input.

### ADR-CB-004: Authority-issued source and target tokens

Decision: accept snapshots only from the recorded prior owner and apply only under the newly active target token.

Rationale: one token is insufficient to prove both authorized origin and fresh destination.

### ADR-CB-005: Prepare target before activation

Decision: record target generation before input activation or skip.

Rationale: a baseline recorded on payload arrival cannot detect a local copy made while the payload was delayed.

### ADR-CB-006: Platform actors, not one generic clipboard object

Decision: isolate Windows, macOS, Wayland, and X11 ownership/version semantics behind serialized actors.

Rationale: platform event loops, privacy, lazy ownership, and bounded reads are materially different.

### ADR-CB-007: UTF-8 text only in V1

Decision: support `Text` and explicit `Empty` only.

Rationale: it proves the protocol and races without multiplying format, conversion, memory, and privacy states.

### ADR-CB-008: No payload persistence or content logs

Decision: payloads are memory-only and observability contains identity/status metadata only.

Rationale: clipboard content is sensitive and replay after restart is semantically stale.

## 25. TLC Verification Record

This section records actual TLC output from 2026-07-14. The checker was:

```text
TLC2 Version 2.19 of 08 August 2024 (rev: 5a47802)
/home/example/.cache/nvim/tla.nvim/tla2tools.jar
```

Model artifacts:

```text
../tla/ClipboardHandoff.tla
../tla/ClipboardHandoff.cfg
../tla/ClipboardHandoff-capability-disabled.cfg
../tla/ClipboardHandoff-liveness.cfg
```

The accepted runs used `-XX:+UseParallelGC`, four workers, and separate `/tmp` metadata directories. The command form was:

```text
java -XX:+UseParallelGC \
  -cp /home/example/.cache/nvim/tla.nvim/tla2tools.jar \
  tlc2.TLC -workers 4 -config <profile>.cfg \
  -metadir <temporary-directory> ClipboardHandoff.tla
```

### 25.1 Deep Safety Profile

Configuration: `ClipboardHandoff.cfg`

```text
SPECIFICATION SafetySpec
CONSTRAINT TLCDeepState
MAX_EPOCH = 3
MAX_GENERATION = 1
MAX_RETRIES = 1
MAX_PAYLOAD = 1
MAX_SESSION = 1
```

`TLCDeepState` is a safety-only finite profile. It keeps each relevant failure independently reachable while excluding irrelevant simultaneous environment products:

- source or target native generation change
- source or target backend loss
- authority restart or peer restart
- channel up or down
- three allocated epochs for abort, retry, remote commit, and fallback traces

No liveness property is claimed from this constrained graph.

Result:

```text
Model checking completed. No error has been found.
32,357,094 states generated
1,525,938 distinct states found
0 states left on queue
complete graph depth 30
finished in 40 seconds
```

All configured state invariants passed, including atomic input ownership, monotonic/non-reused finite identities, source fencing, preparation-before-activation, bounded/non-private payloads, stage identity, at-most-once staging, recorded apply identity, and clipboard-independent input commit/fallback enabledness.

### 25.2 Capability-Disabled Safety Profile

Configuration: `ClipboardHandoff-capability-disabled.cfg`

This profile fixes `clipboard_enabled = FALSE` while leaving input transitions available.

Result:

```text
Model checking completed. No error has been found.
467 states generated
16 distinct states found
0 states left on queue
complete graph depth 7
finished in under one second
```

This proves in the finite abstraction that absence of `clipboard_text_v1` skips clipboard work without disabling remote commit or return-to-server begin.

### 25.3 Unconstrained Liveness and Action-Isolation Profile

Configuration: `ClipboardHandoff-liveness.cfg`

```text
SPECIFICATION Spec
no state/action CONSTRAINT
MAX_EPOCH = 2
MAX_GENERATION = 1
MAX_RETRIES = 1
MAX_PAYLOAD = 1
MAX_SESSION = 0
```

Two ownership epochs cover one server-to-remote commit and one remote-to-server fallback. Restart is omitted from this temporal profile because restart fencing is a safety property covered by the deep-safety profile. Omitting it also avoids using a state constraint in liveness checking.

Checked temporal properties:

```text
InputIndependence
FailurePreservesNativeClipboard
ConfigurationPreservesNativeClipboard
EventuallySwitchSettles
EventuallyClipboardSettles
```

Result:

```text
Model checking completed. No error has been found.
70,107,300 states generated
7,433,184 distinct states found
0 states left on queue
complete graph depth 25
finished in 14 minutes 20 seconds
```

The graph includes both clipboard capability values, both channel states, independent source/target backend availability, bounded source/target native changes, explicit empty/text/oversized/private inputs, delayed retired delivery, corruption, commit/abort ordering, staging, application, and fallback.

### 25.4 Checker-Driven Corrections

The first checker passes found specification-definition problems before state exploration:

- Enabledness invariants referenced actions before those actions were declared. They were moved below the action definitions.
- Existential binders in `ClipboardNext` and `InputNext` were not delimited, so binder names leaked into later disjuncts. Each quantified action was parenthesized.
- Action-isolation properties were initially written as raw boxed action implications. They were rewritten as legal square-action properties.
- A combined constrained liveness run emitted TLC's state-constraint liveness warning. Safety and liveness were split, and the accepted liveness profile has no state or action constraint.
- Runtime clipboard enable/disable was initially only an implementation-plan
  obligation. `SetClipboardEnabled` now cancels active clipboard work while
  leaving input and native clipboard variables unchanged;
  `ClipboardDisabledSettled` and
  `ConfigurationPreservesNativeClipboard` check those requirements.

No behavioral counterexample or invariant violation remained after those corrections. Interrupted exploratory runs are not counted as verification evidence above.
