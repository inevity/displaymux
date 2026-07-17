use crate::config::local_commit;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError};
use input_event::Event;
use lan_mouse_proto::{Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    future,
    net::SocketAddr,
    pin::Pin,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    sync::oneshot,
    task::{JoinHandle, spawn_local},
    time::Sleep,
};

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: lan_mouse_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    /// connection closed
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled {
        session_epoch: u64,
    },
    /// emulation was enabled
    EmulationEnabled {
        keyboard_ready: bool,
        pointer_ready: bool,
        session_epoch: u64,
    },
    /// Capture must be fully released before an incoming handoff can continue.
    ReleaseCapture {
        completion: oneshot::Sender<bool>,
    },
    /// peer sent us a Hello with its build commit hash. Used to
    /// populate `client_manager.peer_commit` from the listen side
    /// too — without this, peer-version visibility silently fails
    /// whenever the outgoing connection in the *other* direction is
    /// broken (one-way setups, asymmetric NAT, peer's TCP listener
    /// down). The connect-side path stays as the primary source;
    /// this is the defensive fallback.
    PeerHello {
        addr: SocketAddr,
        commit: [u8; 8],
    },
    ReleaseAcknowledged {
        addr: SocketAddr,
        release_epoch: u64,
    },
}

enum EmulationRequest {
    Reenable,
    Release {
        addr: SocketAddr,
        release_epoch: u64,
    },
    ChangePort(u16),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        display_selector: Option<String>,
        listener: LanMouseListener,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend, display_selector);
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
        }
    }

    pub(crate) fn request_capture_release(&self, addr: SocketAddr, release_epoch: u64) {
        self.request_tx
            .send(EmulationRequest::Release {
                addr,
                release_epoch,
            })
            .expect("channel closed");
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .expect("channel closed")
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .expect("channel closed");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: LanMouseListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
}

#[derive(Default)]
struct PendingReleases {
    epochs: HashMap<SocketAddr, u64>,
}

const ENTER_HANDOFF_TIMEOUT: Duration = Duration::from_secs(2);

enum PendingEnterStep {
    Releasing(oneshot::Receiver<bool>),
    Centering(oneshot::Receiver<bool>),
}

struct PendingEnter {
    addr: SocketAddr,
    pos: Position,
    fingerprint: String,
    step: PendingEnterStep,
    deadline: Pin<Box<Sleep>>,
}

impl PendingEnter {
    fn new(
        addr: SocketAddr,
        pos: Position,
        fingerprint: String,
        release: oneshot::Receiver<bool>,
    ) -> Self {
        Self {
            addr,
            pos,
            fingerprint,
            step: PendingEnterStep::Releasing(release),
            deadline: Box::pin(tokio::time::sleep(ENTER_HANDOFF_TIMEOUT)),
        }
    }

    fn step_name(&self) -> &'static str {
        match self.step {
            PendingEnterStep::Releasing(_) => "capture_release",
            PendingEnterStep::Centering(_) => "pointer_centering",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum PendingEnterProgress {
    Released(bool),
    Centered(bool),
    TimedOut,
}

#[derive(Debug, Eq, PartialEq)]
enum EnterDisposition {
    Start,
    WaitForPending,
    Acknowledge,
    RejectBusy,
}

fn enter_disposition(
    entered: &HashSet<SocketAddr>,
    pending: Option<&PendingEnter>,
    addr: SocketAddr,
) -> EnterDisposition {
    if entered.contains(&addr) {
        EnterDisposition::Acknowledge
    } else if pending.is_some_and(|pending| pending.addr == addr) {
        EnterDisposition::WaitForPending
    } else if pending.is_some() {
        EnterDisposition::RejectBusy
    } else {
        EnterDisposition::Start
    }
}

async fn wait_for_pending_enter(
    pending: &mut Option<PendingEnter>,
) -> PendingEnterProgress {
    let Some(pending) = pending.as_mut() else {
        return future::pending().await;
    };
    let PendingEnter { step, deadline, .. } = pending;
    match step {
        PendingEnterStep::Releasing(completion) => select! {
            result = completion => PendingEnterProgress::Released(result.unwrap_or(false)),
            _ = deadline.as_mut() => PendingEnterProgress::TimedOut,
        },
        PendingEnterStep::Centering(completion) => select! {
            result = completion => PendingEnterProgress::Centered(result.unwrap_or(false)),
            _ = deadline.as_mut() => PendingEnterProgress::TimedOut,
        },
    }
}

impl PendingReleases {
    fn request(&mut self, addr: SocketAddr, release_epoch: u64) {
        self.epochs.insert(addr, release_epoch);
    }

    fn retry_epoch(&self, addr: SocketAddr) -> Option<u64> {
        self.epochs.get(&addr).copied()
    }

    fn acknowledge(&mut self, addr: SocketAddr, release_epoch: u64) -> bool {
        if self.epochs.get(&addr) != Some(&release_epoch) {
            return false;
        }
        self.epochs.remove(&addr);
        true
    }

    fn disconnected(&mut self, addr: SocketAddr) {
        self.epochs.remove(&addr);
    }
}

impl ListenTask {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_response = HashMap::new();
        let mut rejected_connections = HashMap::new();
        let mut pending_releases = PendingReleases::default();
        let mut pending_enter = None;
        let mut entered_connections = HashSet::new();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::Enter(pos) => {
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    match enter_disposition(
                                        &entered_connections,
                                        pending_enter.as_ref(),
                                        addr,
                                    ) {
                                        EnterDisposition::Acknowledge => {
                                            self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                            continue;
                                        }
                                        EnterDisposition::WaitForPending => {
                                            log::debug!("ignoring duplicate enter from {addr} while handoff is pending");
                                            continue;
                                        }
                                        EnterDisposition::RejectBusy => {
                                            log::warn!("not acknowledging enter from {addr}: another handoff is pending");
                                            continue;
                                        }
                                        EnterDisposition::Start => {}
                                    }
                                    log::info!("releasing capture: {addr} entered this device");
                                    let (completion_tx, completion_rx) = oneshot::channel();
                                    self.event_tx.send(EmulationEvent::ReleaseCapture {
                                        completion: completion_tx,
                                    }).expect("channel closed");
                                    pending_enter = Some(PendingEnter::new(
                                        addr,
                                        pos,
                                        fingerprint,
                                        completion_rx,
                                    ));
                                }
                            }
                            ProtoEvent::Leave(_) => {
                                if pending_enter.as_ref().is_some_and(|pending| pending.addr == addr) {
                                    pending_enter = None;
                                }
                                entered_connections.remove(&addr);
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => self.emulation_proxy.consume(event, addr),
                            ProtoEvent::Ping => {
                                self.listener.reply(addr, ProtoEvent::Pong(self.emulation_proxy.control_ready())).await;
                                self.listener.reply(addr, self.emulation_proxy.readiness()).await;
                                if let Some(release_epoch) = pending_releases.retry_epoch(addr) {
                                    self.listener.reply(addr, ProtoEvent::ReleaseRequest { release_epoch }).await;
                                }
                            }
                            // Peer's version handshake. Echo our own
                            // commit back so the peer's connect-side
                            // receive_loop populates its `peer_commit`,
                            // AND publish a PeerHello upward so our
                            // service can populate ours from the listen
                            // side too — the connect side is the primary
                            // path, but if the outbound direction is
                            // broken (one-way setup, NAT, peer's TCP
                            // listener down) the version display would
                            // otherwise silently say "unknown" while
                            // the peer is in fact happily talking to us.
                            ProtoEvent::Hello { commit } => {
                                self.listener.reply(addr, ProtoEvent::Hello { commit: local_commit() }).await;
                                self.listener.reply(addr, self.emulation_proxy.readiness()).await;
                                self.event_tx.send(EmulationEvent::PeerHello { addr, commit }).expect("channel closed");
                            }
                            ProtoEvent::Readiness {
                                ..
                            } => {}
                            ProtoEvent::ReleaseAck { release_epoch } => {
                                if pending_releases.acknowledge(addr, release_epoch) {
                                    self.event_tx.send(EmulationEvent::ReleaseAcknowledged {
                                        addr,
                                        release_epoch,
                                    }).expect("channel closed");
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).expect("channel closed");
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).expect("channel closed");
                            }
                    }
                    Some(ListenEvent::Disconnected { addr }) => {
                        last_response.remove(&addr);
                        pending_releases.disconnected(addr);
                        if pending_enter.as_ref().is_some_and(|pending| pending.addr == addr) {
                            pending_enter = None;
                        }
                        entered_connections.remove(&addr);
                        self.emulation_proxy.remove(addr);
                        self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                    }
                    None => break
                }}
                progress = wait_for_pending_enter(&mut pending_enter) => {
                    match progress {
                        PendingEnterProgress::Released(true) => {
                            let pending = pending_enter.as_mut().expect("pending enter");
                            pending.step = PendingEnterStep::Centering(
                                self.emulation_proxy.request_center_pointer(pending.addr),
                            );
                        }
                        PendingEnterProgress::Centered(true) => {
                            let completed = pending_enter.take().expect("pending enter");
                            if self.listener.reply(completed.addr, ProtoEvent::Ack(0)).await {
                                entered_connections.insert(completed.addr);
                                self.event_tx.send(EmulationEvent::Entered {
                                    addr: completed.addr,
                                    pos: to_ipc_pos(completed.pos),
                                    fingerprint: completed.fingerprint,
                                }).expect("channel closed");
                            } else {
                                self.emulation_proxy.remove(completed.addr);
                                self.listener.disconnect(completed.addr).await;
                            }
                        }
                        PendingEnterProgress::Released(false) | PendingEnterProgress::Centered(false) => {
                            let failed = pending_enter.take().expect("pending enter");
                            log::warn!(
                                "not acknowledging enter from {}: {} failed",
                                failed.addr,
                                failed.step_name(),
                            );
                            self.emulation_proxy.remove(failed.addr);
                            self.listener.disconnect(failed.addr).await;
                        }
                        PendingEnterProgress::TimedOut => {
                            let failed = pending_enter.take().expect("pending enter");
                            log::warn!(
                                "not acknowledging enter from {}: {} timed out",
                                failed.addr,
                                failed.step_name(),
                            );
                            self.emulation_proxy.remove(failed.addr);
                            self.listener.disconnect(failed.addr).await;
                        }
                    }
                }
                event = self.emulation_proxy.event() => {
                    if matches!(&event, EmulationEvent::EmulationEnabled { .. } | EmulationEvent::EmulationDisabled { .. }) {
                        self.listener.broadcast(self.emulation_proxy.readiness()).await;
                    }
                    self.event_tx.send(event).expect("channel closed");
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release { addr, release_epoch } => {
                        pending_releases.request(addr, release_epoch);
                        self.listener.reply(addr, ProtoEvent::ReleaseRequest { release_epoch }).await;
                    }
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).expect("channel closed");
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    last_response.retain(|&addr,instant| {
                        if instant.elapsed() > Duration::from_secs(1) {
                            log::warn!("releasing keys: {addr} not responding!");
                            self.emulation_proxy.remove(addr);
                            self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                            pending_releases.disconnected(addr);
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    keyboard_ready: Rc<Cell<bool>>,
    pointer_ready: Rc<Cell<bool>>,
    session_epoch: Rc<Cell<u64>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    CenterPointer(SocketAddr, oneshot::Sender<bool>),
    Remove(SocketAddr),
    Terminate,
    Reenable,
}

impl EmulationProxy {
    fn new(backend: Option<input_emulation::Backend>, display_selector: Option<String>) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let keyboard_ready = Rc::new(Cell::new(false));
        let pointer_ready = Rc::new(Cell::new(false));
        let session_epoch = Rc::new(Cell::new(0));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            display_selector,
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            next_id: 0,
            next_session_epoch: (rand::random::<u64>() >> 1).max(1),
        };
        let task = spawn_local(emulation_task.run());
        Self {
            keyboard_ready,
            pointer_ready,
            session_epoch,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        match &event {
            EmulationEvent::EmulationEnabled {
                keyboard_ready,
                pointer_ready,
                session_epoch,
            } => {
                self.session_epoch.set(*session_epoch);
                self.keyboard_ready.set(*keyboard_ready);
                self.pointer_ready.set(*pointer_ready);
            }
            EmulationEvent::EmulationDisabled { session_epoch } => {
                self.session_epoch.set(*session_epoch);
                self.keyboard_ready.set(false);
                self.pointer_ready.set(false);
            }
            _ => {}
        }
        event
    }

    fn readiness(&self) -> ProtoEvent {
        ProtoEvent::Readiness {
            keyboard_ready: self.keyboard_ready.get(),
            pointer_ready: self.pointer_ready.get(),
            session_epoch: self.session_epoch.get(),
        }
    }

    fn control_ready(&self) -> bool {
        self.keyboard_ready.get() && self.pointer_ready.get()
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.control_ready() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .expect("channel closed");
        }
    }

    fn request_center_pointer(&self, addr: SocketAddr) -> oneshot::Receiver<bool> {
        let (result_tx, result_rx) = oneshot::channel();
        if !self.control_ready() {
            let _ = result_tx.send(false);
            return result_rx;
        }
        self.request_tx
            .send(ProxyRequest::CenterPointer(addr, result_tx))
            .expect("channel closed");
        result_rx
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .expect("channel closed");
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .expect("channel closed");
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    display_selector: Option<String>,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    next_id: EmulationHandle,
    next_session_epoch: u64,
}

impl EmulationTask {
    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_emulation().await {
                log::warn!("input emulation exited: {e}");
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await.expect("channel closed") {
                    ProxyRequest::Reenable => break,
                    ProxyRequest::Terminate => return,
                    ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::CenterPointer(_, result_tx) => {
                        let _ = result_tx.send(false);
                    }
                    ProxyRequest::Remove(..) => { /* emulation inactive => ignore */ }
                }
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = InputEmulation::new(self.backend, self.display_selector.clone()) => r?,
            // allow termination event while requesting input emulation
            _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
        };
        let (keyboard_ready, pointer_ready) = backend_readiness(emulation.backend());

        let enabled_epoch = self.next_session_epoch;
        let disabled_epoch = enabled_epoch
            .checked_add(1)
            .expect("emulation session epoch exhausted");
        self.next_session_epoch = disabled_epoch
            .checked_add(1)
            .expect("emulation session epoch exhausted");

        // used to send enabled and disabled events
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled {
                keyboard_ready,
                pointer_ready,
                session_epoch: enabled_epoch,
            },
            EmulationEvent::EmulationDisabled {
                session_epoch: disabled_epoch,
            },
        );

        // create active handles
        if let Err(e) = self.create_clients(&mut emulation).await {
            emulation.terminate().await;
            return Err(e);
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        for handle in self.handles.values() {
            tokio::select! {
                _ = emulation.create(*handle) => {},
                _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        loop {
            tokio::select! {
                error = std::future::poll_fn(|cx| emulation.poll_error(cx)) => {
                    return Err(error.into());
                }
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = self.handle_for_addr(emulation, addr).await;
                        emulation.consume(event, handle).await?;
                    },
                    ProxyRequest::CenterPointer(addr, result_tx) => {
                        let handle = self.handle_for_addr(emulation, addr).await;
                        match emulation.center_pointer(handle) {
                            Ok(()) => {
                                let _ = result_tx.send(true);
                            }
                            Err(error) => {
                                let _ = result_tx.send(false);
                                return Err(error.into());
                            }
                        }
                    }
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                },
            }
        }
    }

    async fn handle_for_addr(
        &mut self,
        emulation: &mut InputEmulation,
        addr: SocketAddr,
    ) -> EmulationHandle {
        match self.handles.get(&addr) {
            Some(&handle) => handle,
            None => {
                let handle = self.next_id;
                self.next_id += 1;
                emulation.create(handle).await;
                self.handles.insert(addr, handle);
                handle
            }
        }
    }
}

fn backend_readiness(backend: input_emulation::Backend) -> (bool, bool) {
    let ready = backend != input_emulation::Backend::Dummy;
    (ready, ready)
}

fn to_ipc_pos(pos: Position) -> lan_mouse_ipc::Position {
    match pos {
        Position::Left => lan_mouse_ipc::Position::Left,
        Position::Right => lan_mouse_ipc::Position::Right,
        Position::Top => lan_mouse_ipc::Position::Top,
        Position::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

async fn wait_for_termination(rx: &mut Receiver<ProxyRequest>) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::CenterPointer(_, result_tx) => {
                let _ = result_tx.send(false);
            }
            ProxyRequest::Remove(_) => continue,
            ProxyRequest::Reenable => continue,
        }
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> SocketAddr {
        "127.0.0.1:4242".parse().unwrap()
    }

    #[test]
    fn pending_release_retries_current_epoch_until_acknowledged() {
        let mut pending = PendingReleases::default();
        pending.request(peer(), 7);

        assert_eq!(pending.retry_epoch(peer()), Some(7));
        assert!(pending.acknowledge(peer(), 7));
        assert_eq!(pending.retry_epoch(peer()), None);
    }

    #[test]
    fn stale_ack_cannot_clear_newer_release_request() {
        let mut pending = PendingReleases::default();
        pending.request(peer(), 7);
        pending.request(peer(), 8);

        assert!(!pending.acknowledge(peer(), 7));
        assert_eq!(pending.retry_epoch(peer()), Some(8));
    }

    #[test]
    fn disconnect_removes_pending_release() {
        let mut pending = PendingReleases::default();
        pending.request(peer(), 7);

        pending.disconnected(peer());

        assert_eq!(pending.retry_epoch(peer()), None);
    }

    #[test]
    fn dummy_backend_never_reports_control_readiness() {
        assert_eq!(
            backend_readiness(input_emulation::Backend::Dummy),
            (false, false)
        );
    }

    #[tokio::test]
    async fn repeated_enter_waits_during_handoff_and_acknowledges_after_completion() {
        let addr = peer();
        let mut entered = HashSet::new();
        let (_release_tx, release_rx) = oneshot::channel();
        let pending = PendingEnter::new(
            addr,
            Position::Left,
            "fingerprint".to_string(),
            release_rx,
        );

        assert_eq!(
            enter_disposition(&entered, Some(&pending), addr),
            EnterDisposition::WaitForPending
        );
        entered.insert(addr);
        assert_eq!(
            enter_disposition(&entered, None, addr),
            EnterDisposition::Acknowledge
        );
    }

    #[tokio::test]
    async fn pending_enter_advances_release_and_centering_independently() {
        let (release_tx, release_rx) = oneshot::channel();
        let mut pending = Some(PendingEnter::new(
            peer(),
            Position::Left,
            "fingerprint".to_string(),
            release_rx,
        ));
        release_tx.send(true).unwrap();
        assert_eq!(
            wait_for_pending_enter(&mut pending).await,
            PendingEnterProgress::Released(true)
        );

        let (center_tx, center_rx) = oneshot::channel();
        pending.as_mut().unwrap().step = PendingEnterStep::Centering(center_rx);
        center_tx.send(true).unwrap();
        assert_eq!(
            wait_for_pending_enter(&mut pending).await,
            PendingEnterProgress::Centered(true)
        );
    }
}
