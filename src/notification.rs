use lan_mouse_ipc::SwitchHost;
use std::io;
use tokio::{process::Command, task::spawn_local};

#[derive(Clone)]
pub(crate) struct SystemNotifier {
    enabled: bool,
    server_host: SwitchHost,
}

#[derive(Debug, Eq, PartialEq)]
struct Notification {
    title: String,
    body: String,
}

impl SystemNotifier {
    pub(crate) fn new(local_host: SwitchHost, server_host: SwitchHost) -> Self {
        Self {
            enabled: local_host == server_host,
            server_host,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            server_host: SwitchHost::Linux,
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
        spawn_local(async move {
            match send_notification(&notification).await {
                Ok(()) => log::info!("system notification sent: {}", notification.title),
                Err(error) => log::warn!("failed to send system notification: {error}"),
            }
        });
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

#[cfg(target_os = "linux")]
async fn send_notification(notification: &Notification) -> io::Result<()> {
    let status = Command::new("notify-send")
        .args([
            "--app-name=Lan Mouse",
            "--urgency=critical",
            "--icon=input-mouse",
            notification.title.as_str(),
            notification.body.as_str(),
        ])
        .status()
        .await?;
    command_result(status)
}

#[cfg(target_os = "macos")]
async fn send_notification(notification: &Notification) -> io::Result<()> {
    let status = Command::new("osascript")
        .env("LAN_MOUSE_NOTIFICATION_TITLE", &notification.title)
        .env("LAN_MOUSE_NOTIFICATION_BODY", &notification.body)
        .args([
            "-e",
            "display notification (system attribute \"LAN_MOUSE_NOTIFICATION_BODY\") with title (system attribute \"LAN_MOUSE_NOTIFICATION_TITLE\")",
        ])
        .status()
        .await?;
    command_result(status)
}

#[cfg(windows)]
async fn send_notification(notification: &Notification) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
$template = [Windows.UI.Notifications.ToastTemplateType]::ToastText02
$xml = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent($template)
$text = $xml.GetElementsByTagName('text')
$text.Item(0).AppendChild($xml.CreateTextNode($env:LAN_MOUSE_NOTIFICATION_TITLE)) > $null
$text.Item(1).AppendChild($xml.CreateTextNode($env:LAN_MOUSE_NOTIFICATION_BODY)) > $null
$toast = [Windows.UI.Notifications.ToastNotification]::new($xml)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Lan Mouse').Show($toast)
"#;

    let status = Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .env("LAN_MOUSE_NOTIFICATION_TITLE", &notification.title)
        .env("LAN_MOUSE_NOTIFICATION_BODY", &notification.body)
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .status()
        .await?;
    command_result(status)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
async fn send_notification(_notification: &Notification) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "system notifications are unsupported on this platform",
    ))
}

fn command_result(status: std::process::ExitStatus) -> io::Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "notification command exited with {status}"
        )))
    }
}

fn host_label(host: SwitchHost) -> &'static str {
    match host {
        SwitchHost::Linux => "Linux",
        SwitchHost::Mac => "macOS",
        SwitchHost::Windows => "Windows",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifier_is_enabled_only_on_the_configured_server_host() {
        assert!(SystemNotifier::new(SwitchHost::Mac, SwitchHost::Mac).enabled);
        assert!(!SystemNotifier::new(SwitchHost::Windows, SwitchHost::Mac).enabled);
    }

    #[test]
    fn failure_notification_names_target_reason_and_fallback_host() {
        let notification = switch_failure_notification(
            Some(SwitchHost::Windows),
            SwitchHost::Mac,
            "peer_bundle_not_ready",
            describe_reason("peer_bundle_not_ready"),
        );

        assert_eq!(notification.title, "Lan Mouse: switch to Windows failed");
        assert!(notification.body.contains("keyboard and pointer bundle"));
        assert!(notification.body.contains("Input remains on macOS"));
        assert!(notification.body.contains("peer_bundle_not_ready"));
    }
}
