// HTTP API: axum routes implementing the TLA+ state machine.
use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use tracing::{error, info};

use crate::state::{Input, TvDaemonState, TvMode};
use crate::tv::TvClient;

pub struct AppState {
    pub daemon: TvDaemonState,
    pub tv_client: TvClient,
    pub hdmi_map: std::collections::HashMap<String, String>,
}

pub fn router(tv_ip: Ipv4Addr, hdmi_map: std::collections::HashMap<String, String>) -> Router {
    let state = Arc::new(AppState {
        daemon: TvDaemonState::default(),
        tv_client: TvClient::new(tv_ip),
        hdmi_map,
    });

    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/enter/{target}", get(enter))
        .route("/multiview/on", get(multiview_on))
        .route("/multiview/off", get(multiview_off))
        .with_state(state)
}

// ---- /health ----
async fn health() -> impl IntoResponse {
    "ok"
}

// ---- /status ----
async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(state.daemon.status())
}

// ---- /enter/{target} ----
async fn enter(
    State(state): State<Arc<AppState>>,
    Path(target): Path<String>,
) -> impl IntoResponse {
    let input = Input::from_str(&target);
    if input == Input::Unknown {
        return (StatusCode::BAD_REQUEST, "unknown target".to_string());
    }

    // P0.4: 503 when daemon dead
    if !*state.daemon.healthy.lock().unwrap() {
        return (StatusCode::SERVICE_UNAVAILABLE, "tv disconnected".to_string());
    }

    // P0.3: pending_switch gate (debounce)
    {
        let pending = state.daemon.pending.lock().unwrap();
        if pending.is_some() {
            let mode = format!("{:?}", *state.daemon.tv_mode.lock().unwrap()).to_lowercase();
            return (StatusCode::OK, mode);
        }
    }

    // C6: no-op if already on target
    {
        let mode = *state.daemon.tv_mode.lock().unwrap();
        let current = *state.daemon.tv_input.lock().unwrap();
        if mode == TvMode::Fullscreen && input == current {
            return (StatusCode::OK, "fullscreen".to_string());
        }
        if mode == TvMode::Multiview {
            return (StatusCode::OK, "multiview".to_string());
        }
    }

    // ReturnToLinux
    if input == Input::Linux {
        let should_switch = state.daemon.return_to_linux().unwrap_or(false);
        if !should_switch {
            return (StatusCode::OK, "multiview".to_string());
        }

        let hdmi = state
            .hdmi_map
            .get("linux")
            .cloned()
            .unwrap_or_else(|| "HDMI_4".to_string());

        info!(target = "linux", action = "switch", input = %hdmi, "enter");

        if let Err(e) = state.tv_client.set_input(&hdmi).await {
            error!(error = %e, "switch_failed");
            *state.daemon.last_error.lock().unwrap() = Some(e.clone());
            state.daemon.switch_complete(); // C1: clear pending on failure
            return (StatusCode::BAD_GATEWAY, format!("error: {}", e));
        }

        state.daemon.switch_complete();
        state.daemon.switch_count.lock().unwrap().linux += 1;
        info!(mode = "fullscreen", input = "linux", "switch_complete");
        return (StatusCode::OK, "fullscreen".to_string());
    }

    // EnterOtherHost: switch to remote host
    let entered = state.daemon.enter_other_host(input);
    if !entered {
        // Guard failed (e.g., tv_input == target after race)
        let mode = format!("{:?}", *state.daemon.tv_mode.lock().unwrap()).to_lowercase();
        return (StatusCode::OK, mode);
    }

    let hdmi = state
        .hdmi_map
        .get(&target)
        .cloned()
        .unwrap_or_else(|| format!("HDMI_UNKNOWN"));

    info!(target = %target, action = "switch", input = %hdmi, "enter");

    if let Err(e) = state.tv_client.set_input(&hdmi).await {
        error!(error = %e, "switch_failed");
        *state.daemon.last_error.lock().unwrap() = Some(e.clone());
        state.daemon.switch_complete();
        return (StatusCode::BAD_GATEWAY, format!("error: {}", e));
    }

    // SwitchComplete
    state.daemon.switch_complete();
    match input {
        Input::Mac => state.daemon.switch_count.lock().unwrap().mac += 1,
        Input::Windows => state.daemon.switch_count.lock().unwrap().windows += 1,
        _ => {}
    }
    info!(mode = "fullscreen", input = %target, "switch_complete");
    (StatusCode::OK, "fullscreen".to_string())
}

// ---- /multiview/on (EnterMultiView) ----
async fn multiview_on(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if !*state.daemon.healthy.lock().unwrap() {
        return (StatusCode::SERVICE_UNAVAILABLE, "tv disconnected".to_string());
    }

    let entered = state.daemon.enter_multiview();
    if !entered {
        let mode = format!("{:?}", *state.daemon.tv_mode.lock().unwrap()).to_lowercase();
        return (StatusCode::OK, mode);
    }

    info!(action = "on", "multiview");

    if let Err(e) = state.tv_client.set_splitscreen(true).await {
        error!(error = %e, "multiview_failed");
        *state.daemon.last_error.lock().unwrap() = Some(e.clone());
        *state.daemon.tv_mode.lock().unwrap() = TvMode::Fullscreen;
        return (StatusCode::BAD_GATEWAY, format!("error: {}", e));
    }

    (StatusCode::OK, "multiview".to_string())
}

// ---- /multiview/off (ExitMultiView) ----
async fn multiview_off(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if !*state.daemon.healthy.lock().unwrap() {
        return (StatusCode::SERVICE_UNAVAILABLE, "tv disconnected".to_string());
    }

    let exited = state.daemon.exit_multiview(Input::Linux); // default to linux
    if !exited {
        let mode = format!("{:?}", *state.daemon.tv_mode.lock().unwrap()).to_lowercase();
        return (StatusCode::OK, mode);
    }

    info!(action = "off", "multiview");

    if let Err(e) = state.tv_client.set_splitscreen(false).await {
        error!(error = %e, "multiview_failed");
        *state.daemon.last_error.lock().unwrap() = Some(e.clone());
        *state.daemon.tv_mode.lock().unwrap() = TvMode::Multiview;
        return (StatusCode::BAD_GATEWAY, format!("error: {}", e));
    }

    (StatusCode::OK, "fullscreen".to_string())
}
