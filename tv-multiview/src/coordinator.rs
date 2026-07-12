use crate::{
    domain::ProtocolState,
    protocol::{self, Effect, Event, ProtocolError, ProtocolTiming},
};
use serde::Serialize;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::Instant,
};
use tracing::{error, info, warn};

struct Request {
    event: Event,
    response: Option<oneshot::Sender<Result<Arc<ProtocolState>, ProtocolError>>>,
}

#[derive(Clone)]
pub struct CoordinatorHandle {
    ordinary_tx: mpsc::Sender<Request>,
    safety_tx: mpsc::Sender<Request>,
    snapshot_rx: watch::Receiver<Arc<ProtocolState>>,
    ordinary_effect_queue: QueueGauge,
    safety_effect_queue: QueueGauge,
    started: Instant,
}

impl CoordinatorHandle {
    pub async fn apply(&self, event: Event) -> Result<Arc<ProtocolState>, CoordinatorError> {
        self.request(&self.ordinary_tx, event).await
    }

    pub async fn apply_safety(&self, event: Event) -> Result<Arc<ProtocolState>, CoordinatorError> {
        self.request(&self.safety_tx, event).await
    }

    async fn request(
        &self,
        sender: &mpsc::Sender<Request>,
        event: Event,
    ) -> Result<Arc<ProtocolState>, CoordinatorError> {
        let (response_tx, response_rx) = oneshot::channel();
        sender
            .try_send(Request {
                event,
                response: Some(response_tx),
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => CoordinatorError::Busy,
                mpsc::error::TrySendError::Closed(_) => CoordinatorError::Closed,
            })?;
        response_rx
            .await
            .map_err(|_| CoordinatorError::Closed)?
            .map_err(CoordinatorError::Protocol)
    }

    pub async fn notify_safety(&self, event: Event) -> Result<(), CoordinatorError> {
        self.safety_tx
            .send(Request {
                event,
                response: None,
            })
            .await
            .map_err(|_| CoordinatorError::Closed)
    }

    pub fn snapshot(&self) -> Arc<ProtocolState> {
        self.snapshot_rx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<ProtocolState>> {
        self.snapshot_rx.clone()
    }

    pub fn now_ms(&self) -> u64 {
        monotonic_ms(self.started)
    }

    pub fn queue_snapshot(&self) -> CoordinatorQueueSnapshot {
        CoordinatorQueueSnapshot {
            ordinary_commands: sender_queue_snapshot(&self.ordinary_tx),
            safety_commands: sender_queue_snapshot(&self.safety_tx),
            ordinary_effects: self.ordinary_effect_queue.snapshot(),
            safety_effects: self.safety_effect_queue.snapshot(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct QueueSnapshot {
    pub depth: usize,
    pub capacity: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CoordinatorQueueSnapshot {
    pub ordinary_commands: QueueSnapshot,
    pub safety_commands: QueueSnapshot,
    pub ordinary_effects: QueueSnapshot,
    pub safety_effects: QueueSnapshot,
}

#[derive(Clone)]
struct QueueGauge {
    depth: Arc<AtomicUsize>,
    capacity: usize,
}

#[derive(Clone)]
struct TrackedEffectSender {
    sender: mpsc::Sender<Effect>,
    queue: QueueGauge,
}

impl TrackedEffectSender {
    fn try_send(&self, effect: Effect) -> Result<(), mpsc::error::TrySendError<Effect>> {
        self.queue.begin_send();
        if let Err(error) = self.sender.try_send(effect) {
            self.queue.cancel_send();
            return Err(error);
        }
        Ok(())
    }
}

struct EffectSenders {
    ordinary: TrackedEffectSender,
    safety: TrackedEffectSender,
}

struct CoordinatorRuntime {
    ordinary_rx: mpsc::Receiver<Request>,
    safety_rx: mpsc::Receiver<Request>,
    effects: EffectSenders,
    snapshot_tx: watch::Sender<Arc<ProtocolState>>,
    started: Instant,
}

impl QueueGauge {
    fn new(capacity: usize) -> Self {
        Self {
            depth: Arc::new(AtomicUsize::new(0)),
            capacity,
        }
    }

    fn begin_send(&self) {
        self.depth.fetch_add(1, Ordering::Relaxed);
    }

    fn cancel_send(&self) {
        self.depth.fetch_sub(1, Ordering::Relaxed);
    }

    fn received(&self) {
        self.depth.fetch_sub(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            depth: self.depth.load(Ordering::Relaxed),
            capacity: self.capacity,
        }
    }
}

fn sender_queue_snapshot<T>(sender: &mpsc::Sender<T>) -> QueueSnapshot {
    QueueSnapshot {
        depth: sender.max_capacity().saturating_sub(sender.capacity()),
        capacity: sender.max_capacity(),
    }
}

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error("coordinator queue is full")]
    Busy,
    #[error("coordinator is closed")]
    Closed,
    #[error(transparent)]
    Protocol(ProtocolError),
}

pub struct EffectReceivers {
    pub(crate) ordinary: TrackedEffectReceiver,
    pub(crate) safety: TrackedEffectReceiver,
}

pub(crate) struct TrackedEffectReceiver {
    receiver: mpsc::Receiver<Effect>,
    queue: QueueGauge,
}

impl TrackedEffectReceiver {
    pub async fn recv(&mut self) -> Option<Effect> {
        let effect = self.receiver.recv().await;
        if effect.is_some() {
            self.queue.received();
        }
        effect
    }

    #[cfg(test)]
    fn try_recv(&mut self) -> Result<Effect, mpsc::error::TryRecvError> {
        let effect = self.receiver.try_recv()?;
        self.queue.received();
        Ok(effect)
    }
}

pub fn spawn(
    initial: ProtocolState,
    timing: ProtocolTiming,
    command_capacity: usize,
    safety_capacity: usize,
) -> (CoordinatorHandle, EffectReceivers, JoinHandle<()>) {
    let (ordinary_tx, ordinary_rx) = mpsc::channel(command_capacity);
    let (safety_tx, safety_rx) = mpsc::channel(safety_capacity);
    let (ordinary_effect_tx, ordinary_effect_rx) = mpsc::channel(command_capacity);
    let (safety_effect_tx, safety_effect_rx) = mpsc::channel(safety_capacity);
    let ordinary_effect_queue = QueueGauge::new(command_capacity);
    let safety_effect_queue = QueueGauge::new(safety_capacity);
    let initial = Arc::new(initial);
    let (snapshot_tx, snapshot_rx) = watch::channel(initial.clone());
    let started = Instant::now();
    let handle = CoordinatorHandle {
        ordinary_tx,
        safety_tx,
        snapshot_rx,
        ordinary_effect_queue: ordinary_effect_queue.clone(),
        safety_effect_queue: safety_effect_queue.clone(),
        started,
    };
    let task = tokio::spawn(run(
        (*initial).clone(),
        timing,
        CoordinatorRuntime {
            ordinary_rx,
            safety_rx,
            effects: EffectSenders {
                ordinary: TrackedEffectSender {
                    sender: ordinary_effect_tx,
                    queue: ordinary_effect_queue.clone(),
                },
                safety: TrackedEffectSender {
                    sender: safety_effect_tx,
                    queue: safety_effect_queue.clone(),
                },
            },
            snapshot_tx,
            started,
        },
    ));
    (
        handle,
        EffectReceivers {
            ordinary: TrackedEffectReceiver {
                receiver: ordinary_effect_rx,
                queue: ordinary_effect_queue,
            },
            safety: TrackedEffectReceiver {
                receiver: safety_effect_rx,
                queue: safety_effect_queue,
            },
        },
        task,
    )
}

async fn run(mut state: ProtocolState, timing: ProtocolTiming, mut runtime: CoordinatorRuntime) {
    loop {
        let timer = deadline_sleep(&state, runtime.started);
        tokio::pin!(timer);
        let deadline_due = state
            .next_deadline_ms()
            .is_some_and(|deadline| deadline <= monotonic_ms(runtime.started));
        let request = if deadline_due {
            Some(Request {
                event: Event::Tick,
                response: None,
            })
        } else {
            tokio::select! {
                biased;
                _ = &mut timer => Some(Request { event: Event::Tick, response: None }),
                request = runtime.safety_rx.recv() => request,
                request = runtime.ordinary_rx.recv() => request,
            }
        };
        let Some(request) = request else {
            break;
        };
        let now_ms = monotonic_ms(runtime.started);
        let event = request.event;
        let event_name = event.name();
        let latency_ms = event_latency_ms(&state, &event, now_ms, timing);
        let previous_phase = state.phase;
        let result = protocol::apply(&state, event, now_ms, timing);
        match result {
            Ok(transition) => {
                state = transition.next;
                dispatch_effects(
                    &mut state,
                    transition.effects,
                    now_ms,
                    timing,
                    &runtime.effects,
                );
                log_transition(event_name, previous_phase, &state, now_ms, latency_ms);
                let snapshot = Arc::new(state.clone());
                runtime.snapshot_tx.send_replace(snapshot.clone());
                if let Some(response) = request.response {
                    let _ = response.send(Ok(snapshot));
                }
            }
            Err(protocol_error) => {
                warn!(
                    event = "protocol_event_rejected",
                    trigger = event_name,
                    phase = ?state.phase,
                    request_epoch = state.request_epoch,
                    switch_epoch = state.switch_epoch,
                    error = %protocol_error,
                );
                if let Some(response) = request.response {
                    let _ = response.send(Err(protocol_error));
                }
            }
        }
    }
    info!(event = "coordinator_stopped");
}

fn event_latency_ms(
    state: &ProtocolState,
    event: &Event,
    now_ms: u64,
    timing: ProtocolTiming,
) -> Option<u64> {
    let expected_duration = match event {
        Event::CommandAcknowledged { .. }
        | Event::CommandFailed { .. }
        | Event::MultiViewAcknowledged { .. } => timing.command_ms,
        Event::Observation { .. } => timing.observation_ms,
        _ => return None,
    };
    state
        .phase_deadline_ms
        .map(|deadline| now_ms.saturating_sub(deadline.saturating_sub(expected_duration)))
}

fn log_transition(
    trigger: &'static str,
    previous_phase: crate::domain::ProtocolPhase,
    state: &ProtocolState,
    now_ms: u64,
    latency_ms: Option<u64>,
) {
    let request = state
        .active_request
        .as_ref()
        .or(state.request_history.back());
    let lease = request
        .map(|request| &request.lease)
        .or_else(|| state.active_session.as_ref().map(|session| &session.lease));
    let grant = request.and_then(|request| request.grant.as_ref());
    let observation_age_ms = state.observed_input.and_then(|host| {
        state
            .input_signal
            .get(&host)
            .filter(|observation| observation.observed_at_ms > 0)
            .map(|observation| now_ms.saturating_sub(observation.observed_at_ms))
    });
    let deadline_remaining_ms = state
        .next_deadline_ms()
        .map(|deadline| deadline.saturating_sub(now_ms));
    info!(
        event = "protocol_transition",
        trigger,
        previous_phase = ?previous_phase,
        next_phase = ?state.phase,
        request_id = request.map(|request| request.request_id.as_str()).unwrap_or(""),
        request_epoch = state.request_epoch,
        switch_epoch = state.switch_epoch,
        commanded_input = state.commanded_input.map(|host| host.as_str()).unwrap_or("unknown"),
        observed_input = state.observed_input.map(|host| host.as_str()).unwrap_or("unknown"),
        keyboard_owner = %state.keyboard_owner,
        pointer_owner = %state.pointer_owner,
        lease_id = lease.map(|lease| lease.lease_id.as_str()).unwrap_or(""),
        lease_epoch = lease.map(|lease| lease.lease_epoch).unwrap_or(0),
        grant_epoch = grant.map(|grant| grant.grant_epoch).unwrap_or(0),
        observation_age_ms = observation_age_ms.unwrap_or(0),
        latency_ms = latency_ms.unwrap_or(0),
        deadline_remaining_ms = deadline_remaining_ms.unwrap_or(0),
        fallback_required = state.fallback_required,
        fallback_reason = state.fallback_reason.as_deref().unwrap_or(""),
    );
}

fn dispatch_effects(
    state: &mut ProtocolState,
    effects: Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
    senders: &EffectSenders,
) {
    let mut pending = effects;
    while let Some(effect) = pending.pop() {
        let safety = matches!(effect, Effect::SetInput { fallback: true, .. });
        let sender = if safety {
            &senders.safety
        } else {
            &senders.ordinary
        };
        if let Err(send_error) = sender.try_send(effect.clone()) {
            if safety {
                warn!(effect = ?effect, "safety effect queue full; deadline scheduler will retry");
                continue;
            }
            let switch_epoch = match effect {
                Effect::SetInput { switch_epoch, .. }
                | Effect::Observe { switch_epoch }
                | Effect::SetMultiView { switch_epoch, .. } => Some(switch_epoch),
                Effect::Wake { .. } => None,
            };
            let failure = switch_epoch.map(|switch_epoch| Event::CommandFailed {
                switch_epoch,
                reason: format!("effect queue unavailable: {send_error}"),
            });
            if let Some(failure) = failure {
                match protocol::apply(state, failure, now_ms, timing) {
                    Ok(transition) => {
                        *state = transition.next;
                        pending.extend(transition.effects);
                    }
                    Err(protocol_error) => {
                        error!(error = %protocol_error, "queue failure transition rejected");
                    }
                }
            } else {
                warn!(effect = ?effect, "wake effect dropped because ordinary queue is full");
            }
        }
    }
}

fn monotonic_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn deadline_sleep(state: &ProtocolState, started: Instant) -> tokio::time::Sleep {
    match state.next_deadline_ms() {
        Some(deadline_ms) => tokio::time::sleep_until(started + Duration::from_millis(deadline_ms)),
        None => tokio::time::sleep(Duration::from_secs(24 * 60 * 60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Host, LeaseIdentity, PeerReadiness, TvMode};
    use std::collections::BTreeMap;

    const TIMING: ProtocolTiming = ProtocolTiming {
        command_ms: 100,
        observation_ms: 100,
        grant_ms: 100,
        wake_ms: 500,
        lease_ms: 300,
        signal_poll_ms: 100,
    };

    fn synchronized_event() -> Event {
        Event::TransportSynchronized {
            mode: TvMode::Fullscreen,
            input: Some(Host::Linux),
            signals: BTreeMap::from([
                (Host::Linux, true),
                (Host::Mac, false),
                (Host::Windows, false),
            ]),
        }
    }

    fn remote_owned_state() -> ProtocolState {
        let synchronized = protocol::apply(
            &ProtocolState::new(Host::Linux, 32),
            synchronized_event(),
            1,
            TIMING,
        )
        .unwrap()
        .next;
        let ready = protocol::apply(
            &synchronized,
            Event::PeerReadinessUpdated {
                host: Host::Mac,
                readiness: PeerReadiness {
                    online: true,
                    keyboard_ready: true,
                    pointer_ready: true,
                    session_epoch: 7,
                    observed_at_ms: 2,
                },
            },
            2,
            TIMING,
        )
        .unwrap()
        .next;
        let switching = protocol::apply(
            &ready,
            Event::CreateEnter {
                request_id: "request-1".to_string(),
                client_id: "client-1".to_string(),
                target: Host::Mac,
                lease: LeaseIdentity {
                    lease_id: "lease-1".to_string(),
                    lease_epoch: 1,
                    peer_session_epoch: 7,
                    expires_at_ms: 1_000,
                },
            },
            10,
            TIMING,
        )
        .unwrap()
        .next;
        let switch_epoch = switching.switch_epoch;
        let granted = protocol::apply(
            &switching,
            Event::Observation {
                switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: BTreeMap::from([
                    (Host::Linux, false),
                    (Host::Mac, true),
                    (Host::Windows, false),
                ]),
            },
            20,
            TIMING,
        )
        .unwrap()
        .next;
        let request = granted.active_request.as_ref().unwrap();
        protocol::apply(
            &granted,
            Event::Commit {
                request_id: request.request_id.clone(),
                request_epoch: request.request_epoch,
                grant_epoch: request.grant.as_ref().unwrap().grant_epoch,
                lease_id: request.lease.lease_id.clone(),
                lease_epoch: request.lease.lease_epoch,
            },
            30,
            TIMING,
        )
        .unwrap()
        .next
    }

    #[tokio::test]
    async fn publishes_coherent_snapshot_after_transition() {
        let (handle, _, task) = spawn(ProtocolState::new(Host::Linux, 32), TIMING, 4, 2);
        let snapshot = handle.apply_safety(synchronized_event()).await.unwrap();
        assert!(snapshot.ready());
        assert_eq!(snapshot.keyboard_owner, snapshot.pointer_owner);
        assert_eq!(handle.snapshot().phase, snapshot.phase);
        task.abort();
    }

    #[tokio::test]
    async fn effect_channel_is_bounded_and_observable() {
        let (handle, mut effects, task) = spawn(ProtocolState::new(Host::Linux, 32), TIMING, 1, 1);
        handle.apply_safety(synchronized_event()).await.unwrap();
        handle
            .apply_safety(Event::PeerReadinessUpdated {
                host: Host::Mac,
                readiness: PeerReadiness {
                    online: true,
                    keyboard_ready: true,
                    pointer_ready: true,
                    session_epoch: 7,
                    observed_at_ms: handle.now_ms(),
                },
            })
            .await
            .unwrap();
        let now_ms = handle.now_ms();
        handle
            .apply(Event::CreateEnter {
                request_id: "request-1".to_string(),
                client_id: "client-1".to_string(),
                target: Host::Mac,
                lease: LeaseIdentity {
                    lease_id: "lease-1".to_string(),
                    lease_epoch: 1,
                    peer_session_epoch: 7,
                    expires_at_ms: now_ms + 1_000,
                },
            })
            .await
            .unwrap();

        assert_eq!(handle.queue_snapshot().ordinary_effects.depth, 1);
        assert!(effects.ordinary.try_recv().is_ok());
        assert_eq!(handle.queue_snapshot().ordinary_effects.depth, 0);
        task.abort();
    }

    #[tokio::test]
    async fn due_deadline_precedes_already_queued_renewal() {
        let mut initial = remote_owned_state();
        let session = initial.active_session.as_mut().unwrap();
        session.renewed_until_ms = 0;
        let renew = Event::Renew {
            request_id: session.request_id.clone(),
            lease_id: session.lease.lease_id.clone(),
            lease_epoch: session.lease.lease_epoch,
            peer_session_epoch: session.lease.peer_session_epoch,
        };
        let (handle, _effects, task) = spawn(initial, TIMING, 4, 2);
        let (response_tx, response_rx) = oneshot::channel();
        handle
            .safety_tx
            .try_send(Request {
                event: renew,
                response: Some(response_tx),
            })
            .unwrap();

        let snapshot = response_rx.await.unwrap().unwrap();
        assert_eq!(snapshot.keyboard_owner, Host::Linux);
        assert_eq!(snapshot.pointer_owner, Host::Linux);
        assert_eq!(
            snapshot.fallback_reason.as_deref(),
            Some("lease_renewal_rejected")
        );
        task.abort();
    }
}
