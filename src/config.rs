use crate::capture_test::TestCaptureArgs;
use crate::emulation_test::TestEmulationArgs;
use clap::{Parser, Subcommand, ValueEnum};
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env::{self, VarError};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io};
use thiserror::Error;
use toml;
use toml_edit::{self, DocumentMut};

use lan_mouse_cli::CliArgs;
use lan_mouse_ipc::{DEFAULT_PORT, Position, SwitchHost};

use input_event::scancode::{
    self,
    Linux::{KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift},
};

use shadow_rs::shadow;

shadow!(build);

/// Local build's 8-byte ASCII short commit hash, suitable for use
/// in [`lan_mouse_proto::ProtoEvent::Hello`]. Pads with `'?'` if
/// shadow_rs returns an unexpected length so the field is always
/// well-formed on the wire.
pub fn local_commit() -> [u8; 8] {
    let bytes = build::SHORT_COMMIT.as_bytes();
    let mut out = [b'?'; 8];
    let n = bytes.len().min(8);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

const CONFIG_FILE_NAME: &str = "config.toml";
const CERT_FILE_NAME: &str = "lan-mouse.pem";

fn default_path() -> Result<PathBuf, VarError> {
    #[cfg(unix)]
    let default_path = {
        let xdg_config_home =
            env::var("XDG_CONFIG_HOME").unwrap_or(format!("{}/.config", env::var("HOME")?));
        format!("{xdg_config_home}/lan-mouse/")
    };

    #[cfg(not(unix))]
    let default_path = {
        let app_data =
            env::var("LOCALAPPDATA").unwrap_or(format!("{}/.config", env::var("USERPROFILE")?));
        format!("{app_data}\\lan-mouse\\")
    };
    Ok(PathBuf::from(default_path))
}

const DEFAULT_CLIPBOARD_MAX_BYTES: usize = 3 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct ConfigToml {
    capture_backend: Option<CaptureBackend>,
    emulation_backend: Option<EmulationBackend>,
    emulation_display: Option<String>,
    port: Option<u16>,
    release_bind: Option<Vec<scancode::Linux>>,
    cert_path: Option<PathBuf>,
    clients: Option<Vec<TomlClient>>,
    authorized_fingerprints: Option<HashMap<String, String>>,
    switch_controller: Option<SwitchControllerToml>,
    clipboard: Option<ClipboardToml>,
}

impl Default for ConfigToml {
    fn default() -> Self {
        Self {
            capture_backend: None,
            emulation_backend: None,
            emulation_display: None,
            port: None,
            release_bind: None,
            cert_path: None,
            clients: None,
            authorized_fingerprints: None,
            switch_controller: None,
            clipboard: Some(ClipboardToml::default()),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct ClipboardToml {
    enabled: bool,
    max_bytes: usize,
}

impl Default for ClipboardToml {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: DEFAULT_CLIPBOARD_MAX_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClipboardConfig {
    pub(crate) enabled: bool,
    pub(crate) max_bytes: usize,
}

impl TryFrom<ClipboardToml> for ClipboardConfig {
    type Error = ConfigError;

    fn try_from(config: ClipboardToml) -> Result<Self, Self::Error> {
        if config.max_bytes == 0 || u64::try_from(config.max_bytes).is_err() {
            return Err(ConfigError::Clipboard(
                "max_bytes must be non-zero and representable by the wire protocol".to_string(),
            ));
        }
        Ok(Self {
            enabled: config.enabled,
            max_bytes: config.max_bytes,
        })
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct SwitchControllerToml {
    url: String,
    token: String,
    local_host: SwitchHost,
    server_host: SwitchHost,
    http_timeout_ms: u64,
    request_timeout_ms: u64,
    poll_interval_ms: u64,
    edge_double_tap_ms: u64,
    lease_ttl_ms: u64,
    renew_interval_ms: u64,
}

#[derive(Clone)]
pub(crate) struct SwitchControllerConfig {
    pub(crate) url: reqwest::Url,
    pub(crate) token: String,
    pub(crate) local_host: SwitchHost,
    pub(crate) server_host: SwitchHost,
    pub(crate) http_timeout_ms: u64,
    pub(crate) request_timeout_ms: u64,
    pub(crate) poll_interval_ms: u64,
    pub(crate) edge_double_tap_ms: u64,
    pub(crate) lease_ttl_ms: u64,
    pub(crate) renew_interval_ms: u64,
}

impl TryFrom<SwitchControllerToml> for SwitchControllerConfig {
    type Error = ConfigError;

    fn try_from(config: SwitchControllerToml) -> Result<Self, Self::Error> {
        let mut url = reqwest::Url::parse(&config.url)
            .map_err(|error| ConfigError::SwitchController(error.to_string()))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ConfigError::SwitchController(
                "url must be an http(s) base URL without credentials, query, or fragment"
                    .to_string(),
            ));
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        if config.token.is_empty() {
            return Err(ConfigError::SwitchController(
                "token must not be empty".to_string(),
            ));
        }
        if config.http_timeout_ms == 0
            || config.request_timeout_ms == 0
            || config.poll_interval_ms == 0
            || config.edge_double_tap_ms == 0
            || config.lease_ttl_ms == 0
            || config.renew_interval_ms == 0
        {
            return Err(ConfigError::SwitchController(
                "all controller timing values must be non-zero".to_string(),
            ));
        }
        if config.http_timeout_ms > config.request_timeout_ms {
            return Err(ConfigError::SwitchController(
                "http_timeout_ms must not exceed request_timeout_ms".to_string(),
            ));
        }
        if config.poll_interval_ms >= config.request_timeout_ms {
            return Err(ConfigError::SwitchController(
                "poll_interval_ms must be less than request_timeout_ms".to_string(),
            ));
        }
        if config.request_timeout_ms >= config.lease_ttl_ms {
            return Err(ConfigError::SwitchController(
                "lease_ttl_ms must exceed request_timeout_ms".to_string(),
            ));
        }
        if config.renew_interval_ms >= config.lease_ttl_ms {
            return Err(ConfigError::SwitchController(
                "renew_interval_ms must be less than lease_ttl_ms".to_string(),
            ));
        }

        Ok(Self {
            url,
            token: config.token,
            local_host: config.local_host,
            server_host: config.server_host,
            http_timeout_ms: config.http_timeout_ms,
            request_timeout_ms: config.request_timeout_ms,
            poll_interval_ms: config.poll_interval_ms,
            edge_double_tap_ms: config.edge_double_tap_ms,
            lease_ttl_ms: config.lease_ttl_ms,
            renew_interval_ms: config.renew_interval_ms,
        })
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
struct TomlClient {
    hostname: Option<String>,
    host_name: Option<String>,
    ips: Option<Vec<IpAddr>>,
    port: Option<u16>,
    position: Option<Position>,
    activate_on_startup: Option<bool>,
    switch_target: Option<SwitchHost>,
}

impl ConfigToml {
    fn new(path: &Path) -> Result<ConfigToml, ConfigError> {
        let config = fs::read_to_string(path)?;
        Ok(toml::from_str::<_>(&config)?)
    }
}

#[derive(Parser, Debug)]
#[command(author, version=build::CLAP_LONG_VERSION, about, long_about = None)]
struct Args {
    /// the listen port for lan-mouse
    #[arg(short, long)]
    port: Option<u16>,

    /// non-default config file location
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// capture backend override
    #[arg(long)]
    capture_backend: Option<CaptureBackend>,

    /// emulation backend override
    #[arg(long)]
    emulation_backend: Option<EmulationBackend>,

    /// path to non-default certificate location
    #[arg(long)]
    cert_path: Option<PathBuf>,

    /// subcommands
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// test input emulation
    TestEmulation(TestEmulationArgs),
    /// test input capture
    TestCapture(TestCaptureArgs),
    /// Lan Mouse commandline interface
    Cli(CliArgs),
    /// run in daemon mode
    Daemon,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum CaptureBackend {
    #[cfg(libei_capture)]
    #[serde(rename = "input-capture-portal")]
    InputCapturePortal,
    #[cfg(layer_shell_capture)]
    #[serde(rename = "layer-shell")]
    LayerShell,
    #[cfg(x11_capture)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl Display for CaptureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11_capture)]
            CaptureBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            CaptureBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => write!(f, "MacOS"),
            CaptureBackend::Dummy => write!(f, "dummy"),
        }
    }
}

impl From<CaptureBackend> for input_capture::Backend {
    fn from(backend: CaptureBackend) -> Self {
        match backend {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => Self::InputCapturePortal,
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => Self::LayerShell,
            #[cfg(x11_capture)]
            CaptureBackend::X11 => Self::X11,
            #[cfg(windows)]
            CaptureBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => Self::MacOs,
            CaptureBackend::Dummy => Self::Dummy,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum EmulationBackend {
    #[cfg(wlroots_emulation)]
    #[serde(rename = "wlroots")]
    Wlroots,
    #[cfg(libei_emulation)]
    #[serde(rename = "libei")]
    Libei,
    #[cfg(rdp_emulation)]
    #[serde(rename = "xdp")]
    Xdp,
    #[cfg(x11_emulation)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl From<EmulationBackend> for input_emulation::Backend {
    fn from(backend: EmulationBackend) -> Self {
        match backend {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => Self::Wlroots,
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => Self::Libei,
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => Self::Xdp,
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => Self::X11,
            #[cfg(windows)]
            EmulationBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => Self::MacOs,
            EmulationBackend::Dummy => Self::Dummy,
        }
    }
}

impl Display for EmulationBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => write!(f, "libei"),
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            EmulationBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => write!(f, "macos"),
            EmulationBackend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct Config {
    /// command line arguments
    args: Args,
    /// path to the certificate file used
    cert_path: PathBuf,
    /// path to the config file used
    config_path: PathBuf,
    /// path to config directory (parent of above)
    config_dir: PathBuf,
    /// the (optional) toml config and it's path
    config_toml: Option<ConfigToml>,
    // filesystem watcher
    watcher: notify::RecommendedWatcher,
    // channel for filesystem events
    watch_rx: tokio::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
}

#[derive(Clone)]
pub(crate) struct ConfigClient {
    pub ips: HashSet<IpAddr>,
    pub hostname: Option<String>,
    pub port: u16,
    pub pos: Position,
    pub active: bool,
    pub switch_target: Option<SwitchHost>,
}

impl From<TomlClient> for ConfigClient {
    fn from(toml: TomlClient) -> Self {
        let active = toml.activate_on_startup.unwrap_or(false);
        let switch_target = toml.switch_target;
        let hostname = toml.hostname;
        let ips = HashSet::from_iter(toml.ips.into_iter().flatten());
        let port = toml.port.unwrap_or(DEFAULT_PORT);
        let pos = toml.position.unwrap_or_default();
        Self {
            ips,
            hostname,
            port,
            pos,
            active,
            switch_target,
        }
    }
}

impl From<ConfigClient> for TomlClient {
    fn from(client: ConfigClient) -> Self {
        let hostname = client.hostname;
        let host_name = None;
        let mut ips = client.ips.into_iter().collect::<Vec<_>>();
        ips.sort();
        let ips = Some(ips);
        let port = if client.port == DEFAULT_PORT {
            None
        } else {
            Some(client.port)
        };
        let position = Some(client.pos);
        let activate_on_startup = if client.active { Some(true) } else { None };
        let switch_target = client.switch_target;
        Self {
            hostname,
            host_name,
            ips,
            port,
            position,
            activate_on_startup,
            switch_target,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Var(#[from] VarError),
    #[error(transparent)]
    Watcher(#[from] notify::Error),
    #[error("invalid switch controller configuration: {0}")]
    SwitchController(String),
    #[error("invalid clipboard configuration: {0}")]
    Clipboard(String),
}

const DEFAULT_RELEASE_KEYS: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

impl Config {
    pub fn new() -> Result<Self, ConfigError> {
        let args = Args::parse();

        // --config <file> overrules default location
        let config_path = args
            .config
            .clone()
            .unwrap_or(default_path()?.join(CONFIG_FILE_NAME));
        let config_dir = config_path
            .parent()
            .expect("config directory")
            .to_path_buf();

        // Ensure the config directory exists and write a default config file
        // if none is present. Runs on every Config::new(), regardless of which
        // entry path (GUI main, spawned daemon, CLI, test commands) we're on,
        // so a fresh Mac never hits "No such file or directory" on config.toml
        // and notify::Watcher (which requires the dir to exist on macOS
        // FSEvents and some Linux backends) has a concrete path to watch.
        fs::create_dir_all(&config_dir)?;
        if !config_path.exists() {
            let default_toml = toml::to_string_pretty(&ConfigToml::default())
                .expect("default ConfigToml serialization cannot fail");
            fs::write(&config_path, default_toml)?;
        }

        let config_toml = match ConfigToml::new(&config_path) {
            Err(e) => {
                log::warn!("{config_path:?}: {e}");
                log::warn!("Continuing without config file ...");
                None
            }
            Ok(c) => Some(c),
        };
        if let Some(controller) = config_toml
            .as_ref()
            .and_then(|config| config.switch_controller.clone())
        {
            SwitchControllerConfig::try_from(controller)?;
        }
        if let Some(clipboard) = config_toml.as_ref().and_then(|config| config.clipboard) {
            ClipboardConfig::try_from(clipboard)?;
        }

        // --cert-path <file> overrules default location
        let cert_path = args
            .cert_path
            .clone()
            .or(config_toml.as_ref().and_then(|c| c.cert_path.clone()))
            .unwrap_or(default_path()?.join(CERT_FILE_NAME));

        let (tx, watch_rx) = tokio::sync::mpsc::channel(16);
        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            notify::Config::default(),
        )?;
        let mut config = Config {
            args,
            cert_path,
            config_path,
            config_dir,
            config_toml,
            watcher,
            watch_rx,
        };
        config.watch()?;
        Ok(config)
    }

    fn watch(&mut self) -> Result<(), notify::Error> {
        self.watcher
            .watch(&self.config_dir, notify::RecursiveMode::NonRecursive)?;
        Ok(())
    }

    fn unwatch(&mut self) -> Result<(), notify::Error> {
        self.watcher.unwatch(&self.config_dir)?;
        Ok(())
    }

    pub async fn changed(&mut self) -> Result<(), notify::Error> {
        loop {
            let event = self.watch_rx.recv().await.expect("channel closed");
            let event = event.expect("filesystem event");
            if event.paths.contains(&self.config_path)
                && matches!(
                    event.kind,
                    EventKind::Create(_)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Remove(_)
                )
                && self.read_from_disk()?
            {
                return Ok(());
            }
        }
    }

    /// the command to run
    pub fn command(&self) -> Option<Command> {
        self.args.command.clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// public key fingerprints authorized for connection
    pub fn authorized_fingerprints(&self) -> HashMap<String, String> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.authorized_fingerprints.clone())
            .unwrap_or_default()
    }

    /// path to certificate
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// optional input-capture backend override
    pub fn capture_backend(&self) -> Option<CaptureBackend> {
        self.args
            .capture_backend
            .or(self.config_toml.as_ref().and_then(|c| c.capture_backend))
    }

    /// optional input-emulation backend override
    pub fn emulation_backend(&self) -> Option<EmulationBackend> {
        self.args
            .emulation_backend
            .or(self.config_toml.as_ref().and_then(|c| c.emulation_backend))
    }

    /// Optional backend-specific display selector used for pointer placement.
    pub fn emulation_display(&self) -> Option<String> {
        self.config_toml
            .as_ref()
            .and_then(|config| config.emulation_display.clone())
    }

    /// the port to use (initially)
    pub fn port(&self) -> u16 {
        self.args
            .port
            .or(self.config_toml.as_ref().and_then(|c| c.port))
            .unwrap_or(DEFAULT_PORT)
    }

    /// list of configured clients
    pub(crate) fn clients(&self) -> Vec<ConfigClient> {
        self.config_toml
            .as_ref()
            .map(|c| c.clients.clone())
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .map(From::<TomlClient>::from)
            .collect()
    }

    pub(crate) fn switch_controller(&self) -> Option<SwitchControllerConfig> {
        self.config_toml
            .as_ref()
            .and_then(|config| config.switch_controller.clone())
            .map(SwitchControllerConfig::try_from)
            .transpose()
            .expect("switch controller was validated before installation")
    }

    pub(crate) fn clipboard(&self) -> ClipboardConfig {
        self.config_toml
            .as_ref()
            .and_then(|config| config.clipboard)
            .unwrap_or_default()
            .try_into()
            .expect("clipboard configuration was validated before installation")
    }

    /// release bind for returning control to the host
    pub fn release_bind(&self) -> Vec<scancode::Linux> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.release_bind.clone())
            .unwrap_or(Vec::from_iter(DEFAULT_RELEASE_KEYS.iter().cloned()))
    }

    /// set configured clients
    pub(crate) fn set_clients(&mut self, clients: Vec<ConfigClient>) {
        if clients.is_empty() {
            return;
        }
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").clients =
            Some(clients.into_iter().map(|c| c.into()).collect::<Vec<_>>());
    }

    /// set authorized keys
    pub fn set_authorized_keys(&mut self, fingerprints: HashMap<String, String>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml
            .as_mut()
            .expect("config")
            .authorized_fingerprints = Some(fingerprints);
    }

    pub fn read_from_disk(&mut self) -> Result<bool, io::Error> {
        log::info!("reading config from {:?}", &self.config_path);

        let current_config = fs::read_to_string(&self.config_path)?;
        let current_config = match current_config.parse::<DocumentMut>() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("{:?} {e}", self.config_path());
                return Ok(false);
            }
        };
        let mut changed = false;
        match toml_edit::de::from_document::<ConfigToml>(current_config) {
            Ok(current_config) => {
                if let Some(controller) = current_config.switch_controller.clone() {
                    if let Err(error) = SwitchControllerConfig::try_from(controller) {
                        log::warn!("{:?}: {error}", self.config_path());
                        return Ok(false);
                    }
                }
                if let Some(clipboard) = current_config.clipboard {
                    if let Err(error) = ClipboardConfig::try_from(clipboard) {
                        log::warn!("{:?}: {error}", self.config_path());
                        return Ok(false);
                    }
                }
                changed = self
                    .config_toml
                    .as_ref()
                    .is_none_or(|c| c != &current_config);
                self.config_toml.replace(current_config);
            }
            Err(e) => log::warn!("{:?} {e}", self.config_path()),
        };
        if changed {
            log::info!("config changed");
        } else {
            log::info!("config unchanged");
        }
        Ok(changed)
    }

    pub fn write_back(&mut self) -> Result<(), io::Error> {
        log::info!("writing config to {:?}", &self.config_path);
        /* the new config */
        let new_config = self.config_toml.clone().unwrap_or_default();
        let new_config = toml_edit::ser::to_string_pretty(&new_config).expect("config");

        /*
         * TODO merge with current config file to preserve comments
         * => eventually we might want to split this up into clients configured
         * via the config file and clients managed through the GUI / frontend.
         * The latter should be saved to $XDG_DATA_HOME instead of $XDG_CONFIG_HOME,
         * and clients configured through .config could be made permanent.
         * For now we just override the config file.
         */

        let _ = self.unwatch();
        /* write new config to file */
        if let Some(p) = self.config_path().parent() {
            fs::create_dir_all(p)?;
        }
        {
            let mut f = File::create(self.config_path())?;
            f.write_all(new_config.as_bytes())?;
            f.sync_all()?;
        }

        let _ = self.watch();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller_toml() -> SwitchControllerToml {
        SwitchControllerToml {
            url: "http://127.0.0.1:9876".to_string(),
            token: "test-token".to_string(),
            local_host: SwitchHost::Linux,
            server_host: SwitchHost::Linux,
            http_timeout_ms: 500,
            request_timeout_ms: 2_000,
            poll_interval_ms: 100,
            edge_double_tap_ms: 500,
            lease_ttl_ms: 5_000,
            renew_interval_ms: 1_000,
        }
    }

    #[test]
    fn validates_complete_switch_controller() {
        let controller = SwitchControllerConfig::try_from(controller_toml()).unwrap();

        assert_eq!(controller.url.as_str(), "http://127.0.0.1:9876/");
        assert_eq!(controller.server_host, SwitchHost::Linux);
    }

    #[test]
    fn rejects_lease_that_can_expire_before_request_deadline() {
        let mut config = controller_toml();
        config.lease_ttl_ms = config.request_timeout_ms;

        assert!(matches!(
            SwitchControllerConfig::try_from(config),
            Err(ConfigError::SwitchController(_))
        ));
    }

    #[test]
    fn rejects_disabled_edge_intent_deadline() {
        let mut config = controller_toml();
        config.edge_double_tap_ms = 0;

        assert!(matches!(
            SwitchControllerConfig::try_from(config),
            Err(ConfigError::SwitchController(_))
        ));
    }

    #[test]
    fn rejects_url_with_embedded_credentials() {
        let mut config = controller_toml();
        config.url = "http://user:secret@127.0.0.1:9876".to_string();

        assert!(matches!(
            SwitchControllerConfig::try_from(config),
            Err(ConfigError::SwitchController(_))
        ));
    }

    #[test]
    fn clipboard_defaults_to_enabled_with_three_megabyte_limit() {
        assert_eq!(
            ClipboardConfig::try_from(ClipboardToml::default()).unwrap(),
            ClipboardConfig {
                enabled: true,
                max_bytes: 3 * 1024 * 1024,
            }
        );
    }

    #[test]
    fn rejects_zero_clipboard_limit() {
        assert!(matches!(
            ClipboardConfig::try_from(ClipboardToml {
                enabled: true,
                max_bytes: 0,
            }),
            Err(ConfigError::Clipboard(_))
        ));
    }

    #[test]
    fn partial_clipboard_table_uses_documented_defaults() {
        let config: ConfigToml = toml::from_str("[clipboard]\nenabled = false\n").unwrap();

        assert_eq!(
            config.clipboard,
            Some(ClipboardToml {
                enabled: false,
                max_bytes: 3 * 1024 * 1024,
            })
        );
    }
}
