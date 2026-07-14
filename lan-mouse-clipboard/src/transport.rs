use crate::{
    AuthenticatedPeer, AuthoritySessionId, ClipboardHello, ClipboardPayload, FrameError,
    FrameMetadata, HandoffEnvelope, HandoffId, HostId, ProcessSessionId, WireMessage,
    authenticate_hello, encode_message, read_frame_validated, write_frame,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Notify, mpsc},
};
use tokio_util::sync::CancellationToken;

const CONTROL_CLASS_COUNT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConnectionId(u64);

impl ConnectionId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedPeer {
    pub connection_id: ConnectionId,
    pub host_id: HostId,
    pub process_session_id: ProcessSessionId,
    pub effective_max_bytes: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationOutcome {
    Accepted(NegotiatedPeer),
    Replaced {
        previous_connection_id: ConnectionId,
        peer: NegotiatedPeer,
    },
    DuplicateRejected {
        active_connection_id: ConnectionId,
    },
}

#[derive(Debug, Default)]
pub struct PeerRegistry {
    next_connection_id: u64,
    peers: HashMap<HostId, NegotiatedPeer>,
}

impl PeerRegistry {
    pub fn register(
        &mut self,
        authenticated_peer: &AuthenticatedPeer,
        hello: &ClipboardHello,
        local_max_bytes: usize,
    ) -> Result<RegistrationOutcome, TransportError> {
        authenticate_hello(authenticated_peer, hello)?;
        if let Some(active) = self.peers.get(&authenticated_peer.host_id) {
            if active.process_session_id == hello.process_session_id {
                return Ok(RegistrationOutcome::DuplicateRejected {
                    active_connection_id: active.connection_id,
                });
            }
        }
        let connection_id = self
            .next_connection_id
            .checked_add(1)
            .ok_or(TransportError::IdentityExhausted)?;
        self.next_connection_id = connection_id;
        let peer = NegotiatedPeer {
            connection_id: ConnectionId(connection_id),
            host_id: authenticated_peer.host_id.clone(),
            process_session_id: hello.process_session_id,
            effective_max_bytes: hello.supports_text_v1().then(|| {
                local_max_bytes.min(usize::try_from(hello.max_receive_bytes).unwrap_or(usize::MAX))
            }),
        };
        let previous = self.peers.insert(peer.host_id.clone(), peer.clone());
        Ok(match previous {
            Some(previous) => RegistrationOutcome::Replaced {
                previous_connection_id: previous.connection_id,
                peer,
            },
            None => RegistrationOutcome::Accepted(peer),
        })
    }

    pub fn get(&self, host_id: &HostId) -> Option<&NegotiatedPeer> {
        self.peers.get(host_id)
    }

    pub fn unregister(&mut self, host_id: &HostId, connection_id: ConnectionId) -> bool {
        if self
            .peers
            .get(host_id)
            .is_some_and(|peer| peer.connection_id == connection_id)
        {
            self.peers.remove(host_id);
            true
        } else {
            false
        }
    }

    pub fn fence(
        &self,
        host_id: &HostId,
        authority_session_id: AuthoritySessionId,
    ) -> Option<PeerFence> {
        let peer = self.peers.get(host_id)?;
        Some(PeerFence {
            authority_session_id,
            host_id: peer.host_id.clone(),
            process_session_id: peer.process_session_id,
            connection_id: peer.connection_id,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerFence {
    pub authority_session_id: AuthoritySessionId,
    pub host_id: HostId,
    pub process_session_id: ProcessSessionId,
    pub connection_id: ConnectionId,
}

impl PeerFence {
    pub fn validate_source(&self, handoff: &HandoffEnvelope) -> Result<(), TransportError> {
        self.validate_authority(handoff)?;
        if handoff.source_token.owner_host_id != self.host_id {
            return Err(TransportError::AuthenticatedHostMismatch);
        }
        if handoff.source_process_session_id != self.process_session_id {
            return Err(TransportError::StalePeerSession);
        }
        Ok(())
    }

    pub fn validate_target(&self, handoff: &HandoffEnvelope) -> Result<(), TransportError> {
        self.validate_authority(handoff)?;
        if handoff.target_token.owner_host_id != self.host_id {
            return Err(TransportError::AuthenticatedHostMismatch);
        }
        if handoff.target_process_session_id != self.process_session_id {
            return Err(TransportError::StalePeerSession);
        }
        Ok(())
    }

    pub fn validate_source_metadata(&self, metadata: &FrameMetadata) -> Result<(), TransportError> {
        self.validate_metadata_authority(metadata)?;
        if metadata
            .source_token
            .as_ref()
            .ok_or(TransportError::MissingIdentity)?
            .owner_host_id
            != self.host_id
        {
            return Err(TransportError::AuthenticatedHostMismatch);
        }
        if metadata
            .source_process_session_id
            .ok_or(TransportError::MissingIdentity)?
            != self.process_session_id
        {
            return Err(TransportError::StalePeerSession);
        }
        Ok(())
    }

    pub fn validate_target_metadata(&self, metadata: &FrameMetadata) -> Result<(), TransportError> {
        self.validate_metadata_authority(metadata)?;
        if metadata
            .target_token
            .as_ref()
            .ok_or(TransportError::MissingIdentity)?
            .owner_host_id
            != self.host_id
        {
            return Err(TransportError::AuthenticatedHostMismatch);
        }
        if metadata
            .target_process_session_id
            .ok_or(TransportError::MissingIdentity)?
            != self.process_session_id
        {
            return Err(TransportError::StalePeerSession);
        }
        Ok(())
    }

    fn validate_authority(&self, handoff: &HandoffEnvelope) -> Result<(), TransportError> {
        if handoff.handoff_id.authority_session_id != self.authority_session_id
            || handoff.source_token.authority_session_id != self.authority_session_id
            || handoff.target_token.authority_session_id != self.authority_session_id
        {
            return Err(TransportError::StaleAuthoritySession);
        }
        Ok(())
    }

    fn validate_metadata_authority(&self, metadata: &FrameMetadata) -> Result<(), TransportError> {
        let handoff_id = metadata.handoff_id.ok_or(TransportError::MissingIdentity)?;
        if handoff_id.authority_session_id != self.authority_session_id
            || metadata
                .source_token
                .as_ref()
                .is_some_and(|token| token.authority_session_id != self.authority_session_id)
            || metadata
                .target_token
                .as_ref()
                .is_some_and(|token| token.authority_session_id != self.authority_session_id)
        {
            return Err(TransportError::StaleAuthoritySession);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Tls(#[from] crate::TlsError),
    #[error("clipboard transport identity exhausted")]
    IdentityExhausted,
    #[error("clipboard transport queue is full")]
    QueueFull,
    #[error("clipboard transport channel is closed")]
    ChannelClosed,
    #[error("payload message used the control queue")]
    PayloadOnControlQueue,
    #[error("control message used the payload slot")]
    ControlInPayloadSlot,
    #[error("authenticated clipboard host does not match message identity")]
    AuthenticatedHostMismatch,
    #[error("clipboard peer process session is stale")]
    StalePeerSession,
    #[error("clipboard authority session is stale")]
    StaleAuthoritySession,
    #[error("clipboard message is missing required identity metadata")]
    MissingIdentity,
}

#[derive(Clone)]
pub struct TransportHandle {
    control_tx: mpsc::Sender<WireMessage>,
    payload: Arc<PayloadSlot>,
    cancellation: CancellationToken,
}

impl TransportHandle {
    pub fn try_send_control(&self, message: WireMessage) -> Result<(), TransportError> {
        if message.is_payload() {
            return Err(TransportError::PayloadOnControlQueue);
        }
        self.control_tx
            .try_send(message)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => TransportError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => TransportError::ChannelClosed,
            })
    }

    pub fn try_send_payload(&self, message: WireMessage) -> Result<(), TransportError> {
        if !message.is_payload() {
            return Err(TransportError::ControlInPayloadSlot);
        }
        let handoff_id =
            message_handoff_id(&message).expect("payload message has handoff identity");
        let mut state = self.payload.state.lock().expect("payload slot poisoned");
        if state.queued.is_some() || state.active.is_some() {
            return Err(TransportError::QueueFull);
        }
        state.queued = Some(QueuedPayload {
            handoff_id,
            message,
            cancellation: self.cancellation.child_token(),
        });
        drop(state);
        self.payload.ready.notify_one();
        Ok(())
    }

    pub fn cancel_handoff(&self, handoff_id: HandoffId) -> bool {
        let mut state = self.payload.state.lock().expect("payload slot poisoned");
        let mut canceled = false;
        if state
            .queued
            .as_ref()
            .is_some_and(|payload| payload.handoff_id == handoff_id)
        {
            if let Some(payload) = state.queued.take() {
                payload.cancellation.cancel();
            }
            canceled = true;
        }
        if state
            .active
            .as_ref()
            .is_some_and(|payload| payload.handoff_id == handoff_id)
        {
            if let Some(payload) = &state.active {
                payload.cancellation.cancel();
            }
            canceled = true;
        }
        canceled
    }

    pub fn shutdown(&self) {
        let mut state = self.payload.state.lock().expect("payload slot poisoned");
        if let Some(payload) = state.queued.take() {
            payload.cancellation.cancel();
        }
        if let Some(payload) = &state.active {
            payload.cancellation.cancel();
        }
        drop(state);
        self.cancellation.cancel();
    }
}

pub struct TransportReceiver {
    control_rx: mpsc::Receiver<WireMessage>,
    payload: Arc<PayloadSlot>,
    cancellation: CancellationToken,
}

struct PayloadSlot {
    state: Mutex<PayloadState>,
    ready: Notify,
}

#[derive(Default)]
struct PayloadState {
    queued: Option<QueuedPayload>,
    active: Option<ActivePayload>,
}

struct QueuedPayload {
    handoff_id: HandoffId,
    message: WireMessage,
    cancellation: CancellationToken,
}

struct ActivePayload {
    handoff_id: HandoffId,
    cancellation: CancellationToken,
}

struct OutboundMessage {
    message: WireMessage,
    handoff_id: Option<HandoffId>,
    cancellation: CancellationToken,
}

pub fn transport_queues() -> (TransportHandle, TransportReceiver) {
    // One active handoff emits at most one message from each non-payload class.
    let (control_tx, control_rx) = mpsc::channel(CONTROL_CLASS_COUNT);
    let payload = Arc::new(PayloadSlot {
        state: Mutex::new(PayloadState::default()),
        ready: Notify::new(),
    });
    let cancellation = CancellationToken::new();
    (
        TransportHandle {
            control_tx,
            payload: payload.clone(),
            cancellation: cancellation.clone(),
        },
        TransportReceiver {
            control_rx,
            payload,
            cancellation,
        },
    )
}

impl TransportReceiver {
    async fn next(&mut self) -> Option<OutboundMessage> {
        loop {
            if let Some(payload) = {
                let mut state = self.payload.state.lock().expect("payload slot poisoned");
                let payload = state.queued.take();
                if let Some(payload) = &payload {
                    state.active = Some(ActivePayload {
                        handoff_id: payload.handoff_id,
                        cancellation: payload.cancellation.clone(),
                    });
                }
                payload
            } {
                return Some(OutboundMessage {
                    message: payload.message,
                    handoff_id: Some(payload.handoff_id),
                    cancellation: payload.cancellation,
                });
            }
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return None,
                message = self.control_rx.recv() => {
                    match message {
                        Some(message) => return Some(OutboundMessage {
                            message,
                            handoff_id: None,
                            cancellation: self.cancellation.clone(),
                        }),
                        None => return None,
                    }
                }
                _ = self.payload.ready.notified() => {}
            }
        }
    }

    fn complete_payload(&self, handoff_id: HandoffId) {
        let mut state = self.payload.state.lock().expect("payload slot poisoned");
        if state
            .active
            .as_ref()
            .is_some_and(|payload| payload.handoff_id == handoff_id)
        {
            state.active = None;
        }
    }
}

pub async fn run_writer<W: AsyncWrite + Unpin>(
    writer: &mut W,
    receiver: &mut TransportReceiver,
    max_payload_bytes: usize,
    transfer_budget: Duration,
) -> Result<(), TransportError> {
    while let Some(outbound) = receiver.next().await {
        let frame = match encode_message(&outbound.message, max_payload_bytes) {
            Ok(frame) => frame,
            Err(error) => {
                if let Some(handoff_id) = outbound.handoff_id {
                    receiver.complete_payload(handoff_id);
                }
                return Err(error.into());
            }
        };
        let write_result =
            write_frame(writer, &frame, transfer_budget, &outbound.cancellation).await;
        if let Some(handoff_id) = outbound.handoff_id {
            receiver.complete_payload(handoff_id);
        }
        write_result?;
        tracing::debug!(
            event = "clipboard_transfer_completed",
            message_type = ?outbound.message.message_type(),
            bytes = frame.encoded_len(),
            "clipboard transport wrote frame"
        );
    }
    Ok(())
}

pub struct InboundTransport {
    pub control_rx: mpsc::Receiver<WireMessage>,
    pub payload_rx: mpsc::Receiver<ClipboardPayload>,
}

struct InboundSink {
    control_tx: mpsc::Sender<WireMessage>,
    payload_tx: mpsc::Sender<ClipboardPayload>,
}

fn inbound_queues() -> (InboundSink, InboundTransport) {
    let (control_tx, control_rx) = mpsc::channel(CONTROL_CLASS_COUNT);
    let (payload_tx, payload_rx) = mpsc::channel(1);
    (
        InboundSink {
            control_tx,
            payload_tx,
        },
        InboundTransport {
            control_rx,
            payload_rx,
        },
    )
}

pub fn spawn_reader<R, V>(
    mut reader: R,
    max_payload_bytes: usize,
    transfer_budget: Duration,
    cancellation: CancellationToken,
    validator: V,
) -> (
    InboundTransport,
    tokio::task::JoinHandle<Result<(), TransportError>>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
    V: Fn(&FrameMetadata) -> Result<(), TransportError> + Send + Sync + 'static,
{
    let (sink, inbound) = inbound_queues();
    let task = tokio::spawn(async move {
        loop {
            let message = read_frame_validated(
                &mut reader,
                max_payload_bytes,
                transfer_budget,
                &cancellation,
                &validator,
            )
            .await?;
            match message {
                WireMessage::SnapshotOffer(payload) | WireMessage::SnapshotDeliver(payload) => {
                    sink.payload_tx
                        .try_send(payload)
                        .map_err(|error| match error {
                            mpsc::error::TrySendError::Full(_) => TransportError::QueueFull,
                            mpsc::error::TrySendError::Closed(_) => TransportError::ChannelClosed,
                        })?;
                }
                control => sink
                    .control_tx
                    .try_send(control)
                    .map_err(|error| match error {
                        mpsc::error::TrySendError::Full(_) => TransportError::QueueFull,
                        mpsc::error::TrySendError::Closed(_) => TransportError::ChannelClosed,
                    })?,
            }
        }
    });
    (inbound, task)
}

fn message_handoff_id(message: &WireMessage) -> Option<HandoffId> {
    match message {
        WireMessage::SnapshotOffer(payload) | WireMessage::SnapshotDeliver(payload) => {
            Some(payload.handoff.handoff_id)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CertificateFingerprint, ClipboardData, ClipboardKind, ClipboardReason, HandoffEpoch,
        NativeGeneration, OperationResult, OwnershipEpoch, OwnershipToken, SnapshotId,
        SnapshotSequence, read_frame,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::AsyncWriteExt;

    fn authenticated(host: &str) -> AuthenticatedPeer {
        AuthenticatedPeer {
            host_id: HostId::from(host),
            fingerprint: CertificateFingerprint::from_certificate(host.as_bytes()),
        }
    }

    fn hello(host: &str, process: u128) -> ClipboardHello {
        ClipboardHello {
            host_id: HostId::from(host),
            process_session_id: ProcessSessionId::new(process),
            offered_capabilities: crate::CLIPBOARD_TEXT_V1,
            max_receive_bytes: 128,
        }
    }

    fn token(host: &str, epoch: u64, authority: u128) -> OwnershipToken {
        OwnershipToken {
            authority_session_id: AuthoritySessionId::new(authority),
            ownership_epoch: OwnershipEpoch::new(epoch),
            owner_host_id: HostId::from(host),
        }
    }

    fn envelope(authority: u128, source_process: u128, target_process: u128) -> HandoffEnvelope {
        HandoffEnvelope {
            handoff_id: HandoffId {
                authority_session_id: AuthoritySessionId::new(authority),
                handoff_epoch: HandoffEpoch::new(2),
            },
            source_token: token("remote-a", 1, authority),
            source_process_session_id: ProcessSessionId::new(source_process),
            target_token: token("remote-b", 2, authority),
            target_process_session_id: ProcessSessionId::new(target_process),
        }
    }

    fn payload(handoff: HandoffEnvelope) -> WireMessage {
        WireMessage::SnapshotDeliver(ClipboardPayload {
            handoff,
            snapshot_id: SnapshotId {
                source_process_session_id: ProcessSessionId::new(11),
                sequence: SnapshotSequence::new(1),
            },
            data: ClipboardData::text(Arc::<[u8]>::from(&b"payload"[..])).unwrap(),
        })
    }

    #[test]
    fn peer_process_replacement_is_deterministic_and_old_disconnect_is_ignored() {
        let mut registry = PeerRegistry::default();
        let peer = authenticated("remote-a");
        let first = match registry
            .register(&peer, &hello("remote-a", 11), 64)
            .unwrap()
        {
            RegistrationOutcome::Accepted(peer) => peer,
            other => panic!("unexpected registration: {other:?}"),
        };
        assert_eq!(first.effective_max_bytes, Some(64));
        assert_eq!(
            registry
                .register(&peer, &hello("remote-a", 11), 64)
                .unwrap(),
            RegistrationOutcome::DuplicateRejected {
                active_connection_id: first.connection_id,
            }
        );
        let second = match registry
            .register(&peer, &hello("remote-a", 12), 64)
            .unwrap()
        {
            RegistrationOutcome::Replaced {
                previous_connection_id,
                peer,
            } => {
                assert_eq!(previous_connection_id, first.connection_id);
                peer
            }
            other => panic!("unexpected replacement: {other:?}"),
        };
        assert!(!registry.unregister(&HostId::from("remote-a"), first.connection_id));
        assert!(registry.unregister(&HostId::from("remote-a"), second.connection_id));
    }

    #[test]
    fn certificate_host_mismatch_is_rejected_before_registration() {
        let mut registry = PeerRegistry::default();
        assert!(matches!(
            registry.register(&authenticated("remote-a"), &hello("remote-b", 11), 64),
            Err(TransportError::Tls(crate::TlsError::HostIdentityMismatch))
        ));
        assert!(registry.get(&HostId::from("remote-a")).is_none());
    }

    #[test]
    fn stale_authority_process_and_authenticated_host_are_independent_fences() {
        let fence = PeerFence {
            authority_session_id: AuthoritySessionId::new(7),
            host_id: HostId::from("remote-a"),
            process_session_id: ProcessSessionId::new(11),
            connection_id: ConnectionId(1),
        };
        assert!(fence.validate_source(&envelope(7, 11, 22)).is_ok());
        assert!(matches!(
            fence.validate_source(&envelope(8, 11, 22)),
            Err(TransportError::StaleAuthoritySession)
        ));
        assert!(matches!(
            fence.validate_source(&envelope(7, 12, 22)),
            Err(TransportError::StalePeerSession)
        ));
        let mut wrong_host = envelope(7, 11, 22);
        wrong_host.source_token.owner_host_id = HostId::from("remote-b");
        assert!(matches!(
            fence.validate_source(&wrong_host),
            Err(TransportError::AuthenticatedHostMismatch)
        ));
    }

    #[tokio::test]
    async fn stale_payload_is_rejected_from_metadata_before_inbound_publication() {
        let fence = PeerFence {
            authority_session_id: AuthoritySessionId::new(7),
            host_id: HostId::from("remote-a"),
            process_session_id: ProcessSessionId::new(11),
            connection_id: ConnectionId(1),
        };
        let message = WireMessage::SnapshotOffer(ClipboardPayload {
            handoff: envelope(7, 12, 22),
            snapshot_id: SnapshotId {
                source_process_session_id: ProcessSessionId::new(12),
                sequence: SnapshotSequence::new(1),
            },
            data: ClipboardData::text(Arc::<[u8]>::from(&b"stale"[..])).unwrap(),
        });
        let frame = encode_message(&message, 64).unwrap();
        let cancellation = CancellationToken::new();
        let (mut writer, reader) = tokio::io::duplex(4096);
        let (mut inbound, task) = spawn_reader(
            reader,
            64,
            Duration::from_secs(1),
            cancellation.clone(),
            move |metadata| fence.validate_source_metadata(metadata),
        );
        write_frame(&mut writer, &frame, Duration::from_secs(1), &cancellation)
            .await
            .unwrap();
        assert!(matches!(
            task.await.unwrap(),
            Err(TransportError::StalePeerSession)
        ));
        assert!(inbound.payload_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn payload_slot_is_single_owner_and_cancel_drops_queued_bytes() {
        let (handle, mut receiver) = transport_queues();
        let first = envelope(7, 11, 22);
        let second = envelope(7, 11, 22);
        handle.try_send_payload(payload(first.clone())).unwrap();
        assert!(matches!(
            handle.try_send_payload(payload(second)),
            Err(TransportError::QueueFull)
        ));
        assert!(handle.cancel_handoff(first.handoff_id));
        handle
            .try_send_control(WireMessage::ProtocolError(ClipboardReason::ProtocolError))
            .unwrap();
        assert!(matches!(
            receiver.next().await.map(|outbound| outbound.message),
            Some(WireMessage::ProtocolError(ClipboardReason::ProtocolError))
        ));
        assert!(!handle.cancel_handoff(first.handoff_id));
    }

    #[tokio::test]
    async fn cancellation_releases_payload_already_owned_by_blocked_writer() {
        let (handle, mut receiver) = transport_queues();
        let handoff = envelope(7, 11, 22);
        let bytes = Arc::<[u8]>::from(&b"payload"[..]);
        let weak = Arc::downgrade(&bytes);
        handle
            .try_send_payload(WireMessage::SnapshotDeliver(ClipboardPayload {
                handoff: handoff.clone(),
                snapshot_id: SnapshotId {
                    source_process_session_id: ProcessSessionId::new(11),
                    sequence: SnapshotSequence::new(1),
                },
                data: ClipboardData::Text(bytes.clone()),
            }))
            .unwrap();
        drop(bytes);
        let (writer, _blocked_reader) = tokio::io::duplex(1);
        let writer_task = tokio::spawn(async move {
            let mut writer = writer;
            run_writer(&mut writer, &mut receiver, 64, Duration::from_secs(30)).await
        });
        loop {
            if handle
                .payload
                .state
                .lock()
                .expect("payload slot poisoned")
                .active
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            handle.try_send_payload(payload(envelope(7, 11, 22))),
            Err(TransportError::QueueFull)
        ));
        assert!(handle.cancel_handoff(handoff.handoff_id));
        assert!(matches!(
            writer_task.await.unwrap(),
            Err(TransportError::Frame(FrameError::Canceled))
        ));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn control_queue_saturation_is_bounded_and_payloads_cannot_enter_it() {
        let (handle, _receiver) = transport_queues();
        for _ in 0..CONTROL_CLASS_COUNT {
            handle
                .try_send_control(WireMessage::ProtocolError(ClipboardReason::ProtocolError))
                .unwrap();
        }
        assert!(matches!(
            handle.try_send_control(WireMessage::CancelHandoff(HandoffId {
                authority_session_id: AuthoritySessionId::new(7),
                handoff_epoch: HandoffEpoch::new(2),
            })),
            Err(TransportError::QueueFull)
        ));
        assert!(matches!(
            handle.try_send_control(payload(envelope(7, 11, 22))),
            Err(TransportError::PayloadOnControlQueue)
        ));
    }

    #[tokio::test]
    async fn writer_owns_stream_and_emits_control_and_payload_without_copying_service_data() {
        let (handle, mut receiver) = transport_queues();
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        let cancellation = receiver.cancellation.clone();
        let task = tokio::spawn(async move {
            run_writer(&mut writer, &mut receiver, 64, Duration::from_secs(1)).await
        });
        handle
            .try_send_control(WireMessage::ProtocolError(ClipboardReason::ProtocolError))
            .unwrap();
        assert!(matches!(
            read_frame(&mut reader, 64, Duration::from_secs(1), &cancellation)
                .await
                .unwrap(),
            WireMessage::ProtocolError(ClipboardReason::ProtocolError)
        ));
        handle
            .try_send_payload(payload(envelope(7, 11, 22)))
            .unwrap();
        assert!(matches!(
            read_frame(&mut reader, 64, Duration::from_secs(1), &cancellation)
                .await
                .unwrap(),
            WireMessage::SnapshotDeliver(_)
        ));
        handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn malformed_clipboard_reader_exits_while_independent_input_progresses() {
        let input_progress = Arc::new(AtomicUsize::new(0));
        let input_counter = input_progress.clone();
        let input_task = tokio::spawn(async move {
            for _ in 0..100 {
                input_counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        });
        let (mut writer, reader) = tokio::io::duplex(64);
        let (_inbound, clipboard_task) = spawn_reader(
            reader,
            64,
            Duration::from_secs(1),
            CancellationToken::new(),
            |_| Ok(()),
        );
        writer.write_all(b"not-a-clipboard-frame").await.unwrap();
        writer.shutdown().await.unwrap();
        assert!(matches!(
            clipboard_task.await.unwrap(),
            Err(TransportError::Frame(
                FrameError::Io(_) | FrameError::InvalidMagic
            ))
        ));
        input_task.await.unwrap();
        assert_eq!(input_progress.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn transport_control_types_remain_fixed_size_metadata() {
        let result = WireMessage::ApplyResult(crate::ApplyResult {
            handoff_id: envelope(7, 11, 22).handoff_id,
            target_token: token("remote-b", 2, 7),
            target_process_session_id: ProcessSessionId::new(22),
            snapshot_id: SnapshotId {
                source_process_session_id: ProcessSessionId::new(11),
                sequence: SnapshotSequence::new(1),
            },
            post_write_generation: Some(NativeGeneration::new(3)),
            result: OperationResult::Completed,
        });
        assert!(!result.is_payload());
        assert_eq!(ClipboardKind::Empty as u8, 1);
    }
}
