use crate::{
    coordinator::{CoordinatorError, CoordinatorHandle, CoordinatorQueueSnapshot},
    domain::{Host, LeaseIdentity, PeerReadiness, ProtocolPhase, ProtocolState, RequestStatus},
    observability::{RuntimeMetrics, RuntimeSnapshot},
    protocol::{Event, ProtocolError},
};
use axum::{
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{str::FromStr, sync::Arc};
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
    if let Err(response) = authorize(request.headers(), &state.controller_token) {
        return response;
    }
    next.run(request).await
}

async fn health() -> &'static str {
    "ok"
}

async fn ready(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
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
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
    }
    let now_ms = state.coordinator.now_ms();
    let mut protocol = (*state.snapshot_rx.borrow()).as_ref().clone();
    let runtime = state.runtime_metrics.snapshot();
    protocol.dropped_logs = runtime.dropped_logs;
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
    Json(ApiEnvelope::new(
        now_ms,
        StatusResponse {
            protocol,
            uptime_ms: now_ms,
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
    Json(body): Json<CreateEnterRequest>,
) -> Response {
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
    }
    let target = match Host::from_str(&target) {
        Ok(target) => target,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, "invalid_host", error.to_string()),
    };
    if body.request_id.is_empty()
        || body.client_id.is_empty()
        || body.lease_id.is_empty()
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
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
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
    Json(body): Json<CommitRequest>,
) -> Response {
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
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
    Json(body): Json<CancelRequest>,
) -> Response {
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
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
    Json(body): Json<RenewRequest>,
) -> Response {
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
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
    Json(body): Json<ReadinessRequest>,
) -> Response {
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
    }
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
    if let Err(response) = authorize(&headers, &state.controller_token) {
        return response;
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

fn authorize(headers: &HeaderMap, expected_token: &str) -> Result<(), Response> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented.is_some_and(|token| constant_time_eq(token.as_bytes(), expected_token.as_bytes()))
    {
        Ok(())
    } else {
        Err(api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid bearer token",
        ))
    }
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
    #[serde(flatten)]
    protocol: ProtocolState,
    uptime_ms: u64,
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
        assert_eq!(body["data"]["queues"]["ordinary_commands"]["depth"], 0);
        assert_eq!(body["data"]["runtime"]["dropped_logs"], 0);
        assert_eq!(body["data"]["runtime"]["retry_alert"], false);
        assert!(body["data"]["uptime_ms"].is_number());
    }
}
