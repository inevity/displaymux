use lan_mouse_ipc::SwitchHost;
use notify_rust::{Notification as NativeNotification, Timeout};
use tokio::task::{spawn_blocking, spawn_local};

#[derive(Clone)]
pub(crate) struct SystemNotifier {
    enabled: bool,
    local_host: Option<SwitchHost>,
    server_host: SwitchHost,
}

#[derive(Debug, Eq, PartialEq)]
struct Notification {
    title: String,
    body: String,
    timeout: Timeout,
}

impl SystemNotifier {
    pub(crate) fn new(local_host: SwitchHost, server_host: SwitchHost) -> Self {
        Self {
            enabled: local_host == server_host,
            local_host: Some(local_host),
            server_host,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            local_host: None,
            server_host: SwitchHost::Controller,
        }
    }

    pub(crate) fn switch_failed(&self, target: Option<SwitchHost>, reason: &str) {
        self.switch_failed_with_detail(target, reason, describe_reason(reason));
    }

    pub(crate) fn switch_failed_with_detail(
        &self,
        target: Option<SwitchHost>,
        reason: &str,
        detail: impl Into<String>,
    ) {
        if !self.enabled {
            return;
        }

        let notification =
            switch_failure_notification(target, self.server_host, reason, detail.into());
        self.show(notification);
    }

    pub(crate) fn clipboard_permission_denied(&self) {
        let Some(local_host) = self.local_host else {
            return;
        };
        self.show(clipboard_permission_notification(local_host));
    }

    fn show(&self, notification: Notification) {
        spawn_local(async move {
            let title = notification.title.clone();
            match send_notification(notification).await {
                Ok(()) => log::info!("system notification sent: {title}"),
                Err(error) => log::warn!("failed to send system notification: {error}"),
            }
        });
    }
}

fn clipboard_permission_notification(local_host: SwitchHost) -> Notification {
    Notification {
        title: "Lan Mouse: clipboard access denied".to_string(),
        body: format!(
            "Clipboard handoff is unavailable on {}. Input switching still works. Set Lan Mouse pasteboard access to Always Allow in System Settings.",
            host_label(local_host)
        ),
        timeout: Timeout::Never,
    }
}

fn switch_failure_notification(
    target: Option<SwitchHost>,
    server_host: SwitchHost,
    reason: &str,
    detail: String,
) -> Notification {
    let target = target.map(host_label).unwrap_or("target");
    Notification {
        title: format!("Lan Mouse: switch to {target} failed"),
        body: format!(
            "{}. Input remains on {}. Reason: {reason}",
            detail,
            host_label(server_host),
        ),
        timeout: Timeout::Never,
    }
}

fn describe_reason(reason: &str) -> String {
    match reason {
        "switch_target_not_configured" => "The edge has no configured switch target".to_string(),
        "switch_controller_not_configured" => "The switch controller is not configured".to_string(),
        "controller_busy" => "Another switch operation is already active".to_string(),
        "peer_missing"
        | "peer_missing_before_grant"
        | "peer_missing_before_commit"
        | "peer_missing_after_commit"
        | "peer_missing_during_renewal"
        | "peer_removed" => "The target lan-mouse peer is unavailable".to_string(),
        "peer_bundle_not_ready" | "peer_readiness_lost" => {
            "The target keyboard and pointer bundle is not ready".to_string()
        }
        "peer_offline" => {
            "No lan-mouse heartbeat was received; the target may be asleep, stopped, or unreachable"
                .to_string()
        }
        "peer_revision_mismatch" => {
            "The target lan-mouse revision does not match the server".to_string()
        }
        "peer_readiness_handshake_missing" => {
            "The target did not publish a current input-readiness session".to_string()
        }
        "peer_keyboard_unavailable" => {
            "The target keyboard emulation backend is unavailable".to_string()
        }
        "peer_pointer_unavailable" => {
            "The target pointer emulation backend is unavailable".to_string()
        }
        "peer_input_unavailable" => {
            "The target keyboard and pointer emulation backends are unavailable".to_string()
        }
        "controller_timeout" => "The TV controller did not respond before the deadline".to_string(),
        "controller_unreachable" => "The TV controller could not be reached".to_string(),
        "controller_identity_race" => {
            "A stale or conflicting request/grant response was rejected".to_string()
        }
        "prepare_failed" => "TV switch preparation failed".to_string(),
        "grant_rejected_locally" | "grant_deadline_missing" | "grant_expired_before_arm" => {
            "The TV grant was invalid or expired".to_string()
        }
        "capture_permit_stale" | "capture_commit_rejected" => {
            "The input capture authorization became stale".to_string()
        }
        "controller_commit_failed" | "commit_ack_rejected" => {
            "The controller did not confirm input ownership".to_string()
        }
        "lease_renewal_failed" | "renewal_ack_rejected" | "local_lease_expired" => {
            "The active keyboard and pointer lease expired".to_string()
        }
        "capture_backend_disabled" => "The local input capture backend stopped".to_string(),
        "peer_readiness_lost_during_capture" => {
            "The target input backend stopped during handoff".to_string()
        }
        "peer_transport_failed_during_capture" => {
            "The target connection failed during handoff".to_string()
        }
        _ => reason.replace('_', " "),
    }
}

async fn send_notification(notification: Notification) -> Result<(), String> {
    spawn_blocking(move || {
        NativeNotification::new()
            .summary(&notification.title)
            .body(&notification.body)
            .appname("Lan Mouse")
            .timeout(notification.timeout)
            .show()
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("notification task failed: {error}"))?
}

fn host_label(host: SwitchHost) -> &'static str {
    match host {
        SwitchHost::Controller => "controller",
        SwitchHost::Right => "right client",
        SwitchHost::Left => "left client",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifier_is_enabled_only_on_the_configured_server_host() {
        assert!(SystemNotifier::new(SwitchHost::Right, SwitchHost::Right).enabled);
        assert!(!SystemNotifier::new(SwitchHost::Left, SwitchHost::Right).enabled);
    }

    #[test]
    fn failure_notification_names_target_reason_and_fallback_host() {
        let notification = switch_failure_notification(
            Some(SwitchHost::Left),
            SwitchHost::Right,
            "peer_bundle_not_ready",
            describe_reason("peer_bundle_not_ready"),
        );

        assert_eq!(
            notification.title,
            "Lan Mouse: switch to left client failed"
        );
        assert!(notification.body.contains("keyboard and pointer bundle"));
        assert!(notification.body.contains("Input remains on right client"));
        assert!(notification.body.contains("peer_bundle_not_ready"));
        assert_eq!(notification.timeout, Timeout::Never);
    }

    #[test]
    fn clipboard_permission_notification_is_actionable_and_preserves_input() {
        let notification = clipboard_permission_notification(SwitchHost::Right);
        assert!(notification.body.contains("Always Allow"));
        assert!(notification.body.contains("Input switching still works"));
        assert!(notification.body.contains("right client"));
        assert_eq!(notification.timeout, Timeout::Never);
    }
}
