use crate::{
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    listen::{LanMouseListener, ListenerCreationError},
    switch::{
        BundleLeaseManager, GateContext, GrantIdentity, PeerBundleReadiness, PreparedGrant,
        SwitchClientError, SwitchController,
    },
};
use futures::StreamExt;
use lan_mouse_ipc::{
    AsyncFrontendListener, ClientHandle, FrontendEvent, FrontendRequest, IpcError,
    IpcListenerCreationError, Position, Status,
};
use log;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    signal,
    sync::{Notify, mpsc},
    task::{JoinHandle, spawn_local},
    time::Sleep,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
    #[error("failed to initialize switch controller: `{0}`")]
    SwitchController(String),
}

async fn wait_for_switch_deadline(deadline: &mut Option<Pin<Box<Sleep>>>) {
    match deadline {
        Some(deadline) => deadline.as_mut().await,
        None => futures::future::pending().await,
    }
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    next_trigger_handle: u64,
    bundle_lease: BundleLeaseManager,
    switch_controller: Option<SwitchController>,
    switch_deadline: Option<Pin<Box<Sleep>>>,
    switch_event_rx: mpsc::Receiver<SwitchTaskEvent>,
    switch_event_tx: mpsc::Sender<SwitchTaskEvent>,
    switch_task: Option<SwitchTask>,
    next_switch_task_epoch: u64,
    active_switch_capture: Option<(ClientHandle, u64)>,
    pending_switch_cleanup: Option<(GateContext, &'static str)>,
    next_release_epoch: u64,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

struct SwitchTask {
    epoch: u64,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

enum SwitchTaskEvent {
    Prepared {
        task_epoch: u64,
        context: GateContext,
        result: Result<PreparedGrant, SwitchClientError>,
    },
    Committed {
        task_epoch: u64,
        context: GateContext,
        grant: GrantIdentity,
        result: Result<u64, SwitchClientError>,
    },
    Renewed {
        task_epoch: u64,
        context: GateContext,
        result: Result<u64, SwitchClientError>,
    },
    CleanupFinished {
        task_epoch: u64,
        request_id: String,
        result: Result<(), SwitchClientError>,
    },
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let switch_controller = config
            .switch_controller()
            .map(|config| {
                SwitchController::new(config)
                    .map_err(|error| ServiceError::SwitchController(error.to_string()))
            })
            .transpose()?;
        let (switch_event_tx, switch_event_rx) = mpsc::channel(1);
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        // listener + connection
        let listener =
            LanMouseListener::new(config.port(), cert.clone(), authorized_keys.clone()).await?;
        let conn = LanMouseConnection::new(cert.clone(), client_manager.clone());

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(capture_backend, conn, config.release_bind());
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();
        let service = Self {
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            public_key_fingerprint,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
            bundle_lease: Default::default(),
            switch_controller,
            switch_deadline: None,
            switch_event_rx,
            switch_event_tx,
            switch_task: None,
            next_switch_task_epoch: 0,
            active_switch_capture: None,
            pending_switch_cleanup: None,
            next_release_epoch: 0,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        loop {
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                event = self.switch_event_rx.recv() => {
                    self.handle_switch_task_event(event.expect("switch event channel closed"));
                }
                _ = wait_for_switch_deadline(&mut self.switch_deadline) => {
                    self.handle_switch_deadline();
                }
                _ = self.config.changed() => self.handle_config_change(),
                r = signal::ctrl_c() => break r.expect("failed to wait for CTRL+C"),
            }
        }

        log::info!("terminating service ...");
        let switch_context = self.bundle_lease.invalidate();
        self.capture.release();
        self.cancel_switch_task();
        self.switch_deadline = None;
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        self.active_switch_capture = None;
        if let (Some(controller), Some(context)) = (&self.switch_controller, switch_context) {
            let cancellation = CancellationToken::new();
            if let Err(error) = controller
                .cancel(&context, "service_shutdown", &cancellation)
                .await
            {
                log::warn!("failed to notify controller during shutdown: {error}");
            }
        }
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;

        Ok(())
    }

    fn handle_frontend_request(&mut self, request: Option<Result<FrontendRequest, IpcError>>) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateSwitchTarget(handle, switch_target) => {
                self.update_switch_target(handle, switch_target);
                self.save_config();
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
        }
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                switch_target: c.switch_target,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    fn handle_config_change(&mut self) {
        self.fail_gate("config_changed");
        self.switch_controller = match self
            .config
            .switch_controller()
            .map(SwitchController::new)
            .transpose()
        {
            Ok(controller) => controller,
            Err(error) => {
                log::error!("failed to apply switch controller config: {error}");
                None
            }
        };
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.clear_peer_readiness(handle);
                    self.broadcast_client(handle);
                    self.handle_peer_readiness_change(handle);
                }
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled { .. } => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled { .. } => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => self.capture.release(),
            EmulationEvent::Connected { addr, fingerprint } => {
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    if self.client_manager.set_peer_commit(handle, Some(commit)) {
                        if !self.client_manager.peer_protocol_compatible(handle) {
                            log::warn!(
                                "peer {addr} build does not match; input bundle remains local"
                            );
                        }
                        self.broadcast_client(handle);
                        self.handle_peer_readiness_change(handle);
                    }
                }
            }
            EmulationEvent::PeerReadiness {
                addr,
                keyboard_ready,
                pointer_ready,
                session_epoch,
            } => {
                if let Some(handle) = self.client_manager.get_client(addr) {
                    if self.client_manager.set_peer_readiness(
                        handle,
                        keyboard_ready,
                        pointer_ready,
                        session_epoch,
                    ) {
                        self.broadcast_client(handle);
                        self.handle_peer_readiness_change(handle);
                    }
                }
            }
            EmulationEvent::ReleaseAcknowledged {
                addr,
                release_epoch,
            } => {
                log::info!("peer {addr} acknowledged capture release epoch {release_epoch}");
            }
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.next_release_epoch = self
                        .next_release_epoch
                        .checked_add(1)
                        .expect("release epoch exhausted");
                    self.emulation
                        .request_capture_release(incoming.addr, self.next_release_epoch);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
                self.active_switch_capture = None;
                if self.pending_switch_cleanup.is_some() {
                    self.complete_pending_switch_cleanup();
                } else {
                    self.release_gate_after_capture("capture_backend_disabled");
                }
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureCandidate(handle) => {
                self.handle_capture_candidate(handle);
            }
            ICaptureEvent::CommitRequested {
                handle,
                lease_epoch,
                peer_session_epoch,
                decision,
            } => {
                let authorized =
                    self.authorize_client_enter(handle, lease_epoch, peer_session_epoch);
                if authorized {
                    self.active_switch_capture = Some((handle, lease_epoch));
                }
                if decision.send(authorized).is_err() && authorized {
                    if let Some(context) = self.bundle_lease.context().cloned() {
                        self.fail_context(context, "capture_commit_decision_dropped");
                    }
                }
            }
            ICaptureEvent::CommitDeniedReleased {
                handle,
                lease_epoch,
            } => {
                if self.active_switch_capture == Some((handle, lease_epoch)) {
                    self.active_switch_capture = None;
                }
                if self.pending_switch_cleanup.is_some() {
                    self.complete_pending_switch_cleanup();
                } else if self.bundle_lease.context().is_some_and(|context| {
                    context.handle == handle && context.lease.lease_epoch == lease_epoch
                }) {
                    self.release_gate_after_capture("capture_commit_not_authorized");
                }
            }
            ICaptureEvent::ClientReleased(handle) => {
                log::info!("released client {handle} capture");
                self.active_switch_capture = None;
                if self.pending_switch_cleanup.is_some() {
                    self.complete_pending_switch_cleanup();
                } else {
                    self.release_gate_after_capture("capture_released");
                }
            }
            ICaptureEvent::PeerReadiness(handle) => {
                self.broadcast_client(handle);
                self.handle_peer_readiness_change(handle);
            }
        }
    }

    fn peer_readiness(&self, handle: ClientHandle) -> Option<PeerBundleReadiness> {
        self.client_manager.peer_input_readiness(handle).map(
            |(online, keyboard_ready, pointer_ready, session_epoch)| PeerBundleReadiness {
                online,
                keyboard_ready,
                pointer_ready,
                session_epoch,
            },
        )
    }

    fn handle_capture_candidate(&mut self, handle: ClientHandle) {
        let Some(target) = self.client_manager.switch_target(handle) else {
            log::warn!("capture candidate {handle} rejected: switch target is not configured");
            return;
        };
        if self
            .switch_controller
            .as_ref()
            .is_some_and(|controller| target == controller.server_host())
        {
            log::info!(
                "server-host capture candidate {handle} stays local; hub release drives fallback"
            );
            return;
        }
        let Some(controller) = self.switch_controller.clone() else {
            log::warn!("capture candidate {handle} rejected: switch controller is not configured");
            return;
        };
        if self.switch_task.is_some() {
            log::debug!("capture candidate {handle} rejected: controller operation is active");
            return;
        }
        let Some(readiness) = self.peer_readiness(handle) else {
            log::warn!("capture candidate {handle} rejected: client does not exist");
            return;
        };
        let bundle_ready = readiness.online
            && readiness.keyboard_ready
            && readiness.pointer_ready
            && readiness.session_epoch != 0;
        let request_id = format!("request-{:032x}", rand::random::<u128>());
        let lease_id = format!("lease-{:032x}", rand::random::<u128>());
        let context = match self.bundle_lease.reserve(
            handle,
            target,
            request_id,
            lease_id,
            readiness.session_epoch,
            bundle_ready,
            controller.now_ms(),
            controller.lease_ttl_ms(),
        ) {
            Ok(context) => context,
            Err(error) => {
                log::warn!("capture candidate {handle} rejected: {error}");
                return;
            }
        };
        self.reset_switch_deadline();
        if !self.start_prepare_task(context.clone(), readiness) {
            self.fail_context(context, "controller_busy");
        }
    }

    fn start_prepare_task(&mut self, context: GateContext, readiness: PeerBundleReadiness) -> bool {
        let Some(controller) = self.switch_controller.clone() else {
            return false;
        };
        let Some((task_epoch, cancellation)) = self.begin_switch_task() else {
            return false;
        };
        let task_cancellation = cancellation.clone();
        let event_tx = self.switch_event_tx.clone();
        let task_context = context.clone();
        let task = spawn_local(async move {
            let result = controller
                .prepare(&task_context, readiness, &task_cancellation)
                .await;
            let _ = event_tx
                .send(SwitchTaskEvent::Prepared {
                    task_epoch,
                    context: task_context,
                    result,
                })
                .await;
        });
        self.switch_task = Some(SwitchTask {
            epoch: task_epoch,
            cancellation,
            task,
        });
        log::info!(
            "preparing switch request {} lease {} epoch {} target {}",
            context.lease.request_id,
            context.lease.lease_id,
            context.lease.lease_epoch,
            context.target
        );
        true
    }

    fn start_commit_task(&mut self, context: GateContext, grant: GrantIdentity) -> bool {
        let Some(controller) = self.switch_controller.clone() else {
            return false;
        };
        let Some((task_epoch, cancellation)) = self.begin_switch_task() else {
            return false;
        };
        let task_cancellation = cancellation.clone();
        let event_tx = self.switch_event_tx.clone();
        let task_context = context.clone();
        let task_grant = grant.clone();
        let task = spawn_local(async move {
            let result = controller
                .commit(&task_context, &task_grant, &task_cancellation)
                .await;
            let _ = event_tx
                .send(SwitchTaskEvent::Committed {
                    task_epoch,
                    context: task_context,
                    grant: task_grant,
                    result,
                })
                .await;
        });
        self.switch_task = Some(SwitchTask {
            epoch: task_epoch,
            cancellation,
            task,
        });
        true
    }

    fn begin_switch_task(&mut self) -> Option<(u64, CancellationToken)> {
        if self.switch_task.is_some() {
            return None;
        }
        self.next_switch_task_epoch = self
            .next_switch_task_epoch
            .checked_add(1)
            .expect("switch task epoch exhausted");
        Some((self.next_switch_task_epoch, CancellationToken::new()))
    }

    fn start_renewal_task(&mut self, context: GateContext) -> bool {
        let Some(controller) = self.switch_controller.clone() else {
            return false;
        };
        let Some((task_epoch, cancellation)) = self.begin_switch_task() else {
            return false;
        };
        let task_cancellation = cancellation.clone();
        let event_tx = self.switch_event_tx.clone();
        let task_context = context.clone();
        let renew_interval = controller.renew_interval();
        let task = spawn_local(async move {
            loop {
                tokio::select! {
                    _ = task_cancellation.cancelled() => break,
                    _ = tokio::time::sleep(renew_interval) => {}
                }
                let result = controller.renew(&task_context, &task_cancellation).await;
                let failed = result.is_err();
                if event_tx
                    .send(SwitchTaskEvent::Renewed {
                        task_epoch,
                        context: task_context.clone(),
                        result,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                if failed {
                    break;
                }
            }
        });
        self.switch_task = Some(SwitchTask {
            epoch: task_epoch,
            cancellation,
            task,
        });
        true
    }

    fn start_cleanup_task(&mut self, context: GateContext, reason: &'static str) {
        let Some(controller) = self.switch_controller.clone() else {
            return;
        };
        let Some((task_epoch, cancellation)) = self.begin_switch_task() else {
            return;
        };
        let readiness = self.peer_readiness(context.handle);
        let task_cancellation = cancellation.clone();
        let event_tx = self.switch_event_tx.clone();
        let request_id = context.lease.request_id.clone();
        let task = spawn_local(async move {
            let readiness_result = if let Some(readiness) = readiness {
                controller
                    .publish_readiness(context.target, readiness, &task_cancellation)
                    .await
            } else {
                Ok(())
            };
            let cancel_result = controller
                .cancel(&context, reason, &task_cancellation)
                .await;
            let result = cancel_result.and(readiness_result);
            let _ = event_tx
                .send(SwitchTaskEvent::CleanupFinished {
                    task_epoch,
                    request_id,
                    result,
                })
                .await;
        });
        self.switch_task = Some(SwitchTask {
            epoch: task_epoch,
            cancellation,
            task,
        });
    }

    fn handle_switch_task_event(&mut self, event: SwitchTaskEvent) {
        match event {
            SwitchTaskEvent::Prepared {
                task_epoch,
                context,
                result,
            } => {
                if !self.finish_switch_task(task_epoch) {
                    return;
                }
                self.handle_prepared(context, result);
            }
            SwitchTaskEvent::Committed {
                task_epoch,
                context,
                grant,
                result,
            } => {
                if !self.finish_switch_task(task_epoch) {
                    return;
                }
                self.handle_committed(context, grant, result);
            }
            SwitchTaskEvent::Renewed {
                task_epoch,
                context,
                result,
            } => {
                if self.switch_task.as_ref().map(|task| task.epoch) != Some(task_epoch) {
                    return;
                }
                self.handle_renewed(task_epoch, context, result);
            }
            SwitchTaskEvent::CleanupFinished {
                task_epoch,
                request_id,
                result,
            } => {
                if !self.finish_switch_task(task_epoch) {
                    return;
                }
                match result {
                    Ok(()) => log::info!("controller cleanup completed for {request_id}"),
                    Err(error) => {
                        log::warn!("controller cleanup failed for {request_id}: {error}")
                    }
                }
            }
        }
    }

    fn handle_prepared(
        &mut self,
        context: GateContext,
        result: Result<PreparedGrant, SwitchClientError>,
    ) {
        if !self
            .bundle_lease
            .context()
            .is_some_and(|current| current.same_identity(&context))
        {
            return;
        }
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                log::warn!(
                    "switch preparation failed for {}: {error}",
                    context.lease.request_id
                );
                self.fail_gate("prepare_failed");
                return;
            }
        };
        let Some(readiness) = self.peer_readiness(context.handle) else {
            self.fail_context(context, "peer_missing_before_grant");
            return;
        };
        let bundle_ready = readiness.online && readiness.keyboard_ready && readiness.pointer_ready;
        let (context, grant) = match self.bundle_lease.arm_grant(
            &context,
            prepared.request_epoch,
            prepared.grant_epoch,
            prepared.lease_expires_at_ms,
            prepared.grant_expires_at_ms,
            bundle_ready,
            readiness.session_epoch,
            self.controller_now_ms(),
        ) {
            Ok(armed) => armed,
            Err(error) => {
                log::warn!("grant rejected locally: {error}");
                self.fail_context(context, "grant_rejected_locally");
                return;
            }
        };
        self.reset_switch_deadline();
        let Some(controller) = self.switch_controller.as_ref() else {
            self.fail_context(context, "controller_missing_after_grant");
            return;
        };
        if context.target == controller.server_host() {
            if !self.start_commit_task(context.clone(), grant) {
                self.fail_context(context, "commit_task_busy");
            }
            return;
        }
        let now_ms = controller.now_ms();
        let Some(deadline_ms) = self.bundle_lease.deadline_ms() else {
            self.fail_context(context, "grant_deadline_missing");
            return;
        };
        let Some(valid_for_ms) = deadline_ms.checked_sub(now_ms) else {
            self.fail_context(context, "grant_expired_before_arm");
            return;
        };
        if valid_for_ms == 0 {
            self.fail_context(context, "grant_expired_before_arm");
            return;
        }
        self.capture.arm(
            context.handle,
            context.lease.lease_epoch,
            context.lease.peer_session_epoch,
            Duration::from_millis(valid_for_ms),
        );
        log::info!(
            "armed switch request {} lease epoch {} for second crossing",
            context.lease.request_id,
            context.lease.lease_epoch
        );
    }

    fn authorize_client_enter(
        &mut self,
        handle: ClientHandle,
        lease_epoch: u64,
        permit_peer_session_epoch: u64,
    ) -> bool {
        let Some(context) = self.bundle_lease.context().cloned() else {
            return false;
        };
        if context.handle != handle
            || context.lease.lease_epoch != lease_epoch
            || context.lease.peer_session_epoch != permit_peer_session_epoch
        {
            self.defer_failure_until_capture_release(context, "capture_permit_stale");
            return false;
        };
        let Some(readiness) = self.peer_readiness(handle) else {
            self.defer_failure_until_capture_release(context, "peer_missing_before_commit");
            return false;
        };
        let bundle_ready = readiness.online && readiness.keyboard_ready && readiness.pointer_ready;
        match self.bundle_lease.commit(
            handle,
            lease_epoch,
            bundle_ready,
            readiness.session_epoch,
            self.controller_now_ms(),
        ) {
            Ok((context, grant)) => {
                self.reset_switch_deadline();
                if !self.start_commit_task(context.clone(), grant) {
                    self.defer_failure_until_capture_release(context, "commit_task_busy");
                    return false;
                }
                true
            }
            Err(error) => {
                log::warn!("capture commit rejected locally: {error}");
                self.defer_failure_until_capture_release(context, "capture_commit_rejected");
                false
            }
        }
    }

    fn handle_committed(
        &mut self,
        context: GateContext,
        _grant: GrantIdentity,
        result: Result<u64, SwitchClientError>,
    ) {
        if !self
            .bundle_lease
            .context()
            .is_some_and(|current| current.same_identity(&context))
        {
            return;
        }
        let renewed_until_ms = match result {
            Ok(deadline) => deadline,
            Err(error) => {
                log::warn!(
                    "controller commit failed for {}: {error}",
                    context.lease.request_id
                );
                self.fail_gate("controller_commit_failed");
                return;
            }
        };
        let Some(controller) = self.switch_controller.as_ref() else {
            self.fail_context(context, "controller_missing_after_commit");
            return;
        };
        if context.target == controller.server_host() {
            self.bundle_lease.invalidate();
            self.switch_deadline = None;
            log::info!(
                "server-host switch {} committed; input remains local",
                context.lease.request_id
            );
            return;
        }
        let Some(readiness) = self.peer_readiness(context.handle) else {
            self.fail_context(context, "peer_missing_after_commit");
            return;
        };
        let bundle_ready = readiness.online && readiness.keyboard_ready && readiness.pointer_ready;
        if let Err(error) = self.bundle_lease.renew(
            &context.lease.request_id,
            renewed_until_ms,
            bundle_ready,
            readiness.session_epoch,
            controller.now_ms(),
        ) {
            log::warn!("committed lease acknowledgement rejected locally: {error}");
            self.fail_context(context, "commit_ack_rejected");
            return;
        }
        self.reset_switch_deadline();
        let context = self
            .bundle_lease
            .context()
            .cloned()
            .expect("renewed lease has context");
        if !self.start_renewal_task(context.clone()) {
            self.fail_context(context, "renewal_task_busy");
        }
    }

    fn handle_renewed(
        &mut self,
        task_epoch: u64,
        context: GateContext,
        result: Result<u64, SwitchClientError>,
    ) {
        if !self
            .bundle_lease
            .context()
            .is_some_and(|current| current.same_identity(&context))
        {
            self.cancel_switch_task();
            return;
        }
        let renewed_until_ms = match result {
            Ok(deadline) => deadline,
            Err(error) => {
                log::warn!(
                    "lease renewal failed for {}: {error}",
                    context.lease.request_id
                );
                self.finish_switch_task(task_epoch);
                self.fail_gate("lease_renewal_failed");
                return;
            }
        };
        let Some(readiness) = self.peer_readiness(context.handle) else {
            self.finish_switch_task(task_epoch);
            self.fail_context(context, "peer_missing_during_renewal");
            return;
        };
        let bundle_ready = readiness.online && readiness.keyboard_ready && readiness.pointer_ready;
        if let Err(error) = self.bundle_lease.renew(
            &context.lease.request_id,
            renewed_until_ms,
            bundle_ready,
            readiness.session_epoch,
            self.controller_now_ms(),
        ) {
            log::warn!("lease renewal acknowledgement rejected locally: {error}");
            self.finish_switch_task(task_epoch);
            self.fail_context(context, "renewal_ack_rejected");
            return;
        }
        self.reset_switch_deadline();
    }

    fn handle_peer_readiness_change(&mut self, handle: ClientHandle) {
        let Some(context) = self
            .bundle_lease
            .context()
            .filter(|context| context.handle == handle)
            .cloned()
        else {
            return;
        };
        let Some(readiness) = self.peer_readiness(handle) else {
            self.fail_context(context, "peer_removed");
            return;
        };
        if !readiness.online
            || !readiness.keyboard_ready
            || !readiness.pointer_ready
            || readiness.session_epoch != context.lease.peer_session_epoch
        {
            self.fail_context(context, "peer_readiness_lost");
        }
    }

    fn controller_now_ms(&self) -> u64 {
        self.switch_controller
            .as_ref()
            .map(SwitchController::now_ms)
            .unwrap_or(u64::MAX)
    }

    fn reset_switch_deadline(&mut self) {
        let Some(deadline_ms) = self.bundle_lease.deadline_ms() else {
            self.switch_deadline = None;
            return;
        };
        let now_ms = self.controller_now_ms();
        self.switch_deadline = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
            deadline_ms.saturating_sub(now_ms),
        ))));
    }

    fn handle_switch_deadline(&mut self) {
        self.switch_deadline = None;
        let now_ms = self.controller_now_ms();
        if let Some(context) = self.bundle_lease.expire(now_ms) {
            self.fail_context(context, "local_lease_expired");
        } else {
            self.reset_switch_deadline();
        }
    }

    fn fail_gate(&mut self, reason: &'static str) {
        if let Some(context) = self.bundle_lease.invalidate() {
            self.fail_context(context, reason);
        }
    }

    fn fail_context(&mut self, context: GateContext, reason: &'static str) {
        if self
            .bundle_lease
            .context()
            .is_some_and(|current| current.same_identity(&context))
        {
            self.bundle_lease.invalidate();
        }
        self.capture.disarm(context.lease.lease_epoch);
        self.cancel_switch_task();
        self.switch_deadline = None;
        log::warn!(
            "switch request {} failed closed: {reason}",
            context.lease.request_id
        );
        if self.active_switch_capture == Some((context.handle, context.lease.lease_epoch)) {
            self.pending_switch_cleanup = Some((context, reason));
            self.capture.release();
        } else {
            self.start_cleanup_task(context, reason);
        }
    }

    fn defer_failure_until_capture_release(&mut self, context: GateContext, reason: &'static str) {
        if self
            .bundle_lease
            .context()
            .is_some_and(|current| current.same_identity(&context))
        {
            self.bundle_lease.invalidate();
        }
        self.capture.disarm(context.lease.lease_epoch);
        self.cancel_switch_task();
        self.switch_deadline = None;
        self.pending_switch_cleanup = Some((context.clone(), reason));
        log::warn!(
            "switch request {} failed closed before capture authorization: {reason}",
            context.lease.request_id
        );
    }

    fn release_gate_after_capture(&mut self, reason: &'static str) {
        let Some(context) = self.bundle_lease.invalidate() else {
            return;
        };
        self.cancel_switch_task();
        self.switch_deadline = None;
        self.start_cleanup_task(context, reason);
    }

    fn complete_pending_switch_cleanup(&mut self) {
        if let Some((context, reason)) = self.pending_switch_cleanup.take() {
            self.start_cleanup_task(context, reason);
        }
    }

    fn finish_switch_task(&mut self, task_epoch: u64) -> bool {
        if self.switch_task.as_ref().map(|task| task.epoch) != Some(task_epoch) {
            return false;
        }
        self.switch_task.take();
        true
    }

    fn cancel_switch_task(&mut self) {
        if let Some(task) = self.switch_task.take() {
            task.cancellation.cancel();
            task.task.abort();
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture.create(handle, pos, CaptureType::EnterOnly);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_switch_target(
        &mut self,
        handle: ClientHandle,
        switch_target: Option<lan_mouse_ipc::SwitchHost>,
    ) {
        self.client_manager.set_switch_target(handle, switch_target);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }
}
