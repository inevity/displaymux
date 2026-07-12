use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    str::FromStr,
};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    Linux,
    Mac,
    Windows,
}

impl Host {
    pub const ALL: [Self; 3] = [Self::Linux, Self::Mac, Self::Windows];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Mac => "mac",
            Self::Windows => "windows",
        }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown host: {0}")]
pub struct HostParseError(String);

impl FromStr for Host {
    type Err = HostParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "linux" => Ok(Self::Linux),
            "mac" => Ok(Self::Mac),
            "windows" => Ok(Self::Windows),
            _ => Err(HostParseError(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TvMode {
    Unknown,
    Fullscreen,
    Multiview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WsState {
    Disconnected,
    Connecting,
    Registering,
    Synchronizing,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolPhase {
    Starting,
    Idle,
    Waking,
    Switching,
    GrantPending,
    RemoteOwned,
    FallbackCommandPending,
    FallbackVerifying,
    FallbackDeferred,
    MultiviewChanging,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerReadiness {
    pub online: bool,
    pub keyboard_ready: bool,
    pub pointer_ready: bool,
    pub session_epoch: u64,
    pub observed_at_ms: u64,
}

impl PeerReadiness {
    pub fn bundle_ready(&self, session_epoch: u64) -> bool {
        self.online
            && self.keyboard_ready
            && self.pointer_ready
            && self.session_epoch == session_epoch
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseIdentity {
    pub lease_id: String,
    pub lease_epoch: u64,
    pub peer_session_epoch: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantIdentity {
    pub grant_epoch: u64,
    pub switch_epoch: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Waking,
    Switching,
    Grant,
    Committed,
    Denied,
    Cancelled,
    Fallback,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnterRequest {
    pub request_id: String,
    pub client_id: String,
    pub target: Host,
    pub request_epoch: u64,
    pub lease: LeaseIdentity,
    pub status: RequestStatus,
    pub switch_epoch: Option<u64>,
    pub grant: Option<GrantIdentity>,
    pub deadline_ms: u64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveSession {
    pub request_id: String,
    pub target: Host,
    pub request_epoch: u64,
    pub switch_epoch: u64,
    pub lease: LeaseIdentity,
    pub renewed_until_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalObservation {
    pub present: bool,
    pub switch_epoch: u64,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolState {
    pub server_host: Host,
    pub ws_state: WsState,
    pub subscribe_active: bool,
    pub synchronized: bool,
    pub phase: ProtocolPhase,
    pub tv_mode: TvMode,
    pub commanded_input: Option<Host>,
    pub observed_input: Option<Host>,
    pub input_signal: BTreeMap<Host, SignalObservation>,
    pub peers: BTreeMap<Host, PeerReadiness>,
    pub request_epoch: u64,
    pub switch_epoch: u64,
    pub verified_epoch: Option<u64>,
    pub grant_epoch: u64,
    pub keyboard_owner: Host,
    pub pointer_owner: Host,
    pub active_request: Option<EnterRequest>,
    pub request_history: VecDeque<EnterRequest>,
    pub retained_request_limit: usize,
    pub active_session: Option<ActiveSession>,
    pub pending_multiview: Option<bool>,
    pub observation_in_flight: Option<u64>,
    pub phase_deadline_ms: Option<u64>,
    pub next_signal_poll_ms: Option<u64>,
    pub fallback_required: bool,
    pub fallback_reason: Option<String>,
    pub reconnect_total: u64,
    pub switch_count: BTreeMap<Host, u64>,
    pub last_error: Option<String>,
    pub dropped_logs: u64,
}

impl ProtocolState {
    pub fn new(server_host: Host, retained_request_limit: usize) -> Self {
        assert!(
            retained_request_limit > 0,
            "request history must be bounded"
        );
        let input_signal = Host::ALL
            .into_iter()
            .map(|host| (host, SignalObservation::default()))
            .collect();
        let peers = Host::ALL
            .into_iter()
            .map(|host| (host, PeerReadiness::default()))
            .collect();
        let switch_count = Host::ALL.into_iter().map(|host| (host, 0)).collect();
        Self {
            server_host,
            ws_state: WsState::Disconnected,
            subscribe_active: false,
            synchronized: false,
            phase: ProtocolPhase::FallbackDeferred,
            tv_mode: TvMode::Unknown,
            commanded_input: None,
            observed_input: None,
            input_signal,
            peers,
            request_epoch: 0,
            switch_epoch: 0,
            verified_epoch: None,
            grant_epoch: 0,
            keyboard_owner: server_host,
            pointer_owner: server_host,
            active_request: None,
            request_history: VecDeque::with_capacity(retained_request_limit),
            retained_request_limit,
            active_session: None,
            pending_multiview: None,
            observation_in_flight: None,
            phase_deadline_ms: None,
            next_signal_poll_ms: None,
            fallback_required: true,
            fallback_reason: Some("startup_unsynchronized".to_string()),
            reconnect_total: 0,
            switch_count,
            last_error: None,
            dropped_logs: 0,
        }
    }

    pub fn daemon_healthy(&self) -> bool {
        self.ws_state == WsState::Connected && self.subscribe_active && self.synchronized
    }

    pub fn ready(&self) -> bool {
        self.daemon_healthy()
            && !self.fallback_required
            && self.phase != ProtocolPhase::FallbackDeferred
    }

    pub fn request(&self, request_id: &str) -> Option<&EnterRequest> {
        self.active_request
            .as_ref()
            .filter(|request| request.request_id == request_id)
            .or_else(|| {
                self.request_history
                    .iter()
                    .rev()
                    .find(|request| request.request_id == request_id)
            })
    }

    pub(crate) fn archived_request_mut(&mut self, request_id: &str) -> Option<&mut EnterRequest> {
        self.request_history
            .iter_mut()
            .rev()
            .find(|request| request.request_id == request_id)
    }

    pub(crate) fn archive_request(&mut self, request: EnterRequest) {
        if let Some(position) = self
            .request_history
            .iter()
            .position(|existing| existing.request_id == request.request_id)
        {
            self.request_history.remove(position);
        }
        while self.request_history.len() >= self.retained_request_limit {
            self.request_history.pop_front();
        }
        self.request_history.push_back(request);
    }

    pub fn next_deadline_ms(&self) -> Option<u64> {
        [
            self.phase_deadline_ms,
            self.next_signal_poll_ms,
            self.active_request
                .as_ref()
                .map(|request| request.deadline_ms),
            self.active_session
                .as_ref()
                .map(|session| session.renewed_until_ms.min(session.lease.expires_at_ms)),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn validate(&self, now_ms: u64) -> Result<(), InvariantViolation> {
        if self.keyboard_owner != self.pointer_owner {
            return Err(InvariantViolation::SplitInputOwnership);
        }
        if self.fallback_required && self.keyboard_owner != self.server_host {
            return Err(InvariantViolation::FallbackOwnsRemoteInput);
        }
        if self.keyboard_owner != self.server_host {
            let session = self
                .active_session
                .as_ref()
                .ok_or(InvariantViolation::RemoteOwnerWithoutSession)?;
            if session.target != self.keyboard_owner {
                return Err(InvariantViolation::RemoteOwnerSessionMismatch);
            }
            if session.renewed_until_ms <= now_ms || session.lease.expires_at_ms <= now_ms {
                return Err(InvariantViolation::RemoteOwnerLeaseExpired);
            }
            let ready = self
                .peers
                .get(&session.target)
                .is_some_and(|peer| peer.bundle_ready(session.lease.peer_session_epoch));
            if !ready {
                return Err(InvariantViolation::RemoteOwnerNotReady);
            }
            if self.observed_input != Some(session.target)
                || self.verified_epoch != Some(session.switch_epoch)
                || self.tv_mode != TvMode::Fullscreen
                || !self
                    .input_signal
                    .get(&session.target)
                    .is_some_and(|signal| {
                        signal.present && signal.switch_epoch == session.switch_epoch
                    })
            {
                return Err(InvariantViolation::RemoteOwnerWithoutVerifiedDisplay);
            }
        }
        if let Some(request) = &self.active_request {
            if request.status == RequestStatus::Grant {
                let grant = request
                    .grant
                    .as_ref()
                    .ok_or(InvariantViolation::GrantStateWithoutGrant)?;
                if request.switch_epoch != Some(grant.switch_epoch)
                    || self.verified_epoch != Some(grant.switch_epoch)
                {
                    return Err(InvariantViolation::GrantEpochMismatch);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvariantViolation {
    #[error("keyboard and pointer ownership split")]
    SplitInputOwnership,
    #[error("fallback required while remote input remains owned")]
    FallbackOwnsRemoteInput,
    #[error("remote owner has no active bundle session")]
    RemoteOwnerWithoutSession,
    #[error("remote owner does not match active bundle session")]
    RemoteOwnerSessionMismatch,
    #[error("remote owner bundle lease expired")]
    RemoteOwnerLeaseExpired,
    #[error("remote owner peer is not ready for both input paths")]
    RemoteOwnerNotReady,
    #[error("remote owner lacks a fresh verified display observation")]
    RemoteOwnerWithoutVerifiedDisplay,
    #[error("grant state has no grant identity")]
    GrantStateWithoutGrant,
    #[error("grant and verified switch epochs differ")]
    GrantEpochMismatch,
}
