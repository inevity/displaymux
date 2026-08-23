use async_trait::async_trait;
use std::{io, ptr, slice};
use x11::{
    xlib::{self, XCloseDisplay},
    xrandr, xtest,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};

use crate::error::EmulationError;

use super::{Emulation, EmulationHandle, error::X11EmulationCreationError};

pub(crate) struct X11Emulation {
    display: *mut xlib::Display,
    display_selector: Option<String>,
}

unsafe impl Send for X11Emulation {}

impl X11Emulation {
    pub(crate) fn new(display_selector: Option<&str>) -> Result<Self, X11EmulationCreationError> {
        let display = unsafe {
            match xlib::XOpenDisplay(ptr::null()) {
                d if std::ptr::eq(d, ptr::null_mut::<xlib::Display>()) => {
                    Err(X11EmulationCreationError::OpenDisplay)
                }
                display => Ok(display),
            }
        }?;
        Ok(Self {
            display,
            display_selector: display_selector.map(ToOwned::to_owned),
        })
    }

    fn relative_motion(&self, dx: i32, dy: i32) {
        unsafe {
            xtest::XTestFakeRelativeMotionEvent(self.display, dx, dy, 0, 0);
        }
    }

    fn emulate_mouse_button(&self, button: u32, state: u32) {
        unsafe {
            let x11_button = match button {
                BTN_RIGHT => 3,
                BTN_MIDDLE => 2,
                BTN_BACK => 8,
                BTN_FORWARD => 9,
                BTN_LEFT => 1,
                _ => 1,
            };
            xtest::XTestFakeButtonEvent(self.display, x11_button, state as i32, 0);
        };
    }

    const SCROLL_UP: u32 = 4;
    const SCROLL_DOWN: u32 = 5;
    const SCROLL_LEFT: u32 = 6;
    const SCROLL_RIGHT: u32 = 7;

    fn emulate_scroll(&self, axis: u8, value: f64) {
        let direction = match axis {
            1 => {
                if value < 0.0 {
                    Self::SCROLL_LEFT
                } else {
                    Self::SCROLL_RIGHT
                }
            }
            _ => {
                if value < 0.0 {
                    Self::SCROLL_UP
                } else {
                    Self::SCROLL_DOWN
                }
            }
        };

        unsafe {
            xtest::XTestFakeButtonEvent(self.display, direction, 1, 0);
            xtest::XTestFakeButtonEvent(self.display, direction, 0, 0);
        }
    }

    #[allow(dead_code)]
    fn emulate_key(&self, key: u32, state: u8) {
        let key = key + 8; // xorg keycodes are shifted by 8
        unsafe {
            xtest::XTestFakeKeyEvent(self.display, key, state as i32, 0);
        }
    }
}

impl Drop for X11Emulation {
    fn drop(&mut self) {
        unsafe {
            XCloseDisplay(self.display);
        }
    }
}

#[async_trait]
impl Emulation for X11Emulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    self.relative_motion(dx as i32, dy as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    self.emulate_mouse_button(button, state);
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    self.emulate_scroll(axis, value);
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    self.emulate_scroll(axis, value as f64);
                }
            },
            Event::Keyboard(KeyboardEvent::Key {
                time: _,
                key,
                state,
            }) => {
                self.emulate_key(key, state);
            }
            _ => {}
        }
        unsafe {
            xlib::XFlush(self.display);
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn destroy(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn terminate(&mut self) {
        /* nothing to do */
    }

    fn center_pointer(&mut self, _handle: EmulationHandle) -> Result<(), EmulationError> {
        let rect = resolve_output_rect(self.display, self.display_selector.as_deref())?;
        let root = unsafe { xlib::XDefaultRootWindow(self.display) };
        let center_x = rect.x + rect.width / 2;
        let center_y = rect.y + rect.height / 2;
        unsafe {
            xlib::XWarpPointer(self.display, 0, root, 0, 0, 0, 0, center_x, center_y);
            xlib::XFlush(self.display);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutputRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn resolve_output_rect(
    display: *mut xlib::Display,
    selector: Option<&str>,
) -> Result<OutputRect, io::Error> {
    let root = unsafe { xlib::XDefaultRootWindow(display) };
    let resources = unsafe { xrandr::XRRGetScreenResourcesCurrent(display, root) };
    if resources.is_null() {
        return Err(io::Error::other("could not enumerate X11 outputs"));
    }

    let mut matching = Vec::new();
    unsafe {
        let outputs = slice::from_raw_parts((*resources).outputs, (*resources).noutput as usize);
        for output in outputs {
            let info = xrandr::XRRGetOutputInfo(display, resources, *output);
            if info.is_null() {
                continue;
            }
            let name_bytes =
                slice::from_raw_parts((*info).name.cast::<u8>(), (*info).nameLen as usize);
            let name = String::from_utf8_lossy(name_bytes);
            let selected = selector.is_none_or(|selector| name == selector);
            if selected
                && i32::from((*info).connection) == xrandr::RR_Connected
                && (*info).crtc != 0
            {
                let crtc = xrandr::XRRGetCrtcInfo(display, resources, (*info).crtc);
                if !crtc.is_null() {
                    matching.push(OutputRect {
                        x: (*crtc).x,
                        y: (*crtc).y,
                        width: (*crtc).width as i32,
                        height: (*crtc).height as i32,
                    });
                    xrandr::XRRFreeCrtcInfo(crtc);
                }
            }
            xrandr::XRRFreeOutputInfo(info);
        }
        xrandr::XRRFreeScreenResources(resources);
    }

    match matching.as_slice() {
        [rect] => Ok(*rect),
        [] => Err(io::Error::other(match selector {
            Some(selector) => format!("configured X11 output {selector:?} is not active"),
            None => "no active X11 output is available".to_owned(),
        })),
        _ => Err(io::Error::other(match selector {
            Some(selector) => format!("configured X11 output {selector:?} is ambiguous"),
            None => "multiple X11 outputs are active; emulation_display is required".to_owned(),
        })),
    }
}
