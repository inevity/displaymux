# Child Plan: tv-multiview Daemon Refactor and Protocol Implementation

Parent: [Main fenced switch implementation plan](plan_main_fullscreen_multiview_switch_implementation.md)

Status (2026-07-13): source implementation, automated verification, and live
deployment are complete through parent commit `97d31e1`. Service startup,
synchronized readiness, and spoke reconnects were observed. The exhaustive TV
reconnect, signal-loss, and latency matrix is blocked by the explicit
normal-use-first acceptance decision recorded in the main plan.

## Objective

Transform the current four-module Rust daemon into a bounded, testable protocol service with one persistent SSAP owner. Keep mechanical refactoring, correctness fixes, and new behavior in separate commits so regressions can be localized.

## Implementation Evidence

- `cargo check --frozen`, all 60 tv-multiview tests, and strict all-target
  clippy pass.
- One coherent coordinator owns transitions and publishes immutable snapshots;
  ordinary and safety command/effect lanes are independently bounded.
- The persistent SSAP actor owns registration, subscription, request
  correlation, observations, and reconnect backoff. Scripted sockets verify
  registration/subscription/reconnect/resync, grant-pending callback and stale
  response ordering, ping/keepalive timeout, and successful reconnect-state
  reset.
- Typed authenticated create/poll/commit/cancel/renew/readiness/MultiView APIs,
  `/status`, `/health`, and `/ready` are implemented; legacy mutating GET routes
  are absent.
- Structured logging is non-blocking and bounded to 1,024 records of 16 KiB;
  queue/drop/reconnect metrics and bounded retained-request occupancy are
  exposed in status and SIGUSR1 state dumps.

## Current Module Map

| File | Current responsibility | Refactoring pressure |
|---|---|---|
| `src/main.rs` | hardcoded config, process lifecycle, reconnect loop, HTTP startup | Configuration, lifecycle, and TV health are coupled; subprocess heartbeat defines health. |
| `src/state.rs` | domain types, eight independent synchronization fields, transitions, status JSON, unit tests | Lock order is inconsistent; command intent is stored as observed state; transition methods cannot emit typed effects. |
| `src/http.rs` | route parsing, state guards, transition selection, TV I/O, rollback, counters | Business logic and I/O are interleaved; plain text hides request identity and denial reason. |
| `src/tv.rs` | one subprocess per TV operation | No persistent connection, subscription, correlation, signal observation, timeout, or backpressure. |

## Target Layout

The exact names may follow local conventions, but ownership must remain clear:

```text
src/
  main.rs             process startup and shutdown only
  config.rs           validated server host, TV, HDMI, deadlines, queue bounds
  domain.rs           Host, modes, phases, epochs, leases, observations, snapshots
  protocol.rs         pure State + Event -> State + Effects reducer
  coordinator.rs      single protocol owner, timers, effect scheduling, snapshots
  http.rs             typed axum request/response adapter
  ssap/
    mod.rs            actor handle and commands
    codec.rs          SSAP request/response/subscription message types
    transport.rs      persistent WebSocket lifecycle
    key_store.rs      LG client-key loading with secret-safe errors
  ssap/transport.rs   only production TV transport; no subprocess fallback
```

Do not introduce an abstraction solely to match this tree. Combine files when ownership stays unambiguous and tests remain focused.

## D0: Characterization and Test Seams

Status: completed for retained characterization evidence. Source behavior and
test seams were captured. The historical subprocess CPU/process/latency
workload is blocked because it was not measured before replacement; running
that obsolete path against the physical TV requires separate authorization.

- Add route-level tests for `/health`, `/status`, the fenced enter API, and MultiView routes.
- Introduce a fakeable TV control seam with a native async trait and generic concrete state, or a typed command handle. Do not use the `async-trait` crate.
- Characterize subprocess success, non-zero exit, spawn failure, and hung-command timeout behavior.
- Add a deterministic regression for the mode/pending lock-order cycle before fixing it. The test must use a bounded timeout and must not leave a blocked test worker behind.
- Record and then replace tests that encode obsolete behavior, especially target no-op, immediate `switch_complete`, reconnect exit at 30, and Linux-specific fallback. Obsolete contracts are not retained as passing compatibility tests.

Verification: `cargo check`; `cargo test`; no endpoint or production default changes.

Commit boundary: tests and injection seams only.

## D1: Coherent Legacy State Refactor

Status: completed. Production state now has one owner and no legacy mutation
adapter remains.

- Introduce a single aggregate state value guarded by one synchronization boundary.
- Move counters and last error into coherent snapshots; keep monotonic counters separate only when their atomic independence is intentional.
- Make transition methods return typed decisions/effects rather than requiring handlers to re-read fields.
- Parse target strings through `FromStr` with typed rejection instead of mapping unknown values into a valid `Input::Unknown` command target.
- Add `ServerHost` and HDMI mapping to validated configuration while preserving current defaults.
- Move all config constants out of `main.rs`; make Ansible values authoritative at deployment.
- Remove old mutating GET routes instead of preserving their behavior.

Verification: characterization tests remain green; one snapshot cannot contain fields from different transitions; lock-order regression passes.

Commit boundaries:

1. Mechanical aggregate-state move.
2. Explicit lock-order and coherent-snapshot fix.
3. Configuration extraction with identical defaults.

## D2: Pure Protocol Core

Status: completed with deterministic invariant and counterexample tests.

Define typed state equivalent to the approved model:

- `Host` and configured `server_host`;
- `TvMode`, `ProtocolPhase`, `WsState`;
- `commanded_input` separate from `observed_input`;
- `request_id`, `request_epoch`, `switch_epoch`, `verified_epoch`, `grant_epoch`;
- `PendingRequest`, `BundleLease`, `Grant`, and active-session lease;
- keyboard and pointer owners as separate fields checked for equality;
- per-host online, keyboard-ready, pointer-ready, and session epoch;
- per-input signal observation with source epoch and monotonic observation time;
- fallback intent and cause;
- command, observation, grant, wake, renewal, and fallback deadlines.

Implement a pure reducer:

```rust
fn apply(state: &ProtocolState, event: Event, now: MonoTime)
    -> Result<Transition, ProtocolError>;

struct Transition {
    next: ProtocolState,
    effects: Vec<Effect>,
}
```

The reducer never performs I/O, locks, sleeps, reads wall time, spawns tasks, or mutates global counters. Effects include SSAP command, fresh observation request, grant publication, fallback scheduling, wake request, lease cancellation, and structured transition log.

Required event families:

- startup/reconnect/register/subscribe/resync/disconnect;
- create/poll/cancel/commit enter request;
- TV command ack/error and correlated observation;
- subscription callback and manual override;
- host online/offline and capability/session update;
- lease reserved/lost/renewed/expired;
- switch, grant, wake, signal-loss, and fallback deadlines;
- MultiView on/off command and observation;
- graceful shutdown/restart epoch invalidation.

After every successful transition, test:

- owner equality;
- remote ownership requires matching valid lease and readiness;
- remote fullscreen ownership requires fresh matching display;
- fallback intent implies local ownership;
- grant identity and verification epochs match;
- no pending request when control is unavailable except explicit waking/fallback-deferred states;
- deadlines cannot move forward without a new epoch;
- observed state changes only on observation events.

Commit boundary: domain and reducer only; no production route uses it yet.

## D3: Coordinator Actor and Snapshot Publication

Status: completed with bounded ordinary/safety command and effect lanes,
coherent watch snapshots, and queue-depth status metrics.

- Add a bounded command channel and a cheap cloneable handle for HTTP and lan-mouse callers.
- The coordinator owns `ProtocolState`; no `Arc<Mutex<ProtocolState>>` is exposed.
- Use oneshot replies for commands and a watch-style immutable status snapshot for reads.
- Drive timers from one monotonic deadline scheduler rather than one spawned task per request.
- Execute effects through bounded adapters and feed correlated results back as events.
- Reserve capacity for safety events or use separate bounded priority lanes so fallback, disconnect, cancellation, and lease loss cannot be starved by ordinary requests.
- Reject overload explicitly; do not spawn unbounded work as backpressure relief.
- On shutdown, first publish not-ready/local ownership intent, invalidate grants, close SSAP, and drain only bounded essential work.

Verification:

- concurrent create requests produce one winner and explicit busy responses;
- queue saturation still accepts or internally prioritizes fallback;
- dropped HTTP responders do not cancel required fallback;
- no state lock exists across await;
- status reads cannot block protocol progress.

Commit boundary: actor runs against fake effect adapters.

## D4: Persistent SSAP Transport

Status: completed and verified for codec/key parsing, scripted registration,
subscription, disconnect/reconnect/resubscribe and resync,
callback-before-response, stale response discard, ping/timeout handling,
backoff reset, bounded coordinator integration, and deployed synchronized
startup. Exhaustive physical-TV timing remains blocked by the explicit
normal-use-first acceptance decision.

Before dependency changes, verify the current LG client-key schema and the authoritative SSAP URI/payloads. Add only necessary dependencies to `Cargo.toml`.

Actor requirements:

- exactly one task owns the WebSocket stream and request ID allocator;
- bounded outbound commands with fallback priority;
- map each response ID to one expected operation and epoch;
- parse subscription callbacks independently from command responses;
- register with the persisted client key without logging key material;
- subscribe to MultiView status, query active input and signal, then publish healthy only after initial synchronization;
- use WebSocket ping/pong for transport health;
- reconnect with bounded exponential backoff indefinitely for transient errors;
- classify invalid local config, rejected credentials, and incompatible protocol separately as fatal;
- reset retry counters only after registration, subscription, and resync;
- perform at most one signal query in flight and coalesce duplicate poll requests;
- apply an explicit timeout to connect, write, response, subscription, and close operations.

Remove the subprocess transport when the SSAP actor is enabled. There is no
production shadow or fallback path, and a subprocess success can never generate
a verified observation event.

Scripted transport tests:

- out-of-order response IDs;
- delayed old response after a new epoch;
- command ack followed by wrong input;
- command ack followed by no signal;
- callback before command response;
- unexpected callback during grant pending;
- disconnect during remote ownership and during fallback;
- reconnect, resubscribe, and initial state resync;
- queue saturation and coalesced poll;
- ping timeout without an application heartbeat subprocess.

Commit boundaries:

1. Codec and key-store parsing.
2. Scripted transport and connection lifecycle.
3. Coordinator integration behind a disabled production flag.
4. Shadow comparison and removal of subprocess heartbeat.

## D5: Fenced HTTP API

Status: completed and covered by authenticated black-box route tests, including
typed create/poll/commit/cancel/renew/readiness/MultiView behavior, malformed
input, idempotency, busy/stale conflicts, status, and absence of legacy GET
mutation.

Implement typed serde request/response enums. Do not return mode strings as authorization.

Public routes from the design:

- `POST /enter/{target}`;
- `GET /enter/request/{id}`;
- `POST /enter/request/{id}/commit`;
- `POST /multiview/on`;
- `POST /multiview/off`;
- `GET /status`;
- `GET /health`;
- `GET /ready`.

Document and implement the internal lease/readiness operations selected in Main Plan Gate 0. Every mutating request carries a client identity and idempotency identity. Response states are typed at minimum as waking, pending, grant, committed, denied, cancelled, fallback, and expired.

HTTP rules:

- malformed host/request/epoch/lease is `400`;
- daemon not synchronized or unresolved fallback is `503`;
- conflicting active request is `409` with active request ID;
- missing request is `404`;
- expired/stale grant or lease is a typed conflict, never success;
- duplicate current create/commit is idempotent;
- legacy GET mutation routes are never registered;
- `/health` is process liveness; `/ready` is protocol readiness.

Handlers parse, send one actor command, await one bounded response, and format it. They do not choose transitions, touch SSAP, or mutate counters.

## D6: Fallback, Signal, Wake, and MultiView Effects

Status: completed with reducer coverage for local-first release, stale and
transient observation handling, readiness loss, duplicate completions, and
verified fallback intent. Physical signal loss, TV reboot, and Wake-on-LAN
timing are blocked by the explicit normal-use-first acceptance decision.

- Always issue the target input command; never use cached observed equality as a no-op.
- Query active input and signal immediately after each command ack and tag the result with `switch_epoch`.
- While remote-owned, schedule one monotonic single-flight signal poll. A bad poll arms one fixed fallback deadline; repeats cannot postpone it.
- Release owners and revoke lease/grant before awaiting fallback TV I/O.
- Retry server fallback until fresh server active-input and signal are observed. Expose `fallback_deferred` while SSAP is unavailable.
- Preserve one request ID/epoch through Wake-on-LAN polling; cancellation or timeout invalidates every late result.
- MultiView entry uses the same bundle lease/commit rule when ownership moves remotely without an HDMI change.
- MultiView exit releases input locally, disables MultiView, commands server input, and verifies fullscreen server signal before ready.
- Unexpected subscription state never completes an expected transaction by coincidence.

## D7: Observability and Operations

Status: completed for source behavior, deployment integration, and persistent
live log availability on Linux, macOS, and Windows.

- Emit one structured transition event containing request/switch epochs, old/new phase, command/observed input, owners, lease/grant identity, deadline, latency, and fallback reason.
- Keep log production non-blocking and bounded. Count dropped records and expose the count in status.
- Add SIGUSR1 state dump with secrets redacted.
- Expose queue depth, in-flight operation, observation age, reconnect count, retry alert state, and deadline remaining.
- Make startup configuration visible without exposing credentials.
- Remove `std::process::exit` from transient reconnect handling.

## Daemon Counterexample Tests

Each row must be a reducer test and, where I/O is involved, an actor/API test:

| Counterexample | Assertion |
|---|---|
| Offline event races with command ack | No grant; local ownership; fallback starts. |
| Readiness loss races with fresh TV observation | Reservation invalid; no grant or commit. |
| Old observation arrives in new switch epoch | Observation stored only as stale telemetry; no state advance. |
| Grant expires while client response is delayed | Commit rejected and fallback begins. |
| SSAP disconnects after remote commit | Input release event occurs before reconnect work. |
| Server fallback command succeeds but observation is missing | `fallback_required` remains true and `/ready` remains false. |
| Repeated signal failures arrive | Original deadline is unchanged. |
| Good signal arrives before armed deadline | Deadline cancels only for the matching active epoch. |
| Manual MultiView callback arrives during switch | Grant/reservation revoked and deterministic fallback chosen. |
| HTTP client disconnects mid-request | Protocol transaction follows its own deadline; no orphan task. |
| Actor queue is full | Bounded denial for ordinary work; safety event remains schedulable. |

## Daemon Completion Gate

- `cargo check` passes after each change.
- Focused tests pass after each logic commit; full `cargo test` passes before integration.
- No production path invokes `bscpylgtvcommand`.
- No independently mutable protocol mutexes remain.
- All public and internal API schemas are documented and versioned.
- Every invariant and counterexample above has executable coverage.
- Live status cannot say ready until registration, subscription, resync, and any required fallback have completed.

Current gate assessment: all source and automated gates above pass. The child
plan is not live-complete until a deployed three-host run proves reconnect,
signal/fallback behavior, persistent forensic logs, and measured production
latencies against the physical TV.
