# Clipboard Handoff Implementation Plan

## Plan Control

- Status: in progress. P0 through P2 completed on 2026-07-14; P3 is the next
  action.
- Plan type: scoped child implementation plan. The requested filename is
  `docs/clipboardplan.md`; this is not a replacement root objective and
  therefore does not supersede
  `osswitch/docs/plan_main_fullscreen_multiview_switch_implementation.md`.
- Normative design:
  `osswitch/docs/clipboarddesgin.md`.
- Formal model:
  `osswitch/tla/ClipboardHandoff.tla` and its three checked configurations.
- Implementation repository: `lan-mouse/`.
- Deployment repository: `osswitch/lan-mouse-deploy/`.
- Starting Lan Mouse revision:
  `7cc0f680768dc9b3ce479e0fb19d486c65ceb9a9`.
- Scope: UTF-8 text and explicit empty clipboard handoff on committed input
  ownership transitions. Keyboard and pointer remain one atomic bundle.
- Compatibility policy: there is no legacy clipboard wire protocol, mixed
  protocol fallback, Deskflow/Synergy adapter, or compatibility shim. A peer
  that does not negotiate `clipboard_text_v1` keeps normal input switching but
  receives no clipboard handoff.
- Delivery policy: implementation commits are made only after their phase
  checks pass. Deployment happens only after Linux, Windows, and macOS native
  builds pass for one exact revision.

The design and TLA+ model are authoritative over this plan. If implementation
work reveals that an action cannot refine the model, stop that phase, update
the design and model first, run TLC again, and then update this living plan by
editing only the affected sections.

## Objective

Implement transition-bound clipboard handoff in Lan Mouse without adding any
clipboard dependency to input readiness, ownership commit, server-host
fallback, pointer forwarding, or keyboard forwarding.

The completed system must satisfy:

```tla
GoalState ==
    /\ ClipboardTextV1Implemented
    /\ InputOwnershipAtomic
    /\ InputIndependence
    /\ ActiveSourceOnly
    /\ PreparedBeforeActivation
    /\ NoStaleApply
    /\ DestinationPreservation
    /\ AtMostOnceApply
    /\ BoundedMemory
    /\ NoPrivateExport
    /\ NoFailureClear
    /\ SessionFreshness
    /\ OneActiveHandoff
    /\ NativeLinuxBuildVerified
    /\ NativeWindowsBuildVerified
    /\ NativeMacosBuildVerified
    /\ ExactRevisionDeployed
```

User-visible success means:

1. Copy supported text on the current input owner.
2. Commit an ordinary Lan Mouse ownership switch.
3. Paste the captured value on the new owner if every clipboard fence remains
   valid.
4. Preserve the target clipboard when capture, preparation, transport,
   validation, application, or any race fails.
5. Preserve normal input switching under every clipboard failure.

## Non-Goals

- Continuous or background clipboard synchronization
- Clipboard publication by an inactive host
- Images, files, HTML, RTF, arbitrary MIME types, or X11 `PRIMARY`
- Clipboard history or persistence
- Retrying input ownership because clipboard failed
- Adding clipboard payloads to `lan-mouse-proto::ProtoEvent`
- Supporting old experimental clipboard formats or mixed clipboard versions
- Restarting a host or restarting `tv-multiview` for a clipboard-only deploy

## TLA Planning Frame

### InitState

```tla
InitState ==
    /\ LanMouseRevision = "7cc0f68"
    /\ ServiceUsesOneTokioSelectLoop
    /\ BundleLeaseManagerOwnsInputGateOnly
    /\ ProtoEventMaximumBytes = 21
    /\ NoClipboardWorkspaceCrate
    /\ NoClipboardCoordinator
    /\ NoClipboardTransport
    /\ NoPeerReleaseStartedEvent
    /\ NoNativeClipboardActors
    /\ NoClipboardConfiguration
```

Current evidence:

- `lan-mouse/Cargo.toml` has no clipboard workspace member or clipboard
  configuration dependency.
- `lan-mouse/src/service.rs` owns `BundleLeaseManager` and processes capture,
  switch, emulation, and frontend events in one service loop.
- `lan-mouse/src/switch.rs` has only `Local`, `Preparing`, `GrantArmed`, and
  `RemoteOwned` bundle-gate states. Clipboard state is absent and must remain
  absent from that manager.
- `lan-mouse/src/capture.rs` exposes `CaptureBegin`, `CaptureCandidate`,
  `CommitRequested`, and `ClientReleased`, but not the required
  identity-only `PeerReleaseStarted` event.
- `lan-mouse/src/service.rs` currently sends `ReleaseRequest` directly from
  incoming `CaptureBegin` handling.
- `lan-mouse-proto` remains the fixed-size DTLS input/control protocol.
- The existing certificate, fingerprint authorization, configured host IDs,
  and native build pipeline are reusable inputs, but they do not currently
  create a TCP/TLS clipboard channel.

### Ordered Actions

```tla
Plan ==
    <<P0_RefinementAndBaselineGate,
      P1_DomainStateAndActorContract,
      P2_FramingAuthenticationAndTransport,
      P3_CoordinatorAndInputTransitionHooks,
      P4_LinuxNativeActor,
      P5_WindowsNativeActor,
      P6_MacOSNativeActor,
      P7_CrossPlatformHardening,
      P8_NativeBuildAndDeployment,
      P9_FinalRefinementAndAcceptance>>
```

No phase may treat a later native backend, deployment script, or manual test as
evidence that an earlier domain or transport gate passed.

## Required Invariants

These are implementation acceptance criteria, not comments.

1. **InputOwnershipAtomic**: clipboard code cannot split keyboard, pointer, or
   input ownership and cannot mutate capture, cursor, TV, grant, or bundle
   lease state.
2. **InputIndependence**: input commit and server-host fallback contain no
   clipboard guard, await, lock acquisition, queue reservation, or success
   condition.
3. **ActiveSourceOnly**: only the host and ownership token that were current
   when the authority began a real handoff may originate its snapshot.
4. **PreparedBeforeActivation**: a trustworthy target native generation is
   recorded before that target ownership token becomes active. A preparation
   still pending at input commit is not retroactively valid.
5. **NoStaleApply**: apply requires matching authority session, source and
   target process sessions, handoff ID, source token, target token, current
   owner host, and authenticated peer identity.
6. **DestinationPreservation**: a target generation change after preparation
   drops the stage and leaves the target clipboard unchanged.
7. **AtMostOnceApply**: one `(HandoffId, SnapshotId)` can cause at most one
   native write, even after duplicate delivery or apply-result loss.
8. **BoundedMemory**: there is at most one active handoff, one source payload,
   one in-flight payload per peer, and one target stage. Every size is checked
   before allocation.
9. **NoPrivateExport**: private, concealed, unsupported, or unavailable data
   cannot become `SnapshotOffer` or `SnapshotDeliver`.
10. **NoFailureClear**: only the explicit `Empty` data variant can clear a
    target. Errors never map to empty data.
11. **SessionFreshness**: authority or peer process restart invalidates all
    prior preparation, payload, stage, and apply identities.
12. **OneActiveHandoff**: a newer handoff consumes new epochs and supersedes
    every replaceable item from the old handoff.
13. **NoAwaitCycle**: no service-state borrow, native clipboard lock, transport
    writer lock, capture release, or input lock is held while awaiting another
    subsystem.
14. **PayloadPrivacy**: logs, notifications, status, crash context, and build
    artifacts never contain clipboard bytes, digest, source application, or
    content-derived metadata.

### Checker Correspondence

The design names above are broader production obligations. Their current
finite-model evidence is:

| Design obligation | Current TLA+/TLC evidence | Required Rust evidence beyond the model |
|---|---|---|
| C1 InputOwnershipAtomic | `InputOwnershipAtomic` | Existing input transition tests remain green under clipboard failure |
| C2 InputIndependence | `InputIndependence`, `CommitEnabledWithoutClipboard`, `FallbackBeginEnabledWithoutClipboard` | Permanently blocked actor/transport integration tests |
| C3 ActiveSourceOnly | `PendingSwitchWellFormed`, `ActiveHandoffFenced`, `BeginSwitch` guards | Authenticated host/token rejection tests |
| C4 PreparedBeforeActivation | `PreparedBeforeActivated` | Native completion timestamp/order and late-prepare tests |
| C5 NoStaleApply | `ActiveHandoffFenced`, `StageIdentityBound`, `AppliedIdentityRecorded` | Full random session and peer fingerprint checks |
| C6 DestinationPreservation | `ApplySnapshot` generation guard, `FailurePreservesNativeClipboard` | Native target-local-copy race tests |
| C7 AtMostOnceApply | `AtMostOnceStaging`, `AppliedIdentityRecorded` | Apply-result loss and duplicate delivery tests |
| C8 BoundedMemory | `PayloadBounded` and finite single handoff/wire/stage records | Real byte limits, queue formulas, and allocation tests |
| C9 NoPrivateExport | `NoPrivatePayload` | Platform privacy-marker tests |
| C10 NoFailureClear | `FailurePreservesNativeClipboard`, `SnapshotKindMatches` | Explicit `Empty` versus every error variant |
| C11 SessionFreshness | restart actions plus active/stage fencing invariants | Random process-session and delayed real-frame tests |
| C12 OneActiveHandoff | Singular `handoff`, `wire`, `stage`, and `retired` state | Multi-peer supersession and payload-slot tests |

Liveness acceptance maps to `EventuallySwitchSettles` and
`EventuallyClipboardSettles` under the model's internal fairness assumptions.
Environment availability remains outside that guarantee; a permanently absent
backend/channel must produce a terminal skip rather than keep an internal
operation active forever.

## State Ownership

| State | Sole owner | May contain payload bytes | Input-path access |
|---|---|---:|---|
| Input bundle lease and grant | Existing `BundleLeaseManager` | No | Existing behavior only |
| Authority session, owner token, handoff metadata | Clipboard coordinator | No | Fixed-size notice after transition |
| Native generation and platform handles | Native clipboard actor | Only its one snapshot/stage | Non-blocking command only |
| TLS peer session and negotiated capability | Clipboard transport task | One bounded `Arc<[u8]>` | None |
| Applied identity | Target native actor | No | None |
| Clipboard status/reason | Coordinator/actor status slot | No | None |

The coordinator receives immutable input-transition facts. It is not passed a
mutable `BundleLeaseManager`, `Capture`, `Emulation`, or controller reference.
This type boundary makes the model's `ClipboardNext => UNCHANGED inputVars`
property enforceable in Rust.

## Model-to-Code Refinement Ledger

| TLA+ action | Rust refinement point | Required rejection/failure behavior |
|---|---|---|
| `Init` | Process startup creates random process session; authority creates random authority session and server owner token | Clipboard initialization failure reports unavailable and leaves input startup unchanged |
| `BeginRemote` | `handle_capture_candidate` after successful bundle reservation | `try_send` failure records `queue_full`; controller preparation still continues |
| `BeginFallback` | Authority handles `PeerReleaseStarted` for the current remote owner | Release continues immediately; stale peer/token is ignored |
| `PrepareTarget` | Target actor returns a baseline for the exact target token before activation | Late or mismatched completion is discarded |
| `PrepareFailure` | Backend/channel/session cannot prepare | Set terminal `Skipped(reason)` only |
| `CaptureSuccess` | Source actor returns stable `Text` or explicit `Empty` | Payload remains in actor/transport ownership |
| `RetrySourceChanged` | Source actor observes generation change while the source token is still current | Retry only within the fixed, justified actor budget |
| `CaptureFailure` | Private, unsupported, oversized, unavailable, racing, or stale source | No wire payload; target unchanged |
| `CommitSwitch` | Successful `BundleLeaseManager::commit`, or server return at `ClientReleased` | Publish activation notice after input mutation; never roll input back |
| `AbortSwitch` | Every `fail_context`, invalidation, denial, reconfiguration, and shutdown path | Non-blocking cancellation and stale-result fencing |
| `SetInputReady` | Existing peer readiness/session updates | Remains an input-only action; clipboard observes loss only to cancel stale work |
| `SendSnapshot` | Transport accepts one active payload for an authenticated negotiated peer | Queue full or channel down skips clipboard only |
| `TransferFailure` | Timeout, EOF, TLS failure, or cancellation | Drop payload and close only the clipboard peer session |
| `CorruptWire` | Fault-injected parser/TLS tests | Reject before publication; input DTLS remains usable |
| `DeliverRetired` | Delayed old-frame test | Reject by authority/session/handoff/token fences |
| `ReceiveSnapshot` | Validated frame becomes one target stage | Validate all lengths, hash, UTF-8, identity, and preparation first |
| `RejectSnapshot` | Any parser, identity, capability, size, or duplicate mismatch | Target unchanged; stable reason code |
| `ActivateTarget` | Coordinator records committed owner and sends exact target token | Only a completed pre-activation preparation is eligible |
| `ActivationFailure` | Process/session/channel changed before activation | Skip; input remains committed |
| `SkipUnpreparedAfterCommit` | Input commit occurs without completed valid preparation | Terminal `target_not_prepared`; no late baseline |
| `ApplySnapshot` | Target actor rechecks owner/token/session/baseline and writes once | Record applied identity before publishing success |
| `DropStage` | Destination changed, backend failed, handoff superseded, or token stale | Drop bytes; native clipboard unchanged |
| `StaleHandoffFailure` | New handoff or owner supersedes old work | Old completions become no-ops with reason metadata |
| `HandoffProcessFailure` | Authority or peer process session changes | Invalidate preparation, payload, stage, and applied attempt |
| `SetChannel` | TLS peer connected/disconnected | Update clipboard availability only |
| `SetBackend` | Native actor ready/unavailable transition | Update clipboard capability only |
| `NativeChange` | Native event-loop generation update | Never publish while inactive; may invalidate target baseline |
| `PeerRestart` | Authenticated hello carries a new process session | Drop old peer clipboard state; preserve input connection state |
| `RestartAuthority` | Server process restart | New random authority session; all old clipboard work is stale |

`CorruptWire` and `DeliverRetired` are checker-environment actions. They are
represented by deterministic fault-injection tests, not public production
methods.

## Dependency Graph

```text
P0
 |
 v
P1 domain and actor contract
 | \
 |  v
 |  P2 framing/TLS transport
 | /
 v
P3 coordinator and input hooks
 |
 v
P4 Linux -> P5 Windows -> P6 macOS
                         |
                         v
               P7 cross-platform hardening
                         |
                         v
               P8 native deploy
                         |
                         v
               P9 final acceptance
```

The native backend order follows the normative design. Work can be prepared in
parallel, but phase completion remains ordered so each new platform reuses the
same already-verified domain, actor, and transport contract.

## P0: Refinement and Baseline Gate

Status: completed on 2026-07-14. The Rust implementation remains unchanged at
the P0 boundary.

P0 evidence:

- exact clean Lan Mouse revision
  `7cc0f680768dc9b3ce479e0fb19d486c65ceb9a9`;
- no-GTK workspace check and 54 tests pass;
- Linux production-feature check and tests pass;
- pre-existing warnings remain limited to the unused no-feature
  `display_selector` parameter and no-GTK `start_service` function;
- the preferred dynamic enable/disable model action was selected and checked;
- the one-remote formal abstraction is accepted only with authenticated
  per-host production state and the required three-host Rust trace.

### P0.1 Freeze Evidence

1. Record the exact Lan Mouse commit and dirty-tree state.
2. Run the existing no-GTK workspace check and tests without modifying source.
3. Run the Linux production-feature check and tests used by deployment.
4. Save the three TLC commands and results with the implementation evidence.
5. Confirm that any existing failure is baseline evidence, not attributed to
   clipboard work.

The accepted post-P0 model baseline is:

- deep safety: 32,357,094 generated states, 1,525,938 distinct states, depth
  30, no error;
- capability disabled: 467 generated states, 16 distinct states, depth 7, no
  error;
- unconstrained liveness/action isolation with runtime enable/disable:
  70,107,300 generated states, 7,433,184 distinct states, depth 25, no error.

These are bounded checker results, not an unbounded proof.

### P0.2 Resolve Remote-to-Server Pre-Capture Refinement

The remote EnterOnly edge occurs before the server authority allocates the
return `HandoffId`. The implementation must not solve that timing gap by
publishing an unscoped clipboard snapshot.

Use this refinement:

1. On the remote current owner, `CaptureBegin` non-blockingly requests one
   memory-only provisional capture tagged with the current authority-issued
   source token and local process session.
2. The remote sends `ReleaseRequest` immediately. It does not await capture.
3. On the server, a valid release request emits `PeerReleaseStarted` before
   local capture release. The authority allocates the return handoff and target
   ownership token and starts local target preparation.
4. `AuthorityState` conveys the exact return handoff identity to the remote.
5. The remote may bind its provisional result only when its source token,
   process session, generation result, and authority session match that
   handoff. Before binding, the provisional result has no wire identity and
   cannot be sent or applied.
6. If capture was not stable before ownership changed, no matching result
   exists, or preparation completes too late, settle to `Skipped`; release and
   fallback remain unaffected.

The provisional capture is a refinement stutter: it has no externally usable
identity or protocol effect until `BeginFallback` has occurred. Add explicit
tests that an old provisional result cannot bind to a newer return handoff. If
this cannot be represented as a stuttering implementation detail, update the
TLA+ model before P1.

### P0.3 Prove Queue Bounds Before Choosing Numbers

Do not add arbitrary channel capacities. For each queue, list the maximum
simultaneous producers and whether an item is replaceable:

- latest authority state: replaceable single-slot state;
- prepare/capture/activate/cancel commands: one per active handoff class;
- source payload: one slot;
- target stage: one slot;
- transport payload send: one per peer;
- service completion records: derive capacity from the finite set of active
  prepare, capture, transfer, and apply operations.

Prefer latest-value/watch slots, one-shot completions, and actor-local options
over general FIFO queues. Any numeric capacity left in code must have the above
producer-count justification in a comment and a saturation test.

### P0.4 Native and Logging Spikes

- Verify the exact platform APIs and crate features before adding dependencies.
  Current lockfile evidence includes `windows`, `objc2`, `wayland-client`,
  `wayland-protocols`, `wayland-protocols-wlr`, and `x11`, but transitive
  presence alone is not permission to rely on an API.
- Select a minimal `tracing` integration for new clipboard events while
  preserving existing persistent `log` output. Do not mass-convert unrelated
  Lan Mouse logging.
- Prove that the current certificate material can configure mutual TLS 1.3 and
  expose the authenticated certificate fingerprint before building framing.

### P0.5 Close Model/Runtime Refinement Boundaries

The checked model chooses `clipboard_enabled` in `Init` and leaves it unchanged
in every action. The design and current Lan Mouse configuration watcher permit
runtime reconfiguration. Do not implement a dynamic enable/disable transition
that the model does not contain.

P0 selected the preferred path below:

1. Preferred: add `SetClipboardEnabled(enabled)` to `ClipboardNext`. Disabling
   must cancel the active handoff, clear wire/stage payload ownership, preserve
   native clipboards, and leave all `inputVars` unchanged. Enabling starts no
   handoff until the next real owner transition. Re-run all three TLC profiles.
2. Conservative: treat clipboard configuration as startup-only and require a
   process restart to change it. Document that operational constraint in the
   design before implementation.

This action is now in `ClipboardNext`. `ClipboardDisabledSettled` requires no
active handoff, wire payload, or target stage while disabled, and
`ConfigurationPreservesNativeClipboard` checks native-value preservation.

The model also uses one abstract remote host while production has multiple
authenticated peers. Record the refinement argument that only one owner and
one handoff are active and all peer state is keyed by authenticated `HostId`.
Add a three-host Rust trace with a delayed frame from remote A while remote B is
current. If implementation introduces cross-peer shared state that invalidates
this symmetry argument, extend the model before P1.

### P0 Exit Gate

- Baseline `cargo check` and `cargo test` results are recorded.
- The return pre-capture sequence has a written event/identity trace.
- Queue capacities have formulas or single-slot semantics.
- TLS certificate conversion and each native API ownership model have a chosen
  implementation path backed by current source/API evidence.
- Runtime enable/disable and the multi-peer abstraction have a checked model or
  an explicit model-consistent restriction.
- No source file has been modified before all five items are resolved.

Exit result: passed. Certificate DER and PKCS#8 key material have direct
rustls representations; TLS 1.3 fingerprint verifiers retain handshake
signature verification; the pinned dependency sources expose ext/wlr data
control, Windows clipboard sequence/listener APIs, and AppKit pasteboard
generation/read/write APIs. Queue ownership uses single replaceable slots plus
one completion per active prepare, capture, transfer, and apply operation.

Rollback: none; P0 is read-only.

## P1: Domain State and Native Actor Contract

Status: completed on 2026-07-14.

P1 evidence:

- added the unused-at-runtime `lan-mouse-clipboard` workspace crate with
  identity newtypes, one-active-handoff coordinator, serialized native actor,
  and a single payload slot outside the Service event path;
- actor apply uses a generation-guarded backend write operation, preventing a
  native generation check/write race;
- 21 focused tests cover identity exhaustion/non-reuse, supersession,
  authority/process/host fences, return-to-server preparation, stable capture,
  unavailable-versus-empty behavior, destination preservation, duplicate
  apply, payload isolation, queue saturation/closure, shutdown, and a delayed
  three-host result;
- `cargo fmt --all -- --check`, focused check/test, and no-GTK workspace
  check/test pass; the two pre-existing no-feature warnings are unchanged;
- no runtime Service or existing workspace crate imports the new crate.

### Files

Create the workspace crate defined by the design:

```text
lan-mouse/lan-mouse-clipboard/Cargo.toml
lan-mouse/lan-mouse-clipboard/src/lib.rs
lan-mouse/lan-mouse-clipboard/src/types.rs
lan-mouse/lan-mouse-clipboard/src/actor.rs
lan-mouse/lan-mouse-clipboard/src/backend/mod.rs
```

Add the workspace member and only dependencies exercised in this phase.

### Domain Types

Implement non-interchangeable newtypes for:

- `ProcessSessionId`
- `AuthoritySessionId`
- `OwnershipEpoch`
- `OwnershipToken`
- `HandoffEpoch`
- `HandoffId`
- `SnapshotSequence`
- `SnapshotId`
- `NativeGeneration`
- authenticated `HostId`

Use checked monotonic allocation. Epoch exhaustion is an explicit fatal state;
no identity wraps or reuses a value after abort.

Represent actor/transport-owned clipboard data as a closed enum:

```text
Text(Arc<[u8]>)
Empty
Unavailable(ClipboardReason)
```

`Unavailable` must not be convertible to `Empty`. UTF-8 validation belongs at
native-capture publication and wire receive boundaries. This payload enum must
never enter a Service/coordinator channel. The actor retains the payload in its
single local slot and reports only fixed-size metadata such as snapshot ID,
kind, and byte count. After coordinator validation, an identity-only publish
command lets the actor transfer the matching slot directly to transport.

### Pure Coordinator Reducer

Implement a pure reducer with one active handoff and independent source and
target substates. Inputs are fixed-size facts; outputs are fixed-size actor or
transport commands. The reducer must cover every model action in the ledger.

Required behavior:

- begin only from the current owner token;
- allocate source/target identities once;
- record preparation completion before activation;
- accept source completion after owner commit only if it was started by the
  recorded source token;
- make superseded completion events idempotent no-ops;
- distinguish `Applied`, `Skipped(reason)`, and `Canceled` terminal states;
- release payload ownership on every terminal transition;
- never return an input mutation command.

### Actor Contract

Use a dedicated serialized actor per process. Prefer bounded channels over an
async trait for thread-affine native APIs. If an async trait is genuinely
needed, use Rust native async trait syntax; do not add `async-trait`.

Commands:

```text
ObserveGeneration
PrepareTarget
CaptureSource
ActivateTarget
StageSnapshot
CancelHandoff
Shutdown
```

Results:

```text
Prepared
Captured(snapshot_id, kind, bytes)
Applied
Skipped(reason)
BackendUnavailable(reason)
Canceled
```

`Captured` is metadata only. The matching actor-local payload remains
inaccessible to `Service` until an identity-only publish command transfers it
to the bounded transport slot.

Native apply is one serialized critical section: recheck every fence, perform
the native write, record post-write generation and applied identity, then
report success. There is no await or cancellation point between successful
native write and applied-identity recording.

Add a deterministic fake backend capable of pausing and failing at every
generation read, content read, preparation, stage, write, and shutdown point.

### P1 Tests

- non-reused owner/handoff epochs across commit, abort, and supersession;
- old authority and process sessions rejected;
- authenticated source host mismatch rejected;
- inactive source token rejected;
- target prepare completion after activation rejected;
- destination generation mismatch drops stage;
- duplicate apply is idempotent;
- explicit empty writes empty; every unavailable reason preserves content;
- private/unsupported/oversized source never creates payload metadata;
- newer handoff supersedes all replaceable state;
- actor queue saturation returns `queue_full` and leaves input facts untouched;
- actor shutdown releases payload and backend resources;
- reducer transition traces correspond to successful and failing TLA+ paths.

### P1 Verification

```text
cargo fmt --all -- --check
cargo check -p lan-mouse-clipboard
cargo test -p lan-mouse-clipboard
cargo check --workspace --exclude lan-mouse-gtk --no-default-features
cargo test --workspace --exclude lan-mouse-gtk --no-default-features
```

P1 exit result: passed. All tests pass, payload count/size assertions are
executable, generation compare-and-write is one backend critical operation,
and no runtime service code imports the new crate yet.

Commit boundary: stage only the new crate, workspace membership, lockfile, and
their focused tests. Proposed subject: `feat: add clipboard handoff domain`.

Rollback: remove the unused workspace member; runtime behavior is unchanged.

## P2: Framing, Authentication, and Transport

Status: completed on 2026-07-14.

P2 evidence:

- implemented fixed network-order `LMCB` V1 framing for all control and
  snapshot messages outside `lan-mouse-proto`;
- prefix validation rejects magic/version/type/flags/header/length faults
  before payload allocation, and fixed identity metadata is validated before
  payload read or inbound publication;
- implemented TLS 1.3-only mutual certificate authentication with exact leaf
  fingerprint authorization, TLS handshake signature verification, ALPN, and
  authenticated certificate-to-`HostId` binding;
- implemented deterministic peer process-session replacement, authority/peer
  fences, one writer, bounded control paths, and one payload total across
  queued and active transfer state;
- handoff cancellation interrupts a blocked payload write and releases its
  bytes; malformed clipboard input exits independently while a fake input path
  continues;
- the focused crate suite now has 47 passing tests; formatting, focused check,
  and no-GTK workspace check/test pass with only the two unchanged baseline
  warnings.

### Files

```text
lan-mouse/lan-mouse-clipboard/src/frame.rs
lan-mouse/lan-mouse-clipboard/src/transport.rs
lan-mouse/lan-mouse-clipboard/src/tls.rs
```

Keep payload framing out of `lan-mouse-proto`.

### Binary Codec

Implement the fixed `LMCB` prefix and network-byte-order headers from the
design. Decode in this exact order:

1. Read a fixed prefix into a fixed-size stack buffer.
2. Validate magic, exact protocol version, message type, known flags, and exact
   header length.
3. Compare `payload_length` to negotiated max and platform `usize` before any
   payload allocation.
4. Allocate at most the accepted length and read exactly that amount under
   cancellation and the fixed transfer budget justified in P0.
5. Validate SHA-256 and UTF-8.
6. Publish an immutable payload only after identity validation succeeds.

Encode and decode:

- `ClipboardHello`
- `AuthorityState`
- `PrepareTarget` / `PrepareResult`
- `OwnershipActivated`
- `SnapshotOffer` / `SnapshotDeliver`
- `ApplyResult`
- `CancelHandoff`
- `ProtocolError`

No JSON, base64, compression, payload log, digest log, or silent version
downgrade is allowed.

### TLS and Peer Identity

- Use TCP with TLS 1.3 on the configured Lan Mouse numeric port; input remains
  UDP/DTLS on its existing socket.
- Reuse each process certificate and the configured authorized fingerprint
  map.
- Require mutual certificate authentication.
- Bind `ClipboardHello.host_id` to the authenticated fingerprint's configured
  host, not to self-asserted hello data.
- The authority initiates one full-duplex connection per configured peer. A
  duplicate authenticated connection is deterministically replaced or
  rejected according to current process session; never retain two payload
  writers for one peer.
- A changed peer process session invalidates that peer's clipboard state only.
- Reconnect asynchronously with bounded backoff outside the Service loop.
- Closing or corrupting this channel cannot close, reset, or backpressure DTLS.

### Transport Ownership

- One bounded control path and one payload slot per peer.
- Payload bytes move as `Arc<[u8]>` between actor and transport tasks without a
  Service-loop copy.
- Exactly one task owns each TLS writer. No actor waits while holding it.
- Cancellation drops queued payload ownership even if the socket task is still
  unwinding.
- Negotiated max is `min(local_max, peer_max)` and is checked at source,
  framing, receiver, and stage boundaries.

### P2 Tests

- fragmented reads for every prefix/header/payload boundary;
- partial writes and EOF at every byte boundary;
- declared length over max rejected before allocation;
- integer conversion overflow rejected;
- invalid magic/version/type/flags/header length;
- unknown version does not fall back;
- hash mismatch and invalid UTF-8;
- exact payload limit and one byte over;
- connection loss and cancellation during payload transfer;
- duplicate connection and process-session replacement;
- authenticated certificate/host mismatch;
- delayed frame from old authority and peer sessions;
- malformed clipboard channel while a fake DTLS/input path continues;
- payload slot saturation drops clipboard work without blocking a producer.

### P2 Verification

Run P1 commands plus focused codec, TLS-loopback, and fault-transport tests.
Add dependencies to `Cargo.toml` only after the exact APIs are used.

P2 exit result: passed. Every malformed-length path is rejected before
oversized allocation, loopback TLS proves mutual fingerprint identity and
host binding, stale payload metadata is rejected before publication, and
input-isolation tests pass.

Commit boundary: framing, TLS transport, necessary dependency/lockfile changes,
and focused tests. Proposed subject: `feat: add authenticated clipboard transport`.

Rollback: transport is not yet started by `Service`; remove the P2 commit.

## P3: Coordinator and Input-Transition Hooks

Status: pending. Depends on P1 and P2.

### Files

```text
lan-mouse/src/clipboard.rs
lan-mouse/src/clipboard_transport.rs
lan-mouse/src/service.rs
lan-mouse/src/capture.rs
lan-mouse/src/config.rs
lan-mouse/src/crypto.rs            # only if TLS material conversion needs it
```

### Configuration

Implement the normative configuration:

```toml
[clipboard]
enabled = true
max_bytes = 3145728
```

Validation requirements:

- `max_bytes` must be non-zero and representable by all framing/allocation
  layers;
- the default is 3 MiB;
- disabling clipboard cancels handoffs and closes clipboard transport only;
- no retry, polling, or timeout knobs are exposed;
- missing switch-controller ownership authority makes clipboard unavailable,
  not input unavailable;
- configuration reload uses a new process/authority fence when required and
  never reinterprets old work under new host identity.

### Service Integration

Add only fixed-size coordinator state and completion receivers to `Service`.
The Service loop must never read, hash, copy, parse, or write clipboard bytes.

Server-to-remote sequence:

1. After `BundleLeaseManager::reserve` succeeds, call non-blocking
   `begin_handoff(server, target)`.
2. Start source capture and target preparation concurrently.
3. Continue the existing controller prepare/arm/commit path regardless of
   clipboard outcome.
4. After `BundleLeaseManager::commit` succeeds, publish
   `OwnershipActivated(target_token)`.
5. If a valid target preparation was not completed before this commit, settle
   the handoff as `target_not_prepared`; do not record a late baseline.

Remote-to-server sequence:

1. On EnterOnly `CaptureBegin`, request the P0 provisional source capture, then
   send `ReleaseRequest` immediately.
2. On a valid server-side release request, emit identity-only
   `ICaptureEvent::PeerReleaseStarted` before releasing local capture.
3. On `PeerReleaseStarted`, the authority allocates the return handoff, starts
   server target preparation, and publishes return `AuthorityState` to the
   remote source.
4. Preserve current release ordering: local input release, key-up/modifier
   cleanup, `Leave`, then `ClientReleased`.
5. On the matching `ClientReleased`, publish the server target token as active.
   Never delay release for preparation, capture, transport, or apply.

Cancellation hooks:

- every `fail_context` variant;
- bundle lease invalidation or expiry;
- peer readiness/session loss;
- commit denial or dropped decision receiver;
- controller failure or cancellation;
- capture backend disable;
- config reload that changes relevant identity/capability;
- service shutdown;
- newer handoff supersession.

Cancellation is `try_send`/latest-state publication only. A full or closed
clipboard channel is already equivalent to cancellation and cannot alter input
cleanup.

### P3 Fake Integration Tests

- server-to-remote successful ordering;
- remote-to-server successful ordering with provisional capture binding;
- provisional capture cannot bind after source token/process change;
- preparation completion one event before and one event after input commit;
- snapshot before and after input commit;
- stage before activation, then apply after activation;
- input commits while native actor is paused forever;
- server fallback commits while clipboard transport is paused forever;
- every existing `fail_context` path cancels matching clipboard state;
- stale cancellation does not cancel a newer handoff;
- queue full on every hook leaves existing bundle-gate state unchanged;
- peer clipboard process restart leaves input readiness unchanged;
- config disable cancels clipboard but leaves active client/input state;
- no `ProtoEvent::Enter`, input forwarding, or release ordering regression.

Instrument tests with explicit event traces. Assertions must show the input
transition result before inspecting clipboard terminal state.

### P3 Verification

```text
cargo fmt --all -- --check
cargo check --workspace --exclude lan-mouse-gtk --no-default-features
cargo test --workspace --exclude lan-mouse-gtk --no-default-features
cargo check --no-default-features --features <current-linux-production-features> --bin lan-mouse
cargo test --no-default-features --features <current-linux-production-features> --bin lan-mouse
```

Use the exact feature list from deployment rather than inventing a new alias.

P3 exit gate: fake actors/transports can be permanently blocked or failed at
every phase while all existing input tests still settle normally.

Commit boundary: coordinator, config, capture/service hooks, fake integration
tests, and necessary TLS startup wiring. Proposed subject:
`feat: integrate clipboard handoff with input ownership`.

Rollback: disable/remove Service startup and hooks as one commit; no
`BundleLeaseManager` schema or DTLS protocol rollback is needed.

## P4: Linux Native Clipboard Actor

Status: pending. Depends on P3.

### Files and Backend Selection

```text
lan-mouse/lan-mouse-clipboard/src/backend/wayland.rs
lan-mouse/lan-mouse-clipboard/src/backend/x11.rs
lan-mouse/lan-mouse-clipboard/src/backend/linux.rs
```

Probe in this order, using APIs verified in P0:

1. ext-data-control where the compositor advertises it;
2. wlr data-control fallback for the current Hyprland environment;
3. X11 `CLIPBOARD` when an X11 session and selection APIs are available;
4. a compatible portal session only when it can satisfy generation,
   preparation, bounded read, and ownership-lifetime requirements;
5. otherwise `backend_unavailable` with no clipboard capability.

Do not shell out to `wl-copy`, `wl-paste`, `xclip`, or `xsel`; the actor must own
the native protocol lifecycle and bound every fd/selection transfer.

### Linux Semantics

- Track clipboard generation from selection-owner/native protocol events.
- Support only UTF-8 text MIME types and explicit empty.
- Bound Wayland fd reads before/during accumulation and cancel stale reads.
- Keep the data source alive for as long as the compositor may request data.
- For X11, implement `CLIPBOARD` ownership and bounded `INCR` receive/send;
  never synchronize `PRIMARY`.
- Mark self-generated changes so the actor advances generation but does not
  republish or invalidate its own completed apply incorrectly.
- If neither backend can supply a trustworthy generation, advertise no
  clipboard capability rather than weakening destination preservation.

### P4 Tests and Gate

- fake protocol tests for owner changes, fd EOF, over-limit stream, and cancel;
- X11 `INCR` chunking at limit and one byte over;
- supported text and explicit empty;
- source changes during read;
- target changes after prepare and after stage;
- backend disconnect/reconnect and process shutdown;
- applied data remains available after the command returns;
- current Linux desktop native smoke test in the interactive user session;
- full no-GTK Linux feature check/test remains green.

P4 exit gate: the Linux binary initializes one real backend, completes native
text/empty and destination-race tests, and input switching works with the
clipboard actor forcibly unavailable.

Commit boundary: Linux backend, exact feature/dependency changes, and tests.
Proposed subject: `feat: add Linux clipboard backend`.

Rollback: backend reports unavailable; domain, transport, and input remain
usable.

## P5: Windows Native Clipboard Actor

Status: pending. Depends on P4.

### Implementation

- Run a dedicated interactive-user clipboard thread with its own message-only
  window and `WM_CLIPBOARDUPDATE` subscription.
- Use `GetClipboardSequenceNumber` as the opaque generation source.
- Read only `CF_UNICODETEXT`; inspect `GlobalSize` before locking/scanning and
  reject invalid UTF-16 or an encoded UTF-8 result over the negotiated limit.
- Distinguish a real empty text value from `OpenClipboard`, format, lock, or
  permission failure.
- Apply with correctly owned global memory and a private self-write marker.
- Record applied identity before reporting success and suppress the matching
  update notification.
- Bound `OpenClipboard` contention retries with the P0 native-API rationale;
  do not expose a user timing knob.
- Shut down the window/thread without leaving clipboard ownership or a lock
  held.

### P5 Tests and Gate

- current-user interactive scheduled-task context;
- UTF-16 round trip, non-BMP text, explicit empty, and invalid encoding;
- clipboard busy/locked and permission failure;
- source sequence changes during read;
- target sequence changes after prepare;
- exact limit and one byte over after UTF-8 conversion;
- duplicate apply and self-notification suppression;
- process restart and old process-session rejection;
- native no-GTK Windows workspace tests and binary build;
- input remains usable when the Windows actor thread is stopped.

P5 exit gate: tests and native build pass on Windows itself. A Linux
cross-compile is not accepted as native backend verification.

Commit boundary: Windows backend and its target-specific dependencies/tests.
Proposed subject: `feat: add Windows clipboard backend`.

Rollback: Windows advertises no clipboard capability; input remains usable.

## P6: macOS Native Clipboard Actor

Status: pending. Depends on P5.

### Implementation

- Run the general pasteboard adapter on the required AppKit run loop/thread.
- Use `NSPasteboard.changeCount` as the opaque generation.
- Read/write only plain UTF-8 text and explicit empty.
- Reject private/concealed/transient content according to the design's privacy
  policy.
- Preserve a target-local change by comparing `changeCount` with the prepared
  baseline immediately before write.
- Keep any required provider/application object alive after apply.
- Maintain the stable launch identity used by the current signed daemon and
  LaunchAgent. Do not create a changing per-build identity that loses user
  permission state.
- Report persistent permission denial as an actionable backend state. A native
  notification is persistent enough to read and explicitly says input
  switching still works.
- Poll only if AppKit provides no event-driven generation update, only at a low
  rate while this host owns the current token, and never as the correctness
  mechanism for transition capture.

### P6 Tests and Gate

- native text, Unicode, and explicit empty;
- source `changeCount` race;
- target local-copy race after prepare and after stage;
- private/concealed and unsupported content;
- permission denied and restored;
- process restart and stale process session;
- exact limit and one byte over;
- actor/app lifecycle after apply;
- debug-profile no-GTK workspace tests and binary build on macOS;
- input remains usable when pasteboard permission is denied.

P6 exit gate: tests and the configured debug native build pass on macOS itself,
with the LaunchAgent running in the interactive user session.

Commit boundary: macOS backend, stable identity integration, target-specific
dependencies, and tests. Proposed subject: `feat: add macOS clipboard backend`.

Rollback: macOS reports unavailable/permission denied and does not negotiate
the capability; input remains usable.

## P7: Cross-Platform Hardening and Observability

Status: pending. Depends on P4, P5, and P6.

### Structured Tracing

Emit the design's stable events with `tracing`:

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

Allowed fields are host IDs, shortened random session identifiers, epochs,
snapshot sequence, byte count, duration, and stable reason code. Never record
payload, hash, source application, MIME content, or text-derived value.

Stable reasons:

```text
capability_missing backend_unavailable permission_denied private_content
unsupported_format oversize source_changed target_not_prepared
destination_changed stale_authority_session stale_peer_session stale_handoff
stale_owner_token duplicate channel_unavailable transfer_timeout
protocol_error integrity_failed invalid_utf8 canceled queue_full
```

Notify only persistent actionable backend failures. Per-handoff transient
failures remain logs/status and do not produce notification spam.

### Concurrency and Failure Audit

- Enumerate every lock and actor/thread boundary.
- Assert no lock/borrow survives an await or native callback.
- Abort each task at every await in fault tests and prove payload slots return.
- Exercise rapid A -> B -> A and A -> B -> C supersession.
- Deliver stale frames after authority restart and peer restart.
- Lose prepare/apply result messages after native completion.
- Keep actor, TLS reader, TLS writer, and reconnect tasks blocked independently
  while input commits and falls back.
- Verify one peer's malformed clipboard connection does not affect another
  peer or the input listener.
- Verify clipboard-disabled and capability-missing hosts do not create retry or
  CPU loops.

### Performance Gate

- Pointer/keyboard forwarding profiles show no clipboard hash, allocation,
  native call, TLS call, or wait.
- A transition hook performs only fixed-size identity construction and
  non-blocking publication.
- One source and one target payload are the maximum live payload set for the
  active handoff.
- No V1 compression or background clipboard polling except the constrained
  macOS generation observation described above.
- Idle clipboard CPU is event-driven on Windows, Wayland, and X11.

### P7 Verification

Run formatting, all no-GTK workspace checks/tests, current Linux feature
checks/tests, focused fault tests, and clippy for all targets that can build on
the current host. Resolve new warnings in touched code; record unrelated
baseline warnings separately.

P7 exit gate: every C1-C12 invariant has at least one direct Rust test and one
model/refinement-ledger reference, and the deadlock/performance audit has no
unowned wait or unbounded queue.

Commit boundary: hardening, tracing bridge, reason-code tests, and audit fixes.
Proposed subject: `feat: harden clipboard handoff lifecycle`.

## P8: Native Build and Deployment

Status: pending. Depends on P7.

Deployment is one final phase, not a substitute for domain/native tests.

### Ansible Changes

Update only the necessary deployment inputs:

- render `[clipboard]` with `enabled` and `max_bytes` for every Lan Mouse host;
- include clipboard feature/dependency inputs in Linux packaging;
- retain native macOS and Windows builds with `strategy: free` so those hosts
  build concurrently;
- retain the configured macOS debug/release variable and current debug default;
- add authenticated TCP ingress for the Lan Mouse port on hosts that accept an
  authority clipboard connection, while preserving existing UDP input rules;
- keep certificate and fingerprint files private and reuse their current host
  mapping;
- include revision, lockfile, toolchain, feature set, and profile in native
  build identity so an exact artifact is not confused with another build;
- verify persistent logs contain structured clipboard status but no content;
- restart only Lan Mouse when a clipboard binary/config/runtime artifact
  changes;
- do not reboot Linux, Windows, or macOS and do not restart `tv-multiview` for
  a clipboard-only change.

Do not add another revision-compatibility cache layer or a clipboard-specific
deployment framework. Reuse the existing exact-revision bundle/vendor/native
build pipeline.

### Native Build Gate

For one exact committed revision:

1. Linux no-GTK production-feature check, test, package build, and install.
2. Windows no-GTK workspace test and release binary build on Windows.
3. macOS no-GTK workspace test and configured debug binary build on macOS.
4. Record source revision and binary digest for all three artifacts.
5. Require zero failed/unreachable Ansible hosts before runtime cutover.

### Deployment Order

1. Package the exact committed source and locked vendor tree once.
2. Build/test Windows and macOS concurrently; build/test Linux through its
   native package path.
3. If any native build fails, deploy nothing and keep all current services.
4. Install the exact revision on all configured Lan Mouse hosts.
5. Restart Lan Mouse services only, then verify process, revision, native
   backend status, clipboard channel status, and persistent logs.
6. If any host cannot start the exact revision, restore the previous exact
   revision on every host. Do not leave an intentional mixed deployment.

### Minimal Runtime Acceptance

Use normal, non-destructive operation rather than an exhaustive live failure
matrix:

- input switches to each configured host and returns to the server host;
- keyboard and pointer always move together;
- supported text and explicit empty hand off in each normally used direction;
- a local copy on the destination during a delayed handoff is preserved;
- disabling or disconnecting clipboard leaves input switching available;
- no service crash, restart loop, payload log, or persistent clipboard lock.

P8 exit gate: one exact revision is running on all configured hosts, native
backend/channel status is observable, and minimal acceptance passes.

Commit boundary: Ansible variables/templates/tasks and deployment-focused
verification. Proposed subject: `deploy: enable native clipboard handoff`.

Rollback: restore the prior exact Lan Mouse revision/config on all hosts and
restart Lan Mouse only. Native clipboards are not rewritten during rollback.

## P9: Final Refinement and Acceptance

Status: pending. Depends on P8.

### TLC Recheck

From `osswitch/tla/`, run all three configurations with the current checker:

```text
java -cp /home/example/.cache/nvim/tla.nvim/tla2tools.jar tlc2.TLC \
  -config ClipboardHandoff.cfg ClipboardHandoff.tla

java -cp /home/example/.cache/nvim/tla.nvim/tla2tools.jar tlc2.TLC \
  -config ClipboardHandoff-capability-disabled.cfg ClipboardHandoff.tla

java -cp /home/example/.cache/nvim/tla.nvim/tla2tools.jar tlc2.TLC \
  -config ClipboardHandoff-liveness.cfg ClipboardHandoff.tla
```

Do not use scenario constraints as evidence for liveness. Preserve the current
split between constrained deep safety profiles and unconstrained liveness.

### Final Rust Verification

- `cargo fmt --all -- --check`
- no-GTK workspace `cargo check`
- no-GTK workspace `cargo test`
- current Linux production-feature `cargo check` and `cargo test`
- focused domain, parser, TLS, fake actor, and integration fault tests
- native Windows test/build
- native macOS test/build
- native Linux package test/build
- clippy on all locally buildable targets

### Documentation Closure

1. Update this plan phase by phase with commit IDs and exact verification
   results; do not rewrite the whole file.
2. Update `clipboarddesgin.md` only for an actual approved semantic/model
   change, not to narrate implementation details.
3. Record any checker-driven model correction before claiming implementation
   completion.
4. Record deployed source revision and native artifact identities.
5. Leave unrelated dirty-tree files untouched.

### Completion Gate

The plan is complete only when:

- all P0-P9 exit gates pass;
- all required invariants have model and Rust evidence;
- input commit/fallback tests pass with every clipboard subsystem blocked;
- all three native actors pass on their own operating systems;
- all three TLC profiles finish with no error;
- one exact revision is deployed and running;
- no payload/content-derived value appears in logs or persistence;
- all verified implementation/deployment changes are committed in scoped
  commits.

## Commit Sequence

Each commit is made immediately after its own verification passes, with only
relevant files staged:

1. `feat: add clipboard handoff domain`
2. `feat: add authenticated clipboard transport`
3. `feat: integrate clipboard handoff with input ownership`
4. `feat: add Linux clipboard backend`
5. `feat: add Windows clipboard backend`
6. `feat: add macOS clipboard backend`
7. `feat: harden clipboard handoff lifecycle`
8. `deploy: enable native clipboard handoff`
9. A documentation/model commit only if those artifacts changed after the
   current checked baseline

Commit subjects may be adjusted to match branch history, but phase boundaries
must not be collapsed before their individual gates pass.

## Risk Register

| Risk | Required control | Blocking evidence |
|---|---|---|
| Clipboard delays input commit/fallback | Fixed-size `try_send`; no guard/await; permanently blocked actor tests | Any input transition waits on clipboard |
| Remote return captures without authority identity | P0 provisional result remains unbound and unsendable until exact handoff assignment | Unscoped snapshot reaches transport |
| Destination clipboard is overwritten after local copy | Prepared generation rechecked immediately before apply | Race test writes incoming value after generation mismatch |
| Old process/frame applies after restart | Random authority/process sessions in every identity | Delayed-frame test causes native write |
| Oversized/malformed frame allocates memory | Prefix and length validation before allocation | Allocation occurs before max validation |
| Native API blocks Service | Dedicated actor/event-loop thread and bounded completion path | Native call appears in Service loop |
| TLS identity is self-asserted | Fingerprint-to-configured-host binding | Hello host accepted without certificate mapping |
| Lazy clipboard provider dies too early | Actor/provider lifetime extends past apply | Paste fails after successful apply result |
| Queue capacity is arbitrary/unbounded | Single-slot semantics or producer-count proof | Unjustified capacity or unbounded channel |
| Persistent notification spam | Notify only persistent actionable backend state | Per-handoff transient notification |
| Mixed native binaries diverge | Exact-revision build identity and all-host cutover/rollback | Intentional mixed revision after deploy |
| Clipboard content leaks | Structured field allowlist and log-capture tests | Payload, hash, MIME content, or source app logged |

## Implementation Status

- P0 Refinement and baseline gate: completed 2026-07-14
- P1 Domain state and actor contract: completed 2026-07-14
- P2 Framing, authentication, and transport: completed 2026-07-14
- P3 Coordinator and input-transition hooks: pending
- P4 Linux native actor: pending
- P5 Windows native actor: pending
- P6 macOS native actor: pending
- P7 Cross-platform hardening and observability: pending
- P8 Native build and deployment: pending
- P9 Final refinement and acceptance: pending
