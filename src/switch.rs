use lan_mouse_ipc::ClientHandle;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SwitchHost {
    Linux,
    Mac,
    Windows,
}

impl SwitchHost {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Mac => "mac",
            Self::Windows => "windows",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LeaseIdentity {
    pub(crate) request_id: String,
    pub(crate) lease_id: String,
    pub(crate) lease_epoch: u64,
    pub(crate) peer_session_epoch: u64,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GrantIdentity {
    pub(crate) request_epoch: u64,
    pub(crate) grant_epoch: u64,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GateContext {
    pub(crate) handle: ClientHandle,
    pub(crate) target: SwitchHost,
    pub(crate) lease: LeaseIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BundleGateState {
    Local,
    Preparing(GateContext),
    GrantArmed {
        context: GateContext,
        grant: GrantIdentity,
    },
    RemoteOwned {
        context: GateContext,
        grant: GrantIdentity,
        renewed_until_ms: u64,
    },
}

impl Default for BundleGateState {
    fn default() -> Self {
        Self::Local
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum GateError {
    #[error("input bundle is already reserved")]
    Busy,
    #[error("peer keyboard and pointer bundle is not ready")]
    PeerNotReady,
    #[error("request or lease identity is invalid")]
    InvalidIdentity,
    #[error("request, lease, or grant identity is stale")]
    StaleIdentity,
    #[error("bundle lease or grant expired")]
    Expired,
    #[error("gate transition is not valid in the current state")]
    InvalidState,
}

#[derive(Debug, Default)]
pub(crate) struct BundleLeaseManager {
    state: BundleGateState,
    next_lease_epoch: u64,
}

impl BundleLeaseManager {
    pub(crate) fn state(&self) -> &BundleGateState {
        &self.state
    }

    pub(crate) fn reserve(
        &mut self,
        handle: ClientHandle,
        target: SwitchHost,
        request_id: String,
        lease_id: String,
        peer_session_epoch: u64,
        peer_bundle_ready: bool,
        now_ms: u64,
        lease_ttl_ms: u64,
    ) -> Result<GateContext, GateError> {
        if self.state != BundleGateState::Local {
            return Err(GateError::Busy);
        }
        if !peer_bundle_ready || peer_session_epoch == 0 {
            return Err(GateError::PeerNotReady);
        }
        if request_id.is_empty() || lease_id.is_empty() || lease_ttl_ms == 0 {
            return Err(GateError::InvalidIdentity);
        }
        let expires_at_ms = now_ms
            .checked_add(lease_ttl_ms)
            .ok_or(GateError::InvalidIdentity)?;
        self.next_lease_epoch = self
            .next_lease_epoch
            .checked_add(1)
            .ok_or(GateError::InvalidIdentity)?;
        let context = GateContext {
            handle,
            target,
            lease: LeaseIdentity {
                request_id,
                lease_id,
                lease_epoch: self.next_lease_epoch,
                peer_session_epoch,
                expires_at_ms,
            },
        };
        self.state = BundleGateState::Preparing(context.clone());
        Ok(context)
    }

    pub(crate) fn arm_grant(
        &mut self,
        context: &GateContext,
        request_epoch: u64,
        grant_epoch: u64,
        grant_expires_at_ms: u64,
        peer_bundle_ready: bool,
        peer_session_epoch: u64,
        now_ms: u64,
    ) -> Result<GrantIdentity, GateError> {
        let BundleGateState::Preparing(current) = &self.state else {
            return Err(GateError::InvalidState);
        };
        if current != context {
            return Err(GateError::StaleIdentity);
        }
        if !peer_bundle_ready || current.lease.peer_session_epoch != peer_session_epoch {
            self.state = BundleGateState::Local;
            return Err(GateError::PeerNotReady);
        }
        if current.lease.expires_at_ms <= now_ms || grant_expires_at_ms <= now_ms {
            self.state = BundleGateState::Local;
            return Err(GateError::Expired);
        }
        if request_epoch == 0 || grant_epoch == 0 {
            return Err(GateError::InvalidIdentity);
        }

        let grant = GrantIdentity {
            request_epoch,
            grant_epoch,
            expires_at_ms: grant_expires_at_ms,
        };
        self.state = BundleGateState::GrantArmed {
            context: current.clone(),
            grant: grant.clone(),
        };
        Ok(grant)
    }

    pub(crate) fn commit(
        &mut self,
        handle: ClientHandle,
        lease_epoch: u64,
        peer_bundle_ready: bool,
        peer_session_epoch: u64,
        now_ms: u64,
    ) -> Result<(GateContext, GrantIdentity), GateError> {
        let BundleGateState::GrantArmed { context, grant } = &self.state else {
            return Err(GateError::InvalidState);
        };
        if context.handle != handle || context.lease.lease_epoch != lease_epoch {
            return Err(GateError::StaleIdentity);
        }
        if !peer_bundle_ready || context.lease.peer_session_epoch != peer_session_epoch {
            self.state = BundleGateState::Local;
            return Err(GateError::PeerNotReady);
        }
        if context.lease.expires_at_ms <= now_ms || grant.expires_at_ms <= now_ms {
            self.state = BundleGateState::Local;
            return Err(GateError::Expired);
        }

        let context = context.clone();
        let grant = grant.clone();
        self.state = BundleGateState::RemoteOwned {
            context: context.clone(),
            grant: grant.clone(),
            renewed_until_ms: context.lease.expires_at_ms.min(grant.expires_at_ms),
        };
        Ok((context, grant))
    }

    pub(crate) fn renew(
        &mut self,
        request_id: &str,
        renewed_until_ms: u64,
        peer_bundle_ready: bool,
        peer_session_epoch: u64,
        now_ms: u64,
    ) -> Result<(), GateError> {
        let BundleGateState::RemoteOwned {
            context,
            renewed_until_ms: current_renewal,
            ..
        } = &mut self.state
        else {
            return Err(GateError::InvalidState);
        };
        if context.lease.request_id != request_id {
            return Err(GateError::StaleIdentity);
        }
        if !peer_bundle_ready || context.lease.peer_session_epoch != peer_session_epoch {
            self.state = BundleGateState::Local;
            return Err(GateError::PeerNotReady);
        }
        if context.lease.expires_at_ms <= now_ms || renewed_until_ms <= now_ms {
            self.state = BundleGateState::Local;
            return Err(GateError::Expired);
        }
        context.lease.expires_at_ms = renewed_until_ms;
        *current_renewal = renewed_until_ms;
        Ok(())
    }

    pub(crate) fn expire(&mut self, now_ms: u64) -> Option<GateContext> {
        let expired = match &self.state {
            BundleGateState::Local => false,
            BundleGateState::Preparing(context) => context.lease.expires_at_ms <= now_ms,
            BundleGateState::GrantArmed { context, grant } => {
                context.lease.expires_at_ms <= now_ms || grant.expires_at_ms <= now_ms
            }
            BundleGateState::RemoteOwned {
                context,
                renewed_until_ms,
                ..
            } => context.lease.expires_at_ms <= now_ms || *renewed_until_ms <= now_ms,
        };
        expired.then(|| self.invalidate()).flatten()
    }

    pub(crate) fn invalidate(&mut self) -> Option<GateContext> {
        let previous = std::mem::take(&mut self.state);
        match previous {
            BundleGateState::Local => None,
            BundleGateState::Preparing(context)
            | BundleGateState::GrantArmed { context, .. }
            | BundleGateState::RemoteOwned { context, .. } => Some(context),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reserve(manager: &mut BundleLeaseManager) -> GateContext {
        manager
            .reserve(
                4,
                SwitchHost::Windows,
                "request-1".to_string(),
                "lease-1".to_string(),
                22,
                true,
                100,
                50,
            )
            .unwrap()
    }

    fn arm(manager: &mut BundleLeaseManager, context: &GateContext) -> GrantIdentity {
        manager
            .arm_grant(context, 7, 9, 140, true, 22, 110)
            .unwrap()
    }

    #[test]
    fn reservation_requires_atomic_peer_bundle() {
        let mut manager = BundleLeaseManager::default();

        let result = manager.reserve(
            4,
            SwitchHost::Windows,
            "request-1".to_string(),
            "lease-1".to_string(),
            22,
            false,
            100,
            50,
        );

        assert_eq!(result, Err(GateError::PeerNotReady));
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn one_bundle_reservation_excludes_competing_target() {
        let mut manager = BundleLeaseManager::default();
        reserve(&mut manager);

        let result = manager.reserve(
            5,
            SwitchHost::Mac,
            "request-2".to_string(),
            "lease-2".to_string(),
            31,
            true,
            101,
            50,
        );

        assert_eq!(result, Err(GateError::Busy));
    }

    #[test]
    fn stale_grant_cannot_replace_active_preparation() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        let mut stale = context.clone();
        stale.lease.request_id = "old-request".to_string();

        assert_eq!(
            manager.arm_grant(&stale, 7, 9, 140, true, 22, 110),
            Err(GateError::StaleIdentity)
        );
        assert_eq!(manager.state(), &BundleGateState::Preparing(context));
    }

    #[test]
    fn readiness_loss_before_grant_returns_local() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);

        assert_eq!(
            manager.arm_grant(&context, 7, 9, 140, false, 22, 110),
            Err(GateError::PeerNotReady)
        );
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn commit_requires_same_handle_epoch_and_peer_session() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        arm(&mut manager, &context);

        assert_eq!(
            manager.commit(5, context.lease.lease_epoch, true, 22, 115),
            Err(GateError::StaleIdentity)
        );
        assert!(matches!(
            manager.state(),
            BundleGateState::GrantArmed { .. }
        ));

        assert_eq!(
            manager.commit(4, context.lease.lease_epoch, true, 23, 115),
            Err(GateError::PeerNotReady)
        );
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn valid_commit_moves_the_whole_bundle_to_remote_owned() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        let grant = arm(&mut manager, &context);

        let committed = manager
            .commit(4, context.lease.lease_epoch, true, 22, 115)
            .unwrap();

        assert_eq!(committed, (context.clone(), grant.clone()));
        assert_eq!(
            manager.state(),
            &BundleGateState::RemoteOwned {
                context,
                grant,
                renewed_until_ms: 140,
            }
        );
    }

    #[test]
    fn renewal_requires_matching_active_request_and_peer_session() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        arm(&mut manager, &context);
        manager
            .commit(4, context.lease.lease_epoch, true, 22, 115)
            .unwrap();

        assert_eq!(
            manager.renew("old-request", 135, true, 22, 120),
            Err(GateError::StaleIdentity)
        );
        assert!(matches!(
            manager.state(),
            BundleGateState::RemoteOwned { .. }
        ));

        assert_eq!(
            manager.renew("request-1", 135, true, 23, 120),
            Err(GateError::PeerNotReady)
        );
        assert_eq!(manager.state(), &BundleGateState::Local);
    }

    #[test]
    fn valid_renewal_replaces_the_active_lease_deadline() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        let grant = arm(&mut manager, &context);
        manager
            .commit(4, context.lease.lease_epoch, true, 22, 115)
            .unwrap();

        manager.renew("request-1", 200, true, 22, 120).unwrap();
        let mut renewed_context = context;
        renewed_context.lease.expires_at_ms = 200;

        assert_eq!(
            manager.state(),
            &BundleGateState::RemoteOwned {
                context: renewed_context,
                grant,
                renewed_until_ms: 200,
            }
        );
    }

    #[test]
    fn expiry_always_invalidates_to_local() {
        let mut manager = BundleLeaseManager::default();
        let context = reserve(&mut manager);
        arm(&mut manager, &context);

        assert_eq!(manager.expire(140), Some(context));
        assert_eq!(manager.state(), &BundleGateState::Local);
    }
}
