use crate::domain::{
    ActiveSession, EnterRequest, GrantIdentity, Host, LeaseIdentity, PeerReadiness, ProtocolPhase,
    ProtocolState, RequestStatus, SignalObservation, TvMode, WsState,
};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, Copy)]
pub struct ProtocolTiming {
    pub command_ms: u64,
    pub observation_ms: u64,
    pub grant_ms: u64,
    pub wake_ms: u64,
    pub lease_ms: u64,
    pub signal_poll_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    TransportConnecting,
    TransportRegistering,
    TransportSubscribed,
    TransportSynchronized {
        mode: TvMode,
        input: Option<Host>,
        signals: BTreeMap<Host, bool>,
    },
    TransportDisconnected {
        reason: String,
    },
    PeerReadinessUpdated {
        host: Host,
        readiness: PeerReadiness,
    },
    ArmManualRecovery {
        target: Host,
    },
    CreateEnter {
        request_id: String,
        client_id: String,
        target: Host,
        lease: LeaseIdentity,
    },
    CommandAcknowledged {
        switch_epoch: u64,
        target: Host,
    },
    CommandFailed {
        switch_epoch: u64,
        reason: String,
    },
    Observation {
        switch_epoch: u64,
        mode: TvMode,
        input: Option<Host>,
        signals: BTreeMap<Host, bool>,
    },
    Commit {
        request_id: String,
        request_epoch: u64,
        grant_epoch: u64,
        lease_id: String,
        lease_epoch: u64,
    },
    Cancel {
        request_id: String,
        reason: String,
    },
    Renew {
        request_id: String,
        lease_id: String,
        lease_epoch: u64,
        peer_session_epoch: u64,
    },
    MultiViewRequested {
        enabled: bool,
    },
    MultiViewAcknowledged {
        switch_epoch: u64,
        enabled: bool,
    },
    SubscriptionObserved {
        mode: TvMode,
        input: Option<Host>,
    },
    Tick,
    Shutdown,
}

impl Event {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::TransportConnecting => "transport_connecting",
            Self::TransportRegistering => "transport_registering",
            Self::TransportSubscribed => "transport_subscribed",
            Self::TransportSynchronized { .. } => "transport_synchronized",
            Self::TransportDisconnected { .. } => "transport_disconnected",
            Self::PeerReadinessUpdated { .. } => "peer_readiness_updated",
            Self::ArmManualRecovery { .. } => "arm_manual_recovery",
            Self::CreateEnter { .. } => "create_enter",
            Self::CommandAcknowledged { .. } => "command_acknowledged",
            Self::CommandFailed { .. } => "command_failed",
            Self::Observation { .. } => "observation",
            Self::Commit { .. } => "commit",
            Self::Cancel { .. } => "cancel",
            Self::Renew { .. } => "renew",
            Self::MultiViewRequested { .. } => "multiview_requested",
            Self::MultiViewAcknowledged { .. } => "multiview_acknowledged",
            Self::SubscriptionObserved { .. } => "subscription_observed",
            Self::Tick => "tick",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    SetInput {
        target: Host,
        switch_epoch: u64,
        fallback: bool,
    },
    Observe {
        switch_epoch: u64,
    },
    Wake {
        target: Host,
        request_epoch: u64,
    },
    SetMultiView {
        enabled: bool,
        switch_epoch: u64,
    },
}

#[derive(Debug, Clone)]
pub struct Transition {
    pub next: ProtocolState,
    pub effects: Vec<Effect>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("daemon is not synchronized or fallback is unresolved")]
    Unavailable,
    #[error("request conflicts with active request {active_request_id}")]
    Busy { active_request_id: String },
    #[error("request id already exists with different identity")]
    RequestIdentityConflict,
    #[error("bundle lease is invalid or expired")]
    InvalidLease,
    #[error("request not found")]
    RequestNotFound,
    #[error("request, grant, or lease identity is stale")]
    StaleIdentity,
    #[error("multiview change requires local server ownership")]
    InputNotLocal,
    #[error("manual recovery target must be a remote host")]
    InvalidManualRecoveryTarget,
    #[error("protocol invariant failed: {0}")]
    Invariant(String),
}

pub fn apply(
    state: &ProtocolState,
    event: Event,
    now_ms: u64,
    timing: ProtocolTiming,
) -> Result<Transition, ProtocolError> {
    let mut next = state.clone();
    let mut effects = Vec::new();

    match event {
        Event::TransportConnecting => {
            next.ws_state = WsState::Connecting;
            next.subscribe_active = false;
            next.synchronized = false;
        }
        Event::TransportRegistering => {
            next.ws_state = WsState::Registering;
            next.subscribe_active = false;
            next.synchronized = false;
        }
        Event::TransportSubscribed => {
            next.ws_state = WsState::Synchronizing;
            next.subscribe_active = true;
            next.synchronized = false;
        }
        Event::TransportSynchronized {
            mode,
            input,
            signals,
        } => {
            next.ws_state = WsState::Connected;
            next.subscribe_active = true;
            next.synchronized = true;
            let switch_epoch = next.switch_epoch;
            store_observation(&mut next, switch_epoch, now_ms, mode, input, signals);
            if server_display_verified(&next, switch_epoch) {
                finish_fallback(&mut next);
            } else if manual_display_observed(&next) {
                accept_manual_display(&mut next);
            } else {
                begin_fallback(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    "startup_or_reconnect_resync",
                );
            }
        }
        Event::TransportDisconnected { reason } => {
            next.ws_state = WsState::Disconnected;
            next.subscribe_active = false;
            next.synchronized = false;
            next.reconnect_total = next.reconnect_total.saturating_add(1);
            next.last_error = Some(reason.clone());
            begin_fallback(&mut next, &mut effects, now_ms, timing, &reason);
        }
        Event::PeerReadinessUpdated { host, readiness } => {
            next.peers.insert(host, readiness.clone());
            let request_target = next.active_request.as_ref().map(|request| request.target);
            if request_target == Some(host) {
                let lease_session = next
                    .active_request
                    .as_ref()
                    .map(|request| request.lease.peer_session_epoch)
                    .unwrap_or_default();
                if next.phase == ProtocolPhase::Waking && readiness.bundle_ready(lease_session) {
                    begin_switch(&mut next, &mut effects, now_ms, timing);
                } else if matches!(
                    next.phase,
                    ProtocolPhase::Switching
                        | ProtocolPhase::Verifying
                        | ProtocolPhase::GrantPending
                ) && !readiness.bundle_ready(lease_session)
                {
                    begin_fallback(
                        &mut next,
                        &mut effects,
                        now_ms,
                        timing,
                        "target_readiness_lost",
                    );
                }
            }
            if next.keyboard_owner == host && host != next.server_host {
                let active_session_epoch = next
                    .active_session
                    .as_ref()
                    .map(|session| session.lease.peer_session_epoch)
                    .unwrap_or_default();
                if !readiness.bundle_ready(active_session_epoch) {
                    begin_active_session_failure_fallback(
                        &mut next,
                        &mut effects,
                        now_ms,
                        timing,
                        "active_peer_readiness_lost",
                    );
                }
            }
        }
        Event::ArmManualRecovery { target } => {
            if target == next.server_host {
                return Err(ProtocolError::InvalidManualRecoveryTarget);
            }
            if !state.ready()
                || state.phase != ProtocolPhase::Idle
                || state.keyboard_owner != state.server_host
                || state.pointer_owner != state.server_host
                || state.active_request.is_some()
                || state.active_session.is_some()
                || !server_display_verified(state, state.switch_epoch)
            {
                return Err(ProtocolError::Unavailable);
            }
            next.manual_recovery_target = Some(target);
        }
        Event::CreateEnter {
            request_id,
            client_id,
            target,
            lease,
        } => {
            if let Some(existing) = state.request(&request_id) {
                if existing.client_id == client_id
                    && existing.target == target
                    && existing.lease.lease_id == lease.lease_id
                    && existing.lease.lease_epoch == lease.lease_epoch
                {
                    return Ok(Transition {
                        next,
                        effects: Vec::new(),
                    });
                }
                return Err(ProtocolError::RequestIdentityConflict);
            }
            if let Some(active) = &state.active_request {
                return Err(ProtocolError::Busy {
                    active_request_id: active.request_id.clone(),
                });
            }
            if !state.ready() {
                return Err(ProtocolError::Unavailable);
            }
            if lease.lease_id.is_empty() || lease.expires_at_ms <= now_ms {
                return Err(ProtocolError::InvalidLease);
            }

            next.manual_recovery_target = None;
            next.request_epoch = next.request_epoch.saturating_add(1);
            let request_epoch = next.request_epoch;
            let mut request = EnterRequest {
                request_id,
                client_id,
                target,
                request_epoch,
                lease,
                status: RequestStatus::Switching,
                switch_epoch: None,
                grant: None,
                deadline_ms: now_ms.saturating_add(timing.command_ms),
                reason: None,
            };

            if target == next.server_host {
                release_to_server(&mut next);
            } else {
                let readiness = next.peers.get(&target).cloned().unwrap_or_default();
                if !readiness.online {
                    request.status = RequestStatus::Waking;
                    request.deadline_ms = now_ms.saturating_add(timing.wake_ms);
                    next.phase = ProtocolPhase::Waking;
                    next.phase_deadline_ms = Some(request.deadline_ms);
                    effects.push(Effect::Wake {
                        target,
                        request_epoch,
                    });
                    next.active_request = Some(request);
                    return validated(next, effects, now_ms);
                }
                if !readiness.bundle_ready(request.lease.peer_session_epoch) {
                    request.status = RequestStatus::Denied;
                    request.reason = Some("target_input_not_ready".to_string());
                    next.manual_recovery_target = Some(target);
                    next.archive_request(request);
                    return validated(next, effects, now_ms);
                }
            }

            next.active_request = Some(request);
            begin_switch(&mut next, &mut effects, now_ms, timing);
        }
        Event::CommandAcknowledged {
            switch_epoch,
            target,
        } => {
            if next.switch_epoch != switch_epoch || next.commanded_input != Some(target) {
                return Ok(Transition { next, effects });
            }
            if next.observation_in_flight.is_none()
                && matches!(
                    next.phase,
                    ProtocolPhase::Switching | ProtocolPhase::FallbackCommandPending
                )
            {
                next.phase = if next.fallback_required {
                    ProtocolPhase::FallbackVerifying
                } else {
                    ProtocolPhase::Verifying
                };
                request_observation(&mut next, &mut effects, now_ms, timing, switch_epoch);
            }
        }
        Event::CommandFailed {
            switch_epoch,
            reason,
        } => {
            if next.switch_epoch == switch_epoch {
                next.last_error = Some(reason.clone());
                begin_fallback(&mut next, &mut effects, now_ms, timing, &reason);
            }
        }
        Event::Observation {
            switch_epoch,
            mode,
            input,
            signals,
        } => {
            store_observation(&mut next, switch_epoch, now_ms, mode, input, signals);
            if next.observation_in_flight == Some(switch_epoch) {
                next.observation_in_flight = None;
            }

            if next.fallback_required && next.switch_epoch == switch_epoch {
                if server_display_verified(&next, switch_epoch) {
                    finish_fallback(&mut next);
                } else {
                    next.phase = ProtocolPhase::FallbackVerifying;
                }
            } else if let Some(request) = next.active_request.as_ref() {
                if request.switch_epoch != Some(switch_epoch) {
                    return validated(next, effects, now_ms);
                }
                let target = request.target;
                let verified = target_display_verified(&next, target, switch_epoch);
                match next.phase {
                    ProtocolPhase::Verifying if verified => {
                        if target != next.server_host {
                            let ready = next.peers.get(&target).is_some_and(|peer| {
                                peer.bundle_ready(request.lease.peer_session_epoch)
                            });
                            if !ready || request.lease.expires_at_ms <= now_ms {
                                begin_fallback(
                                    &mut next,
                                    &mut effects,
                                    now_ms,
                                    timing,
                                    "lease_or_readiness_lost_before_grant",
                                );
                            } else {
                                issue_grant(&mut next, now_ms, timing);
                            }
                        } else {
                            issue_grant(&mut next, now_ms, timing);
                        }
                    }
                    ProtocolPhase::GrantPending if !verified => {
                        begin_fallback(
                            &mut next,
                            &mut effects,
                            now_ms,
                            timing,
                            "target_observation_failed",
                        );
                    }
                    _ => {}
                }
            } else if next.phase == ProtocolPhase::RemoteOwned {
                if let Some(session) = &next.active_session {
                    if session.switch_epoch == switch_epoch
                        && !target_display_verified(&next, session.target, switch_epoch)
                    {
                        begin_active_session_failure_fallback(
                            &mut next,
                            &mut effects,
                            now_ms,
                            timing,
                            "active_signal_lost",
                        );
                    } else {
                        next.phase_deadline_ms = None;
                        next.next_signal_poll_ms =
                            Some(now_ms.saturating_add(timing.signal_poll_ms));
                    }
                }
            } else if next.phase == ProtocolPhase::MultiviewChanging {
                complete_multiview_observation(&mut next, &mut effects, now_ms, timing);
            }
        }
        Event::Commit {
            request_id,
            request_epoch,
            grant_epoch,
            lease_id,
            lease_epoch,
        } => {
            if let Some(committed) = next.request_history.iter().rev().find(|request| {
                request.request_id == request_id && request.status == RequestStatus::Committed
            }) {
                if committed.request_epoch == request_epoch
                    && committed.lease.lease_id == lease_id
                    && committed.lease.lease_epoch == lease_epoch
                    && committed.grant.as_ref().map(|grant| grant.grant_epoch) == Some(grant_epoch)
                {
                    return validated(next, effects, now_ms);
                }
                return Err(ProtocolError::StaleIdentity);
            }
            let request = next
                .active_request
                .as_ref()
                .ok_or(ProtocolError::RequestNotFound)?;
            let grant = request.grant.as_ref().ok_or(ProtocolError::StaleIdentity)?;
            if request.request_id != request_id
                || request.request_epoch != request_epoch
                || request.status != RequestStatus::Grant
                || request.lease.lease_id != lease_id
                || request.lease.lease_epoch != lease_epoch
                || grant.grant_epoch != grant_epoch
                || grant.expires_at_ms <= now_ms
                || request.lease.expires_at_ms <= now_ms
            {
                return Err(ProtocolError::StaleIdentity);
            }
            if request.target != next.server_host
                && !next
                    .peers
                    .get(&request.target)
                    .is_some_and(|peer| peer.bundle_ready(request.lease.peer_session_epoch))
            {
                begin_fallback(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    "readiness_lost_before_commit",
                );
            } else {
                let mut request = next.active_request.take().expect("checked request");
                let switch_epoch = request.switch_epoch.expect("grant switch epoch");
                request.status = RequestStatus::Committed;
                request.reason = None;
                request.lease.expires_at_ms = now_ms.saturating_add(timing.lease_ms);
                let target = request.target;
                next.keyboard_owner = target;
                next.pointer_owner = target;
                next.fallback_required = false;
                next.fallback_reason = None;
                next.manual_recovery_target = None;
                next.phase_deadline_ms = None;
                next.observation_in_flight = None;
                if target == next.server_host {
                    next.active_session = None;
                    next.phase = ProtocolPhase::Idle;
                    next.next_signal_poll_ms = None;
                } else {
                    next.active_session = Some(ActiveSession {
                        request_id: request.request_id.clone(),
                        target,
                        request_epoch,
                        switch_epoch,
                        lease: request.lease.clone(),
                        renewed_until_ms: now_ms.saturating_add(timing.lease_ms),
                    });
                    next.phase = ProtocolPhase::RemoteOwned;
                    next.next_signal_poll_ms = next
                        .has_display_route(target)
                        .then(|| now_ms.saturating_add(timing.signal_poll_ms));
                }
                next.archive_request(request);
            }
        }
        Event::Cancel { request_id, reason } => {
            if next.request_history.iter().rev().any(|request| {
                request.request_id == request_id
                    && matches!(
                        request.status,
                        RequestStatus::Cancelled | RequestStatus::Fallback
                    )
            }) {
                return validated(next, effects, now_ms);
            }
            if next
                .active_session
                .as_ref()
                .is_some_and(|session| session.request_id == request_id)
            {
                let request = next
                    .archived_request_mut(&request_id)
                    .ok_or(ProtocolError::RequestNotFound)?;
                request.status = RequestStatus::Cancelled;
                request.reason = Some(reason.clone());
                begin_fallback(&mut next, &mut effects, now_ms, timing, &reason);
            } else {
                let request = next
                    .active_request
                    .as_ref()
                    .ok_or(ProtocolError::RequestNotFound)?;
                if request.request_id != request_id {
                    return Err(ProtocolError::RequestNotFound);
                }
                let tv_may_have_changed = request.switch_epoch.is_some();
                let mut request = next.active_request.take().expect("checked request");
                if request.target != next.server_host {
                    next.manual_recovery_target = Some(request.target);
                }
                request.status = RequestStatus::Cancelled;
                request.reason = Some(reason.clone());
                next.archive_request(request);
                if tv_may_have_changed {
                    begin_fallback(&mut next, &mut effects, now_ms, timing, &reason);
                } else {
                    next.phase = ProtocolPhase::Idle;
                    next.phase_deadline_ms = None;
                }
            }
        }
        Event::Renew {
            request_id,
            lease_id,
            lease_epoch,
            peer_session_epoch,
        } => {
            let valid = next.active_session.as_ref().is_some_and(|session| {
                session.request_id == request_id
                    && session.lease.lease_id == lease_id
                    && session.lease.lease_epoch == lease_epoch
                    && session.lease.peer_session_epoch == peer_session_epoch
                    && next
                        .peers
                        .get(&session.target)
                        .is_some_and(|peer| peer.bundle_ready(peer_session_epoch))
            });
            if !valid {
                begin_active_session_failure_fallback(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    "lease_renewal_rejected",
                );
                return validated(next, effects, now_ms);
            }
            let session = next.active_session.as_mut().expect("validated session");
            let expires = now_ms.saturating_add(timing.lease_ms);
            session.renewed_until_ms = expires;
            session.lease.expires_at_ms = expires;
        }
        Event::MultiViewRequested { enabled } => {
            if !next.ready() {
                return Err(ProtocolError::Unavailable);
            }
            if next.keyboard_owner != next.server_host || next.pointer_owner != next.server_host {
                return Err(ProtocolError::InputNotLocal);
            }
            if next.active_request.is_some() || next.pending_multiview.is_some() {
                return Err(ProtocolError::Busy {
                    active_request_id: next
                        .active_request
                        .as_ref()
                        .map(|request| request.request_id.clone())
                        .unwrap_or_else(|| "multiview".to_string()),
                });
            }
            next.switch_epoch = next.switch_epoch.saturating_add(1);
            next.pending_multiview = Some(enabled);
            next.phase = ProtocolPhase::MultiviewChanging;
            next.phase_deadline_ms = Some(now_ms.saturating_add(timing.command_ms));
            effects.push(Effect::SetMultiView {
                enabled,
                switch_epoch: next.switch_epoch,
            });
        }
        Event::MultiViewAcknowledged {
            switch_epoch,
            enabled,
        } => {
            if next.switch_epoch == switch_epoch && next.pending_multiview == Some(enabled) {
                request_observation(&mut next, &mut effects, now_ms, timing, switch_epoch);
                next.phase = ProtocolPhase::MultiviewChanging;
            }
        }
        Event::SubscriptionObserved { mode, input } => {
            next.tv_mode = mode;
            next.observed_input = input;
            let active_target = next.active_request.as_ref().map(|request| request.target);
            let session_target = next.active_session.as_ref().map(|session| session.target);
            let target_mismatch =
                |target| mode != TvMode::Fullscreen || input != Some(target);

            // Distinct protocol phases intentionally converge on shared effects.
            #[allow(clippy::if_same_then_else)]
            if next.fallback_required
                && next.phase == ProtocolPhase::FallbackVerifying
                && mode == TvMode::Fullscreen
                && input == Some(next.server_host)
                && next.observation_in_flight.is_none()
            {
                let switch_epoch = next.switch_epoch;
                request_observation_preserving_deadline(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    switch_epoch,
                );
            } else if next.phase == ProtocolPhase::Verifying
                && active_target
                    .is_some_and(|target| mode == TvMode::Fullscreen && input == Some(target))
                && next.observation_in_flight.is_none()
            {
                let switch_epoch = next.switch_epoch;
                request_observation_preserving_deadline(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    switch_epoch,
                );
            } else if next.phase == ProtocolPhase::GrantPending
                && active_target.is_some_and(|target| {
                    next.has_display_route(target) && target_mismatch(target)
                })
            {
                begin_fallback(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    "unexpected_tv_subscription",
                );
            } else if session_target.is_some_and(|target| {
                next.has_display_route(target) && target_mismatch(target)
            }) {
                begin_fallback(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    "unexpected_tv_subscription",
                );
            } else if manual_display_observed(&next) {
                accept_manual_display(&mut next);
            } else if next.phase == ProtocolPhase::Idle
                && mode == TvMode::Fullscreen
                && input == Some(next.server_host)
            {
                next.manual_recovery_target = None;
            } else if next.phase == ProtocolPhase::Idle
                && (mode != TvMode::Fullscreen
                    || input.is_some_and(|observed| observed != next.server_host))
            {
                begin_fallback(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    "manual_tv_override",
                );
            }
        }
        Event::Tick => {
            if let Some(request) = next.active_request.as_ref() {
                if request.deadline_ms <= now_ms {
                    let waking = request.status == RequestStatus::Waking;
                    let mut request = next.active_request.take().expect("active request");
                    if request.target != next.server_host {
                        next.manual_recovery_target = Some(request.target);
                    }
                    request.status = RequestStatus::Expired;
                    request.reason = Some("request_deadline".to_string());
                    next.archive_request(request);
                    if waking {
                        next.phase = ProtocolPhase::Idle;
                        next.phase_deadline_ms = None;
                    } else {
                        begin_fallback(&mut next, &mut effects, now_ms, timing, "request_deadline");
                    }
                }
            }
            if next.active_session.as_ref().is_some_and(|session| {
                session.renewed_until_ms <= now_ms || session.lease.expires_at_ms <= now_ms
            }) {
                begin_active_session_failure_fallback(
                    &mut next,
                    &mut effects,
                    now_ms,
                    timing,
                    "active_lease_expired",
                );
            } else if next.phase == ProtocolPhase::RemoteOwned
                && next.observation_in_flight.is_none()
                && next
                    .next_signal_poll_ms
                    .is_some_and(|deadline| deadline <= now_ms)
            {
                let epoch = next
                    .active_session
                    .as_ref()
                    .map(|session| session.switch_epoch)
                    .expect("remote owned session");
                request_observation(&mut next, &mut effects, now_ms, timing, epoch);
            } else if next.observation_in_flight.is_some()
                && next
                    .phase_deadline_ms
                    .is_some_and(|deadline| deadline <= now_ms)
            {
                let reason = verification_timeout_reason(&next);
                if next.active_session.is_some() {
                    begin_active_session_failure_fallback(
                        &mut next,
                        &mut effects,
                        now_ms,
                        timing,
                        reason,
                    );
                } else {
                    begin_fallback(&mut next, &mut effects, now_ms, timing, reason);
                }
            } else if next.phase == ProtocolPhase::Verifying
                && next
                    .phase_deadline_ms
                    .is_some_and(|deadline| deadline <= now_ms)
            {
                let reason = verification_timeout_reason(&next);
                begin_fallback(&mut next, &mut effects, now_ms, timing, reason);
            } else if (matches!(
                next.phase,
                ProtocolPhase::FallbackCommandPending | ProtocolPhase::FallbackVerifying
            ) && next
                .phase_deadline_ms
                .is_some_and(|deadline| deadline <= now_ms))
                || (next.phase == ProtocolPhase::FallbackDeferred && next.daemon_healthy())
            {
                issue_fallback_command(&mut next, &mut effects, now_ms, timing);
            } else if next.phase == ProtocolPhase::MultiviewChanging
                && next
                    .phase_deadline_ms
                    .is_some_and(|deadline| deadline <= now_ms)
            {
                begin_fallback(&mut next, &mut effects, now_ms, timing, "multiview_timeout");
            }
        }
        Event::Shutdown => {
            next.ws_state = WsState::Disconnected;
            next.subscribe_active = false;
            next.synchronized = false;
            release_to_server(&mut next);
            next.fallback_required = true;
            next.fallback_reason = Some("shutdown".to_string());
            next.phase = ProtocolPhase::FallbackDeferred;
            next.active_request = None;
            next.pending_multiview = None;
            next.phase_deadline_ms = None;
        }
    }

    validated(next, effects, now_ms)
}

fn begin_switch(
    state: &mut ProtocolState,
    effects: &mut Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
) {
    state.switch_epoch = state.switch_epoch.saturating_add(1);
    let switch_epoch = state.switch_epoch;
    let target = {
        let request = state.active_request.as_mut().expect("active request");
        request.status = RequestStatus::Switching;
        request.switch_epoch = Some(switch_epoch);
        request.deadline_ms = now_ms
            .saturating_add(timing.command_ms)
            .saturating_add(timing.observation_ms);
        request.target
    };
    if !state.has_display_route(target) {
        state.commanded_input = None;
        state.phase_deadline_ms = None;
        issue_grant(state, now_ms, timing);
        return;
    }
    state.commanded_input = Some(target);
    state.phase = ProtocolPhase::Switching;
    state.phase_deadline_ms = Some(now_ms.saturating_add(timing.command_ms));
    effects.push(Effect::SetInput {
        target,
        switch_epoch,
        fallback: false,
    });
}

fn request_observation(
    state: &mut ProtocolState,
    effects: &mut Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
    switch_epoch: u64,
) {
    state.observation_in_flight = Some(switch_epoch);
    state.next_signal_poll_ms = None;
    state.phase_deadline_ms = Some(now_ms.saturating_add(timing.observation_ms));
    effects.push(Effect::Observe { switch_epoch });
}

fn request_observation_preserving_deadline(
    state: &mut ProtocolState,
    effects: &mut Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
    switch_epoch: u64,
) {
    let deadline = state.phase_deadline_ms;
    request_observation(state, effects, now_ms, timing, switch_epoch);
    state.phase_deadline_ms = deadline;
}

fn issue_grant(state: &mut ProtocolState, now_ms: u64, timing: ProtocolTiming) {
    state.grant_epoch = state.grant_epoch.saturating_add(1);
    let request = state.active_request.as_mut().expect("active request");
    let target = request.target;
    let switch_epoch = request.switch_epoch.expect("switch epoch");
    let expires_at_ms = now_ms
        .saturating_add(timing.grant_ms)
        .min(request.lease.expires_at_ms);
    request.status = RequestStatus::Grant;
    request.deadline_ms = expires_at_ms;
    request.grant = Some(GrantIdentity {
        grant_epoch: state.grant_epoch,
        switch_epoch,
        expires_at_ms,
    });
    state.verified_epoch = Some(switch_epoch);
    state.phase = ProtocolPhase::GrantPending;
    state.phase_deadline_ms = Some(expires_at_ms);
    record_verified_switch(state, target);
}

fn begin_fallback(
    state: &mut ProtocolState,
    effects: &mut Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
    reason: &str,
) {
    let restore_display = display_restore_required(state);
    if let Some(target) = state
        .active_request
        .as_ref()
        .map(|request| request.target)
        .filter(|target| *target != state.server_host && state.has_display_route(*target))
    {
        state.manual_recovery_target = Some(target);
    }
    let committed_request_id = state
        .active_session
        .as_ref()
        .map(|session| session.request_id.clone());
    if let Some(request_id) = committed_request_id {
        if let Some(request) = state.archived_request_mut(&request_id) {
            if request.status == RequestStatus::Committed {
                request.status = RequestStatus::Fallback;
                request.reason = Some(reason.to_string());
            }
        }
    }
    release_to_server(state);
    state.fallback_required = restore_display;
    state.fallback_reason = Some(reason.to_string());
    state.verified_epoch = None;
    state.pending_multiview = None;
    state.observation_in_flight = None;
    state.next_signal_poll_ms = None;
    if let Some(mut request) = state.active_request.take() {
        request.status = RequestStatus::Fallback;
        request.reason = Some(reason.to_string());
        state.archive_request(request);
    }
    if restore_display && state.daemon_healthy() {
        issue_fallback_command(state, effects, now_ms, timing);
    } else if restore_display || !state.daemon_healthy() {
        state.phase = ProtocolPhase::FallbackDeferred;
        state.phase_deadline_ms = None;
    } else {
        state.fallback_reason = None;
        state.phase = ProtocolPhase::Idle;
        state.phase_deadline_ms = None;
    }
}

fn begin_active_session_failure_fallback(
    state: &mut ProtocolState,
    effects: &mut Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
    reason: &str,
) {
    if let Some(target) = state
        .active_session
        .as_ref()
        .map(|session| session.target)
        .filter(|target| *target != state.server_host && state.has_display_route(*target))
    {
        state.manual_recovery_target = Some(target);
    }
    begin_fallback(state, effects, now_ms, timing, reason);
}

fn issue_fallback_command(
    state: &mut ProtocolState,
    effects: &mut Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
) {
    state.switch_epoch = state.switch_epoch.saturating_add(1);
    state.commanded_input = Some(state.server_host);
    state.phase = ProtocolPhase::FallbackCommandPending;
    state.phase_deadline_ms = Some(now_ms.saturating_add(timing.command_ms));
    effects.push(Effect::SetInput {
        target: state.server_host,
        switch_epoch: state.switch_epoch,
        fallback: true,
    });
}

fn finish_fallback(state: &mut ProtocolState) {
    release_to_server(state);
    state.fallback_required = false;
    state.fallback_reason = None;
    state.phase = ProtocolPhase::Idle;
    state.phase_deadline_ms = None;
    state.observation_in_flight = None;
    state.next_signal_poll_ms = None;
    state.verified_epoch = Some(state.switch_epoch);
    record_verified_switch(state, state.server_host);
}

fn manual_display_observed(state: &ProtocolState) -> bool {
    (state.tv_mode != TvMode::Fullscreen || state.observed_input != Some(state.server_host))
        && state.keyboard_owner == state.server_host
        && state.pointer_owner == state.server_host
        && state.active_request.is_none()
        && state.active_session.is_none()
}

fn accept_manual_display(state: &mut ProtocolState) {
    release_to_server(state);
    state.commanded_input = None;
    state.manual_recovery_target = None;
    state.fallback_required = false;
    state.fallback_reason = None;
    state.pending_multiview = None;
    state.phase = ProtocolPhase::Idle;
    state.phase_deadline_ms = None;
    state.observation_in_flight = None;
    state.next_signal_poll_ms = None;
}

fn display_restore_required(state: &ProtocolState) -> bool {
    state.fallback_required
        || state.pending_multiview.is_some()
        || state.active_request.as_ref().is_some_and(|request| {
            request.switch_epoch.is_some() && state.has_display_route(request.target)
        })
        || state
            .active_session
            .as_ref()
            .is_some_and(|session| state.has_display_route(session.target))
}

fn record_verified_switch(state: &mut ProtocolState, target: Host) {
    if state.switch_epoch == 0 {
        return;
    }
    let count = state
        .switch_count
        .get_mut(&target)
        .expect("all host counters initialized");
    *count = count.saturating_add(1);
}

fn release_to_server(state: &mut ProtocolState) {
    state.keyboard_owner = state.server_host;
    state.pointer_owner = state.server_host;
    state.active_session = None;
}

fn store_observation(
    state: &mut ProtocolState,
    switch_epoch: u64,
    now_ms: u64,
    mode: TvMode,
    input: Option<Host>,
    signals: BTreeMap<Host, bool>,
) {
    state.tv_mode = mode;
    state.observed_input = input;
    for host in Host::ALL {
        if let Some(present) = signals.get(&host) {
            state.input_signal.insert(
                host,
                SignalObservation {
                    present: *present,
                    switch_epoch,
                    observed_at_ms: now_ms,
                },
            );
        }
    }
}

fn target_display_verified(state: &ProtocolState, target: Host, switch_epoch: u64) -> bool {
    state.tv_mode == TvMode::Fullscreen
        && state.observed_input == Some(target)
        && state
            .input_signal
            .get(&target)
            .is_some_and(|signal| signal.present && signal.switch_epoch == switch_epoch)
}

fn target_verification_failure_reason(
    state: &ProtocolState,
    target: Host,
    switch_epoch: u64,
) -> &'static str {
    if state.tv_mode != TvMode::Fullscreen {
        "target_not_fullscreen"
    } else if state.observed_input != Some(target) {
        "target_input_not_observed"
    } else {
        match state.input_signal.get(&target) {
            Some(signal) if signal.switch_epoch == switch_epoch && !signal.present => {
                "target_signal_absent"
            }
            Some(signal) if signal.switch_epoch != switch_epoch => "target_signal_stale",
            _ => "target_signal_not_observed",
        }
    }
}

fn verification_timeout_reason(state: &ProtocolState) -> &'static str {
    if state.phase != ProtocolPhase::Verifying {
        return "observation_timeout";
    }
    state
        .active_request
        .as_ref()
        .map(|request| {
            target_verification_failure_reason(
                state,
                request.target,
                request.switch_epoch.unwrap_or(state.switch_epoch),
            )
        })
        .unwrap_or("target_verification_timeout")
}

fn server_display_verified(state: &ProtocolState, switch_epoch: u64) -> bool {
    target_display_verified(state, state.server_host, switch_epoch)
}

fn complete_multiview_observation(
    state: &mut ProtocolState,
    effects: &mut Vec<Effect>,
    now_ms: u64,
    timing: ProtocolTiming,
) {
    let Some(enabled) = state.pending_multiview else {
        return;
    };
    let expected = if enabled {
        TvMode::Multiview
    } else {
        TvMode::Fullscreen
    };
    if state.tv_mode != expected {
        begin_fallback(
            state,
            effects,
            now_ms,
            timing,
            "multiview_observation_mismatch",
        );
        return;
    }
    if enabled {
        state.pending_multiview = None;
        state.phase = ProtocolPhase::Idle;
        state.phase_deadline_ms = None;
    } else {
        begin_fallback(
            state,
            effects,
            now_ms,
            timing,
            "multiview_exit_server_verify",
        );
    }
}

fn validated(
    next: ProtocolState,
    effects: Vec<Effect>,
    now_ms: u64,
) -> Result<Transition, ProtocolError> {
    next.validate(now_ms)
        .map_err(|error| ProtocolError::Invariant(error.to_string()))?;
    Ok(Transition { next, effects })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMING: ProtocolTiming = ProtocolTiming {
        command_ms: 100,
        observation_ms: 100,
        grant_ms: 100,
        wake_ms: 500,
        lease_ms: 300,
        signal_poll_ms: 100,
    };

    fn signals(host: Host) -> BTreeMap<Host, bool> {
        Host::ALL
            .into_iter()
            .map(|candidate| (candidate, candidate == host))
            .collect()
    }

    fn synchronized() -> ProtocolState {
        synchronized_with_limit(32)
    }

    fn synchronized_with_limit(retained_request_limit: usize) -> ProtocolState {
        let state = ProtocolState::new(Host::Linux, retained_request_limit);
        apply(
            &state,
            Event::TransportSynchronized {
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
                signals: signals(Host::Linux),
            },
            1,
            TIMING,
        )
        .unwrap()
        .next
    }

    fn synchronized_without_windows_display() -> ProtocolState {
        let state = ProtocolState::with_display_hosts(
            Host::Linux,
            32,
            [Host::Linux, Host::Mac],
        );
        apply(
            &state,
            Event::TransportSynchronized {
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
                signals: signals(Host::Linux),
            },
            1,
            TIMING,
        )
        .unwrap()
        .next
    }

    fn archived_request(request_id: &str, request_epoch: u64) -> EnterRequest {
        EnterRequest {
            request_id: request_id.to_string(),
            client_id: "hub".to_string(),
            target: Host::Mac,
            request_epoch,
            lease: LeaseIdentity {
                lease_id: format!("lease-{request_id}"),
                lease_epoch: request_epoch,
                peer_session_epoch: 11,
                expires_at_ms: 1_000,
            },
            status: RequestStatus::Cancelled,
            switch_epoch: None,
            grant: None,
            deadline_ms: 100,
            reason: Some("test".to_string()),
        }
    }

    fn ready_peer(state: &ProtocolState, host: Host, session_epoch: u64) -> ProtocolState {
        apply(
            state,
            Event::PeerReadinessUpdated {
                host,
                readiness: PeerReadiness {
                    online: true,
                    keyboard_ready: true,
                    pointer_ready: true,
                    session_epoch,
                    observed_at_ms: 2,
                },
            },
            2,
            TIMING,
        )
        .unwrap()
        .next
    }

    fn lease(session_epoch: u64) -> LeaseIdentity {
        LeaseIdentity {
            lease_id: "lease-1".to_string(),
            lease_epoch: 7,
            peer_session_epoch: session_epoch,
            expires_at_ms: 1_000,
        }
    }

    fn create_mac(state: &ProtocolState) -> Result<Transition, ProtocolError> {
        apply(
            state,
            Event::CreateEnter {
                request_id: "request-1".to_string(),
                client_id: "hub".to_string(),
                target: Host::Mac,
                lease: lease(11),
            },
            10,
            TIMING,
        )
    }

    fn acknowledge_switch(state: &ProtocolState, now_ms: u64) -> ProtocolState {
        apply(
            state,
            Event::CommandAcknowledged {
                switch_epoch: state.switch_epoch,
                target: state.commanded_input.expect("commanded target"),
            },
            now_ms,
            TIMING,
        )
        .unwrap()
        .next
    }

    fn grant_mac(state: &ProtocolState) -> ProtocolState {
        let switching = create_mac(state).unwrap().next;
        let verifying = acknowledge_switch(&switching, 11);
        apply(
            &verifying,
            Event::Observation {
                switch_epoch: verifying.switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: signals(Host::Mac),
            },
            20,
            TIMING,
        )
        .unwrap()
        .next
    }

    fn remote_owned() -> ProtocolState {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let granted = grant_mac(&state);
        let request = granted.active_request.as_ref().unwrap();
        apply(
            &granted,
            Event::Commit {
                request_id: request.request_id.clone(),
                request_epoch: request.request_epoch,
                grant_epoch: request.grant.as_ref().unwrap().grant_epoch,
                lease_id: request.lease.lease_id.clone(),
                lease_epoch: request.lease.lease_epoch,
            },
            30,
            TIMING,
        )
        .unwrap()
        .next
    }

    #[test]
    fn startup_is_local_and_not_ready_until_server_is_observed() {
        let state = ProtocolState::new(Host::Linux, 32);
        assert_eq!(state.keyboard_owner, Host::Linux);
        assert_eq!(state.pointer_owner, Host::Linux);
        assert!(state.fallback_required);
        assert!(!state.ready());
    }

    #[test]
    fn server_owned_manual_display_selection_never_commands_the_tv() {
        let state = synchronized();
        let transition = apply(
            &state,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: None,
            },
            10,
            TIMING,
        )
        .unwrap();

        assert!(transition.effects.is_empty());
        assert_eq!(transition.next.phase, ProtocolPhase::Idle);
        assert_eq!(transition.next.observed_input, None);
        assert_eq!(transition.next.keyboard_owner, Host::Linux);
        assert_eq!(transition.next.pointer_owner, Host::Linux);
        assert!(!transition.next.fallback_required);
    }

    #[test]
    fn peer_without_display_route_transfers_input_without_touching_tv() {
        let state = ready_peer(&synchronized_without_windows_display(), Host::Windows, 11);
        let prepared = apply(
            &state,
            Event::CreateEnter {
                request_id: "request-windows".to_string(),
                client_id: "hub".to_string(),
                target: Host::Windows,
                lease: lease(11),
            },
            10,
            TIMING,
        )
        .unwrap();

        assert!(prepared.effects.is_empty());
        assert_eq!(prepared.next.phase, ProtocolPhase::GrantPending);
        assert_eq!(prepared.next.commanded_input, None);
        let request = prepared.next.active_request.as_ref().unwrap();
        let committed = apply(
            &prepared.next,
            Event::Commit {
                request_id: request.request_id.clone(),
                request_epoch: request.request_epoch,
                grant_epoch: request.grant.as_ref().unwrap().grant_epoch,
                lease_id: request.lease.lease_id.clone(),
                lease_epoch: request.lease.lease_epoch,
            },
            20,
            TIMING,
        )
        .unwrap()
        .next;
        assert_eq!(committed.keyboard_owner, Host::Windows);
        assert_eq!(committed.pointer_owner, Host::Windows);
        assert_eq!(committed.phase, ProtocolPhase::RemoteOwned);
        assert_eq!(committed.next_signal_poll_ms, None);

        let released = apply(
            &committed,
            Event::PeerReadinessUpdated {
                host: Host::Windows,
                readiness: PeerReadiness::default(),
            },
            30,
            TIMING,
        )
        .unwrap();
        assert!(released.effects.is_empty());
        assert_eq!(released.next.keyboard_owner, Host::Linux);
        assert_eq!(released.next.pointer_owner, Host::Linux);
        assert_eq!(released.next.phase, ProtocolPhase::Idle);
        assert!(!released.next.fallback_required);
    }

    #[test]
    fn retained_request_id_remains_idempotent_after_newer_request() {
        let mut state = synchronized_with_limit(2);
        let first = archived_request("request-old", 1);
        state.archive_request(first.clone());
        state.archive_request(archived_request("request-new", 2));

        let duplicate = apply(
            &state,
            Event::CreateEnter {
                request_id: first.request_id.clone(),
                client_id: first.client_id.clone(),
                target: first.target,
                lease: first.lease.clone(),
            },
            10,
            TIMING,
        )
        .unwrap();
        assert_eq!(duplicate.next, state);
        assert!(duplicate.effects.is_empty());

        let conflict = apply(
            &state,
            Event::CreateEnter {
                request_id: first.request_id,
                client_id: "different-client".to_string(),
                target: first.target,
                lease: first.lease,
            },
            10,
            TIMING,
        );
        assert!(matches!(
            conflict,
            Err(ProtocolError::RequestIdentityConflict)
        ));
    }

    #[test]
    fn request_history_evicts_oldest_at_configured_bound() {
        let mut state = synchronized_with_limit(2);
        state.archive_request(archived_request("request-1", 1));
        state.archive_request(archived_request("request-2", 2));
        state.archive_request(archived_request("request-3", 3));

        assert!(state.request("request-1").is_none());
        assert!(state.request("request-2").is_some());
        assert!(state.request("request-3").is_some());
        assert_eq!(state.request_history.len(), 2);
    }

    #[test]
    fn synchronized_manual_display_choice_is_not_overridden() {
        let state = ProtocolState::new(Host::Linux, 32);
        let transition = apply(
            &state,
            Event::TransportSynchronized {
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: signals(Host::Mac),
            },
            1,
            TIMING,
        )
        .unwrap();
        assert_eq!(transition.next.commanded_input, None);
        assert_eq!(transition.next.observed_input, Some(Host::Mac));
        assert_eq!(transition.next.manual_recovery_target, None);
        assert!(transition.effects.is_empty());
        assert_eq!(transition.next.phase, ProtocolPhase::Idle);
        assert!(!transition.next.fallback_required);
        assert_eq!(transition.next.keyboard_owner, Host::Linux);
        assert_eq!(transition.next.pointer_owner, Host::Linux);
    }

    #[test]
    fn partial_readiness_denies_before_tv_command() {
        let mut state = synchronized();
        state.peers.insert(
            Host::Mac,
            PeerReadiness {
                online: true,
                keyboard_ready: true,
                pointer_ready: false,
                session_epoch: 11,
                observed_at_ms: 2,
            },
        );
        let denied = create_mac(&state).unwrap();
        let request = denied.next.request("request-1").unwrap();
        assert_eq!(request.status, RequestStatus::Denied);
        assert_eq!(request.reason.as_deref(), Some("target_input_not_ready"));
        assert_eq!(denied.next.manual_recovery_target, Some(Host::Mac));
        assert_eq!(denied.next.commanded_input, None);
        assert!(denied.effects.is_empty());
    }

    #[test]
    fn failed_enter_arms_exact_manual_recovery_target_after_server_fallback() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let failed = apply(
            &switching,
            Event::CommandFailed {
                switch_epoch: switching.switch_epoch,
                reason: "switch failed".to_string(),
            },
            11,
            TIMING,
        )
        .unwrap()
        .next;
        assert_eq!(failed.manual_recovery_target, Some(Host::Mac));
        assert!(failed.fallback_required);

        let verifying = acknowledge_switch(&failed, 12);
        let recovered = apply(
            &verifying,
            Event::Observation {
                switch_epoch: verifying.switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
                signals: signals(Host::Linux),
            },
            13,
            TIMING,
        )
        .unwrap()
        .next;
        assert_eq!(recovered.phase, ProtocolPhase::Idle);
        assert!(!recovered.fallback_required);
        assert_eq!(recovered.manual_recovery_target, Some(Host::Mac));

        let manual = apply(
            &recovered,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
            },
            14,
            TIMING,
        )
        .unwrap();
        assert!(manual.effects.is_empty());
        assert_eq!(manual.next.phase, ProtocolPhase::Idle);
        assert!(!manual.next.fallback_required);
        assert_eq!(manual.next.observed_input, Some(Host::Mac));
        assert_eq!(manual.next.keyboard_owner, Host::Linux);
        assert_eq!(manual.next.pointer_owner, Host::Linux);
    }

    #[test]
    fn manual_display_selection_cancels_in_progress_server_fallback() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let fallback = apply(
            &switching,
            Event::CommandFailed {
                switch_epoch: switching.switch_epoch,
                reason: "switch failed".to_string(),
            },
            11,
            TIMING,
        )
        .unwrap()
        .next;
        assert_eq!(fallback.phase, ProtocolPhase::FallbackCommandPending);
        assert_eq!(fallback.keyboard_owner, Host::Linux);
        assert_eq!(fallback.pointer_owner, Host::Linux);
        let stale_fallback_epoch = fallback.switch_epoch;

        let manual = apply(
            &fallback,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: None,
            },
            12,
            TIMING,
        )
        .unwrap();
        assert!(manual.effects.is_empty());
        assert_eq!(manual.next.phase, ProtocolPhase::Idle);
        assert_eq!(manual.next.commanded_input, None);
        assert!(!manual.next.fallback_required);

        let delayed_ack = apply(
            &manual.next,
            Event::CommandAcknowledged {
                switch_epoch: stale_fallback_epoch,
                target: Host::Linux,
            },
            13,
            TIMING,
        )
        .unwrap();
        assert!(delayed_ack.effects.is_empty());
        assert_eq!(delayed_ack.next.phase, ProtocolPhase::Idle);
        assert_eq!(delayed_ack.next.observed_input, None);
    }

    #[test]
    fn manual_display_choice_clears_stale_recovery_target() {
        let mut state = synchronized();
        state.manual_recovery_target = Some(Host::Mac);

        let wrong_target = apply(
            &state,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: Some(Host::Windows),
            },
            10,
            TIMING,
        )
        .unwrap();
        assert!(wrong_target.effects.is_empty());
        assert_eq!(wrong_target.next.phase, ProtocolPhase::Idle);
        assert!(!wrong_target.next.fallback_required);
        assert_eq!(wrong_target.next.manual_recovery_target, None);
        assert_eq!(wrong_target.next.keyboard_owner, Host::Linux);
        assert_eq!(wrong_target.next.pointer_owner, Host::Linux);

        let returned = apply(
            &wrong_target.next,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
            },
            10,
            TIMING,
        )
        .unwrap();
        assert_eq!(returned.next.manual_recovery_target, None);
    }

    #[test]
    fn explicit_manual_recovery_requires_settled_server_fallback() {
        let state = synchronized();
        let armed = apply(
            &state,
            Event::ArmManualRecovery {
                target: Host::Windows,
            },
            10,
            TIMING,
        )
        .unwrap();

        assert!(armed.effects.is_empty());
        assert_eq!(armed.next.phase, ProtocolPhase::Idle);
        assert_eq!(armed.next.keyboard_owner, Host::Linux);
        assert_eq!(armed.next.pointer_owner, Host::Linux);
        assert_eq!(armed.next.manual_recovery_target, Some(Host::Windows));

        let manual = apply(
            &armed.next,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: Some(Host::Windows),
            },
            11,
            TIMING,
        )
        .unwrap();
        assert!(manual.effects.is_empty());
        assert_eq!(manual.next.phase, ProtocolPhase::Idle);
        assert!(!manual.next.fallback_required);

        let remote = remote_owned();
        assert_eq!(
            apply(
                &remote,
                Event::ArmManualRecovery {
                    target: Host::Windows,
                },
                40,
                TIMING,
            )
            .unwrap_err(),
            ProtocolError::Unavailable
        );
        assert_eq!(
            apply(
                &state,
                Event::ArmManualRecovery {
                    target: Host::Linux,
                },
                10,
                TIMING,
            )
            .unwrap_err(),
            ProtocolError::InvalidManualRecoveryTarget
        );
    }

    #[test]
    fn create_keeps_both_owners_local_until_commit() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let transition = create_mac(&state).unwrap();
        assert_eq!(transition.next.keyboard_owner, Host::Linux);
        assert_eq!(transition.next.pointer_owner, Host::Linux);
        assert_eq!(transition.next.phase, ProtocolPhase::Switching);
        assert!(matches!(
            transition.effects.as_slice(),
            [Effect::SetInput {
                target: Host::Mac,
                fallback: false,
                ..
            }]
        ));
    }

    #[test]
    fn stale_observation_cannot_issue_grant() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let transition = create_mac(&state).unwrap();
        let epoch = transition.next.switch_epoch;
        let stale = apply(
            &transition.next,
            Event::Observation {
                switch_epoch: epoch.saturating_sub(1),
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: signals(Host::Mac),
            },
            20,
            TIMING,
        )
        .unwrap();
        assert_eq!(
            stale.next.active_request.unwrap().status,
            RequestStatus::Switching
        );
    }

    #[test]
    fn correct_observation_issues_grant_but_does_not_move_owners() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let verifying = acknowledge_switch(&switching, 11);
        let epoch = verifying.switch_epoch;
        let observed = apply(
            &verifying,
            Event::Observation {
                switch_epoch: epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: signals(Host::Mac),
            },
            20,
            TIMING,
        )
        .unwrap()
        .next;
        assert_eq!(observed.phase, ProtocolPhase::GrantPending);
        assert_eq!(observed.keyboard_owner, Host::Linux);
        assert_eq!(observed.pointer_owner, Host::Linux);
        assert_eq!(
            observed.active_request.as_ref().unwrap().status,
            RequestStatus::Grant
        );
    }

    #[test]
    fn transient_wrong_or_missing_target_signal_waits_for_convergence() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let verifying = acknowledge_switch(&switching, 11);
        let switch_epoch = verifying.switch_epoch;
        let pending = apply(
            &verifying,
            Event::Observation {
                switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: signals(Host::Linux),
            },
            20,
            TIMING,
        )
        .unwrap();

        assert_eq!(pending.next.keyboard_owner, Host::Linux);
        assert_eq!(pending.next.pointer_owner, Host::Linux);
        assert_eq!(pending.next.phase, ProtocolPhase::Verifying);
        assert!(pending.next.active_request.is_some());
        assert!(!pending.next.fallback_required);
        assert!(pending.effects.is_empty());
    }

    #[test]
    fn target_subscription_reobserves_without_extending_deadline() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let verifying = acknowledge_switch(&switching, 11);
        let switch_epoch = verifying.switch_epoch;
        let pending = apply(
            &verifying,
            Event::Observation {
                switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
                signals: signals(Host::Linux),
            },
            20,
            TIMING,
        )
        .unwrap()
        .next;
        let deadline = pending.phase_deadline_ms;

        let retry = apply(
            &pending,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
            },
            30,
            TIMING,
        )
        .unwrap();

        assert_eq!(retry.effects, vec![Effect::Observe { switch_epoch }]);
        assert_eq!(retry.next.phase_deadline_ms, deadline);
        assert_eq!(retry.next.observation_in_flight, Some(switch_epoch));

        let timed_out = apply(&retry.next, Event::Tick, deadline.unwrap(), TIMING).unwrap();
        assert_eq!(
            timed_out.next.fallback_reason.as_deref(),
            Some("target_signal_absent")
        );
    }

    #[test]
    fn transient_mismatch_falls_back_only_at_observation_deadline() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let verifying = acknowledge_switch(&switching, 11);
        let pending = apply(
            &verifying,
            Event::Observation {
                switch_epoch: verifying.switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
                signals: signals(Host::Linux),
            },
            20,
            TIMING,
        )
        .unwrap()
        .next;

        let timed_out = apply(
            &pending,
            Event::Tick,
            pending.phase_deadline_ms.unwrap(),
            TIMING,
        )
        .unwrap();
        assert_eq!(timed_out.next.keyboard_owner, Host::Linux);
        assert_eq!(timed_out.next.pointer_owner, Host::Linux);
        assert_eq!(
            timed_out.next.fallback_reason.as_deref(),
            Some("target_input_not_observed")
        );
    }

    #[test]
    fn target_signal_absence_is_reported_at_verification_deadline() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let verifying = acknowledge_switch(&switching, 11);
        let pending = apply(
            &verifying,
            Event::Observation {
                switch_epoch: verifying.switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: signals(Host::Linux),
            },
            20,
            TIMING,
        )
        .unwrap()
        .next;

        let timed_out = apply(
            &pending,
            Event::Tick,
            pending.phase_deadline_ms.unwrap(),
            TIMING,
        )
        .unwrap();

        assert_eq!(
            timed_out.next.fallback_reason.as_deref(),
            Some("target_signal_absent")
        );
        assert_eq!(timed_out.next.keyboard_owner, Host::Linux);
        assert_eq!(timed_out.next.pointer_owner, Host::Linux);
    }

    #[test]
    fn duplicate_command_ack_does_not_queue_second_observation() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let switch_epoch = switching.switch_epoch;
        let acknowledged = apply(
            &switching,
            Event::CommandAcknowledged {
                switch_epoch,
                target: Host::Mac,
            },
            11,
            TIMING,
        )
        .unwrap();
        assert_eq!(acknowledged.effects, vec![Effect::Observe { switch_epoch }]);
        let deadline = acknowledged.next.phase_deadline_ms;

        let duplicate = apply(
            &acknowledged.next,
            Event::CommandAcknowledged {
                switch_epoch,
                target: Host::Mac,
            },
            12,
            TIMING,
        )
        .unwrap();
        assert!(duplicate.effects.is_empty());
        assert_eq!(duplicate.next.phase_deadline_ms, deadline);
        assert_eq!(duplicate.next.observation_in_flight, Some(switch_epoch));
    }

    #[test]
    fn duplicate_valid_observation_does_not_regenerate_grant() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let verifying = acknowledge_switch(&switching, 11);
        let switch_epoch = verifying.switch_epoch;
        let observed = apply(
            &verifying,
            Event::Observation {
                switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: signals(Host::Mac),
            },
            20,
            TIMING,
        )
        .unwrap()
        .next;
        let grant = observed.active_request.as_ref().unwrap().grant.clone();
        let grant_epoch = observed.grant_epoch;
        assert_eq!(observed.switch_count[&Host::Mac], 1);

        let duplicate = apply(
            &observed,
            Event::Observation {
                switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
                signals: signals(Host::Mac),
            },
            21,
            TIMING,
        )
        .unwrap();
        assert!(duplicate.effects.is_empty());
        assert_eq!(duplicate.next.grant_epoch, grant_epoch);
        assert_eq!(duplicate.next.active_request.as_ref().unwrap().grant, grant);
        assert_eq!(duplicate.next.switch_count[&Host::Mac], 1);
    }

    #[test]
    fn commit_moves_keyboard_and_pointer_atomically() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let granted = grant_mac(&state);
        let request = granted.active_request.as_ref().unwrap();
        let committed = apply(
            &granted,
            Event::Commit {
                request_id: request.request_id.clone(),
                request_epoch: request.request_epoch,
                grant_epoch: request.grant.as_ref().unwrap().grant_epoch,
                lease_id: request.lease.lease_id.clone(),
                lease_epoch: request.lease.lease_epoch,
            },
            30,
            TIMING,
        )
        .unwrap()
        .next;
        assert_eq!(committed.keyboard_owner, Host::Mac);
        assert_eq!(committed.pointer_owner, Host::Mac);
        assert_eq!(committed.phase, ProtocolPhase::RemoteOwned);
        committed.validate(30).unwrap();
    }

    #[test]
    fn cancelling_committed_session_releases_input_and_is_idempotent() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let granted = grant_mac(&state);
        let request = granted.active_request.as_ref().unwrap();
        let committed = apply(
            &granted,
            Event::Commit {
                request_id: request.request_id.clone(),
                request_epoch: request.request_epoch,
                grant_epoch: request.grant.as_ref().unwrap().grant_epoch,
                lease_id: request.lease.lease_id.clone(),
                lease_epoch: request.lease.lease_epoch,
            },
            30,
            TIMING,
        )
        .unwrap()
        .next;

        let cancelled = apply(
            &committed,
            Event::Cancel {
                request_id: "request-1".to_string(),
                reason: "capture_released".to_string(),
            },
            31,
            TIMING,
        )
        .unwrap();
        assert_eq!(cancelled.next.keyboard_owner, Host::Linux);
        assert_eq!(cancelled.next.pointer_owner, Host::Linux);
        assert!(cancelled.next.active_session.is_none());
        assert_eq!(
            cancelled.next.request_history.back().unwrap().status,
            RequestStatus::Cancelled
        );
        assert!(matches!(
            cancelled.effects.as_slice(),
            [Effect::SetInput {
                target: Host::Linux,
                fallback: true,
                ..
            }]
        ));

        let repeated = apply(
            &cancelled.next,
            Event::Cancel {
                request_id: "request-1".to_string(),
                reason: "capture_released".to_string(),
            },
            32,
            TIMING,
        )
        .unwrap();
        assert!(repeated.effects.is_empty());
        assert_eq!(repeated.next, cancelled.next);
    }

    #[test]
    fn cancelling_after_readiness_fallback_is_idempotent() {
        let remote = remote_owned();
        let request_id = remote.active_session.as_ref().unwrap().request_id.clone();
        let fallback = apply(
            &remote,
            Event::PeerReadinessUpdated {
                host: Host::Mac,
                readiness: PeerReadiness::default(),
            },
            40,
            TIMING,
        )
        .unwrap();
        assert_eq!(
            fallback.next.request(&request_id).unwrap().status,
            RequestStatus::Fallback
        );

        let repeated = apply(
            &fallback.next,
            Event::Cancel {
                request_id,
                reason: "peer_readiness_lost".to_string(),
            },
            41,
            TIMING,
        )
        .unwrap();

        assert!(repeated.effects.is_empty());
        assert_eq!(repeated.next, fallback.next);
    }

    #[test]
    fn readiness_loss_before_commit_revokes_grant_and_falls_back() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let granted = grant_mac(&state);
        let lost = apply(
            &granted,
            Event::PeerReadinessUpdated {
                host: Host::Mac,
                readiness: PeerReadiness::default(),
            },
            21,
            TIMING,
        )
        .unwrap();
        assert_eq!(lost.next.keyboard_owner, Host::Linux);
        assert!(lost.next.fallback_required);
        assert!(matches!(
            lost.effects.as_slice(),
            [Effect::SetInput {
                target: Host::Linux,
                fallback: true,
                ..
            }]
        ));
    }

    #[test]
    fn disconnect_while_remote_owned_releases_input_before_reconnect() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let granted = grant_mac(&state);
        let request = granted.active_request.as_ref().unwrap();
        let remote = apply(
            &granted,
            Event::Commit {
                request_id: request.request_id.clone(),
                request_epoch: request.request_epoch,
                grant_epoch: request.grant.as_ref().unwrap().grant_epoch,
                lease_id: request.lease.lease_id.clone(),
                lease_epoch: request.lease.lease_epoch,
            },
            30,
            TIMING,
        )
        .unwrap()
        .next;
        let disconnected = apply(
            &remote,
            Event::TransportDisconnected {
                reason: "socket closed".to_string(),
            },
            31,
            TIMING,
        )
        .unwrap();
        assert_eq!(disconnected.next.keyboard_owner, Host::Linux);
        assert_eq!(disconnected.next.pointer_owner, Host::Linux);
        assert_eq!(disconnected.next.phase, ProtocolPhase::FallbackDeferred);
        assert!(disconnected.effects.is_empty());
    }

    #[test]
    fn offline_target_wakes_without_commanding_tv() {
        let state = synchronized();
        let transition = create_mac(&state).unwrap();
        assert_eq!(transition.next.phase, ProtocolPhase::Waking);
        assert_eq!(transition.next.commanded_input, None);
        assert!(matches!(
            transition.effects.as_slice(),
            [Effect::Wake {
                target: Host::Mac,
                ..
            }]
        ));
    }

    #[test]
    fn command_failure_releases_locally_and_starts_verified_fallback() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let failed = apply(
            &switching,
            Event::CommandFailed {
                switch_epoch: switching.switch_epoch,
                reason: "command rejected".to_string(),
            },
            11,
            TIMING,
        )
        .unwrap();

        assert_eq!(failed.next.keyboard_owner, Host::Linux);
        assert_eq!(failed.next.pointer_owner, Host::Linux);
        assert_eq!(failed.next.phase, ProtocolPhase::FallbackCommandPending);
        assert!(matches!(
            failed.effects.as_slice(),
            [Effect::SetInput {
                target: Host::Linux,
                fallback: true,
                ..
            }]
        ));
    }

    #[test]
    fn duplicate_current_commit_is_idempotent_and_stale_identity_is_rejected() {
        let remote = remote_owned();
        let committed = remote.request_history.back().unwrap();
        let grant = committed.grant.as_ref().unwrap();
        let event = Event::Commit {
            request_id: committed.request_id.clone(),
            request_epoch: committed.request_epoch,
            grant_epoch: grant.grant_epoch,
            lease_id: committed.lease.lease_id.clone(),
            lease_epoch: committed.lease.lease_epoch,
        };
        let duplicate = apply(&remote, event, 40, TIMING).unwrap();
        assert_eq!(duplicate.next, remote);
        assert!(duplicate.effects.is_empty());

        let stale = apply(
            &remote,
            Event::Commit {
                request_id: committed.request_id.clone(),
                request_epoch: committed.request_epoch,
                grant_epoch: grant.grant_epoch + 1,
                lease_id: committed.lease.lease_id.clone(),
                lease_epoch: committed.lease.lease_epoch,
            },
            40,
            TIMING,
        );
        assert_eq!(stale.unwrap_err(), ProtocolError::StaleIdentity);
    }

    #[test]
    fn active_lease_expiry_releases_both_inputs_before_fallback_io() {
        let remote = remote_owned();
        let deadline = remote.active_session.as_ref().unwrap().renewed_until_ms;
        let expired = apply(&remote, Event::Tick, deadline, TIMING).unwrap();

        assert_eq!(expired.next.keyboard_owner, Host::Linux);
        assert_eq!(expired.next.pointer_owner, Host::Linux);
        assert!(expired.next.active_session.is_none());
        assert_eq!(
            expired.next.fallback_reason.as_deref(),
            Some("active_lease_expired")
        );
        assert!(matches!(
            expired.effects.as_slice(),
            [Effect::SetInput {
                target: Host::Linux,
                fallback: true,
                ..
            }]
        ));
    }

    #[test]
    fn signal_poll_is_single_flight_and_timeout_falls_back() {
        let remote = remote_owned();
        let poll_at = remote.next_signal_poll_ms.unwrap();
        let polling = apply(&remote, Event::Tick, poll_at, TIMING).unwrap();
        assert_eq!(
            polling.effects,
            vec![Effect::Observe {
                switch_epoch: remote.switch_epoch
            }]
        );
        assert_eq!(polling.next.next_signal_poll_ms, None);
        assert_eq!(
            polling.next.next_deadline_ms(),
            polling.next.phase_deadline_ms
        );
        assert!(polling.next.next_deadline_ms().unwrap() > poll_at);

        let duplicate = apply(&polling.next, Event::Tick, poll_at + 1, TIMING).unwrap();
        assert!(duplicate.effects.is_empty());
        assert_eq!(
            duplicate.next.observation_in_flight,
            Some(remote.switch_epoch)
        );
        assert!(duplicate.next.next_deadline_ms().unwrap() > poll_at + 1);

        let timed_out = apply(
            &duplicate.next,
            Event::Tick,
            duplicate.next.phase_deadline_ms.unwrap(),
            TIMING,
        )
        .unwrap();
        assert_eq!(timed_out.next.keyboard_owner, Host::Linux);
        assert_eq!(
            timed_out.next.fallback_reason.as_deref(),
            Some("observation_timeout")
        );
    }

    #[test]
    fn invalid_renewal_and_active_readiness_loss_fail_local() {
        let remote = remote_owned();
        let session = remote.active_session.as_ref().unwrap();
        let invalid_renewal = apply(
            &remote,
            Event::Renew {
                request_id: session.request_id.clone(),
                lease_id: session.lease.lease_id.clone(),
                lease_epoch: session.lease.lease_epoch,
                peer_session_epoch: session.lease.peer_session_epoch + 1,
            },
            40,
            TIMING,
        )
        .unwrap();
        assert_eq!(invalid_renewal.next.keyboard_owner, Host::Linux);
        assert_eq!(
            invalid_renewal.next.fallback_reason.as_deref(),
            Some("lease_renewal_rejected")
        );

        let readiness_lost = apply(
            &remote,
            Event::PeerReadinessUpdated {
                host: Host::Mac,
                readiness: PeerReadiness::default(),
            },
            40,
            TIMING,
        )
        .unwrap();
        assert_eq!(readiness_lost.next.keyboard_owner, Host::Linux);
        assert_eq!(
            readiness_lost.next.fallback_reason.as_deref(),
            Some("active_peer_readiness_lost")
        );
    }

    #[test]
    fn active_readiness_loss_allows_manual_recovery_after_server_fallback() {
        let remote = remote_owned();
        let failed = apply(
            &remote,
            Event::PeerReadinessUpdated {
                host: Host::Mac,
                readiness: PeerReadiness::default(),
            },
            40,
            TIMING,
        )
        .unwrap()
        .next;

        assert_eq!(failed.keyboard_owner, Host::Linux);
        assert_eq!(failed.pointer_owner, Host::Linux);
        assert_eq!(failed.manual_recovery_target, Some(Host::Mac));
        assert_eq!(
            failed.fallback_reason.as_deref(),
            Some("active_peer_readiness_lost")
        );

        let verifying = acknowledge_switch(&failed, 41);
        let recovered = apply(
            &verifying,
            Event::Observation {
                switch_epoch: verifying.switch_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
                signals: signals(Host::Linux),
            },
            42,
            TIMING,
        )
        .unwrap()
        .next;
        assert_eq!(recovered.phase, ProtocolPhase::Idle);
        assert!(!recovered.fallback_required);
        assert_eq!(recovered.manual_recovery_target, Some(Host::Mac));

        let manual = apply(
            &recovered,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: Some(Host::Mac),
            },
            43,
            TIMING,
        )
        .unwrap();
        assert!(manual.effects.is_empty());
        assert_eq!(manual.next.phase, ProtocolPhase::Idle);
        assert!(!manual.next.fallback_required);
        assert_eq!(manual.next.observed_input, Some(Host::Mac));
        assert_eq!(manual.next.keyboard_owner, Host::Linux);
        assert_eq!(manual.next.pointer_owner, Host::Linux);
    }

    #[test]
    fn unexpected_subscription_during_remote_ownership_falls_back() {
        let remote = remote_owned();
        let changed = apply(
            &remote,
            Event::SubscriptionObserved {
                mode: TvMode::Multiview,
                input: None,
            },
            40,
            TIMING,
        )
        .unwrap();

        assert_eq!(changed.next.keyboard_owner, Host::Linux);
        assert_eq!(changed.next.manual_recovery_target, None);
        assert_eq!(
            changed.next.fallback_reason.as_deref(),
            Some("unexpected_tv_subscription")
        );
    }

    #[test]
    fn fullscreen_manual_change_during_remote_ownership_falls_back() {
        let remote = remote_owned();
        let changed = apply(
            &remote,
            Event::SubscriptionObserved {
                mode: TvMode::Fullscreen,
                input: Some(Host::Windows),
            },
            40,
            TIMING,
        )
        .unwrap();

        assert_eq!(changed.next.keyboard_owner, Host::Linux);
        assert_eq!(changed.next.pointer_owner, Host::Linux);
        assert_eq!(
            changed.next.fallback_reason.as_deref(),
            Some("unexpected_tv_subscription")
        );
        assert!(matches!(
            changed.effects.as_slice(),
            [Effect::SetInput {
                target: Host::Linux,
                fallback: true,
                ..
            }]
        ));
    }

    #[test]
    fn unexpected_subscription_while_grant_pending_revokes_grant() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let granted = grant_mac(&state);
        assert_eq!(granted.phase, ProtocolPhase::GrantPending);

        let changed = apply(
            &granted,
            Event::SubscriptionObserved {
                mode: TvMode::Multiview,
                input: None,
            },
            21,
            TIMING,
        )
        .unwrap();
        assert_eq!(changed.next.keyboard_owner, Host::Linux);
        assert!(changed.next.active_request.is_none());
        assert_eq!(
            changed.next.fallback_reason.as_deref(),
            Some("unexpected_tv_subscription")
        );
    }

    #[test]
    fn transport_disconnect_during_fallback_remains_local_and_deferred() {
        let state = ready_peer(&synchronized(), Host::Mac, 11);
        let switching = create_mac(&state).unwrap().next;
        let fallback = apply(
            &switching,
            Event::CommandFailed {
                switch_epoch: switching.switch_epoch,
                reason: "switch failed".to_string(),
            },
            11,
            TIMING,
        )
        .unwrap()
        .next;
        let disconnected = apply(
            &fallback,
            Event::TransportDisconnected {
                reason: "socket closed".to_string(),
            },
            12,
            TIMING,
        )
        .unwrap();

        assert_eq!(disconnected.next.keyboard_owner, Host::Linux);
        assert_eq!(disconnected.next.pointer_owner, Host::Linux);
        assert_eq!(disconnected.next.phase, ProtocolPhase::FallbackDeferred);
        assert!(disconnected.effects.is_empty());
    }

    #[test]
    fn multiview_on_verifies_observation_and_off_verifies_server_fallback() {
        let state = synchronized();
        let requested = apply(
            &state,
            Event::MultiViewRequested { enabled: true },
            10,
            TIMING,
        )
        .unwrap();
        let switch_epoch = requested.next.switch_epoch;
        assert_eq!(
            requested.effects,
            vec![Effect::SetMultiView {
                enabled: true,
                switch_epoch
            }]
        );
        let acknowledged = apply(
            &requested.next,
            Event::MultiViewAcknowledged {
                switch_epoch,
                enabled: true,
            },
            11,
            TIMING,
        )
        .unwrap();
        assert_eq!(acknowledged.effects, vec![Effect::Observe { switch_epoch }]);
        let enabled = apply(
            &acknowledged.next,
            Event::Observation {
                switch_epoch,
                mode: TvMode::Multiview,
                input: Some(Host::Linux),
                signals: signals(Host::Linux),
            },
            12,
            TIMING,
        )
        .unwrap()
        .next;
        assert_eq!(enabled.phase, ProtocolPhase::Idle);
        assert_eq!(enabled.tv_mode, TvMode::Multiview);

        let off = apply(
            &enabled,
            Event::MultiViewRequested { enabled: false },
            20,
            TIMING,
        )
        .unwrap();
        let off_epoch = off.next.switch_epoch;
        let off_ack = apply(
            &off.next,
            Event::MultiViewAcknowledged {
                switch_epoch: off_epoch,
                enabled: false,
            },
            21,
            TIMING,
        )
        .unwrap();
        let observed = apply(
            &off_ack.next,
            Event::Observation {
                switch_epoch: off_epoch,
                mode: TvMode::Fullscreen,
                input: Some(Host::Linux),
                signals: signals(Host::Linux),
            },
            22,
            TIMING,
        )
        .unwrap();
        assert_eq!(observed.next.phase, ProtocolPhase::FallbackCommandPending);
        assert!(matches!(
            observed.effects.as_slice(),
            [Effect::SetInput {
                target: Host::Linux,
                fallback: true,
                ..
            }]
        ));
    }

    #[test]
    fn shutdown_from_remote_ownership_is_local_and_deferred() {
        let remote = remote_owned();
        let shutdown = apply(&remote, Event::Shutdown, 40, TIMING).unwrap();

        assert_eq!(shutdown.next.keyboard_owner, Host::Linux);
        assert_eq!(shutdown.next.pointer_owner, Host::Linux);
        assert_eq!(shutdown.next.phase, ProtocolPhase::FallbackDeferred);
        assert!(shutdown.next.active_session.is_none());
        assert!(shutdown.effects.is_empty());
    }
}
