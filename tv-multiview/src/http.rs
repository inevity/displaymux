use crate::{
    coordinator::{CoordinatorError, CoordinatorHandle, CoordinatorQueueSnapshot},
    domain::{
        Host, LeaseIdentity, PeerReadiness, ProtocolPhase, ProtocolState, RequestStatus,
        SignalObservation, TvMode, WsState,
    },
    observability::{RuntimeMetrics, RuntimeSnapshot},
    protocol::{Event, ProtocolError},
};
use axum::{
    extract::{rejection::JsonRejection, Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, str::FromStr, sync::Arc};
use tokio::sync::watch;

const PROTOCOL_VERSION: u16 = 1;
const AUTHORIZATION: &str = "authorization";

#[derive(Clone)]
struct AppState {
    coordinator: CoordinatorHandle,
    snapshot_rx: watch::Receiver<Arc<ProtocolState>>,
    controller_token: Arc<str>,
    max_lease_ms: u64,
    runtime_metrics: RuntimeMetrics,
}

struct ApiFailure {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        api_error(self.status, self.code, self.message)
    }
}

pub fn router(
    coordinator: CoordinatorHandle,
    controller_token: String,
    max_lease_ms: u64,
    runtime_metrics: RuntimeMetrics,
) -> Router {
    let snapshot_rx = coordinator.subscribe();
    let state = Arc::new(AppState {
        coordinator,
        snapshot_rx,
        controller_token: controller_token.into(),
        max_lease_ms,
        runtime_metrics,
    });
    let protected = Router::new()
        .route("/ready", get(ready))
        .route("/status", get(status))
        .route("/enter/{target}", post(create_enter))
        .route("/enter/request/{request_id}", get(poll_enter))
        .route("/enter/request/{request_id}/commit", post(commit_enter))
        .route(
            "/internal/enter/request/{request_id}/cancel",
            post(cancel_enter),
        )
        .route(
            "/internal/enter/request/{request_id}/renew",
            post(renew_enter),
        )
        .route("/internal/readiness/{host}", post(update_readiness))
        .route("/multiview/on", post(multiview_on))
        .route("/multiview/off", post(multiview_off))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

async fn authenticate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if let Err(error) = authorize(request.headers(), &state.controller_token) {
        return error.into_response();
    }
    next.run(request).await
}

async fn health() -> &'static str {
    "ok"
}

async fn ready(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    let snapshot = state.snapshot_rx.borrow().clone();
    let status = if snapshot.ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let server_now_ms = state.coordinator.now_ms();
    (
        status,
        Json(ApiEnvelope::new(
            server_now_ms,
            ReadyResponse {
                ready: snapshot.ready(),
                phase: snapshot.phase,
                fallback_required: snapshot.fallback_required,
            },
        )),
    )
        .into_response()
}

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    let now_ms = state.coordinator.now_ms();
    let protocol = (*state.snapshot_rx.borrow()).as_ref().clone();
    let runtime = state.runtime_metrics.snapshot();
    let observation_age_ms = protocol.observed_input.and_then(|host| {
        protocol
            .input_signal
            .get(&host)
            .filter(|observation| observation.observed_at_ms > 0)
            .map(|observation| now_ms.saturating_sub(observation.observed_at_ms))
    });
    let deadline_remaining_ms = protocol
        .next_deadline_ms()
        .map(|deadline| deadline.saturating_sub(now_ms));
    let in_flight_operation = in_flight_operation(&protocol);
    let request = protocol.active_request.as_ref().or_else(|| {
        protocol.active_session.as_ref().and_then(|session| {
            protocol
                .request_history
                .iter()
                .rev()
                .find(|request| request.request_id == session.request_id)
        })
    });
    let request_id = protocol
        .active_request
        .as_ref()
        .map(|request| request.request_id.clone())
        .or_else(|| {
            protocol
                .active_session
                .as_ref()
                .map(|session| session.request_id.clone())
        });
    let pending_switch = protocol
        .active_request
        .as_ref()
        .map(|request| request.target);
    let input_signal = protocol
        .input_signal
        .iter()
        .map(|(host, observation)| (*host, observation.present))
        .collect();
    let remote_online = protocol
        .peers
        .iter()
        .filter(|(host, _)| **host != protocol.server_host)
        .map(|(host, readiness)| (*host, readiness.online))
        .collect();
    Json(ApiEnvelope::new(
        now_ms,
        StatusResponse {
            mode: protocol.tv_mode,
            observed_input: protocol.observed_input,
            commanded_input: protocol.commanded_input,
            healthy: protocol.daemon_healthy(),
            ready: protocol.ready(),
            ws_state: protocol.ws_state,
            subscribe_active: protocol.subscribe_active,
            protocol_phase: protocol.phase,
            request_id,
            request_epoch: protocol.request_epoch,
            switch_epoch: protocol.switch_epoch,
            verified_epoch: protocol.verified_epoch,
            pending_switch,
            switch_timer: deadline_remaining_ms.unwrap_or(0),
            fallback_required: protocol.fallback_required,
            keyboard_owner: protocol.keyboard_owner,
            pointer_owner: protocol.pointer_owner,
            reservation_target: pending_switch,
            grant_epoch: request
                .and_then(|request| request.grant.as_ref())
                .map(|grant| grant.grant_epoch),
            input_signal,
            signal_observations: protocol.input_signal.clone(),
            remote_online,
            peer_readiness: protocol.peers.clone(),
            uptime_seconds: now_ms / 1_000,
            reconnect_total: protocol.reconnect_total,
            switch_count: protocol.switch_count.clone(),
            last_error: protocol.last_error.clone(),
            dropped_logs: runtime.dropped_logs,
            request_history_len: protocol.request_history.len(),
            retained_request_limit: protocol.retained_request_limit,
            queues: state.coordinator.queue_snapshot(),
            runtime,
            observation_age_ms,
            deadline_remaining_ms,
            in_flight_operation,
        },
    ))
    .into_response()
}

fn in_flight_operation(state: &ProtocolState) -> Option<&'static str> {
    if state.observation_in_flight.is_some() {
        return Some("observe");
    }
    match state.phase {
        ProtocolPhase::Waking => Some("wake"),
        ProtocolPhase::Switching | ProtocolPhase::FallbackCommandPending => Some("set_input"),
        ProtocolPhase::Verifying => Some("verify_target"),
        ProtocolPhase::FallbackVerifying => Some("verify_fallback"),
        ProtocolPhase::MultiviewChanging => Some("set_multiview"),
        ProtocolPhase::Starting
        | ProtocolPhase::Idle
        | ProtocolPhase::GrantPending
        | ProtocolPhase::RemoteOwned
        | ProtocolPhase::FallbackDeferred => None,
    }
}

async fn create_enter(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(target): Path<String>,
    body: Result<Json<CreateEnterRequest>, JsonRejection>,
) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    let body = match json_body(body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let target = match Host::from_str(&target) {
        Ok(target) => target,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, "invalid_host", error.to_string()),
    };
    if body.request_id.is_empty()
        || body.client_id.is_empty()
        || body.lease_id.is_empty()
        || body.lease_epoch == 0
        || body.peer_session_epoch == 0
        || body.lease_ttl_ms == 0
        || body.lease_ttl_ms > state.max_lease_ms
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_identity",
            "request, client, lease identity and bounded lease_ttl_ms are required",
        );
    }
    let now_ms = state.coordinator.now_ms();
    let request_id = body.request_id.clone();
    let event = Event::CreateEnter {
        request_id: body.request_id,
        client_id: body.client_id,
        target,
        lease: LeaseIdentity {
            lease_id: body.lease_id,
            lease_epoch: body.lease_epoch,
            peer_session_epoch: body.peer_session_epoch,
            expires_at_ms: now_ms.saturating_add(body.lease_ttl_ms),
        },
    };
    match state.coordinator.apply(event).await {
        Ok(snapshot) => request_response(&snapshot, &request_id, state.coordinator.now_ms()),
        Err(error) => coordinator_error(error),
    }
}

async fn poll_enter(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    request_response(
        &state.snapshot_rx.borrow(),
        &request_id,
        state.coordinator.now_ms(),
    )
}

async fn commit_enter(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    body: Result<Json<CommitRequest>, JsonRejection>,
) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    let body = match json_body(body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if body.request_epoch == 0
        || body.grant_epoch == 0
        || body.lease_id.is_empty()
        || body.lease_epoch == 0
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_identity",
            "current request, grant, and lease identities are required",
        );
    }
    match state
        .coordinator
        .apply(Event::Commit {
            request_id: request_id.clone(),
            request_epoch: body.request_epoch,
            grant_epoch: body.grant_epoch,
            lease_id: body.lease_id,
            lease_epoch: body.lease_epoch,
        })
        .await
    {
        Ok(snapshot) => request_response(&snapshot, &request_id, state.coordinator.now_ms()),
        Err(error) => coordinator_error(error),
    }
}

async fn cancel_enter(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    body: Result<Json<CancelRequest>, JsonRejection>,
) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    let body = match json_body(body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if body.reason.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_reason",
            "cancellation reason is required",
        );
    }
    match state
        .coordinator
        .apply_safety(Event::Cancel {
            request_id: request_id.clone(),
            reason: body.reason,
        })
        .await
    {
        Ok(snapshot) => request_response(&snapshot, &request_id, state.coordinator.now_ms()),
        Err(error) => coordinator_error(error),
    }
}

async fn renew_enter(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    body: Result<Json<RenewRequest>, JsonRejection>,
) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    let body = match json_body(body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if body.lease_id.is_empty() || body.lease_epoch == 0 || body.peer_session_epoch == 0 {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_identity",
            "current lease and peer session identities are required",
        );
    }
    match state
        .coordinator
        .apply_safety(Event::Renew {
            request_id,
            lease_id: body.lease_id,
            lease_epoch: body.lease_epoch,
            peer_session_epoch: body.peer_session_epoch,
        })
        .await
    {
        Ok(snapshot) => Json(ApiEnvelope::new(
            state.coordinator.now_ms(),
            RenewResponse {
                renewed: snapshot.active_session.is_some(),
                renewed_until_ms: snapshot
                    .active_session
                    .as_ref()
                    .map(|session| session.renewed_until_ms),
                phase: snapshot.phase,
            },
        ))
        .into_response(),
        Err(error) => coordinator_error(error),
    }
}

async fn update_readiness(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(host): Path<String>,
    body: Result<Json<ReadinessRequest>, JsonRejection>,
) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    let body = match json_body(body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let host = match Host::from_str(&host) {
        Ok(host) => host,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, "invalid_host", error.to_string()),
    };
    let now_ms = state.coordinator.now_ms();
    match state
        .coordinator
        .apply_safety(Event::PeerReadinessUpdated {
            host,
            readiness: PeerReadiness {
                online: body.online,
                keyboard_ready: body.keyboard_ready,
                pointer_ready: body.pointer_ready,
                session_epoch: body.session_epoch,
                observed_at_ms: now_ms,
            },
        })
        .await
    {
        Ok(snapshot) => Json(ApiEnvelope::new(
            state.coordinator.now_ms(),
            ReadinessResponse {
                host,
                readiness: snapshot.peers.get(&host).cloned().unwrap_or_default(),
            },
        ))
        .into_response(),
        Err(error) => coordinator_error(error),
    }
}

async fn multiview_on(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    multiview(state, headers, true).await
}

async fn multiview_off(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    multiview(state, headers, false).await
}

async fn multiview(state: Arc<AppState>, headers: HeaderMap, enabled: bool) -> Response {
    if let Err(error) = authorize(&headers, &state.controller_token) {
        return error.into_response();
    }
    match state
        .coordinator
        .apply_safety(Event::MultiViewRequested { enabled })
        .await
    {
        Ok(snapshot) => (
            StatusCode::ACCEPTED,
            Json(ApiEnvelope::new(
                state.coordinator.now_ms(),
                MultiViewResponse {
                    enabled,
                    phase: snapshot.phase,
                    switch_epoch: snapshot.switch_epoch,
                },
            )),
        )
            .into_response(),
        Err(error) => coordinator_error(error),
    }
}

fn request_response(state: &ProtocolState, request_id: &str, server_now_ms: u64) -> Response {
    let Some(request) = state.request(request_id).cloned() else {
        return api_error(
            StatusCode::NOT_FOUND,
            "request_not_found",
            "request not found",
        );
    };
    let status = match request.status {
        RequestStatus::Waking | RequestStatus::Switching => StatusCode::ACCEPTED,
        RequestStatus::Grant | RequestStatus::Committed => StatusCode::OK,
        RequestStatus::Denied
        | RequestStatus::Cancelled
        | RequestStatus::Fallback
        | RequestStatus::Expired => StatusCode::CONFLICT,
    };
    (status, Json(ApiEnvelope::new(server_now_ms, request))).into_response()
}

fn coordinator_error(error: CoordinatorError) -> Response {
    match error {
        CoordinatorError::Busy => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "coordinator_busy",
            error.to_string(),
        ),
        CoordinatorError::Closed => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "coordinator_closed",
            error.to_string(),
        ),
        CoordinatorError::Protocol(ProtocolError::Unavailable) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            error.to_string(),
        ),
        CoordinatorError::Protocol(ProtocolError::Busy { active_request_id }) => (
            StatusCode::CONFLICT,
            Json(ApiError {
                protocol_version: PROTOCOL_VERSION,
                code: "request_busy",
                message: "another request is active".to_string(),
                active_request_id: Some(active_request_id),
            }),
        )
            .into_response(),
        CoordinatorError::Protocol(ProtocolError::RequestNotFound) => api_error(
            StatusCode::NOT_FOUND,
            "request_not_found",
            error.to_string(),
        ),
        CoordinatorError::Protocol(
            ProtocolError::TargetNotReady
            | ProtocolError::InvalidLease
            | ProtocolError::StaleIdentity
            | ProtocolError::RequestIdentityConflict
            | ProtocolError::InputNotLocal,
        ) => api_error(StatusCode::CONFLICT, "request_conflict", error.to_string()),
        CoordinatorError::Protocol(ProtocolError::Invariant(_)) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invariant_violation",
            error.to_string(),
        ),
    }
}

fn authorize(headers: &HeaderMap, expected_token: &str) -> Result<(), ApiFailure> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented.is_some_and(|token| constant_time_eq(token.as_bytes(), expected_token.as_bytes()))
    {
        Ok(())
    } else {
        Err(ApiFailure {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: "missing or invalid bearer token".to_string(),
        })
    }
}

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiFailure> {
    body.map(|Json(body)| body).map_err(|rejection| ApiFailure {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_json",
        message: rejection.body_text(),
    })
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn api_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            protocol_version: PROTOCOL_VERSION,
            code,
            message: message.into(),
            active_request_id: None,
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct CreateEnterRequest {
    client_id: String,
    request_id: String,
    lease_id: String,
    lease_epoch: u64,
    peer_session_epoch: u64,
    lease_ttl_ms: u64,
}

#[derive(Debug, Deserialize)]
struct CommitRequest {
    request_epoch: u64,
    grant_epoch: u64,
    lease_id: String,
    lease_epoch: u64,
}

#[derive(Debug, Deserialize)]
struct CancelRequest {
    reason: String,
}

#[derive(Debug, Deserialize)]
struct RenewRequest {
    lease_id: String,
    lease_epoch: u64,
    peer_session_epoch: u64,
}

#[derive(Debug, Deserialize)]
struct ReadinessRequest {
    online: bool,
    keyboard_ready: bool,
    pointer_ready: bool,
    session_epoch: u64,
}

#[derive(Debug, Serialize)]
struct ApiEnvelope<T> {
    protocol_version: u16,
    server_now_ms: u64,
    data: T,
}

impl<T> ApiEnvelope<T> {
    fn new(server_now_ms: u64, data: T) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            server_now_ms,
            data,
        }
    }
}

#[derive(Debug, Serialize)]
struct ApiError {
    protocol_version: u16,
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_request_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReadyResponse {
    ready: bool,
    phase: crate::domain::ProtocolPhase,
    fallback_required: bool,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    mode: TvMode,
    observed_input: Option<Host>,
    commanded_input: Option<Host>,
    healthy: bool,
    ready: bool,
    ws_state: WsState,
    subscribe_active: bool,
    protocol_phase: ProtocolPhase,
    request_id: Option<String>,
    request_epoch: u64,
    switch_epoch: u64,
    verified_epoch: Option<u64>,
    pending_switch: Option<Host>,
    switch_timer: u64,
    fallback_required: bool,
    keyboard_owner: Host,
    pointer_owner: Host,
    reservation_target: Option<Host>,
    grant_epoch: Option<u64>,
    input_signal: BTreeMap<Host, bool>,
    signal_observations: BTreeMap<Host, SignalObservation>,
    remote_online: BTreeMap<Host, bool>,
    peer_readiness: BTreeMap<Host, PeerReadiness>,
    uptime_seconds: u64,
    reconnect_total: u64,
    switch_count: BTreeMap<Host, u64>,
    last_error: Option<String>,
    dropped_logs: u64,
    request_history_len: usize,
    retained_request_limit: usize,
    queues: CoordinatorQueueSnapshot,
    runtime: RuntimeSnapshot,
    observation_age_ms: Option<u64>,
    deadline_remaining_ms: Option<u64>,
    in_flight_operation: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct RenewResponse {
    renewed: bool,
    renewed_until_ms: Option<u64>,
    phase: crate::domain::ProtocolPhase,
}

#[derive(Debug, Serialize)]
struct ReadinessResponse {
    host: Host,
    readiness: PeerReadiness,
}

#[derive(Debug, Serialize)]
struct MultiViewResponse {
    enabled: bool,
    phase: crate::domain::ProtocolPhase,
    switch_epoch: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        coordinator,
        domain::{ProtocolState, TvMode},
        protocol::ProtocolTiming,
    };
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use tower::ServiceExt;

    const TIMING: ProtocolTiming = ProtocolTiming {
        command_ms: 100,
        observation_ms: 100,
        grant_ms: 100,
        wake_ms: 500,
        lease_ms: 300,
        signal_poll_ms: 100,
    };

    fn authenticated(request: axum::http::request::Builder) -> axum::http::request::Builder {
        request.header(AUTHORIZATION, "Bearer test-token")
    }

    fn create_body(request_id: &str, lease_id: &str) -> Body {
        Body::from(
            json!({
                "client_id": "hub",
                "request_id": request_id,
                "lease_id": lease_id,
                "lease_epoch": 1,
                "peer_session_epoch": 9,
                "lease_ttl_ms": 300
            })
            .to_string(),
        )
    }

    async fn body_json(response: Response) -> Value {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn ready_app() -> (Router, CoordinatorHandle, coordinator::EffectReceivers) {
        let (handle, effects, _) =
            coordinator::spawn(ProtocolState::new(Host::Linux, 32), TIMING, 8, 4);
        handle
            .apply_safety(Event::TransportSynchronized {
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
                signals: BTreeMap::from([
                    (Host::Linux, true),
                    (Host::Mac, false),
                    (Host::Windows, false),
                ]),
            })
            .await
            .unwrap();
        handle
            .apply_safety(Event::PeerReadinessUpdated {
                host: Host::Mac,
                readiness: PeerReadiness {
                    online: true,
                    keyboard_ready: true,
                    pointer_ready: true,
                    session_epoch: 9,
                    observed_at_ms: handle.now_ms(),
                },
            })
            .await
            .unwrap();
        (
            router(
                handle.clone(),
                "test-token".to_string(),
                500,
                RuntimeMetrics::default(),
            ),
            handle,
            effects,
        )
    }

    #[tokio::test]
    async fn get_enter_is_method_not_allowed() {
        let (app, _, _) = ready_app().await;
        let response = app
            .oneshot(
                authenticated(Request::builder().method("GET").uri("/enter/mac"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn mutations_require_bearer_authentication() {
        let (app, _, _) = ready_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/enter/mac")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn fenced_create_returns_pending_identity_and_emits_one_command() {
        let (app, handle, mut effects) = ready_app().await;
        let response = app
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/enter/mac"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "client_id": "hub",
                            "request_id": "request-1",
                            "lease_id": "lease-1",
                            "lease_epoch": 1,
                            "peer_session_epoch": 9,
                            "lease_ttl_ms": 300
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = body_json(response).await;
        assert_eq!(body["data"]["request_id"], "request-1");
        assert_eq!(body["data"]["status"], "switching");
        let server_now_ms = body["server_now_ms"].as_u64().unwrap();
        let lease_expires_at_ms = body["data"]["lease"]["expires_at_ms"].as_u64().unwrap();
        assert!(lease_expires_at_ms >= server_now_ms);
        assert!(lease_expires_at_ms - server_now_ms <= 300);
        assert_eq!(handle.snapshot().keyboard_owner, Host::Linux);
        assert!(matches!(
            effects.ordinary.recv().await,
            Some(crate::protocol::Effect::SetInput {
                target: Host::Mac,
                fallback: false,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn renewal_returns_deadline_in_the_daemon_clock_domain() {
        let (app, handle, _effects) = ready_app().await;
        let create_response = app
            .clone()
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/enter/mac"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "client_id": "hub",
                            "request_id": "request-1",
                            "lease_id": "lease-1",
                            "lease_epoch": 1,
                            "peer_session_epoch": 9,
                            "lease_ttl_ms": 300
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_response.status(), StatusCode::ACCEPTED);

        let switch_epoch = handle.snapshot().switch_epoch;
        handle
            .apply(Event::CommandAcknowledged {
                switch_epoch,
                target: Host::Mac,
            })
            .await
            .unwrap();
        handle
            .apply(Event::Observation {
                switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: BTreeMap::from([
                    (Host::Linux, true),
                    (Host::Mac, true),
                    (Host::Windows, false),
                ]),
            })
            .await
            .unwrap();
        let request = handle.snapshot().active_request.clone().unwrap();
        let grant = request.grant.clone().unwrap();

        let commit_response = app
            .clone()
            .oneshot(
                authenticated(
                    Request::builder()
                        .method("POST")
                        .uri("/enter/request/request-1/commit"),
                )
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "request_epoch": request.request_epoch,
                        "grant_epoch": grant.grant_epoch,
                        "lease_id": request.lease.lease_id,
                        "lease_epoch": request.lease.lease_epoch
                    })
                    .to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(commit_response.status(), StatusCode::OK);

        let renew_response = app
            .oneshot(
                authenticated(
                    Request::builder()
                        .method("POST")
                        .uri("/internal/enter/request/request-1/renew"),
                )
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "lease_id": "lease-1",
                        "lease_epoch": 1,
                        "peer_session_epoch": 9
                    })
                    .to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(renew_response.status(), StatusCode::OK);
        let body = body_json(renew_response).await;
        assert_eq!(body["data"]["renewed"], true);
        let server_now_ms = body["server_now_ms"].as_u64().unwrap();
        let renewed_until_ms = body["data"]["renewed_until_ms"].as_u64().unwrap();
        assert!(renewed_until_ms > server_now_ms);
        assert!(renewed_until_ms - server_now_ms <= TIMING.lease_ms);
    }

    #[tokio::test]
    async fn ready_and_status_are_typed_json() {
        let (app, _, _) = ready_app().await;
        let ready_response = app
            .clone()
            .oneshot(
                authenticated(Request::builder().uri("/ready"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ready_response.status(), StatusCode::OK);
        assert_eq!(body_json(ready_response).await["data"]["ready"], true);

        let status_response = app
            .oneshot(
                authenticated(Request::builder().uri("/status"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status_response.status(), StatusCode::OK);
        let body = body_json(status_response).await;
        assert_eq!(body["data"]["keyboard_owner"], "linux");
        assert_eq!(body["data"]["mode"], "fullscreen");
        assert_eq!(body["data"]["protocol_phase"], "idle");
        assert_eq!(body["data"]["healthy"], true);
        assert_eq!(body["data"]["ready"], true);
        assert_eq!(body["data"]["input_signal"]["linux"], true);
        assert_eq!(body["data"]["queues"]["ordinary_commands"]["depth"], 0);
        assert_eq!(body["data"]["runtime"]["dropped_logs"], 0);
        assert_eq!(body["data"]["runtime"]["retry_alert"], false);
        assert!(body["data"]["uptime_seconds"].is_number());
        assert!(body["data"].get("active_request").is_none());
    }

    #[tokio::test]
    async fn malformed_json_and_zero_identities_are_typed_bad_requests() {
        let (app, _, _) = ready_app().await;
        let malformed = app
            .clone()
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/enter/mac"))
                    .header("content-type", "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(malformed).await["code"], "invalid_json");

        let zero_epoch = app
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/enter/mac"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "client_id": "hub",
                            "request_id": "request-1",
                            "lease_id": "lease-1",
                            "lease_epoch": 0,
                            "peer_session_epoch": 9,
                            "lease_ttl_ms": 300
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(zero_epoch.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(zero_epoch).await["code"], "invalid_identity");
    }

    #[tokio::test]
    async fn duplicate_create_is_idempotent_and_conflict_names_active_request() {
        let (app, _, mut effects) = ready_app().await;
        let first = app
            .clone()
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/enter/mac"))
                    .header("content-type", "application/json")
                    .body(create_body("request-1", "lease-1"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        assert!(effects.ordinary.recv().await.is_some());

        let duplicate = app
            .clone()
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/enter/mac"))
                    .header("content-type", "application/json")
                    .body(create_body("request-1", "lease-1"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(duplicate.status(), StatusCode::ACCEPTED);
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            effects.ordinary.recv()
        )
        .await
        .is_err());

        let conflict = app
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/enter/mac"))
                    .header("content-type", "application/json")
                    .body(create_body("request-2", "lease-2"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let body = body_json(conflict).await;
        assert_eq!(body["code"], "request_busy");
        assert_eq!(body["active_request_id"], "request-1");
    }

    #[tokio::test]
    async fn poll_stale_commit_and_cancel_have_typed_contracts() {
        let (app, handle, _effects) = ready_app().await;
        let created = app
            .clone()
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/enter/mac"))
                    .header("content-type", "application/json")
                    .body(create_body("request-1", "lease-1"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::ACCEPTED);

        let polled = app
            .clone()
            .oneshot(
                authenticated(Request::builder().uri("/enter/request/request-1"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(polled.status(), StatusCode::ACCEPTED);
        let missing = app
            .clone()
            .oneshot(
                authenticated(Request::builder().uri("/enter/request/missing"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let switch_epoch = handle.snapshot().switch_epoch;
        handle
            .apply(Event::CommandAcknowledged {
                switch_epoch,
                target: Host::Mac,
            })
            .await
            .unwrap();
        handle
            .apply(Event::Observation {
                switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: BTreeMap::from([
                    (Host::Linux, true),
                    (Host::Mac, true),
                    (Host::Windows, false),
                ]),
            })
            .await
            .unwrap();
        let request = handle.snapshot().active_request.clone().unwrap();
        let stale_commit = app
            .clone()
            .oneshot(
                authenticated(
                    Request::builder()
                        .method("POST")
                        .uri("/enter/request/request-1/commit"),
                )
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "request_epoch": request.request_epoch,
                        "grant_epoch": request.grant.as_ref().unwrap().grant_epoch + 1,
                        "lease_id": request.lease.lease_id,
                        "lease_epoch": request.lease.lease_epoch
                    })
                    .to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale_commit.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(stale_commit).await["code"], "request_conflict");
        assert_eq!(handle.snapshot().keyboard_owner, Host::Linux);

        let cancelled = app
            .oneshot(
                authenticated(
                    Request::builder()
                        .method("POST")
                        .uri("/internal/enter/request/request-1/cancel"),
                )
                .header("content-type", "application/json")
                .body(Body::from(json!({"reason": "test_cancel"}).to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancelled.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(cancelled).await["data"]["status"], "cancelled");
        assert_eq!(handle.snapshot().pointer_owner, Host::Linux);
    }

    #[tokio::test]
    async fn readiness_and_multiview_routes_drive_typed_protocol_events() {
        let (app, _, mut effects) = ready_app().await;
        let readiness = app
            .clone()
            .oneshot(
                authenticated(
                    Request::builder()
                        .method("POST")
                        .uri("/internal/readiness/windows"),
                )
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "online": true,
                        "keyboard_ready": true,
                        "pointer_ready": false,
                        "session_epoch": 12
                    })
                    .to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(readiness.status(), StatusCode::OK);
        let body = body_json(readiness).await;
        assert_eq!(body["data"]["host"], "windows");
        assert_eq!(body["data"]["readiness"]["pointer_ready"], false);

        let multiview = app
            .oneshot(
                authenticated(Request::builder().method("POST").uri("/multiview/on"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(multiview.status(), StatusCode::ACCEPTED);
        assert_eq!(body_json(multiview).await["data"]["enabled"], true);
        assert!(matches!(
            effects.ordinary.recv().await,
            Some(crate::protocol::Effect::SetMultiView { enabled: true, .. })
        ));
    }
}
