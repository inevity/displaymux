use tokio::signal;
use tracing::{error, info};

mod config;
mod coordinator;
mod domain;
mod http;
mod observability;
mod protocol;
mod ssap;

use config::DaemonConfig;
use domain::ProtocolState;
use protocol::{Event, ProtocolTiming};

#[tokio::main]
async fn main() {
    let runtime_metrics = observability::init_logging();

    if let Err(run_error) = run(runtime_metrics).await {
        error!(error = %run_error, "fatal daemon error");
        std::process::exit(1);
    }
}

async fn run(
    runtime_metrics: observability::RuntimeMetrics,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = DaemonConfig::load()?;
    let timing = ProtocolTiming {
        command_ms: config.timeouts.command_ms,
        observation_ms: config.timeouts.observation_ms,
        grant_ms: config.timeouts.grant_ms,
        wake_ms: config.timeouts.wake_ms,
        lease_ms: config.timeouts.lease_ms,
        signal_poll_ms: config.timeouts.signal_poll_ms,
    };
    let (coordinator, effects, coordinator_task) = coordinator::spawn(
        ProtocolState::with_display_hosts(
            config.server_host,
            config.limits.retained_requests,
            config.display_hosts(),
        ),
        timing,
        config.limits.command_queue,
        config.limits.safety_queue,
    );
    let ssap_task = ssap::spawn(
        config.clone(),
        coordinator.clone(),
        effects,
        runtime_metrics.clone(),
    );
    spawn_state_dump(coordinator.clone(), runtime_metrics.clone());

    let app = http::router(
        coordinator.clone(),
        config.controller_token.clone(),
        config.timeouts.lease_ms,
        runtime_metrics,
    );
    let listener = tokio::net::TcpListener::bind(config.bind_address).await?;
    info!(
        event = "startup",
        bind_address = %config.bind_address,
        tv_ip = %config.tv_ip,
        server_host = %config.server_host,
        command_queue = config.limits.command_queue,
        safety_queue = config.limits.safety_queue,
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    let _ = coordinator.apply_safety(Event::Shutdown).await;
    ssap_task.abort();
    coordinator_task.abort();
    info!(event = "shutdown");
    Ok(())
}

#[cfg(unix)]
fn spawn_state_dump(
    coordinator: coordinator::CoordinatorHandle,
    runtime_metrics: observability::RuntimeMetrics,
) {
    tokio::spawn(async move {
        let Ok(mut signal) = signal::unix::signal(signal::unix::SignalKind::user_defined1()) else {
            return;
        };
        while signal.recv().await.is_some() {
            let snapshot = coordinator.snapshot();
            info!(
                event = "state_dump",
                state = ?snapshot,
                queues = ?coordinator.queue_snapshot(),
                runtime = ?runtime_metrics.snapshot(),
            );
        }
    });
}

#[cfg(not(unix))]
fn spawn_state_dump(
    _coordinator: coordinator::CoordinatorHandle,
    _runtime_metrics: observability::RuntimeMetrics,
) {
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!(event = "shutdown_signal", signal = "SIGINT"),
        _ = terminate => info!(event = "shutdown_signal", signal = "SIGTERM"),
    }
}
