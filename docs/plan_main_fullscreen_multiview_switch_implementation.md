# Main Plan: Fenced Fullscreen / MultiView Switch Implementation

## Plan Control

- Status: implementation, automated verification, native builds, and
  coordinated production deployment are complete as of 2026-07-13. Linux,
  macOS, and Windows run the same pinned lan-mouse revision. The exhaustive
  live failure matrix and production percentile measurements are blocked by
  the explicit normal-use-first acceptance decision; failures encountered in
  ordinary use will be analyzed from the persistent logs.
- Design baseline: `fullscreenmultiviewswitchdesign.md` originated at commit
  `319b762`; its living prose and model now describe the implemented protocol.
- Rust daemon implementation: parent-repository commit `97d31e1` plus its
  prerequisite fenced-protocol commits and verification-cause commit
  `d4d08e3`.
- lan-mouse implementation: clean `../../lan-mouse` commit
  `192354206aad01609488633f44b324564fce7ee0`.
- Controlling objective: implement the approved design without allowing keyboard and pointer ownership to split, and without claiming server-host fallback before the TV state is freshly observed.
- Compatibility policy: there is no legacy mutation API, compatibility flag,
  fire-and-forget hook authorization, or mixed-version operation. Old peers and
  old clients fail closed; deployment replaces daemon and lan-mouse together.
- This plan supersedes `fullscreenmultiviewswitchplan.md` and `fullscreenmultiviewswitchplanrust.md` for new work. Those files remain historical artifacts.

Linked child plans:

- [tv-multiview daemon refactor and protocol implementation](plan_tv_multiview_daemon_refactor.md)
- [lan-mouse atomic input gate and lease integration](plan_lan_mouse_atomic_input_gate.md)

## Implementation Evidence (2026-07-13)

- tv-multiview `cargo check`, all 61 tests, and strict all-target
  clippy pass. Coverage includes deadline precedence, lease/signal failure,
  MultiView, typed HTTP conflicts and malformed input, scripted SSAP
  registration/subscription/reconnect/resync, grant-pending callback and
  stale-response handling, ping/keepalive timeout, atomic ownership, bounded
  queues, and bounded logging.
- lan-mouse no-GTK workspace check/test and Linux production-feature
  check/test pass. Coverage includes the same-edge continuation, 39 core
  protocol and server-notification tests, bounded logging, CLI status, wire
  behavior, and native macOS/Windows center-coordinate tests. The only local
  warning is the pre-existing unused `start_service` function.
- The historical three-host cache-only release matrix used source
  `4425c5789b04025720dce234887e6a2d30919258` and Cargo.lock SHA-256
  `d91c91ed08149293a08eb958281c174a5596788f5160e39de751babd52767c93`:
  Linux ELF SHA-256 `fef56b26610eba4e970460bc94b9b03370ce4eb0cc59a0287f814898d43479e0`,
  macOS Mach-O SHA-256 `6f8c734c0563b50245f339793c97180a04c2dc6039294bac29f96ecb29976bb7`,
  and Windows PE SHA-256 `1B603699A6907FC646362FCB60B7084BF4C0102B369F56F84EA7C6AFB1FC2D6E`.
- Linux used `/usr/bin/rustc 1.96.0`; macOS and Windows used native rustup
  `rustc 1.97.0`. The current `1923542` revision was subsequently tested and
  release-built natively on all three hosts, installed, and started. It orders
  backend release before controller preparation, requires a current service
  commit decision before `Enter`, resumes a still-focused verified edge,
  centers the receiving pointer before `Ack`, and bypasses ambient HTTP proxies
  for the local controller. The configured server also reports switch failure
  through a native Rust notification backend with detailed evidence and the
  controller's predicate-level reason code; notification failure cannot affect
  fallback and no notification command is installed or launched.
- Ansible syntax/task expansion and idempotent template rendering pass. Rendered
  TOML, plist, shell, PKGBUILD, systemd, and Windows PowerShell artifacts pass
  their available native parsers; bounded macOS and Windows rotation wrappers
  also passed temporary-file runtime tests on their respective hosts.

## TLA Planning Frame

```tla
GoalState ==
    /\ FencedTvDaemonImplemented
    /\ LanMouseBundleGateImplemented
    /\ KeyboardOwner = PointerOwner
    /\ RemoteOwnershipImpliesFreshDisplayAndValidLease
    /\ EveryFailureReleasesInputToServerHost
    /\ FallbackCompletionImpliesFreshServerDisplay
    /\ OneBoundedSsapOwner
    /\ PersistentForensicLogsAvailable

InitState ==
    /\ OneShotTvSubprocesses
    /\ IndependentStateMutexes
    /\ CommandSuccessTreatedAsObservation
    /\ FireAndForgetEnterHooks
    /\ CaptureStartsBeforeHookCompletes
    /\ RemoteReadinessIsOneBoolean

Plan ==
    <<BaselineAndContractGate,
      CharacterizeCurrentBehavior,
      RefactorDaemonWithoutProtocolChange,
      ImplementPureProtocolReducer,
      ImplementPersistentSsapActor,
      ImplementLanMouseReadinessAndCaptureGate,
      EnableFencedApiAndLeaseLifecycle,
      CutOverAllHostsAtomically,
      VerifyAndRemoveLegacyPath>>
```

No later action may start by assuming an earlier gate passed. Each phase has an explicit verification and rollback state.

## Historical InitState Evidence

The pre-refactor code implemented an older protocol with different safety semantics. This table is retained as the historical `InitState`, not as a description of current source.

| Area | Current evidence | Consequence |
|---|---|---|
| TV transport | `tv-multiview/src/tv.rs:21-85` starts `bscpylgtvcommand` for every operation. | No persistent SSAP owner, response correlation, subscription, bounded queue, or poll coalescing. |
| Completion | `tv-multiview/src/http.rs:135-149` calls `switch_complete()` immediately after subprocess success. | Command acknowledgement is incorrectly treated as fresh active-input and signal observation. |
| Observed state | `tv-multiview/src/state.rs:82-98` writes `tv_input = target` before the TV command. | Command intent overwrites observation and can authorize a false stable state. |
| Failure | `tv-multiview/src/http.rs:135-139` clears pending state on command failure without issuing and verifying server fallback. | The daemon can report settled state while the visible display is unknown. |
| Locking | `state.rs:82-86` acquires mode then pending; `http.rs:71-75` holds pending then acquires mode. | A real lock-order cycle exists. Independent mutexes also expose torn status snapshots. |
| Health | `main.rs:83-116` polls a subprocess every five seconds and exits after a retry cap. | Health does not mean registered, subscribed, and synchronized; transient failure can terminate the daemon. |
| Configuration | `main.rs:35-41` hardcodes TV IP, port, server host, and HDMI map. | Ansible `tv_ip` and `tv_inputs` are not authoritative for the Rust process. |
| HTTP contract | `http.rs:33-39` exposes synchronous GET mutation endpoints and plain-text responses. | There is no request ID, epoch, deadline, grant, commit, cancellation, or typed denial. |
| Hook order | `../../lan-mouse/src/capture.rs:348-368` activates the client and sends `ProtoEvent::Enter`; `service.rs:350-352` separately spawns the hook. | The shell hook is observational, not a capture gate. Remote traffic can start before TV verification. |
| Readiness | `lan-mouse-ipc/src/lib.rs:159-184` has only `alive`; `lan-mouse-proto/src/lib.rs:61-74` has `Pong(bool)`. | Keyboard and pointer availability cannot be independently represented or reserved as a bundle. |
| Windows logs | `lan-mouse-deploy/playbook.yml:488-501` starts the scheduled task without stdout/stderr redirection. | The required persistent Windows failure history does not exist. |

## Required Invariants

These invariants are implementation acceptance criteria, not comments:

1. `keyboard_owner == pointer_owner == input_owner` in every externally visible snapshot.
2. A remote host cannot own input unless its keyboard and pointer readiness are both true, its bundle lease is current, and its session epoch matches.
3. TV command intent never changes `observed_input`, `input_signal`, or `verified_epoch`.
4. A grant requires a correlated command acknowledgement plus a fresh active-input and signal observation for the current `switch_epoch`.
5. Only a current, unexpired grant plus a current bundle lease can commit both input paths.
6. Any command, observation, readiness, lease, grant, network, callback, or client failure releases both input paths to `SERVER_HOST` before asynchronous display recovery.
7. Fallback is complete only after fullscreen server input and server signal are freshly observed. TV-control outage remains `fallback_deferred` and not ready.
8. At most one enter request, one TV command transaction, and one signal query are active. Work queues and log sinks are bounded.
9. No shared state lock is held across SSAP, HTTP, lan-mouse, file, subprocess, or timer await points.
10. Transient SSAP failure retries indefinitely with bounded backoff. Retry thresholds alert; they do not fabricate recovery or force an ordinary process exit.
11. Unknown peer capability, unknown TV input, stale epoch, legacy client, and missing lease all fail closed.
12. The lan-mouse server host is configured data, not a synonym for Linux in control flow.

## Gate 0: Resolve the Capture Boundary Before Coding

The current lan-mouse event arrives after the input backend has begun capture. Running a faster HTTP hook cannot make that event a pre-capture authorization point.

Before protocol implementation begins, prove one of these safe refinements in the matching `392af44` source:

1. Preferred: add a backend-neutral pre-capture decision point that leaves keyboard and pointer locally usable while the request is pending, then enters capture only after a valid grant.
2. Conservative baseline: immediately release the first edge capture, perform
   the fenced TV request while input remains local, and arm the returned grant.
   Resume the same crossing only if that exact edge remains focused with the
   same enter serial; otherwise commit on the next matching crossing.

Keeping an exclusive capture active while waiting, suppressing events, or sending repeated `Enter` packets is not an acceptable interpretation of local ownership. If the preferred path is not implementable across the active Linux backend, select the conservative baseline and record the UX tradeoff in the design before code proceeds.

Gate evidence:

- A focused test proves no `ProtoEvent::Enter` or `ProtoEvent::Input` is emitted before grant commit.
- A manual backend test proves pointer buttons, motion, scroll, and keyboard remain usable on `SERVER_HOST` while pending and after every denial/failure.
- Cancellation and timeout release any backend capture without waiting for the TV daemon.

Gate 0 decision (2026-07-11, refined 2026-07-13): use the conservative
release-first refinement with same-edge continuation.
The current backend-neutral API reports `CaptureEvent::Begin` only after the
backend has entered exclusive capture, so a portable one-crossing pre-capture
candidate does not exist. On the first crossing, lan-mouse must emit only a
candidate, call the existing backend-neutral `capture.release()`, and keep all
input local while obtaining an expiring grant. A backend may re-grab and
synthesize `Begin` only if the original edge is still focused and its enter
serial is unchanged; otherwise only a later matching crossing may revalidate
the grant and emit `ProtoEvent::Enter`. This decision remains subject to the
automated event-order and native-backend verification gates below; no backend
may buffer input while the grant is pending.

## Interface Decision Required Before Phase 3

The approved public API defines create, poll, and commit, but the implementation also needs a transport for lease invalidation, active-session renewal, readiness loss, and client cancellation. Before coding those paths, add one documented internal contract to the design:

- lan-mouse owns the keyboard-pointer bundle lease.
- `POST /enter/{target}` carries a client request identity and lease identity/epoch.
- lan-mouse revalidates the lease before commit.
- cancellation or readiness loss is pushed immediately through an internal cancel/readiness operation; it is not deferred until grant timeout.
- an active remote session renews its lease; missing renewal forces local input release and verified fallback.
- daemon or lan-mouse restart invalidates all old leases, grants, and request epochs.

The exact local transport may be HTTP or local IPC, but it must be bounded, authenticated to the local deployment boundary, cancellable, and testable without shell processes. Do not infer lease validity from a cached `/status` read.

## Ordered Actions

### Phase 0: Baseline and Characterization

Status: completed for the retained baseline evidence. Source identities,
unsafe legacy behavior, test seams, and protocol shapes are recorded. The
pre-refactor subprocess CPU, process-count, and latency workload is blocked:
it was not captured before replacement and must not be reconstructed or
claimed without an explicitly authorized historical-path run.

Owner: both child plans.

- Record current crate and lan-mouse commit IDs in test output and deployment metadata.
- Add characterization tests before changing semantics: current state transitions, route status codes, subprocess error mapping, capture/hook ordering, and peer availability propagation.
- Introduce fake TV and fake switch-controller seams using native Rust async traits or typed channels; do not add `async-trait`.
- Record current subprocess CPU, process count, command latency, and end-to-end switch latency with exact commands and workload.
- Confirm the LG client-key storage schema and SSAP message/URI shapes from local source or authoritative protocol documentation before selecting dependencies.

Exit gate: tests describe current behavior, known unsafe behavior is named rather than encoded as desired behavior, and no production behavior has changed.

### Phase 1: Mechanical Daemon Refactor

Status: completed and verified in the current tv-multiview implementation.

Owner: `plan_tv_multiview_daemon_refactor.md`.

- Separate configuration, domain types, I/O adapters, HTTP adapters, and transition logic.
- Replace independently locked fields with one coherent state owner and immutable snapshots.
- Remove the lock-order cycle in a dedicated correctness commit, not hidden inside transport work.
- Remove the old mutating GET routes when the coordinator becomes the
  production state owner; do not preserve their response or switching behavior.

Exit gate: state/reducer tests pass, status snapshots are coherent, no lock is held across await, and `cargo check` plus `cargo test` pass.

### Phase 2: Pure Protocol Reducer

Status: completed and covered by deterministic reducer tests.

Owner: `plan_tv_multiview_daemon_refactor.md`.

- Implement design variables as typed state: commanded versus observed input, protocol phase, request/switch/verified/grant epochs, fallback intent, owners, signal observations, deadlines, and readiness.
- Implement transitions as pure `State + Event -> State + Effects` logic.
- Assert all required invariants after every transition in tests.
- Use an injected monotonic clock for deterministic deadline tests. Wall-clock time is only for logs.

Exit gate: counterexample tests cover stale observations, stale grants, readiness loss, disconnect races, fallback deferral, unexpected callbacks, and timer rearm/livelock.

### Phase 3: Persistent SSAP Actor

Status: completed and covered for scripted registration, subscription,
initial resynchronization, callback-before-response, delayed stale response,
ping handling, response timeout, bounded coordinator/effect queues, reconnect
backoff reset, and single-owner state transitions. Physical-TV reconnect and
resubscription. Physical failure timing remains blocked under the explicit
normal-use-first acceptance decision.

Owner: `plan_tv_multiview_daemon_refactor.md`.

- Add one actor that owns WebSocket connection state, SSAP IDs, registration, subscription, response correlation, deadlines, and the bounded priority queue.
- Keep the subprocess adapter only as a temporary comparison/rollback adapter; it cannot be the production completion authority.
- Publish read-only snapshots through a watch-style channel; HTTP handlers do not share mutable protocol locks.
- Coalesce duplicate observations and give fallback effects priority over ordinary requests.
- Retry transient failures indefinitely with bounded backoff and explicit fatal-error classification.

Exit gate: scripted transport tests prove correlation, reconnect/resubscribe/resync, fallback priority, one signal query in flight, and bounded queue behavior.

### Phase 4: lan-mouse Capability, Lease, and Capture Gate

Status: completed and covered by protocol, lease, release-before-notify,
crossing commit authorization, same-edge continuation, native controller
lifecycle, capture permit, Linux tests, native spoke builds, and live backend
startup. Exhaustive manual input scenarios remain blocked by the explicit
normal-use-first acceptance decision.

Owner: `plan_lan_mouse_atomic_input_gate.md`.

- Extend peer protocol and IPC state with keyboard readiness, pointer readiness, and a session epoch. Missing fields mean false.
- Add one bundle lease state machine on the hub; partial reservation is impossible.
- Replace fire-and-forget hook behavior with a native, bounded switch client integrated into capture control.
- Do not send `Enter` or input events before grant validation and atomic commit.
- On any failure, release local capture first, invalidate the lease, cancel the daemon request, and preserve server-host control.

Exit gate: unit/integration tests prove both capabilities move together and all pre-grant, timeout, disconnect, and restart paths remain local.

### Phase 5: Fenced HTTP and MultiView Protocol

Status: completed and verified by reducer and black-box HTTP tests; no legacy
mutating GET route or shell authorization path is retained.

Owner: daemon child plan with lan-mouse integration tests.

- Add typed POST create, GET poll, POST commit, internal cancel/readiness/renew operations, POST MultiView operations, `/status`, `/health`, and `/ready`.
- Make duplicate client request IDs idempotent and conflicting requests explicit `409` responses containing the active request ID.
- Route MultiView exit, remote loss, signal loss, SSAP loss, and manual unexpected callbacks through the same verified server fallback transaction.
- Do not register legacy mutating GET endpoints. Only the fenced POST API may
  create or commit switching work.

Exit gate: black-box API tests prove status code, body schema, idempotency, stale commit rejection, cancellation, and absence of legacy mutation routes.

### Phase 6: Coordinated Deployment Cutover

Status: completed. Revision `1923542` passed Linux, macOS, and Windows native
tests and release builds from the pinned bundle, was installed on all three
hosts, and started with matching peer identity. Windows initialized native
capture/emulation immediately. macOS failed closed while Accessibility was
absent, then initialized native capture/emulation after approval and a
LaunchAgent-only restart. macOS and Windows native task sequences executed in
parallel under one Ansible process. Each target binary contains its native
notification backend; `lan_mouse_server_host` selects the emitting instance
without an external notification executable.

Owner: main plan.

Cutover order is intentionally fail-closed and contains no legacy/observe-only
compatibility mode:

1. Build and test the exact pinned revision in each native-host cache; verify
   bounded persistent log paths before changing a live process.
2. Release any remote capture and force the TV plus input to a freshly observed
   `SERVER_HOST` baseline.
3. Install and restart the new daemon and hub together on `SERVER_HOST`. Until
   each spoke is replaced, missing capability/session/commit identity makes it
   ineligible for a lease.
4. Install and restart the macOS and Windows spokes from the same pinned
   revision. Require both capabilities, current session epoch, and exact commit
   identity before enabling each remote target.
5. Verify generated configs contain no `enter_hook` authorization and verify
   the daemon binary exposes no legacy mutating route.
6. Confirm correlated persistent logs and a freshly verified server fallback,
   then run the failure matrix before restoring unattended startup.

Never deploy a daemon that can issue grants to the old fire-and-forget hook client. Never roll back only one side of the protocol.

### Phase 7: Verification and Legacy Removal

Status: completed for automated verification, native builds, deployment,
persistent logs, TLA model checking, and legacy source removal. The exhaustive
physical failure matrix and p50/p95/p99 production timing measurements are
blocked by the explicit normal-use-first acceptance decision; incident traces
from ordinary use remain the acceptance source.

- Run `cargo check` after every Rust change and focused `cargo test` after every logic change in each affected repository.
- Run full tests for `tv-multiview`, lan-mouse core, protocol, IPC, capture, and emulation before cutover.
- Verify service units and Ansible rendering; validate all generated host configs before restart.
- Exercise all design failure rows, including target shutdown during command, readiness loss between observation and commit, stale delayed responses, TV reboot, SSAP disconnect, signal loss, daemon restart, hub restart, and Windows `no capacity available` equivalent.
- Reconstruct each test from persistent logs using request ID, request epoch, switch epoch, lease ID, and previous/next phase.
- Measure p50/p95/p99 command, observation, grant, commit, fallback, wake, and disconnect detection times. Set deadlines only from recorded data plus a documented margin.
- Remove the subprocess transport and obsolete Python daemon template as part
  of the coordinated cutover; no compatibility flag or legacy GET switching is
  implemented.

TLC execution was separately authorized and completed after this plan was
created. `tla/TvDisplaySwitchFinite.cfg` now checks the corrected finite model;
the bounded result and limitations are recorded in `tla/README.md` and the
design document. Rust transition and integration tests remain required because
bounded model checking is not proof of the implementation or unbounded spec.

## Verification Matrix

| Scenario | Required result |
|---|---|
| Target lacks pointer but has keyboard | No reservation, no TV command, both owners local. |
| Target goes offline after command ack | No grant or stale grant rejected; immediate local release; verified server fallback. |
| TV reports cached target signal | Observation epoch mismatch; no grant. |
| Grant response arrives after timeout | Epoch/lease mismatch; no capture. |
| Client commits twice | First current commit is idempotent; later/stale commit cannot alter ownership. |
| SSAP disconnects while remote owns input | Input local immediately; display state `fallback_deferred` until reconnect and fresh server observation. |
| Unexpected TV remote callback | Revoke remote grant/lease and converge through verified fallback. |
| MultiView off | Input local first, disable MultiView, command server input, observe fullscreen signal, then ready. |
| Queue saturated | Ordinary request receives bounded busy/unavailable response; fallback remains schedulable. |
| Log sink blocked | Switching and local release continue; bounded log loss is counted and reported. |
| Daemon restarts | Old request, epoch, grant, and lease cannot commit. Startup is not ready until resynchronized. |
| lan-mouse restarts | Capture is local; old active lease expires; daemon initiates fallback. |

## Rollback State

Rollback means fail closed, not return to the known unsafe GET-hook behavior:

- release both input paths to `SERVER_HOST`;
- disable remote capture clients or fence them at the hub;
- command and freshly verify server-host display when TV control is available;
- leave `fallback_deferred` visible when it is not;
- keep new logs and status endpoints available for diagnosis;
- roll back daemon and lan-mouse versions together only after protocol compatibility is verified.

## Completion Criteria

The objective is complete only when:

- every required invariant has an executable test and live failure-case evidence;
- the persistent SSAP actor is the only production TV transport;
- no shell hook can authorize capture;
- keyboard and pointer readiness, lease, grant, and owner are visible per host;
- all failures converge to local input plus freshly verified server display or an honest `fallback_deferred` state;
- deployment and rollback are repeatable across Linux, macOS, and Windows;
- the obsolete plans are marked historical or removed in a separate reviewed cleanup, and no compatibility path remains in production code.

Current completion assessment: source-level implementation and automated test
criteria are satisfied, but the objective is not production-complete. Exact-pin
macOS/Windows builds, the live cutover, native active-backend behavior, physical
TV failure matrix, forensic log reconstruction, and production percentile
measurements remain blocked on explicit coordinated-host authorization.
