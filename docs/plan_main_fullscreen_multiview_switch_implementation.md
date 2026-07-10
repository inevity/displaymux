# Main Plan: Fenced Fullscreen / MultiView Switch Implementation

## Plan Control

- Status: proposed; implementation has not started.
- Design baseline: `fullscreenmultiviewswitchdesign.md` at commit `319b762`.
- Rust daemon baseline: current clean `tv-multiview` crate at commit `319b762`.
- lan-mouse baseline: local clean checkout `../../lan-mouse` at commit `392af44`, matching `lan_mouse_release_tag: main-392af44`.
- Controlling objective: implement the approved design without allowing keyboard and pointer ownership to split, and without claiming server-host fallback before the TV state is freshly observed.
- This plan supersedes `fullscreenmultiviewswitchplan.md` and `fullscreenmultiviewswitchplanrust.md` for new work. Those files remain historical artifacts.

Linked child plans:

- [tv-multiview daemon refactor and protocol implementation](plan_tv_multiview_daemon_refactor.md)
- [lan-mouse atomic input gate and lease integration](plan_lan_mouse_atomic_input_gate.md)

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

## Current InitState Evidence

The current code is not a partial implementation of the new protocol; it implements an older protocol with different safety semantics.

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
2. Conservative baseline: immediately release the first edge capture, perform the fenced TV request while input remains local, arm the returned grant, and commit it on the next edge crossing. This costs a second crossing but satisfies the safety invariant.

Keeping an exclusive capture active while waiting, suppressing events, or sending repeated `Enter` packets is not an acceptable interpretation of local ownership. If the preferred path is not implementable across the active Linux backend, select the conservative baseline and record the UX tradeoff in the design before code proceeds.

Gate evidence:

- A focused test proves no `ProtoEvent::Enter` or `ProtoEvent::Input` is emitted before grant commit.
- A manual backend test proves pointer buttons, motion, scroll, and keyboard remain usable on `SERVER_HOST` while pending and after every denial/failure.
- Cancellation and timeout release any backend capture without waiting for the TV daemon.

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

Owner: both child plans.

- Record current crate and lan-mouse commit IDs in test output and deployment metadata.
- Add characterization tests before changing semantics: current state transitions, route status codes, subprocess error mapping, capture/hook ordering, and peer availability propagation.
- Introduce fake TV and fake switch-controller seams using native Rust async traits or typed channels; do not add `async-trait`.
- Record current subprocess CPU, process count, command latency, and end-to-end switch latency with exact commands and workload.
- Confirm the LG client-key storage schema and SSAP message/URI shapes from local source or authoritative protocol documentation before selecting dependencies.

Exit gate: tests describe current behavior, known unsafe behavior is named rather than encoded as desired behavior, and no production behavior has changed.

### Phase 1: Mechanical Daemon Refactor

Owner: `plan_tv_multiview_daemon_refactor.md`.

- Separate configuration, domain types, I/O adapters, HTTP adapters, and transition logic.
- Replace independently locked fields with one coherent state owner and immutable snapshots.
- Remove the lock-order cycle in a dedicated correctness commit, not hidden inside transport work.
- Preserve legacy endpoint behavior temporarily so refactoring failures are separable from protocol changes.

Exit gate: legacy behavior tests pass, status snapshots are coherent, no lock is held across await, and `cargo check` plus `cargo test` pass.

### Phase 2: Pure Protocol Reducer

Owner: `plan_tv_multiview_daemon_refactor.md`.

- Implement design variables as typed state: commanded versus observed input, protocol phase, request/switch/verified/grant epochs, fallback intent, owners, signal observations, deadlines, and readiness.
- Implement transitions as pure `State + Event -> State + Effects` logic.
- Assert all required invariants after every transition in tests.
- Use an injected monotonic clock for deterministic deadline tests. Wall-clock time is only for logs.

Exit gate: counterexample tests cover stale observations, stale grants, readiness loss, disconnect races, fallback deferral, unexpected callbacks, and timer rearm/livelock.

### Phase 3: Persistent SSAP Actor

Owner: `plan_tv_multiview_daemon_refactor.md`.

- Add one actor that owns WebSocket connection state, SSAP IDs, registration, subscription, response correlation, deadlines, and the bounded priority queue.
- Keep the subprocess adapter only as a temporary comparison/rollback adapter; it cannot be the production completion authority.
- Publish read-only snapshots through a watch-style channel; HTTP handlers do not share mutable protocol locks.
- Coalesce duplicate observations and give fallback effects priority over ordinary requests.
- Retry transient failures indefinitely with bounded backoff and explicit fatal-error classification.

Exit gate: scripted transport tests prove correlation, reconnect/resubscribe/resync, fallback priority, one signal query in flight, and bounded queue behavior.

### Phase 4: lan-mouse Capability, Lease, and Capture Gate

Owner: `plan_lan_mouse_atomic_input_gate.md`.

- Extend peer protocol and IPC state with keyboard readiness, pointer readiness, and a session epoch. Missing fields mean false.
- Add one bundle lease state machine on the hub; partial reservation is impossible.
- Replace fire-and-forget hook behavior with a native, bounded switch client integrated into capture control.
- Do not send `Enter` or input events before grant validation and atomic commit.
- On any failure, release local capture first, invalidate the lease, cancel the daemon request, and preserve server-host control.

Exit gate: unit/integration tests prove both capabilities move together and all pre-grant, timeout, disconnect, and restart paths remain local.

### Phase 5: Fenced HTTP and MultiView Protocol

Owner: daemon child plan with lan-mouse integration tests.

- Add typed POST create, GET poll, POST commit, internal cancel/readiness/renew operations, POST MultiView operations, `/status`, `/health`, and `/ready`.
- Make duplicate client request IDs idempotent and conflicting requests explicit `409` responses containing the active request ID.
- Route MultiView exit, remote loss, signal loss, SSAP loss, and manual unexpected callbacks through the same verified server fallback transaction.
- Keep legacy GET endpoints behind an explicit compatibility switch during staging. They must not be enabled at final cutover.

Exit gate: black-box API tests prove status code, body schema, idempotency, stale commit rejection, cancellation, and fail-closed legacy behavior.

### Phase 6: Coordinated Deployment Cutover

Owner: main plan.

Cutover order is intentionally fail-closed:

1. Add persistent Windows stdout/stderr files and rotation; verify Linux journal and macOS LaunchAgent logs.
2. Deploy capability-reporting lan-mouse builds to both spokes. Keep old capture behavior disabled from using the new protocol.
3. Deploy the new daemon with fenced endpoints available but remote grant issuance disabled.
4. Deploy the new hub with capture gate in observe-only mode; verify per-host keyboard and pointer readiness and session epochs.
5. Force the TV and input to verified `SERVER_HOST` baseline.
6. Enable fenced switching on the daemon and hub in one maintenance action.
7. Remove `enter_hook = curl ...` from generated configs and disable legacy mutating GET routes.
8. Run the failure matrix before restoring unattended startup.

Never deploy a daemon that can issue grants to the old fire-and-forget hook client. Never roll back only one side of the protocol.

### Phase 7: Verification and Legacy Removal

- Run `cargo check` after every Rust change and focused `cargo test` after every logic change in each affected repository.
- Run full tests for `tv-multiview`, lan-mouse core, protocol, IPC, capture, and emulation before cutover.
- Verify service units and Ansible rendering; validate all generated host configs before restart.
- Exercise all design failure rows, including target shutdown during command, readiness loss between observation and commit, stale delayed responses, TV reboot, SSAP disconnect, signal loss, daemon restart, hub restart, and Windows `no capacity available` equivalent.
- Reconstruct each test from persistent logs using request ID, request epoch, switch epoch, lease ID, and previous/next phase.
- Measure p50/p95/p99 command, observation, grant, commit, fallback, wake, and disconnect detection times. Set deadlines only from recorded data plus a documented margin.
- Remove subprocess transport, legacy GET switching, compatibility flags, and obsolete Python daemon template only after the new path passes the complete matrix and rollback drill.

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
- the obsolete plans and compatibility paths are marked historical or removed in a separate reviewed cleanup.
