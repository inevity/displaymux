use super::{codec, key_store};
use crate::{
    config::DaemonConfig,
    coordinator::{CoordinatorHandle, EffectReceivers},
    domain::{Host, TvMode},
    protocol::{Effect, Event},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use thiserror::Error;
use tokio::{net::TcpStream, task::JoinHandle, time::Instant};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{self, Message},
    Connector, MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, warn};

type TvSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub fn spawn(
    config: DaemonConfig,
    coordinator: CoordinatorHandle,
    effects: EffectReceivers,
) -> JoinHandle<()> {
    tokio::spawn(run(config, coordinator, effects))
}

async fn run(config: DaemonConfig, coordinator: CoordinatorHandle, mut effects: EffectReceivers) {
    let client_key = match key_store::load_client_key(&config.client_key_path, config.tv_ip) {
        Ok(client_key) => client_key,
        Err(key_error) => {
            error!(error = %key_error, path = %config.client_key_path.display(), "fatal client-key configuration");
            let _ = coordinator
                .notify_safety(Event::TransportDisconnected {
                    reason: "client_key_unavailable".to_string(),
                })
                .await;
            return;
        }
    };

    let mut backoff_ms = config.timeouts.reconnect_initial_ms;
    loop {
        let _ = coordinator.notify_safety(Event::TransportConnecting).await;
        match connect_and_run(&config, &client_key, &coordinator, &mut effects).await {
            Ok(()) => return,
            Err(session_error) => {
                warn!(error = %session_error, retry_ms = backoff_ms, "SSAP session ended");
                let _ = coordinator
                    .notify_safety(Event::TransportDisconnected {
                        reason: session_error.public_reason().to_string(),
                    })
                    .await;
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = backoff_ms
                    .saturating_mul(2)
                    .min(config.timeouts.reconnect_max_ms);
            }
        }
    }
}

async fn connect_and_run(
    config: &DaemonConfig,
    client_key: &str,
    coordinator: &CoordinatorHandle,
    effects: &mut EffectReceivers,
) -> Result<(), SsapError> {
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()?;
    let url = format!("wss://{}:3001/", config.tv_ip);
    let connect = connect_async_tls_with_config(url, None, false, Some(Connector::NativeTls(tls)));
    let (mut socket, _) =
        tokio::time::timeout(Duration::from_millis(config.timeouts.command_ms), connect)
            .await
            .map_err(|_| SsapError::Timeout("connect"))??;

    coordinator
        .notify_safety(Event::TransportRegistering)
        .await
        .map_err(|_| SsapError::CoordinatorClosed)?;
    send_json(&mut socket, codec::registration(client_key)).await?;
    wait_for_registration(&mut socket, config.timeouts.command_ms).await?;
    info!(event = "ssap_registered", client_key_present = true);

    send_json(&mut socket, codec::subscription()).await?;
    let subscription = wait_for_id(
        &mut socket,
        codec::MULTIVIEW_SUBSCRIPTION_ID,
        config.timeouts.command_ms,
        coordinator,
    )
    .await?;
    let _ = codec::successful_payload(&subscription)?;
    coordinator
        .notify_safety(Event::TransportSubscribed)
        .await
        .map_err(|_| SsapError::CoordinatorClosed)?;
    info!(event = "ssap_subscribed", topic = "multiViewStatus");

    let mut next_id = 1u64;
    let switch_epoch = coordinator.snapshot().switch_epoch;
    let (mode, input, signals) = observe(&mut socket, &mut next_id, config, coordinator).await?;
    coordinator
        .notify_safety(Event::TransportSynchronized {
            mode,
            input,
            signals,
        })
        .await
        .map_err(|_| SsapError::CoordinatorClosed)?;
    info!(event = "ssap_synchronized", switch_epoch);

    let mut keepalive = tokio::time::interval(Duration::from_millis(config.timeouts.keepalive_ms));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_received = Instant::now();
    loop {
        tokio::select! {
            biased;
            effect = effects.safety.recv() => {
                let Some(effect) = effect else { return Ok(()); };
                execute_effect(&mut socket, &mut next_id, config, coordinator, effect).await?;
                last_received = Instant::now();
            }
            message = socket.next() => {
                let message = message.ok_or(SsapError::Closed)??;
                if let Some(value) = decode_message(&mut socket, message).await? {
                    handle_unsolicited(value, coordinator).await?;
                }
                last_received = Instant::now();
            }
            effect = effects.ordinary.recv() => {
                let Some(effect) = effect else { return Ok(()); };
                execute_effect(&mut socket, &mut next_id, config, coordinator, effect).await?;
                last_received = Instant::now();
            }
            _ = keepalive.tick() => {
                if last_received.elapsed() >= Duration::from_millis(config.timeouts.keepalive_timeout_ms) {
                    return Err(SsapError::Timeout("keepalive"));
                }
                socket.send(Message::Ping(Vec::new().into())).await?;
                socket.flush().await?;
            }
        }
    }
}

async fn execute_effect(
    socket: &mut TvSocket,
    next_id: &mut u64,
    config: &DaemonConfig,
    coordinator: &CoordinatorHandle,
    effect: Effect,
) -> Result<(), SsapError> {
    match effect {
        Effect::SetInput {
            target,
            switch_epoch,
            ..
        } => {
            let result = request_payload(
                socket,
                next_id,
                codec::SET_INPUT,
                json!({"inputId": config.input_for(target)}),
                config.timeouts.command_ms,
                coordinator,
            )
            .await;
            match result {
                Ok(_) => coordinator
                    .notify_safety(Event::CommandAcknowledged {
                        switch_epoch,
                        target,
                    })
                    .await
                    .map_err(|_| SsapError::CoordinatorClosed)?,
                Err(command_error) if command_error.recoverable_command_error() => coordinator
                    .notify_safety(Event::CommandFailed {
                        switch_epoch,
                        reason: command_error.public_reason().to_string(),
                    })
                    .await
                    .map_err(|_| SsapError::CoordinatorClosed)?,
                Err(connection_error) => return Err(connection_error),
            }
        }
        Effect::Observe { switch_epoch } => {
            match observe(socket, next_id, config, coordinator).await {
                Ok((mode, input, signals)) => coordinator
                    .notify_safety(Event::Observation {
                        switch_epoch,
                        mode,
                        input,
                        signals,
                    })
                    .await
                    .map_err(|_| SsapError::CoordinatorClosed)?,
                Err(observation_error) if observation_error.recoverable_command_error() => {
                    coordinator
                        .notify_safety(Event::CommandFailed {
                            switch_epoch,
                            reason: observation_error.public_reason().to_string(),
                        })
                        .await
                        .map_err(|_| SsapError::CoordinatorClosed)?;
                }
                Err(connection_error) => return Err(connection_error),
            }
        }
        Effect::SetMultiView {
            enabled,
            switch_epoch,
        } => {
            let value = if enabled { "on" } else { "off" };
            let result = request_payload(
                socket,
                next_id,
                codec::SET_SYSTEM_SETTINGS,
                json!({
                    "category": "commercial",
                    "settings": {"splitscreenEnable": value}
                }),
                config.timeouts.command_ms,
                coordinator,
            )
            .await;
            match result {
                Ok(_) => coordinator
                    .notify_safety(Event::MultiViewAcknowledged {
                        switch_epoch,
                        enabled,
                    })
                    .await
                    .map_err(|_| SsapError::CoordinatorClosed)?,
                Err(command_error) if command_error.recoverable_command_error() => coordinator
                    .notify_safety(Event::CommandFailed {
                        switch_epoch,
                        reason: command_error.public_reason().to_string(),
                    })
                    .await
                    .map_err(|_| SsapError::CoordinatorClosed)?,
                Err(connection_error) => return Err(connection_error),
            }
        }
        Effect::Wake { target, .. } => {
            let mac = config
                .wake_on_lan
                .get(&target)
                .ok_or(SsapError::MissingWakeAddress(target))?;
            send_wake_packet(mac).await?;
            info!(event = "wake_sent", target = %target);
        }
    }
    Ok(())
}

async fn observe(
    socket: &mut TvSocket,
    next_id: &mut u64,
    config: &DaemonConfig,
    coordinator: &CoordinatorHandle,
) -> Result<(TvMode, Option<Host>, BTreeMap<Host, bool>), SsapError> {
    let settings = request_payload(
        socket,
        next_id,
        codec::GET_SYSTEM_SETTINGS,
        json!({"category": "option", "keys": ["multiViewStatus"]}),
        config.timeouts.observation_ms,
        coordinator,
    )
    .await?;
    let mode = codec::parse_multiview_mode(&settings).unwrap_or(TvMode::Fullscreen);
    let current_app = request_payload(
        socket,
        next_id,
        codec::GET_CURRENT_APP,
        json!({}),
        config.timeouts.observation_ms,
        coordinator,
    )
    .await?;
    let inputs = request_payload(
        socket,
        next_id,
        codec::GET_INPUTS,
        json!({}),
        config.timeouts.observation_ms,
        coordinator,
    )
    .await?;
    Ok((
        mode,
        codec::parse_current_input(&current_app, &config.inputs)?,
        codec::parse_signals(&inputs, &config.inputs)?,
    ))
}

async fn request_payload(
    socket: &mut TvSocket,
    next_id: &mut u64,
    uri: &str,
    payload: Value,
    timeout_ms: u64,
    coordinator: &CoordinatorHandle,
) -> Result<Value, SsapError> {
    let id = format!("request-{}", *next_id);
    *next_id = next_id.saturating_add(1);
    send_json(socket, codec::request(&id, uri, payload)).await?;
    let response = wait_for_id(socket, &id, timeout_ms, coordinator).await?;
    Ok(codec::successful_payload(&response)?.clone())
}

async fn wait_for_registration(socket: &mut TvSocket, timeout_ms: u64) -> Result<(), SsapError> {
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        loop {
            let value = receive_json(socket).await?;
            if codec::registered_client_key(&value)?.is_some() {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| SsapError::Timeout("registration"))?
}

async fn wait_for_id(
    socket: &mut TvSocket,
    expected_id: &str,
    timeout_ms: u64,
    coordinator: &CoordinatorHandle,
) -> Result<Value, SsapError> {
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        loop {
            let value = receive_json(socket).await?;
            if codec::response_id(&value) == Some(expected_id) {
                return Ok(value);
            }
            handle_unsolicited(value, coordinator).await?;
        }
    })
    .await
    .map_err(|_| SsapError::Timeout("response"))?
}

async fn handle_unsolicited(
    value: Value,
    coordinator: &CoordinatorHandle,
) -> Result<(), SsapError> {
    if codec::response_id(&value) == Some(codec::MULTIVIEW_SUBSCRIPTION_ID) {
        let payload = codec::successful_payload(&value)?;
        if let Some(mode) = codec::parse_multiview_mode(payload) {
            coordinator
                .notify_safety(Event::SubscriptionObserved { mode, input: None })
                .await
                .map_err(|_| SsapError::CoordinatorClosed)?;
        }
    } else {
        debug!(message = %value, "unmatched SSAP message");
    }
    Ok(())
}

async fn send_json(socket: &mut TvSocket, value: Value) -> Result<(), SsapError> {
    socket.send(Message::Text(value.to_string().into())).await?;
    socket.flush().await?;
    Ok(())
}

async fn receive_json(socket: &mut TvSocket) -> Result<Value, SsapError> {
    loop {
        let message = socket.next().await.ok_or(SsapError::Closed)??;
        if let Some(value) = decode_message(socket, message).await? {
            return Ok(value);
        }
    }
}

async fn decode_message(
    socket: &mut TvSocket,
    message: Message,
) -> Result<Option<Value>, SsapError> {
    match message {
        Message::Text(text) => Ok(Some(serde_json::from_str(&text)?)),
        Message::Binary(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Message::Ping(payload) => {
            socket.send(Message::Pong(payload)).await?;
            socket.flush().await?;
            Ok(None)
        }
        Message::Pong(_) | Message::Frame(_) => Ok(None),
        Message::Close(_) => Err(SsapError::Closed),
    }
}

async fn send_wake_packet(mac: &str) -> Result<(), SsapError> {
    let bytes = parse_mac(mac)?;
    let mut packet = [0xffu8; 102];
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&bytes);
    }
    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    socket
        .send_to(&packet, SocketAddr::from((Ipv4Addr::BROADCAST, 9)))
        .await?;
    Ok(())
}

fn parse_mac(mac: &str) -> Result<[u8; 6], SsapError> {
    let parts = mac.split(':').collect::<Vec<_>>();
    if parts.len() != 6 {
        return Err(SsapError::InvalidMacAddress);
    }
    let mut bytes = [0u8; 6];
    for (output, part) in bytes.iter_mut().zip(parts) {
        *output = u8::from_str_radix(part, 16).map_err(|_| SsapError::InvalidMacAddress)?;
    }
    Ok(bytes)
}

#[derive(Debug, Error)]
enum SsapError {
    #[error("TLS configuration failed: {0}")]
    Tls(#[from] native_tls::Error),
    #[error("WebSocket transport failed: {0}")]
    WebSocket(#[from] tungstenite::Error),
    #[error("SSAP JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("SSAP protocol failed: {0}")]
    Codec(#[from] codec::CodecError),
    #[error("SSAP {0} timed out")]
    Timeout(&'static str),
    #[error("SSAP socket closed")]
    Closed,
    #[error("coordinator closed")]
    CoordinatorClosed,
    #[error("wake address missing for {0}")]
    MissingWakeAddress(Host),
    #[error("invalid Wake-on-LAN MAC address")]
    InvalidMacAddress,
    #[error("Wake-on-LAN I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl SsapError {
    fn recoverable_command_error(&self) -> bool {
        matches!(
            self,
            Self::Codec(_) | Self::MissingWakeAddress(_) | Self::InvalidMacAddress
        )
    }

    fn public_reason(&self) -> &'static str {
        match self {
            Self::Tls(_) => "tls_configuration",
            Self::WebSocket(_) => "websocket_transport",
            Self::Json(_) => "invalid_ssap_json",
            Self::Codec(_) => "ssap_command_failed",
            Self::Timeout(_) => "ssap_timeout",
            Self::Closed => "ssap_closed",
            Self::CoordinatorClosed => "coordinator_closed",
            Self::MissingWakeAddress(_) => "wake_address_missing",
            Self::InvalidMacAddress => "wake_address_invalid",
            Self::Io(_) => "wake_io_failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colon_separated_mac() {
        assert_eq!(
            parse_mac("01:23:45:67:89:ab").unwrap(),
            [0x01, 0x23, 0x45, 0x67, 0x89, 0xab]
        );
    }

    #[test]
    fn rejects_incomplete_mac() {
        assert!(matches!(
            parse_mac("01:23:45"),
            Err(SsapError::InvalidMacAddress)
        ));
    }
}
