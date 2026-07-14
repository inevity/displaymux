use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use lan_mouse_clipboard::{
    AuthenticatedPeer, AuthoritySessionId, AuthorizedPeers, CLIPBOARD_TEXT_V1,
    CertificateFingerprint, ClipboardHello, ClipboardReason, ConnectionId, FrameMetadata, HostId,
    MessageType, NegotiatedPeer, PeerFence, PeerRegistry, ProcessSessionId, RegistrationOutcome,
    TlsIdentity, TransportError, TransportHandle, WireMessage, authenticate_alpn,
    authenticate_hello, authenticate_peer_certificates, client_config, clipboard_server_name,
    encode_message, read_frame, run_writer, server_config, spawn_reader, transport_queues,
    write_frame,
};
use lan_mouse_ipc::SwitchHost;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use tokio_util::sync::CancellationToken;

use crate::config::ConfigClient;

const HANDSHAKE_BUDGET: Duration = Duration::from_secs(10);
const TRANSFER_BUDGET: Duration = Duration::from_secs(10);
const RECONNECT_INITIAL: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
const PER_PEER_EVENT_CLASSES: usize = 10;
const FIXED_EVENT_CLASSES: usize = 2;

#[derive(Clone, Debug)]
pub(crate) enum PeerEndpoint {
    Ip(SocketAddr),
    Hostname(String, u16),
}

impl PeerEndpoint {
    async fn connect(&self) -> io::Result<TcpStream> {
        match self {
            Self::Ip(address) => TcpStream::connect(address).await,
            Self::Hostname(hostname, port) => TcpStream::connect((hostname.as_str(), *port)).await,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClipboardPeerConfig {
    pub(crate) host_id: HostId,
    pub(crate) fingerprint: CertificateFingerprint,
    pub(crate) endpoints: Vec<PeerEndpoint>,
}

pub(crate) fn configured_peers(
    clients: Vec<ConfigClient>,
    authorized_fingerprints: &HashMap<String, String>,
) -> (Vec<ClipboardPeerConfig>, AuthorizedPeers) {
    let mut peer_by_host = HashMap::<HostId, ClipboardPeerConfig>::new();
    let mut authorized = Vec::new();

    for client in clients {
        let (Some(target), Some(hostname)) = (client.switch_target, client.hostname.as_ref())
        else {
            continue;
        };
        let canonical_host = HostId::from(target.to_string());
        let fingerprint = authorized_fingerprints
            .iter()
            .find_map(|(fingerprint, description)| {
                (description == hostname || description == canonical_host.as_str())
                    .then(|| CertificateFingerprint::parse(fingerprint).ok())
                    .flatten()
            });
        let Some(fingerprint) = fingerprint else {
            tracing::debug!(
                event = "clipboard_backend_unavailable",
                host = %canonical_host,
                reason = ClipboardReason::CapabilityMissing.code(),
                "clipboard peer has no canonical authorized fingerprint mapping"
            );
            continue;
        };

        let mut endpoints = client
            .ips
            .iter()
            .copied()
            .map(|ip| PeerEndpoint::Ip(SocketAddr::new(ip, client.port)))
            .collect::<Vec<_>>();
        endpoints.push(PeerEndpoint::Hostname(hostname.clone(), client.port));
        let peer = ClipboardPeerConfig {
            host_id: canonical_host.clone(),
            fingerprint,
            endpoints,
        };
        authorized.push((fingerprint, canonical_host.clone()));
        peer_by_host.insert(canonical_host, peer);
    }

    let mut peers = peer_by_host.into_values().collect::<Vec<_>>();
    peers.sort_by(|left, right| left.host_id.cmp(&right.host_id));
    (peers, AuthorizedPeers::new(authorized))
}

#[derive(Clone)]
pub(crate) enum ClipboardTransportRole {
    Authority {
        authority_session_id: AuthoritySessionId,
    },
    Peer {
        authority_session_id: Arc<RwLock<Option<AuthoritySessionId>>>,
    },
}

pub(crate) struct ClipboardTransportConfig {
    pub(crate) enabled: bool,
    pub(crate) local_host: HostId,
    pub(crate) process_session_id: ProcessSessionId,
    pub(crate) max_bytes: usize,
    pub(crate) capabilities: u64,
    pub(crate) port: u16,
    pub(crate) identity: TlsIdentity,
    pub(crate) peers: Vec<ClipboardPeerConfig>,
    pub(crate) authorized_peers: AuthorizedPeers,
    pub(crate) role: ClipboardTransportRole,
}

pub(crate) enum ClipboardTransportEvent {
    Connected {
        peer: NegotiatedPeer,
        outbound: TransportHandle,
    },
    Disconnected {
        host_id: HostId,
        process_session_id: ProcessSessionId,
        connection_id: ConnectionId,
    },
    Message {
        host_id: HostId,
        process_session_id: ProcessSessionId,
        connection_id: ConnectionId,
        message: WireMessage,
    },
}

pub(crate) struct ClipboardTransport {
    event_rx: mpsc::Receiver<ClipboardTransportEvent>,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl ClipboardTransport {
    pub(crate) fn start(config: ClipboardTransportConfig) -> Self {
        let event_capacity = FIXED_EVENT_CLASSES.saturating_add(
            config
                .peers
                .len()
                .max(1)
                .saturating_mul(PER_PEER_EVENT_CLASSES),
        );
        let (event_tx, event_rx) = mpsc::channel(event_capacity);
        let cancellation = CancellationToken::new();
        let task = config
            .enabled
            .then(|| tokio::task::spawn_local(run_manager(config, event_tx, cancellation.clone())));
        Self {
            event_rx,
            cancellation,
            task,
        }
    }

    pub(crate) async fn event(&mut self) -> Option<ClipboardTransportEvent> {
        self.event_rx.recv().await
    }

    pub(crate) fn shutdown(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for ClipboardTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct RawConnection {
    authenticated_peer: AuthenticatedPeer,
    hello: ClipboardHello,
    stream: TlsStream<TcpStream>,
    reconnect: Option<oneshot::Sender<()>>,
}

enum ManagerEvent {
    Connection(RawConnection),
    Ended {
        host_id: HostId,
        connection_id: ConnectionId,
    },
}

struct ActiveConnection {
    process_session_id: ProcessSessionId,
    connection_id: ConnectionId,
    outbound: TransportHandle,
    cancellation: CancellationToken,
    reconnect: Option<oneshot::Sender<()>>,
}

async fn run_manager(
    config: ClipboardTransportConfig,
    event_tx: mpsc::Sender<ClipboardTransportEvent>,
    cancellation: CancellationToken,
) {
    let manager_capacity = config.peers.len().max(1).saturating_mul(2);
    let (manager_tx, mut manager_rx) = mpsc::channel(manager_capacity);
    match &config.role {
        ClipboardTransportRole::Authority { .. } => {
            for peer in config.peers.clone() {
                tokio::task::spawn_local(run_connector(
                    peer,
                    config.identity.clone(),
                    config.local_host.clone(),
                    config.process_session_id,
                    config.capabilities,
                    config.max_bytes,
                    manager_tx.clone(),
                    cancellation.child_token(),
                ));
            }
        }
        ClipboardTransportRole::Peer { .. } => {
            tokio::task::spawn_local(run_listener(
                config.port,
                config.identity.clone(),
                config.authorized_peers.clone(),
                config.local_host.clone(),
                config.process_session_id,
                config.capabilities,
                config.max_bytes,
                manager_tx.clone(),
                cancellation.child_token(),
            ));
        }
    }

    let mut registry = PeerRegistry::default();
    let mut active = HashMap::<HostId, ActiveConnection>::new();
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            event = manager_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    ManagerEvent::Connection(raw) => {
                        let outcome = registry.register(
                            &raw.authenticated_peer,
                            &raw.hello,
                            config.max_bytes,
                        );
                        let peer = match outcome {
                            Ok(RegistrationOutcome::Accepted(peer)) => peer,
                            Ok(RegistrationOutcome::Replaced { peer, .. }) => {
                                if let Some(previous) = active.remove(&peer.host_id) {
                                    previous.outbound.shutdown();
                                    previous.cancellation.cancel();
                                    if let Some(reconnect) = previous.reconnect {
                                        let _ = reconnect.send(());
                                    }
                                }
                                peer
                            }
                            Ok(RegistrationOutcome::DuplicateRejected { .. }) | Err(_) => {
                                if let Some(reconnect) = raw.reconnect {
                                    let _ = reconnect.send(());
                                }
                                continue;
                            }
                        };
                        let (outbound, connection_cancellation) = start_connection(
                            raw.stream,
                            peer.clone(),
                            config.role.clone(),
                            config.local_host.clone(),
                            config.process_session_id,
                            config.max_bytes,
                            manager_tx.clone(),
                            event_tx.clone(),
                        );
                        if event_tx.try_send(ClipboardTransportEvent::Connected {
                            peer: peer.clone(),
                            outbound: outbound.clone(),
                        }).is_err() {
                            outbound.shutdown();
                            connection_cancellation.cancel();
                            if let Some(reconnect) = raw.reconnect {
                                let _ = reconnect.send(());
                            }
                            continue;
                        }
                        active.insert(peer.host_id.clone(), ActiveConnection {
                            process_session_id: peer.process_session_id,
                            connection_id: peer.connection_id,
                            outbound,
                            cancellation: connection_cancellation,
                            reconnect: raw.reconnect,
                        });
                    }
                    ManagerEvent::Ended { host_id, connection_id } => {
                        if active.get(&host_id).is_some_and(|connection| {
                            connection.connection_id == connection_id
                        }) {
                            let connection = active.remove(&host_id).expect("active checked");
                            registry.unregister(&host_id, connection_id);
                            let _ = event_tx.try_send(ClipboardTransportEvent::Disconnected {
                                host_id,
                                process_session_id: connection.process_session_id,
                                connection_id,
                            });
                            if let Some(reconnect) = connection.reconnect {
                                let _ = reconnect.send(());
                            }
                        }
                    }
                }
            }
        }
    }
    for (_, connection) in active {
        connection.outbound.shutdown();
        connection.cancellation.cancel();
        if let Some(reconnect) = connection.reconnect {
            let _ = reconnect.send(());
        }
    }
}

fn start_connection(
    stream: TlsStream<TcpStream>,
    peer: NegotiatedPeer,
    role: ClipboardTransportRole,
    local_host: HostId,
    local_process_session_id: ProcessSessionId,
    max_bytes: usize,
    manager_tx: mpsc::Sender<ManagerEvent>,
    event_tx: mpsc::Sender<ClipboardTransportEvent>,
) -> (TransportHandle, CancellationToken) {
    let (reader, mut writer) = tokio::io::split(stream);
    let (outbound, mut outbound_rx) = transport_queues();
    let cancellation = CancellationToken::new();
    let reader_cancellation = cancellation.child_token();
    let validator = metadata_validator(
        role.clone(),
        peer.clone(),
        local_host,
        local_process_session_id,
    );
    let (mut inbound, mut reader_task) = spawn_reader(
        reader,
        peer.effective_max_bytes.unwrap_or(max_bytes),
        TRANSFER_BUDGET,
        reader_cancellation,
        validator,
    );
    let mut writer_task = tokio::spawn(async move {
        run_writer(
            &mut writer,
            &mut outbound_rx,
            peer.effective_max_bytes.unwrap_or(max_bytes),
            TRANSFER_BUDGET,
        )
        .await
    });
    let connection_id = peer.connection_id;
    let host_id = peer.host_id.clone();
    let process_session_id = peer.process_session_id;
    let task_cancellation = cancellation.clone();
    tokio::task::spawn_local(async move {
        loop {
            tokio::select! {
                _ = task_cancellation.cancelled() => break,
                result = &mut reader_task => {
                    if let Ok(Err(error)) = result {
                        tracing::debug!(event = "clipboard_transfer_rejected", host = %host_id, error = %error);
                    }
                    break;
                }
                result = &mut writer_task => {
                    if let Ok(Err(error)) = result {
                        tracing::debug!(event = "clipboard_transfer_rejected", host = %host_id, error = %error);
                    }
                    break;
                }
                message = inbound.control_rx.recv() => {
                    let Some(message) = message else { break };
                    if !accept_authority_state(&role, process_session_id, &message) {
                        break;
                    }
                    if event_tx.try_send(ClipboardTransportEvent::Message {
                        host_id: host_id.clone(),
                        process_session_id,
                        connection_id,
                        message,
                    }).is_err() {
                        break;
                    }
                }
                payload = inbound.payload_rx.recv() => {
                    let Some(payload) = payload else { break };
                    let message = match &role {
                        ClipboardTransportRole::Authority { .. } => WireMessage::SnapshotOffer(payload),
                        ClipboardTransportRole::Peer { .. } => WireMessage::SnapshotDeliver(payload),
                    };
                    if event_tx.try_send(ClipboardTransportEvent::Message {
                        host_id: host_id.clone(),
                        process_session_id,
                        connection_id,
                        message,
                    }).is_err() {
                        break;
                    }
                }
            }
        }
        task_cancellation.cancel();
        reader_task.abort();
        writer_task.abort();
        let _ = manager_tx
            .send(ManagerEvent::Ended {
                host_id,
                connection_id,
            })
            .await;
    });
    (outbound, cancellation)
}

fn metadata_validator(
    role: ClipboardTransportRole,
    peer: NegotiatedPeer,
    local_host: HostId,
    local_process_session_id: ProcessSessionId,
) -> impl Fn(&FrameMetadata) -> Result<(), TransportError> + Send + Sync + 'static {
    move |metadata| match &role {
        ClipboardTransportRole::Authority {
            authority_session_id,
        } => {
            let fence = PeerFence {
                authority_session_id: *authority_session_id,
                host_id: peer.host_id.clone(),
                process_session_id: peer.process_session_id,
                connection_id: peer.connection_id,
            };
            match metadata.message_type {
                MessageType::SnapshotOffer => fence.validate_source_metadata(metadata),
                MessageType::PrepareResult | MessageType::ApplyResult => {
                    fence.validate_target_metadata(metadata)
                }
                MessageType::ProtocolError => Ok(()),
                _ => Err(TransportError::AuthenticatedHostMismatch),
            }
        }
        ClipboardTransportRole::Peer {
            authority_session_id,
        } => validate_authority_metadata(
            metadata,
            authority_session_id,
            &local_host,
            local_process_session_id,
        ),
    }
}

fn validate_authority_metadata(
    metadata: &FrameMetadata,
    authority_session: &RwLock<Option<AuthoritySessionId>>,
    local_host: &HostId,
    local_process_session_id: ProcessSessionId,
) -> Result<(), TransportError> {
    if matches!(metadata.message_type, MessageType::AuthorityState) {
        return Ok(());
    }
    if matches!(metadata.message_type, MessageType::ProtocolError) {
        return Ok(());
    }
    let authority_session_id = authority_session
        .read()
        .expect("authority session lock poisoned")
        .ok_or(TransportError::StaleAuthoritySession)?;
    let handoff_id = metadata.handoff_id.ok_or(TransportError::MissingIdentity)?;
    if handoff_id.authority_session_id != authority_session_id {
        return Err(TransportError::StaleAuthoritySession);
    }
    if matches!(
        metadata.message_type,
        MessageType::PrepareTarget | MessageType::OwnershipActivated | MessageType::SnapshotDeliver
    ) {
        if metadata
            .target_token
            .as_ref()
            .is_none_or(|token| token.owner_host_id != *local_host)
        {
            return Err(TransportError::AuthenticatedHostMismatch);
        }
        if metadata.target_process_session_id != Some(local_process_session_id) {
            return Err(TransportError::StalePeerSession);
        }
    }
    Ok(())
}

fn accept_authority_state(
    role: &ClipboardTransportRole,
    peer_process_session_id: ProcessSessionId,
    message: &WireMessage,
) -> bool {
    let ClipboardTransportRole::Peer {
        authority_session_id,
    } = role
    else {
        return true;
    };
    let WireMessage::AuthorityState(state) = message else {
        return true;
    };
    if state.authority_process_session_id != peer_process_session_id {
        return false;
    }
    let session = state.current_token.authority_session_id;
    if state.active_handoff.as_ref().is_some_and(|handoff| {
        handoff.handoff_id.authority_session_id != session
            || handoff.source_token.authority_session_id != session
            || handoff.target_token.authority_session_id != session
    }) {
        return false;
    }
    *authority_session_id
        .write()
        .expect("authority session lock poisoned") = Some(session);
    true
}

async fn run_connector(
    peer: ClipboardPeerConfig,
    identity: TlsIdentity,
    local_host: HostId,
    process_session_id: ProcessSessionId,
    capabilities: u64,
    max_bytes: usize,
    manager_tx: mpsc::Sender<ManagerEvent>,
    cancellation: CancellationToken,
) {
    let config = match client_config(&identity, peer.fingerprint) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(event = "clipboard_backend_unavailable", host = %peer.host_id, error = %error);
            return;
        }
    };
    let mut backoff = RECONNECT_INITIAL;
    loop {
        let connection = connect_peer(
            &peer,
            config.clone(),
            local_host.clone(),
            process_session_id,
            capabilities,
            max_bytes,
            &cancellation,
        )
        .await;
        match connection {
            Ok((authenticated_peer, hello, stream)) => {
                backoff = RECONNECT_INITIAL;
                let (reconnect_tx, reconnect_rx) = oneshot::channel();
                if manager_tx
                    .send(ManagerEvent::Connection(RawConnection {
                        authenticated_peer,
                        hello,
                        stream,
                        reconnect: Some(reconnect_tx),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = reconnect_rx => {}
                }
            }
            Err(error) => tracing::debug!(
                event = "clipboard_backend_unavailable",
                host = %peer.host_id,
                error = %error,
                "clipboard peer connection failed"
            ),
        }
        tokio::select! {
            _ = cancellation.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = backoff.saturating_mul(2).min(RECONNECT_MAX);
    }
}

async fn connect_peer(
    peer: &ClipboardPeerConfig,
    config: Arc<rustls::ClientConfig>,
    local_host: HostId,
    process_session_id: ProcessSessionId,
    capabilities: u64,
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<(AuthenticatedPeer, ClipboardHello, TlsStream<TcpStream>), EstablishError> {
    let mut last_error = None;
    for endpoint in &peer.endpoints {
        let result = tokio::select! {
            _ = cancellation.cancelled() => return Err(EstablishError::Canceled),
            result = endpoint.connect() => result,
        };
        let stream = match result {
            Ok(stream) => stream,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let connector = TlsConnector::from(config.clone());
        let stream = tokio::time::timeout(
            HANDSHAKE_BUDGET,
            connector.connect(clipboard_server_name(), stream),
        )
        .await
        .map_err(|_| EstablishError::Timeout)??;
        authenticate_alpn(stream.get_ref().1.alpn_protocol())?;
        let authenticated_peer = AuthenticatedPeer {
            host_id: peer.host_id.clone(),
            fingerprint: peer.fingerprint,
        };
        let stream = TlsStream::Client(stream);
        let (stream, hello) = exchange_hello(
            stream,
            &authenticated_peer,
            local_host,
            process_session_id,
            capabilities,
            max_bytes,
            cancellation,
        )
        .await?;
        return Ok((authenticated_peer, hello, stream));
    }
    Err(EstablishError::Io(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "clipboard peer has no endpoint")
    })))
}

async fn run_listener(
    port: u16,
    identity: TlsIdentity,
    authorized_peers: AuthorizedPeers,
    local_host: HostId,
    process_session_id: ProcessSessionId,
    capabilities: u64,
    max_bytes: usize,
    manager_tx: mpsc::Sender<ManagerEvent>,
    cancellation: CancellationToken,
) {
    let config = match server_config(&identity, authorized_peers.clone()) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(event = "clipboard_backend_unavailable", error = %error);
            return;
        }
    };
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(event = "clipboard_backend_unavailable", error = %error, port);
            return;
        }
    };
    loop {
        let accepted = tokio::select! {
            _ = cancellation.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else {
            continue;
        };
        let acceptor = TlsAcceptor::from(config.clone());
        let authorized_peers = authorized_peers.clone();
        let local_host = local_host.clone();
        let manager_tx = manager_tx.clone();
        let cancellation = cancellation.child_token();
        tokio::task::spawn_local(async move {
            let result = async {
                let stream = tokio::time::timeout(HANDSHAKE_BUDGET, acceptor.accept(stream))
                    .await
                    .map_err(|_| EstablishError::Timeout)??;
                authenticate_alpn(stream.get_ref().1.alpn_protocol())?;
                let authenticated_peer = authenticate_peer_certificates(
                    stream.get_ref().1.peer_certificates(),
                    &authorized_peers,
                )?;
                let stream = TlsStream::Server(stream);
                let (stream, hello) = exchange_hello(
                    stream,
                    &authenticated_peer,
                    local_host,
                    process_session_id,
                    capabilities,
                    max_bytes,
                    &cancellation,
                )
                .await?;
                Ok::<_, EstablishError>((authenticated_peer, hello, stream))
            }
            .await;
            if let Ok((authenticated_peer, hello, stream)) = result {
                let _ = manager_tx
                    .send(ManagerEvent::Connection(RawConnection {
                        authenticated_peer,
                        hello,
                        stream,
                        reconnect: None,
                    }))
                    .await;
            }
        });
    }
}

async fn exchange_hello(
    mut stream: TlsStream<TcpStream>,
    authenticated_peer: &AuthenticatedPeer,
    local_host: HostId,
    process_session_id: ProcessSessionId,
    capabilities: u64,
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<(TlsStream<TcpStream>, ClipboardHello), EstablishError> {
    let hello = WireMessage::ClipboardHello(ClipboardHello {
        host_id: local_host,
        process_session_id,
        offered_capabilities: capabilities,
        max_receive_bytes: u64::try_from(max_bytes).map_err(|_| EstablishError::InvalidLimit)?,
    });
    let frame = encode_message(&hello, max_bytes)?;
    write_frame(&mut stream, &frame, HANDSHAKE_BUDGET, cancellation).await?;
    let peer_hello =
        match read_frame(&mut stream, max_bytes, HANDSHAKE_BUDGET, cancellation).await? {
            WireMessage::ClipboardHello(hello) => hello,
            _ => return Err(EstablishError::MissingHello),
        };
    authenticate_hello(authenticated_peer, &peer_hello)?;
    Ok((stream, peer_hello))
}

#[derive(Debug, thiserror::Error)]
enum EstablishError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] lan_mouse_clipboard::FrameError),
    #[error(transparent)]
    Tls(#[from] lan_mouse_clipboard::TlsError),
    #[error("clipboard handshake timed out")]
    Timeout,
    #[error("clipboard connection canceled")]
    Canceled,
    #[error("clipboard peer did not send hello first")]
    MissingHello,
    #[error("clipboard maximum is not representable on the wire")]
    InvalidLimit,
}

pub(crate) fn text_v1_capability(backend_ready: bool) -> u64 {
    if backend_ready { CLIPBOARD_TEXT_V1 } else { 0 }
}

pub(crate) fn host_id(host: SwitchHost) -> HostId {
    HostId::from(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, net::IpAddr};

    fn client(hostname: &str, target: SwitchHost) -> ConfigClient {
        ConfigClient {
            ips: HashSet::from([IpAddr::from([127, 0, 0, 1])]),
            hostname: Some(hostname.to_string()),
            port: 4242,
            pos: lan_mouse_ipc::Position::Left,
            active: true,
            switch_target: Some(target),
        }
    }

    #[test]
    fn configured_fingerprint_label_maps_to_canonical_switch_host() {
        let fingerprint = "00:01:02:03:04:05:06:07:08:09:0a:0b:0c:0d:0e:0f:10:11:12:13:14:15:16:17:18:19:1a:1b:1c:1d:1e:1f";
        let authorized = HashMap::from([(fingerprint.to_string(), "win-desktop".to_string())]);

        let (peers, authorized_peers) = configured_peers(
            vec![client("win-desktop", SwitchHost::Windows)],
            &authorized,
        );

        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].host_id, HostId::from("windows"));
        assert_eq!(
            authorized_peers.host_for(CertificateFingerprint::parse(fingerprint).unwrap()),
            Some(HostId::from("windows"))
        );
    }

    #[test]
    fn unmatched_fingerprint_is_not_authorized_for_clipboard() {
        let (peers, authorized_peers) = configured_peers(
            vec![client("win-desktop", SwitchHost::Windows)],
            &HashMap::new(),
        );

        assert!(peers.is_empty());
        assert!(
            authorized_peers
                .host_for(CertificateFingerprint::from_certificate(b"unknown"))
                .is_none()
        );
    }

    #[test]
    fn capability_is_absent_until_native_backend_is_ready() {
        assert_eq!(text_v1_capability(false), 0);
        assert_eq!(text_v1_capability(true), CLIPBOARD_TEXT_V1);
    }
}
