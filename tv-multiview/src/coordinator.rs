use crate::{
    domain::ProtocolState,
    protocol::{self, Effect, Event, ProtocolError, ProtocolTiming},
};
use std::{sync::Arc, time::Duration};
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
    pub ordinary: mpsc::Receiver<Effect>,
    pub safety: mpsc::Receiver<Effect>,
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
    let initial = Arc::new(initial);
    let (snapshot_tx, snapshot_rx) = watch::channel(initial.clone());
    let started = Instant::now();
    let handle = CoordinatorHandle {
        ordinary_tx,
        safety_tx,
        snapshot_rx,
        started,
    };
    let task = tokio::spawn(run(
        (*initial).clone(),
        timing,
        ordinary_rx,
        safety_rx,
        ordinary_effect_tx,
        safety_effect_tx,
        snapshot_tx,
        started,
    ));
    (
        handle,
        EffectReceivers {
            ordinary: ordinary_effect_rx,
            safety: safety_effect_rx,
        },
        task,
    )
}

async fn run(
    mut state: ProtocolState,
    timing: ProtocolTiming,
    mut ordinary_rx: mpsc::Receiver<Request>,
    mut safety_rx: mpsc::Receiver<Request>,
    ordinary_effect_tx: mpsc::Sender<Effect>,
    safety_effect_tx: mpsc::Sender<Effect>,
    snapshot_tx: watch::Sender<Arc<ProtocolState>>,
    started: Instant,
) {
    loop {
        let timer = deadline_sleep(&state, started);
        tokio::pin!(timer);
        let request = tokio::select! {
            biased;
            request = safety_rx.recv() => request,
            request = ordinary_rx.recv() => request,
            _ = &mut timer => Some(Request { event: Event::Tick, response: None }),
        };
        let Some(request) = request else {
            break;
        };
        let now_ms = monotonic_ms(started);
        let previous_phase = state.phase;
        let result = protocol::apply(&state, request.event, now_ms, timing);
        match result {
            Ok(transition) => {
                state = transition.next;
                dispatch_effects(
                    &mut state,
                    transition.effects,
                    now_ms,
                    timing,
                    &ordinary_effect_tx,
                    &safety_effect_tx,
                );
                if previous_phase != state.phase {
                    info!(
                        event = "protocol_transition",
                        from = ?previous_phase,
                        to = ?state.phase,
                        request_epoch = state.request_epoch,
                        switch_epoch = state.switch_epoch,
                        keyboard_owner = %state.keyboard_owner,
                        pointer_owner = %state.pointer_owner,
                        fallback_required = state.fallback_required,
                    );
                }
                let snapshot = Arc::new(state.clone());
                snapshot_tx.send_replace(snapshot.clone());
                if let Some(response) = request.response {
                    let _ = response.send(Ok(snapshot));
                }
            }
            Err(protocol_error) => {
                if let Some(response) = request.response {
                    let _ = response.send(Err(protocol_error));
                } else {
                    warn!(error = %protocol_error, "coordinator event rejected");
                }
            }
        }
    }
    info!(event = "coordinator_stopped");
}

fn dispatch_effects(
    state: &mut ProtocolState,
    effects: Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
    ordinary_tx: &mpsc::Sender<Effect>,
    safety_tx: &mpsc::Sender<Effect>,
) {
    let mut pending = effects;
    while let Some(effect) = pending.pop() {
        let safety = matches!(effect, Effect::SetInput { fallback: true, .. });
        let sender = if safety { safety_tx } else { ordinary_tx };
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
    let mut deadlines = [
        state.phase_deadline_ms,
        state.next_signal_poll_ms,
        state
            .active_request
            .as_ref()
            .map(|request| request.deadline_ms),
        state
            .active_session
            .as_ref()
            .map(|session| session.renewed_until_ms.min(session.lease.expires_at_ms)),
    ]
    .into_iter()
    .flatten();
    match deadlines
        .next()
        .map(|first| deadlines.fold(first, u64::min))
    {
        Some(deadline_ms) => tokio::time::sleep_until(started + Duration::from_millis(deadline_ms)),
        None => tokio::time::sleep(Duration::from_secs(24 * 60 * 60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Host, TvMode};
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

    #[tokio::test]
    async fn publishes_coherent_snapshot_after_transition() {
        let (handle, _, task) = spawn(ProtocolState::new(Host::Linux), TIMING, 4, 2);
        let snapshot = handle.apply_safety(synchronized_event()).await.unwrap();
        assert!(snapshot.ready());
        assert_eq!(snapshot.keyboard_owner, snapshot.pointer_owner);
        assert_eq!(handle.snapshot().phase, snapshot.phase);
        task.abort();
    }

    #[tokio::test]
    async fn effect_channel_is_bounded_and_observable() {
        let (handle, mut effects, task) = spawn(ProtocolState::new(Host::Linux), TIMING, 1, 1);
        handle.apply_safety(synchronized_event()).await.unwrap();
        let snapshot = handle.snapshot();
        assert!(snapshot.ready());
        assert!(effects.ordinary.try_recv().is_err());
        task.abort();
    }
}
