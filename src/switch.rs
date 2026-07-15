use crate::config::SwitchControllerConfig;
use futures::StreamExt;
use lan_mouse_ipc::{ClientHandle, SwitchHost};
use reqwest::{Client, Response, StatusCode, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

const PROTOCOL_VERSION: u16 = 1;
// Controller responses contain one state record. Capping them prevents a
// broken or unauthenticated endpoint from turning control traffic into an
// unbounded allocation.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LeaseIdentity {
    pub(crate) request_id: String,
    pub(crate) lease_id: String,
    pub(crate) lease_epoch: u64,
    pub(crate) peer_session_epoch: u64,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GrantIdentity {
    pub(crate) request_epoch: u64,
    pub(crate) grant_epoch: u64,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GateContext {
    pub(crate) handle: ClientHandle,
    pub(crate) target: SwitchHost,
    pub(crate) lease: LeaseIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EdgeIntentKey {
    pub(crate) handle: ClientHandle,
    pub(crate) target: SwitchHost,
    pub(crate) peer_session_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EdgeIntentDecision {
    Primed,
    AwaitingRetreat,
    Confirmed,
}

#[derive(Clone, Copy, Debug)]
struct EdgeIntent {
    key: EdgeIntentKey,
    rearmed: bool,
    expires_at: Instant,
}

pub(crate) struct EdgeIntentGate {
    valid_for: Duration,
    intent: Option<EdgeIntent>,
}

impl EdgeIntentGate {
    pub(crate) fn new(valid_for: Duration) -> Self {
        Self {
            valid_for,
            intent: None,
        }
    }

    pub(crate) fn candidate(&mut self, key: EdgeIntentKey, now: Instant) -> EdgeIntentDecision {
        self.expire(now);
        if let Some(intent) = self.intent {
            if intent.key == key {
                if intent.rearmed {
                    self.intent = None;
                    return EdgeIntentDecision::Confirmed;
                }
                return EdgeIntentDecision::AwaitingRetreat;
            }
        }

        self.intent = now
            .checked_add(self.valid_for)
            .map(|expires_at| EdgeIntent {
                key,
                rearmed: false,
                expires_at,
            });
        EdgeIntentDecision::Primed
    }

    pub(crate) fn retreat(&mut self, handle: ClientHandle, now: Instant) -> bool {
        self.expire(now);
        let Some(expires_at) = now.checked_add(self.valid_for) else {
            self.intent = None;
            return false;
        };
        let Some(intent) = self.intent.as_mut() else {
            return false;
        };
        if intent.key.handle != handle {
            return false;
        }
        intent.rearmed = true;
        intent.expires_at = expires_at;
        true
    }

    pub(crate) fn remove(&mut self, handle: ClientHandle) {
        if self
            .intent
            .is_some_and(|intent| intent.key.handle == handle)
        {
            self.intent = None;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.intent = None;
    }

    fn expire(&mut self, now: Instant) {
        if self.intent.is_some_and(|intent| intent.expires_at <= now) {
            self.intent = None;
        }
    }
}

impl GateContext {
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.handle == other.handle
            && self.target == other.target
            && self.lease.request_id == other.lease.request_id
            && self.lease.lease_id == other.lease.lease_id
            && self.lease.lease_epoch == other.lease.lease_epoch
            && self.lease.peer_session_epoch == other.lease.peer_session_epoch
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BundleGateState {
    Local,
    Preparing(GateContext),
    GrantArmed {
        context: GateContext,
        grant: GrantIdentity,
    },
    RemoteOwned {
        context: GateContext,
        grant: GrantIdentity,
        renewed_until_ms: u64,
    },
}

impl Default for BundleGateState {
    fn default() -> Self {
        Self::Local
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum GateError {
    #[error("input bundle is already reserved")]
    Busy,
    #[error("peer keyboard and pointer bundle is not ready")]
    PeerNotReady,
    #[error("request or lease identity is invalid")]
    InvalidIdentity,
    #[error("request, lease, or grant identity is stale")]
    StaleIdentity,
    #[error("bundle lease or grant expired")]
    Expired,
    #[error("gate transition is not valid in the current state")]
    InvalidState,
}

#[derive(Debug, Default)]
pub(crate) struct BundleLeaseManager {
    state: BundleGateState,
    next_lease_epoch: u64,
}

impl BundleLeaseManager {
    #[cfg(test)]
    pub(crate) fn state(&self) -> &BundleGateState {
        &self.state
    }

    pub(crate) fn context(&self) -> Option<&GateContext> {
        match &self.state {
            BundleGateState::Local => None,
            BundleGateState::Preparing(context)
            | BundleGateState::GrantArmed { context, .. }
            | BundleGateState::RemoteOwned { context, .. } => Some(context),
        }
    }

    pub(crate) fn reserve(
        &mut self,
        handle: ClientHandle,
        target: SwitchHost,
        request_id: String,
        lease_id: String,
        peer_session_epoch: u64,
        peer_bundle_ready: bool,
        now_ms: u64,
        lease_ttl_ms: u64,
    ) -> Result<GateContext, GateError> {
        if self.state != BundleGateState::Local {
            return Err(GateError::Busy);
        }
        if !peer_bundle_ready || peer_session_epoch == 0 {
            return Err(GateError::PeerNotReady);
        }
        if request_id.is_empty() || lease_id.is_empty() || lease_ttl_ms == 0 {
            return Err(GateError::InvalidIdentity);
        }
        let expires_at_ms = now_ms
            .checked_add(lease_ttl_ms)
            .ok_or(GateError::InvalidIdentity)?;
        self.next_lease_epoch = self
            .next_lease_epoch
            .checked_add(1)
            .ok_or(GateError::InvalidIdentity)?;
        let context = GateContext {
            handle,
            target,
            lease: LeaseIdentity {
                request_id,
                lease_id,
                lease_epoch: self.next_lease_epoch,
                peer_session_epoch,
                expires_at_ms,
            },
        };
        self.state = BundleGateState::Preparing(context.clone());
        Ok(context)
    }

    pub(crate) fn arm_grant(
        &mut self,
        context: &GateContext,
        request_epoch: u64,
        grant_epoch: u64,
        lease_expires_at_ms: u64,
        grant_expires_at_ms: u64,
        peer_bundle_ready: bool,
        peer_session_epoch: u64,
        now_ms: u64,
    ) -> Result<(GateContext, GrantIdentity), GateError> {
        let BundleGateState::Preparing(current) = &self.state else {
            return Err(GateError::InvalidState);
        };
        if current != context {
            return Err(GateError::StaleIdentity);
        }
        if !peer_bundle_ready || current.lease.peer_session_epoch != peer_session_epoch {
            self.state = BundleGateState::Local;
            return Err(GateError::PeerNotReady);
        }
        if current.lease.expires_at_ms <= now_ms
            || lease_expires_at_ms <= now_ms
            || grant_expires_at_ms <= now_ms
        {
            self.state = BundleGateState::Local;
            return Err(GateError::Expired);
        }
        if request_epoch == 0 || grant_epoch == 0 {
            return Err(GateError::InvalidIdentity);
        }

        let grant = GrantIdentity {
            request_epoch,
            grant_epoch,
            expires_at_ms: grant_expires_at_ms,
        };
        let mut context = current.clone();
        context.lease.expires_at_ms = context.lease.expires_at_ms.min(lease_expires_at_ms);
        self.state = BundleGateState::GrantArmed {
            context: context.clone(),
            grant: grant.clone(),
        };
        Ok((context, grant))
    }

    pub(crate) fn commit(
        &mut self,
        handle: ClientHandle,
        lease_epoch: u64,
        peer_bundle_ready: bool,
        peer_session_epoch: u64,
        now_ms: u64,
    ) -> Result<(GateContext, GrantIdentity), GateError> {
        let BundleGateState::GrantArmed { context, grant } = &self.state else {
            return Err(GateError::InvalidState);
        };
        if context.handle != handle || context.lease.lease_epoch != lease_epoch {
            return Err(GateError::StaleIdentity);
        }
        if !peer_bundle_ready || context.lease.peer_session_epoch != peer_session_epoch {
            self.state = BundleGateState::Local;
            return Err(GateError::PeerNotReady);
        }
        if context.lease.expires_at_ms <= now_ms || grant.expires_at_ms <= now_ms {
            self.state = BundleGateState::Local;
            return Err(GateError::Expired);
        }

        let context = context.clone();
        let grant = grant.clone();
        self.state = BundleGateState::RemoteOwned {
            context: context.clone(),
            grant: grant.clone(),
            renewed_until_ms: context.lease.expires_at_ms.min(grant.expires_at_ms),
        };
        Ok((context, grant))
    }

    pub(crate) fn renew(
        &mut self,
        request_id: &str,
        renewed_until_ms: u64,
        peer_bundle_ready: bool,
        peer_session_epoch: u64,
        now_ms: u64,
    ) -> Result<(), GateError> {
        let BundleGateState::RemoteOwned {
            context,
            renewed_until_ms: current_renewal,
            ..
        } = &mut self.state
        else {
            return Err(GateError::InvalidState);
        };
        if context.lease.request_id != request_id {
            return Err(GateError::StaleIdentity);
        }
        if !peer_bundle_ready || context.lease.peer_session_epoch != peer_session_epoch {
            self.state = BundleGateState::Local;
            return Err(GateError::PeerNotReady);
        }
        if context.lease.expires_at_ms <= now_ms || renewed_until_ms <= now_ms {
            self.state = BundleGateState::Local;
            return Err(GateError::Expired);
        }
        context.lease.expires_at_ms = renewed_until_ms;
        *current_renewal = renewed_until_ms;
        Ok(())
    }

    pub(crate) fn expire(&mut self, now_ms: u64) -> Option<GateContext> {
        let expired = match &self.state {
            BundleGateState::Local => false,
            BundleGateState::Preparing(context) => context.lease.expires_at_ms <= now_ms,
            BundleGateState::GrantArmed { context, grant } => {
                context.lease.expires_at_ms <= now_ms || grant.expires_at_ms <= now_ms
            }
            BundleGateState::RemoteOwned {
                context,
                renewed_until_ms,
                ..
            } => context.lease.expires_at_ms <= now_ms || *renewed_until_ms <= now_ms,
        };
        expired.then(|| self.invalidate()).flatten()
    }

    pub(crate) fn invalidate(&mut self) -> Option<GateContext> {
        let previous = std::mem::take(&mut self.state);
        match previous {
            BundleGateState::Local => None,
            BundleGateState::Preparing(context)
            | BundleGateState::GrantArmed { context, .. }
            | BundleGateState::RemoteOwned { context, .. } => Some(context),
        }
    }

    pub(crate) fn deadline_ms(&self) -> Option<u64> {
        match &self.state {
            BundleGateState::Local => None,
            BundleGateState::Preparing(context) => Some(context.lease.expires_at_ms),
            BundleGateState::GrantArmed { context, grant } => {
                Some(context.lease.expires_at_ms.min(grant.expires_at_ms))
            }
            BundleGateState::RemoteOwned {
                context,
                renewed_until_ms,
                ..
            } => Some(context.lease.expires_at_ms.min(*renewed_until_ms)),
        }
    }
}

#[derive(Clone)]
pub(crate) struct SwitchController {
    config: SwitchControllerConfig,
    http: Client,
    started_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PeerBundleReadiness {
    pub(crate) online: bool,
    pub(crate) keyboard_ready: bool,
    pub(crate) pointer_ready: bool,
    pub(crate) session_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedGrant {
    pub(crate) request_epoch: u64,
    pub(crate) grant_epoch: u64,
    pub(crate) lease_expires_at_ms: u64,
    pub(crate) grant_expires_at_ms: u64,
}

#[derive(Debug, Error)]
pub(crate) enum SwitchClientError {
    #[error("switch request was cancelled")]
    Cancelled,
    #[error("switch request deadline expired")]
    Timeout,
    #[error("controller response exceeded the protocol size limit")]
    ResponseTooLarge,
    #[error("controller returned protocol version {0}")]
    ProtocolVersion(u16),
    #[error("controller returned stale or conflicting identity")]
    StaleIdentity,
    #[error("controller denied request: {status}: {reason}")]
    Denied { status: String, reason: String },
    #[error("controller returned HTTP {status}: {code}: {message}")]
    Http {
        status: StatusCode,
        code: String,
        message: String,
    },
    #[error("controller URL cannot accept path segments")]
    InvalidBaseUrl,
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl SwitchClientError {
    pub(crate) fn notification_detail(&self, operation: &str) -> (&'static str, String) {
        if let Self::Denied { status, reason } = self {
            return controller_denial_notification(status, reason);
        }

        let code = match self {
            Self::Timeout => "controller_timeout",
            Self::StaleIdentity => "controller_identity_race",
            Self::Request(error) if error.is_timeout() => "controller_timeout",
            Self::Request(error) if error.is_connect() => "controller_unreachable",
            Self::Cancelled => "controller_cancelled",
            Self::ResponseTooLarge => "controller_response_too_large",
            Self::ProtocolVersion(_) => "controller_protocol_mismatch",
            Self::Denied { .. } => unreachable!("handled above"),
            Self::Http { .. } => "controller_http_error",
            Self::InvalidBaseUrl => "controller_configuration_invalid",
            Self::Request(_) => "controller_request_failed",
            Self::Json(_) => "controller_response_invalid",
        };
        (code, format!("{operation}: {self}"))
    }
}

fn controller_denial_notification(status: &str, reason: &str) -> (&'static str, String) {
    let (code, detail) = match reason {
        "target_signal_absent" => (
            "target_signal_absent",
            "The TV selected the target input, but did not detect an active HDMI signal before verification timed out",
        ),
        "target_signal_stale" => (
            "target_signal_stale",
            "The TV returned HDMI signal state from an older switch attempt, so ownership was not transferred",
        ),
        "target_signal_not_observed" => (
            "target_signal_not_observed",
            "The TV did not return HDMI signal state for the target input before verification timed out",
        ),
        "target_input_not_observed" => (
            "target_input_not_observed",
            "The TV did not confirm that it reached the target input before verification timed out",
        ),
        "target_not_fullscreen" => (
            "target_not_fullscreen",
            "The TV did not confirm fullscreen mode before verification timed out",
        ),
        "target_verification_timeout" | "observation_timeout" => (
            "target_verification_timeout",
            "The TV did not confirm fullscreen mode, the target input, and an active HDMI signal before verification timed out",
        ),
        _ => {
            return (
                "controller_denied",
                format!("The TV controller ended the request as {status}: {reason}"),
            );
        }
    };
    (code, detail.to_string())
}

impl SwitchController {
    pub(crate) fn new(config: SwitchControllerConfig) -> Result<Self, SwitchClientError> {
        let http = Client::builder()
            .timeout(Duration::from_millis(config.http_timeout_ms))
            .no_proxy()
            .build()?;
        Ok(Self {
            config,
            http,
            started_at: Instant::now(),
        })
    }

    pub(crate) fn server_host(&self) -> SwitchHost {
        self.config.server_host
    }

    pub(crate) fn local_host(&self) -> SwitchHost {
        self.config.local_host
    }

    pub(crate) fn lease_ttl_ms(&self) -> u64 {
        self.config.lease_ttl_ms
    }

    pub(crate) fn edge_double_tap_timeout(&self) -> Duration {
        Duration::from_millis(self.config.edge_double_tap_ms)
    }

    pub(crate) fn renew_interval(&self) -> Duration {
        Duration::from_millis(self.config.renew_interval_ms)
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.started_at
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    pub(crate) async fn prepare(
        &self,
        context: &GateContext,
        readiness: PeerBundleReadiness,
        cancellation: &CancellationToken,
    ) -> Result<PreparedGrant, SwitchClientError> {
        let deadline_ms = self
            .now_ms()
            .checked_add(self.config.request_timeout_ms)
            .ok_or(SwitchClientError::Timeout)?;
        self.publish_readiness(context.target, readiness, cancellation)
            .await?;
        if self.now_ms() >= deadline_ms {
            return Err(SwitchClientError::Timeout);
        }
        let body = CreateEnterRequest {
            client_id: self.config.local_host.to_string(),
            request_id: context.lease.request_id.clone(),
            lease_id: context.lease.lease_id.clone(),
            lease_epoch: context.lease.lease_epoch,
            peer_session_epoch: context.lease.peer_session_epoch,
            lease_ttl_ms: self.config.lease_ttl_ms,
        };
        let mut result = self
            .request_enter(
                self.http
                    .post(self.endpoint(["enter", &context.target.to_string()])?)
                    .bearer_auth(&self.config.token)
                    .json(&body),
                cancellation,
            )
            .await?;

        loop {
            self.validate_request(context, &result.envelope.data)?;
            match result.envelope.data.status {
                RemoteRequestStatus::Grant => {
                    let grant = result
                        .envelope
                        .data
                        .grant
                        .as_ref()
                        .ok_or(SwitchClientError::StaleIdentity)?;
                    return Ok(PreparedGrant {
                        request_epoch: result.envelope.data.request_epoch,
                        grant_epoch: grant.grant_epoch,
                        lease_expires_at_ms: local_deadline(
                            result.request_started_ms,
                            result.envelope.server_now_ms,
                            result.envelope.data.lease.expires_at_ms,
                        )?,
                        grant_expires_at_ms: local_deadline(
                            result.request_started_ms,
                            result.envelope.server_now_ms,
                            grant.expires_at_ms,
                        )?,
                    });
                }
                RemoteRequestStatus::Waking | RemoteRequestStatus::Switching => {}
                status => {
                    return Err(SwitchClientError::Denied {
                        status: status.as_str().to_string(),
                        reason: result
                            .envelope
                            .data
                            .reason
                            .clone()
                            .unwrap_or_else(|| "no reason supplied".to_string()),
                    });
                }
            }

            let now_ms = self.now_ms();
            if now_ms >= deadline_ms {
                return Err(SwitchClientError::Timeout);
            }
            let sleep_ms = self
                .config
                .poll_interval_ms
                .min(deadline_ms.saturating_sub(now_ms));
            tokio::select! {
                _ = cancellation.cancelled() => return Err(SwitchClientError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {}
            }
            if self.now_ms() >= deadline_ms {
                return Err(SwitchClientError::Timeout);
            }
            result = self
                .request_enter(
                    self.http
                        .get(self.endpoint(["enter", "request", &context.lease.request_id])?)
                        .bearer_auth(&self.config.token),
                    cancellation,
                )
                .await?;
        }
    }

    pub(crate) async fn commit(
        &self,
        context: &GateContext,
        grant: &GrantIdentity,
        cancellation: &CancellationToken,
    ) -> Result<u64, SwitchClientError> {
        let body = CommitRequest {
            request_epoch: grant.request_epoch,
            grant_epoch: grant.grant_epoch,
            lease_id: context.lease.lease_id.clone(),
            lease_epoch: context.lease.lease_epoch,
        };
        let result = self
            .request_enter(
                self.http
                    .post(self.endpoint([
                        "enter",
                        "request",
                        &context.lease.request_id,
                        "commit",
                    ])?)
                    .bearer_auth(&self.config.token)
                    .json(&body),
                cancellation,
            )
            .await?;
        self.validate_request(context, &result.envelope.data)?;
        if result.envelope.data.status != RemoteRequestStatus::Committed {
            return Err(SwitchClientError::Denied {
                status: result.envelope.data.status.as_str().to_string(),
                reason: result
                    .envelope
                    .data
                    .reason
                    .unwrap_or_else(|| "commit was not accepted".to_string()),
            });
        }
        local_deadline(
            result.request_started_ms,
            result.envelope.server_now_ms,
            result.envelope.data.lease.expires_at_ms,
        )
    }

    pub(crate) async fn renew(
        &self,
        context: &GateContext,
        cancellation: &CancellationToken,
    ) -> Result<u64, SwitchClientError> {
        let body = RenewRequest {
            lease_id: context.lease.lease_id.clone(),
            lease_epoch: context.lease.lease_epoch,
            peer_session_epoch: context.lease.peer_session_epoch,
        };
        let request_started_ms = self.now_ms();
        let response = self
            .send(
                self.http
                    .post(self.endpoint([
                        "internal",
                        "enter",
                        "request",
                        &context.lease.request_id,
                        "renew",
                    ])?)
                    .bearer_auth(&self.config.token)
                    .json(&body),
                cancellation,
            )
            .await?;
        let envelope: ApiEnvelope<RenewResponse> = self.decode(response).await?;
        if !envelope.data.renewed {
            return Err(SwitchClientError::Denied {
                status: envelope.data.phase,
                reason: "lease renewal was rejected".to_string(),
            });
        }
        local_deadline(
            request_started_ms,
            envelope.server_now_ms,
            envelope
                .data
                .renewed_until_ms
                .ok_or(SwitchClientError::StaleIdentity)?,
        )
    }

    pub(crate) async fn cancel(
        &self,
        context: &GateContext,
        reason: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), SwitchClientError> {
        let response = self
            .send(
                self.http
                    .post(self.endpoint([
                        "internal",
                        "enter",
                        "request",
                        &context.lease.request_id,
                        "cancel",
                    ])?)
                    .bearer_auth(&self.config.token)
                    .json(&CancelRequest { reason }),
                cancellation,
            )
            .await?;
        let _: ApiEnvelope<RemoteEnterRequest> = self.decode(response).await?;
        Ok(())
    }

    pub(crate) async fn publish_readiness(
        &self,
        host: SwitchHost,
        readiness: PeerBundleReadiness,
        cancellation: &CancellationToken,
    ) -> Result<(), SwitchClientError> {
        let response = self
            .send(
                self.http
                    .post(self.endpoint(["internal", "readiness", &host.to_string()])?)
                    .bearer_auth(&self.config.token)
                    .json(&ReadinessRequest::from(readiness)),
                cancellation,
            )
            .await?;
        let envelope: ApiEnvelope<ReadinessResponse> = self.decode(response).await?;
        if envelope.data.host != host
            || envelope.data.readiness.session_epoch != readiness.session_epoch
            || envelope.data.readiness.online != readiness.online
            || envelope.data.readiness.keyboard_ready != readiness.keyboard_ready
            || envelope.data.readiness.pointer_ready != readiness.pointer_ready
        {
            return Err(SwitchClientError::StaleIdentity);
        }
        Ok(())
    }

    fn endpoint<const N: usize>(&self, segments: [&str; N]) -> Result<Url, SwitchClientError> {
        let mut url = self.config.url.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| SwitchClientError::InvalidBaseUrl)?;
        path.pop_if_empty();
        path.extend(segments);
        drop(path);
        Ok(url)
    }

    async fn request_enter(
        &self,
        request: reqwest::RequestBuilder,
        cancellation: &CancellationToken,
    ) -> Result<TimedEnvelope<RemoteEnterRequest>, SwitchClientError> {
        let request_started_ms = self.now_ms();
        let response = self.send(request, cancellation).await?;
        let envelope = self.decode(response).await?;
        Ok(TimedEnvelope {
            envelope,
            request_started_ms,
        })
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cancellation: &CancellationToken,
    ) -> Result<Response, SwitchClientError> {
        tokio::select! {
            _ = cancellation.cancelled() => Err(SwitchClientError::Cancelled),
            response = request.send() => Ok(response?),
        }
    }

    async fn decode<T: DeserializeOwned>(
        &self,
        response: Response,
    ) -> Result<ApiEnvelope<T>, SwitchClientError> {
        let status = response.status();
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(SwitchClientError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        if let Ok(envelope) = serde_json::from_slice::<ApiEnvelope<T>>(&body) {
            if envelope.protocol_version != PROTOCOL_VERSION {
                return Err(SwitchClientError::ProtocolVersion(
                    envelope.protocol_version,
                ));
            }
            return Ok(envelope);
        }
        if let Ok(error) = serde_json::from_slice::<ApiError>(&body) {
            if error.protocol_version != PROTOCOL_VERSION {
                return Err(SwitchClientError::ProtocolVersion(error.protocol_version));
            }
            return Err(SwitchClientError::Http {
                status,
                code: error.code,
                message: error.message,
            });
        }
        if !status.is_success() {
            return Err(SwitchClientError::Http {
                status,
                code: "invalid_error_response".to_string(),
                message: "controller returned an unparseable error body".to_string(),
            });
        }
        match serde_json::from_slice::<ApiEnvelope<T>>(&body) {
            Err(error) => Err(error.into()),
            Ok(_) => unreachable!("successful parse returned above"),
        }
    }

    fn validate_request(
        &self,
        context: &GateContext,
        request: &RemoteEnterRequest,
    ) -> Result<(), SwitchClientError> {
        if request.request_id != context.lease.request_id
            || request.target != context.target
            || request.lease.lease_id != context.lease.lease_id
            || request.lease.lease_epoch != context.lease.lease_epoch
            || request.lease.peer_session_epoch != context.lease.peer_session_epoch
            || request.request_epoch == 0
        {
            return Err(SwitchClientError::StaleIdentity);
        }
        Ok(())
    }
}

fn local_deadline(
    request_started_ms: u64,
    server_now_ms: u64,
    remote_deadline_ms: u64,
) -> Result<u64, SwitchClientError> {
    let remaining_ms = remote_deadline_ms
        .checked_sub(server_now_ms)
        .filter(|remaining| *remaining > 0)
        .ok_or(SwitchClientError::Timeout)?;
    request_started_ms
        .checked_add(remaining_ms)
        .ok_or(SwitchClientError::Timeout)
}

struct TimedEnvelope<T> {
    envelope: ApiEnvelope<T>,
    request_started_ms: u64,
}

#[derive(Deserialize)]
struct ApiEnvelope<T> {
    protocol_version: u16,
    server_now_ms: u64,
    data: T,
}

#[derive(Deserialize)]
struct ApiError {
    protocol_version: u16,
    code: String,
    message: String,
}

#[derive(Serialize)]
struct CreateEnterRequest {
    client_id: String,
    request_id: String,
    lease_id: String,
    lease_epoch: u64,
    peer_session_epoch: u64,
    lease_ttl_ms: u64,
}

#[derive(Serialize)]
struct CommitRequest {
    request_epoch: u64,
    grant_epoch: u64,
    lease_id: String,
    lease_epoch: u64,
}

#[derive(Serialize)]
struct RenewRequest {
    lease_id: String,
    lease_epoch: u64,
    peer_session_epoch: u64,
}

#[derive(Serialize)]
struct CancelRequest<'a> {
    reason: &'a str,
}

#[derive(Serialize)]
struct ReadinessRequest {
    online: bool,
    keyboard_ready: bool,
    pointer_ready: bool,
    session_epoch: u64,
}

impl From<PeerBundleReadiness> for ReadinessRequest {
    fn from(readiness: PeerBundleReadiness) -> Self {
        Self {
            online: readiness.online,
            keyboard_ready: readiness.keyboard_ready,
            pointer_ready: readiness.pointer_ready,
            session_epoch: readiness.session_epoch,
        }
    }
}

#[derive(Deserialize)]
struct RemoteEnterRequest {
    request_id: String,
    target: SwitchHost,
    request_epoch: u64,
    lease: RemoteLeaseIdentity,
    status: RemoteRequestStatus,
    grant: Option<RemoteGrantIdentity>,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct RemoteLeaseIdentity {
    lease_id: String,
    lease_epoch: u64,
    peer_session_epoch: u64,
    expires_at_ms: u64,
}

#[derive(Deserialize)]
struct RemoteGrantIdentity {
    grant_epoch: u64,
    expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RemoteRequestStatus {
    Waking,
    Switching,
    Grant,
    Committed,
    Denied,
    Cancelled,
    Fallback,
    Expired,
}

impl RemoteRequestStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Waking => "waking",
            Self::Switching => "switching",
            Self::Grant => "grant",
            Self::Committed => "committed",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::Fallback => "fallback",
            Self::Expired => "expired",
        }
    }
}

#[derive(Deserialize)]
struct RenewResponse {
    renewed: bool,
    renewed_until_ms: Option<u64>,
    phase: String,
}

#[derive(Deserialize)]
struct ReadinessResponse {
    host: SwitchHost,
    readiness: RemoteReadiness,
}

#[derive(Deserialize)]
struct RemoteReadiness {
    online: bool,
    keyboard_ready: bool,
    pointer_ready: bool,
    session_epoch: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_details_distinguish_timeout_and_identity_race() {
        let timeout = SwitchClientError::Timeout.notification_detail("Switch preparation failed");
        assert_eq!(timeout.0, "controller_timeout");
        assert!(timeout.1.contains("deadline expired"));

        let stale =
            SwitchClientError::StaleIdentity.notification_detail("Switch preparation failed");
        assert_eq!(stale.0, "controller_identity_race");
        assert!(stale.1.contains("stale or conflicting identity"));
    }

    #[test]
    fn notification_detail_preserves_controller_verification_cause() {
        let denied = SwitchClientError::Denied {
            status: "fallback".to_string(),
            reason: "target_signal_absent".to_string(),
        }
        .notification_detail("TV switch preparation failed");

        assert_eq!(denied.0, "target_signal_absent");
        assert!(denied.1.contains("did not detect an active HDMI signal"));
        assert!(!denied.1.contains("TV switch preparation failed"));
    }

    fn edge_key(handle: ClientHandle, session_epoch: u64) -> EdgeIntentKey {
        EdgeIntentKey {
            handle,
            target: SwitchHost::Mac,
            peer_session_epoch: session_epoch,
        }
    }

    #[test]
    fn edge_intent_requires_retreat_before_matching_second_entry() {
        let now = Instant::now();
        let mut gate = EdgeIntentGate::new(Duration::from_millis(500));
        let key = edge_key(4, 22);

        assert_eq!(gate.candidate(key, now), EdgeIntentDecision::Primed);
        assert_eq!(
            gate.candidate(key, now),
            EdgeIntentDecision::AwaitingRetreat
        );
        assert!(gate.retreat(4, now));
        assert_eq!(gate.candidate(key, now), EdgeIntentDecision::Confirmed);
        assert_eq!(gate.candidate(key, now), EdgeIntentDecision::Primed);
    }

    #[test]
    fn stale_or_different_edge_intent_cannot_confirm() {
        let now = Instant::now();
        let mut gate = EdgeIntentGate::new(Duration::from_millis(10));

        assert_eq!(
            gate.candidate(edge_key(4, 22), now),
            EdgeIntentDecision::Primed
        );
        assert!(!gate.retreat(5, now));
        assert!(gate.retreat(4, now));
        assert_eq!(
            gate.candidate(edge_key(4, 23), now),
            EdgeIntentDecision::Primed
        );
        assert!(gate.retreat(4, now));
        assert_eq!(
            gate.candidate(
                edge_key(4, 23),
                now.checked_add(Duration::from_millis(10)).unwrap(),
            ),
            EdgeIntentDecision::Primed
        );
    }

    #[test]
    fn retreat_starts_a_fresh_confirmation_window() {
        let now = Instant::now();
        let valid_for = Duration::from_millis(500);
        let mut gate = EdgeIntentGate::new(valid_for);
        let key = edge_key(4, 22);
        let retreat_at = now.checked_add(Duration::from_millis(499)).unwrap();
        let confirm_at = now.checked_add(Duration::from_millis(998)).unwrap();

        assert_eq!(gate.candidate(key, now), EdgeIntentDecision::Primed);
        assert!(gate.retreat(4, retreat_at));
        assert_eq!(
            gate.candidate(key, confirm_at),
            EdgeIntentDecision::Confirmed
        );
    }

    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn reserve(manager: &mut BundleLeaseManager) -> GateContext {
        manager
            .reserve(
                4,
                SwitchHost::Windows,
                "request-1".to_string(),
                "lease-1".to_string(),
                22,
                true,
                100,
                50,
            )
            .unwrap()
    }

    fn arm(manager: &mut BundleLeaseManager, context: &GateContext) -> GrantIdentity {
        manager
            .arm_grant(context, 7, 9, 150, 140, true, 22, 110)
            .unwrap()
            .1
    }

    #[test]
    fn reservation_requires_atomic_peer_bundle() {
        let mut manager = BundleLeaseManager::default();

        let result = manager.reserve(
            4,
            SwitchHost::Windows,
            "request-1".to_string(),
            "lease-1".to_string(),
            22,
            false,
            100,
            50,
        );

        assert_eq!(result, Err(GateError::PeerNotReady));
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn one_bundle_reservation_excludes_competing_target() {
        let mut manager = BundleLeaseManager::default();
        reserve(&mut manager);

        let result = manager.reserve(
            5,
            SwitchHost::Mac,
            "request-2".to_string(),
            "lease-2".to_string(),
            31,
            true,
            101,
            50,
        );

        assert_eq!(result, Err(GateError::Busy));
    }

    #[test]
    fn stale_grant_cannot_replace_active_preparation() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        let mut stale = context.clone();
        stale.lease.request_id = "old-request".to_string();

        assert_eq!(
            manager.arm_grant(&stale, 7, 9, 150, 140, true, 22, 110),
            Err(GateError::StaleIdentity)
        );
        assert_eq!(manager.state(), &BundleGateState::Preparing(context));
    }

    #[test]
    fn readiness_loss_before_grant_returns_local() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);

        assert_eq!(
            manager.arm_grant(&context, 7, 9, 150, 140, false, 22, 110),
            Err(GateError::PeerNotReady)
        );
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn grant_clamps_local_lease_to_daemon_remaining_time() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);

        manager
            .arm_grant(&context, 7, 9, 130, 140, true, 22, 110)
            .unwrap();

        let BundleGateState::GrantArmed { context, .. } = manager.state() else {
            panic!("grant was not armed");
        };
        assert_eq!(context.lease.expires_at_ms, 130);
    }

    #[test]
    fn commit_requires_same_handle_epoch_and_peer_session() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        arm(&mut manager, &context);

        assert_eq!(
            manager.commit(5, context.lease.lease_epoch, true, 22, 115),
            Err(GateError::StaleIdentity)
        );
        assert!(matches!(
            manager.state(),
            BundleGateState::GrantArmed { .. }
        ));

        assert_eq!(
            manager.commit(4, context.lease.lease_epoch, true, 23, 115),
            Err(GateError::PeerNotReady)
        );
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn valid_commit_moves_the_whole_bundle_to_remote_owned() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        let grant = arm(&mut manager, &context);

        let committed = manager
            .commit(4, context.lease.lease_epoch, true, 22, 115)
            .unwrap();

        assert_eq!(committed, (context.clone(), grant.clone()));
        assert_eq!(
            manager.state(),
            &BundleGateState::RemoteOwned {
                context,
                grant,
                renewed_until_ms: 140,
            }
        );
    }

    #[test]
    fn renewal_requires_matching_active_request_and_peer_session() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        arm(&mut manager, &context);
        manager
            .commit(4, context.lease.lease_epoch, true, 22, 115)
            .unwrap();

        assert_eq!(
            manager.renew("old-request", 135, true, 22, 120),
            Err(GateError::StaleIdentity)
        );
        assert!(matches!(
            manager.state(),
            BundleGateState::RemoteOwned { .. }
        ));

        assert_eq!(
            manager.renew("request-1", 135, true, 23, 120),
            Err(GateError::PeerNotReady)
        );
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn valid_renewal_replaces_the_active_lease_deadline() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        let grant = arm(&mut manager, &context);
        manager
            .commit(4, context.lease.lease_epoch, true, 22, 115)
            .unwrap();

        manager.renew("request-1", 200, true, 22, 120).unwrap();
        let mut renewed_context = context;
        renewed_context.lease.expires_at_ms = 200;

        assert_eq!(
            manager.state(),
            &BundleGateState::RemoteOwned {
                context: renewed_context,
                grant,
                renewed_until_ms: 200,
            }
        );
    }

    #[test]
    fn expiry_always_invalidates_to_local() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        arm(&mut manager, &context);

        assert_eq!(manager.expire(140), Some(context));
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn remote_deadline_maps_from_request_start_conservatively() {
        assert_eq!(local_deadline(200, 1_000, 1_300).unwrap(), 500);
        assert!(matches!(
            local_deadline(200, 1_300, 1_300),
            Err(SwitchClientError::Timeout)
        ));
    }

    #[test]
    fn parses_versioned_grant_envelope() {
        let envelope: ApiEnvelope<RemoteEnterRequest> = serde_json::from_value(serde_json::json!({
            "protocol_version": 1,
            "server_now_ms": 1000,
            "data": {
                "request_id": "request-1",
                "target": "windows",
                "request_epoch": 7,
                "lease": {
                    "lease_id": "lease-1",
                    "lease_epoch": 3,
                    "peer_session_epoch": 22,
                    "expires_at_ms": 1400
                },
                "status": "grant",
                "grant": {
                    "grant_epoch": 9,
                    "expires_at_ms": 1200
                },
                "reason": null
            }
        }))
        .unwrap();

        assert_eq!(envelope.protocol_version, PROTOCOL_VERSION);
        assert_eq!(envelope.data.target, SwitchHost::Windows);
        assert_eq!(envelope.data.grant.unwrap().grant_epoch, 9);
    }

    #[tokio::test]
    async fn native_controller_runs_fenced_request_lifecycle() {
        let responses = vec![
            json!({
                "protocol_version": 1,
                "server_now_ms": 1_000,
                "data": {
                    "host": "windows",
                    "readiness": {
                        "online": true,
                        "keyboard_ready": true,
                        "pointer_ready": true,
                        "session_epoch": 22
                    }
                }
            }),
            enter_envelope("grant", 5_000, Some(4_000)),
            enter_envelope("committed", 6_000, Some(4_000)),
            json!({
                "protocol_version": 1,
                "server_now_ms": 1_000,
                "data": {
                    "renewed": true,
                    "renewed_until_ms": 7_000,
                    "phase": "remote_owned"
                }
            }),
            enter_envelope("cancelled", 7_000, Some(4_000)),
        ];
        let (base_url, server) = spawn_http_responses(responses).await;
        let controller = SwitchController::new(SwitchControllerConfig {
            url: base_url,
            token: "test-token".to_string(),
            local_host: SwitchHost::Linux,
            server_host: SwitchHost::Linux,
            http_timeout_ms: 1_000,
            request_timeout_ms: 2_000,
            poll_interval_ms: 10,
            edge_double_tap_ms: 500,
            lease_ttl_ms: 5_000,
            renew_interval_ms: 1_000,
        })
        .unwrap();
        let context = GateContext {
            handle: 4,
            target: SwitchHost::Windows,
            lease: LeaseIdentity {
                request_id: "request-1".to_string(),
                lease_id: "lease-1".to_string(),
                lease_epoch: 3,
                peer_session_epoch: 22,
                expires_at_ms: 5_000,
            },
        };
        let readiness = PeerBundleReadiness {
            online: true,
            keyboard_ready: true,
            pointer_ready: true,
            session_epoch: 22,
        };
        let cancellation = CancellationToken::new();

        let prepared = controller
            .prepare(&context, readiness, &cancellation)
            .await
            .unwrap();
        assert_eq!(prepared.request_epoch, 7);
        assert_eq!(prepared.grant_epoch, 9);
        let grant = GrantIdentity {
            request_epoch: prepared.request_epoch,
            grant_epoch: prepared.grant_epoch,
            expires_at_ms: prepared.grant_expires_at_ms,
        };
        assert!(
            controller
                .commit(&context, &grant, &cancellation)
                .await
                .is_ok()
        );
        assert!(controller.renew(&context, &cancellation).await.is_ok());
        controller
            .cancel(&context, "test_complete", &cancellation)
            .await
            .unwrap();

        let requests = server.await.unwrap();
        let request_lines: Vec<_> = requests
            .iter()
            .map(|request| request.lines().next().unwrap())
            .collect();
        assert_eq!(
            request_lines,
            [
                "POST /internal/readiness/windows HTTP/1.1",
                "POST /enter/windows HTTP/1.1",
                "POST /enter/request/request-1/commit HTTP/1.1",
                "POST /internal/enter/request/request-1/renew HTTP/1.1",
                "POST /internal/enter/request/request-1/cancel HTTP/1.1",
            ]
        );
        let create_body: Value = serde_json::from_str(request_body(&requests[1])).unwrap();
        assert_eq!(create_body["request_id"], "request-1");
        assert_eq!(create_body["lease_epoch"], 3);
        assert_eq!(create_body["peer_session_epoch"], 22);
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-token")
        }));
    }

    fn enter_envelope(status: &str, lease_expires_at_ms: u64, grant: Option<u64>) -> Value {
        json!({
            "protocol_version": 1,
            "server_now_ms": 1_000,
            "data": {
                "request_id": "request-1",
                "target": "windows",
                "request_epoch": 7,
                "lease": {
                    "lease_id": "lease-1",
                    "lease_epoch": 3,
                    "peer_session_epoch": 22,
                    "expires_at_ms": lease_expires_at_ms
                },
                "status": status,
                "grant": grant.map(|expires_at_ms| json!({
                    "grant_epoch": 9,
                    "expires_at_ms": expires_at_ms
                })),
                "reason": null
            }
        })
    }

    async fn spawn_http_responses(
        responses: Vec<Value>,
    ) -> (Url, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                requests.push(request);
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
            requests
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), task)
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection closed before request completed");
            request.extend_from_slice(&chunk[..read]);
            let Some(headers_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
            else {
                continue;
            };
            let headers_end = headers_end + 4;
            let headers = String::from_utf8_lossy(&request[..headers_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= headers_end + content_length {
                return String::from_utf8(request).unwrap();
            }
        }
    }

    fn request_body(request: &str) -> &str {
        request.split_once("\r\n\r\n").unwrap().1
    }
}
