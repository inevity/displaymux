use crate::domain::Host;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    env, fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};
use thiserror::Error;

const CONFIG_ENV: &str = "TV_MULTIVIEW_CONFIG";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub bind_address: SocketAddr,
    pub tv_ip: IpAddr,
    pub server_host: Host,
    pub controller_token: String,
    pub client_key_path: PathBuf,
    pub inputs: BTreeMap<Host, String>,
    #[serde(default)]
    pub wake_on_lan: BTreeMap<Host, String>,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub command_ms: u64,
    pub observation_ms: u64,
    pub grant_ms: u64,
    pub wake_ms: u64,
    pub lease_ms: u64,
    pub signal_poll_ms: u64,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
    pub keepalive_ms: u64,
    pub keepalive_timeout_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            command_ms: 10_000,
            observation_ms: 5_000,
            grant_ms: 5_000,
            wake_ms: 60_000,
            lease_ms: 15_000,
            signal_poll_ms: 10_000,
            reconnect_initial_ms: 1_000,
            reconnect_max_ms: 60_000,
            keepalive_ms: 10_000,
            keepalive_timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub command_queue: usize,
    pub safety_queue: usize,
    pub retained_requests: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            command_queue: 64,
            safety_queue: 16,
            retained_requests: 32,
        }
    }
}

impl DaemonConfig {
    pub fn load() -> Result<Self, ConfigError> {
        let path = match env::var_os(CONFIG_ENV) {
            Some(path) => PathBuf::from(path),
            None => {
                let home = env::var_os("HOME").ok_or(ConfigError::HomeMissing)?;
                PathBuf::from(home)
                    .join(".config")
                    .join("tv-multiview")
                    .join("config.toml")
            }
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.controller_token.is_empty() {
            return Err(ConfigError::EmptyControllerToken);
        }
        for host in Host::ALL {
            let input = self
                .inputs
                .get(&host)
                .ok_or(ConfigError::MissingInput(host))?;
            if input.trim().is_empty() {
                return Err(ConfigError::EmptyInput(host));
            }
        }
        for (name, value) in [
            ("command_ms", self.timeouts.command_ms),
            ("observation_ms", self.timeouts.observation_ms),
            ("grant_ms", self.timeouts.grant_ms),
            ("wake_ms", self.timeouts.wake_ms),
            ("lease_ms", self.timeouts.lease_ms),
            ("signal_poll_ms", self.timeouts.signal_poll_ms),
            ("reconnect_initial_ms", self.timeouts.reconnect_initial_ms),
            ("reconnect_max_ms", self.timeouts.reconnect_max_ms),
            ("keepalive_ms", self.timeouts.keepalive_ms),
            ("keepalive_timeout_ms", self.timeouts.keepalive_timeout_ms),
        ] {
            if value == 0 {
                return Err(ConfigError::ZeroTimeout(name));
            }
        }
        for (name, value) in [
            ("command_queue", self.limits.command_queue),
            ("safety_queue", self.limits.safety_queue),
            ("retained_requests", self.limits.retained_requests),
        ] {
            if value == 0 {
                return Err(ConfigError::ZeroLimit(name));
            }
        }
        Ok(())
    }

    pub fn input_for(&self, host: Host) -> &str {
        self.inputs.get(&host).expect("validated input mapping")
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("HOME is unset and {CONFIG_ENV} was not provided")]
    HomeMissing,
    #[error("failed to read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid TOML configuration: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("controller_token must not be empty")]
    EmptyControllerToken,
    #[error("missing HDMI input mapping for {0}")]
    MissingInput(Host),
    #[error("HDMI input mapping for {0} is empty")]
    EmptyInput(Host),
    #[error("timeout {0} must be greater than zero")]
    ZeroTimeout(&'static str),
    #[error("limit {0} must be greater than zero")]
    ZeroLimit(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
bind_address = "127.0.0.1:8765"
tv_ip = "192.0.2.10"
server_host = "linux"
controller_token = "test-token"
client_key_path = "/tmp/key.sqlite"

[inputs]
linux = "HDMI_4"
mac = "HDMI_3"
windows = "HDMI_2"
"#;

    #[test]
    fn parses_complete_configuration() {
        let config: DaemonConfig = toml::from_str(VALID).unwrap();
        config.validate().unwrap();
        assert_eq!(config.server_host, Host::Linux);
        assert_eq!(config.input_for(Host::Windows), "HDMI_2");
    }

    #[test]
    fn rejects_missing_host_mapping() {
        let config: DaemonConfig =
            toml::from_str(&VALID.replace("mac = \"HDMI_3\"\n", "")).expect("syntactically valid");
        assert!(matches!(
            config.validate(),
            Err(ConfigError::MissingInput(Host::Mac))
        ));
    }

    #[test]
    fn rejects_zero_deadline() {
        let mut config: DaemonConfig = toml::from_str(VALID).unwrap();
        config.timeouts.grant_ms = 0;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::ZeroTimeout("grant_ms"))
        ));
    }
}
