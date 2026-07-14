mod actor;
mod backend;
mod coordinator;
mod types;

pub use actor::{ActorCommand, ActorEvent, ActorHandle, ActorPayload, SpawnedActor, spawn_actor};
pub use backend::ClipboardBackend;
pub use coordinator::{
    ActiveHandoff, BeginHandoff, Coordinator, CoordinatorCommand, CoordinatorError, HandoffPhase,
    SnapshotMetadata, TargetPreparation,
};
pub use types::{
    AppliedIdentity, AuthoritySessionId, ClipboardData, ClipboardKind, ClipboardReason,
    HandoffEpoch, HandoffId, HostId, NativeGeneration, OwnershipEpoch, OwnershipToken,
    ProcessSessionId, SnapshotId, SnapshotSequence, StagedSnapshot,
};
