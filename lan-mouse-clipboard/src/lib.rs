mod actor;
mod backend;
mod coordinator;
mod frame;
mod tls;
mod transport;
mod types;

pub use actor::{ActorCommand, ActorEvent, ActorHandle, ActorPayload, SpawnedActor, spawn_actor};
pub use backend::ClipboardBackend;
#[cfg(target_os = "linux")]
pub use backend::LinuxClipboardBackend;
#[cfg(target_os = "windows")]
pub use backend::WindowsClipboardBackend;
pub use coordinator::{
    ActiveHandoff, BeginHandoff, Coordinator, CoordinatorCommand, CoordinatorError, HandoffPhase,
    SnapshotMetadata, TargetPreparation,
};
pub use frame::{
    ApplyResult, AuthorityState, CLIPBOARD_TEXT_V1, ClipboardHello, ClipboardPayload, EncodedFrame,
    FrameError, FrameMetadata, HandoffEnvelope, MessageType, OperationResult, PROTOCOL_VERSION,
    PrepareResult, WireMessage, encode_message, read_frame, read_frame_validated, write_frame,
};
pub use tls::{
    AuthenticatedPeer, AuthorizedPeers, CertificateFingerprint, TlsError, TlsIdentity,
    authenticate_alpn, authenticate_hello, authenticate_peer_certificates, client_config,
    clipboard_server_name, server_config,
};
pub use transport::{
    ConnectionId, InboundTransport, NegotiatedPeer, PeerFence, PeerRegistry, RegistrationOutcome,
    TransportError, TransportHandle, TransportReceiver, run_writer, spawn_reader, transport_queues,
};
pub use types::{
    AppliedIdentity, AuthoritySessionId, ClipboardData, ClipboardKind, ClipboardReason,
    HandoffEpoch, HandoffId, HostId, NativeGeneration, OwnershipEpoch, OwnershipToken,
    ProcessSessionId, SnapshotId, SnapshotSequence, StagedSnapshot,
};
