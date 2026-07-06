// TV operations via bscpylgtvcommand subprocess.
// Mirrors LG_Buddy's approach: shells out to the Python CLI
// at /usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand.

use std::net::Ipv4Addr;
use std::process::Output;
use tokio::process::Command;

const BSCPYLGTV: &str = "/usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand";

#[derive(Debug)]
pub struct TvClient {
    pub ip: Ipv4Addr,
}

impl TvClient {
    pub fn new(ip: Ipv4Addr) -> Self {
        Self { ip }
    }

    async fn run(&self, args: &[&str]) -> Result<Output, std::io::Error> {
        Command::new(BSCPYLGTV)
            .arg(self.ip.to_string())
            .args(args)
            .output()
            .await
    }

    /// Set the TV input to the given HDMI port.
    pub async fn set_input(&self, hdmi: &str) -> Result<(), String> {
        let out = self.run(&["set_input", hdmi]).await.map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!("set_input failed: {}", stderr.trim()))
        }
    }

    /// Toggle multiView (splitscreenEnable).
    pub async fn set_splitscreen(&self, enable: bool) -> Result<(), String> {
        let val = if enable { "on" } else { "off" };
        let payload = format!(r#"{{"splitscreenEnable":"{}"}}"#, val);

        let out = self
            .run(&["set_system_settings", "commercial", &payload])
            .await
            .map_err(|e| e.to_string())?;

        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!("set_splitscreen failed: {}", stderr.trim()))
        }
    }

    /// Heartbeat: get current software info.
    pub async fn get_sw_info(&self) -> Result<(), String> {
        let out = self
            .run(&["get_current_sw_info"])
            .await
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!("get_sw_info failed: {}", stderr.trim()))
        }
    }

    /// Poll multiViewStatus. Returns Some(true) if active, Some(false) if not,
    /// None on error.
    pub async fn poll_multiview_status(&self) -> Option<bool> {
        let payload = r#"{"category":"option","keys":["multiViewStatus"]}"#;
        let out = self
            .run(&["get_system_settings", "option", payload])
            .await
            .ok()?;

        if !out.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        // Response is JSON: {"settings":{"multiViewStatus":"on"}}
        let val: serde_json::Value = serde_json::from_str(&stdout).ok()?;
        let status = val
            .get("settings")?
            .get("multiViewStatus")?
            .as_str()?;
        Some(status == "on")
    }
}
