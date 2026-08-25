use super::{codec, key_store};
use crate::{
    config::DaemonConfig,
    coordinator::{CoordinatorHandle, EffectReceivers},
    domain::{Host, TvMode},
    observability::RuntimeMetrics,
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

trait SsapSocket {
    async fn write_message(&mut self, message: Message) -> Result<(), SsapError>;
    async fn read_message(&mut self) -> Result<Message, SsapError>;
}

impl SsapSocket for TvSocket {
    async fn write_message(&mut self, message: Message) -> Result<(), SsapError> {
        self.send(message).await?;
        self.flush().await?;
        Ok(())
    }

    async fn read_message(&mut self) -> Result<Message, SsapError> {
        self.next()
            .await
            .ok_or(SsapError::Closed)?
            .map_err(Into::into)
    }
}

#[derive(Debug)]
struct ReconnectBackoff {
    initial_ms: u64,
    max_ms: u64,
    next_ms: u64,
    consecutive_failures: u64,
}

impl ReconnectBackoff {
    fn new(initial_ms: u64, max_ms: u64) -> Self {
        Self {
            initial_ms,
            max_ms,
            next_ms: initial_ms,
            consecutive_failures: 0,
        }
    }

    fn failed(&mut self) -> (u64, u64) {
        let delay_ms = self.next_ms;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.next_ms = self.next_ms.saturating_mul(2).min(self.max_ms);
        (delay_ms, self.consecutive_failures)
    }

    fn synchronized(&mut self) {
        self.next_ms = self.initial_ms;
        self.consecutive_failures = 0;
    }
}

pub fn spawn(
    config: DaemonConfig,
    coordinator: CoordinatorHandle,
    effects: EffectReceivers,
    runtime_metrics: RuntimeMetrics,
) -> JoinHandle<()> {
    tokio::spawn(run(config, coordinator, effects, runtime_metrics))
}

async fn run(
    config: DaemonConfig,
    coordinator: CoordinatorHandle,
    mut effects: EffectReceivers,
    runtime_metrics: RuntimeMetrics,
) {
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

    let mut backoff = ReconnectBackoff::new(
        config.timeouts.reconnect_initial_ms,
        config.timeouts.reconnect_max_ms,
    );
    loop {
        let _ = coordinator.notify_safety(Event::TransportConnecting).await;
        match connect_and_run(
            &config,
            &client_key,
            &coordinator,
            &mut effects,
            &mut backoff,
            &runtime_metrics,
        )
        .await
        {
            Ok(()) => return,
            Err(session_error) => {
                let (retry_ms, consecutive_failures) = backoff.failed();
                let retry_alert =
                    consecutive_failures >= config.limits.reconnect_alert_after as u64;
                runtime_metrics.record_reconnect_failure(
                    consecutive_failures,
                    retry_ms,
                    config.limits.reconnect_alert_after as u64,
                );
                warn!(
                    event = "ssap_disconnected",
                    error = %session_error,
                    retry_ms,
                    consecutive_failures,
                    retry_alert,
                    "SSAP session ended"
                );
                let _ = coordinator
                    .notify_safety(Event::TransportDisconnected {
                        reason: session_error.public_reason().to_string(),
                    })
                    .await;
                tokio::time::sleep(Duration::from_millis(retry_ms)).await;
            }
        }
    }
}

async fn connect_and_run(
    config: &DaemonConfig,
    client_key: &str,
    coordinator: &CoordinatorHandle,
    effects: &mut EffectReceivers,
    backoff: &mut ReconnectBackoff,
    runtime_metrics: &RuntimeMetrics,
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

    run_connected_session(
        &mut socket,
        config,
        client_key,
        coordinator,
        effects,
        backoff,
        runtime_metrics,
    )
    .await
}

async fn run_connected_session<S: SsapSocket>(
    socket: &mut S,
    config: &DaemonConfig,
    client_key: &str,
    coordinator: &CoordinatorHandle,
    effects: &mut EffectReceivers,
    backoff: &mut ReconnectBackoff,
    runtime_metrics: &RuntimeMetrics,
) -> Result<(), SsapError> {
    coordinator
        .notify_safety(Event::TransportRegistering)
        .await
        .map_err(|_| SsapError::CoordinatorClosed)?;
    send_json(socket, codec::registration(client_key)).await?;
    wait_for_registration(socket, config.timeouts.command_ms).await?;
    info!(event = "ssap_registered", client_key_present = true);

    send_json(socket, codec::subscription()).await?;
    let subscription = wait_for_id(
        socket,
        codec::CURRENT_APP_SUBSCRIPTION_ID,
        config.timeouts.command_ms,
        coordinator,
        &config.inputs,
    )
    .await?;
    let _ = codec::successful_payload(&subscription)?;
    coordinator
        .notify_safety(Event::TransportSubscribed)
        .await
        .map_err(|_| SsapError::CoordinatorClosed)?;
    info!(event = "ssap_subscribed", topic = "foregroundApp");

    let mut next_id = 1u64;
    set_multiview(socket, &mut next_id, false, config, coordinator).await?;
    let switch_epoch = coordinator.snapshot().switch_epoch;
    let (input, signals) = observe(socket, &mut next_id, config, coordinator).await?;
    coordinator
        .notify_safety(Event::TransportSynchronized {
            mode: TvMode::Fullscreen,
            input,
            signals,
        })
        .await
        .map_err(|_| SsapError::CoordinatorClosed)?;
    backoff.synchronized();
    runtime_metrics.record_synchronized();
    info!(event = "ssap_synchronized", switch_epoch);

    let mut keepalive = tokio::time::interval(Duration::from_millis(config.timeouts.keepalive_ms));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending_keepalive_since = None;
    loop {
        tokio::select! {
            biased;
            effect = effects.safety.recv() => {
                let Some(effect) = effect else { return Ok(()); };
                execute_effect(socket, &mut next_id, config, coordinator, effect).await?;
                pending_keepalive_since = None;
            }
            message = socket.read_message() => {
                let message = message?;
                if let Some(value) = decode_message(socket, message).await? {
                    handle_unsolicited(value, coordinator, &config.inputs).await?;
                }
                pending_keepalive_since = None;
            }
            effect = effects.ordinary.recv() => {
                let Some(effect) = effect else { return Ok(()); };
                execute_effect(socket, &mut next_id, config, coordinator, effect).await?;
                pending_keepalive_since = None;
            }
            _ = keepalive.tick() => {
                if pending_keepalive_since.is_some_and(|started: Instant| {
                    started.elapsed()
                        >= Duration::from_millis(config.timeouts.keepalive_timeout_ms)
                }) {
                    return Err(SsapError::Timeout("keepalive"));
                }
                if pending_keepalive_since.is_none() {
                    socket.write_message(Message::Ping(Vec::new().into())).await?;
                    pending_keepalive_since = Some(Instant::now());
                }
            }
        }
    }
}

async fn execute_effect<S: SsapSocket>(
    socket: &mut S,
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
            let Some(input_id) = config.input_for(target) else {
                coordinator
                    .notify_safety(Event::CommandFailed {
                        switch_epoch,
                        reason: format!("display_route_missing:{target}"),
                    })
                    .await
                    .map_err(|_| SsapError::CoordinatorClosed)?;
                return Ok(());
            };
            let result = request_payload(
                socket,
                next_id,
                codec::SET_INPUT,
                json!({"inputId": input_id}),
                config.timeouts.command_ms,
                coordinator,
                &config.inputs,
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
            let snapshot = coordinator.snapshot();
            let mode = match snapshot.pending_multiview {
                Some(true) => TvMode::Multiview,
                Some(false) => TvMode::Fullscreen,
                None => snapshot.tv_mode,
            };
            match observe(socket, next_id, config, coordinator).await {
                Ok((input, signals)) => coordinator
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
            let result = set_multiview(socket, next_id, enabled, config, coordinator).await;
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

async fn set_multiview<S: SsapSocket>(
    socket: &mut S,
    next_id: &mut u64,
    enabled: bool,
    config: &DaemonConfig,
    coordinator: &CoordinatorHandle,
) -> Result<(), SsapError> {
    let value = if enabled { "on" } else { "off" };
    let luna_uri = "luna://com.webos.settingsservice/setSystemSettings";
    let params = json!({
        "category": "commercial",
        "settings": {"splitscreenEnable": value}
    });
    let action = json!({"uri": luna_uri, "params": params.clone()});
    let created = request_payload(
        socket,
        next_id,
        codec::CREATE_ALERT,
        json!({
            "message": " ",
            "buttons": [{"label": "", "onClick": luna_uri, "params": params}],
            "onclose": action.clone(),
            "onfail": action,
        }),
        config.timeouts.command_ms,
        coordinator,
        &config.inputs,
    )
    .await?;
    let alert_id = created
        .get("alertId")
        .cloned()
        .ok_or(codec::CodecError::MissingField("alertId"))?;
    request_payload(
        socket,
        next_id,
        codec::CLOSE_ALERT,
        json!({"alertId": alert_id}),
        config.timeouts.command_ms,
        coordinator,
        &config.inputs,
    )
    .await?;
    Ok(())
}

async fn observe<S: SsapSocket>(
    socket: &mut S,
    next_id: &mut u64,
    config: &DaemonConfig,
    coordinator: &CoordinatorHandle,
) -> Result<(Option<Host>, BTreeMap<Host, bool>), SsapError> {
    let current_app = request_payload(
        socket,
        next_id,
        codec::GET_CURRENT_APP,
        json!({}),
        config.timeouts.observation_ms,
        coordinator,
        &config.inputs,
    )
    .await?;
    let inputs = request_payload(
        socket,
        next_id,
        codec::GET_INPUTS,
        json!({}),
        config.timeouts.observation_ms,
        coordinator,
        &config.inputs,
    )
    .await?;
    Ok((
        codec::parse_current_input(&current_app, &config.inputs)?,
        codec::parse_signals(&inputs, &config.inputs)?,
    ))
}

async fn request_payload<S: SsapSocket>(
    socket: &mut S,
    next_id: &mut u64,
    uri: &str,
    payload: Value,
    timeout_ms: u64,
    coordinator: &CoordinatorHandle,
    inputs: &BTreeMap<Host, String>,
) -> Result<Value, SsapError> {
    let id = format!("request-{}", *next_id);
    *next_id = next_id.saturating_add(1);
    send_json(socket, codec::request(&id, uri, payload)).await?;
    let response = wait_for_id(socket, &id, timeout_ms, coordinator, inputs).await?;
    Ok(codec::successful_payload(&response)?.clone())
}

async fn wait_for_registration<S: SsapSocket>(
    socket: &mut S,
    timeout_ms: u64,
) -> Result<(), SsapError> {
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

async fn wait_for_id<S: SsapSocket>(
    socket: &mut S,
    expected_id: &str,
    timeout_ms: u64,
    coordinator: &CoordinatorHandle,
    inputs: &BTreeMap<Host, String>,
) -> Result<Value, SsapError> {
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        loop {
            let value = receive_json(socket).await?;
            if codec::response_id(&value) == Some(expected_id) {
                return Ok(value);
            }
            handle_unsolicited(value, coordinator, inputs).await?;
        }
    })
    .await
    .map_err(|_| SsapError::Timeout("response"))?
}

async fn handle_unsolicited(
    value: Value,
    coordinator: &CoordinatorHandle,
    inputs: &BTreeMap<Host, String>,
) -> Result<(), SsapError> {
    if codec::response_id(&value) == Some(codec::CURRENT_APP_SUBSCRIPTION_ID) {
        let payload = codec::successful_payload(&value)?;
        let mode = coordinator.snapshot().tv_mode;
        let input = codec::parse_current_input(payload, inputs)?;
        coordinator
            .notify_safety(Event::SubscriptionObserved { mode, input })
            .await
            .map_err(|_| SsapError::CoordinatorClosed)?;
    } else {
        debug!(message = %value, "unmatched SSAP message");
    }
    Ok(())
}

async fn send_json<S: SsapSocket>(socket: &mut S, value: Value) -> Result<(), SsapError> {
    socket
        .write_message(Message::Text(value.to_string().into()))
        .await
}

async fn receive_json<S: SsapSocket>(socket: &mut S) -> Result<Value, SsapError> {
    loop {
        let message = socket.read_message().await?;
        if let Some(value) = decode_message(socket, message).await? {
            return Ok(value);
        }
    }
}

async fn decode_message<S: SsapSocket>(
    socket: &mut S,
    message: Message,
) -> Result<Option<Value>, SsapError> {
    match message {
        Message::Text(text) => Ok(Some(serde_json::from_str(&text)?)),
        Message::Binary(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Message::Ping(payload) => {
            socket.write_message(Message::Pong(payload)).await?;
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
    use crate::{
        coordinator,
        domain::{ProtocolPhase, ProtocolState},
        protocol::{Event, ProtocolTiming},
    };
    use std::{collections::VecDeque, future::pending};

    enum ReadStep {
        Message(Message),
        Pending,
    }

    #[derive(Default)]
    struct ScriptedSocket {
        incoming: VecDeque<ReadStep>,
        sent: Vec<Message>,
    }

    impl ScriptedSocket {
        fn new(incoming: impl IntoIterator<Item = Message>) -> Self {
            Self {
                incoming: incoming.into_iter().map(ReadStep::Message).collect(),
                sent: Vec::new(),
            }
        }
    }

    impl SsapSocket for ScriptedSocket {
        async fn write_message(&mut self, message: Message) -> Result<(), SsapError> {
            self.sent.push(message);
            Ok(())
        }

        async fn read_message(&mut self) -> Result<Message, SsapError> {
            if matches!(self.incoming.front(), Some(ReadStep::Pending)) {
                return pending().await;
            }
            match self.incoming.pop_front() {
                Some(ReadStep::Message(message)) => Ok(message),
                Some(ReadStep::Pending) => unreachable!("pending step handled above"),
                None => Err(SsapError::Closed),
            }
        }
    }

    fn test_config() -> DaemonConfig {
        toml::from_str(
            r#"
bind_address = "127.0.0.1:8765"
tv_ip = "192.0.2.10"
server_host = "controller"
controller_token = "test-token"
client_key_path = "/tmp/client-key.sqlite"

[inputs]
controller = "HDMI_4"
right = "HDMI_3"
left = "HDMI_2"
"#,
        )
        .unwrap()
    }

    fn timing(config: &DaemonConfig) -> ProtocolTiming {
        ProtocolTiming {
            command_ms: config.timeouts.command_ms,
            observation_ms: config.timeouts.observation_ms,
            grant_ms: config.timeouts.grant_ms,
            wake_ms: config.timeouts.wake_ms,
            lease_ms: config.timeouts.lease_ms,
            signal_poll_ms: config.timeouts.signal_poll_ms,
        }
    }

    fn text(value: Value) -> Message {
        Message::Text(value.to_string().into())
    }

    fn response(id: &str, payload: Value) -> Message {
        text(json!({"id": id, "type": "response", "payload": payload}))
    }

    fn synchronized_messages() -> [Message; 6] {
        [
            text(json!({
                "type": "registered",
                "payload": {"client-key": "test-key"}
            })),
            response(
                codec::CURRENT_APP_SUBSCRIPTION_ID,
                json!({"subscribed": true}),
            ),
            response(
                "request-1",
                json!({"returnValue": true, "alertId": "multiview-reset"}),
            ),
            response("request-2", json!({"returnValue": true})),
            response(
                "request-3",
                json!({"returnValue": true, "appId": "com.webos.app.hdmi4"}),
            ),
            response(
                "request-4",
                json!({
                    "returnValue": true,
                    "devices": [
                        {"id": "HDMI_2", "hdmiSignalExist": false},
                        {"id": "HDMI_3", "hdmiSignalExist": false},
                        {"id": "HDMI_4", "hdmiSignalExist": true}
                    ]
                }),
            ),
        ]
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_millis(100), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition was not reached");
    }

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

    #[test]
    fn reconnect_backoff_resets_after_a_synchronized_session() {
        let mut backoff = ReconnectBackoff::new(1_000, 60_000);
        assert_eq!(backoff.failed(), (1_000, 1));
        assert_eq!(backoff.failed(), (2_000, 2));
        backoff.synchronized();
        assert_eq!(backoff.failed(), (1_000, 1));
    }

    #[tokio::test]
    async fn connected_session_registers_subscribes_and_resynchronizes() {
        let config = test_config();
        let (coordinator, mut effects, coordinator_task) = coordinator::spawn(
            ProtocolState::new(Host::Controller, 32),
            timing(&config),
            8,
            4,
        );
        let mut socket = ScriptedSocket::new(synchronized_messages());
        let mut backoff = ReconnectBackoff::new(1_000, 60_000);
        assert_eq!(backoff.failed(), (1_000, 1));
        let result = run_connected_session(
            &mut socket,
            &config,
            "test-key",
            &coordinator,
            &mut effects,
            &mut backoff,
            &RuntimeMetrics::default(),
        )
        .await;

        assert!(matches!(result, Err(SsapError::Closed)));
        wait_until(|| coordinator.snapshot().ready()).await;
        let snapshot = coordinator.snapshot();
        assert!(snapshot.ready());
        assert_eq!(snapshot.observed_input, Some(Host::Controller));
        assert!(snapshot.input_signal[&Host::Controller].present);
        assert_eq!(backoff.failed(), (1_000, 1));
        let sent: Vec<Value> = socket
            .sent
            .iter()
            .map(|message| match message {
                Message::Text(text) => serde_json::from_str(text).unwrap(),
                other => panic!("unexpected outbound message: {other:?}"),
            })
            .collect();
        assert_eq!(sent[0]["id"], "register-0");
        assert_eq!(sent[1]["id"], codec::CURRENT_APP_SUBSCRIPTION_ID);
        assert_eq!(sent[2]["id"], "request-1");
        assert_eq!(sent[2]["uri"], "ssap://system.notifications/createAlert");
        assert_eq!(
            sent[2]["payload"]["onclose"]["uri"],
            "luna://com.webos.settingsservice/setSystemSettings"
        );
        assert_eq!(
            sent[2]["payload"]["onclose"]["params"]["settings"]["splitscreenEnable"],
            "off"
        );
        assert_eq!(sent[3]["id"], "request-2");
        assert_eq!(sent[4]["id"], "request-3");
        assert_eq!(sent[5]["id"], "request-4");
        coordinator_task.abort();
    }

    #[tokio::test]
    async fn disconnected_session_resubscribes_and_resynchronizes_on_next_connection() {
        let config = test_config();
        let (coordinator, mut effects, coordinator_task) = coordinator::spawn(
            ProtocolState::new(Host::Controller, 32),
            timing(&config),
            8,
            4,
        );
        let mut backoff = ReconnectBackoff::new(1_000, 60_000);
        let metrics = RuntimeMetrics::default();
        let mut first = ScriptedSocket::new(synchronized_messages());
        assert!(matches!(
            run_connected_session(
                &mut first,
                &config,
                "test-key",
                &coordinator,
                &mut effects,
                &mut backoff,
                &metrics,
            )
            .await,
            Err(SsapError::Closed)
        ));
        wait_until(|| coordinator.snapshot().ready()).await;
        coordinator
            .apply_safety(Event::TransportDisconnected {
                reason: "test disconnect".to_string(),
            })
            .await
            .unwrap();
        assert!(!coordinator.snapshot().ready());
        assert_eq!(backoff.failed(), (1_000, 1));

        let mut second = ScriptedSocket::new(synchronized_messages());
        assert!(matches!(
            run_connected_session(
                &mut second,
                &config,
                "test-key",
                &coordinator,
                &mut effects,
                &mut backoff,
                &metrics,
            )
            .await,
            Err(SsapError::Closed)
        ));
        wait_until(|| coordinator.snapshot().ready()).await;
        assert!(second.sent.iter().any(|message| {
            matches!(message, Message::Text(text) if text.contains(codec::CURRENT_APP_SUBSCRIPTION_ID))
        }));
        assert_eq!(backoff.failed(), (1_000, 1));
        coordinator_task.abort();
    }

    #[tokio::test]
    async fn wait_for_id_handles_callback_and_discards_delayed_old_response() {
        let config = test_config();
        let (coordinator, _effects, coordinator_task) = coordinator::spawn(
            ProtocolState::new(Host::Controller, 32),
            timing(&config),
            8,
            4,
        );
        coordinator
            .apply_safety(Event::TransportSynchronized {
                mode: TvMode::Fullscreen,
                input: Some(Host::Controller),
                signals: BTreeMap::from([
                    (Host::Controller, true),
                    (Host::Right, false),
                    (Host::Left, false),
                ]),
            })
            .await
            .unwrap();
        let mut socket = ScriptedSocket::new([
            response(
                codec::CURRENT_APP_SUBSCRIPTION_ID,
                json!({
                    "returnValue": true,
                    "appId": "com.webos.app.hdmi3"
                }),
            ),
            response("request-old", json!({"returnValue": true})),
            response("request-current", json!({"returnValue": true, "value": 7})),
        ]);

        let current = wait_for_id(
            &mut socket,
            "request-current",
            100,
            &coordinator,
            &config.inputs,
        )
        .await
        .unwrap();
        assert_eq!(current["payload"]["value"], 7);
        wait_until(|| coordinator.snapshot().observed_input == Some(Host::Right)).await;
        let snapshot = coordinator.snapshot();
        assert_eq!(snapshot.phase, ProtocolPhase::Idle);
        assert!(!snapshot.fallback_required);
        assert_eq!(snapshot.manual_recovery_target, None);
        assert_eq!(snapshot.keyboard_owner, Host::Controller);
        assert_eq!(snapshot.pointer_owner, Host::Controller);
        coordinator_task.abort();
    }

    #[tokio::test]
    async fn receive_json_replies_to_ping_before_returning_payload() {
        let mut socket = ScriptedSocket::new([
            Message::Ping(vec![1, 2, 3].into()),
            text(json!({"id": "request-1", "payload": {"returnValue": true}})),
        ]);

        let value = receive_json(&mut socket).await.unwrap();
        assert_eq!(value["id"], "request-1");
        assert_eq!(socket.sent, vec![Message::Pong(vec![1, 2, 3].into())]);
    }

    #[tokio::test]
    async fn response_timeout_is_bounded() {
        let config = test_config();
        let (coordinator, _effects, coordinator_task) = coordinator::spawn(
            ProtocolState::new(Host::Controller, 32),
            timing(&config),
            8,
            4,
        );
        let mut socket = ScriptedSocket {
            incoming: VecDeque::from([ReadStep::Pending]),
            sent: Vec::new(),
        };

        let result = wait_for_id(&mut socket, "request-1", 1, &coordinator, &config.inputs).await;
        assert!(matches!(result, Err(SsapError::Timeout("response"))));
        coordinator_task.abort();
    }

    #[tokio::test]
    async fn keepalive_timeout_terminates_silent_synchronized_session() {
        let mut config = test_config();
        config.timeouts.keepalive_ms = 1;
        config.timeouts.keepalive_timeout_ms = 2;
        let (coordinator, mut effects, coordinator_task) = coordinator::spawn(
            ProtocolState::new(Host::Controller, 32),
            timing(&config),
            8,
            4,
        );
        let mut incoming: VecDeque<_> = synchronized_messages()
            .into_iter()
            .map(ReadStep::Message)
            .collect();
        incoming.push_back(ReadStep::Pending);
        let mut socket = ScriptedSocket {
            incoming,
            sent: Vec::new(),
        };
        let mut backoff = ReconnectBackoff::new(1_000, 60_000);

        let result = run_connected_session(
            &mut socket,
            &config,
            "test-key",
            &coordinator,
            &mut effects,
            &mut backoff,
            &RuntimeMetrics::default(),
        )
        .await;

        assert!(matches!(result, Err(SsapError::Timeout("keepalive"))));
        assert!(socket
            .sent
            .iter()
            .any(|message| matches!(message, Message::Ping(_))));
        coordinator_task.abort();
    }
}
