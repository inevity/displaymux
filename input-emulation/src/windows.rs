use super::error::{EmulationError, WindowsEmulationCreationError};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent,
    LAN_MOUSE_WINDOWS_EXTRA_INFO, PointerEvent, scancode,
};

use async_trait::async_trait;
use std::{
    io,
    ops::BitOrAssign,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{sync::mpsc, task::AbortHandle};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_0, KEYEVENTF_EXTENDEDKEY, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EDD_GET_DEVICE_INTERFACE_NAME, SetCursorPos, XBUTTON1, XBUTTON2,
};
use windows::core::PCWSTR;

use super::{Emulation, EmulationHandle};

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);

pub(crate) struct WindowsEmulation {
    display_selector: Option<String>,
    repeat_task: Option<AbortHandle>,
    error_tx: mpsc::Sender<EmulationError>,
    error_rx: mpsc::Receiver<EmulationError>,
}

impl WindowsEmulation {
    pub(crate) fn new(
        display_selector: Option<&str>,
    ) -> Result<Self, WindowsEmulationCreationError> {
        let (error_tx, error_rx) = mpsc::channel(1);
        Ok(Self {
            display_selector: display_selector.map(ToOwned::to_owned),
            repeat_task: None,
            error_tx,
            error_rx,
        })
    }
}

#[async_trait]
impl Emulation for WindowsEmulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    rel_mouse(dx as i32, dy as i32)?;
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => mouse_button(button, state)?,
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => scroll(axis, value as i32)?,
                PointerEvent::AxisDiscrete120 { axis, value } => scroll(axis, value)?,
            },
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    match state {
                        // pressed
                        0 => self.kill_repeat_task(),
                        1 => self.spawn_repeat_task(key),
                        _ => {}
                    }
                    key_event(key, state)?;
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {}

    fn center_pointer(&mut self, _handle: EmulationHandle) -> Result<(), EmulationError> {
        let rect = resolve_display_rect(self.display_selector.as_deref())?;
        let center = rect_center(rect);
        unsafe { SetCursorPos(center.x, center.y) }.map_err(windows_io_error)?;
        Ok(())
    }

    fn poll_error(&mut self, cx: &mut Context<'_>) -> Poll<EmulationError> {
        match self.error_rx.poll_recv(cx) {
            Poll::Ready(Some(error)) => Poll::Ready(error),
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }
}

fn resolve_display_rect(selector: Option<&str>) -> Result<RECT, io::Error> {
    let mut matching = Vec::new();
    unsafe {
        for index in 0.. {
            let mut adapter = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(None, index, &mut adapter, EDD_GET_DEVICE_INTERFACE_NAME)
                .as_bool()
            {
                break;
            }
            if !adapter
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                continue;
            }

            let mut identifiers = display_device_identifiers(&adapter);
            for monitor_index in 0.. {
                let mut monitor = DISPLAY_DEVICEW {
                    cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(
                    PCWSTR::from_raw(adapter.DeviceName.as_ptr()),
                    monitor_index,
                    &mut monitor,
                    EDD_GET_DEVICE_INTERFACE_NAME,
                )
                .as_bool()
                {
                    break;
                }
                identifiers.extend(display_device_identifiers(&monitor));
            }

            if selector.is_some_and(|selector| {
                identifiers
                    .iter()
                    .any(|identifier| display_identifier_matches(selector, identifier))
            }) || selector.is_none()
            {
                let mut mode = DEVMODEW {
                    dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                    ..Default::default()
                };
                if !EnumDisplaySettingsW(
                    PCWSTR::from_raw(adapter.DeviceName.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                )
                .as_bool()
                {
                    continue;
                }
                let position = mode.Anonymous1.Anonymous2.dmPosition;
                matching.push(RECT {
                    left: position.x,
                    top: position.y,
                    right: position.x + mode.dmPelsWidth as i32,
                    bottom: position.y + mode.dmPelsHeight as i32,
                });
            }
        }
    }

    match matching.as_slice() {
        [rect] => Ok(*rect),
        [] => Err(io::Error::other(match selector {
            Some(selector) => format!("configured Windows display {selector:?} is not active"),
            None => "no active Windows display is available".to_owned(),
        })),
        _ => Err(io::Error::other(match selector {
            Some(selector) => format!("configured Windows display {selector:?} is ambiguous"),
            None => {
                "multiple Windows displays are active; emulation_display is required".to_owned()
            }
        })),
    }
}

fn display_device_identifiers(device: &DISPLAY_DEVICEW) -> Vec<String> {
    [
        wide_string(&device.DeviceName),
        wide_string(&device.DeviceString),
        wide_string(&device.DeviceID),
        wide_string(&device.DeviceKey),
    ]
    .into_iter()
    .filter(|value| !value.is_empty())
    .collect()
}

fn display_identifier_matches(selector: &str, identifier: &str) -> bool {
    if let Some(hardware_id) = selector.strip_prefix("hardware:") {
        return identifier
            .split(['\\', '#'])
            .any(|component| component.eq_ignore_ascii_case(hardware_id));
    }
    identifier.eq_ignore_ascii_case(selector)
}

fn wide_string(value: &[u16]) -> String {
    let len = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..len])
}

fn windows_io_error(error: windows::core::Error) -> EmulationError {
    io::Error::other(error.to_string()).into()
}

fn rect_center(rect: RECT) -> POINT {
    POINT {
        x: rect.left + (rect.right - rect.left) / 2,
        y: rect.top + (rect.bottom - rect.top) / 2,
    }
}

impl WindowsEmulation {
    fn spawn_repeat_task(&mut self, key: u32) {
        // there can only be one repeating key and it's
        // always the last to be pressed
        self.kill_repeat_task();
        let error_tx = self.error_tx.clone();
        let repeat_task = tokio::task::spawn_local(async move {
            tokio::time::sleep(DEFAULT_REPEAT_DELAY).await;
            loop {
                if let Err(error) = key_event(key, 1) {
                    let _ = error_tx.send(error).await;
                    break;
                }
                tokio::time::sleep(DEFAULT_REPEAT_INTERVAL).await;
            }
        });
        self.repeat_task = Some(repeat_task.abort_handle());
    }
    fn kill_repeat_task(&mut self) {
        if let Some(task) = self.repeat_task.take() {
            task.abort();
        }
    }
}

fn send_input(input: INPUT) -> Result<(), EmulationError> {
    let submitted = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if submitted == 1 {
        Ok(())
    } else {
        Err(EmulationError::WindowsSendInput(io::Error::last_os_error()))
    }
}

fn send_mouse_input(mi: MOUSEINPUT) -> Result<(), EmulationError> {
    send_input(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    })
}

fn send_keyboard_input(ki: KEYBDINPUT) -> Result<(), EmulationError> {
    send_input(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    })
}

fn rel_mouse(dx: i32, dy: i32) -> Result<(), EmulationError> {
    let mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: LAN_MOUSE_WINDOWS_EXTRA_INFO,
    };
    send_mouse_input(mi)
}

fn mouse_button(button: u32, state: u32) -> Result<(), EmulationError> {
    let dw_flags = match state {
        0 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTUP,
            BTN_RIGHT => MOUSEEVENTF_RIGHTUP,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEUP,
            BTN_BACK => MOUSEEVENTF_XUP,
            BTN_FORWARD => MOUSEEVENTF_XUP,
            _ => return Ok(()),
        },
        1 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTDOWN,
            BTN_RIGHT => MOUSEEVENTF_RIGHTDOWN,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEDOWN,
            BTN_BACK => MOUSEEVENTF_XDOWN,
            BTN_FORWARD => MOUSEEVENTF_XDOWN,
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    let mouse_data = match button {
        BTN_BACK => XBUTTON1 as u32,
        BTN_FORWARD => XBUTTON2 as u32,
        _ => 0,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0, // no movement
        mouseData: mouse_data,
        dwFlags: dw_flags,
        time: 0,
        dwExtraInfo: LAN_MOUSE_WINDOWS_EXTRA_INFO,
    };
    send_mouse_input(mi)
}

fn scroll(axis: u8, value: i32) -> Result<(), EmulationError> {
    let event_type = match axis {
        0 => MOUSEEVENTF_WHEEL,
        1 => MOUSEEVENTF_HWHEEL,
        _ => return Ok(()),
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: -value as u32,
        dwFlags: event_type,
        time: 0,
        dwExtraInfo: LAN_MOUSE_WINDOWS_EXTRA_INFO,
    };
    send_mouse_input(mi)
}

fn key_event(key: u32, state: u8) -> Result<(), EmulationError> {
    let scancode = match linux_keycode_to_windows_scancode(key) {
        Some(code) => code,
        None => return Ok(()),
    };
    let extended = scancode > 0xff;
    let scancode = scancode & 0xff;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
    }
    if state == 0 {
        flags.bitor_assign(KEYEVENTF_KEYUP);
    }
    let ki = KEYBDINPUT {
        wVk: Default::default(),
        wScan: scancode,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_keyboard_input(ki)
}

fn linux_keycode_to_windows_scancode(linux_keycode: u32) -> Option<u16> {
    let linux_scancode = match scancode::Linux::try_from(linux_keycode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("unknown keycode: {linux_keycode}");
            return None;
        }
    };
    log::trace!("linux code: {linux_scancode:?}");
    let windows_scancode = match scancode::Windows::try_from(linux_scancode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("failed to translate linux code into windows scancode: {linux_scancode:?}");
            return None;
        }
    };
    log::trace!("windows code: {windows_scancode:?}");
    Some(windows_scancode as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_center_handles_negative_desktop_coordinates() {
        let center = rect_center(RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        });

        assert_eq!(center, POINT { x: -960, y: 540 });
    }

    #[test]
    fn hardware_selector_matches_one_pnp_component() {
        assert!(display_identifier_matches(
            "hardware:GSM82CD",
            r"DISPLAY\GSM82CD\5&39B6B5E7&1&UID257"
        ));
        assert!(!display_identifier_matches(
            "hardware:GSM82CD",
            r"DISPLAY\CMN1404\5&39B6B5E7&1&UID256"
        ));
    }
}
