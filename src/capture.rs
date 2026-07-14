use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::LanMouseConnection;

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    gate: Rc<RefCell<CaptureGate>>,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// a client was entered
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// An unarmed outgoing edge was reached and its backend capture was released.
    CaptureCandidate(CaptureHandle),
    /// A matching one-shot permit reached the edge and needs a current service decision.
    CommitRequested {
        handle: CaptureHandle,
        lease_epoch: u64,
        peer_session_epoch: u64,
        decision: oneshot::Sender<bool>,
    },
    /// Capture was released after the service denied or dropped commit authorization.
    CommitDeniedReleased {
        handle: CaptureHandle,
        lease_epoch: u64,
    },
    /// An active outgoing capture was released.
    ClientReleased {
        handle: CaptureHandle,
        reason: CaptureReleaseReason,
    },
    /// A peer readiness/session update was received on the outgoing connection.
    PeerReadiness(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureReleaseReason {
    PeerLeft,
    PeerReadinessLost,
    PeerReleaseRequested,
    ServiceRequested,
    ReleaseBind,
    TransportFailed,
}

impl CaptureReleaseReason {
    pub(crate) const fn failure_reason(self) -> Option<&'static str> {
        match self {
            Self::PeerReadinessLost => Some("peer_readiness_lost_during_capture"),
            Self::TransportFailed => Some("peer_transport_failed_during_capture"),
            Self::PeerLeft
            | Self::PeerReleaseRequested
            | Self::ServiceRequested
            | Self::ReleaseBind => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release(Option<oneshot::Sender<bool>>),
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// Resume a released edge when the backend still has authoritative focus.
    ResumeIfFocused(CaptureHandle),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: LanMouseConnection,
        release_bind: Vec<scancode::Linux>,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let gate = Rc::new(RefCell::new(CaptureGate::default()));
        let capture_task = CaptureTask {
            active_peer_session_epoch: None,
            active_client: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            gate: gate.clone(),
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            state: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            gate,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self) {
        self.request_release(None);
    }

    pub(crate) fn release_with_completion(&self, completion: oneshot::Sender<bool>) {
        self.request_release(Some(completion));
    }

    fn request_release(&self, completion: Option<oneshot::Sender<bool>>) {
        self.gate.borrow_mut().clear();
        self.request_tx
            .send(CaptureRequest::Release(completion))
            .expect("channel closed");
    }

    pub(crate) fn arm(
        &self,
        handle: CaptureHandle,
        lease_epoch: u64,
        peer_session_epoch: u64,
        valid_for: Duration,
    ) {
        let armed = self
            .gate
            .borrow_mut()
            .arm(handle, lease_epoch, peer_session_epoch, valid_for);
        if armed {
            self.request_tx
                .send(CaptureRequest::ResumeIfFocused(handle))
                .expect("channel closed");
        }
    }

    pub(crate) fn disarm(&self, lease_epoch: u64) {
        self.gate.borrow_mut().disarm(lease_epoch);
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

struct CaptureTask {
    active_peer_session_epoch: Option<u64>,
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: LanMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    gate: Rc<RefCell<CaptureGate>>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
        self.gate.borrow_mut().remove(handle);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release(completion) => {
                            self.gate.borrow_mut().clear();
                            if let Some(completion) = completion {
                                let _ = completion.send(true);
                            }
                        }
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                        CaptureRequest::ResumeIfFocused(_) => {}
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, _type) in captures {
            tokio::select! {
                r = capture.create(handle, pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        loop {
            tokio::select! {
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(_) => {
                            log::info!("client {handle} acknowledged the connection!");
                            self.state = State::Sending;
                        }
                        // client disconnected
                        ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture, CaptureReleaseReason::PeerLeft)
                                .await?;
                        },
                        ProtoEvent::Readiness {
                            keyboard_ready,
                            pointer_ready,
                            session_epoch,
                        } => {
                            if self.active_client == Some(handle)
                                && (!keyboard_ready
                                    || !pointer_ready
                                    || self.active_peer_session_epoch != Some(session_epoch))
                            {
                                log::warn!("releasing capture: peer input readiness changed");
                                self.release_capture(
                                    capture,
                                    CaptureReleaseReason::PeerReadinessLost,
                                )
                                .await?;
                            }
                            self.event_tx
                                .send(ICaptureEvent::PeerReadiness(handle))
                                .expect("channel closed");
                        }
                        ProtoEvent::Hello { .. } => {
                            self.event_tx
                                .send(ICaptureEvent::PeerReadiness(handle))
                                .expect("channel closed");
                        }
                        ProtoEvent::ReleaseRequest { release_epoch } => {
                            log::info!(
                                "releasing capture for peer request epoch {release_epoch}"
                            );
                            self.release_capture(
                                capture,
                                CaptureReleaseReason::PeerReleaseRequested,
                            )
                            .await?;
                            if let Err(error) = self
                                .conn
                                .send(ProtoEvent::ReleaseAck { release_epoch }, handle)
                                .await
                            {
                                log::warn!(
                                    "failed to acknowledge release epoch {release_epoch}: {error}"
                                );
                            }
                        }
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release(completion) => {
                        self.gate.borrow_mut().clear();
                        let result = self
                            .release_capture(capture, CaptureReleaseReason::ServiceRequested)
                            .await;
                        if let Some(completion) = completion {
                            let _ = completion.send(result.is_ok());
                        }
                        result?;
                    }
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        capture.create(h, p).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    CaptureRequest::ResumeIfFocused(handle) => {
                        if capture.resume_if_focused(handle)? {
                            log::info!("resuming still-focused edge for client {handle}");
                        }
                    }
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self
                .release_capture(capture, CaptureReleaseReason::ReleaseBind)
                .await;
        }

        if event == CaptureEvent::Begin {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if self.get_type(handle) == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // An outgoing capture may only become active by consuming a current,
        // one-shot permit. The first crossing is released before the service
        // may start any controller work.
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
            let permit = self.gate.borrow_mut().consume(handle);
            let Some(permit) = permit else {
                release_then_notify(capture.release(), || {
                    self.event_tx
                        .send(ICaptureEvent::CaptureCandidate(handle))
                        .expect("channel closed");
                })
                .await?;
                return Ok(());
            };

            let authorized = request_commit_authorization(&self.event_tx, handle, permit).await;
            if !authorized {
                release_then_notify(capture.release(), || {
                    self.event_tx
                        .send(ICaptureEvent::CommitDeniedReleased {
                            handle,
                            lease_epoch: permit.lease_epoch,
                        })
                        .expect("channel closed");
                })
                .await?;
                return Ok(());
            }

            self.state = State::WaitingForAck;
            self.active_client.replace(handle);
            self.active_peer_session_epoch = Some(permit.peer_session_epoch);
        }

        if Some(handle) != self.active_client {
            capture.release().await?;
            return Ok(());
        }

        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());

        let event = match event {
            CaptureEvent::Begin => ProtoEvent::Enter(opposite_pos),
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => ProtoEvent::Enter(opposite_pos),
                State::Sending => ProtoEvent::Input(e),
            },
        };

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            self.release_capture(capture, CaptureReleaseReason::TransportFailed)
                .await?;
        }
        Ok(())
    }

    async fn release_capture(
        &mut self,
        capture: &mut InputCapture,
        reason: CaptureReleaseReason,
    ) -> Result<(), CaptureError> {
        let released_handle = self.active_client.take();
        let pressed_keys = if released_handle.is_some() {
            capture.take_pressed_keys()
        } else {
            Default::default()
        };

        // Local keyboard and pointer ownership must be restored before any
        // network cleanup can block or fail.
        capture.release().await?;

        // If we had an active client, notify it after local release.
        if let Some(handle) = released_handle {
            self.active_peer_session_epoch = None;
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            for key in pressed_keys {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
        if let Some(handle) = released_handle {
            self.event_tx
                .send(ICaptureEvent::ClientReleased { handle, reason })
                .expect("channel closed");
        }
        Ok(())
    }
}

async fn request_commit_authorization(
    event_tx: &Sender<ICaptureEvent>,
    handle: CaptureHandle,
    permit: CapturePermit,
) -> bool {
    let (decision_tx, decision_rx) = oneshot::channel();
    event_tx
        .send(ICaptureEvent::CommitRequested {
            handle,
            lease_epoch: permit.lease_epoch,
            peer_session_epoch: permit.peer_session_epoch,
            decision: decision_tx,
        })
        .expect("channel closed");
    decision_rx.await.unwrap_or(false)
}

async fn release_then_notify<F, N, E>(release: F, notify: N) -> Result<(), E>
where
    F: Future<Output = Result<(), E>>,
    N: FnOnce(),
{
    release.await?;
    notify();
    Ok(())
}

#[derive(Debug, Default)]
struct CaptureGate {
    armed: Option<CapturePermit>,
    last_epoch: u64,
}

#[derive(Clone, Copy, Debug)]
struct CapturePermit {
    handle: CaptureHandle,
    lease_epoch: u64,
    peer_session_epoch: u64,
    expires_at: Instant,
}

impl CaptureGate {
    fn arm(
        &mut self,
        handle: CaptureHandle,
        lease_epoch: u64,
        peer_session_epoch: u64,
        valid_for: Duration,
    ) -> bool {
        if lease_epoch > self.last_epoch {
            if let Some(expires_at) = Instant::now().checked_add(valid_for) {
                self.last_epoch = lease_epoch;
                self.armed = Some(CapturePermit {
                    handle,
                    lease_epoch,
                    peer_session_epoch,
                    expires_at,
                });
                return true;
            }
        }
        false
    }

    fn consume(&mut self, handle: CaptureHandle) -> Option<CapturePermit> {
        let permit = self.armed?;
        if permit.expires_at <= Instant::now() {
            self.armed = None;
            return None;
        }
        if permit.handle != handle {
            return None;
        }
        self.armed = None;
        Some(permit)
    }

    fn disarm(&mut self, lease_epoch: u64) {
        if self
            .armed
            .is_some_and(|permit| permit.lease_epoch == lease_epoch)
        {
            self.armed = None;
        }
    }

    fn remove(&mut self, handle: CaptureHandle) {
        if self.armed.is_some_and(|permit| permit.handle == handle) {
            self.armed = None;
        }
    }

    fn clear(&mut self) {
        self.armed = None;
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> lan_mouse_proto::Position {
    match pos {
        input_capture::Position::Left => lan_mouse_proto::Position::Left,
        input_capture::Position::Right => lan_mouse_proto::Position::Right,
        input_capture::Position::Top => lan_mouse_proto::Position::Top,
        input_capture::Position::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_failed_capture_releases_request_a_notification() {
        assert_eq!(CaptureReleaseReason::PeerLeft.failure_reason(), None);
        assert_eq!(CaptureReleaseReason::ReleaseBind.failure_reason(), None);
        assert_eq!(
            CaptureReleaseReason::PeerReadinessLost.failure_reason(),
            Some("peer_readiness_lost_during_capture")
        );
        assert_eq!(
            CaptureReleaseReason::TransportFailed.failure_reason(),
            Some("peer_transport_failed_during_capture")
        );
    }
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn first_crossing_has_no_capture_permit() {
        let mut gate = CaptureGate::default();

        assert!(gate.consume(4).is_none());
    }

    #[test]
    fn permit_is_targeted_and_consumed_once() {
        let mut gate = CaptureGate::default();
        gate.arm(4, 7, 22, Duration::from_secs(1));

        assert!(gate.consume(5).is_none());
        let permit = gate.consume(4).unwrap();
        assert_eq!(permit.lease_epoch, 7);
        assert_eq!(permit.peer_session_epoch, 22);
        assert!(gate.consume(4).is_none());
    }

    #[test]
    fn delayed_arm_and_disarm_cannot_change_newer_permit() {
        let mut gate = CaptureGate::default();
        assert!(gate.arm(4, 8, 22, Duration::from_secs(1)));
        assert!(!gate.arm(5, 7, 31, Duration::from_secs(1)));
        gate.disarm(7);

        assert_eq!(gate.consume(4).unwrap().lease_epoch, 8);
    }

    #[test]
    fn expired_permit_cannot_enable_capture() {
        let mut gate = CaptureGate::default();
        gate.arm(4, 8, 22, Duration::ZERO);

        assert!(gate.consume(4).is_none());
    }

    #[tokio::test]
    async fn release_completes_before_candidate_notification() {
        let order = Rc::new(RefCell::new(Vec::new()));
        let release_order = order.clone();
        let notify_order = order.clone();

        release_then_notify(
            async move {
                release_order.borrow_mut().push("released");
                Ok::<_, ()>(())
            },
            move || notify_order.borrow_mut().push("notified"),
        )
        .await
        .unwrap();

        assert_eq!(&*order.borrow(), &["released", "notified"]);
    }

    #[tokio::test]
    async fn enter_authorization_waits_for_service_commit_decision() {
        let (event_tx, mut event_rx) = channel();
        let permit = CapturePermit {
            handle: 4,
            lease_epoch: 7,
            peer_session_epoch: 22,
            expires_at: Instant::now() + Duration::from_secs(1),
        };

        let request = request_commit_authorization(&event_tx, 4, permit);
        let decision = async {
            let ICaptureEvent::CommitRequested {
                handle,
                lease_epoch,
                peer_session_epoch,
                decision,
            } = event_rx.recv().await.unwrap()
            else {
                panic!("expected commit request");
            };
            assert_eq!(handle, 4);
            assert_eq!(lease_epoch, 7);
            assert_eq!(peer_session_epoch, 22);
            decision.send(true).unwrap();
        };

        let (authorized, ()) = futures::join!(request, decision);
        assert!(authorized);
    }

    #[tokio::test]
    async fn dropped_commit_decision_fails_closed() {
        let (event_tx, mut event_rx) = channel();
        let permit = CapturePermit {
            handle: 4,
            lease_epoch: 7,
            peer_session_epoch: 22,
            expires_at: Instant::now() + Duration::from_secs(1),
        };

        let request = request_commit_authorization(&event_tx, 4, permit);
        let decision = async {
            let ICaptureEvent::CommitRequested { decision, .. } = event_rx.recv().await.unwrap()
            else {
                panic!("expected commit request");
            };
            drop(decision);
        };

        let (authorized, ()) = futures::join!(request, decision);
        assert!(!authorized);
    }
}
