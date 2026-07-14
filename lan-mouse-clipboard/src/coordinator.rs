use crate::{
    AuthoritySessionId, ClipboardKind, ClipboardReason, HandoffEpoch, HandoffId, HostId,
    NativeGeneration, OwnershipEpoch, OwnershipToken, ProcessSessionId, SnapshotId,
};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandoffPhase {
    Capturing,
    Ready,
    InFlight,
    Staged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotMetadata {
    pub snapshot_id: SnapshotId,
    pub kind: ClipboardKind,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetPreparation {
    pub process_session_id: ProcessSessionId,
    pub baseline_generation: NativeGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveHandoff {
    pub id: HandoffId,
    pub source_host: HostId,
    pub source_token: OwnershipToken,
    pub source_process_session_id: ProcessSessionId,
    pub target_host: HostId,
    pub target_token: OwnershipToken,
    pub target_process_session_id: ProcessSessionId,
    pub phase: HandoffPhase,
    pub snapshot: Option<SnapshotMetadata>,
    pub target_preparation: Option<TargetPreparation>,
    pub target_activated: bool,
    snapshot_published: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginHandoff {
    pub handoff_id: HandoffId,
    pub source_token: OwnershipToken,
    pub target_token: OwnershipToken,
    pub commands: Vec<CoordinatorCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingOwnership {
    handoff_id: HandoffId,
    target_token: OwnershipToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoordinatorCommand {
    PrepareTarget {
        handoff_id: HandoffId,
        target_token: OwnershipToken,
        target_process_session_id: ProcessSessionId,
    },
    CaptureSource {
        handoff_id: HandoffId,
        source_token: OwnershipToken,
        source_process_session_id: ProcessSessionId,
        max_bytes: usize,
    },
    PublishSnapshot {
        handoff_id: HandoffId,
        snapshot_id: SnapshotId,
    },
    ActivateTarget {
        handoff_id: HandoffId,
        target_token: OwnershipToken,
        target_process_session_id: ProcessSessionId,
    },
    CancelHandoff {
        handoff_id: HandoffId,
    },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CoordinatorError {
    #[error("clipboard handoff identity exhausted")]
    IdentityExhausted,
    #[error("clipboard target is already the current owner")]
    SameHost,
    #[error("clipboard handoff is stale")]
    StaleHandoff,
    #[error("clipboard owner token is stale")]
    StaleOwnerToken,
    #[error("clipboard process session is stale")]
    StaleProcessSession,
    #[error("clipboard target was not prepared before activation")]
    TargetNotPrepared,
    #[error("clipboard snapshot is invalid")]
    InvalidSnapshot,
}

#[derive(Debug)]
pub struct Coordinator {
    enabled: bool,
    authority_session_id: AuthoritySessionId,
    current_token: OwnershipToken,
    next_ownership_epoch: OwnershipEpoch,
    next_handoff_epoch: HandoffEpoch,
    max_bytes: usize,
    process_sessions: HashMap<HostId, ProcessSessionId>,
    pending_ownership: Option<PendingOwnership>,
    active: Option<ActiveHandoff>,
    last_terminal: Option<(HandoffId, Result<(), ClipboardReason>)>,
}

impl Coordinator {
    pub fn new(
        enabled: bool,
        authority_session_id: AuthoritySessionId,
        server_host: HostId,
        server_process_session_id: ProcessSessionId,
        max_bytes: usize,
    ) -> Self {
        let current_token = OwnershipToken {
            authority_session_id,
            ownership_epoch: OwnershipEpoch::new(0),
            owner_host_id: server_host.clone(),
        };
        let process_sessions = HashMap::from([(server_host, server_process_session_id)]);
        Self {
            enabled,
            authority_session_id,
            current_token,
            next_ownership_epoch: OwnershipEpoch::new(0),
            next_handoff_epoch: HandoffEpoch::new(0),
            max_bytes,
            process_sessions,
            pending_ownership: None,
            active: None,
            last_terminal: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn current_token(&self) -> &OwnershipToken {
        &self.current_token
    }

    pub fn active(&self) -> Option<&ActiveHandoff> {
        self.active.as_ref()
    }

    pub fn last_terminal(&self) -> Option<&(HandoffId, Result<(), ClipboardReason>)> {
        self.last_terminal.as_ref()
    }

    pub fn set_process_session(
        &mut self,
        host: HostId,
        session: ProcessSessionId,
    ) -> Vec<CoordinatorCommand> {
        let changed = self
            .process_sessions
            .insert(host.clone(), session)
            .is_some_and(|previous| previous != session);
        if changed
            && self
                .active
                .as_ref()
                .is_some_and(|handoff| handoff.source_host == host || handoff.target_host == host)
        {
            self.cancel(ClipboardReason::StalePeerSession)
                .into_iter()
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn remove_process_session(&mut self, host: &HostId) -> Vec<CoordinatorCommand> {
        if self.process_sessions.remove(host).is_none() {
            return Vec::new();
        }
        if self
            .active
            .as_ref()
            .is_some_and(|handoff| handoff.source_host == *host || handoff.target_host == *host)
        {
            self.cancel(ClipboardReason::ChannelUnavailable)
                .into_iter()
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) -> Vec<CoordinatorCommand> {
        if self.enabled == enabled {
            return Vec::new();
        }
        self.enabled = enabled;
        if enabled {
            Vec::new()
        } else {
            self.cancel(ClipboardReason::Canceled).into_iter().collect()
        }
    }

    pub fn begin_handoff(&mut self, target_host: HostId) -> Result<BeginHandoff, CoordinatorError> {
        if target_host == self.current_token.owner_host_id {
            return Err(CoordinatorError::SameHost);
        }
        let source_process_session_id = self
            .process_sessions
            .get(&self.current_token.owner_host_id)
            .copied();
        let target_process_session_id = self.process_sessions.get(&target_host).copied();

        let ownership_epoch = self
            .next_ownership_epoch
            .get()
            .checked_add(1)
            .ok_or(CoordinatorError::IdentityExhausted)?;
        let handoff_epoch = self
            .next_handoff_epoch
            .get()
            .checked_add(1)
            .ok_or(CoordinatorError::IdentityExhausted)?;
        let mut commands: Vec<_> = self.cancel(ClipboardReason::Canceled).into_iter().collect();
        self.next_ownership_epoch = OwnershipEpoch::new(ownership_epoch);
        self.next_handoff_epoch = HandoffEpoch::new(handoff_epoch);

        let handoff_id = HandoffId {
            authority_session_id: self.authority_session_id,
            handoff_epoch: self.next_handoff_epoch,
        };
        let target_token = OwnershipToken {
            authority_session_id: self.authority_session_id,
            ownership_epoch: self.next_ownership_epoch,
            owner_host_id: target_host.clone(),
        };
        let source_token = self.current_token.clone();
        self.pending_ownership = Some(PendingOwnership {
            handoff_id,
            target_token: target_token.clone(),
        });
        if let (true, Some(source_process_session_id), Some(target_process_session_id)) = (
            self.enabled,
            source_process_session_id,
            target_process_session_id,
        ) {
            commands.extend([
                CoordinatorCommand::PrepareTarget {
                    handoff_id,
                    target_token: target_token.clone(),
                    target_process_session_id,
                },
                CoordinatorCommand::CaptureSource {
                    handoff_id,
                    source_token: source_token.clone(),
                    source_process_session_id,
                    max_bytes: self.max_bytes,
                },
            ]);
            self.active = Some(ActiveHandoff {
                id: handoff_id,
                source_host: source_token.owner_host_id.clone(),
                source_token: source_token.clone(),
                source_process_session_id,
                target_host,
                target_token: target_token.clone(),
                target_process_session_id,
                phase: HandoffPhase::Capturing,
                snapshot: None,
                target_preparation: None,
                target_activated: false,
                snapshot_published: false,
            });
        } else {
            let reason = if self.enabled {
                ClipboardReason::CapabilityMissing
            } else {
                ClipboardReason::Canceled
            };
            self.active = None;
            self.last_terminal = Some((handoff_id, Err(reason)));
        }
        Ok(BeginHandoff {
            handoff_id,
            source_token,
            target_token,
            commands,
        })
    }

    pub fn target_prepared(
        &mut self,
        handoff_id: HandoffId,
        target_token: &OwnershipToken,
        target_process_session_id: ProcessSessionId,
        baseline_generation: NativeGeneration,
    ) -> Result<Vec<CoordinatorCommand>, CoordinatorError> {
        let handoff = self.match_active_mut(handoff_id)?;
        if handoff.target_token != *target_token {
            return Err(CoordinatorError::StaleOwnerToken);
        }
        if handoff.target_process_session_id != target_process_session_id {
            return Err(CoordinatorError::StaleProcessSession);
        }
        if handoff.target_activated {
            return Err(CoordinatorError::TargetNotPrepared);
        }
        handoff.target_preparation = Some(TargetPreparation {
            process_session_id: target_process_session_id,
            baseline_generation,
        });
        Ok(Self::publish_if_ready(handoff))
    }

    pub fn source_captured(
        &mut self,
        handoff_id: HandoffId,
        source_token: &OwnershipToken,
        source_process_session_id: ProcessSessionId,
        snapshot: SnapshotMetadata,
    ) -> Result<Vec<CoordinatorCommand>, CoordinatorError> {
        if snapshot.bytes > self.max_bytes {
            return Err(CoordinatorError::InvalidSnapshot);
        }
        let handoff = self.match_active_mut(handoff_id)?;
        if handoff.source_token != *source_token {
            return Err(CoordinatorError::StaleOwnerToken);
        }
        if handoff.source_process_session_id != source_process_session_id
            || snapshot.snapshot_id.source_process_session_id != source_process_session_id
        {
            return Err(CoordinatorError::StaleProcessSession);
        }
        handoff.snapshot = Some(snapshot);
        handoff.phase = HandoffPhase::Ready;
        Ok(Self::publish_if_ready(handoff))
    }

    pub fn ownership_activated(
        &mut self,
        handoff_id: HandoffId,
        target_token: &OwnershipToken,
        target_process_session_id: ProcessSessionId,
    ) -> Result<Vec<CoordinatorCommand>, CoordinatorError> {
        let pending = self
            .pending_ownership
            .as_ref()
            .ok_or(CoordinatorError::StaleHandoff)?;
        if pending.handoff_id != handoff_id {
            return Err(CoordinatorError::StaleHandoff);
        }
        if pending.target_token != *target_token {
            return Err(CoordinatorError::StaleOwnerToken);
        }
        self.pending_ownership = None;
        self.current_token = target_token.clone();
        if self.active.is_none() {
            return Ok(Vec::new());
        }
        {
            let handoff = self.match_active_mut(handoff_id)?;
            if handoff.target_token != *target_token {
                return Err(CoordinatorError::StaleOwnerToken);
            }
            if handoff.target_process_session_id != target_process_session_id {
                return Err(CoordinatorError::StaleProcessSession);
            }
        }
        if self
            .active
            .as_ref()
            .is_some_and(|handoff| handoff.target_preparation.is_none())
        {
            self.active = None;
            self.last_terminal = Some((handoff_id, Err(ClipboardReason::TargetNotPrepared)));
            return Err(CoordinatorError::TargetNotPrepared);
        }
        let handoff = self
            .active
            .as_mut()
            .expect("validated active handoff remains installed");
        handoff.target_activated = true;
        let mut commands = vec![CoordinatorCommand::ActivateTarget {
            handoff_id,
            target_token: target_token.clone(),
            target_process_session_id,
        }];
        commands.extend(Self::publish_if_ready(handoff));
        Ok(commands)
    }

    pub fn snapshot_staged(&mut self, handoff_id: HandoffId) -> Result<(), CoordinatorError> {
        let handoff = self.match_active_mut(handoff_id)?;
        handoff.phase = HandoffPhase::Staged;
        Ok(())
    }

    pub fn applied(
        &mut self,
        handoff_id: HandoffId,
        snapshot_id: SnapshotId,
    ) -> Result<(), CoordinatorError> {
        let handoff = self.match_active_mut(handoff_id)?;
        if !handoff.target_activated
            || handoff
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.snapshot_id)
                != Some(snapshot_id)
        {
            return Err(CoordinatorError::InvalidSnapshot);
        }
        self.active = None;
        self.last_terminal = Some((handoff_id, Ok(())));
        Ok(())
    }

    pub fn skip(
        &mut self,
        handoff_id: HandoffId,
        reason: ClipboardReason,
    ) -> Result<(), CoordinatorError> {
        self.match_active_mut(handoff_id)?;
        self.active = None;
        self.last_terminal = Some((handoff_id, Err(reason)));
        Ok(())
    }

    pub fn abort(
        &mut self,
        handoff_id: HandoffId,
    ) -> Result<Vec<CoordinatorCommand>, CoordinatorError> {
        if self
            .pending_ownership
            .as_ref()
            .is_none_or(|pending| pending.handoff_id != handoff_id)
        {
            return Err(CoordinatorError::StaleHandoff);
        }
        self.pending_ownership = None;
        Ok(self.cancel(ClipboardReason::Canceled).into_iter().collect())
    }

    fn cancel(&mut self, reason: ClipboardReason) -> Option<CoordinatorCommand> {
        let handoff = self.active.take()?;
        self.last_terminal = Some((handoff.id, Err(reason)));
        Some(CoordinatorCommand::CancelHandoff {
            handoff_id: handoff.id,
        })
    }

    fn match_active_mut(
        &mut self,
        handoff_id: HandoffId,
    ) -> Result<&mut ActiveHandoff, CoordinatorError> {
        let handoff = self.active.as_mut().ok_or(CoordinatorError::StaleHandoff)?;
        if handoff.id != handoff_id {
            return Err(CoordinatorError::StaleHandoff);
        }
        Ok(handoff)
    }

    fn publish_if_ready(handoff: &mut ActiveHandoff) -> Vec<CoordinatorCommand> {
        if handoff.snapshot_published
            || handoff.snapshot.is_none()
            || handoff.target_preparation.is_none()
        {
            return Vec::new();
        }
        handoff.snapshot_published = true;
        handoff.phase = HandoffPhase::InFlight;
        vec![CoordinatorCommand::PublishSnapshot {
            handoff_id: handoff.id,
            snapshot_id: handoff.snapshot.expect("snapshot checked").snapshot_id,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClipboardKind, SnapshotSequence};

    fn fixture() -> (
        Coordinator,
        HostId,
        HostId,
        ProcessSessionId,
        ProcessSessionId,
    ) {
        let server = HostId::from("server");
        let remote = HostId::from("remote");
        let server_process = ProcessSessionId::new(11);
        let remote_process = ProcessSessionId::new(22);
        let mut coordinator = Coordinator::new(
            true,
            AuthoritySessionId::new(7),
            server.clone(),
            server_process,
            32,
        );
        assert!(
            coordinator
                .set_process_session(remote.clone(), remote_process)
                .is_empty()
        );
        (coordinator, server, remote, server_process, remote_process)
    }

    fn snapshot(process: ProcessSessionId, sequence: u64) -> SnapshotMetadata {
        SnapshotMetadata {
            snapshot_id: SnapshotId {
                source_process_session_id: process,
                sequence: SnapshotSequence::new(sequence),
            },
            kind: ClipboardKind::Text,
            bytes: 5,
        }
    }

    #[test]
    fn abort_consumes_owner_and_handoff_epochs() {
        let (mut coordinator, _, remote, _, _) = fixture();
        let first = coordinator.begin_handoff(remote.clone()).unwrap();
        coordinator.abort(first.handoff_id).unwrap();
        let second = coordinator.begin_handoff(remote).unwrap();
        assert!(
            second.target_token.ownership_epoch.get() > first.target_token.ownership_epoch.get()
        );
        assert!(second.handoff_id.handoff_epoch.get() > first.handoff_id.handoff_epoch.get());
    }

    #[test]
    fn exhausted_identity_is_rejected_without_wrap_or_partial_handoff() {
        let (mut coordinator, _, remote, _, _) = fixture();
        coordinator.next_ownership_epoch = OwnershipEpoch::new(u64::MAX);
        assert_eq!(
            coordinator.begin_handoff(remote),
            Err(CoordinatorError::IdentityExhausted)
        );
        assert!(coordinator.active().is_none());
        assert_eq!(
            coordinator.next_ownership_epoch,
            OwnershipEpoch::new(u64::MAX)
        );
    }

    #[test]
    fn source_and_target_must_match_recorded_sessions_and_tokens() {
        let (mut coordinator, _, remote, server_process, remote_process) = fixture();
        let begin = coordinator.begin_handoff(remote).unwrap();
        let mut wrong_token = begin.source_token.clone();
        wrong_token.ownership_epoch = OwnershipEpoch::new(99);
        assert_eq!(
            coordinator.source_captured(
                begin.handoff_id,
                &wrong_token,
                server_process,
                snapshot(server_process, 1),
            ),
            Err(CoordinatorError::StaleOwnerToken)
        );
        assert_eq!(
            coordinator.target_prepared(
                begin.handoff_id,
                &begin.target_token,
                ProcessSessionId::new(remote_process.get() + 1),
                NativeGeneration::new(1),
            ),
            Err(CoordinatorError::StaleProcessSession)
        );
        let mut wrong_host = begin.source_token.clone();
        wrong_host.owner_host_id = HostId::from("other");
        assert_eq!(
            coordinator.source_captured(
                begin.handoff_id,
                &wrong_host,
                server_process,
                snapshot(server_process, 1),
            ),
            Err(CoordinatorError::StaleOwnerToken)
        );
    }

    #[test]
    fn prepare_and_capture_publish_once_in_either_order() {
        let (mut coordinator, _, remote, server_process, remote_process) = fixture();
        let begin = coordinator.begin_handoff(remote).unwrap();
        assert!(
            coordinator
                .source_captured(
                    begin.handoff_id,
                    &begin.source_token,
                    server_process,
                    snapshot(server_process, 1),
                )
                .unwrap()
                .is_empty()
        );
        let commands = coordinator
            .target_prepared(
                begin.handoff_id,
                &begin.target_token,
                remote_process,
                NativeGeneration::new(4),
            )
            .unwrap();
        assert!(matches!(
            commands.as_slice(),
            [CoordinatorCommand::PublishSnapshot { .. }]
        ));
        assert!(
            coordinator
                .target_prepared(
                    begin.handoff_id,
                    &begin.target_token,
                    remote_process,
                    NativeGeneration::new(4),
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn activation_without_completed_prepare_skips_without_blocking_owner_commit() {
        let (mut coordinator, _, remote, _, remote_process) = fixture();
        let begin = coordinator.begin_handoff(remote).unwrap();
        assert_eq!(
            coordinator.ownership_activated(begin.handoff_id, &begin.target_token, remote_process,),
            Err(CoordinatorError::TargetNotPrepared)
        );
        assert_eq!(coordinator.current_token(), &begin.target_token);
        assert!(coordinator.active().is_none());
        assert_eq!(
            coordinator.last_terminal(),
            Some(&(begin.handoff_id, Err(ClipboardReason::TargetNotPrepared)))
        );
        assert_eq!(
            coordinator.target_prepared(
                begin.handoff_id,
                &begin.target_token,
                remote_process,
                NativeGeneration::new(1),
            ),
            Err(CoordinatorError::StaleHandoff)
        );
    }

    #[test]
    fn disable_cancels_only_clipboard_state() {
        let (mut coordinator, _, remote, _, remote_process) = fixture();
        let token_before = coordinator.current_token().clone();
        let begin = coordinator.begin_handoff(remote).unwrap();
        assert_eq!(
            coordinator.set_enabled(false),
            vec![CoordinatorCommand::CancelHandoff {
                handoff_id: begin.handoff_id,
            }]
        );
        assert_eq!(coordinator.current_token(), &token_before);
        assert!(coordinator.active().is_none());
        assert!(
            coordinator
                .ownership_activated(begin.handoff_id, &begin.target_token, remote_process)
                .unwrap()
                .is_empty()
        );
        assert_eq!(coordinator.current_token(), &begin.target_token);
        assert!(coordinator.set_enabled(true).is_empty());
    }

    #[test]
    fn missing_peer_capability_still_advances_input_owner_token() {
        let (mut coordinator, server, _, server_process, _) = fixture();
        let remote = HostId::from("not-connected");

        let outbound = coordinator.begin_handoff(remote).unwrap();
        assert!(outbound.commands.is_empty());
        assert!(coordinator.active().is_none());
        assert_eq!(
            coordinator.last_terminal(),
            Some(&(outbound.handoff_id, Err(ClipboardReason::CapabilityMissing)))
        );
        assert!(
            coordinator
                .ownership_activated(
                    outbound.handoff_id,
                    &outbound.target_token,
                    ProcessSessionId::new(0),
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(coordinator.current_token(), &outbound.target_token);

        let fallback = coordinator.begin_handoff(server).unwrap();
        assert!(fallback.commands.is_empty());
        assert_eq!(fallback.source_token, outbound.target_token);
        assert!(
            coordinator
                .ownership_activated(fallback.handoff_id, &fallback.target_token, server_process,)
                .unwrap()
                .is_empty()
        );
        assert_eq!(coordinator.current_token(), &fallback.target_token);
    }

    #[test]
    fn abort_clears_pending_activation_even_without_clipboard_capability() {
        let (mut coordinator, _, _, _, _) = fixture();
        let begin = coordinator
            .begin_handoff(HostId::from("not-connected"))
            .unwrap();

        assert!(coordinator.abort(begin.handoff_id).unwrap().is_empty());
        assert_eq!(
            coordinator.ownership_activated(
                begin.handoff_id,
                &begin.target_token,
                ProcessSessionId::new(0),
            ),
            Err(CoordinatorError::StaleHandoff)
        );
        assert_eq!(coordinator.current_token(), &begin.source_token);
    }

    #[test]
    fn delayed_remote_a_result_cannot_mutate_remote_b_handoff() {
        let (mut coordinator, server, remote_a, server_process, remote_a_process) = fixture();
        let remote_b = HostId::from("remote-b");
        let remote_b_process = ProcessSessionId::new(33);
        assert!(
            coordinator
                .set_process_session(remote_b.clone(), remote_b_process)
                .is_empty()
        );
        let first = coordinator.begin_handoff(remote_a).unwrap();
        coordinator.abort(first.handoff_id).unwrap();
        let second = coordinator.begin_handoff(remote_b).unwrap();
        assert_eq!(
            coordinator.source_captured(
                first.handoff_id,
                &first.source_token,
                server_process,
                snapshot(server_process, 1),
            ),
            Err(CoordinatorError::StaleHandoff)
        );
        assert_eq!(coordinator.active().unwrap().id, second.handoff_id);
        assert_eq!(coordinator.current_token().owner_host_id, server);
        assert_eq!(
            coordinator.active().unwrap().target_process_session_id,
            remote_b_process
        );
        assert_ne!(remote_a_process, remote_b_process);
    }

    #[test]
    fn supersession_reports_cancel_before_new_work_and_never_reuses_epochs() {
        let (mut coordinator, _, remote_a, _, _) = fixture();
        let remote_b = HostId::from("remote-b");
        assert!(
            coordinator
                .set_process_session(remote_b.clone(), ProcessSessionId::new(33))
                .is_empty()
        );
        let first = coordinator.begin_handoff(remote_a).unwrap();
        let second = coordinator.begin_handoff(remote_b).unwrap();
        assert_eq!(
            second.commands.first(),
            Some(&CoordinatorCommand::CancelHandoff {
                handoff_id: first.handoff_id,
            })
        );
        assert!(second.handoff_id.handoff_epoch > first.handoff_id.handoff_epoch);
        assert!(second.target_token.ownership_epoch > first.target_token.ownership_epoch);
    }

    #[test]
    fn process_restart_cancels_matching_handoff() {
        let (mut coordinator, _, remote, _, remote_process) = fixture();
        let begin = coordinator.begin_handoff(remote.clone()).unwrap();
        assert_eq!(
            coordinator
                .set_process_session(remote, ProcessSessionId::new(remote_process.get() + 1),),
            vec![CoordinatorCommand::CancelHandoff {
                handoff_id: begin.handoff_id,
            }]
        );
        assert!(coordinator.active().is_none());
    }

    #[test]
    fn peer_disconnect_cancels_clipboard_but_preserves_pending_input_activation() {
        let (mut coordinator, _, remote, _, remote_process) = fixture();
        let begin = coordinator.begin_handoff(remote.clone()).unwrap();

        assert_eq!(
            coordinator.remove_process_session(&remote),
            vec![CoordinatorCommand::CancelHandoff {
                handoff_id: begin.handoff_id,
            }]
        );
        assert!(coordinator.active().is_none());
        assert!(
            coordinator
                .ownership_activated(begin.handoff_id, &begin.target_token, remote_process)
                .unwrap()
                .is_empty()
        );
        assert_eq!(coordinator.current_token(), &begin.target_token);
    }

    #[test]
    fn successful_trace_commits_then_consumes_new_epochs_for_fallback() {
        let (mut coordinator, server, remote, server_process, remote_process) = fixture();
        let begin = coordinator.begin_handoff(remote).unwrap();
        assert!(matches!(
            coordinator
                .target_prepared(
                    begin.handoff_id,
                    &begin.target_token,
                    remote_process,
                    NativeGeneration::new(4),
                )
                .unwrap()
                .as_slice(),
            []
        ));
        assert!(matches!(
            coordinator
                .source_captured(
                    begin.handoff_id,
                    &begin.source_token,
                    server_process,
                    snapshot(server_process, 1),
                )
                .unwrap()
                .as_slice(),
            [CoordinatorCommand::PublishSnapshot { .. }]
        ));
        assert!(matches!(
            coordinator
                .ownership_activated(begin.handoff_id, &begin.target_token, remote_process,)
                .unwrap()
                .as_slice(),
            [CoordinatorCommand::ActivateTarget { .. }]
        ));
        coordinator.snapshot_staged(begin.handoff_id).unwrap();
        coordinator
            .applied(begin.handoff_id, snapshot(server_process, 1).snapshot_id)
            .unwrap();
        assert_eq!(coordinator.current_token(), &begin.target_token);
        assert_eq!(
            coordinator.last_terminal(),
            Some(&(begin.handoff_id, Ok(())))
        );

        let fallback = coordinator.begin_handoff(server).unwrap();
        assert!(fallback.handoff_id.handoff_epoch > begin.handoff_id.handoff_epoch);
        assert!(fallback.target_token.ownership_epoch > begin.target_token.ownership_epoch);
    }
}
