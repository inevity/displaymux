#[cfg(target_os = "linux")]
use std::thread;
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, RwLock},
    time::Duration,
};

use lan_mouse_clipboard::{
    ActorCommand, ActorEvent, ActorHandle, ActorPayload, ApplyResult, AuthoritySessionId,
    AuthorityState, ClipboardPayload, ClipboardReason, Coordinator, CoordinatorCommand,
    CoordinatorError, HandoffEnvelope, HandoffId, HostId, NativeGeneration, NegotiatedPeer,
    OperationResult, OwnershipEpoch, OwnershipToken, PrepareResult, ProcessSessionId, SnapshotId,
    SnapshotMetadata, SpawnedActor, StagedSnapshot, TlsIdentity, TransportHandle, WireMessage,
};
#[cfg(target_os = "linux")]
use lan_mouse_clipboard::{LinuxClipboardBackend, spawn_actor};
use lan_mouse_ipc::SwitchHost;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    clipboard_transport::{
        ClipboardTransport, ClipboardTransportConfig, ClipboardTransportEvent,
        ClipboardTransportRole, configured_peers, host_id, text_v1_capability,
    },
    config::{ClipboardConfig, ConfigClient},
};

// The service loop can have one pending notice from each transition class:
// begin, provisional capture, activation, targeted cancel, global cancel, and
// shutdown.
const SERVICE_HOOK_CLASS_COUNT: usize = 6;
const ACTOR_COMPLETION_CLASS_COUNT: usize = 10;
// Native session endpoints can disappear during compositor or login-session replacement. A fixed
// two-second retry bounds idle probe load to 0.5 Hz without exposing another timing knob.
const NATIVE_ACTOR_RETRY_DELAY: Duration = Duration::from_secs(2);
type NativeActorRetry = Pin<Box<tokio::time::Sleep>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClipboardTransitionId {
    Outgoing { lease_epoch: u64 },
    Fallback { release_epoch: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ServiceHook {
    Begin {
        transition: ClipboardTransitionId,
        target: HostId,
    },
    CaptureProvisional,
    Activate {
        transition: ClipboardTransitionId,
    },
    Cancel {
        transition: ClipboardTransitionId,
        reason: ClipboardReason,
    },
    CancelCurrent {
        reason: ClipboardReason,
    },
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct ClipboardHandle {
    hook_tx: mpsc::Sender<ServiceHook>,
}

impl ClipboardHandle {
    pub(crate) fn begin_remote(&self, lease_epoch: u64, target: SwitchHost) -> bool {
        self.try_send(ServiceHook::Begin {
            transition: ClipboardTransitionId::Outgoing { lease_epoch },
            target: host_id(target),
        })
    }

    pub(crate) fn capture_provisional(&self) -> bool {
        self.try_send(ServiceHook::CaptureProvisional)
    }

    pub(crate) fn begin_fallback(&self, release_epoch: u64, server_host: SwitchHost) -> bool {
        self.try_send(ServiceHook::Begin {
            transition: ClipboardTransitionId::Fallback { release_epoch },
            target: host_id(server_host),
        })
    }

    pub(crate) fn activate(&self, transition: ClipboardTransitionId) -> bool {
        self.try_send(ServiceHook::Activate { transition })
    }

    pub(crate) fn cancel(&self, transition: ClipboardTransitionId, reason: ClipboardReason) {
        let _ = self.try_send(ServiceHook::Cancel { transition, reason });
    }

    pub(crate) fn cancel_current(&self, reason: ClipboardReason) {
        let _ = self.try_send(ServiceHook::CancelCurrent { reason });
    }

    fn try_send(&self, hook: ServiceHook) -> bool {
        match self.hook_tx.try_send(hook) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!(
                    event = "clipboard_handoff_skipped",
                    reason = ClipboardReason::QueueFull.code(),
                    "clipboard service hook queue is full"
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

pub(crate) struct ClipboardRuntime {
    handle: ClipboardHandle,
    task: Option<JoinHandle<()>>,
}

impl ClipboardRuntime {
    pub(crate) fn start(
        config: ClipboardConfig,
        local_host: Option<SwitchHost>,
        server_host: Option<SwitchHost>,
        port: u16,
        identity: Option<TlsIdentity>,
        clients: Vec<ConfigClient>,
        authorized_fingerprints: HashMap<String, String>,
    ) -> Self {
        let (hook_tx, hook_rx) = mpsc::channel(SERVICE_HOOK_CLASS_COUNT);
        let handle = ClipboardHandle { hook_tx };
        let process_session_id = ProcessSessionId::new(rand::random());
        let authority_session_id = AuthoritySessionId::new(rand::random());
        let authority_session = Arc::new(RwLock::new(None));
        let mut core = match (local_host, server_host) {
            (Some(local), Some(server)) if local == server => RuntimeCore::authority(
                config,
                host_id(server),
                process_session_id,
                authority_session_id,
            ),
            (Some(local), Some(server)) => RuntimeCore::peer(
                config,
                host_id(local),
                host_id(server),
                process_session_id,
                authority_session.clone(),
            ),
            _ => RuntimeCore::unavailable(process_session_id),
        };
        let (actor_completion_tx, actor_completion_rx) =
            mpsc::channel(ACTOR_COMPLETION_CLASS_COUNT);
        core.actor_completion_tx = Some(actor_completion_tx);
        let actor_init = core.start_native_actor();
        let transport_config = match (identity, local_host, server_host) {
            (Some(identity), Some(local), Some(server)) => {
                let (peers, authorized_peers) = configured_peers(clients, &authorized_fingerprints);
                let role = if local == server {
                    ClipboardTransportRole::Authority {
                        authority_session_id,
                    }
                } else {
                    ClipboardTransportRole::Peer {
                        authority_session_id: authority_session,
                    }
                };
                Some(ClipboardTransportConfig {
                    enabled: config.enabled,
                    local_host: host_id(local),
                    process_session_id,
                    max_bytes: config.max_bytes,
                    capabilities: text_v1_capability(false),
                    port,
                    identity,
                    peers,
                    authorized_peers,
                    role,
                })
            }
            _ => None,
        };
        let task = tokio::task::spawn_local(run(
            core,
            hook_rx,
            transport_config,
            actor_init,
            actor_completion_rx,
        ));
        Self {
            handle,
            task: Some(task),
        }
    }

    pub(crate) fn handle(&self) -> &ClipboardHandle {
        &self.handle
    }

    pub(crate) fn shutdown(&mut self) {
        let _ = self.handle.try_send(ServiceHook::Shutdown);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for ClipboardRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn run(
    mut core: RuntimeCore,
    mut hook_rx: mpsc::Receiver<ServiceHook>,
    mut transport_config: Option<ClipboardTransportConfig>,
    mut actor_init: Option<oneshot::Receiver<Result<SpawnedActor, ClipboardReason>>>,
    mut actor_completion_rx: mpsc::Receiver<(ActorCommand, ActorEvent)>,
) {
    let mut transport = transport_config.clone().map(ClipboardTransport::start);
    let mut actor = None;
    let mut actor_retry = None;
    loop {
        tokio::select! {
            hook = hook_rx.recv() => {
                let Some(hook) = hook else { break };
                let shutdown = matches!(hook, ServiceHook::Shutdown);
                core.handle(hook);
                if shutdown {
                    break;
                }
            }
            event = wait_for_transport_event(&mut transport) => {
                match event {
                    Some(event) => core.handle_transport_event(event),
                    None => transport = None,
                }
            }
            initialized = wait_for_actor_init(&mut actor_init) => {
                actor_init = None;
                match initialized {
                    Ok(spawned) => {
                        core.install_actor(spawned.handle.clone());
                        actor = Some(spawned);
                        if let Some(config) = transport_config.as_mut() {
                            config.capabilities = text_v1_capability(true);
                            if let Some(transport) = transport.as_mut() {
                                transport.shutdown();
                            }
                            transport = Some(ClipboardTransport::start(config.clone()));
                        }
                    }
                    Err(reason) => tracing::warn!(
                        event = "clipboard_backend_unavailable",
                        reason = reason.code(),
                        "native clipboard actor initialization failed; input remains available"
                    ),
                }
                if actor.is_none() {
                    actor_retry = Some(Box::pin(tokio::time::sleep(NATIVE_ACTOR_RETRY_DELAY)));
                }
            }
            payload = wait_for_actor_payload(&mut actor) => {
                match payload {
                    Some(payload) => core.handle_actor_payload(payload),
                    None => {
                        actor = None;
                        core.uninstall_actor();
                        if let Some(config) = transport_config.as_mut() {
                            config.capabilities = text_v1_capability(false);
                            if let Some(transport) = transport.as_mut() {
                                transport.shutdown();
                            }
                            transport = Some(ClipboardTransport::start(config.clone()));
                        }
                        actor_retry = Some(Box::pin(tokio::time::sleep(NATIVE_ACTOR_RETRY_DELAY)));
                    }
                }
            }
            () = wait_for_actor_retry(&mut actor_retry) => {
                actor_retry = None;
                actor_init = core.start_native_actor();
            }
            completion = actor_completion_rx.recv() => {
                if let Some((command, event)) = completion {
                    core.handle_actor_event(command, event);
                }
            }
        }
    }
    if let Some(mut transport) = transport {
        transport.shutdown();
    }
}

#[cfg(target_os = "linux")]
fn initialize_native_actor(
    process_session_id: ProcessSessionId,
    local_host: HostId,
    initial_token: OwnershipToken,
) -> oneshot::Receiver<Result<SpawnedActor, ClipboardReason>> {
    let (completion, receiver) = oneshot::channel();
    let _ = thread::Builder::new()
        .name("lan-mouse-clipboard-init".to_string())
        .spawn(move || {
            let actor = LinuxClipboardBackend::connect().and_then(|backend| {
                let backend_name = backend.name();
                let spawned = spawn_actor(
                    "lan-mouse-clipboard",
                    backend,
                    process_session_id,
                    local_host,
                    initial_token,
                )
                .map_err(|_| ClipboardReason::BackendUnavailable)?;
                tracing::info!(event = "clipboard_backend_ready", backend = backend_name);
                Ok(spawned)
            });
            let _ = completion.send(actor);
        });
    receiver
}

#[cfg(not(target_os = "linux"))]
fn initialize_native_actor(
    _process_session_id: ProcessSessionId,
    _local_host: HostId,
    _initial_token: OwnershipToken,
) -> oneshot::Receiver<Result<SpawnedActor, ClipboardReason>> {
    let (completion, receiver) = oneshot::channel();
    let _ = completion.send(Err(ClipboardReason::BackendUnavailable));
    receiver
}

async fn wait_for_transport_event(
    transport: &mut Option<ClipboardTransport>,
) -> Option<ClipboardTransportEvent> {
    match transport {
        Some(transport) => transport.event().await,
        None => futures::future::pending().await,
    }
}

async fn wait_for_actor_init(
    actor_init: &mut Option<oneshot::Receiver<Result<SpawnedActor, ClipboardReason>>>,
) -> Result<SpawnedActor, ClipboardReason> {
    match actor_init {
        Some(receiver) => receiver
            .await
            .unwrap_or(Err(ClipboardReason::BackendUnavailable)),
        None => futures::future::pending().await,
    }
}

async fn wait_for_actor_payload(actor: &mut Option<SpawnedActor>) -> Option<ActorPayload> {
    match actor {
        Some(actor) => actor.payload_rx.recv().await,
        None => futures::future::pending().await,
    }
}

async fn wait_for_actor_retry(retry: &mut Option<NativeActorRetry>) {
    match retry {
        Some(retry) => retry.as_mut().await,
        None => futures::future::pending().await,
    }
}

#[derive(Debug)]
struct PendingTransition {
    id: ClipboardTransitionId,
    handoff_id: HandoffId,
    target_token: OwnershipToken,
    target_process_session_id: ProcessSessionId,
    activated: bool,
}

#[derive(Debug)]
enum RuntimeRole {
    Authority(Coordinator),
    Peer {
        local_host: HostId,
        server_host: HostId,
        authority_state: Option<AuthorityState>,
        preparation: Option<(HandoffId, OwnershipToken, NativeGeneration)>,
    },
    Unavailable,
}

struct RuntimeCore {
    local_host: Option<HostId>,
    process_session_id: ProcessSessionId,
    actor_enabled: bool,
    max_bytes: usize,
    role: RuntimeRole,
    pending: Option<PendingTransition>,
    actor: Option<ActorHandle>,
    actor_completion_tx: Option<mpsc::Sender<(ActorCommand, ActorEvent)>>,
    pending_payload: Option<ClipboardPayload>,
    #[cfg(test)]
    effects: Vec<CoordinatorCommand>,
    network_peers: HashMap<HostId, (NegotiatedPeer, TransportHandle)>,
}

impl RuntimeCore {
    fn authority(
        config: ClipboardConfig,
        server_host: HostId,
        process_session_id: ProcessSessionId,
        authority_session_id: AuthoritySessionId,
    ) -> Self {
        Self {
            local_host: Some(server_host.clone()),
            process_session_id,
            actor_enabled: config.enabled,
            max_bytes: config.max_bytes,
            role: RuntimeRole::Authority(Coordinator::new(
                config.enabled,
                authority_session_id,
                server_host,
                process_session_id,
                config.max_bytes,
            )),
            pending: None,
            actor: None,
            actor_completion_tx: None,
            pending_payload: None,
            #[cfg(test)]
            effects: Vec::new(),
            network_peers: HashMap::new(),
        }
    }

    fn peer(
        config: ClipboardConfig,
        local_host: HostId,
        server_host: HostId,
        process_session_id: ProcessSessionId,
        _authority_session_id: Arc<RwLock<Option<AuthoritySessionId>>>,
    ) -> Self {
        Self {
            local_host: Some(local_host.clone()),
            process_session_id,
            actor_enabled: config.enabled,
            max_bytes: config.max_bytes,
            role: RuntimeRole::Peer {
                local_host,
                server_host,
                authority_state: None,
                preparation: None,
            },
            pending: None,
            actor: None,
            actor_completion_tx: None,
            pending_payload: None,
            #[cfg(test)]
            effects: Vec::new(),
            network_peers: HashMap::new(),
        }
    }

    fn unavailable(process_session_id: ProcessSessionId) -> Self {
        Self {
            local_host: None,
            process_session_id,
            actor_enabled: false,
            max_bytes: 0,
            role: RuntimeRole::Unavailable,
            pending: None,
            actor: None,
            actor_completion_tx: None,
            pending_payload: None,
            #[cfg(test)]
            effects: Vec::new(),
            network_peers: HashMap::new(),
        }
    }

    fn actor_initial_token(&self) -> Option<OwnershipToken> {
        match &self.role {
            RuntimeRole::Authority(coordinator) => Some(coordinator.current_token().clone()),
            RuntimeRole::Peer {
                server_host,
                authority_state,
                ..
            } => authority_state
                .as_ref()
                .map(|state| state.current_token.clone())
                .or_else(|| {
                    Some(OwnershipToken {
                        authority_session_id: AuthoritySessionId::new(0),
                        ownership_epoch: OwnershipEpoch::new(0),
                        owner_host_id: server_host.clone(),
                    })
                }),
            RuntimeRole::Unavailable => None,
        }
    }

    fn start_native_actor(
        &self,
    ) -> Option<oneshot::Receiver<Result<SpawnedActor, ClipboardReason>>> {
        if !self.actor_enabled {
            return None;
        }
        let initial_token = self.actor_initial_token()?;
        let local_host = self.local_host.clone()?;
        Some(initialize_native_actor(
            self.process_session_id,
            local_host,
            initial_token,
        ))
    }

    fn install_actor(&mut self, actor: ActorHandle) {
        self.actor = Some(actor);
    }

    fn uninstall_actor(&mut self) {
        self.actor = None;
        self.pending_payload = None;
        match &mut self.role {
            RuntimeRole::Authority(coordinator) => {
                if let Some(handoff_id) = coordinator.active().map(|handoff| handoff.id) {
                    let _ = coordinator.skip(handoff_id, ClipboardReason::BackendUnavailable);
                }
            }
            RuntimeRole::Peer { preparation, .. } => *preparation = None,
            RuntimeRole::Unavailable => {}
        }
    }

    fn queue_actor(&self, command: ActorCommand) -> Result<(), ClipboardReason> {
        let Some(actor) = self.actor.as_ref() else {
            #[cfg(test)]
            return Ok(());
            #[cfg(not(test))]
            return Err(ClipboardReason::BackendUnavailable);
        };
        let completion = actor.try_request(command.clone())?;
        let completion_tx = self
            .actor_completion_tx
            .as_ref()
            .ok_or(ClipboardReason::ChannelUnavailable)?
            .clone();
        tokio::task::spawn_local(async move {
            if let Ok(event) = completion.await {
                if completion_tx.try_send((command, event)).is_err() {
                    tracing::debug!(
                        event = "clipboard_handoff_skipped",
                        reason = ClipboardReason::QueueFull.code(),
                        "clipboard actor completion queue is unavailable"
                    );
                }
            }
        });
        Ok(())
    }

    fn handle(&mut self, hook: ServiceHook) {
        match hook {
            ServiceHook::Begin { transition, target } => self.begin(transition, target),
            ServiceHook::Activate { transition } => self.activate(transition),
            ServiceHook::Cancel { transition, reason } => self.cancel(transition, reason),
            ServiceHook::CancelCurrent { reason } => self.cancel_current(reason),
            ServiceHook::CaptureProvisional => {
                let source_token = match &self.role {
                    RuntimeRole::Peer {
                        local_host,
                        server_host,
                        authority_state: Some(state),
                        ..
                    } if state.current_token.owner_host_id == *local_host => {
                        tracing::debug!(
                            event = "clipboard_provisional_capture_requested",
                            local_host = %local_host,
                            server_host = %server_host,
                            process_session = self.process_session_id.get(),
                            "clipboard provisional capture notice accepted"
                        );
                        Some(state.current_token.clone())
                    }
                    _ => None,
                };
                if let Some(source_token) = source_token {
                    let _ = self.queue_actor(ActorCommand::CaptureProvisional {
                        source_token,
                        max_bytes: self.max_bytes,
                    });
                } else if let RuntimeRole::Peer {
                    local_host,
                    server_host,
                    ..
                } = &self.role
                {
                    tracing::debug!(
                        event = "clipboard_handoff_skipped",
                        local_host = %local_host,
                        server_host = %server_host,
                        reason = ClipboardReason::StaleOwnerToken.code(),
                        "clipboard provisional capture has no current local owner token"
                    );
                }
            }
            ServiceHook::Shutdown => self.cancel_current(ClipboardReason::Canceled),
        }
    }

    fn handle_transport_event(&mut self, event: ClipboardTransportEvent) {
        match event {
            ClipboardTransportEvent::Connected { peer, outbound } => {
                if peer.effective_max_bytes.is_some() {
                    self.peer_session_changed(peer.host_id.clone(), peer.process_session_id);
                }
                self.network_peers
                    .insert(peer.host_id.clone(), (peer.clone(), outbound));
                self.publish_authority_state(&peer.host_id);
            }
            ClipboardTransportEvent::Disconnected {
                host_id,
                process_session_id,
                connection_id,
            } => {
                let current = self.network_peers.get(&host_id).is_some_and(|(peer, _)| {
                    peer.connection_id == connection_id
                        && peer.process_session_id == process_session_id
                });
                if !current {
                    return;
                }
                self.network_peers.remove(&host_id);
                let mut actor_cancel = None;
                let commands = match &mut self.role {
                    RuntimeRole::Authority(coordinator) => {
                        coordinator.remove_process_session(&host_id)
                    }
                    RuntimeRole::Peer {
                        server_host,
                        authority_state,
                        preparation,
                        ..
                    } if *server_host == host_id => {
                        actor_cancel = authority_state
                            .as_ref()
                            .and_then(|state| state.active_handoff.as_ref())
                            .map(|handoff| handoff.handoff_id);
                        *authority_state = None;
                        *preparation = None;
                        Vec::new()
                    }
                    _ => Vec::new(),
                };
                if let Some(handoff_id) = actor_cancel {
                    let _ = self.queue_actor(ActorCommand::CancelHandoff { handoff_id });
                }
                self.dispatch(commands);
            }
            ClipboardTransportEvent::Message {
                host_id,
                process_session_id,
                connection_id,
                message,
            } => {
                if self.network_peers.get(&host_id).is_none_or(|(peer, _)| {
                    peer.connection_id != connection_id
                        || peer.process_session_id != process_session_id
                }) {
                    return;
                }
                self.handle_wire_message(host_id, message);
            }
        }
    }

    fn handle_wire_message(&mut self, host_id: HostId, message: WireMessage) {
        match message {
            WireMessage::AuthorityState(state) => self.accept_authority_state(state),
            WireMessage::PrepareTarget(handoff) => {
                if matches!(self.role, RuntimeRole::Peer { .. }) {
                    let command = ActorCommand::PrepareTarget {
                        handoff_id: handoff.handoff_id,
                        target_token: handoff.target_token.clone(),
                    };
                    if let Err(reason) = self.queue_actor(command) {
                        self.send_prepare_result(&handoff, None, OperationResult::Skipped(reason));
                    }
                }
            }
            WireMessage::PrepareResult(result) => match result.result {
                OperationResult::Completed => {
                    if let Some(generation) = result.baseline_generation {
                        let _ = self.target_prepared(
                            result.handoff_id,
                            &result.target_token,
                            result.target_process_session_id,
                            generation,
                        );
                    }
                }
                OperationResult::Skipped(reason) => self.skip(result.handoff_id, reason),
            },
            WireMessage::OwnershipActivated(handoff) => {
                if let RuntimeRole::Peer {
                    authority_state: Some(state),
                    ..
                } = &mut self.role
                {
                    state.current_token = handoff.target_token.clone();
                    state.active_handoff = Some(handoff.clone());
                }
                let _ = self.queue_actor(ActorCommand::ActivateTarget {
                    handoff_id: handoff.handoff_id,
                    target_token: handoff.target_token,
                });
            }
            WireMessage::SnapshotOffer(payload) => self.accept_snapshot_offer(host_id, payload),
            WireMessage::SnapshotDeliver(payload) => self.stage_delivered_snapshot(payload),
            WireMessage::ApplyResult(result) => match result.result {
                OperationResult::Completed => {
                    if let RuntimeRole::Authority(coordinator) = &mut self.role {
                        let _ = coordinator.applied(result.handoff_id, result.snapshot_id);
                    }
                }
                OperationResult::Skipped(reason) => self.skip(result.handoff_id, reason),
            },
            WireMessage::CancelHandoff(handoff_id) => {
                if let RuntimeRole::Peer { preparation, .. } = &mut self.role {
                    *preparation = None;
                }
                let _ = self.queue_actor(ActorCommand::CancelHandoff { handoff_id });
            }
            WireMessage::ProtocolError(reason) => tracing::debug!(
                event = "clipboard_transfer_rejected",
                host = %host_id,
                reason = reason.code(),
                "clipboard peer reported protocol error"
            ),
            WireMessage::ClipboardHello(_) => {}
        }
    }

    fn accept_authority_state(&mut self, state: AuthorityState) {
        let active_handoff = state.active_handoff.clone();
        let current_token = state.current_token.clone();
        let local_host = match &mut self.role {
            RuntimeRole::Peer {
                local_host,
                authority_state,
                preparation,
                ..
            } => {
                if authority_state.as_ref().is_some_and(|previous| {
                    previous.current_token.authority_session_id
                        != current_token.authority_session_id
                }) {
                    *preparation = None;
                }
                *authority_state = Some(state);
                local_host.clone()
            }
            _ => return,
        };
        let _ = self.queue_actor(ActorCommand::SynchronizeAuthority { current_token });
        if let Some(handoff) = active_handoff {
            if handoff.source_token.owner_host_id == local_host {
                let _ = self.queue_actor(ActorCommand::BindProvisional {
                    handoff_id: handoff.handoff_id,
                    source_token: handoff.source_token,
                });
            }
        }
    }

    fn accept_snapshot_offer(&mut self, host_id: HostId, payload: ClipboardPayload) {
        if !matches!(self.role, RuntimeRole::Authority(_))
            || payload.handoff.source_token.owner_host_id != host_id
            || self.active_envelope(payload.handoff.handoff_id) != Some(payload.handoff.clone())
        {
            return;
        }
        let Ok(kind) = payload.data.kind() else {
            return;
        };
        let metadata = SnapshotMetadata {
            snapshot_id: payload.snapshot_id,
            kind,
            bytes: payload.data.len(),
        };
        self.pending_payload = Some(payload.clone());
        if self
            .source_captured(
                payload.handoff.handoff_id,
                &payload.handoff.source_token,
                payload.handoff.source_process_session_id,
                metadata,
            )
            .is_err()
        {
            self.pending_payload = None;
        }
    }

    fn stage_delivered_snapshot(&mut self, payload: ClipboardPayload) {
        let preparation = match &self.role {
            RuntimeRole::Peer {
                preparation: Some((handoff_id, target_token, generation)),
                ..
            } if *handoff_id == payload.handoff.handoff_id
                && *target_token == payload.handoff.target_token =>
            {
                Some(*generation)
            }
            _ => None,
        };
        let Some(baseline_generation) = preparation else {
            return;
        };
        let command = ActorCommand::StageSnapshot(StagedSnapshot {
            handoff_id: payload.handoff.handoff_id,
            snapshot_id: payload.snapshot_id,
            target_token: payload.handoff.target_token,
            target_process_session_id: payload.handoff.target_process_session_id,
            baseline_generation,
            data: payload.data,
        });
        let _ = self.queue_actor(command);
    }

    fn publish_authority_state(&self, host: &HostId) {
        let RuntimeRole::Authority(coordinator) = &self.role else {
            return;
        };
        let Some((_, outbound)) = self.network_peers.get(host) else {
            return;
        };
        let active_handoff = coordinator.active().map(|handoff| HandoffEnvelope {
            handoff_id: handoff.id,
            source_token: handoff.source_token.clone(),
            source_process_session_id: handoff.source_process_session_id,
            target_token: handoff.target_token.clone(),
            target_process_session_id: handoff.target_process_session_id,
        });
        let _ = outbound.try_send_control(WireMessage::AuthorityState(AuthorityState {
            authority_process_session_id: self.process_session_id,
            current_token: coordinator.current_token().clone(),
            active_handoff,
        }));
    }

    fn publish_authority_state_all(&self) {
        for host in self.network_peers.keys() {
            self.publish_authority_state(host);
        }
    }

    fn begin(&mut self, transition: ClipboardTransitionId, target: HostId) {
        let result = match &mut self.role {
            RuntimeRole::Authority(coordinator) => coordinator.begin_handoff(target),
            _ => return,
        };
        match result {
            Ok(begin) => {
                let target_process_session_id = self
                    .coordinator()
                    .and_then(Coordinator::active)
                    .map_or(ProcessSessionId::new(0), |handoff| {
                        handoff.target_process_session_id
                    });
                let commands = begin.commands;
                self.pending = Some(PendingTransition {
                    id: transition,
                    handoff_id: begin.handoff_id,
                    target_token: begin.target_token,
                    target_process_session_id,
                    activated: false,
                });
                self.dispatch(commands);
            }
            Err(error) => self.log_reducer_error("begin", error),
        }
    }

    fn activate(&mut self, transition: ClipboardTransitionId) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if pending.id != transition {
            return;
        }
        let handoff_id = pending.handoff_id;
        let target_token = pending.target_token.clone();
        let target_process_session_id = pending.target_process_session_id;
        let RuntimeRole::Authority(coordinator) = &mut self.role else {
            return;
        };
        match coordinator.ownership_activated(handoff_id, &target_token, target_process_session_id)
        {
            Ok(commands) => {
                if coordinator.active().is_some() {
                    self.pending.as_mut().expect("transition remains").activated = true;
                } else {
                    self.pending = None;
                }
                self.dispatch(commands);
                self.publish_authority_state_all();
            }
            Err(CoordinatorError::TargetNotPrepared) => {
                self.pending = None;
                tracing::debug!(
                    event = "clipboard_handoff_skipped",
                    reason = ClipboardReason::TargetNotPrepared.code(),
                    "clipboard target was not prepared before input activation"
                );
            }
            Err(error) => self.log_reducer_error("activate", error),
        }
    }

    fn cancel(&mut self, transition: ClipboardTransitionId, reason: ClipboardReason) {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.id != transition)
        {
            return;
        }
        self.cancel_current(reason);
    }

    fn cancel_current(&mut self, reason: ClipboardReason) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if let RuntimeRole::Authority(coordinator) = &mut self.role {
            match coordinator.abort(pending.handoff_id) {
                Ok(commands) => self.dispatch(commands),
                Err(error) => self.log_reducer_error("cancel", error),
            }
        }
        tracing::debug!(
            event = "clipboard_handoff_canceled",
            reason = reason.code(),
            "clipboard handoff canceled"
        );
    }

    fn peer_session_changed(&mut self, host: HostId, process_session_id: ProcessSessionId) {
        let commands = match &mut self.role {
            RuntimeRole::Authority(coordinator) => {
                coordinator.set_process_session(host, process_session_id)
            }
            _ => Vec::new(),
        };
        self.dispatch(commands);
    }

    fn target_prepared(
        &mut self,
        handoff_id: HandoffId,
        target_token: &OwnershipToken,
        target_process_session_id: ProcessSessionId,
        baseline_generation: NativeGeneration,
    ) -> Result<(), CoordinatorError> {
        let RuntimeRole::Authority(coordinator) = &mut self.role else {
            return Err(CoordinatorError::StaleHandoff);
        };
        let commands = coordinator.target_prepared(
            handoff_id,
            target_token,
            target_process_session_id,
            baseline_generation,
        )?;
        self.dispatch(commands);
        Ok(())
    }

    fn source_captured(
        &mut self,
        handoff_id: HandoffId,
        source_token: &OwnershipToken,
        source_process_session_id: ProcessSessionId,
        snapshot: SnapshotMetadata,
    ) -> Result<(), CoordinatorError> {
        let RuntimeRole::Authority(coordinator) = &mut self.role else {
            return Err(CoordinatorError::StaleHandoff);
        };
        let commands = coordinator.source_captured(
            handoff_id,
            source_token,
            source_process_session_id,
            snapshot,
        )?;
        self.dispatch(commands);
        Ok(())
    }

    fn skip(&mut self, handoff_id: HandoffId, reason: ClipboardReason) {
        if let RuntimeRole::Authority(coordinator) = &mut self.role {
            if coordinator.skip(handoff_id, reason).is_ok() {
                self.pending_payload = None;
            }
        }
    }

    fn coordinator(&self) -> Option<&Coordinator> {
        match &self.role {
            RuntimeRole::Authority(coordinator) => Some(coordinator),
            _ => None,
        }
    }

    fn active_envelope(&self, handoff_id: HandoffId) -> Option<HandoffEnvelope> {
        let handoff = self.coordinator()?.active()?;
        (handoff.id == handoff_id).then(|| HandoffEnvelope {
            handoff_id: handoff.id,
            source_token: handoff.source_token.clone(),
            source_process_session_id: handoff.source_process_session_id,
            target_token: handoff.target_token.clone(),
            target_process_session_id: handoff.target_process_session_id,
        })
    }

    fn dispatch(&mut self, commands: Vec<CoordinatorCommand>) {
        for command in commands {
            #[cfg(test)]
            self.effects.push(command.clone());
            match command {
                CoordinatorCommand::PrepareTarget {
                    handoff_id,
                    target_token,
                    target_process_session_id,
                } => {
                    if self.local_host.as_ref() == Some(&target_token.owner_host_id) {
                        if let Err(reason) = self.queue_actor(ActorCommand::PrepareTarget {
                            handoff_id,
                            target_token,
                        }) {
                            self.skip(handoff_id, reason);
                        }
                        continue;
                    }
                    let Some(envelope) = self.active_envelope(handoff_id) else {
                        continue;
                    };
                    self.try_send_control(
                        &target_token.owner_host_id,
                        target_process_session_id,
                        WireMessage::PrepareTarget(envelope),
                    );
                }
                CoordinatorCommand::CaptureSource {
                    handoff_id,
                    source_token,
                    source_process_session_id,
                    max_bytes,
                } => {
                    if self.local_host.as_ref() == Some(&source_token.owner_host_id) {
                        if let Err(reason) = self.queue_actor(ActorCommand::CaptureSource {
                            handoff_id,
                            source_token,
                            max_bytes,
                        }) {
                            self.skip(handoff_id, reason);
                        }
                    } else {
                        self.publish_authority_state(&source_token.owner_host_id);
                        if self
                            .network_peers
                            .get(&source_token.owner_host_id)
                            .is_none_or(|(peer, _)| {
                                peer.process_session_id != source_process_session_id
                            })
                        {
                            tracing::debug!(
                                event = "clipboard_handoff_skipped",
                                handoff_epoch = handoff_id.handoff_epoch.get(),
                                reason = ClipboardReason::StalePeerSession.code(),
                                "clipboard source peer session is unavailable"
                            );
                        }
                    }
                }
                CoordinatorCommand::PublishSnapshot {
                    handoff_id,
                    snapshot_id,
                } => {
                    let source_is_local = self
                        .active_envelope(handoff_id)
                        .and_then(|handoff| {
                            self.local_host
                                .as_ref()
                                .map(|local| handoff.source_token.owner_host_id == *local)
                        })
                        .unwrap_or(false);
                    if source_is_local {
                        if let Err(reason) = self.queue_actor(ActorCommand::PublishSnapshot {
                            handoff_id,
                            snapshot_id,
                        }) {
                            self.skip(handoff_id, reason);
                        }
                    } else {
                        self.publish_pending_payload(handoff_id, snapshot_id);
                    }
                }
                CoordinatorCommand::ActivateTarget {
                    handoff_id,
                    target_token,
                    target_process_session_id,
                } => {
                    if self.local_host.as_ref() == Some(&target_token.owner_host_id) {
                        if let Err(reason) = self.queue_actor(ActorCommand::ActivateTarget {
                            handoff_id,
                            target_token,
                        }) {
                            self.skip(handoff_id, reason);
                        }
                        continue;
                    }
                    let Some(envelope) = self.active_envelope(handoff_id) else {
                        continue;
                    };
                    self.try_send_control(
                        &target_token.owner_host_id,
                        target_process_session_id,
                        WireMessage::OwnershipActivated(envelope),
                    );
                }
                CoordinatorCommand::CancelHandoff { handoff_id } => {
                    self.pending_payload = None;
                    let _ = self.queue_actor(ActorCommand::CancelHandoff { handoff_id });
                    for (_, outbound) in self.network_peers.values() {
                        outbound.cancel_handoff(handoff_id);
                        let _ = outbound.try_send_control(WireMessage::CancelHandoff(handoff_id));
                    }
                }
            }
        }
    }

    fn handle_actor_event(&mut self, command: ActorCommand, event: ActorEvent) {
        match event {
            ActorEvent::Prepared {
                handoff_id,
                target_token,
                process_session_id,
                baseline_generation,
            } => {
                if matches!(self.role, RuntimeRole::Authority(_)) {
                    let _ = self.target_prepared(
                        handoff_id,
                        &target_token,
                        process_session_id,
                        baseline_generation,
                    );
                } else {
                    if let RuntimeRole::Peer { preparation, .. } = &mut self.role {
                        *preparation =
                            Some((handoff_id, target_token.clone(), baseline_generation));
                    }
                    if let Some(handoff) = self.peer_handoff(handoff_id) {
                        self.send_prepare_result(
                            &handoff,
                            Some(baseline_generation),
                            OperationResult::Completed,
                        );
                    }
                }
            }
            ActorEvent::Captured {
                handoff_id,
                source_token,
                snapshot_id,
                kind,
                bytes,
            } => {
                if matches!(self.role, RuntimeRole::Authority(_)) {
                    let _ = self.source_captured(
                        handoff_id,
                        &source_token,
                        self.process_session_id,
                        SnapshotMetadata {
                            snapshot_id,
                            kind,
                            bytes,
                        },
                    );
                } else {
                    let _ = self.queue_actor(ActorCommand::PublishSnapshot {
                        handoff_id,
                        snapshot_id,
                    });
                }
            }
            ActorEvent::Staged { handoff_id, .. } => {
                if let RuntimeRole::Authority(coordinator) = &mut self.role {
                    let _ = coordinator.snapshot_staged(handoff_id);
                }
            }
            ActorEvent::Applied(identity) => {
                if let RuntimeRole::Authority(coordinator) = &mut self.role {
                    let _ = coordinator.applied(identity.handoff_id, identity.snapshot_id);
                } else if let Some(handoff) = self.peer_handoff(identity.handoff_id) {
                    self.send_apply_result(
                        &handoff,
                        identity.snapshot_id,
                        OperationResult::Completed,
                    );
                }
            }
            ActorEvent::Skipped { handoff_id, reason }
            | ActorEvent::BackendUnavailable { handoff_id, reason } => {
                let Some(handoff_id) = handoff_id else {
                    return;
                };
                if matches!(self.role, RuntimeRole::Authority(_)) {
                    self.skip(handoff_id, reason);
                    return;
                }
                match command {
                    ActorCommand::PrepareTarget { .. } => {
                        if let Some(handoff) = self.peer_handoff(handoff_id) {
                            self.send_prepare_result(
                                &handoff,
                                None,
                                OperationResult::Skipped(reason),
                            );
                        }
                    }
                    ActorCommand::StageSnapshot(stage) => {
                        if let Some(handoff) = self.peer_handoff(handoff_id) {
                            self.send_apply_result(
                                &handoff,
                                stage.snapshot_id,
                                OperationResult::Skipped(reason),
                            );
                        }
                    }
                    _ => self.send_to_server_control(WireMessage::ProtocolError(reason)),
                }
            }
            ActorEvent::AuthoritySynchronized { .. }
            | ActorEvent::Generation(_)
            | ActorEvent::ProvisionalCaptured { .. }
            | ActorEvent::Activated { .. }
            | ActorEvent::Published { .. }
            | ActorEvent::Canceled { .. }
            | ActorEvent::Shutdown => {}
        }
    }

    fn handle_actor_payload(&mut self, payload: ActorPayload) {
        if let Some(handoff) = self.active_envelope(payload.handoff_id) {
            if handoff.source_token == payload.source_token
                && handoff.source_process_session_id == self.process_session_id
                && payload.snapshot_id.source_process_session_id == self.process_session_id
            {
                self.publish_payload(ClipboardPayload {
                    handoff,
                    snapshot_id: payload.snapshot_id,
                    data: payload.data,
                });
            }
            return;
        }
        let Some(handoff) = self.peer_handoff(payload.handoff_id) else {
            return;
        };
        if handoff.source_token != payload.source_token
            || handoff.source_process_session_id != self.process_session_id
            || payload.snapshot_id.source_process_session_id != self.process_session_id
        {
            return;
        }
        self.send_to_server_payload(WireMessage::SnapshotOffer(ClipboardPayload {
            handoff,
            snapshot_id: payload.snapshot_id,
            data: payload.data,
        }));
    }

    fn publish_pending_payload(&mut self, handoff_id: HandoffId, snapshot_id: SnapshotId) {
        let Some(payload) = self.pending_payload.take() else {
            return;
        };
        if payload.handoff.handoff_id != handoff_id || payload.snapshot_id != snapshot_id {
            return;
        }
        self.publish_payload(payload);
    }

    fn publish_payload(&mut self, payload: ClipboardPayload) {
        let Some(active) = self.coordinator().and_then(Coordinator::active) else {
            return;
        };
        if active.id != payload.handoff.handoff_id
            || active.source_token != payload.handoff.source_token
            || active.target_token != payload.handoff.target_token
        {
            return;
        }
        if self.local_host.as_ref() == Some(&active.target_host) {
            let Some(preparation) = active.target_preparation else {
                return;
            };
            let command = ActorCommand::StageSnapshot(StagedSnapshot {
                handoff_id: payload.handoff.handoff_id,
                snapshot_id: payload.snapshot_id,
                target_token: payload.handoff.target_token,
                target_process_session_id: payload.handoff.target_process_session_id,
                baseline_generation: preparation.baseline_generation,
                data: payload.data,
            });
            if let Err(reason) = self.queue_actor(command) {
                self.skip(payload.handoff.handoff_id, reason);
            }
            return;
        }
        let target_host = payload.handoff.target_token.owner_host_id.clone();
        let target_session = payload.handoff.target_process_session_id;
        let Some((peer, outbound)) = self.network_peers.get(&target_host) else {
            self.skip(
                payload.handoff.handoff_id,
                ClipboardReason::ChannelUnavailable,
            );
            return;
        };
        if peer.process_session_id != target_session
            || outbound
                .try_send_payload(WireMessage::SnapshotDeliver(payload.clone()))
                .is_err()
        {
            self.skip(
                payload.handoff.handoff_id,
                ClipboardReason::ChannelUnavailable,
            );
        }
    }

    fn peer_handoff(&self, handoff_id: HandoffId) -> Option<HandoffEnvelope> {
        match &self.role {
            RuntimeRole::Peer {
                authority_state: Some(state),
                ..
            } => state
                .active_handoff
                .as_ref()
                .filter(|handoff| handoff.handoff_id == handoff_id)
                .cloned(),
            _ => None,
        }
    }

    fn send_prepare_result(
        &self,
        handoff: &HandoffEnvelope,
        baseline_generation: Option<NativeGeneration>,
        result: OperationResult,
    ) {
        self.send_to_server_control(WireMessage::PrepareResult(PrepareResult {
            handoff_id: handoff.handoff_id,
            target_token: handoff.target_token.clone(),
            target_process_session_id: self.process_session_id,
            baseline_generation,
            result,
        }));
    }

    fn send_apply_result(
        &self,
        handoff: &HandoffEnvelope,
        snapshot_id: SnapshotId,
        result: OperationResult,
    ) {
        self.send_to_server_control(WireMessage::ApplyResult(ApplyResult {
            handoff_id: handoff.handoff_id,
            target_token: handoff.target_token.clone(),
            target_process_session_id: self.process_session_id,
            snapshot_id,
            post_write_generation: None,
            result,
        }));
    }

    fn send_to_server_control(&self, message: WireMessage) {
        let RuntimeRole::Peer { server_host, .. } = &self.role else {
            return;
        };
        if let Some((_, outbound)) = self.network_peers.get(server_host) {
            let _ = outbound.try_send_control(message);
        }
    }

    fn send_to_server_payload(&self, message: WireMessage) {
        let RuntimeRole::Peer { server_host, .. } = &self.role else {
            return;
        };
        if let Some((_, outbound)) = self.network_peers.get(server_host) {
            let _ = outbound.try_send_payload(message);
        }
    }

    fn try_send_control(
        &self,
        host: &HostId,
        process_session_id: ProcessSessionId,
        message: WireMessage,
    ) {
        let Some((peer, outbound)) = self.network_peers.get(host) else {
            return;
        };
        if peer.process_session_id != process_session_id {
            return;
        }
        if let Err(error) = outbound.try_send_control(message) {
            tracing::debug!(
                event = "clipboard_transfer_rejected",
                host = %host,
                error = %error,
                "clipboard control message could not be queued"
            );
        }
    }

    fn log_reducer_error(&self, action: &'static str, error: CoordinatorError) {
        tracing::debug!(
            event = "clipboard_handoff_skipped",
            action,
            error = %error,
            "clipboard coordinator rejected stale or unavailable work"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lan_mouse_clipboard::{
        AuthenticatedPeer, CLIPBOARD_TEXT_V1, CertificateFingerprint, ClipboardData,
        ClipboardHello, ClipboardKind, HandoffEpoch, PeerRegistry, RegistrationOutcome, SnapshotId,
        SnapshotMetadata, SnapshotSequence, read_frame, run_writer, transport_queues,
    };
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn config(enabled: bool) -> ClipboardConfig {
        ClipboardConfig {
            enabled,
            max_bytes: 1024,
        }
    }

    fn authority() -> RuntimeCore {
        RuntimeCore::authority(
            config(true),
            HostId::from("linux"),
            ProcessSessionId::new(10),
            AuthoritySessionId::new(20),
        )
    }

    fn negotiated_peer(host: &str, process_session_id: ProcessSessionId) -> NegotiatedPeer {
        let host_id = HostId::from(host);
        let authenticated = AuthenticatedPeer {
            host_id: host_id.clone(),
            fingerprint: CertificateFingerprint::from_certificate(host.as_bytes()),
        };
        let hello = ClipboardHello {
            host_id,
            process_session_id,
            offered_capabilities: CLIPBOARD_TEXT_V1,
            max_receive_bytes: 1024,
        };
        match PeerRegistry::default()
            .register(&authenticated, &hello, 1024)
            .unwrap()
        {
            RegistrationOutcome::Accepted(peer) => peer,
            _ => unreachable!(),
        }
    }

    #[test]
    fn server_to_remote_orders_begin_before_activation() {
        let mut core = authority();
        let remote_process = ProcessSessionId::new(30);
        core.peer_session_changed(HostId::from("windows"), remote_process);
        let transition = ClipboardTransitionId::Outgoing { lease_epoch: 7 };

        core.handle(ServiceHook::Begin {
            transition,
            target: HostId::from("windows"),
        });
        let pending = core.pending.as_ref().unwrap();
        let handoff_id = pending.handoff_id;
        let target_token = pending.target_token.clone();
        let source_token = match &core.role {
            RuntimeRole::Authority(coordinator) => {
                coordinator.active().unwrap().source_token.clone()
            }
            _ => unreachable!(),
        };
        assert!(matches!(
            core.effects.as_slice(),
            [
                CoordinatorCommand::PrepareTarget { .. },
                CoordinatorCommand::CaptureSource { .. }
            ]
        ));
        core.effects.clear();

        core.target_prepared(
            handoff_id,
            &target_token,
            remote_process,
            NativeGeneration::new(4),
        )
        .unwrap();
        core.source_captured(
            handoff_id,
            &source_token,
            ProcessSessionId::new(10),
            SnapshotMetadata {
                snapshot_id: SnapshotId {
                    source_process_session_id: ProcessSessionId::new(10),
                    sequence: SnapshotSequence::new(1),
                },
                kind: ClipboardKind::Text,
                bytes: 5,
            },
        )
        .unwrap();
        assert!(matches!(
            core.effects.as_slice(),
            [CoordinatorCommand::PublishSnapshot { .. }]
        ));
        core.effects.clear();

        core.handle(ServiceHook::Activate { transition });
        assert!(matches!(
            core.effects.as_slice(),
            [CoordinatorCommand::ActivateTarget { .. }]
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn framed_handoff_prepares_and_delivers_before_remote_activation() {
        let mut core = authority();
        let remote_process = ProcessSessionId::new(30);
        let remote_host = HostId::from("windows");
        core.peer_session_changed(remote_host.clone(), remote_process);
        let (outbound, mut receiver) = transport_queues();
        core.network_peers.insert(
            remote_host.clone(),
            (negotiated_peer("windows", remote_process), outbound),
        );
        let transition = ClipboardTransitionId::Outgoing { lease_epoch: 11 };
        core.handle(ServiceHook::Begin {
            transition,
            target: remote_host,
        });
        let handoff = core.coordinator().unwrap().active().unwrap().clone();
        let cancellation = CancellationToken::new();
        let (mut first_reader, mut first_writer) = tokio::io::duplex(4096);
        let first = tokio::select! {
            result = read_frame(
                &mut first_reader,
                1024,
                Duration::from_secs(1),
                &cancellation,
            ) => result.unwrap(),
            result = run_writer(
                &mut first_writer,
                &mut receiver,
                1024,
                Duration::from_secs(1),
            ) => panic!("transport writer ended before prepare: {result:?}"),
        };
        assert!(matches!(first, WireMessage::PrepareTarget(_)));

        core.target_prepared(
            handoff.id,
            &handoff.target_token,
            remote_process,
            NativeGeneration::new(7),
        )
        .unwrap();
        let snapshot_id = SnapshotId {
            source_process_session_id: ProcessSessionId::new(10),
            sequence: SnapshotSequence::new(1),
        };
        core.handle_actor_event(
            ActorCommand::CaptureSource {
                handoff_id: handoff.id,
                source_token: handoff.source_token.clone(),
                max_bytes: 1024,
            },
            ActorEvent::Captured {
                handoff_id: handoff.id,
                source_token: handoff.source_token.clone(),
                snapshot_id,
                kind: ClipboardKind::Text,
                bytes: 5,
            },
        );
        core.handle_actor_payload(ActorPayload {
            handoff_id: handoff.id,
            snapshot_id,
            source_token: handoff.source_token,
            data: ClipboardData::text(b"hello".to_vec()).unwrap(),
        });
        core.handle(ServiceHook::Activate { transition });

        let (mut reader, mut writer) = tokio::io::duplex(8192);
        let messages = tokio::select! {
            result = async {
                let mut messages = Vec::new();
                for _ in 0..2 {
                    messages.push(read_frame(
                        &mut reader,
                        1024,
                        Duration::from_secs(1),
                        &cancellation,
                    ).await?);
                }
                Ok::<_, lan_mouse_clipboard::FrameError>(messages)
            } => result.unwrap(),
            result = run_writer(
                &mut writer,
                &mut receiver,
                1024,
                Duration::from_secs(1),
            ) => panic!("transport writer ended before frames: {result:?}"),
        };

        assert!(matches!(messages[0], WireMessage::SnapshotDeliver(_)));
        assert!(matches!(messages[1], WireMessage::OwnershipActivated(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn framed_handoff_delivers_snapshot_captured_after_remote_activation() {
        let mut core = authority();
        let remote_process = ProcessSessionId::new(30);
        let remote_host = HostId::from("windows");
        core.peer_session_changed(remote_host.clone(), remote_process);
        let (outbound, mut receiver) = transport_queues();
        core.network_peers.insert(
            remote_host.clone(),
            (negotiated_peer("windows", remote_process), outbound),
        );
        let transition = ClipboardTransitionId::Outgoing { lease_epoch: 12 };
        core.handle(ServiceHook::Begin {
            transition,
            target: remote_host,
        });
        let handoff = core.coordinator().unwrap().active().unwrap().clone();
        core.target_prepared(
            handoff.id,
            &handoff.target_token,
            remote_process,
            NativeGeneration::new(8),
        )
        .unwrap();
        core.handle(ServiceHook::Activate { transition });

        let cancellation = CancellationToken::new();
        let (mut first_reader, mut first_writer) = tokio::io::duplex(4096);
        let first = tokio::select! {
            result = async {
                let prepare = read_frame(
                    &mut first_reader,
                    1024,
                    Duration::from_secs(1),
                    &cancellation,
                ).await?;
                let activated = read_frame(
                    &mut first_reader,
                    1024,
                    Duration::from_secs(1),
                    &cancellation,
                ).await?;
                Ok::<_, lan_mouse_clipboard::FrameError>((prepare, activated))
            } => result.unwrap(),
            result = run_writer(
                &mut first_writer,
                &mut receiver,
                1024,
                Duration::from_secs(1),
            ) => panic!("transport writer ended before activation: {result:?}"),
        };
        assert!(matches!(first.0, WireMessage::PrepareTarget(_)));
        assert!(matches!(first.1, WireMessage::OwnershipActivated(_)));

        let snapshot_id = SnapshotId {
            source_process_session_id: ProcessSessionId::new(10),
            sequence: SnapshotSequence::new(2),
        };
        core.handle_actor_event(
            ActorCommand::CaptureSource {
                handoff_id: handoff.id,
                source_token: handoff.source_token.clone(),
                max_bytes: 1024,
            },
            ActorEvent::Captured {
                handoff_id: handoff.id,
                source_token: handoff.source_token.clone(),
                snapshot_id,
                kind: ClipboardKind::Text,
                bytes: 4,
            },
        );
        core.handle_actor_payload(ActorPayload {
            handoff_id: handoff.id,
            snapshot_id,
            source_token: handoff.source_token,
            data: ClipboardData::text(b"late".to_vec()).unwrap(),
        });

        let (mut reader, mut writer) = tokio::io::duplex(4096);
        let delivered = tokio::select! {
            result = read_frame(
                &mut reader,
                1024,
                Duration::from_secs(1),
                &cancellation,
            ) => result.unwrap(),
            result = run_writer(
                &mut writer,
                &mut receiver,
                1024,
                Duration::from_secs(1),
            ) => panic!("transport writer ended before snapshot: {result:?}"),
        };
        assert!(matches!(delivered, WireMessage::SnapshotDeliver(_)));
    }

    #[test]
    fn activation_before_prepare_skips_without_retaining_input_transition() {
        let mut core = authority();
        core.peer_session_changed(HostId::from("mac"), ProcessSessionId::new(40));
        let transition = ClipboardTransitionId::Outgoing { lease_epoch: 8 };
        core.handle(ServiceHook::Begin {
            transition,
            target: HostId::from("mac"),
        });
        let handoff_id = core.pending.as_ref().unwrap().handoff_id;
        let target_token = core.pending.as_ref().unwrap().target_token.clone();

        core.handle(ServiceHook::Activate { transition });

        assert!(core.pending.is_none());
        let RuntimeRole::Authority(coordinator) = &core.role else {
            unreachable!();
        };
        assert_eq!(coordinator.current_token(), &target_token);
        assert_eq!(
            coordinator.last_terminal(),
            Some(&(handoff_id, Err(ClipboardReason::TargetNotPrepared)))
        );
    }

    #[test]
    fn stale_cancel_does_not_cancel_newer_handoff() {
        let mut core = authority();
        core.peer_session_changed(HostId::from("windows"), ProcessSessionId::new(30));
        core.peer_session_changed(HostId::from("mac"), ProcessSessionId::new(40));
        let old = ClipboardTransitionId::Outgoing { lease_epoch: 1 };
        let new = ClipboardTransitionId::Outgoing { lease_epoch: 2 };
        core.handle(ServiceHook::Begin {
            transition: old,
            target: HostId::from("windows"),
        });
        core.handle(ServiceHook::Begin {
            transition: new,
            target: HostId::from("mac"),
        });
        let new_handoff = core.pending.as_ref().unwrap().handoff_id;

        core.handle(ServiceHook::Cancel {
            transition: old,
            reason: ClipboardReason::Canceled,
        });

        assert_eq!(core.pending.as_ref().unwrap().handoff_id, new_handoff);
    }

    #[test]
    fn disabled_runtime_tracks_owner_without_clipboard_effects() {
        let mut core = RuntimeCore::authority(
            config(false),
            HostId::from("linux"),
            ProcessSessionId::new(10),
            AuthoritySessionId::new(20),
        );
        let transition = ClipboardTransitionId::Outgoing { lease_epoch: 9 };
        core.handle(ServiceHook::Begin {
            transition,
            target: HostId::from("windows"),
        });
        let target_token = core.pending.as_ref().unwrap().target_token.clone();

        core.handle(ServiceHook::Activate { transition });

        assert!(core.effects.is_empty());
        let RuntimeRole::Authority(coordinator) = &core.role else {
            unreachable!();
        };
        assert_eq!(coordinator.current_token(), &target_token);
    }

    #[test]
    fn full_or_closed_hook_queue_never_blocks_input_caller() {
        let (hook_tx, _hook_rx) = mpsc::channel(1);
        let handle = ClipboardHandle { hook_tx };
        assert!(handle.capture_provisional());
        assert!(!handle.capture_provisional());

        let (hook_tx, hook_rx) = mpsc::channel(1);
        drop(hook_rx);
        let handle = ClipboardHandle { hook_tx };
        assert!(!handle.capture_provisional());
    }

    #[test]
    fn fallback_transition_has_distinct_identity_class() {
        assert_ne!(
            ClipboardTransitionId::Fallback { release_epoch: 3 },
            ClipboardTransitionId::Outgoing { lease_epoch: 3 }
        );
        assert_eq!(
            HandoffId {
                authority_session_id: AuthoritySessionId::new(1),
                handoff_epoch: HandoffEpoch::new(1),
            }
            .handoff_epoch
            .get(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remote_fallback_publishes_provisional_handoff_before_activation() {
        let mut core = authority();
        let remote_session = ProcessSessionId::new(30);
        core.peer_session_changed(HostId::from("windows"), remote_session);

        let outgoing = ClipboardTransitionId::Outgoing { lease_epoch: 1 };
        core.handle(ServiceHook::Begin {
            transition: outgoing,
            target: HostId::from("windows"),
        });
        let outgoing_pending = core.pending.as_ref().unwrap();
        core.target_prepared(
            outgoing_pending.handoff_id,
            &outgoing_pending.target_token.clone(),
            remote_session,
            NativeGeneration::new(1),
        )
        .unwrap();
        core.handle(ServiceHook::Activate {
            transition: outgoing,
        });

        let (outbound, mut receiver) = transport_queues();
        core.network_peers.insert(
            HostId::from("windows"),
            (negotiated_peer("windows", remote_session), outbound),
        );
        core.effects.clear();
        core.handle(ServiceHook::Begin {
            transition: ClipboardTransitionId::Fallback { release_epoch: 4 },
            target: HostId::from("linux"),
        });

        let (mut reader, mut writer) = tokio::io::duplex(4096);
        let cancellation = CancellationToken::new();
        let messages = tokio::select! {
            result = async {
                let first = read_frame(
                    &mut reader,
                    1024,
                    Duration::from_secs(1),
                    &cancellation,
                ).await?;
                let second = read_frame(
                    &mut reader,
                    1024,
                    Duration::from_secs(1),
                    &cancellation,
                ).await?;
                Ok::<_, lan_mouse_clipboard::FrameError>((first, second))
            } => result.unwrap(),
            result = run_writer(
                &mut writer,
                &mut receiver,
                1024,
                Duration::from_secs(1),
            ) => panic!("transport writer ended before frame: {result:?}"),
        };

        assert!(matches!(messages.0, WireMessage::CancelHandoff(_)));
        let WireMessage::AuthorityState(state) = messages.1 else {
            panic!("expected authority state before fallback activation");
        };
        let handoff = state.active_handoff.expect("fallback handoff");
        assert_eq!(handoff.source_token.owner_host_id, HostId::from("windows"));
        assert_eq!(handoff.target_token.owner_host_id, HostId::from("linux"));
        assert_eq!(
            core.coordinator().unwrap().current_token().owner_host_id,
            HostId::from("windows")
        );
    }
}
