use std::{fmt, sync::Arc};

macro_rules! integer_id {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name($inner);

        impl $name {
            pub const fn new(value: $inner) -> Self {
                Self(value)
            }

            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

integer_id!(ProcessSessionId, u128);
integer_id!(AuthoritySessionId, u128);
integer_id!(OwnershipEpoch, u64);
integer_id!(HandoffEpoch, u64);
integer_id!(SnapshotSequence, u64);
integer_id!(NativeGeneration, u64);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HostId(Arc<str>);

impl HostId {
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for HostId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for HostId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OwnershipToken {
    pub authority_session_id: AuthoritySessionId,
    pub ownership_epoch: OwnershipEpoch,
    pub owner_host_id: HostId,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct HandoffId {
    pub authority_session_id: AuthoritySessionId,
    pub handoff_epoch: HandoffEpoch,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SnapshotId {
    pub source_process_session_id: ProcessSessionId,
    pub sequence: SnapshotSequence,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClipboardKind {
    Text,
    Empty,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ClipboardData {
    Text(Arc<[u8]>),
    Empty,
    Unavailable(ClipboardReason),
}

impl ClipboardData {
    pub fn text(value: impl Into<Arc<[u8]>>) -> Result<Self, ClipboardReason> {
        let bytes = value.into();
        std::str::from_utf8(&bytes).map_err(|_| ClipboardReason::InvalidUtf8)?;
        Ok(Self::Text(bytes))
    }

    pub fn kind(&self) -> Result<ClipboardKind, ClipboardReason> {
        match self {
            Self::Text(_) => Ok(ClipboardKind::Text),
            Self::Empty => Ok(ClipboardKind::Empty),
            Self::Unavailable(reason) => Err(*reason),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Text(bytes) => bytes.len(),
            Self::Empty | Self::Unavailable(_) => 0,
        }
    }

    pub fn is_explicit_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}

impl fmt::Debug for ClipboardData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(bytes) => f.debug_struct("Text").field("bytes", &bytes.len()).finish(),
            Self::Empty => f.write_str("Empty"),
            Self::Unavailable(reason) => f.debug_tuple("Unavailable").field(reason).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ClipboardReason {
    CapabilityMissing,
    BackendUnavailable,
    PermissionDenied,
    PrivateContent,
    UnsupportedFormat,
    Oversize,
    SourceChanged,
    TargetNotPrepared,
    DestinationChanged,
    StaleAuthoritySession,
    StalePeerSession,
    StaleHandoff,
    StaleOwnerToken,
    Duplicate,
    ChannelUnavailable,
    TransferTimeout,
    ProtocolError,
    IntegrityFailed,
    InvalidUtf8,
    Canceled,
    QueueFull,
    IdentityExhausted,
}

impl ClipboardReason {
    pub const fn code(self) -> &'static str {
        match self {
            Self::CapabilityMissing => "capability_missing",
            Self::BackendUnavailable => "backend_unavailable",
            Self::PermissionDenied => "permission_denied",
            Self::PrivateContent => "private_content",
            Self::UnsupportedFormat => "unsupported_format",
            Self::Oversize => "oversize",
            Self::SourceChanged => "source_changed",
            Self::TargetNotPrepared => "target_not_prepared",
            Self::DestinationChanged => "destination_changed",
            Self::StaleAuthoritySession => "stale_authority_session",
            Self::StalePeerSession => "stale_peer_session",
            Self::StaleHandoff => "stale_handoff",
            Self::StaleOwnerToken => "stale_owner_token",
            Self::Duplicate => "duplicate",
            Self::ChannelUnavailable => "channel_unavailable",
            Self::TransferTimeout => "transfer_timeout",
            Self::ProtocolError => "protocol_error",
            Self::IntegrityFailed => "integrity_failed",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::Canceled => "canceled",
            Self::QueueFull => "queue_full",
            Self::IdentityExhausted => "identity_exhausted",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AppliedIdentity {
    pub handoff_id: HandoffId,
    pub snapshot_id: SnapshotId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedSnapshot {
    pub handoff_id: HandoffId,
    pub snapshot_id: SnapshotId,
    pub target_token: OwnershipToken,
    pub target_process_session_id: ProcessSessionId,
    pub baseline_generation: NativeGeneration,
    pub data: ClipboardData,
}
