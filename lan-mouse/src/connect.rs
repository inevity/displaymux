use crate::client::ClientManager;
use crate::config::local_commit;
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
    time::{MissedTickBehavior, timeout},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
const CONNECTION_SEND_TIMEOUT: Duration = CONNECTION_MAINTENANCE_INTERVAL;

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, LanMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), LanMouseConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(LanMouseConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

pub(crate) struct LanMouseConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    maintenance_task: tokio::task::JoinHandle<()>,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
}

impl LanMouseConnection {
    pub(crate) fn new(cert: Certificate, client_manager: ClientManager) -> Self {
        let (recv_tx, recv_rx) = channel();
        let conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>> = Default::default();
        let connecting: Rc<Mutex<HashSet<ClientHandle>>> = Default::default();
        let ping_response: Rc<RefCell<HashSet<SocketAddr>>> = Default::default();
        let maintenance_task = spawn_connection_maintenance(
            client_manager.clone(),
            cert.clone(),
            conns.clone(),
            connecting.clone(),
            recv_tx.clone(),
            ping_response.clone(),
        );
        Self {
            cert,
            client_manager,
            conns,
            connecting,
            maintenance_task,
            recv_rx,
            recv_tx,
            ping_response,
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
                if !self.client_manager.alive(handle) {
                    return Err(LanMouseConnectionError::TargetEmulationDisabled);
                }
                match timeout(CONNECTION_SEND_TIMEOUT, conn.send(buf)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        log::warn!("client {handle} failed to send: {e}");
                        fail_connection(
                            &self.client_manager,
                            handle,
                            addr,
                            &conn,
                            &self.conns,
                            &self.ping_response,
                            &self.recv_tx,
                        )
                        .await;
                        return Err(e.into());
                    }
                    Err(_) => {
                        log::warn!("client {handle} send timed out");
                        fail_connection(
                            &self.client_manager,
                            handle,
                            addr,
                            &conn,
                            &self.conns,
                            &self.ping_response,
                            &self.recv_tx,
                        )
                        .await;
                        return Err(LanMouseConnectionError::Timeout);
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        request_connection(
            self.client_manager.clone(),
            self.cert.clone(),
            handle,
            self.conns.clone(),
            self.connecting.clone(),
            self.recv_tx.clone(),
            self.ping_response.clone(),
        );
        Err(LanMouseConnectionError::NotConnected)
    }
}

impl Drop for LanMouseConnection {
    fn drop(&mut self) {
        self.maintenance_task.abort();
    }
}

fn spawn_connection_maintenance(
    client_manager: ClientManager,
    cert: Certificate,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) -> tokio::task::JoinHandle<()> {
    spawn_local(async move {
        let mut interval = tokio::time::interval(CONNECTION_MAINTENANCE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            for handle in client_manager.active_clients() {
                request_connection(
                    client_manager.clone(),
                    cert.clone(),
                    handle,
                    conns.clone(),
                    connecting.clone(),
                    tx.clone(),
                    ping_response.clone(),
                );
            }
        }
    })
}

fn request_connection(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    if client_manager.active_addr(handle).is_some()
        || !client_manager
            .get_state(handle)
            .is_some_and(|(_, state)| state.active)
    {
        return;
    }

    spawn_local(async move {
        let mut connecting_guard = connecting.lock().await;
        if !connecting_guard.insert(handle) {
            return;
        }
        drop(connecting_guard);

        if let Err(error) = connect_to_handle(
            client_manager,
            cert,
            handle,
            conns,
            connecting,
            tx,
            ping_response,
        )
        .await
        {
            log::debug!("client {handle} connection attempt ended: {error}");
        }
    });
}

async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let res = connect_any(&addrs, cert).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        log::info!("client ({handle}) connected @ {addr}");
        client_manager.set_active_addr(handle, Some(addr));
        conns.lock().await.insert(addr, conn.clone());
        connecting.lock().await.remove(&handle);

        // Best-effort version handshake. Send our commit hash once
        // immediately after the DTLS handshake; the listen side
        // mirrors a Hello back so the receive loop can populate
        // `peer_commit`. Old peers will silently skip this event
        // per the forward-compat handler in [`receive_loop`].
        let (buf, len) = ProtoEvent::Hello {
            commit: local_commit(),
        }
        .into();
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("hello send to {addr} failed: {e}");
        }

        // poll connection for active
        spawn_local(ping_pong(
            client_manager.clone(),
            handle,
            addr,
            conn.clone(),
            conns.clone(),
            tx.clone(),
            ping_response.clone(),
        ));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            conn,
            conns,
            tx,
            ping_response.clone(),
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(LanMouseConnectionError::NotConnected)
}

async fn ping_pong(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                fail_connection(
                    &client_manager,
                    handle,
                    addr,
                    &conn,
                    &conns,
                    &ping_response,
                    &tx,
                )
                .await;
                return;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            fail_connection(
                &client_manager,
                handle,
                addr,
                &conn,
                &conns,
                &ping_response,
                &tx,
            )
            .await;
            return;
        }
    }
}

async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    let mut buf = [0u8; MAX_EVENT_SIZE];
    let mut latest_peer_session_epoch = None;
    while conn.recv(&mut buf).await.is_ok() {
        let current = conns
            .lock()
            .await
            .get(&addr)
            .is_some_and(|active| Arc::ptr_eq(active, &conn));
        if !current {
            break;
        }
        match buf.try_into() {
            Ok(event) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong(control_ready) => {
                        record_pong(&client_manager, handle, addr, control_ready);
                        ping_response.borrow_mut().insert(addr);
                    }
                    ProtoEvent::Hello { commit } => {
                        if client_manager.set_peer_commit(handle, Some(commit)) {
                            tx.send((handle, ProtoEvent::Hello { commit }))
                                .expect("channel closed");
                        }
                    }
                    ProtoEvent::Readiness {
                        keyboard_ready,
                        pointer_ready,
                        session_epoch,
                    } => {
                        if !accept_readiness_epoch(
                            &mut latest_peer_session_epoch,
                            keyboard_ready,
                            pointer_ready,
                            session_epoch,
                        ) {
                            continue;
                        }
                        client_manager.set_peer_readiness(
                            handle,
                            keyboard_ready,
                            pointer_ready,
                            session_epoch,
                        );
                        tx.send((handle, event)).expect("channel closed");
                    }
                    event => tx.send((handle, event)).expect("channel closed"),
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    fail_connection(
        &client_manager,
        handle,
        addr,
        &conn,
        &conns,
        &ping_response,
        &tx,
    )
    .await;
}

fn record_pong(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    _control_ready: bool,
) {
    // Receipt proves transport liveness. Atomic input availability is carried
    // separately by Readiness and must not make a responsive daemon offline.
    client_manager.set_active_addr(handle, Some(addr));
    client_manager.set_alive(handle, true);
}

fn accept_readiness_epoch(
    latest: &mut Option<u64>,
    keyboard_ready: bool,
    pointer_ready: bool,
    candidate: u64,
) -> bool {
    if candidate == 0 {
        return latest.is_none() && !keyboard_ready && !pointer_ready;
    }
    if latest.is_some_and(|current| candidate < current) {
        return false;
    }
    *latest = Some(candidate);
    true
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    expected_conn: &Arc<dyn Conn + Send + Sync>,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
) -> bool {
    log::warn!("client ({handle}) @ {addr} connection closed");
    let mut conns_guard = conns.lock().await;
    let current = conns_guard
        .get(&addr)
        .is_some_and(|active| Arc::ptr_eq(active, expected_conn));
    if !current {
        return false;
    }
    conns_guard.remove(&addr);
    let active: Vec<SocketAddr> = conns_guard.keys().copied().collect();
    drop(conns_guard);
    client_manager.set_active_addr(handle, None);
    client_manager.set_alive(handle, false);
    client_manager.set_peer_commit(handle, None);
    client_manager.clear_peer_readiness(handle);
    log::info!("active connections: {active:?}");
    true
}

async fn fail_connection(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: &Arc<dyn Conn + Send + Sync>,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
    ping_response: &RefCell<HashSet<SocketAddr>>,
    tx: &Sender<(ClientHandle, ProtoEvent)>,
) {
    ping_response.borrow_mut().remove(&addr);

    // Reconnection state is authoritative and must not depend on a blocked
    // receive task or on the failed transport accepting a close operation.
    if disconnect(client_manager, handle, addr, conn, conns).await {
        tx.send((
            handle,
            ProtoEvent::Readiness {
                keyboard_ready: false,
                pointer_ready: false,
                session_epoch: 0,
            },
        ))
        .expect("channel closed");
    }

    match timeout(CONNECTION_SEND_TIMEOUT, conn.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::debug!("failed to close connection {addr}: {error}"),
        Err(_) => log::warn!("timed out closing failed connection {addr}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lan_mouse_ipc::{ClientConfig, ClientState};
    use std::collections::HashSet;
    use tokio::task::LocalSet;

    #[tokio::test(flavor = "current_thread")]
    async fn active_client_starts_connection_without_input_send() {
        LocalSet::new()
            .run_until(async {
                let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let sink_addr = sink.local_addr().unwrap();
                let client_manager = ClientManager::default();
                let handle = client_manager.add_client();
                let ip = sink_addr.ip();
                client_manager.set_config(
                    handle,
                    ClientConfig {
                        fix_ips: vec![ip],
                        port: sink_addr.port(),
                        ..Default::default()
                    },
                );
                client_manager.set_state(
                    handle,
                    ClientState {
                        active: true,
                        ips: HashSet::from([ip]),
                        ..Default::default()
                    },
                );
                let cert = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
                let connection = LanMouseConnection::new(cert, client_manager);

                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        if connection.connecting.lock().await.contains(&handle) {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("active client did not start a readiness connection");
            })
            .await;
    }

    #[test]
    fn readiness_order_is_scoped_to_one_connection() {
        let mut first_connection = None;
        assert!(accept_readiness_epoch(
            &mut first_connection,
            true,
            true,
            100
        ));
        assert!(accept_readiness_epoch(
            &mut first_connection,
            false,
            false,
            102
        ));
        assert!(!accept_readiness_epoch(
            &mut first_connection,
            true,
            true,
            101
        ));

        let mut restarted_connection = None;
        assert!(accept_readiness_epoch(
            &mut restarted_connection,
            true,
            true,
            3
        ));
    }

    #[test]
    fn zero_epoch_only_describes_initial_unavailability() {
        let mut latest = None;
        assert!(accept_readiness_epoch(&mut latest, false, false, 0));
        assert!(!accept_readiness_epoch(&mut latest, true, true, 0));
        assert!(accept_readiness_epoch(&mut latest, true, true, 7));
        assert!(!accept_readiness_epoch(&mut latest, false, false, 0));
    }

    #[test]
    fn pong_proves_liveness_even_when_input_is_unavailable() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr = "127.0.0.1:4242".parse().unwrap();

        record_pong(&client_manager, handle, addr, false);

        assert!(client_manager.alive(handle));
        assert_eq!(client_manager.active_addr(handle), Some(addr));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transport_failure_clears_reconnect_gate_before_receive_loop_exit() {
        LocalSet::new()
            .run_until(async {
                let client_manager = ClientManager::default();
                let handle = client_manager.add_client();
                let addr: SocketAddr = "127.0.0.1:4243".parse().unwrap();
                client_manager.set_active_addr(handle, Some(addr));
                client_manager.set_alive(handle, true);
                client_manager.set_peer_commit(handle, Some(local_commit()));
                assert!(client_manager.set_peer_readiness(handle, true, true, 17));

                let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let conn: Arc<dyn Conn + Send + Sync> = Arc::new(socket);
                let conns = Mutex::new(HashMap::from([(addr, conn.clone())]));
                let ping_response = RefCell::new(HashSet::from([addr]));
                let (tx, mut rx) = channel();

                fail_connection(
                    &client_manager,
                    handle,
                    addr,
                    &conn,
                    &conns,
                    &ping_response,
                    &tx,
                )
                .await;

                assert_eq!(client_manager.active_addr(handle), None);
                assert!(!client_manager.alive(handle));
                assert_eq!(
                    client_manager.peer_input_readiness(handle),
                    Some((false, false, false, 0))
                );
                assert!(conns.lock().await.is_empty());
                assert!(!ping_response.borrow().contains(&addr));
                let (event_handle, event) = rx.recv().await.expect("readiness reset event");
                assert_eq!(event_handle, handle);
                assert!(
                    matches!(
                        event,
                        ProtoEvent::Readiness {
                            keyboard_ready: false,
                            pointer_ready: false,
                            session_epoch: 0,
                        }
                    ),
                    "unexpected event: {event:?}"
                );
            })
            .await;
    }
}
