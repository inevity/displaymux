use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::{PCWSTR, w};

use tokio::sync::oneshot;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME, GetMessageW,
    HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT,
    PostThreadMessageW, RegisterClassW, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WINDOW_STYLE, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN,
    WM_XBUTTONUP, WNDCLASSW, WNDPROC,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent,
    LAN_MOUSE_WINDOWS_EXTRA_INFO, PointerEvent,
    scancode::{self, Linux},
};

use super::{CaptureEvent, Position, display_util};
use crate::event_queue::{EventQueue, PushOutcome};

const EDGE_REARM_DISTANCE: i32 = 3;

pub(crate) struct EventThread {
    event_queue: Arc<EventQueue>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    release_waiters: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
    resume_requests: Arc<Mutex<Vec<(Position, oneshot::Sender<bool>)>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
}

impl EventThread {
    pub(crate) fn new(event_queue: Arc<EventQueue>) -> Self {
        let request_buffer = Default::default();
        let release_waiters = Default::default();
        let resume_requests = Default::default();
        let (thread, thread_id) = start(
            event_queue.clone(),
            Arc::clone(&request_buffer),
            Arc::clone(&release_waiters),
            Arc::clone(&resume_requests),
        );
        Self {
            event_queue,
            request_buffer,
            release_waiters,
            resume_requests,
            thread: Some(thread),
            thread_id,
        }
    }

    pub(crate) async fn release_capture(&self) {
        let (completion_tx, completion_rx) = oneshot::channel();
        self.release_waiters.lock().unwrap().push(completion_tx);
        self.signal(RequestType::Release);
        let _ = completion_rx.await;
    }

    pub(crate) async fn resume_if_focused(&self, pos: Position) -> bool {
        let (completion_tx, completion_rx) = oneshot::channel();
        self.resume_requests
            .lock()
            .unwrap()
            .push((pos, completion_tx));
        self.signal(RequestType::ResumeIfFocused);
        completion_rx.await.unwrap_or(false)
    }

    pub(crate) fn create(&self, pos: Position) {
        self.client_update(ClientUpdate::Create(pos));
    }

    pub(crate) fn destroy(&self, pos: Position) {
        self.client_update(ClientUpdate::Destroy(pos));
    }

    fn exit(&self) {
        self.signal(RequestType::Exit);
    }

    fn client_update(&self, request: ClientUpdate) {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        self.signal(RequestType::ClientUpdate);
    }

    fn signal(&self, event_type: RequestType) {
        let id = self.thread_id;
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)).unwrap() };
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.exit();
        let _ = self.thread.take().expect("thread").join();
        self.event_queue.close();
    }
}

enum RequestType {
    ClientUpdate = 0,
    Release = 1,
    ResumeIfFocused = 2,
    Exit = 3,
}

enum ClientUpdate {
    Create(Position),
    Destroy(Position),
}

fn send_event(pos: Position, event: CaptureEvent) -> PushOutcome {
    EVENT_QUEUE.with_borrow(|queue| queue.as_ref().unwrap().push(pos, event))
}

thread_local! {
    /// all configured clients
    static CLIENTS: RefCell<HashSet<Position>> = RefCell::new(HashSet::new());
    /// currently active client
    static ACTIVE_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// released edge blocked until the pointer moves inward again
    static REARM_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// input event queue
    static EVENT_QUEUE: RefCell<Option<Arc<EventQueue>>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// previous mouse position
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<RECT>, i32)> = const { RefCell::new((Vec::new(), 0)) };
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

fn start(
    event_queue: Arc<EventQueue>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    release_waiters: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
    resume_requests: Arc<Mutex<Vec<(Position, oneshot::Sender<bool>)>>>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thead id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread = thread::spawn(|| {
        start_routine(
            thread_id_,
            event_queue,
            request_buffer,
            release_waiters,
            resume_requests,
        )
    });

    /* wait for thread to set its id */
    let (cond, thread_id) = &*thread_id;
    let mut thread_id = thread_id.lock().unwrap();
    while (*thread_id).is_none() {
        thread_id = cond.wait(thread_id).expect("channel closed");
    }
    (msg_thread, thread_id.expect("thread id"))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_queue: Arc<EventQueue>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    release_waiters: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
    resume_requests: Arc<Mutex<Vec<(Position, oneshot::Sender<bool>)>>>,
) {
    EVENT_QUEUE.replace(Some(event_queue));
    /* communicate thread id */
    {
        let (cnd, mtx) = &*ready;
        let mut ready = mtx.lock().unwrap();
        *ready = Some(unsafe { GetCurrentThreadId() });
        cnd.notify_one();
    }

    let mouse_proc: HOOKPROC = Some(mouse_proc);
    let kybrd_proc: HOOKPROC = Some(kybrd_proc);
    let window_proc: WNDPROC = Some(window_proc);

    /* register hooks */
    unsafe {
        let _ = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0).unwrap();
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0).unwrap();
    }

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("lan-mouse-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* window is used ro receive WM_DISPLAYCHANGE messages */
    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("CreateWindowExW");
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Release as usize => {
                    if let Some(pos) = ACTIVE_CLIENT.take() {
                        REARM_CLIENT.replace(Some(pos));
                    }
                    let waiters = release_waiters
                        .lock()
                        .unwrap()
                        .drain(..)
                        .collect::<Vec<_>>();
                    for waiter in waiters {
                        let _ = waiter.send(());
                    }
                }
                x if x == RequestType::ResumeIfFocused as usize => {
                    let requests = resume_requests
                        .lock()
                        .unwrap()
                        .drain(..)
                        .collect::<Vec<_>>();
                    for (pos, completion) in requests {
                        let resumed = ACTIVE_CLIENT.get().is_none()
                            && REARM_CLIENT.get() == Some(pos)
                            && send_event(pos, CaptureEvent::Begin) == PushOutcome::Queued;
                        if resumed {
                            ACTIVE_CLIENT.replace(Some(pos));
                            REARM_CLIENT.take();
                        }
                        let _ = completion.send(resumed);
                    }
                }
                x if x == RequestType::ClientUpdate as usize => {
                    let requests = {
                        let mut res = vec![];
                        let mut requests = request_buffer.lock().unwrap();
                        for request in requests.drain(..) {
                            res.push(request);
                        }
                        res
                    };

                    for request in requests {
                        update_clients(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn check_client_activation(wparam: WPARAM, lparam: LPARAM) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return ACTIVE_CLIENT.get().is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* next event is the first actual event */
    let ret = ACTIVE_CLIENT.get().is_some();

    /* client already active, no need to check */
    if ACTIVE_CLIENT.get().is_some() {
        return ret;
    }

    if let Some(pos) = REARM_CLIENT.get() {
        if edge_retreat_observed(pos, ENTRY_POINT.get(), prev_pos, curr_pos)
            && send_event(pos, CaptureEvent::EdgeRetreated) == PushOutcome::Queued
        {
            REARM_CLIENT.take();
        }
        return false;
    }

    /* check if a client was activated */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(pos) = entered else {
        return ret;
    };

    /* check if a client is registered for the barrier */
    if !CLIENTS.with_borrow(|clients| clients.contains(&pos)) {
        return ret;
    }

    /* update active client and entry point */
    ACTIVE_CLIENT.replace(Some(pos));
    let entry_point = DISPLAYS.with_borrow(|(displays, _)| {
        display_util::clamp_to_display_bounds(displays, prev_pos, curr_pos)
    });
    ENTRY_POINT.replace(entry_point);

    /* notify main thread */
    log::debug!("ENTERED @ {prev_pos:?} -> {curr_pos:?}");
    let active = ACTIVE_CLIENT.get().expect("active client");
    if send_event(active, CaptureEvent::Begin) == PushOutcome::Overflow {
        log::error!("critical input queue overflowed while entering capture; staying local");
        ACTIVE_CLIENT.take();
    }

    ret
}

fn edge_retreat_observed(
    position: Position,
    entry: (i32, i32),
    previous: (i32, i32),
    current: (i32, i32),
) -> bool {
    match position {
        Position::Left => current.0 >= entry.0 + EDGE_REARM_DISTANCE && current.0 > previous.0,
        Position::Right => current.0 <= entry.0 - EDGE_REARM_DISTANCE && current.0 < previous.0,
        Position::Top => current.1 >= entry.1 + EDGE_REARM_DISTANCE && current.1 > previous.1,
        Position::Bottom => current.1 <= entry.1 - EDGE_REARM_DISTANCE && current.1 < previous.1,
    }
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let mouse = *(lparam.0 as *const MSLLHOOKSTRUCT);
    if mouse.flags & LLMHF_INJECTED != 0 {
        if is_lan_mouse_injected_motion(wparam, &mouse) {
            // Input received from the current remote owner must remain visible
            // to Windows, but its pointer motion also drives the local return
            // barrier. Other injected input remains outside the capture path.
            check_client_activation(wparam, lparam);
        }
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(pos) = ACTIVE_CLIENT.get() else {
        return LRESULT(1);
    };

    /* convert to lan-mouse event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if send_event(pos, CaptureEvent::Input(Event::Pointer(pointer_event))) == PushOutcome::Overflow
    {
        log::error!("critical input queue overflowed; releasing pointer capture");
        ACTIVE_CLIENT.take();
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* don't pass event to applications */
    LRESULT(1)
}

fn is_lan_mouse_injected_motion(wparam: WPARAM, mouse: &MSLLHOOKSTRUCT) -> bool {
    wparam.0 == WM_MOUSEMOVE as usize
        && mouse.flags & LLMHF_INJECTED != 0
        && mouse.dwExtraInfo == LAN_MOUSE_WINDOWS_EXTRA_INFO
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let keyboard = *(lparam.0 as *const KBDLLHOOKSTRUCT);
    if keyboard.flags.contains(LLKHF_INJECTED) {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT.get() else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))) == PushOutcome::Overflow
    {
        log::error!("critical input queue overflowed; releasing keyboard capture");
        ACTIVE_CLIENT.take();
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    }
    LRESULT(1)
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<RECT>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<RECT>) {
    display_rects.clear();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
    }
}

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::Create(pos) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(pos));
        }
        ClientUpdate::Destroy(pos) => {
            if let Some(active_pos) = ACTIVE_CLIENT.get() {
                if pos == active_pos {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            if REARM_CLIENT.get() == Some(pos) {
                REARM_CLIENT.take();
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&pos));
        }
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => {
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            let (ex, ey) = ENTRY_POINT.get();
            let (dx, dy) = (x - ex, y - ey);
            let (dx, dy) = (dx as f64, dy as f64);
            Some(PointerEvent::Motion { time: 0, dx, dy })
        }
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_rearm_requires_inward_motion_beyond_the_rearm_zone() {
        let entry = (99, 50);

        assert!(!edge_retreat_observed(
            Position::Right,
            entry,
            (99, 50),
            (98, 50),
        ));
        assert!(edge_retreat_observed(
            Position::Right,
            entry,
            (98, 50),
            (96, 50),
        ));
        assert!(!edge_retreat_observed(
            Position::Right,
            entry,
            (95, 50),
            (96, 50),
        ));
    }

    #[test]
    fn only_lan_mouse_injected_motion_drives_return_barrier() {
        let own_motion = MSLLHOOKSTRUCT {
            flags: LLMHF_INJECTED,
            dwExtraInfo: LAN_MOUSE_WINDOWS_EXTRA_INFO,
            ..Default::default()
        };
        assert!(is_lan_mouse_injected_motion(
            WPARAM(WM_MOUSEMOVE as usize),
            &own_motion,
        ));

        let unrelated_motion = MSLLHOOKSTRUCT {
            dwExtraInfo: 0,
            ..own_motion
        };
        assert!(!is_lan_mouse_injected_motion(
            WPARAM(WM_MOUSEMOVE as usize),
            &unrelated_motion,
        ));
        assert!(!is_lan_mouse_injected_motion(
            WPARAM(WM_LBUTTONDOWN as usize),
            &own_motion,
        ));
    }
}
