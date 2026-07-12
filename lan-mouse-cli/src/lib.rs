use clap::{Args, Parser, Subcommand};
use futures::StreamExt;
use serde::Serialize;

use std::{net::IpAddr, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, ConnectionError, FrontendEvent, FrontendRequest,
    IpcError, Position, SwitchHost, connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
    #[error("error serializing service status: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "lan-mouse-cli", about = "LanMouse CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    switch_target: Option<SwitchHost>,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List {
        /// emit one JSON array for deployment and diagnostics
        #[arg(long)]
        json: bool,
    },
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// set the display/input host selected for this client
    SetSwitchTarget {
        id: ClientHandle,
        target: Option<SwitchHost>,
    },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
}

#[derive(Debug, Serialize)]
struct ClientStatus {
    id: ClientHandle,
    hostname: Option<String>,
    port: u16,
    position: Position,
    switch_target: Option<SwitchHost>,
    active: bool,
    alive: bool,
    keyboard_ready: bool,
    pointer_ready: bool,
    peer_session_epoch: u64,
    peer_commit: Option<String>,
}

impl ClientStatus {
    fn from_client(id: ClientHandle, config: ClientConfig, state: ClientState) -> Self {
        Self {
            id,
            hostname: config.hostname,
            port: config.port,
            position: config.pos,
            switch_target: config.switch_target,
            active: state.active,
            alive: state.alive,
            keyboard_ready: state.keyboard_ready,
            pointer_ready: state.pointer_ready,
            peer_session_epoch: state.peer_session_epoch,
            peer_commit: state
                .peer_commit
                .map(|commit| String::from_utf8_lossy(&commit).into_owned()),
        }
    }
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            switch_target,
        }) => {
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
                    if let Some(hostname) = hostname {
                        tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                            .await?;
                    }
                    if let Some(port) = port {
                        tx.request(FrontendRequest::UpdatePort(handle, port))
                            .await?;
                    }
                    if let Some(ips) = ips {
                        tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                            .await?;
                    }
                    if let Some(target) = switch_target {
                        tx.request(FrontendRequest::UpdateSwitchTarget(handle, Some(target)))
                            .await?;
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List { json } => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
                    if json {
                        let clients = clients
                            .into_iter()
                            .map(|(handle, config, state)| {
                                ClientStatus::from_client(handle, config, state)
                            })
                            .collect::<Vec<_>>();
                        println!("{}", serde_json::to_string(&clients)?);
                    } else {
                        for (handle, config, state) in clients {
                            let host = config.hostname.unwrap_or("unknown".to_owned());
                            let port = config.port;
                            let pos = config.pos;
                            let active = state.active;
                            let ips = state.ips;
                            println!(
                                "id {handle}: {host}:{port} ({pos}) active: {active}, ips: {ips:?}"
                            );
                        }
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::SetIps { id, ips } => {
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        CliSubcommand::SetSwitchTarget { id, target } => {
            tx.request(FrontendRequest::UpdateSwitchTarget(id, target))
                .await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_json_flag_is_explicit() {
        let args = CliArgs::try_parse_from(["lan-mouse-cli", "list", "--json"]).unwrap();
        assert_eq!(args.command, CliSubcommand::List { json: true });
    }

    #[test]
    fn client_status_serializes_bundle_readiness_identity() {
        let config = ClientConfig {
            hostname: Some("mac".to_string()),
            switch_target: Some(SwitchHost::Mac),
            ..ClientConfig::default()
        };
        let state = ClientState {
            active: true,
            alive: true,
            keyboard_ready: true,
            pointer_ready: true,
            peer_session_epoch: 17,
            peer_commit: Some(*b"4425c578"),
            ..ClientState::default()
        };

        let status = ClientStatus::from_client(3, config, state);
        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["id"], 3);
        assert_eq!(json["switch_target"], "mac");
        assert_eq!(json["alive"], true);
        assert_eq!(json["keyboard_ready"], true);
        assert_eq!(json["pointer_ready"], true);
        assert_eq!(json["peer_session_epoch"], 17);
        assert_eq!(json["peer_commit"], "4425c578");
    }
}
