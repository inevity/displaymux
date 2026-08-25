# Child Plan: lan-mouse Atomic Input Gate and Lease Integration

Parent: [Main fenced switch implementation plan](plan_main_fullscreen_multiview_switch_implementation.md)

Status (2026-07-14): completed at
`7cc0f680768dc9b3ce479e0fb19d486c65ceb9a9`. The accidental-edge correction,
native rebuilding, three-host deployment, service startup, and persistent-log
checks passed. Normal use is the selected runtime acceptance path.

## Objective

Change lan-mouse from a fire-and-forget TV hook producer into the authority that reserves, commits, renews, and releases keyboard plus pointer ownership as one bundle. The TV daemon can issue a grant, but only lan-mouse can change the data plane.

## Implementation Evidence

- No-GTK workspace check/test and Linux production-feature check/test pass.
  Coverage includes input-capture queues and same-edge resume, 39 core
  capture/readiness/lease/release/controller tests, bounded logging, CLI
  status, protocol wire behavior, server-role notification classification,
  and native macOS/Windows display-center arithmetic.
- The first crossing completes backend release before publishing its candidate.
  A targeted, current, single-use permit then requires a service commit reply
  before a still-focused same-edge continuation or a later matching crossing
  can emit `Enter`. Commit validates request, lease, grant, handle, peer
  session, and deadline as one bundle.
- Peer readiness names keyboard and pointer capability plus process session
  epoch. Unknown, stale, partial, disconnected, or commit-mismatched peers fail
  closed.
- Native tests and release builds succeeded on Linux, macOS, and Windows for
  revision `1923542` from one exact git bundle and lockfile vendor archive, and
  that revision is installed on all three hosts. It includes revision fencing,
  release-complete and commit-authorization ordering, same-edge continuation,
  receiver-side center-before-ack behavior, native-library server-role failure
  notifications, and preservation of controller verification causes.
- Revision `7cc0f68` adds the pre-controller edge-intent gate. Local and full
  workspace checks pass; the exact pinned revision also passed Linux package,
  macOS debug, and Windows release native test/build/deploy sequences. The
  macOS and Windows sequences ran concurrently and all host recaps completed
  with zero failures or unreachable hosts.
- lan-mouse log production is non-blocking and bounded to 1,024 records of
  16 KiB, and reports accumulated drops when its persistent sink recovers.

## Verified Current Behavior

At plan creation, the deployed release tag and clean baseline checkout were
`main-392af44` and commit `392af44`. The evidence below is retained as the
historical unsafe `InitState`; it is not the current source implementation.

- `src/capture.rs:348-354` sets `WaitingForAck`, installs `active_client`, and emits `ClientEntered` as soon as capture begins.
- `src/capture.rs:357-368` immediately converts the same event to `ProtoEvent::Enter` and sends it.
- `src/service.rs:350-352` responds to `ClientEntered` by spawning the hook.
- `src/service.rs:585-608` runs the shell hook in an independent local task and only logs its eventual exit status.
- `lan-mouse-proto/src/lib.rs:61-74` represents availability as `Pong(bool)` and version identity as `Hello { commit }`.
- `lan-mouse-ipc/src/lib.rs:159-184` stores one `alive` boolean and no keyboard/pointer capability or session epoch.

Therefore an enter hook cannot authorize capture, and current availability cannot prove `RemoteReadyForControl`.

## Required State

Add explicit state, with names adjusted to local conventions:

```text
PeerReadiness {
  online,
  keyboard_ready,
  pointer_ready,
  session_epoch,
  last_update,
}

BundleLease {
  lease_id,
  target,
  request_epoch,
  peer_session_epoch,
  expires_at,
}

CaptureGateState =
  Local
  | Preparing { target, client_request_id, lease }
  | GrantArmed { target, request_id, grant_epoch, lease, expires_at }
  | RemoteOwned { target, request_id, lease }
  | Releasing { reason }
```

Invariant checks:

- a lease exists for both keyboard and pointer or for neither;
- `Preparing` and `GrantArmed` emit no remote `Enter` or input event;
- `RemoteOwned` requires current peer session, both capabilities, unexpired lease, and matching grant;
- every failure transition reaches `Local` and releases backend capture without daemon cooperation;
- process restart starts `Local` with a new session epoch and no reusable lease.

## L0: Capture-Gate Feasibility

Status: completed for the 2026-07-14 refinement. The first tap is release-only;
backend retreat evidence and a matching second tap are required before any
controller request, and post-grant continuation requires same-edge proof.

Use Main Plan Gate 0 before broad implementation.

Preferred one-crossing path:

- add a backend-neutral pre-capture candidate event;
- leave local keyboard and pointer delivery intact while the daemon request runs;
- enter exclusive capture only after a grant is armed and revalidated.

Conservative safe path if pre-capture is not portable:

- on the first `CaptureEvent::Begin`, immediately release capture and prime a
  bounded intent without emitting a controller candidate;
- require native retreat evidence for that same edge, then a second matching
  `Begin` for the same handle, target, and peer session before the deadline;
- only after that confirmation run one fenced request while the user remains local;
- store an expiring grant without switching ownership;
- if the same edge remains focused, resume that crossing after the grant;
  otherwise, on the next matching crossing, revalidate grant plus lease,
  atomically enter remote capture, and commit the same request;
- if the grant expires first, cancel it and restore/verify server display.

Do not retain current `active_client`/`WaitingForAck` behavior while the TV transaction is pending. Do not buffer keyboard or pointer events for later remote replay.

Decision (2026-07-11, refined 2026-07-14): implement the conservative safe path. The shared
capture abstraction has no pre-capture edge notification; all native backends
surface `CaptureEvent::Begin` after exclusive capture begins, while all expose
the same asynchronous `release()` operation. The first `Begin` therefore
releases immediately and primes only local intent. Layer-shell `Leave` or
native macOS/Windows inward motion rearms the intent. The input-capture portal
does not expose post-release local motion and must therefore fail closed rather
than infer retreat from its release cursor offset. The second matching `Begin`
creates the fenced request. Only its valid grant may enable same-focused-edge
continuation or a later matching crossing. This preserves one state machine
across Linux, macOS, and Windows while making backend evidence capability
explicit; a server must select a backend that can prove retreat before edge
switching is enabled.

Exit gate result: automated event-order tests, native workspace tests, builds,
and service startup pass. Runtime pointer behavior is accepted through normal
use rather than a separate scripted physical test matrix.

## L1: Capability-Aware Peer Protocol

Status: completed and covered for both/partial/unknown readiness, session
replacement, disconnect clearing, and wire round trips. Existing event numbers
remain stable only to avoid protocol misparsing; old peers are never considered
ready and no legacy authorization behavior exists.

- Add a forward-compatible peer event carrying keyboard readiness, pointer readiness, and peer session epoch.
- Preserve old event numeric values; older peers may ignore the new event but must not be interpreted as ready.
- Generate a new session epoch on every spoke process start and emulation backend recreation.
- Derive readiness from actual initialized backend capabilities, not only process/network liveness.
- Publish readiness changes immediately when either capability is lost or restored.
- Extend `ClientState` and frontend events with named capabilities and session epoch.
- Keep `alive` only as non-authoritative telemetry; stop using it as bundle readiness.

Likely source ownership:

- `lan-mouse-proto/src/lib.rs`: wire event and encode/decode tests;
- `input-emulation/*`: report actual keyboard/pointer capability lifecycle;
- `src/emulation.rs`: convert backend lifecycle into peer readiness events;
- `src/connect.rs`: receive/store peer readiness and clear it on disconnect;
- `lan-mouse-ipc/src/lib.rs`: expose readiness fields;
- `src/client.rs`: store per-client readiness and session epoch.

Tests:

- keyboard-only, pointer-only, both, neither;
- old peer sends no capability event;
- process restart changes session epoch;
- out-of-order old readiness event cannot revive a new session;
- disconnect clears both capabilities immediately.

## L2: Bundle Lease Manager

Status: completed with deterministic reservation, exclusion, expiry, renewal,
stale-session, and atomic commit tests.

- Implement one hub-local lease manager; it is the authority for reserving both paths.
- A reservation captures target, daemon client request identity, local request epoch, peer session epoch, and monotonic expiry.
- Reject partial capability, stale session, existing conflicting lease, or missing active connection before contacting the TV daemon.
- Revalidate before accepting a grant and immediately before capture commit.
- Convert the reservation into an active session lease at commit.
- Renew only while peer readiness and connection session still match.
- On expiry, disconnect, capability loss, process shutdown, or daemon cancellation: atomically invalidate the bundle and release capture.
- Persist no active lease across process restart.

Use one monotonic timer owner rather than one task per lease. Lease IDs must be unpredictable enough to prevent accidental collision, but local correctness depends on epoch and state matching rather than secrecy alone.

## L3: Native Switch Client

Status: completed. The native bounded client owns create/poll/commit/cancel and
renewal lifecycles; a local HTTP lifecycle test covers every operation, ambient
proxies are bypassed for the controller boundary, and shell/curl authorization
is absent.

Replace shell-command authorization with one bounded native client owned by the service:

- one request lifecycle per capture candidate;
- explicit connect/request/poll/commit/cancel deadlines;
- no new process per poll;
- no unbounded retry task;
- typed parsing of pending, grant, denied, fallback, and stale responses;
- duplicate create/poll/commit handled idempotently;
- cancellation token tied to capture release, peer loss, config change, and shutdown;
- structured logs with daemon request ID, request epoch, lease ID, peer session epoch, and gate state.

Select the HTTP or IPC client implementation only after checking the existing workspace dependency policy. Use native Rust async traits for test adapters; do not use `async-trait`.

For return to `SERVER_HOST`, release remote capture first even if the TV daemon is unreachable. Display recovery is then requested and may remain `fallback_deferred`; daemon failure must never keep input captured remotely.

## L4: Capture State-Machine Integration

Status: completed. The pre-controller intent gate, target/epoch permits,
release completion, explicit commit authorization, native same-edge
continuation, single-use commit, and delayed-command rejection are implemented
and verified.

Refactor event ownership before adding network behavior:

- `capture.rs` reports the first released edge and retreat evidence without
  installing `active_client`, sending `Enter`, or starting controller work;
  only the second matching rearmed edge becomes a candidate.
- `service.rs` owns one gate state and starts the bounded switch client request.
- only a validated `GrantArmed` command may let capture install the target and emit `Enter`.
- both keyboard and pointer forwarding become enabled by the same state transition.
- no input event is queued while preparing; events remain local under the selected Gate 0 refinement.
- remote `Ack` cannot bypass the TV grant because no `Enter` is sent before grant.
- the receiver centers its native pointer on the current display before
  acknowledging `Enter`; failed centering withholds `Ack` and fails closed.
- release bind, incoming-device entry, daemon denial, timeout, and connection failure all cancel the same gate and lease.
- repeated edge events while preparing attach to or reject against the active request; they do not create new tasks.

Add explicit events between service and capture task, for example candidate, grant, deny, cancel, and release-complete. Each event carries gate/request epoch so delayed messages are harmless.

## L5: Commit, Renewal, and Failure Feedback

Status: completed for source logic, automated tests, native builds, and live
backend startup. The macOS permission-loss path was observed to fail closed to
the dummy backend and recovered after Accessibility approval plus LaunchAgent
restart. Other physical failure timing cases are blocked by the explicit
normal-use-first acceptance decision.

- At capture commit, revalidate peer capabilities, peer session, request epoch, grant epoch, lease ID, and expiry in one service-loop transition.
- Enable keyboard and pointer forwarding together and notify the daemon commit endpoint.
- Treat commit notification failure as unresolved: release capture and request/cause fallback rather than assuming the daemon recorded ownership.
- Send immediate cancellation/readiness loss to the daemon when a pending or active target loses either capability.
- Renew the active session lease through the selected bounded control channel.
- If renewal acknowledgement or daemon health is absent past its deadline, release locally first and let display recovery proceed independently.
- A stale daemon grant can be logged but cannot alter gate or lease state.

## L6: Configuration and Deployment

Status: completed. Generated fenced configuration, exact-revision native
build/test, bounded macOS/Windows log wrappers, installation, service restart,
and persistent log verification passed for revision `7cc0f68` on all hosts.
The macOS and Windows task sequences run concurrently under one Ansible
`strategy: free` play, and their native test/build commands use bounded async
polling. Each native build links the platform notification implementation into
lan-mouse; deployment has no notification-command or runtime-package
prerequisite. The `controller` entry in `displaymux_host_assignments` selects
the sole emitting instance.

- Replace generated `enter_hook = "curl ..."` authorization with explicit switch-controller configuration understood by the patched lan-mouse build.
- Remove shell-hook authorization entirely when fenced capture is enabled; no compatibility option is implemented.
- Include daemon address, protocol version, server host, request timeout, and lease timing in validated config generated by Ansible.
- Build Linux, macOS, and Windows from one pinned patched commit; capability-unknown or revision-mismatched peers remain not ready.
- Add Windows scheduled-task stdout/stderr redirection to fixed `%LOCALAPPDATA%\lan-mouse\` files and implement bounded rotation.
- Keep macOS LaunchAgent paths and Linux journald source; add request/lease fields to all platform logs.

### Native Non-GTK Build Matrix

The root package's default feature set contains GTK plus Linux input backends.
Windows and macOS native backends are target-selected and do not need Cargo
features, but Linux would fall back to dummy capture/emulation if all features
were disabled. Build on each managed target OS; the current repository has no
supported Linux-to-Windows/macOS cross-build configuration.

Linux hub, preserving every current non-GTK backend:

```sh
cargo build --frozen --release --bin lan-mouse \
  --no-default-features \
  -F layer_shell_capture,libei_capture,x11_capture,wlroots_emulation,libei_emulation,x11_emulation,rdp_emulation
```

macOS spoke, using its native capture/emulation implementation:

```sh
cargo build --frozen --profile dev --bin lan-mouse --no-default-features
```

Windows spoke, using its native capture/emulation implementation:

```powershell
cargo build --frozen --release --bin lan-mouse --no-default-features
```

The Ansible implementation must:

1. Define one `lan_mouse_repo_url` and immutable `lan_mouse_revision` used by
   all three host plays.
2. Check out that revision into a persistent per-host build cache and build
   only when the recorded revision or build inputs change.
3. Run the Linux build on Linux, the macOS build on macOS, and the Windows
   build on Windows. Native Rust plus the platform linker/SDK are prerequisites;
   GTK, libadwaita, `gvsbuild`, and GTK runtime DLL collection are not.
4. Keep the existing Linux PKGBUILD feature list, but point its source at the
   patched repository/revision instead of the upstream release tag.
5. Install macOS `target/debug/lan-mouse` at a stable daemon path and update
   the LaunchAgent. A changed executable may require Accessibility permission
   to be granted again.
6. Install only Windows `target\release\lan-mouse.exe` into the existing
   program directory; the non-GTK build does not require the current GTK DLL
   archive.
7. Pin a locally checked commit, run platform-appropriate `cargo test` with the
   same feature selection before installation, record the installed build
   identity only after native build/test success, then restart the service and
   verify the peer-reported commit/capability version.

Rollout handshake:

- build and test the exact revision on every native host before changing a live
  service;
- force input and display to a freshly verified `SERVER_HOST` baseline;
- replace and restart the daemon plus hub together; old spokes then lack the
  required capability/session/commit identity and fail closed;
- replace and restart each spoke from the same pinned revision;
- require both capabilities, current session epoch, and exact commit identity
  before that host can receive a bundle lease;
- confirm generated configuration contains no hook authorization and the daemon
  exposes no legacy mutation route;
- fail closed by deactivating remote clients on any mixed-version detection.

## lan-mouse Test Matrix

| Sequence | Required result |
|---|---|
| Edge crossing with old peer | Capture remains local; no daemon request that can grant. |
| Edge crossing with pointer false | No bundle lease; no TV command. |
| Capability loss during pending request | Lease cancelled; capture local; daemon cancellation sent. |
| Capability loss after grant before crossing/commit | Grant rejected locally; fallback requested. |
| Peer session changes with same address | Old lease invalid; no event forwarding. |
| Daemon returns delayed old grant | Epoch mismatch; no capture. |
| Daemon unreachable | Immediate local behavior; bounded error; no retry task leak. |
| Hook/client task crashes | Capture release is independent and guaranteed. |
| Return from remote while daemon is down | Remote capture releases; input returns to server; display recovery remains pending. |
| Keyboard event during preparing | Delivered locally under chosen gate refinement; never buffered/sent remote. |
| Pointer button/scroll during preparing | Delivered locally; never buffered/sent remote. |
| Concurrent left/right edge candidates | One gate/lease winner; other remains local with explicit busy state. |
| Hub restart while remote active | Backend releases; new session has no lease; daemon falls back. |

## Commit Boundaries

1. Protocol capability types plus encode/decode tests.
2. Emulation readiness reporting and IPC exposure.
3. Pure bundle lease state machine and tests.
4. Capture candidate/grant event refactor with fake controller.
5. Explicit safety fix that prevents pre-grant `Enter`/input emission.
6. Native daemon client and cancellation/renewal lifecycle.
7. Coordinated config/deployment templates and persistent Windows logs.
8. Remove the shell authorization path before deployment; no compatibility
   implementation is retained during cutover.

Do not combine backend refactoring, protocol wire changes, TV behavior, and deployment changes in one commit.

## Completion Gate

- `cargo check` passes after every Rust change and relevant focused tests pass after each logic change.
- Full lan-mouse workspace tests pass on the hub build and platform-specific capture/emulation tests pass on each host.
- No `ProtoEvent::Enter` or input event can precede grant validation.
- Keyboard and pointer capabilities, lease, and owner always transition together.
- Local release does not depend on the daemon, TV, network, shell, or log sink.
- Mixed or old versions fail closed.
- Persistent logs on all three hosts reconstruct create, grant, commit, renewal, release, and fallback using shared identities.

Current gate assessment: source, automated, native build, deployment, managed
service startup, and persistent log-source gates pass for `7cc0f68`. Runtime
interaction acceptance proceeds through normal use; physical incident traces
will be evaluated if ordinary use exposes a failure.
