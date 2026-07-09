// tv-multiview: MultiView-aware HDMI input switch daemon.
// Implements the TvDisplaySwitch TLA+ spec.
//
// Architecture:
//   bscpylgtvcommand (subprocess) → WebOS SSAP → TV
//   axum HTTP server → lan-mouse enter_hook + multiView toggle
//   tokio background task → health polling + reconnect

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::signal;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod http;
mod state;
mod tv;

use state::TvDaemonState;

#[tokio::main]
async fn main() {
    // Structured JSON logging
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_current_span(false)
        .init();

    // Configuration (hardcoded — templated by ansible at deploy time)
    let tv_ip: Ipv4Addr = "192.0.2.20".parse().expect("invalid TV_IP");
    let daemon_port: u16 = 8765;
    let mut hdmi_map = HashMap::new();
    hdmi_map.insert("linux".to_string(), "HDMI_4".to_string());
    hdmi_map.insert("mac".to_string(), "HDMI_3".to_string());
    hdmi_map.insert("windows".to_string(), "HDMI_2".to_string());

    let daemon = Arc::new(TvDaemonState::default());
    let tv_client = Arc::new(tv::TvClient::new(tv_ip));

    // ---- Background: health polling + reconnect lifecycle ----
    let poll_daemon = Arc::clone(&daemon);
    let poll_client = Arc::clone(&tv_client);

    tokio::spawn(async move {
        maintain_connection(poll_daemon, poll_client, tv_ip).await;
    });

    // ---- HTTP server ----
    let app = http::router(Arc::clone(&daemon), (*tv_client).clone(), hdmi_map);
    let addr = SocketAddr::from(([0, 0, 0, 0], daemon_port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    info!(port = daemon_port, tv_ip = %tv_ip, "startup");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    info!("shutdown");
}

async fn maintain_connection(
    daemon: Arc<TvDaemonState>,
    client: Arc<tv::TvClient>,
    _tv_ip: Ipv4Addr,
) {
    // Initial state: dead (healthy=false from Default)
    // ReconnectFails loop
    loop {
        info!(
            event = "connect",
            retry = daemon
                .reconnect_count
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        if let Err(e) = client.get_sw_info().await {
            error!(error = %e, "connect_failed");
            if daemon.reconnect_failed() {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            } else {
                // C3: cap reached, exit to let systemd restart
                error!("reconnect_cap_reached, exiting");
                std::process::exit(1);
            }
        }

        // DaemonReconnects
        daemon.mark_healthy();
        info!(event = "connected");

        // Heartbeat every 5s
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

            if let Err(e) = client.get_sw_info().await {
                error!(error = %e, "heartbeat_failed");
                break;
            }
        }

        // DaemonDies
        daemon.mark_dead();
        daemon
            .reconnect_count
            .store(0, std::sync::atomic::Ordering::SeqCst);

        // Reconnect delay
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.unwrap();
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .unwrap()
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("SIGINT received"),
        _ = terminate => info!("SIGTERM received"),
    }
}
