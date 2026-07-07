// TV operations via bscpylgtvcommand subprocess.
// Mirrors LG_Buddy's approach: shells out to the Python CLI
// at /usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand.

use std::net::Ipv4Addr;
use tokio::process::Command;
use tracing::debug;

const BSCPYLGTV: &str = "/usr/bin/LG_Buddy_PIP/bin/bscpylgtvcommand";

#[derive(Debug, Clone)]
pub struct TvClient {
    pub ip: Ipv4Addr,
}

impl TvClient {
    pub fn new(ip: Ipv4Addr) -> Self {
        Self { ip }
    }

    async fn run(&self, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
        let cmd = format!("{} {} {}", BSCPYLGTV, self.ip, args.join(" "));
        debug!(cmd = %cmd, "bscpylgtvcommand");

        Command::new(BSCPYLGTV)
            .arg(self.ip.to_string())
            .args(args)
            .output()
            .await
            .inspect(|out| {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stdout.trim().is_empty() {
                    debug!(stdout = %stdout.trim(), "bscpylgtvcommand stdout");
                }
                if !stderr.trim().is_empty() {
                    debug!(stderr = %stderr.trim(), "bscpylgtvcommand stderr");
                }
            })
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

    /// Toggle multiView (splitscreenEnable) via Luna API.
    pub async fn set_splitscreen(&self, enable: bool) -> Result<(), String> {
        let val = if enable { "on" } else { "off" };
        let payload = format!("{{\"splitscreenEnable\":\"{}\"}}", val);

        let out = self
            .run(&["set_settings", "commercial", &payload])
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
            .run(&["get_software_info"])
            .await
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!("get_sw_info failed: {}", stderr.trim()))
        }
    }
}
