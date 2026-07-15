use super::ClipboardBackend;
use crate::{ClipboardData, ClipboardReason, NativeGeneration};
use ::windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            DataExchange::{
                AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
                GetClipboardSequenceNumber, GetOpenClipboardWindow, IsClipboardFormatAvailable,
                OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener,
                SetClipboardData,
            },
            LibraryLoader::GetModuleHandleW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
            GetWindowThreadProcessId, HWND_MESSAGE, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
            TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE, WNDCLASSW,
        },
    },
    core::{Error as WindowsError, w},
};
use std::{
    ffi::c_void,
    ptr, slice,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const CF_UNICODETEXT_FORMAT: u32 = 13;
// Clipboard locks are normally held for one short message dispatch. This fixed actor-local budget
// tolerates brief contention without letting a foreign process hold the clipboard actor forever.
const OPEN_DEADLINE: Duration = Duration::from_millis(200);
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(5);

static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

fn backend_operation_failed(
    operation: &'static str,
    reason: ClipboardReason,
    error: &WindowsError,
) -> ClipboardReason {
    tracing::warn!(
        event = "clipboard_backend_operation_failed",
        backend = "windows",
        operation,
        reason = reason.code(),
        error = %error,
        "Windows clipboard backend operation failed"
    );
    reason
}

#[derive(Default)]
struct GenerationTracker {
    last_raw: Option<u32>,
    wrap_base: u64,
    self_write_raw: Option<u32>,
    suppressed_self_notifications: u64,
}

impl GenerationTracker {
    fn observe(&mut self, raw: u32) -> Result<NativeGeneration, ClipboardReason> {
        if self.last_raw.is_some_and(|previous| raw < previous) {
            self.wrap_base = self
                .wrap_base
                .checked_add(1_u64 << 32)
                .ok_or(ClipboardReason::IdentityExhausted)?;
        }
        self.last_raw = Some(raw);
        self.wrap_base
            .checked_add(u64::from(raw))
            .map(NativeGeneration::new)
            .ok_or(ClipboardReason::IdentityExhausted)
    }

    fn mark_self_write(&mut self, raw: u32) {
        self.self_write_raw = Some(raw);
    }

    fn observe_notification(&mut self, raw: u32) -> Result<(), ClipboardReason> {
        self.observe(raw)?;
        if self.self_write_raw == Some(raw) {
            self.self_write_raw = None;
            self.suppressed_self_notifications = self
                .suppressed_self_notifications
                .checked_add(1)
                .ok_or(ClipboardReason::IdentityExhausted)?;
        }
        Ok(())
    }
}

pub struct WindowsClipboardBackend {
    // Store the thread-affine HWND as an integer so an uninitialized backend can be moved exactly
    // once into the actor thread. The handle is created, used, and destroyed only on that thread.
    window: Option<isize>,
    private_format: u32,
    self_write_format: u32,
    generation: GenerationTracker,
}

impl WindowsClipboardBackend {
    pub fn connect() -> Result<Self, ClipboardReason> {
        Ok(Self {
            window: None,
            private_format: 0,
            self_write_format: 0,
            generation: GenerationTracker::default(),
        })
    }

    fn hwnd(&self) -> Result<HWND, ClipboardReason> {
        self.window
            .map(|window| HWND(window as *mut c_void))
            .ok_or(ClipboardReason::BackendUnavailable)
    }

    fn ensure_window(&mut self) -> Result<HWND, ClipboardReason> {
        if let Ok(window) = self.hwnd() {
            return Ok(window);
        }
        let instance = unsafe { GetModuleHandleW(None) }.map_err(|error| {
            backend_operation_failed(
                "get_module_handle",
                ClipboardReason::BackendUnavailable,
                &error,
            )
        })?;
        if WINDOW_CLASS_REGISTERED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("lan-mouse-clipboard-window-v1"),
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0 {
                let error = WindowsError::from_win32();
                WINDOW_CLASS_REGISTERED.store(false, Ordering::SeqCst);
                return Err(backend_operation_failed(
                    "register_window_class",
                    ClipboardReason::BackendUnavailable,
                    &error,
                ));
            }
        }
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("lan-mouse-clipboard-window-v1"),
                w!("lan-mouse-clipboard"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            )
        }
        .map_err(|error| {
            backend_operation_failed(
                "create_message_window",
                ClipboardReason::BackendUnavailable,
                &error,
            )
        })?;
        unsafe { AddClipboardFormatListener(window) }.map_err(|error| {
            backend_operation_failed(
                "add_format_listener",
                ClipboardReason::PermissionDenied,
                &error,
            )
        })?;
        let private_format =
            unsafe { RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")) };
        let self_write_format =
            unsafe { RegisterClipboardFormatW(w!("LanMouseClipboardSelfWriteV1")) };
        if private_format == 0 || self_write_format == 0 {
            let error = WindowsError::from_win32();
            let _ = unsafe { RemoveClipboardFormatListener(window) };
            let _ = unsafe { DestroyWindow(window) };
            return Err(backend_operation_failed(
                "register_clipboard_formats",
                ClipboardReason::BackendUnavailable,
                &error,
            ));
        }
        self.window = Some(window.0 as isize);
        self.private_format = private_format;
        self.self_write_format = self_write_format;
        Ok(window)
    }

    fn pump_notifications(&mut self) -> Result<(), ClipboardReason> {
        let window = self.ensure_window()?;
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, Some(window), 0, 0, PM_REMOVE) }.as_bool() {
            if message.message == WM_CLIPBOARDUPDATE {
                let raw = unsafe { GetClipboardSequenceNumber() };
                self.generation.observe_notification(raw)?;
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(())
    }

    fn current_generation(&mut self) -> Result<NativeGeneration, ClipboardReason> {
        self.pump_notifications()?;
        self.generation
            .observe(unsafe { GetClipboardSequenceNumber() })
    }

    fn capture_text(&mut self, max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
        let window = self.ensure_window()?;
        let _clipboard = OpenClipboardGuard::open(window, "capture")?;
        if unsafe { IsClipboardFormatAvailable(self.private_format) }.is_ok() {
            return Err(ClipboardReason::PrivateContent);
        }
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT_FORMAT) }.is_err() {
            return Err(ClipboardReason::UnsupportedFormat);
        }
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT_FORMAT) }
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        let global = HGLOBAL(handle.0);
        let allocation_bytes = unsafe { GlobalSize(global) };
        if allocation_bytes < size_of::<u16>() || allocation_bytes % size_of::<u16>() != 0 {
            return Err(ClipboardReason::InvalidUtf8);
        }
        let allocation_units = allocation_bytes / size_of::<u16>();
        let inspect_units =
            allocation_units.min(max_bytes.checked_add(1).ok_or(ClipboardReason::Oversize)?);
        let locked = GlobalLockGuard::lock(global)?;
        let units = unsafe { slice::from_raw_parts(locked.pointer.cast::<u16>(), inspect_units) };
        let Some(nul) = units.iter().position(|unit| *unit == 0) else {
            return if inspect_units < allocation_units {
                Err(ClipboardReason::Oversize)
            } else {
                Err(ClipboardReason::InvalidUtf8)
            };
        };
        decode_utf16_text(&units[..nul], max_bytes)
    }

    fn apply_data(&mut self, data: &ClipboardData) -> Result<u32, ClipboardReason> {
        let text = match data {
            ClipboardData::Text(bytes) => {
                std::str::from_utf8(bytes).map_err(|_| ClipboardReason::InvalidUtf8)?
            }
            ClipboardData::Empty => "",
            ClipboardData::Unavailable(reason) => return Err(*reason),
        };
        let utf16 = encode_native_utf16(text);
        let mut text_memory = OwnedGlobal::from_u16(&utf16)?;
        let mut marker_memory = OwnedGlobal::from_bytes(&[1])?;
        let window = self.ensure_window()?;
        {
            let _clipboard = OpenClipboardGuard::open(window, "apply")?;
            unsafe { EmptyClipboard() }.map_err(|_| ClipboardReason::BackendUnavailable)?;
            unsafe {
                SetClipboardData(CF_UNICODETEXT_FORMAT, Some(HANDLE(text_memory.handle().0)))
            }
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
            text_memory.transfer_to_clipboard();
            match unsafe {
                SetClipboardData(
                    self.self_write_format,
                    Some(HANDLE(marker_memory.handle().0)),
                )
            } {
                Ok(_) => marker_memory.transfer_to_clipboard(),
                Err(error) => tracing::debug!(
                    event = "clipboard_self_marker_failed",
                    error = %error,
                    "clipboard text was applied but the private self-write marker was not"
                ),
            }
        }
        let raw = unsafe { GetClipboardSequenceNumber() };
        self.generation.mark_self_write(raw);
        Ok(raw)
    }
}

impl ClipboardBackend for WindowsClipboardBackend {
    fn initialize(&mut self) -> Result<(), ClipboardReason> {
        let window = self.ensure_window()?;
        drop(OpenClipboardGuard::open(window, "initialize")?);
        self.current_generation().map(|_| ())
    }

    fn generation(&mut self) -> Result<NativeGeneration, ClipboardReason> {
        self.current_generation()
    }

    fn capture(&mut self, max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
        self.capture_text(max_bytes)
    }

    fn apply(
        &mut self,
        expected_generation: NativeGeneration,
        data: &ClipboardData,
    ) -> Result<NativeGeneration, ClipboardReason> {
        if self.current_generation()? != expected_generation {
            return Err(ClipboardReason::DestinationChanged);
        }
        let raw = self.apply_data(data)?;
        self.generation.observe(raw)
    }

    fn shutdown(&mut self) {
        if let Some(window) = self.window.take() {
            let window = HWND(window as *mut c_void);
            let _ = unsafe { RemoveClipboardFormatListener(window) };
            let _ = unsafe { DestroyWindow(window) };
        }
    }
}

struct OpenClipboardGuard;

impl OpenClipboardGuard {
    fn open(window: HWND, operation: &'static str) -> Result<Self, ClipboardReason> {
        let deadline = Instant::now() + OPEN_DEADLINE;
        let mut attempts = 0_u32;
        loop {
            attempts += 1;
            let error = match unsafe { OpenClipboard(Some(window)) } {
                Ok(()) => return Ok(Self),
                Err(error) => error,
            };
            if Instant::now() >= deadline {
                let open_window = unsafe { GetOpenClipboardWindow() }.ok();
                let open_window_handle = open_window
                    .map(|window| window.0 as usize)
                    .unwrap_or_default();
                let mut open_window_pid = 0_u32;
                if let Some(open_window) = open_window {
                    unsafe {
                        GetWindowThreadProcessId(open_window, Some(&mut open_window_pid));
                    }
                }
                tracing::warn!(
                    event = "clipboard_open_failed",
                    operation,
                    attempts,
                    error = %error,
                    open_window = open_window_handle,
                    open_window_pid,
                    "Windows clipboard remained unavailable for the actor-local open budget"
                );
                return Err(ClipboardReason::BackendUnavailable);
            }
            thread::sleep(OPEN_RETRY_DELAY);
        }
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

struct GlobalLockGuard {
    handle: HGLOBAL,
    pointer: *mut c_void,
}

impl GlobalLockGuard {
    fn lock(handle: HGLOBAL) -> Result<Self, ClipboardReason> {
        let pointer = unsafe { GlobalLock(handle) };
        if pointer.is_null() {
            return Err(ClipboardReason::BackendUnavailable);
        }
        Ok(Self { handle, pointer })
    }
}

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        let _ = unsafe { GlobalUnlock(self.handle) };
    }
}

struct OwnedGlobal(Option<HGLOBAL>);

impl OwnedGlobal {
    fn from_u16(units: &[u16]) -> Result<Self, ClipboardReason> {
        let bytes = units
            .len()
            .checked_mul(size_of::<u16>())
            .ok_or(ClipboardReason::Oversize)?;
        let memory = Self::allocate(bytes)?;
        {
            let locked = GlobalLockGuard::lock(memory.handle())?;
            unsafe {
                ptr::copy_nonoverlapping(units.as_ptr(), locked.pointer.cast::<u16>(), units.len());
            }
        }
        Ok(memory)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, ClipboardReason> {
        let memory = Self::allocate(bytes.len())?;
        {
            let locked = GlobalLockGuard::lock(memory.handle())?;
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), locked.pointer.cast::<u8>(), bytes.len());
            }
        }
        Ok(memory)
    }

    fn allocate(bytes: usize) -> Result<Self, ClipboardReason> {
        unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
            .map(|handle| Self(Some(handle)))
            .map_err(|_| ClipboardReason::BackendUnavailable)
    }

    fn handle(&self) -> HGLOBAL {
        self.0.expect("owned global memory is present")
    }

    fn transfer_to_clipboard(&mut self) {
        self.0 = None;
    }
}

impl Drop for OwnedGlobal {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = unsafe { GlobalFree(Some(handle)) };
        }
    }
}

fn decode_utf16_text(units: &[u16], max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
    let text = String::from_utf16(units).map_err(|_| ClipboardReason::InvalidUtf8)?;
    let normalized = normalize_line_endings(&text);
    if normalized.len() > max_bytes {
        return Err(ClipboardReason::Oversize);
    }
    if normalized.is_empty() {
        Ok(ClipboardData::Empty)
    } else {
        ClipboardData::text(normalized.into_bytes())
    }
}

fn encode_native_utf16(text: &str) -> Vec<u16> {
    let native = text.replace('\n', "\r\n");
    native.encode_utf16().chain([0]).collect()
}

fn normalize_line_endings(text: &str) -> String {
    if !text.as_bytes().contains(&b'\r') {
        return text.to_owned();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_round_trip_preserves_non_bmp_and_canonicalizes_lines() {
        let encoded = encode_native_utf16("first\nemoji 😀");
        assert_eq!(encoded.last(), Some(&0));
        assert_eq!(
            decode_utf16_text(&encoded[..encoded.len() - 1], 64).unwrap(),
            ClipboardData::text("first\nemoji 😀".as_bytes().to_vec()).unwrap()
        );
    }

    #[test]
    fn utf8_limit_is_checked_after_utf16_conversion() {
        let units = "é".encode_utf16().collect::<Vec<_>>();
        assert_eq!(decode_utf16_text(&units, 2).unwrap().len(), 2);
        assert_eq!(decode_utf16_text(&units, 1), Err(ClipboardReason::Oversize));
    }

    #[test]
    fn invalid_utf16_is_never_exported() {
        assert_eq!(
            decode_utf16_text(&[0xd800], 8),
            Err(ClipboardReason::InvalidUtf8)
        );
    }

    #[test]
    fn empty_utf16_is_explicit_empty() {
        assert_eq!(decode_utf16_text(&[], 8).unwrap(), ClipboardData::Empty);
    }

    #[test]
    fn self_notification_is_suppressed_once_without_hiding_generation() {
        let mut tracker = GenerationTracker::default();
        tracker.observe(9).unwrap();
        tracker.mark_self_write(10);
        tracker.observe_notification(10).unwrap();
        assert_eq!(tracker.suppressed_self_notifications, 1);
        assert!(tracker.self_write_raw.is_none());
        assert_eq!(tracker.observe(10).unwrap(), NativeGeneration::new(10));
    }

    #[test]
    fn zero_sequence_is_valid_initially_and_after_wrap() {
        let mut initial = GenerationTracker::default();
        assert_eq!(initial.observe(0).unwrap(), NativeGeneration::new(0));

        let mut wrapped = GenerationTracker::default();
        assert_eq!(
            wrapped.observe(u32::MAX).unwrap(),
            NativeGeneration::new(u64::from(u32::MAX))
        );
        assert_eq!(
            wrapped.observe(0).unwrap(),
            NativeGeneration::new(1_u64 << 32)
        );
    }

    #[test]
    #[ignore = "requires an interactive Windows clipboard station with the clipboard held unavailable"]
    fn live_busy_clipboard_prevents_backend_initialization() {
        let mut backend = WindowsClipboardBackend::connect().unwrap();
        let window = backend.ensure_window().unwrap();
        assert!(matches!(
            OpenClipboardGuard::open(window, "contention_test"),
            Err(ClipboardReason::BackendUnavailable)
        ));
        assert_eq!(
            backend.initialize(),
            Err(ClipboardReason::BackendUnavailable)
        );
        backend.shutdown();
    }

    #[test]
    #[ignore = "requires an interactive Windows clipboard station and mutates then restores it"]
    fn live_text_empty_and_destination_race_restore_original_clipboard() {
        let mut backend = WindowsClipboardBackend::connect().unwrap();
        backend.initialize().unwrap();
        let original = backend.capture(3 * 1024 * 1024).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let baseline = backend.generation().unwrap();
            backend
                .apply(
                    baseline,
                    &ClipboardData::text("lan-mouse 😀\nclipboard".as_bytes().to_vec()).unwrap(),
                )
                .unwrap();
            assert_eq!(
                backend.capture(1024).unwrap(),
                ClipboardData::text("lan-mouse 😀\nclipboard".as_bytes().to_vec()).unwrap()
            );
            let baseline = backend.generation().unwrap();
            backend.apply(baseline, &ClipboardData::Empty).unwrap();
            assert_eq!(backend.capture(1024).unwrap(), ClipboardData::Empty);

            let stale = backend.generation().unwrap();
            backend
                .apply_data(&ClipboardData::text(b"destination changed".to_vec()).unwrap())
                .unwrap();
            assert_eq!(
                backend.apply(stale, &ClipboardData::Empty),
                Err(ClipboardReason::DestinationChanged)
            );
        }));
        let restore_baseline = backend.generation().unwrap();
        backend.apply(restore_baseline, &original).unwrap();
        backend.shutdown();
        result.unwrap();
    }
}
