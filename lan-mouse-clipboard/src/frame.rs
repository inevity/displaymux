use crate::{
    AuthoritySessionId, ClipboardData, ClipboardKind, ClipboardReason, HandoffEpoch, HandoffId,
    HostId, NativeGeneration, OwnershipEpoch, OwnershipToken, ProcessSessionId, SnapshotId,
    SnapshotSequence,
};
use sha2::{Digest, Sha256};
use std::{io, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

pub const PROTOCOL_VERSION: u16 = 1;
pub const CLIPBOARD_TEXT_V1: u64 = 1;
const MAGIC: [u8; 4] = *b"LMCB";
const PREFIX_BYTES: usize = 24;
const KNOWN_FLAGS: u32 = 0;
const MAX_HOST_ID_BYTES: usize = u8::MAX as usize;
const TOKEN_FIXED_BYTES: usize = 16 + 8 + 1;
const HANDOFF_BYTES: usize = 16 + 8;
const MAX_TOKEN_BYTES: usize = TOKEN_FIXED_BYTES + MAX_HOST_ID_BYTES;
const ENVELOPE_FIXED_BYTES: usize = HANDOFF_BYTES + MAX_TOKEN_BYTES * 2 + 16 * 2;
const MAX_HEADER_BYTES: usize = 16 + MAX_TOKEN_BYTES + 1 + ENVELOPE_FIXED_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum MessageType {
    ClipboardHello = 1,
    AuthorityState = 2,
    PrepareTarget = 3,
    PrepareResult = 4,
    OwnershipActivated = 5,
    SnapshotOffer = 6,
    SnapshotDeliver = 7,
    ApplyResult = 8,
    CancelHandoff = 9,
    ProtocolError = 10,
}

impl TryFrom<u16> for MessageType {
    type Error = FrameError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::ClipboardHello),
            2 => Ok(Self::AuthorityState),
            3 => Ok(Self::PrepareTarget),
            4 => Ok(Self::PrepareResult),
            5 => Ok(Self::OwnershipActivated),
            6 => Ok(Self::SnapshotOffer),
            7 => Ok(Self::SnapshotDeliver),
            8 => Ok(Self::ApplyResult),
            9 => Ok(Self::CancelHandoff),
            10 => Ok(Self::ProtocolError),
            other => Err(FrameError::UnknownMessageType(other)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardHello {
    pub host_id: HostId,
    pub process_session_id: ProcessSessionId,
    pub offered_capabilities: u64,
    pub max_receive_bytes: u64,
}

impl ClipboardHello {
    pub fn supports_text_v1(&self) -> bool {
        self.offered_capabilities & CLIPBOARD_TEXT_V1 != 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandoffEnvelope {
    pub handoff_id: HandoffId,
    pub source_token: OwnershipToken,
    pub source_process_session_id: ProcessSessionId,
    pub target_token: OwnershipToken,
    pub target_process_session_id: ProcessSessionId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityState {
    pub authority_process_session_id: ProcessSessionId,
    pub current_token: OwnershipToken,
    pub active_handoff: Option<HandoffEnvelope>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationResult {
    Completed,
    Skipped(ClipboardReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareResult {
    pub handoff_id: HandoffId,
    pub target_token: OwnershipToken,
    pub target_process_session_id: ProcessSessionId,
    pub baseline_generation: Option<NativeGeneration>,
    pub result: OperationResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardPayload {
    pub handoff: HandoffEnvelope,
    pub snapshot_id: SnapshotId,
    pub data: ClipboardData,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyResult {
    pub handoff_id: HandoffId,
    pub target_token: OwnershipToken,
    pub target_process_session_id: ProcessSessionId,
    pub snapshot_id: SnapshotId,
    pub post_write_generation: Option<NativeGeneration>,
    pub result: OperationResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireMessage {
    ClipboardHello(ClipboardHello),
    AuthorityState(AuthorityState),
    PrepareTarget(HandoffEnvelope),
    PrepareResult(PrepareResult),
    OwnershipActivated(HandoffEnvelope),
    SnapshotOffer(ClipboardPayload),
    SnapshotDeliver(ClipboardPayload),
    ApplyResult(ApplyResult),
    CancelHandoff(HandoffId),
    ProtocolError(ClipboardReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameMetadata {
    pub message_type: MessageType,
    pub handoff_id: Option<HandoffId>,
    pub source_token: Option<OwnershipToken>,
    pub source_process_session_id: Option<ProcessSessionId>,
    pub target_token: Option<OwnershipToken>,
    pub target_process_session_id: Option<ProcessSessionId>,
    pub snapshot_id: Option<SnapshotId>,
}

impl WireMessage {
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::ClipboardHello(_) => MessageType::ClipboardHello,
            Self::AuthorityState(_) => MessageType::AuthorityState,
            Self::PrepareTarget(_) => MessageType::PrepareTarget,
            Self::PrepareResult(_) => MessageType::PrepareResult,
            Self::OwnershipActivated(_) => MessageType::OwnershipActivated,
            Self::SnapshotOffer(_) => MessageType::SnapshotOffer,
            Self::SnapshotDeliver(_) => MessageType::SnapshotDeliver,
            Self::ApplyResult(_) => MessageType::ApplyResult,
            Self::CancelHandoff(_) => MessageType::CancelHandoff,
            Self::ProtocolError(_) => MessageType::ProtocolError,
        }
    }

    pub fn is_payload(&self) -> bool {
        matches!(self, Self::SnapshotOffer(_) | Self::SnapshotDeliver(_))
    }
}

pub struct EncodedFrame {
    prefix: [u8; PREFIX_BYTES],
    header: Vec<u8>,
    payload: Option<Arc<[u8]>>,
}

impl EncodedFrame {
    pub fn encoded_len(&self) -> usize {
        self.prefix.len()
            + self.header.len()
            + self.payload.as_ref().map_or(0, |payload| payload.len())
    }

    #[cfg(test)]
    fn to_vec(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.encoded_len());
        bytes.extend_from_slice(&self.prefix);
        bytes.extend_from_slice(&self.header);
        if let Some(payload) = &self.payload {
            bytes.extend_from_slice(payload);
        }
        bytes
    }
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("clipboard frame I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("clipboard frame operation canceled")]
    Canceled,
    #[error("clipboard frame transfer timed out")]
    TransferTimeout,
    #[error("invalid clipboard frame magic")]
    InvalidMagic,
    #[error("unsupported clipboard protocol version: {0}")]
    UnsupportedVersion(u16),
    #[error("unknown clipboard message type: {0}")]
    UnknownMessageType(u16),
    #[error("unknown clipboard frame flags: {0:#x}")]
    UnknownFlags(u32),
    #[error("invalid clipboard frame header length")]
    InvalidHeaderLength,
    #[error("clipboard payload length exceeds negotiated maximum")]
    PayloadTooLarge,
    #[error("clipboard payload length is not representable on this platform")]
    PlatformLengthOverflow,
    #[error("clipboard message has an unexpected payload")]
    UnexpectedPayload,
    #[error("clipboard payload hash does not match")]
    IntegrityFailed,
    #[error("clipboard text is not valid UTF-8")]
    InvalidUtf8,
    #[error("invalid clipboard host identity")]
    InvalidHostId,
    #[error("invalid clipboard message field")]
    InvalidField,
    #[error("unavailable clipboard data cannot be serialized")]
    UnavailableData,
}

impl FrameError {
    pub const fn reason(&self) -> ClipboardReason {
        match self {
            Self::Io(_) => ClipboardReason::ChannelUnavailable,
            Self::Canceled => ClipboardReason::Canceled,
            Self::TransferTimeout => ClipboardReason::TransferTimeout,
            Self::PayloadTooLarge | Self::PlatformLengthOverflow => ClipboardReason::Oversize,
            Self::IntegrityFailed => ClipboardReason::IntegrityFailed,
            Self::InvalidUtf8 => ClipboardReason::InvalidUtf8,
            Self::UnavailableData => ClipboardReason::BackendUnavailable,
            Self::InvalidMagic
            | Self::UnsupportedVersion(_)
            | Self::UnknownMessageType(_)
            | Self::UnknownFlags(_)
            | Self::InvalidHeaderLength
            | Self::UnexpectedPayload
            | Self::InvalidHostId
            | Self::InvalidField => ClipboardReason::ProtocolError,
        }
    }
}

pub fn encode_message(
    message: &WireMessage,
    max_payload_bytes: usize,
) -> Result<EncodedFrame, FrameError> {
    let mut header = HeaderWriter::default();
    let mut payload = None;
    match message {
        WireMessage::ClipboardHello(hello) => {
            header.host(&hello.host_id)?;
            header.u128(hello.process_session_id.get());
            header.u64(hello.offered_capabilities);
            header.u64(hello.max_receive_bytes);
        }
        WireMessage::AuthorityState(state) => {
            header.u128(state.authority_process_session_id.get());
            header.token(&state.current_token)?;
            header.u8(u8::from(state.active_handoff.is_some()));
            if let Some(handoff) = &state.active_handoff {
                header.envelope(handoff)?;
            }
        }
        WireMessage::PrepareTarget(handoff) | WireMessage::OwnershipActivated(handoff) => {
            header.envelope(handoff)?;
        }
        WireMessage::PrepareResult(result) => {
            header.handoff(result.handoff_id);
            header.token(&result.target_token)?;
            header.u128(result.target_process_session_id.get());
            header.optional_generation(result.baseline_generation);
            header.operation_result(&result.result);
        }
        WireMessage::SnapshotOffer(snapshot) | WireMessage::SnapshotDeliver(snapshot) => {
            header.envelope(&snapshot.handoff)?;
            header.snapshot(snapshot.snapshot_id);
            let bytes = match &snapshot.data {
                ClipboardData::Text(bytes) => {
                    std::str::from_utf8(bytes).map_err(|_| FrameError::InvalidUtf8)?;
                    header.u8(1);
                    bytes.clone()
                }
                ClipboardData::Empty => {
                    header.u8(2);
                    Arc::<[u8]>::from([])
                }
                ClipboardData::Unavailable(_) => return Err(FrameError::UnavailableData),
            };
            if bytes.len() > max_payload_bytes {
                return Err(FrameError::PayloadTooLarge);
            }
            header.bytes(&Sha256::digest(&bytes));
            payload = Some(bytes);
        }
        WireMessage::ApplyResult(result) => {
            header.handoff(result.handoff_id);
            header.token(&result.target_token)?;
            header.u128(result.target_process_session_id.get());
            header.snapshot(result.snapshot_id);
            header.optional_generation(result.post_write_generation);
            header.operation_result(&result.result);
        }
        WireMessage::CancelHandoff(handoff_id) => header.handoff(*handoff_id),
        WireMessage::ProtocolError(reason) => header.reason(*reason),
    }
    if header.len() > MAX_HEADER_BYTES {
        return Err(FrameError::InvalidHeaderLength);
    }
    let payload_length = payload.as_ref().map_or(0, |payload| payload.len() as u64);
    let mut prefix = [0_u8; PREFIX_BYTES];
    prefix[..4].copy_from_slice(&MAGIC);
    prefix[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    prefix[6..8].copy_from_slice(&(message.message_type() as u16).to_be_bytes());
    prefix[8..12].copy_from_slice(&KNOWN_FLAGS.to_be_bytes());
    prefix[12..16].copy_from_slice(&(header.len() as u32).to_be_bytes());
    prefix[16..24].copy_from_slice(&payload_length.to_be_bytes());
    Ok(EncodedFrame {
        prefix,
        header: header.finish(),
        payload,
    })
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &EncodedFrame,
    transfer_budget: Duration,
    cancellation: &CancellationToken,
) -> Result<(), FrameError> {
    let deadline = Instant::now() + transfer_budget;
    write_all_before(writer, &frame.prefix, deadline, cancellation).await?;
    write_all_before(writer, &frame.header, deadline, cancellation).await?;
    if let Some(payload) = &frame.payload {
        write_all_before(writer, payload, deadline, cancellation).await?;
    }
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_payload_bytes: usize,
    transfer_budget: Duration,
    cancellation: &CancellationToken,
) -> Result<WireMessage, FrameError> {
    read_frame_validated(
        reader,
        max_payload_bytes,
        transfer_budget,
        cancellation,
        |_| Ok(()),
    )
    .await
}

pub async fn read_frame_validated<R, E, V>(
    reader: &mut R,
    max_payload_bytes: usize,
    transfer_budget: Duration,
    cancellation: &CancellationToken,
    validator: V,
) -> Result<WireMessage, E>
where
    R: AsyncRead + Unpin,
    E: From<FrameError>,
    V: FnOnce(&FrameMetadata) -> Result<(), E>,
{
    let mut prefix = [0_u8; PREFIX_BYTES];
    let deadline = Instant::now() + transfer_budget;
    read_exact_before(reader, &mut prefix, deadline, cancellation)
        .await
        .map_err(E::from)?;
    read_frame_after_prefix(
        reader,
        max_payload_bytes,
        prefix,
        deadline,
        cancellation,
        validator,
    )
    .await
}

pub(crate) async fn read_next_frame_validated<R, E, V>(
    reader: &mut R,
    max_payload_bytes: usize,
    transfer_budget: Duration,
    cancellation: &CancellationToken,
    validator: V,
) -> Result<WireMessage, E>
where
    R: AsyncRead + Unpin,
    E: From<FrameError>,
    V: FnOnce(&FrameMetadata) -> Result<(), E>,
{
    let mut prefix = [0_u8; PREFIX_BYTES];
    read_exact_until_cancel(reader, &mut prefix[..1], cancellation)
        .await
        .map_err(E::from)?;
    let deadline = Instant::now() + transfer_budget;
    read_exact_before(reader, &mut prefix[1..], deadline, cancellation)
        .await
        .map_err(E::from)?;
    read_frame_after_prefix(
        reader,
        max_payload_bytes,
        prefix,
        deadline,
        cancellation,
        validator,
    )
    .await
}

async fn read_frame_after_prefix<R, E, V>(
    reader: &mut R,
    max_payload_bytes: usize,
    prefix: [u8; PREFIX_BYTES],
    deadline: Instant,
    cancellation: &CancellationToken,
    validator: V,
) -> Result<WireMessage, E>
where
    R: AsyncRead + Unpin,
    E: From<FrameError>,
    V: FnOnce(&FrameMetadata) -> Result<(), E>,
{
    let decoded = decode_prefix(&prefix, max_payload_bytes).map_err(E::from)?;

    let mut header_storage = [0_u8; MAX_HEADER_BYTES];
    let header = &mut header_storage[..decoded.header_length];
    read_exact_before(reader, header, deadline, cancellation)
        .await
        .map_err(E::from)?;
    let parsed = parse_header(decoded.message_type, header).map_err(E::from)?;

    if !matches!(
        decoded.message_type,
        MessageType::SnapshotOffer | MessageType::SnapshotDeliver
    ) && decoded.payload_length != 0
    {
        return Err(E::from(FrameError::UnexpectedPayload));
    }
    validator(&frame_metadata(decoded.message_type, &parsed))?;
    let mut payload = vec![0_u8; decoded.payload_length];
    read_exact_before(reader, &mut payload, deadline, cancellation)
        .await
        .map_err(E::from)?;
    finish_message(decoded.message_type, parsed, payload).map_err(E::from)
}

struct DecodedPrefix {
    message_type: MessageType,
    header_length: usize,
    payload_length: usize,
}

fn decode_prefix(
    prefix: &[u8; PREFIX_BYTES],
    max_payload_bytes: usize,
) -> Result<DecodedPrefix, FrameError> {
    if prefix[..4] != MAGIC {
        return Err(FrameError::InvalidMagic);
    }
    let version = u16::from_be_bytes(prefix[4..6].try_into().expect("prefix slice"));
    if version != PROTOCOL_VERSION {
        return Err(FrameError::UnsupportedVersion(version));
    }
    let message_type = MessageType::try_from(u16::from_be_bytes(
        prefix[6..8].try_into().expect("prefix slice"),
    ))?;
    let flags = u32::from_be_bytes(prefix[8..12].try_into().expect("prefix slice"));
    if flags != KNOWN_FLAGS {
        return Err(FrameError::UnknownFlags(flags));
    }
    let header_length =
        u32::from_be_bytes(prefix[12..16].try_into().expect("prefix slice")) as usize;
    if header_length > MAX_HEADER_BYTES {
        return Err(FrameError::InvalidHeaderLength);
    }
    let payload_length = u64::from_be_bytes(prefix[16..24].try_into().expect("prefix slice"));
    let payload_length =
        checked_payload_length(payload_length, max_payload_bytes, usize::MAX as u64)?;
    Ok(DecodedPrefix {
        message_type,
        header_length,
        payload_length,
    })
}

fn checked_payload_length(
    declared: u64,
    negotiated_max: usize,
    platform_max: u64,
) -> Result<usize, FrameError> {
    if declared > negotiated_max as u64 {
        return Err(FrameError::PayloadTooLarge);
    }
    if declared > platform_max {
        return Err(FrameError::PlatformLengthOverflow);
    }
    usize::try_from(declared).map_err(|_| FrameError::PlatformLengthOverflow)
}

enum ParsedHeader {
    Hello(ClipboardHello),
    Authority(AuthorityState),
    Envelope(HandoffEnvelope),
    PrepareResult(PrepareResult),
    Snapshot {
        handoff: HandoffEnvelope,
        snapshot_id: SnapshotId,
        kind: ClipboardKind,
        digest: [u8; 32],
    },
    ApplyResult(ApplyResult),
    Handoff(HandoffId),
    Reason(ClipboardReason),
}

fn frame_metadata(message_type: MessageType, header: &ParsedHeader) -> FrameMetadata {
    let mut metadata = FrameMetadata {
        message_type,
        handoff_id: None,
        source_token: None,
        source_process_session_id: None,
        target_token: None,
        target_process_session_id: None,
        snapshot_id: None,
    };
    match header {
        ParsedHeader::Authority(state) => {
            if let Some(handoff) = &state.active_handoff {
                metadata.set_envelope(handoff);
            }
        }
        ParsedHeader::Envelope(handoff) => metadata.set_envelope(handoff),
        ParsedHeader::PrepareResult(result) => {
            metadata.handoff_id = Some(result.handoff_id);
            metadata.target_token = Some(result.target_token.clone());
            metadata.target_process_session_id = Some(result.target_process_session_id);
        }
        ParsedHeader::Snapshot {
            handoff,
            snapshot_id,
            ..
        } => {
            metadata.set_envelope(handoff);
            metadata.snapshot_id = Some(*snapshot_id);
        }
        ParsedHeader::ApplyResult(result) => {
            metadata.handoff_id = Some(result.handoff_id);
            metadata.target_token = Some(result.target_token.clone());
            metadata.target_process_session_id = Some(result.target_process_session_id);
            metadata.snapshot_id = Some(result.snapshot_id);
        }
        ParsedHeader::Handoff(handoff_id) => metadata.handoff_id = Some(*handoff_id),
        ParsedHeader::Hello(_) | ParsedHeader::Reason(_) => {}
    }
    metadata
}

impl FrameMetadata {
    fn set_envelope(&mut self, handoff: &HandoffEnvelope) {
        self.handoff_id = Some(handoff.handoff_id);
        self.source_token = Some(handoff.source_token.clone());
        self.source_process_session_id = Some(handoff.source_process_session_id);
        self.target_token = Some(handoff.target_token.clone());
        self.target_process_session_id = Some(handoff.target_process_session_id);
    }
}

fn parse_header(message_type: MessageType, header: &[u8]) -> Result<ParsedHeader, FrameError> {
    let mut reader = HeaderReader::new(header);
    let parsed = match message_type {
        MessageType::ClipboardHello => ParsedHeader::Hello(ClipboardHello {
            host_id: reader.host()?,
            process_session_id: ProcessSessionId::new(reader.u128()?),
            offered_capabilities: reader.u64()?,
            max_receive_bytes: reader.u64()?,
        }),
        MessageType::AuthorityState => {
            let authority_process_session_id = ProcessSessionId::new(reader.u128()?);
            let current_token = reader.token()?;
            let active_handoff = match reader.u8()? {
                0 => None,
                1 => Some(reader.envelope()?),
                _ => return Err(FrameError::InvalidField),
            };
            ParsedHeader::Authority(AuthorityState {
                authority_process_session_id,
                current_token,
                active_handoff,
            })
        }
        MessageType::PrepareTarget | MessageType::OwnershipActivated => {
            ParsedHeader::Envelope(reader.envelope()?)
        }
        MessageType::PrepareResult => ParsedHeader::PrepareResult(PrepareResult {
            handoff_id: reader.handoff()?,
            target_token: reader.token()?,
            target_process_session_id: ProcessSessionId::new(reader.u128()?),
            baseline_generation: reader.optional_generation()?,
            result: reader.operation_result()?,
        }),
        MessageType::SnapshotOffer | MessageType::SnapshotDeliver => {
            let handoff = reader.envelope()?;
            let snapshot_id = reader.snapshot()?;
            let kind = match reader.u8()? {
                1 => ClipboardKind::Text,
                2 => ClipboardKind::Empty,
                _ => return Err(FrameError::InvalidField),
            };
            let digest = reader.array::<32>()?;
            ParsedHeader::Snapshot {
                handoff,
                snapshot_id,
                kind,
                digest,
            }
        }
        MessageType::ApplyResult => ParsedHeader::ApplyResult(ApplyResult {
            handoff_id: reader.handoff()?,
            target_token: reader.token()?,
            target_process_session_id: ProcessSessionId::new(reader.u128()?),
            snapshot_id: reader.snapshot()?,
            post_write_generation: reader.optional_generation()?,
            result: reader.operation_result()?,
        }),
        MessageType::CancelHandoff => ParsedHeader::Handoff(reader.handoff()?),
        MessageType::ProtocolError => ParsedHeader::Reason(reader.reason()?),
    };
    if !reader.is_finished() {
        return Err(FrameError::InvalidHeaderLength);
    }
    Ok(parsed)
}

fn finish_message(
    message_type: MessageType,
    header: ParsedHeader,
    payload: Vec<u8>,
) -> Result<WireMessage, FrameError> {
    match (message_type, header) {
        (MessageType::ClipboardHello, ParsedHeader::Hello(hello)) => {
            require_empty_payload(&payload)?;
            Ok(WireMessage::ClipboardHello(hello))
        }
        (MessageType::AuthorityState, ParsedHeader::Authority(state)) => {
            require_empty_payload(&payload)?;
            Ok(WireMessage::AuthorityState(state))
        }
        (MessageType::PrepareTarget, ParsedHeader::Envelope(handoff)) => {
            require_empty_payload(&payload)?;
            Ok(WireMessage::PrepareTarget(handoff))
        }
        (MessageType::OwnershipActivated, ParsedHeader::Envelope(handoff)) => {
            require_empty_payload(&payload)?;
            Ok(WireMessage::OwnershipActivated(handoff))
        }
        (MessageType::PrepareResult, ParsedHeader::PrepareResult(result)) => {
            require_empty_payload(&payload)?;
            Ok(WireMessage::PrepareResult(result))
        }
        (
            MessageType::SnapshotOffer | MessageType::SnapshotDeliver,
            ParsedHeader::Snapshot {
                handoff,
                snapshot_id,
                kind,
                digest,
            },
        ) => {
            if Sha256::digest(&payload).as_slice() != digest {
                return Err(FrameError::IntegrityFailed);
            }
            let data = match kind {
                ClipboardKind::Text => {
                    std::str::from_utf8(&payload).map_err(|_| FrameError::InvalidUtf8)?;
                    ClipboardData::Text(Arc::from(payload))
                }
                ClipboardKind::Empty if payload.is_empty() => ClipboardData::Empty,
                ClipboardKind::Empty => return Err(FrameError::UnexpectedPayload),
            };
            let clipboard_payload = ClipboardPayload {
                handoff,
                snapshot_id,
                data,
            };
            if message_type == MessageType::SnapshotOffer {
                Ok(WireMessage::SnapshotOffer(clipboard_payload))
            } else {
                Ok(WireMessage::SnapshotDeliver(clipboard_payload))
            }
        }
        (MessageType::ApplyResult, ParsedHeader::ApplyResult(result)) => {
            require_empty_payload(&payload)?;
            Ok(WireMessage::ApplyResult(result))
        }
        (MessageType::CancelHandoff, ParsedHeader::Handoff(handoff_id)) => {
            require_empty_payload(&payload)?;
            Ok(WireMessage::CancelHandoff(handoff_id))
        }
        (MessageType::ProtocolError, ParsedHeader::Reason(reason)) => {
            require_empty_payload(&payload)?;
            Ok(WireMessage::ProtocolError(reason))
        }
        _ => Err(FrameError::InvalidField),
    }
}

fn require_empty_payload(payload: &[u8]) -> Result<(), FrameError> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(FrameError::UnexpectedPayload)
    }
}

async fn read_exact_until_cancel<R: AsyncRead + Unpin>(
    reader: &mut R,
    bytes: &mut [u8],
    cancellation: &CancellationToken,
) -> Result<(), FrameError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(FrameError::Canceled),
        result = reader.read_exact(bytes) => match result {
            Ok(_) => Ok(()),
            Err(error) => Err(FrameError::Io(error)),
        },
    }
}

async fn read_exact_before<R: AsyncRead + Unpin>(
    reader: &mut R,
    bytes: &mut [u8],
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(), FrameError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(FrameError::Canceled),
        result = timeout_at(deadline, reader.read_exact(bytes)) => match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(FrameError::Io(error)),
            Err(_) => Err(FrameError::TransferTimeout),
        },
    }
}

async fn write_all_before<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(), FrameError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(FrameError::Canceled),
        result = timeout_at(deadline, writer.write_all(bytes)) => match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(FrameError::Io(error)),
            Err(_) => Err(FrameError::TransferTimeout),
        },
    }
}

#[derive(Default)]
struct HeaderWriter {
    bytes: Vec<u8>,
}

impl HeaderWriter {
    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes(&value.to_be_bytes());
    }

    fn u128(&mut self, value: u128) {
        self.bytes(&value.to_be_bytes());
    }

    fn host(&mut self, host: &HostId) -> Result<(), FrameError> {
        let bytes = host.as_str().as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_HOST_ID_BYTES {
            return Err(FrameError::InvalidHostId);
        }
        self.u8(bytes.len() as u8);
        self.bytes(bytes);
        Ok(())
    }

    fn token(&mut self, token: &OwnershipToken) -> Result<(), FrameError> {
        self.u128(token.authority_session_id.get());
        self.u64(token.ownership_epoch.get());
        self.host(&token.owner_host_id)
    }

    fn handoff(&mut self, handoff_id: HandoffId) {
        self.u128(handoff_id.authority_session_id.get());
        self.u64(handoff_id.handoff_epoch.get());
    }

    fn snapshot(&mut self, snapshot_id: SnapshotId) {
        self.u128(snapshot_id.source_process_session_id.get());
        self.u64(snapshot_id.sequence.get());
    }

    fn envelope(&mut self, handoff: &HandoffEnvelope) -> Result<(), FrameError> {
        self.handoff(handoff.handoff_id);
        self.token(&handoff.source_token)?;
        self.u128(handoff.source_process_session_id.get());
        self.token(&handoff.target_token)?;
        self.u128(handoff.target_process_session_id.get());
        Ok(())
    }

    fn optional_generation(&mut self, generation: Option<NativeGeneration>) {
        self.u8(u8::from(generation.is_some()));
        self.u64(generation.map_or(0, NativeGeneration::get));
    }

    fn operation_result(&mut self, result: &OperationResult) {
        match result {
            OperationResult::Completed => {
                self.u8(0);
                self.u16(0);
            }
            OperationResult::Skipped(reason) => {
                self.u8(1);
                self.reason(*reason);
            }
        }
    }

    fn reason(&mut self, reason: ClipboardReason) {
        self.u16(reason_to_u16(reason));
    }
}

struct HeaderReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> HeaderReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_finished(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], FrameError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(FrameError::InvalidHeaderLength)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(FrameError::InvalidHeaderLength)?;
        self.offset = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], FrameError> {
        self.take(N)?
            .try_into()
            .map_err(|_| FrameError::InvalidHeaderLength)
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, FrameError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, FrameError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn u128(&mut self) -> Result<u128, FrameError> {
        Ok(u128::from_be_bytes(self.array()?))
    }

    fn host(&mut self) -> Result<HostId, FrameError> {
        let len = self.u8()? as usize;
        if len == 0 {
            return Err(FrameError::InvalidHostId);
        }
        let value = std::str::from_utf8(self.take(len)?).map_err(|_| FrameError::InvalidHostId)?;
        Ok(HostId::from(value))
    }

    fn token(&mut self) -> Result<OwnershipToken, FrameError> {
        Ok(OwnershipToken {
            authority_session_id: AuthoritySessionId::new(self.u128()?),
            ownership_epoch: OwnershipEpoch::new(self.u64()?),
            owner_host_id: self.host()?,
        })
    }

    fn handoff(&mut self) -> Result<HandoffId, FrameError> {
        Ok(HandoffId {
            authority_session_id: AuthoritySessionId::new(self.u128()?),
            handoff_epoch: HandoffEpoch::new(self.u64()?),
        })
    }

    fn snapshot(&mut self) -> Result<SnapshotId, FrameError> {
        Ok(SnapshotId {
            source_process_session_id: ProcessSessionId::new(self.u128()?),
            sequence: SnapshotSequence::new(self.u64()?),
        })
    }

    fn envelope(&mut self) -> Result<HandoffEnvelope, FrameError> {
        Ok(HandoffEnvelope {
            handoff_id: self.handoff()?,
            source_token: self.token()?,
            source_process_session_id: ProcessSessionId::new(self.u128()?),
            target_token: self.token()?,
            target_process_session_id: ProcessSessionId::new(self.u128()?),
        })
    }

    fn optional_generation(&mut self) -> Result<Option<NativeGeneration>, FrameError> {
        let present = self.u8()?;
        let value = self.u64()?;
        match present {
            0 if value == 0 => Ok(None),
            1 => Ok(Some(NativeGeneration::new(value))),
            _ => Err(FrameError::InvalidField),
        }
    }

    fn operation_result(&mut self) -> Result<OperationResult, FrameError> {
        let status = self.u8()?;
        let reason = self.u16()?;
        match (status, reason) {
            (0, 0) => Ok(OperationResult::Completed),
            (1, reason) => Ok(OperationResult::Skipped(reason_from_u16(reason)?)),
            _ => Err(FrameError::InvalidField),
        }
    }

    fn reason(&mut self) -> Result<ClipboardReason, FrameError> {
        reason_from_u16(self.u16()?)
    }
}

fn reason_to_u16(reason: ClipboardReason) -> u16 {
    match reason {
        ClipboardReason::CapabilityMissing => 1,
        ClipboardReason::BackendUnavailable => 2,
        ClipboardReason::PermissionDenied => 3,
        ClipboardReason::PrivateContent => 4,
        ClipboardReason::UnsupportedFormat => 5,
        ClipboardReason::Oversize => 6,
        ClipboardReason::SourceChanged => 7,
        ClipboardReason::TargetNotPrepared => 8,
        ClipboardReason::DestinationChanged => 9,
        ClipboardReason::StaleAuthoritySession => 10,
        ClipboardReason::StalePeerSession => 11,
        ClipboardReason::StaleHandoff => 12,
        ClipboardReason::StaleOwnerToken => 13,
        ClipboardReason::Duplicate => 14,
        ClipboardReason::ChannelUnavailable => 15,
        ClipboardReason::TransferTimeout => 16,
        ClipboardReason::ProtocolError => 17,
        ClipboardReason::IntegrityFailed => 18,
        ClipboardReason::InvalidUtf8 => 19,
        ClipboardReason::Canceled => 20,
        ClipboardReason::QueueFull => 21,
        ClipboardReason::IdentityExhausted => 22,
    }
}

fn reason_from_u16(reason: u16) -> Result<ClipboardReason, FrameError> {
    match reason {
        1 => Ok(ClipboardReason::CapabilityMissing),
        2 => Ok(ClipboardReason::BackendUnavailable),
        3 => Ok(ClipboardReason::PermissionDenied),
        4 => Ok(ClipboardReason::PrivateContent),
        5 => Ok(ClipboardReason::UnsupportedFormat),
        6 => Ok(ClipboardReason::Oversize),
        7 => Ok(ClipboardReason::SourceChanged),
        8 => Ok(ClipboardReason::TargetNotPrepared),
        9 => Ok(ClipboardReason::DestinationChanged),
        10 => Ok(ClipboardReason::StaleAuthoritySession),
        11 => Ok(ClipboardReason::StalePeerSession),
        12 => Ok(ClipboardReason::StaleHandoff),
        13 => Ok(ClipboardReason::StaleOwnerToken),
        14 => Ok(ClipboardReason::Duplicate),
        15 => Ok(ClipboardReason::ChannelUnavailable),
        16 => Ok(ClipboardReason::TransferTimeout),
        17 => Ok(ClipboardReason::ProtocolError),
        18 => Ok(ClipboardReason::IntegrityFailed),
        19 => Ok(ClipboardReason::InvalidUtf8),
        20 => Ok(ClipboardReason::Canceled),
        21 => Ok(ClipboardReason::QueueFull),
        22 => Ok(ClipboardReason::IdentityExhausted),
        _ => Err(FrameError::InvalidField),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::AsyncWrite;

    const MAX_PAYLOAD: usize = 64;
    const BUDGET: Duration = Duration::from_secs(1);

    fn token(host: &str, epoch: u64) -> OwnershipToken {
        OwnershipToken {
            authority_session_id: AuthoritySessionId::new(7),
            ownership_epoch: OwnershipEpoch::new(epoch),
            owner_host_id: HostId::from(host),
        }
    }

    fn handoff_id(epoch: u64) -> HandoffId {
        HandoffId {
            authority_session_id: AuthoritySessionId::new(7),
            handoff_epoch: HandoffEpoch::new(epoch),
        }
    }

    fn snapshot_id(sequence: u64) -> SnapshotId {
        SnapshotId {
            source_process_session_id: ProcessSessionId::new(11),
            sequence: SnapshotSequence::new(sequence),
        }
    }

    fn envelope() -> HandoffEnvelope {
        HandoffEnvelope {
            handoff_id: handoff_id(3),
            source_token: token("server", 2),
            source_process_session_id: ProcessSessionId::new(11),
            target_token: token("remote", 3),
            target_process_session_id: ProcessSessionId::new(22),
        }
    }

    fn text_payload() -> ClipboardPayload {
        ClipboardPayload {
            handoff: envelope(),
            snapshot_id: snapshot_id(4),
            data: ClipboardData::text(Arc::<[u8]>::from(&b"alpha\nbeta"[..])).unwrap(),
        }
    }

    fn every_message() -> Vec<WireMessage> {
        vec![
            WireMessage::ClipboardHello(ClipboardHello {
                host_id: HostId::from("server"),
                process_session_id: ProcessSessionId::new(11),
                offered_capabilities: CLIPBOARD_TEXT_V1,
                max_receive_bytes: MAX_PAYLOAD as u64,
            }),
            WireMessage::AuthorityState(AuthorityState {
                authority_process_session_id: ProcessSessionId::new(11),
                current_token: token("server", 2),
                active_handoff: Some(envelope()),
            }),
            WireMessage::PrepareTarget(envelope()),
            WireMessage::PrepareResult(PrepareResult {
                handoff_id: handoff_id(3),
                target_token: token("remote", 3),
                target_process_session_id: ProcessSessionId::new(22),
                baseline_generation: Some(NativeGeneration::new(9)),
                result: OperationResult::Completed,
            }),
            WireMessage::OwnershipActivated(envelope()),
            WireMessage::SnapshotOffer(text_payload()),
            WireMessage::SnapshotDeliver(ClipboardPayload {
                data: ClipboardData::Empty,
                ..text_payload()
            }),
            WireMessage::ApplyResult(ApplyResult {
                handoff_id: handoff_id(3),
                target_token: token("remote", 3),
                target_process_session_id: ProcessSessionId::new(22),
                snapshot_id: snapshot_id(4),
                post_write_generation: Some(NativeGeneration::new(10)),
                result: OperationResult::Skipped(ClipboardReason::DestinationChanged),
            }),
            WireMessage::CancelHandoff(handoff_id(3)),
            WireMessage::ProtocolError(ClipboardReason::ProtocolError),
        ]
    }

    async fn decode_bytes(bytes: &[u8], max_payload: usize) -> Result<WireMessage, FrameError> {
        let mut reader = bytes;
        read_frame(&mut reader, max_payload, BUDGET, &CancellationToken::new()).await
    }

    #[tokio::test]
    async fn every_message_round_trips_with_exact_header_consumption() {
        for message in every_message() {
            let frame = encode_message(&message, MAX_PAYLOAD).unwrap();
            assert_eq!(
                decode_bytes(&frame.to_vec(), MAX_PAYLOAD).await.unwrap(),
                message
            );
        }
    }

    #[tokio::test]
    async fn one_byte_fragments_cover_prefix_header_and_payload_boundaries() {
        let message = WireMessage::SnapshotDeliver(text_payload());
        let frame = encode_message(&message, MAX_PAYLOAD).unwrap();
        let bytes = frame.to_vec();
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let write_task = tokio::spawn(async move {
            for byte in bytes {
                writer.write_all(&[byte]).await.unwrap();
            }
        });
        let decoded = read_frame(&mut reader, MAX_PAYLOAD, BUDGET, &CancellationToken::new())
            .await
            .unwrap();
        write_task.await.unwrap();
        assert_eq!(decoded, message);
    }

    struct PartialWriter {
        bytes: Vec<u8>,
        chunk: usize,
    }

    impl AsyncWrite for PartialWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let count = bytes.len().min(self.chunk);
            self.bytes.extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn partial_writes_emit_one_complete_frame() {
        let message = WireMessage::SnapshotOffer(text_payload());
        let frame = encode_message(&message, MAX_PAYLOAD).unwrap();
        let mut writer = PartialWriter {
            bytes: Vec::new(),
            chunk: 1,
        };
        write_frame(&mut writer, &frame, BUDGET, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(writer.bytes.len(), frame.encoded_len());
        assert_eq!(
            decode_bytes(&writer.bytes, MAX_PAYLOAD).await.unwrap(),
            message
        );
    }

    #[tokio::test]
    async fn eof_at_every_byte_boundary_is_rejected() {
        let bytes = encode_message(&WireMessage::SnapshotOffer(text_payload()), MAX_PAYLOAD)
            .unwrap()
            .to_vec();
        for end in 0..bytes.len() {
            assert!(matches!(
                decode_bytes(&bytes[..end], MAX_PAYLOAD).await,
                Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof
            ));
        }
    }

    #[tokio::test]
    async fn malformed_prefix_fields_fail_before_header_or_payload_read() {
        let base = encode_message(
            &WireMessage::ClipboardHello(ClipboardHello {
                host_id: HostId::from("server"),
                process_session_id: ProcessSessionId::new(11),
                offered_capabilities: CLIPBOARD_TEXT_V1,
                max_receive_bytes: MAX_PAYLOAD as u64,
            }),
            MAX_PAYLOAD,
        )
        .unwrap()
        .to_vec();
        async fn malformed(base: &[u8], offset: usize, replacement: &[u8]) -> FrameError {
            let mut bytes = base[..PREFIX_BYTES].to_vec();
            bytes[offset..offset + replacement.len()].copy_from_slice(replacement);
            decode_bytes(&bytes, MAX_PAYLOAD).await.unwrap_err()
        }
        assert!(matches!(
            malformed(&base, 0, b"X").await,
            FrameError::InvalidMagic
        ));
        assert!(matches!(
            malformed(&base, 4, &2_u16.to_be_bytes()).await,
            FrameError::UnsupportedVersion(2)
        ));
        assert!(matches!(
            malformed(&base, 6, &99_u16.to_be_bytes()).await,
            FrameError::UnknownMessageType(99)
        ));
        assert!(matches!(
            malformed(&base, 8, &1_u32.to_be_bytes()).await,
            FrameError::UnknownFlags(1)
        ));
        assert!(matches!(
            malformed(&base, 12, &((MAX_HEADER_BYTES + 1) as u32).to_be_bytes(),).await,
            FrameError::InvalidHeaderLength
        ));
        assert!(matches!(
            malformed(&base, 16, &((MAX_PAYLOAD + 1) as u64).to_be_bytes()).await,
            FrameError::PayloadTooLarge
        ));
    }

    #[test]
    fn simulated_narrow_platform_rejects_unrepresentable_length() {
        assert!(matches!(
            checked_payload_length(u32::MAX as u64 + 1, usize::MAX, u32::MAX as u64),
            Err(FrameError::PlatformLengthOverflow)
        ));
    }

    #[tokio::test]
    async fn exact_limit_is_accepted_and_one_byte_over_is_rejected() {
        let exact = WireMessage::SnapshotDeliver(ClipboardPayload {
            handoff: envelope(),
            snapshot_id: snapshot_id(4),
            data: ClipboardData::Text(Arc::from(vec![b'a'; MAX_PAYLOAD])),
        });
        let frame = encode_message(&exact, MAX_PAYLOAD).unwrap();
        assert_eq!(
            decode_bytes(&frame.to_vec(), MAX_PAYLOAD).await.unwrap(),
            exact
        );

        let over = WireMessage::SnapshotDeliver(ClipboardPayload {
            handoff: envelope(),
            snapshot_id: snapshot_id(4),
            data: ClipboardData::Text(Arc::from(vec![b'a'; MAX_PAYLOAD + 1])),
        });
        assert!(matches!(
            encode_message(&over, MAX_PAYLOAD),
            Err(FrameError::PayloadTooLarge)
        ));
    }

    #[tokio::test]
    async fn hash_mismatch_and_valid_hash_with_invalid_utf8_are_distinct() {
        let mut frame =
            encode_message(&WireMessage::SnapshotDeliver(text_payload()), MAX_PAYLOAD).unwrap();
        let payload = frame.payload.as_mut().unwrap();
        let mut corrupted = payload.to_vec();
        corrupted[0] ^= 0xff;
        *payload = Arc::from(corrupted.clone());
        assert!(matches!(
            decode_bytes(&frame.to_vec(), MAX_PAYLOAD).await,
            Err(FrameError::IntegrityFailed)
        ));

        let digest = Sha256::digest(&corrupted);
        let digest_offset = frame.header.len() - 32;
        frame.header[digest_offset..].copy_from_slice(&digest);
        assert!(matches!(
            decode_bytes(&frame.to_vec(), MAX_PAYLOAD).await,
            Err(FrameError::InvalidUtf8)
        ));
    }

    #[tokio::test]
    async fn exact_header_length_and_empty_kind_are_enforced() {
        let message = WireMessage::SnapshotDeliver(ClipboardPayload {
            data: ClipboardData::Empty,
            ..text_payload()
        });
        let mut bytes = encode_message(&message, MAX_PAYLOAD).unwrap().to_vec();
        let header_length = u32::from_be_bytes(bytes[12..16].try_into().unwrap());
        bytes[12..16].copy_from_slice(&(header_length - 1).to_be_bytes());
        assert!(matches!(
            decode_bytes(&bytes, MAX_PAYLOAD).await,
            Err(FrameError::InvalidHeaderLength | FrameError::InvalidField)
        ));

        let mut frame = encode_message(&message, MAX_PAYLOAD).unwrap();
        frame.payload = Some(Arc::from(&b"x"[..]));
        frame.prefix[16..24].copy_from_slice(&1_u64.to_be_bytes());
        let digest_offset = frame.header.len() - 32;
        frame.header[digest_offset..].copy_from_slice(&Sha256::digest(b"x"));
        assert!(matches!(
            decode_bytes(&frame.to_vec(), MAX_PAYLOAD).await,
            Err(FrameError::UnexpectedPayload)
        ));
    }

    #[tokio::test]
    async fn identity_validation_runs_before_payload_read_or_allocation() {
        let frame =
            encode_message(&WireMessage::SnapshotDeliver(text_payload()), MAX_PAYLOAD).unwrap();
        let bytes = frame.to_vec();
        let metadata_end = PREFIX_BYTES + frame.header.len();
        let mut reader = &bytes[..metadata_end];
        let error = read_frame_validated::<_, FrameError, _>(
            &mut reader,
            MAX_PAYLOAD,
            BUDGET,
            &CancellationToken::new(),
            |metadata| {
                assert_eq!(metadata.message_type, MessageType::SnapshotDeliver);
                assert_eq!(metadata.handoff_id, Some(handoff_id(3)));
                assert_eq!(
                    metadata.source_process_session_id,
                    Some(ProcessSessionId::new(11))
                );
                Err(FrameError::InvalidField)
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, FrameError::InvalidField));
    }

    #[tokio::test]
    async fn cancellation_and_deadline_stop_incomplete_reads() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (_writer, mut pending) = tokio::io::duplex(1);
        assert!(matches!(
            read_frame(&mut pending, MAX_PAYLOAD, BUDGET, &cancellation).await,
            Err(FrameError::Canceled)
        ));

        let (_writer, mut pending) = tokio::io::duplex(1);
        assert!(matches!(
            read_frame(
                &mut pending,
                MAX_PAYLOAD,
                Duration::from_millis(1),
                &CancellationToken::new(),
            )
            .await,
            Err(FrameError::TransferTimeout)
        ));
    }

    #[test]
    fn payload_debug_output_contains_size_not_text() {
        let message = WireMessage::SnapshotDeliver(text_payload());
        let debug = format!("{message:?}");
        assert!(debug.contains("bytes: 10"));
        assert!(!debug.contains("alpha"));
        assert!(!debug.contains("beta"));
    }

    #[test]
    fn frame_failures_map_to_stable_public_reasons() {
        assert_eq!(FrameError::Canceled.reason(), ClipboardReason::Canceled);
        assert_eq!(
            FrameError::TransferTimeout.reason(),
            ClipboardReason::TransferTimeout
        );
        assert_eq!(
            FrameError::PayloadTooLarge.reason(),
            ClipboardReason::Oversize
        );
        assert_eq!(
            FrameError::IntegrityFailed.reason(),
            ClipboardReason::IntegrityFailed
        );
        assert_eq!(
            FrameError::InvalidUtf8.reason(),
            ClipboardReason::InvalidUtf8
        );
        assert_eq!(
            FrameError::InvalidMagic.reason(),
            ClipboardReason::ProtocolError
        );
        assert_eq!(
            FrameError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "closed")).reason(),
            ClipboardReason::ChannelUnavailable
        );
    }
}
