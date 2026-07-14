use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use lan_mouse_clipboard::{
    AuthoritySessionId, AuthorityState, ClipboardReason, Coordinator, CoordinatorCommand,
    CoordinatorError, HandoffEnvelope, HandoffId, HostId, NativeGeneration, NegotiatedPeer,
    OwnershipToken, ProcessSessionId, TlsIdentity, TransportHandle, WireMessage,
};
use lan_mouse_ipc::SwitchHost;
use tokio::{sync::mpsc, task::JoinHandle};

#[cfg(test)]
use lan_mouse_clipboard::SnapshotMetadata;

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
        let core = match (local_host, server_host) {
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
        let transport = match (identity, local_host, server_host) {
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
                Some(ClipboardTransport::start(ClipboardTransportConfig {
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
                }))
            }
            _ => None,
        };
        let task = tokio::task::spawn_local(run(core, hook_rx, transport));
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
    mut transport: Option<ClipboardTransport>,
) {
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
        }
    }
    if let Some(mut transport) = transport {
        transport.shutdown();
    }
}

async fn wait_for_transport_event(
    transport: &mut Option<ClipboardTransport>,
) -> Option<ClipboardTransportEvent> {
    match transport {
        Some(transport) => transport.event().await,
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
    },
    Unavailable,
}

struct RuntimeCore {
    local_host: Option<HostId>,
    process_session_id: ProcessSessionId,
    role: RuntimeRole,
    pending: Option<PendingTransition>,
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
            role: RuntimeRole::Authority(Coordinator::new(
                config.enabled,
                authority_session_id,
                server_host,
                process_session_id,
                config.max_bytes,
            )),
            pending: None,
            #[cfg(test)]
            effects: Vec::new(),
            network_peers: HashMap::new(),
        }
    }

    fn peer(
        _config: ClipboardConfig,
        local_host: HostId,
        server_host: HostId,
        process_session_id: ProcessSessionId,
        _authority_session_id: Arc<RwLock<Option<AuthoritySessionId>>>,
    ) -> Self {
        Self {
            local_host: Some(local_host.clone()),
            process_session_id,
            role: RuntimeRole::Peer {
                local_host,
                server_host,
                authority_state: None,
            },
            pending: None,
            #[cfg(test)]
            effects: Vec::new(),
            network_peers: HashMap::new(),
        }
    }

    fn unavailable(process_session_id: ProcessSessionId) -> Self {
        Self {
            local_host: None,
            process_session_id,
            role: RuntimeRole::Unavailable,
            pending: None,
            #[cfg(test)]
            effects: Vec::new(),
            network_peers: HashMap::new(),
        }
    }

    fn handle(&mut self, hook: ServiceHook) {
        match hook {
            ServiceHook::Begin { transition, target } => self.begin(transition, target),
            ServiceHook::Activate { transition } => self.activate(transition),
            ServiceHook::Cancel { transition, reason } => self.cancel(transition, reason),
            ServiceHook::CancelCurrent { reason } => self.cancel_current(reason),
            ServiceHook::CaptureProvisional => {
                if let RuntimeRole::Peer {
                    local_host,
                    server_host,
                    ..
                } = &self.role
                {
                    tracing::debug!(
                        event = "clipboard_provisional_capture_requested",
                        local_host = %local_host,
                        server_host = %server_host,
                        process_session = self.process_session_id.get(),
                        "clipboard provisional capture notice accepted"
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
                let commands = match &mut self.role {
                    RuntimeRole::Authority(coordinator) => {
                        coordinator.remove_process_session(&host_id)
                    }
                    _ => Vec::new(),
                };
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
        match (&mut self.role, message) {
            (
                RuntimeRole::Peer {
                    authority_state, ..
                },
                WireMessage::AuthorityState(state),
            ) => *authority_state = Some(state),
            (RuntimeRole::Authority(_), WireMessage::PrepareResult(result)) => {
                if let Some(generation) = result.baseline_generation {
                    let _ = self.target_prepared(
                        result.handoff_id,
                        &result.target_token,
                        result.target_process_session_id,
                        generation,
                    );
                }
            }
            (_, WireMessage::ProtocolError(reason)) => tracing::debug!(
                event = "clipboard_transfer_rejected",
                host = %host_id,
                reason = reason.code(),
                "clipboard peer reported protocol error"
            ),
            _ => {}
        }
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

    #[cfg(test)]
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
                    ..
                } => {
                    if self.local_host.as_ref() != Some(&source_token.owner_host_id) {
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
                CoordinatorCommand::PublishSnapshot { .. } => {
                    // P4 native actor integration owns payload publication.
                }
                CoordinatorCommand::ActivateTarget {
                    handoff_id,
                    target_token,
                    target_process_session_id,
                } => {
                    if self.local_host.as_ref() == Some(&target_token.owner_host_id) {
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
                    for (_, outbound) in self.network_peers.values() {
                        outbound.cancel_handoff(handoff_id);
                        let _ = outbound.try_send_control(WireMessage::CancelHandoff(handoff_id));
                    }
                }
            }
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
        AuthenticatedPeer, CLIPBOARD_TEXT_V1, CertificateFingerprint, ClipboardHello,
        ClipboardKind, HandoffEpoch, PeerRegistry, RegistrationOutcome, SnapshotId,
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
