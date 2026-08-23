use std::{
    cell::RefCell,
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    rc::Rc,
};

use slab::Slab;

use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState, Position};

use crate::config::{ConfigClient, local_commit};

fn peer_protocol_compatible(state: &ClientState) -> bool {
    state.peer_commit == Some(local_commit())
}

#[derive(Clone, Default)]
pub struct ClientManager {
    clients: Rc<RefCell<Slab<(ClientConfig, ClientState)>>>,
}

impl ClientManager {
    /// get all clients
    pub fn clients(&self) -> Vec<(ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(_, c)| c.clone())
            .collect::<Vec<_>>()
    }

    pub(crate) fn add_with_config(&self, config_client: ConfigClient) -> ClientHandle {
        let config = ClientConfig {
            hostname: config_client.hostname,
            fix_ips: config_client.ips.into_iter().collect(),
            port: config_client.port,
            pos: config_client.pos,
            switch_target: config_client.switch_target,
        };
        let state = ClientState {
            active: config_client.active,
            ips: HashSet::from_iter(config.fix_ips.iter().cloned()),
            ..Default::default()
        };
        let handle = self.add_client();
        self.set_config(handle, config);
        self.set_state(handle, state);
        handle
    }

    /// add a new client to this manager
    pub fn add_client(&self) -> ClientHandle {
        self.clients.borrow_mut().insert(Default::default()) as ClientHandle
    }

    /// set the config of the given client
    pub fn set_config(&self, handle: ClientHandle, config: ClientConfig) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *c = config;
        }
    }

    /// set the state of the given client
    pub fn set_state(&self, handle: ClientHandle, state: ClientState) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *s = state;
        }
    }

    /// activate the given client
    /// returns, whether the client was activated
    pub fn activate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if !s.active => {
                s.active = true;
                true
            }
            _ => false,
        }
    }

    /// deactivate the given client
    /// returns, whether the client was deactivated
    pub fn deactivate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if s.active => {
                s.active = false;
                true
            }
            _ => false,
        }
    }

    /// find a client by its address
    pub fn get_client(&self, addr: SocketAddr) -> Option<ClientHandle> {
        // since there shouldn't be more than a handful of clients at any given
        // time this is likely faster than using a HashMap
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (_, s))| {
                if s.active && s.ips.contains(&addr.ip()) {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    /// get the client at the given position
    pub fn client_at(&self, pos: Position) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (c, s))| {
                if s.active && c.pos == pos {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    pub(crate) fn get_hostname(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow_mut()
            .get_mut(handle as usize)
            .and_then(|(c, _)| c.hostname.clone())
    }

    /// get the position of the corresponding client
    pub(crate) fn get_pos(&self, handle: ClientHandle) -> Option<Position> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.pos)
    }

    /// remove a client from the list
    pub fn remove_client(&self, client: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        // remove id from occupied ids
        self.clients.borrow_mut().try_remove(client as usize)
    }

    /// get the config & state of the given client
    pub fn get_state(&self, handle: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        self.clients.borrow().get(handle as usize).cloned()
    }

    /// get the current config & state of all clients
    pub fn get_client_states(&self) -> Vec<(ClientHandle, ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(k, v)| (k as ClientHandle, v.0.clone(), v.1.clone()))
            .collect()
    }

    /// update the fix ips of the client
    pub fn set_fix_ips(&self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.fix_ips = fix_ips
        }
        self.update_ips(handle);
    }

    /// update the dns-ips of the client
    pub fn set_dns_ips(&self, handle: ClientHandle, dns_ips: Vec<IpAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.dns_ips = dns_ips
        }
        self.update_ips(handle);
    }

    fn update_ips(&self, handle: ClientHandle) {
        if let Some((c, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.ips = c
                .fix_ips
                .iter()
                .cloned()
                .chain(s.dns_ips.iter().cloned())
                .collect::<HashSet<_>>();
        }
    }

    /// update the hostname of the given client
    /// this automatically clears the active ip address and ips from dns
    pub fn set_hostname(&self, handle: ClientHandle, hostname: Option<String>) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((c, s)) = clients.get_mut(handle as usize) else {
            return false;
        };

        // hostname changed
        if c.hostname != hostname {
            c.hostname = hostname;
            s.active_addr = None;
            s.dns_ips.clear();
            drop(clients);
            self.update_ips(handle);
            true
        } else {
            false
        }
    }

    /// update the port of the client
    pub(crate) fn set_port(&self, handle: ClientHandle, port: u16) {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.port != port => {
                c.port = port;
                s.active_addr = s.active_addr.map(|a| SocketAddr::new(a.ip(), port));
            }
            _ => {}
        };
    }

    /// update the position of the client
    /// returns true, if a change in capture position is required (pos changed & client is active)
    pub(crate) fn set_pos(&self, handle: ClientHandle, pos: Position) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.pos != pos => {
                log::info!("update pos {handle} {} -> {}", c.pos, pos);
                c.pos = pos;
                s.active
            }
            _ => false,
        }
    }

    pub(crate) fn set_switch_target(
        &self,
        handle: ClientHandle,
        switch_target: Option<lan_mouse_ipc::SwitchHost>,
    ) {
        if let Some((c, _s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.switch_target = switch_target;
        }
    }

    /// set resolving status of the client
    pub(crate) fn set_resolving(&self, handle: ClientHandle, status: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.resolving = status;
        }
    }

    pub(crate) fn switch_target(&self, handle: ClientHandle) -> Option<lan_mouse_ipc::SwitchHost> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(c, _)| c.switch_target)
    }

    /// returns all clients that are currently registered
    pub(crate) fn registered_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    /// returns all clients that are currently active
    pub(crate) fn active_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (_, s))| s.active)
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    pub(crate) fn set_active_addr(&self, handle: ClientHandle, addr: Option<SocketAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.active_addr = addr;
        }
    }

    pub(crate) fn set_alive(&self, handle: ClientHandle, alive: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.alive = alive;
        }
    }

    pub(crate) fn set_peer_commit(&self, handle: ClientHandle, commit: Option<[u8; 8]>) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((_, state)) = clients.get_mut(handle as usize) else {
            return false;
        };
        if state.peer_commit == commit {
            return false;
        }
        state.peer_commit = commit;
        true
    }

    pub(crate) fn set_peer_readiness(
        &self,
        handle: ClientHandle,
        keyboard_ready: bool,
        pointer_ready: bool,
        session_epoch: u64,
    ) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((_, state)) = clients.get_mut(handle as usize) else {
            return false;
        };

        if session_epoch == 0 && (keyboard_ready || pointer_ready) {
            return false;
        }

        state.keyboard_ready = keyboard_ready;
        state.pointer_ready = pointer_ready;
        state.peer_session_epoch = session_epoch;
        true
    }

    pub(crate) fn clear_peer_readiness(&self, handle: ClientHandle) {
        if let Some((_, state)) = self.clients.borrow_mut().get_mut(handle as usize) {
            state.keyboard_ready = false;
            state.pointer_ready = false;
            state.peer_session_epoch = 0;
        }
    }

    #[cfg(test)]
    pub(crate) fn peer_bundle_ready(&self, handle: ClientHandle) -> Option<(bool, u64)> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, state)| {
                (
                    state.alive
                        && peer_protocol_compatible(state)
                        && state.keyboard_ready
                        && state.pointer_ready,
                    state.peer_session_epoch,
                )
            })
    }

    pub(crate) fn peer_input_readiness(
        &self,
        handle: ClientHandle,
    ) -> Option<(bool, bool, bool, u64)> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, state)| {
                let compatible = peer_protocol_compatible(state);
                (
                    state.alive,
                    compatible && state.keyboard_ready,
                    compatible && state.pointer_ready,
                    state.peer_session_epoch,
                )
            })
    }

    pub(crate) fn peer_protocol_compatible(&self, handle: ClientHandle) -> bool {
        self.clients
            .borrow()
            .get(handle as usize)
            .is_some_and(|(_, state)| peer_protocol_compatible(state))
    }

    pub(crate) fn active_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.active_addr)
    }

    pub(crate) fn alive(&self, handle: ClientHandle) -> bool {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.alive)
            .unwrap_or(false)
    }

    pub(crate) fn get_port(&self, handle: ClientHandle) -> Option<u16> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.port)
    }

    pub(crate) fn get_ips(&self, handle: ClientHandle) -> Option<HashSet<IpAddr>> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.ips.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_readiness_requires_connection_and_both_capabilities() {
        let clients = ClientManager::default();
        let handle = clients.add_client();
        clients.set_peer_commit(handle, Some(local_commit()));

        assert_eq!(clients.peer_bundle_ready(handle), Some((false, 0)));

        clients.set_alive(handle, true);
        assert!(clients.set_peer_readiness(handle, true, false, 10));
        assert_eq!(clients.peer_bundle_ready(handle), Some((false, 10)));

        assert!(clients.set_peer_readiness(handle, true, true, 10));
        assert_eq!(clients.peer_bundle_ready(handle), Some((true, 10)));
    }

    #[test]
    fn disconnect_reset_accepts_new_process_epoch() {
        let clients = ClientManager::default();
        let handle = clients.add_client();
        clients.set_peer_commit(handle, Some(local_commit()));
        clients.set_alive(handle, true);
        assert!(clients.set_peer_readiness(handle, true, true, 100));

        clients.set_alive(handle, false);
        clients.clear_peer_readiness(handle);
        clients.set_alive(handle, true);
        assert!(clients.set_peer_readiness(handle, true, true, 3));

        assert_eq!(clients.peer_bundle_ready(handle), Some((true, 3)));
    }

    #[test]
    fn zero_epoch_can_never_report_ready() {
        let clients = ClientManager::default();
        let handle = clients.add_client();
        clients.set_peer_commit(handle, Some(local_commit()));
        clients.set_alive(handle, true);

        assert!(!clients.set_peer_readiness(handle, true, true, 0));
        assert_eq!(clients.peer_bundle_ready(handle), Some((false, 0)));
    }

    #[test]
    fn unknown_or_mismatched_commit_masks_control_readiness() {
        let clients = ClientManager::default();
        let handle = clients.add_client();
        clients.set_alive(handle, true);
        assert!(clients.set_peer_readiness(handle, true, true, 10));

        assert_eq!(
            clients.peer_input_readiness(handle),
            Some((true, false, false, 10))
        );

        let mut mismatch = local_commit();
        mismatch[0] ^= 0xff;
        assert!(clients.set_peer_commit(handle, Some(mismatch)));
        assert_eq!(clients.peer_bundle_ready(handle), Some((false, 10)));

        assert!(clients.set_peer_commit(handle, Some(local_commit())));
        assert_eq!(clients.peer_bundle_ready(handle), Some((true, 10)));
        assert_eq!(
            clients.peer_input_readiness(handle),
            Some((true, true, true, 10))
        );
    }
}
