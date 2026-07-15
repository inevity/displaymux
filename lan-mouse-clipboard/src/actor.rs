use crate::backend::ClipboardBackend;
use crate::{
    AppliedIdentity, ClipboardData, ClipboardKind, ClipboardReason, HandoffId, HostId,
    NativeGeneration, OwnershipToken, ProcessSessionId, SnapshotId, SnapshotSequence,
    StagedSnapshot,
};
use std::thread;
use tokio::sync::{mpsc, oneshot};

const COMMAND_CLASS_COUNT: usize = 10;
const PAYLOAD_SLOT_COUNT: usize = 1;
const SOURCE_CHANGE_RETRIES: usize = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActorCommand {
    SynchronizeAuthority {
        current_token: OwnershipToken,
    },
    ObserveGeneration,
    PrepareTarget {
        handoff_id: HandoffId,
        target_token: OwnershipToken,
    },
    CaptureSource {
        handoff_id: HandoffId,
        source_token: OwnershipToken,
        max_bytes: usize,
    },
    CaptureProvisional {
        source_token: OwnershipToken,
        max_bytes: usize,
    },
    BindProvisional {
        handoff_id: HandoffId,
        source_token: OwnershipToken,
    },
    PublishSnapshot {
        handoff_id: HandoffId,
        snapshot_id: SnapshotId,
    },
    ActivateTarget {
        handoff_id: HandoffId,
        target_token: OwnershipToken,
    },
    StageSnapshot(StagedSnapshot),
    CancelHandoff {
        handoff_id: HandoffId,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActorEvent {
    AuthoritySynchronized {
        current_token: OwnershipToken,
    },
    Generation(NativeGeneration),
    Prepared {
        handoff_id: HandoffId,
        target_token: OwnershipToken,
        process_session_id: ProcessSessionId,
        baseline_generation: NativeGeneration,
    },
    Captured {
        handoff_id: HandoffId,
        source_token: OwnershipToken,
        snapshot_id: SnapshotId,
        kind: ClipboardKind,
        bytes: usize,
    },
    ProvisionalCaptured {
        source_token: OwnershipToken,
        snapshot_id: SnapshotId,
        kind: ClipboardKind,
        bytes: usize,
    },
    Activated {
        handoff_id: HandoffId,
        target_token: OwnershipToken,
    },
    Staged {
        handoff_id: HandoffId,
        snapshot_id: SnapshotId,
    },
    Applied(AppliedIdentity),
    Published {
        handoff_id: HandoffId,
        snapshot_id: SnapshotId,
    },
    Skipped {
        handoff_id: Option<HandoffId>,
        reason: ClipboardReason,
    },
    BackendUnavailable {
        handoff_id: Option<HandoffId>,
        reason: ClipboardReason,
    },
    Canceled {
        handoff_id: HandoffId,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorPayload {
    pub handoff_id: HandoffId,
    pub snapshot_id: SnapshotId,
    pub source_token: OwnershipToken,
    pub data: ClipboardData,
}

struct ActorRequest {
    command: ActorCommand,
    completion: oneshot::Sender<ActorEvent>,
}

#[derive(Clone)]
pub struct ActorHandle {
    request_tx: mpsc::Sender<ActorRequest>,
}

impl ActorHandle {
    pub fn try_request(
        &self,
        command: ActorCommand,
    ) -> Result<oneshot::Receiver<ActorEvent>, ClipboardReason> {
        let (completion, receiver) = oneshot::channel();
        self.request_tx
            .try_send(ActorRequest {
                command,
                completion,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ClipboardReason::QueueFull,
                mpsc::error::TrySendError::Closed(_) => ClipboardReason::ChannelUnavailable,
            })?;
        Ok(receiver)
    }
}

pub struct SpawnedActor {
    pub handle: ActorHandle,
    pub payload_rx: mpsc::Receiver<ActorPayload>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SpawnedActor {
    pub fn join(mut self) -> thread::Result<()> {
        self.thread.take().expect("actor thread missing").join()
    }
}

pub fn spawn_actor<B: ClipboardBackend>(
    name: &str,
    backend: B,
    process_session_id: ProcessSessionId,
    local_host_id: HostId,
    initial_token: OwnershipToken,
) -> std::io::Result<SpawnedActor> {
    // One active handoff can emit at most one request for each command class
    // before completion. Payload bytes use a separate single ownership slot.
    let (request_tx, request_rx) = mpsc::channel(COMMAND_CLASS_COUNT);
    let (payload_tx, payload_rx) = mpsc::channel(PAYLOAD_SLOT_COUNT);
    let thread = thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            Actor::new(
                backend,
                process_session_id,
                local_host_id,
                initial_token,
                request_rx,
                payload_tx,
            )
            .run();
        })?;
    Ok(SpawnedActor {
        handle: ActorHandle { request_tx },
        payload_rx,
        thread: Some(thread),
    })
}

struct StoredSnapshot {
    handoff_id: Option<HandoffId>,
    source_token: OwnershipToken,
    snapshot_id: SnapshotId,
    data: ClipboardData,
}

struct Actor<B> {
    backend: B,
    process_session_id: ProcessSessionId,
    local_host_id: HostId,
    current_token: OwnershipToken,
    next_snapshot_sequence: SnapshotSequence,
    preparation: Option<(HandoffId, OwnershipToken, NativeGeneration)>,
    source_snapshot: Option<StoredSnapshot>,
    stage: Option<StagedSnapshot>,
    applied: Option<AppliedIdentity>,
    request_rx: mpsc::Receiver<ActorRequest>,
    payload_tx: mpsc::Sender<ActorPayload>,
}

impl<B: ClipboardBackend> Actor<B> {
    fn new(
        backend: B,
        process_session_id: ProcessSessionId,
        local_host_id: HostId,
        current_token: OwnershipToken,
        request_rx: mpsc::Receiver<ActorRequest>,
        payload_tx: mpsc::Sender<ActorPayload>,
    ) -> Self {
        Self {
            backend,
            process_session_id,
            local_host_id,
            current_token,
            next_snapshot_sequence: SnapshotSequence::new(0),
            preparation: None,
            source_snapshot: None,
            stage: None,
            applied: None,
            request_rx,
            payload_tx,
        }
    }

    fn run(mut self) {
        while let Some(request) = self.request_rx.blocking_recv() {
            let shutdown = matches!(request.command, ActorCommand::Shutdown);
            let event = self.handle(request.command);
            let backend_lost = matches!(event, ActorEvent::BackendUnavailable { .. });
            let _ = request.completion.send(event);
            if shutdown || backend_lost {
                break;
            }
        }
        self.backend.shutdown();
        self.source_snapshot = None;
        self.stage = None;
        self.preparation = None;
    }

    fn handle(&mut self, command: ActorCommand) -> ActorEvent {
        match command {
            ActorCommand::SynchronizeAuthority { current_token } => {
                self.synchronize_authority(current_token)
            }
            ActorCommand::ObserveGeneration => match self.backend.generation() {
                Ok(generation) => ActorEvent::Generation(generation),
                Err(reason) => Self::native_failure(None, reason),
            },
            ActorCommand::PrepareTarget {
                handoff_id,
                target_token,
            } => self.prepare_target(handoff_id, target_token),
            ActorCommand::CaptureSource {
                handoff_id,
                source_token,
                max_bytes,
            } => self.capture_source(Some(handoff_id), source_token, max_bytes),
            ActorCommand::CaptureProvisional {
                source_token,
                max_bytes,
            } => self.capture_source(None, source_token, max_bytes),
            ActorCommand::BindProvisional {
                handoff_id,
                source_token,
            } => self.bind_provisional(handoff_id, source_token),
            ActorCommand::PublishSnapshot {
                handoff_id,
                snapshot_id,
            } => self.publish_snapshot(handoff_id, snapshot_id),
            ActorCommand::ActivateTarget {
                handoff_id,
                target_token,
            } => self.activate_target(handoff_id, target_token),
            ActorCommand::StageSnapshot(stage) => self.stage_snapshot(stage),
            ActorCommand::CancelHandoff { handoff_id } => {
                self.cancel_handoff(handoff_id);
                ActorEvent::Canceled { handoff_id }
            }
            ActorCommand::Shutdown => ActorEvent::Shutdown,
        }
    }

    fn synchronize_authority(&mut self, current_token: OwnershipToken) -> ActorEvent {
        if current_token.authority_session_id != self.current_token.authority_session_id
            || current_token != self.current_token
        {
            self.source_snapshot = None;
            self.stage = None;
            self.preparation = None;
            self.applied = None;
            self.current_token = current_token.clone();
        }
        ActorEvent::AuthoritySynchronized { current_token }
    }

    fn prepare_target(
        &mut self,
        handoff_id: HandoffId,
        target_token: OwnershipToken,
    ) -> ActorEvent {
        if handoff_id.authority_session_id != target_token.authority_session_id
            || target_token.authority_session_id != self.current_token.authority_session_id
        {
            return Self::skipped(Some(handoff_id), ClipboardReason::StaleAuthoritySession);
        }
        if target_token.owner_host_id == self.local_host_id && target_token != self.current_token {
            let baseline_generation = match self.backend.generation() {
                Ok(generation) => generation,
                Err(reason) => return Self::native_failure(Some(handoff_id), reason),
            };
            self.preparation = Some((handoff_id, target_token.clone(), baseline_generation));
            ActorEvent::Prepared {
                handoff_id,
                target_token,
                process_session_id: self.process_session_id,
                baseline_generation,
            }
        } else {
            Self::skipped(Some(handoff_id), ClipboardReason::StaleOwnerToken)
        }
    }

    fn capture_source(
        &mut self,
        handoff_id: Option<HandoffId>,
        source_token: OwnershipToken,
        max_bytes: usize,
    ) -> ActorEvent {
        if source_token != self.current_token {
            return Self::skipped(handoff_id, ClipboardReason::StaleOwnerToken);
        }
        if handoff_id.is_some_and(|id| id.authority_session_id != source_token.authority_session_id)
        {
            return Self::skipped(handoff_id, ClipboardReason::StaleAuthoritySession);
        }
        let data = match self.stable_capture(max_bytes) {
            Ok(data) => data,
            Err(ActorFailure::Native(reason)) => return Self::native_failure(handoff_id, reason),
            Err(ActorFailure::Handoff(reason)) => return Self::skipped(handoff_id, reason),
        };
        let sequence = match self.next_snapshot_sequence.get().checked_add(1) {
            Some(sequence) => sequence,
            None => {
                return Self::native_failure(handoff_id, ClipboardReason::IdentityExhausted);
            }
        };
        self.next_snapshot_sequence = SnapshotSequence::new(sequence);
        let snapshot_id = SnapshotId {
            source_process_session_id: self.process_session_id,
            sequence: self.next_snapshot_sequence,
        };
        let kind = data
            .kind()
            .expect("stable capture rejects unavailable clipboard data");
        let bytes = data.len();
        self.source_snapshot = Some(StoredSnapshot {
            handoff_id,
            source_token: source_token.clone(),
            snapshot_id,
            data,
        });
        match handoff_id {
            Some(handoff_id) => ActorEvent::Captured {
                handoff_id,
                source_token,
                snapshot_id,
                kind,
                bytes,
            },
            None => ActorEvent::ProvisionalCaptured {
                source_token,
                snapshot_id,
                kind,
                bytes,
            },
        }
    }

    fn stable_capture(&mut self, max_bytes: usize) -> Result<ClipboardData, ActorFailure> {
        for retry in 0..=SOURCE_CHANGE_RETRIES {
            let before = self.backend.generation().map_err(ActorFailure::Native)?;
            let data = self
                .backend
                .capture(max_bytes)
                .map_err(ActorFailure::Native)?;
            match &data {
                ClipboardData::Text(bytes) => {
                    std::str::from_utf8(bytes)
                        .map_err(|_| ActorFailure::Handoff(ClipboardReason::InvalidUtf8))?;
                }
                ClipboardData::Empty => {}
                ClipboardData::Unavailable(reason) => {
                    return Err(ActorFailure::Handoff(*reason));
                }
            }
            if data.len() > max_bytes {
                return Err(ActorFailure::Handoff(ClipboardReason::Oversize));
            }
            let after = self.backend.generation().map_err(ActorFailure::Native)?;
            if before == after {
                return Ok(data);
            }
            if retry == SOURCE_CHANGE_RETRIES {
                return Err(ActorFailure::Handoff(ClipboardReason::SourceChanged));
            }
        }
        unreachable!("bounded capture loop always returns")
    }

    fn bind_provisional(
        &mut self,
        handoff_id: HandoffId,
        source_token: OwnershipToken,
    ) -> ActorEvent {
        let Some(snapshot) = self.source_snapshot.as_mut() else {
            return Self::skipped(Some(handoff_id), ClipboardReason::SourceChanged);
        };
        if snapshot.handoff_id.is_some() || snapshot.source_token != source_token {
            return Self::skipped(Some(handoff_id), ClipboardReason::StaleOwnerToken);
        }
        if handoff_id.authority_session_id != source_token.authority_session_id {
            return Self::skipped(Some(handoff_id), ClipboardReason::StaleAuthoritySession);
        }
        snapshot.handoff_id = Some(handoff_id);
        ActorEvent::Captured {
            handoff_id,
            source_token,
            snapshot_id: snapshot.snapshot_id,
            kind: snapshot
                .data
                .kind()
                .expect("stored snapshots are transferable data"),
            bytes: snapshot.data.len(),
        }
    }

    fn publish_snapshot(&mut self, handoff_id: HandoffId, snapshot_id: SnapshotId) -> ActorEvent {
        let Some(snapshot) = self.source_snapshot.take() else {
            return Self::skipped(Some(handoff_id), ClipboardReason::StaleHandoff);
        };
        if snapshot.handoff_id != Some(handoff_id) || snapshot.snapshot_id != snapshot_id {
            self.source_snapshot = Some(snapshot);
            return Self::skipped(Some(handoff_id), ClipboardReason::StaleHandoff);
        }
        let payload = ActorPayload {
            handoff_id,
            snapshot_id,
            source_token: snapshot.source_token,
            data: snapshot.data,
        };
        if let Err(error) = self.payload_tx.try_send(payload) {
            let reason = match error {
                mpsc::error::TrySendError::Full(_) => ClipboardReason::QueueFull,
                mpsc::error::TrySendError::Closed(_) => ClipboardReason::ChannelUnavailable,
            };
            return Self::skipped(Some(handoff_id), reason);
        }
        ActorEvent::Published {
            handoff_id,
            snapshot_id,
        }
    }

    fn activate_target(
        &mut self,
        handoff_id: HandoffId,
        target_token: OwnershipToken,
    ) -> ActorEvent {
        if handoff_id.authority_session_id != target_token.authority_session_id {
            return Self::skipped(Some(handoff_id), ClipboardReason::StaleAuthoritySession);
        }
        if target_token.owner_host_id != self.local_host_id {
            return Self::skipped(Some(handoff_id), ClipboardReason::StaleOwnerToken);
        }
        let Some((prepared_handoff, prepared_token, _)) = self.preparation.as_ref() else {
            return Self::skipped(Some(handoff_id), ClipboardReason::TargetNotPrepared);
        };
        if *prepared_handoff != handoff_id || *prepared_token != target_token {
            return Self::skipped(Some(handoff_id), ClipboardReason::StaleHandoff);
        }
        self.current_token = target_token.clone();
        let event = ActorEvent::Activated {
            handoff_id,
            target_token,
        };
        if self
            .stage
            .as_ref()
            .is_some_and(|stage| stage.handoff_id == handoff_id)
        {
            self.apply_stage()
        } else {
            event
        }
    }

    fn stage_snapshot(&mut self, stage: StagedSnapshot) -> ActorEvent {
        if let ClipboardData::Unavailable(reason) = &stage.data {
            return Self::skipped(Some(stage.handoff_id), *reason);
        }
        if stage.handoff_id.authority_session_id != stage.target_token.authority_session_id {
            return Self::skipped(
                Some(stage.handoff_id),
                ClipboardReason::StaleAuthoritySession,
            );
        }
        if stage.target_token.owner_host_id != self.local_host_id {
            return Self::skipped(Some(stage.handoff_id), ClipboardReason::StaleOwnerToken);
        }
        if self.applied
            == Some(AppliedIdentity {
                handoff_id: stage.handoff_id,
                snapshot_id: stage.snapshot_id,
            })
        {
            return Self::skipped(Some(stage.handoff_id), ClipboardReason::Duplicate);
        }
        let Some((prepared_handoff, prepared_token, prepared_generation)) =
            self.preparation.as_ref()
        else {
            return Self::skipped(Some(stage.handoff_id), ClipboardReason::TargetNotPrepared);
        };
        if *prepared_handoff != stage.handoff_id
            || *prepared_token != stage.target_token
            || *prepared_generation != stage.baseline_generation
            || stage.target_process_session_id != self.process_session_id
        {
            return Self::skipped(Some(stage.handoff_id), ClipboardReason::StaleHandoff);
        }
        let handoff_id = stage.handoff_id;
        let snapshot_id = stage.snapshot_id;
        self.stage = Some(stage);
        if self.current_token == self.stage.as_ref().expect("stage installed").target_token {
            self.apply_stage()
        } else {
            ActorEvent::Staged {
                handoff_id,
                snapshot_id,
            }
        }
    }

    fn apply_stage(&mut self) -> ActorEvent {
        let Some(stage) = self.stage.take() else {
            return Self::skipped(None, ClipboardReason::StaleHandoff);
        };
        if stage.target_token != self.current_token {
            self.stage = Some(stage);
            return Self::skipped(None, ClipboardReason::StaleOwnerToken);
        }
        let identity = AppliedIdentity {
            handoff_id: stage.handoff_id,
            snapshot_id: stage.snapshot_id,
        };
        if self.applied == Some(identity) {
            return Self::skipped(Some(stage.handoff_id), ClipboardReason::Duplicate);
        }

        // The native write and applied-identity record are synchronous in this
        // serialized actor, with no await or cancellation point between them.
        let post_write_generation = match self.backend.apply(stage.baseline_generation, &stage.data)
        {
            Ok(generation) => generation,
            Err(reason) => return Self::native_failure(Some(stage.handoff_id), reason),
        };
        self.applied = Some(identity);
        self.preparation = None;
        tracing::debug!(
            event = "clipboard_apply_completed",
            handoff_epoch = identity.handoff_id.handoff_epoch.get(),
            snapshot_sequence = identity.snapshot_id.sequence.get(),
            native_generation = post_write_generation.get(),
            "clipboard actor applied snapshot"
        );
        ActorEvent::Applied(identity)
    }

    fn cancel_handoff(&mut self, handoff_id: HandoffId) {
        if self
            .preparation
            .as_ref()
            .is_some_and(|(id, _, _)| *id == handoff_id)
        {
            self.preparation = None;
        }
        if self
            .source_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.handoff_id == Some(handoff_id))
        {
            self.source_snapshot = None;
        }
        if self
            .stage
            .as_ref()
            .is_some_and(|stage| stage.handoff_id == handoff_id)
        {
            self.stage = None;
        }
    }

    fn skipped(handoff_id: Option<HandoffId>, reason: ClipboardReason) -> ActorEvent {
        tracing::debug!(
            event = "clipboard_snapshot_skipped",
            reason = reason.code(),
            "clipboard actor skipped work"
        );
        ActorEvent::Skipped { handoff_id, reason }
    }

    fn native_failure(handoff_id: Option<HandoffId>, reason: ClipboardReason) -> ActorEvent {
        match reason {
            ClipboardReason::BackendUnavailable
            | ClipboardReason::PermissionDenied
            | ClipboardReason::IdentityExhausted => {
                ActorEvent::BackendUnavailable { handoff_id, reason }
            }
            _ => Self::skipped(handoff_id, reason),
        }
    }
}

enum ActorFailure {
    Native(ClipboardReason),
    Handoff(ClipboardReason),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthoritySessionId, HandoffEpoch, HostId, OwnershipEpoch};
    use std::{
        collections::VecDeque,
        sync::{Arc, Condvar, Mutex},
        time::Duration,
    };

    #[derive(Clone)]
    struct BackendControl {
        state: Arc<(Mutex<BackendState>, Condvar)>,
    }

    struct BackendState {
        generation: NativeGeneration,
        generation_results: VecDeque<Result<NativeGeneration, ClipboardReason>>,
        data: ClipboardData,
        capture_failure: Option<ClipboardReason>,
        apply_failure: Option<ClipboardReason>,
        block_generation: bool,
        entered_generation: bool,
        shutdown: bool,
        apply_count: usize,
    }

    impl BackendControl {
        fn new(data: ClipboardData) -> Self {
            Self {
                state: Arc::new((
                    Mutex::new(BackendState {
                        generation: NativeGeneration::new(0),
                        generation_results: VecDeque::new(),
                        data,
                        capture_failure: None,
                        apply_failure: None,
                        block_generation: false,
                        entered_generation: false,
                        shutdown: false,
                        apply_count: 0,
                    }),
                    Condvar::new(),
                )),
            }
        }

        fn backend(&self) -> FakeBackend {
            FakeBackend {
                control: self.clone(),
            }
        }

        fn set_generation(&self, generation: u64) {
            self.state.0.lock().unwrap().generation = NativeGeneration::new(generation);
        }

        fn set_data(&self, data: ClipboardData) {
            self.state.0.lock().unwrap().data = data;
        }

        fn script_generations(
            &self,
            generations: impl IntoIterator<Item = Result<NativeGeneration, ClipboardReason>>,
        ) {
            self.state.0.lock().unwrap().generation_results = generations.into_iter().collect();
        }

        fn fail_apply(&self, reason: ClipboardReason) {
            self.state.0.lock().unwrap().apply_failure = Some(reason);
        }

        fn fail_capture(&self, reason: ClipboardReason) {
            self.state.0.lock().unwrap().capture_failure = Some(reason);
        }

        fn block_generation(&self) {
            self.state.0.lock().unwrap().block_generation = true;
        }

        fn wait_generation(&self) {
            let (lock, condvar) = &*self.state;
            let mut state = lock.lock().unwrap();
            while !state.entered_generation {
                state = condvar.wait(state).unwrap();
            }
        }

        fn unblock_generation(&self) {
            let (lock, condvar) = &*self.state;
            lock.lock().unwrap().block_generation = false;
            condvar.notify_all();
        }
    }

    struct FakeBackend {
        control: BackendControl,
    }

    impl ClipboardBackend for FakeBackend {
        fn generation(&mut self) -> Result<NativeGeneration, ClipboardReason> {
            let (lock, condvar) = &*self.control.state;
            let mut state = lock.lock().unwrap();
            state.entered_generation = true;
            condvar.notify_all();
            while state.block_generation {
                state = condvar.wait(state).unwrap();
            }
            state
                .generation_results
                .pop_front()
                .unwrap_or(Ok(state.generation))
        }

        fn capture(&mut self, _max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
            let mut state = self.control.state.0.lock().unwrap();
            match state.capture_failure.take() {
                Some(reason) => Err(reason),
                None => Ok(state.data.clone()),
            }
        }

        fn apply(
            &mut self,
            expected_generation: NativeGeneration,
            data: &ClipboardData,
        ) -> Result<NativeGeneration, ClipboardReason> {
            let mut state = self.control.state.0.lock().unwrap();
            if let Some(reason) = state.apply_failure.take() {
                return Err(reason);
            }
            if state.generation != expected_generation {
                return Err(ClipboardReason::DestinationChanged);
            }
            state.data = data.clone();
            state.generation = NativeGeneration::new(state.generation.get() + 1);
            state.apply_count += 1;
            Ok(state.generation)
        }

        fn shutdown(&mut self) {
            self.control.state.0.lock().unwrap().shutdown = true;
        }
    }

    fn token(host: &str, epoch: u64) -> OwnershipToken {
        OwnershipToken {
            authority_session_id: AuthoritySessionId::new(1),
            ownership_epoch: OwnershipEpoch::new(epoch),
            owner_host_id: HostId::from(host),
        }
    }

    fn handoff(epoch: u64) -> HandoffId {
        HandoffId {
            authority_session_id: AuthoritySessionId::new(1),
            handoff_epoch: HandoffEpoch::new(epoch),
        }
    }

    fn recv(receiver: oneshot::Receiver<ActorEvent>) -> ActorEvent {
        receiver.blocking_recv().unwrap()
    }

    #[test]
    fn captured_payload_bypasses_completion_event() {
        let control =
            BackendControl::new(ClipboardData::text(Arc::<[u8]>::from(&b"alpha"[..])).unwrap());
        let process = ProcessSessionId::new(5);
        let source = token("server", 0);
        let mut actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            process,
            HostId::from("server"),
            source.clone(),
        )
        .unwrap();
        let captured = recv(
            actor
                .handle
                .try_request(ActorCommand::CaptureSource {
                    handoff_id: handoff(1),
                    source_token: source,
                    max_bytes: 16,
                })
                .unwrap(),
        );
        let ActorEvent::Captured {
            snapshot_id, bytes, ..
        } = captured
        else {
            panic!("capture did not complete")
        };
        assert_eq!(bytes, 5);
        assert!(actor.payload_rx.try_recv().is_err());
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::PublishSnapshot {
                        handoff_id: handoff(1),
                        snapshot_id,
                    })
                    .unwrap()
            ),
            ActorEvent::Published { .. }
        ));
        assert_eq!(actor.payload_rx.blocking_recv().unwrap().data.len(), 5);
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }

    #[test]
    fn destination_change_prevents_apply_and_preserves_value() {
        let original = ClipboardData::text(Arc::<[u8]>::from(&b"local"[..])).unwrap();
        let incoming = ClipboardData::text(Arc::<[u8]>::from(&b"remote"[..])).unwrap();
        let control = BackendControl::new(original.clone());
        let process = ProcessSessionId::new(5);
        let current = token("server", 0);
        let target = token("remote", 1);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            process,
            HostId::from("remote"),
            current,
        )
        .unwrap();
        let ActorEvent::Prepared {
            baseline_generation,
            ..
        } = recv(
            actor
                .handle
                .try_request(ActorCommand::PrepareTarget {
                    handoff_id: handoff(1),
                    target_token: target.clone(),
                })
                .unwrap(),
        )
        else {
            panic!("target did not prepare")
        };
        control.set_generation(1);
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::StageSnapshot(StagedSnapshot {
                        handoff_id: handoff(1),
                        snapshot_id: SnapshotId {
                            source_process_session_id: ProcessSessionId::new(9),
                            sequence: SnapshotSequence::new(1),
                        },
                        target_token: target.clone(),
                        target_process_session_id: process,
                        baseline_generation,
                        data: incoming,
                    }))
                    .unwrap()
            ),
            ActorEvent::Staged { .. }
        ));
        let event = recv(
            actor
                .handle
                .try_request(ActorCommand::ActivateTarget {
                    handoff_id: handoff(1),
                    target_token: target,
                })
                .unwrap(),
        );
        assert_eq!(
            event,
            ActorEvent::Skipped {
                handoff_id: Some(handoff(1)),
                reason: ClipboardReason::DestinationChanged,
            }
        );
        assert_eq!(control.state.0.lock().unwrap().data, original);
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }

    #[test]
    fn explicit_empty_applies_once_and_unavailable_never_clears_target() {
        let original = ClipboardData::text(Arc::<[u8]>::from(&b"local"[..])).unwrap();
        let control = BackendControl::new(original.clone());
        let process = ProcessSessionId::new(5);
        let target = token("remote", 1);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            process,
            HostId::from("remote"),
            token("server", 0),
        )
        .unwrap();
        let ActorEvent::Prepared {
            baseline_generation,
            ..
        } = recv(
            actor
                .handle
                .try_request(ActorCommand::PrepareTarget {
                    handoff_id: handoff(1),
                    target_token: target.clone(),
                })
                .unwrap(),
        )
        else {
            panic!("target did not prepare")
        };

        let unavailable_reasons = [
            ClipboardReason::CapabilityMissing,
            ClipboardReason::BackendUnavailable,
            ClipboardReason::PermissionDenied,
            ClipboardReason::PrivateContent,
            ClipboardReason::UnsupportedFormat,
            ClipboardReason::Oversize,
            ClipboardReason::SourceChanged,
            ClipboardReason::TargetNotPrepared,
            ClipboardReason::DestinationChanged,
            ClipboardReason::StaleAuthoritySession,
            ClipboardReason::StalePeerSession,
            ClipboardReason::StaleHandoff,
            ClipboardReason::StaleOwnerToken,
            ClipboardReason::Duplicate,
            ClipboardReason::ChannelUnavailable,
            ClipboardReason::TransferTimeout,
            ClipboardReason::ProtocolError,
            ClipboardReason::IntegrityFailed,
            ClipboardReason::InvalidUtf8,
            ClipboardReason::Canceled,
            ClipboardReason::QueueFull,
            ClipboardReason::IdentityExhausted,
        ];
        for (sequence, reason) in unavailable_reasons.into_iter().enumerate() {
            let event = recv(
                actor
                    .handle
                    .try_request(ActorCommand::StageSnapshot(StagedSnapshot {
                        handoff_id: handoff(1),
                        snapshot_id: SnapshotId {
                            source_process_session_id: ProcessSessionId::new(9),
                            sequence: SnapshotSequence::new(sequence as u64 + 1),
                        },
                        target_token: target.clone(),
                        target_process_session_id: process,
                        baseline_generation,
                        data: ClipboardData::Unavailable(reason),
                    }))
                    .unwrap(),
            );
            assert_eq!(
                event,
                ActorEvent::Skipped {
                    handoff_id: Some(handoff(1)),
                    reason,
                }
            );
            assert_eq!(control.state.0.lock().unwrap().data, original);
        }

        let empty_snapshot = SnapshotId {
            source_process_session_id: ProcessSessionId::new(9),
            sequence: SnapshotSequence::new(100),
        };
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::StageSnapshot(StagedSnapshot {
                        handoff_id: handoff(1),
                        snapshot_id: empty_snapshot,
                        target_token: target.clone(),
                        target_process_session_id: process,
                        baseline_generation,
                        data: ClipboardData::Empty,
                    }))
                    .unwrap()
            ),
            ActorEvent::Staged { .. }
        ));
        assert_eq!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::ActivateTarget {
                        handoff_id: handoff(1),
                        target_token: target.clone(),
                    })
                    .unwrap()
            ),
            ActorEvent::Applied(AppliedIdentity {
                handoff_id: handoff(1),
                snapshot_id: empty_snapshot,
            })
        );
        assert!(control.state.0.lock().unwrap().data.is_explicit_empty());
        assert_eq!(control.state.0.lock().unwrap().apply_count, 1);

        assert_eq!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::StageSnapshot(StagedSnapshot {
                        handoff_id: handoff(1),
                        snapshot_id: empty_snapshot,
                        target_token: target,
                        target_process_session_id: process,
                        baseline_generation,
                        data: ClipboardData::Empty,
                    }))
                    .unwrap()
            ),
            ActorEvent::Skipped {
                handoff_id: Some(handoff(1)),
                reason: ClipboardReason::Duplicate,
            }
        );
        assert_eq!(control.state.0.lock().unwrap().apply_count, 1);
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }

    #[test]
    fn source_filtering_never_publishes_private_unsupported_oversized_or_invalid_text() {
        let control = BackendControl::new(ClipboardData::Empty);
        let process = ProcessSessionId::new(5);
        let source = token("server", 0);
        let mut actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            process,
            HostId::from("server"),
            source.clone(),
        )
        .unwrap();

        for reason in [
            ClipboardReason::PrivateContent,
            ClipboardReason::UnsupportedFormat,
            ClipboardReason::PermissionDenied,
        ] {
            control.set_data(ClipboardData::Unavailable(reason));
            assert_eq!(
                recv(
                    actor
                        .handle
                        .try_request(ActorCommand::CaptureSource {
                            handoff_id: handoff(1),
                            source_token: source.clone(),
                            max_bytes: 16,
                        })
                        .unwrap()
                ),
                ActorEvent::Skipped {
                    handoff_id: Some(handoff(1)),
                    reason,
                }
            );
            assert!(actor.payload_rx.try_recv().is_err());
        }

        control.set_data(ClipboardData::Text(Arc::<[u8]>::from(
            &b"0123456789abcdefg"[..],
        )));
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::CaptureSource {
                        handoff_id: handoff(1),
                        source_token: source.clone(),
                        max_bytes: 16,
                    })
                    .unwrap()
            ),
            ActorEvent::Skipped {
                reason: ClipboardReason::Oversize,
                ..
            }
        ));

        control.set_data(ClipboardData::Text(Arc::<[u8]>::from(&[0xff, 0xfe][..])));
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::CaptureSource {
                        handoff_id: handoff(1),
                        source_token: source.clone(),
                        max_bytes: 16,
                    })
                    .unwrap()
            ),
            ActorEvent::Skipped {
                reason: ClipboardReason::InvalidUtf8,
                ..
            }
        ));
        assert!(actor.payload_rx.try_recv().is_err());
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }

    #[test]
    fn native_capture_failure_reports_backend_loss_and_closes_actor() {
        let control = BackendControl::new(ClipboardData::Empty);
        control.fail_capture(ClipboardReason::BackendUnavailable);
        let source = token("server", 0);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            ProcessSessionId::new(5),
            HostId::from("server"),
            source.clone(),
        )
        .unwrap();
        assert_eq!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::CaptureSource {
                        handoff_id: handoff(1),
                        source_token: source,
                        max_bytes: 16,
                    })
                    .unwrap()
            ),
            ActorEvent::BackendUnavailable {
                handoff_id: Some(handoff(1)),
                reason: ClipboardReason::BackendUnavailable,
            }
        );
        let closed_handle = actor.handle.clone();
        actor.join().unwrap();
        assert!(control.state.0.lock().unwrap().shutdown);
        assert!(matches!(
            closed_handle.try_request(ActorCommand::ObserveGeneration),
            Err(ClipboardReason::ChannelUnavailable)
        ));
    }

    #[test]
    fn source_change_retries_once_and_backend_failures_preserve_target() {
        let control =
            BackendControl::new(ClipboardData::text(Arc::<[u8]>::from(&b"stable"[..])).unwrap());
        control.script_generations([
            Ok(NativeGeneration::new(0)),
            Ok(NativeGeneration::new(1)),
            Ok(NativeGeneration::new(1)),
            Ok(NativeGeneration::new(1)),
        ]);
        let process = ProcessSessionId::new(5);
        let source = token("server", 0);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            process,
            HostId::from("server"),
            source.clone(),
        )
        .unwrap();
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::CaptureSource {
                        handoff_id: handoff(1),
                        source_token: source.clone(),
                        max_bytes: 16,
                    })
                    .unwrap()
            ),
            ActorEvent::Captured { .. }
        ));
        control.script_generations([
            Ok(NativeGeneration::new(1)),
            Ok(NativeGeneration::new(2)),
            Ok(NativeGeneration::new(2)),
            Ok(NativeGeneration::new(3)),
        ]);
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::CaptureSource {
                        handoff_id: handoff(2),
                        source_token: source.clone(),
                        max_bytes: 16,
                    })
                    .unwrap()
            ),
            ActorEvent::Skipped {
                reason: ClipboardReason::SourceChanged,
                ..
            }
        ));
        control.script_generations([Err(ClipboardReason::BackendUnavailable)]);
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::CaptureSource {
                        handoff_id: handoff(3),
                        source_token: source,
                        max_bytes: 16,
                    })
                    .unwrap()
            ),
            ActorEvent::BackendUnavailable {
                reason: ClipboardReason::BackendUnavailable,
                ..
            }
        ));
        let closed_handle = actor.handle.clone();
        actor.join().unwrap();
        assert!(matches!(
            closed_handle.try_request(ActorCommand::Shutdown),
            Err(ClipboardReason::ChannelUnavailable)
        ));

        let original = ClipboardData::text(Arc::<[u8]>::from(&b"local"[..])).unwrap();
        let control = BackendControl::new(original.clone());
        let target = token("remote", 1);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            process,
            HostId::from("remote"),
            token("server", 0),
        )
        .unwrap();
        let ActorEvent::Prepared {
            baseline_generation,
            ..
        } = recv(
            actor
                .handle
                .try_request(ActorCommand::PrepareTarget {
                    handoff_id: handoff(1),
                    target_token: target.clone(),
                })
                .unwrap(),
        )
        else {
            panic!("target did not prepare")
        };
        control.fail_apply(ClipboardReason::BackendUnavailable);
        recv(
            actor
                .handle
                .try_request(ActorCommand::StageSnapshot(StagedSnapshot {
                    handoff_id: handoff(1),
                    snapshot_id: SnapshotId {
                        source_process_session_id: ProcessSessionId::new(9),
                        sequence: SnapshotSequence::new(1),
                    },
                    target_token: target.clone(),
                    target_process_session_id: process,
                    baseline_generation,
                    data: ClipboardData::Empty,
                }))
                .unwrap(),
        );
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::ActivateTarget {
                        handoff_id: handoff(1),
                        target_token: target,
                    })
                    .unwrap()
            ),
            ActorEvent::BackendUnavailable {
                reason: ClipboardReason::BackendUnavailable,
                ..
            }
        ));
        assert_eq!(control.state.0.lock().unwrap().data, original);
        actor.join().unwrap();
    }

    #[test]
    fn target_identity_checks_allow_new_server_epoch_and_reject_wrong_host_or_authority() {
        let control = BackendControl::new(ClipboardData::Empty);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            ProcessSessionId::new(5),
            HostId::from("server"),
            token("server", 0),
        )
        .unwrap();
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::PrepareTarget {
                        handoff_id: handoff(2),
                        target_token: token("server", 2),
                    })
                    .unwrap()
            ),
            ActorEvent::Prepared { .. }
        ));
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::PrepareTarget {
                        handoff_id: handoff(3),
                        target_token: token("remote", 3),
                    })
                    .unwrap()
            ),
            ActorEvent::Skipped {
                reason: ClipboardReason::StaleOwnerToken,
                ..
            }
        ));
        let mut old_authority = token("server", 4);
        old_authority.authority_session_id = AuthoritySessionId::new(9);
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::PrepareTarget {
                        handoff_id: handoff(4),
                        target_token: old_authority,
                    })
                    .unwrap()
            ),
            ActorEvent::Skipped {
                reason: ClipboardReason::StaleAuthoritySession,
                ..
            }
        ));
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }

    #[test]
    fn provisional_capture_binds_only_matching_source_token() {
        let control = BackendControl::new(ClipboardData::Empty);
        let process = ProcessSessionId::new(5);
        let source = token("remote", 3);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            process,
            HostId::from("remote"),
            source.clone(),
        )
        .unwrap();
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::CaptureProvisional {
                        source_token: source.clone(),
                        max_bytes: 16,
                    })
                    .unwrap()
            ),
            ActorEvent::ProvisionalCaptured { .. }
        ));
        assert_eq!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::BindProvisional {
                        handoff_id: handoff(4),
                        source_token: token("remote", 2),
                    })
                    .unwrap()
            ),
            ActorEvent::Skipped {
                handoff_id: Some(handoff(4)),
                reason: ClipboardReason::StaleOwnerToken,
            }
        );
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::BindProvisional {
                        handoff_id: handoff(4),
                        source_token: source,
                    })
                    .unwrap()
            ),
            ActorEvent::Captured { .. }
        ));
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }

    #[test]
    fn authority_synchronization_fences_old_actor_state_before_new_work() {
        let control = BackendControl::new(ClipboardData::Empty);
        let old_token = token("remote", 1);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            ProcessSessionId::new(5),
            HostId::from("remote"),
            old_token.clone(),
        )
        .unwrap();
        recv(
            actor
                .handle
                .try_request(ActorCommand::CaptureProvisional {
                    source_token: old_token,
                    max_bytes: 16,
                })
                .unwrap(),
        );
        let mut current_token = token("server", 8);
        current_token.authority_session_id = AuthoritySessionId::new(2);

        assert_eq!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::SynchronizeAuthority {
                        current_token: current_token.clone(),
                    })
                    .unwrap()
            ),
            ActorEvent::AuthoritySynchronized {
                current_token: current_token.clone(),
            }
        );
        let mut target_token = token("remote", 9);
        target_token.authority_session_id = AuthoritySessionId::new(2);
        let new_handoff = HandoffId {
            authority_session_id: AuthoritySessionId::new(2),
            handoff_epoch: HandoffEpoch::new(3),
        };
        assert!(matches!(
            recv(
                actor
                    .handle
                    .try_request(ActorCommand::PrepareTarget {
                        handoff_id: new_handoff,
                        target_token,
                    })
                    .unwrap()
            ),
            ActorEvent::Prepared { .. }
        ));
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }

    #[test]
    fn command_queue_saturates_without_unbounded_growth() {
        let control = BackendControl::new(ClipboardData::Empty);
        control.block_generation();
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            ProcessSessionId::new(5),
            HostId::from("server"),
            token("server", 0),
        )
        .unwrap();
        let first = actor
            .handle
            .try_request(ActorCommand::ObserveGeneration)
            .unwrap();
        control.wait_generation();
        let mut queued = Vec::new();
        for _ in 0..COMMAND_CLASS_COUNT {
            queued.push(
                actor
                    .handle
                    .try_request(ActorCommand::ObserveGeneration)
                    .unwrap(),
            );
        }
        assert!(matches!(
            actor.handle.try_request(ActorCommand::ObserveGeneration),
            Err(ClipboardReason::QueueFull)
        ));
        control.unblock_generation();
        recv(first);
        for receiver in queued {
            recv(receiver);
        }
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }

    #[test]
    fn shutdown_releases_actor_resources() {
        let control = BackendControl::new(ClipboardData::Empty);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            ProcessSessionId::new(5),
            HostId::from("server"),
            token("server", 0),
        )
        .unwrap();
        let handle = actor.handle.clone();
        assert_eq!(
            recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap()),
            ActorEvent::Shutdown
        );
        actor.join().unwrap();
        assert!(control.state.0.lock().unwrap().shutdown);
        assert!(matches!(
            handle.try_request(ActorCommand::ObserveGeneration),
            Err(ClipboardReason::ChannelUnavailable)
        ));
    }

    #[test]
    fn closed_payload_slot_reports_channel_unavailable_without_exposing_bytes() {
        let control =
            BackendControl::new(ClipboardData::text(Arc::<[u8]>::from(&b"alpha"[..])).unwrap());
        let process = ProcessSessionId::new(5);
        let source = token("server", 0);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            process,
            HostId::from("server"),
            source.clone(),
        )
        .unwrap();
        let SpawnedActor {
            handle,
            payload_rx,
            thread,
        } = actor;
        drop(payload_rx);
        let ActorEvent::Captured { snapshot_id, .. } = recv(
            handle
                .try_request(ActorCommand::CaptureSource {
                    handoff_id: handoff(1),
                    source_token: source,
                    max_bytes: 16,
                })
                .unwrap(),
        ) else {
            panic!("source did not capture")
        };
        assert_eq!(
            recv(
                handle
                    .try_request(ActorCommand::PublishSnapshot {
                        handoff_id: handoff(1),
                        snapshot_id,
                    })
                    .unwrap()
            ),
            ActorEvent::Skipped {
                handoff_id: Some(handoff(1)),
                reason: ClipboardReason::ChannelUnavailable,
            }
        );
        recv(handle.try_request(ActorCommand::Shutdown).unwrap());
        thread.expect("actor thread missing").join().unwrap();
    }

    #[test]
    fn actor_completion_does_not_depend_on_async_runtime() {
        let control = BackendControl::new(ClipboardData::Empty);
        let actor = spawn_actor(
            "clipboard-test",
            control.backend(),
            ProcessSessionId::new(5),
            HostId::from("server"),
            token("server", 0),
        )
        .unwrap();
        let receiver = actor
            .handle
            .try_request(ActorCommand::ObserveGeneration)
            .unwrap();
        thread::sleep(Duration::from_millis(1));
        assert!(matches!(recv(receiver), ActorEvent::Generation(_)));
        recv(actor.handle.try_request(ActorCommand::Shutdown).unwrap());
        actor.join().unwrap();
    }
}
