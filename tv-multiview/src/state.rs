// TLA+ state variables mapped to Rust.
// Mirrors the TvDisplaySwitch module from fullscreenmultiviewswitchdesign.md.

use serde::Serialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Instant;

// ---- Type definitions (TLA+ CONSTANTS) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TvMode {
    Fullscreen,
    Multiview,
    Transitioning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    Linux,
    Mac,
    Windows,
    Unknown,
}

impl Input {
    pub fn from_str(s: &str) -> Self {
        match s {
            "linux" => Input::Linux,
            "mac" => Input::Mac,
            "windows" => Input::Windows,
            _ => Input::Unknown,
        }
    }
}

// ---- TLA+ VARIABLES mapped to Rust struct ----

pub struct TvDaemonState {
    pub tv_mode: Mutex<TvMode>,
    pub tv_input: Mutex<Input>,
    pub healthy: Mutex<bool>,
    pub pending: Mutex<Option<Input>>,
    pub reconnect_count: AtomicU32,
    pub switch_count: Mutex<SwitchCount>,
    pub last_error: Mutex<Option<String>>,
    pub start_time: Instant,
}

#[derive(Debug, Default, Serialize)]
pub struct SwitchCount {
    pub linux: u64,
    pub mac: u64,
    pub windows: u64,
}

// ---- TLA+ Init ----

impl Default for TvDaemonState {
    fn default() -> Self {
        Self {
            tv_mode: Mutex::new(TvMode::Fullscreen),
            tv_input: Mutex::new(Input::Linux),
            healthy: Mutex::new(false),
            pending: Mutex::new(None),
            reconnect_count: AtomicU32::new(0),
            switch_count: Mutex::new(SwitchCount::default()),
            last_error: Mutex::new(None),
            start_time: Instant::now(),
        }
    }
}

// ---- TLA+ TRANSITIONS implemented as methods ----

impl TvDaemonState {
    // --- EnterOtherHost (F→T) ---
    // Guard: tv_mode=Fullscreen, pending=None, healthy, tv_input != target
    // Effect: pending=target, tv_mode=Transitioning, tv_input=target
    pub fn enter_other_host(&self, target: Input) -> bool {
        let mut mode = self.tv_mode.lock().unwrap();
        let mut input = self.tv_input.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        let healthy = *self.healthy.lock().unwrap();

        if *mode != TvMode::Fullscreen || pending.is_some() || !healthy {
            return false;
        }
        if *input == target {
            return false; // C6: no-op if already on target
        }

        *mode = TvMode::Transitioning;
        *input = target;
        *pending = Some(target);
        true
    }

    // --- SwitchComplete (T→F) ---
    pub fn switch_complete(&self) {
        let mut mode = self.tv_mode.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        *mode = TvMode::Fullscreen;
        *pending = None;
    }

    // --- ReturnToLinux (→T→F) ---
    // Guard: pending=None
    // Fullscreen branch: pending=Linux, tv_mode=Transitioning, tv_input=Linux
    // Else branch: leave TV alone
    pub fn return_to_linux(&self) -> Option<bool> {
        let mode = self.tv_mode.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        let healthy = *self.healthy.lock().unwrap();

        if pending.is_some() {
            return Some(false); // debounce
        }

        if *mode == TvMode::Fullscreen {
            if !healthy {
                return Some(false);
            }
            drop(mode);
            let mut mode = self.tv_mode.lock().unwrap();
            let mut input = self.tv_input.lock().unwrap();
            *mode = TvMode::Transitioning;
            *input = Input::Linux;
            *pending = Some(Input::Linux);
            Some(true) // caller must issue set_input
        } else {
            Some(false) // multiview or transitioning: no TV change
        }
    }

    // --- EnterMultiView (F→M, atomic) ---
    // C1 fix: pending cleared directly, no transitioning
    pub fn enter_multiview(&self) -> bool {
        let mut mode = self.tv_mode.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        let healthy = *self.healthy.lock().unwrap();

        if *mode != TvMode::Fullscreen || pending.is_some() || !healthy {
            return false;
        }

        *mode = TvMode::Multiview;
        *pending = None; // atomic
        true
    }

    // --- ExitMultiView (M→F, atomic) ---
    // C1 fix: pending cleared directly, no transitioning
    pub fn exit_multiview(&self, target: Input) -> bool {
        let mut mode = self.tv_mode.lock().unwrap();
        let mut input = self.tv_input.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        let healthy = *self.healthy.lock().unwrap();

        if *mode != TvMode::Multiview || pending.is_some() || !healthy {
            return false;
        }

        *mode = TvMode::Fullscreen;
        *input = target;
        *pending = None; // atomic
        true
    }

    // --- EnterMultiViewHost (capture-only, used in HTTP handler inline) ---
    #[allow(dead_code)]
    pub fn enter_multiview_host(&self) -> bool {
        let mode = self.tv_mode.lock().unwrap();
        let pending = self.pending.lock().unwrap();
        *mode == TvMode::Multiview && pending.is_none()
    }

    // --- DaemonDies ---
    pub fn mark_dead(&self) {
        *self.healthy.lock().unwrap() = false;
        *self.pending.lock().unwrap() = None;
        self.reconnect_count.store(0, Ordering::SeqCst);
    }

    // --- ReconnectFails ---
    pub fn reconnect_failed(&self) -> bool {
        let count = self.reconnect_count.load(Ordering::SeqCst);
        if count < 30 {
            self.reconnect_count.store(count + 1, Ordering::SeqCst);
            true
        } else {
            false // DaemonExits: cap reached, systemd restarts
        }
    }

    // --- DaemonReconnects ---
    pub fn mark_healthy(&self) {
        *self.healthy.lock().unwrap() = true;
        *self.pending.lock().unwrap() = None;
        *self.tv_mode.lock().unwrap() = TvMode::Fullscreen;
        *self.tv_input.lock().unwrap() = Input::Unknown;
        self.reconnect_count.store(0, Ordering::SeqCst);
    }

    // --- TvRemoteOverride ---
    // C4 fix: clears pending_switch, restricts to {Fullscreen, Multiview}
    #[allow(dead_code)]
    pub fn remote_override(&self, new_mode: TvMode) {
        if new_mode != TvMode::Fullscreen && new_mode != TvMode::Multiview {
            return; // C4: remote can't set Transitioning
        }
        *self.tv_mode.lock().unwrap() = new_mode;
        *self.pending.lock().unwrap() = None; // C4: clear pending
    }

    // --- Status snapshot (for /status endpoint) ---
    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": *self.tv_mode.lock().unwrap(),
            "input": *self.tv_input.lock().unwrap(),
            "healthy": *self.healthy.lock().unwrap(),
            "pending_switch": self.pending.lock().unwrap().map(|p| match p {
                Input::Linux => "linux",
                Input::Mac => "mac",
                Input::Windows => "windows",
                Input::Unknown => "unknown",
            }),
            "uptime_seconds": self.start_time.elapsed().as_secs(),
            "reconnect_total": self.reconnect_count.load(Ordering::SeqCst),
            "switch_count": &*self.switch_count.lock().unwrap(),
            "last_error": &*self.last_error.lock().unwrap(),
        })
    }
}

// ---- TLA+-verified unit tests ----

#[cfg(test)]
mod tests {
    use super::*;

    fn init() -> TvDaemonState {
        let s = TvDaemonState::default();
        *s.healthy.lock().unwrap() = true;
        s
    }

    // --- EnterOtherHost: no-op when already on target (C6) ---
    #[test]
    fn enter_other_host_noop_if_already_on_target() {
        let s = init();
        assert!(!s.enter_other_host(Input::Linux));
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Fullscreen);
    }

    // --- EnterOtherHost: sets transitioning + pending ---
    #[test]
    fn enter_other_host_transitions() {
        let s = init();
        assert!(s.enter_other_host(Input::Mac));
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Transitioning);
        assert_eq!(*s.tv_input.lock().unwrap(), Input::Mac);
        assert_eq!(*s.pending.lock().unwrap(), Some(Input::Mac));
    }

    // --- EnterOtherHost: blocked by pending_switch (C1 gate) ---
    #[test]
    fn enter_other_host_blocked_by_pending() {
        let s = init();
        s.enter_other_host(Input::Mac);
        assert!(!s.enter_other_host(Input::Windows));
    }

    // --- EnterOtherHost: blocked when unhealthy ---
    #[test]
    fn enter_other_host_blocked_when_unhealthy() {
        let s = init();
        *s.healthy.lock().unwrap() = false;
        assert!(!s.enter_other_host(Input::Mac));
    }

    // --- SwitchComplete: clears pending ---
    #[test]
    fn switch_complete_clears_pending() {
        let s = init();
        s.enter_other_host(Input::Mac);
        s.switch_complete();
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Fullscreen);
        assert_eq!(*s.pending.lock().unwrap(), None);
    }

    // --- ReturnToLinux: fullscreen path routes through transitioning (C1) ---
    #[test]
    fn return_to_linux_fullscreen_sets_transitioning() {
        let s = init();
        s.enter_other_host(Input::Mac);
        s.switch_complete();
        let should_switch = s.return_to_linux().unwrap();
        assert!(should_switch);
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Transitioning);
        assert_eq!(*s.tv_input.lock().unwrap(), Input::Linux);
        assert_eq!(*s.pending.lock().unwrap(), Some(Input::Linux));
    }

    // --- ReturnToLinux: multiview path, no TV change ---
    #[test]
    fn return_to_linux_multiview_noop() {
        let s = init();
        *s.tv_mode.lock().unwrap() = TvMode::Multiview;
        *s.pending.lock().unwrap() = None;
        let should_switch = s.return_to_linux().unwrap();
        assert!(!should_switch);
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Multiview);
    }

    // --- ReturnToLinux: blocked by pending (debounce) ---
    #[test]
    fn return_to_linux_blocked_by_pending() {
        let s = init();
        s.enter_other_host(Input::Mac);
        let should_switch = s.return_to_linux().unwrap();
        assert!(!should_switch);
    }

    // --- EnterMultiView: atomic, clears pending directly (C1) ---
    #[test]
    fn enter_multiview_atomic() {
        let s = init();
        assert!(s.enter_multiview());
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Multiview);
        assert_eq!(*s.pending.lock().unwrap(), None);
    }

    // --- EnterMultiView: blocked if already multiview ---
    #[test]
    fn enter_multiview_blocked_already_multiview() {
        let s = init();
        s.enter_multiview();
        assert!(!s.enter_multiview());
    }

    // --- EnterMultiView: blocked when unhealthy ---
    #[test]
    fn enter_multiview_blocked_unhealthy() {
        let s = init();
        *s.healthy.lock().unwrap() = false;
        assert!(!s.enter_multiview());
    }

    // --- ExitMultiView: atomic, clears pending directly (C1) ---
    #[test]
    fn exit_multiview_atomic() {
        let s = init();
        s.enter_multiview();
        assert!(s.exit_multiview(Input::Linux));
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Fullscreen);
        assert_eq!(*s.tv_input.lock().unwrap(), Input::Linux);
        assert_eq!(*s.pending.lock().unwrap(), None);
    }

    // --- ExitMultiView: blocked if in fullscreen ---
    #[test]
    fn exit_multiview_blocked_in_fullscreen() {
        let s = init();
        assert!(!s.exit_multiview(Input::Linux));
    }

    // --- DaemonDies: clears pending (NoPendingWhenDead invariant) ---
    #[test]
    fn daemon_dies_clears_pending() {
        let s = init();
        s.enter_other_host(Input::Mac);
        s.mark_dead();
        assert!(!*s.healthy.lock().unwrap());
        assert_eq!(*s.pending.lock().unwrap(), None);
    }

    // --- ReconnectFails: increments counter ---
    #[test]
    fn reconnect_fails_increments() {
        let s = init();
        s.mark_dead();
        let before = s.reconnect_count.load(Ordering::SeqCst);
        assert!(s.reconnect_failed());
        assert_eq!(s.reconnect_count.load(Ordering::SeqCst), before + 1);
    }

    // --- ReconnectFails: returns false at cap (C3 DaemonExits) ---
    #[test]
    fn reconnect_fails_cap_returns_false() {
        let s = init();
        s.mark_dead();
        s.reconnect_count.store(30, Ordering::SeqCst);
        assert!(!s.reconnect_failed());
    }

    // --- DaemonReconnects: clears pending (C2 fix) ---
    #[test]
    fn daemon_reconnects_clears_pending() {
        let s = init();
        s.enter_other_host(Input::Mac);
        s.mark_dead();
        s.reconnect_count.store(5, Ordering::SeqCst);
        s.mark_healthy();
        assert!(*s.healthy.lock().unwrap());
        assert_eq!(*s.pending.lock().unwrap(), None);
        assert_eq!(s.reconnect_count.load(Ordering::SeqCst), 0);
    }

    // --- TvRemoteOverride: clears pending (C4 fix) ---
    #[test]
    fn remote_override_clears_pending() {
        let s = init();
        s.enter_other_host(Input::Mac);
        s.remote_override(TvMode::Multiview);
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Multiview);
        assert_eq!(*s.pending.lock().unwrap(), None);
    }

    // --- TvRemoteOverride: blocks Transitioning (C4 fix) ---
    #[test]
    fn remote_override_blocks_transitioning() {
        let s = init();
        s.remote_override(TvMode::Transitioning);
        assert_eq!(*s.tv_mode.lock().unwrap(), TvMode::Fullscreen);
    }

    // --- EnterMultiViewHost: guard conditions ---
    #[test]
    fn enter_multiview_host_guard() {
        let s = init();
        *s.pending.lock().unwrap() = None;
        assert!(!s.enter_multiview_host()); // not in multiview

        *s.tv_mode.lock().unwrap() = TvMode::Multiview;
        assert!(s.enter_multiview_host());

        *s.pending.lock().unwrap() = Some(Input::Mac);
        assert!(!s.enter_multiview_host()); // pending blocks
    }

    // --- Status snapshot ---
    #[test]
    fn status_returns_json() {
        let s = init();
        let status = s.status();
        assert_eq!(status["mode"], "fullscreen");
        assert_eq!(status["input"], "linux");
        assert_eq!(status["healthy"], true);
        assert!(status["pending_switch"].is_null());
    }
}
