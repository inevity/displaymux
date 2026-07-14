use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    task::{Context, Poll},
};

use input_event::{Event, KeyboardEvent, PointerEvent};

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

#[cfg(wlroots)]
mod wlroots;

#[cfg(rdp)]
mod xdg_desktop_portal;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

/// fallback input emulation (logs events)
mod dummy;
mod error;

pub type EmulationHandle = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(wlroots)]
    Wlroots,
    #[cfg(libei)]
    Libei,
    #[cfg(rdp)]
    Xdp,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots)]
            Backend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei)]
            Backend::Libei => write!(f, "libei"),
            #[cfg(rdp)]
            Backend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "macos"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputEmulation {
    backend: Backend,
    emulation: Box<dyn Emulation>,
    handles: HashSet<EmulationHandle>,
    pressed_buttons: HashMap<EmulationHandle, HashSet<u32>>,
    pressed_keys: HashMap<EmulationHandle, HashSet<u32>>,
}

impl InputEmulation {
    async fn with_backend(
        backend: Backend,
        display_selector: Option<&str>,
    ) -> Result<InputEmulation, EmulationCreationError> {
        let emulation: Box<dyn Emulation> = match backend {
            #[cfg(wlroots)]
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new(display_selector)?),
            #[cfg(libei)]
            Backend::Libei => Box::new(libei::LibeiEmulation::new(display_selector).await?),
            #[cfg(x11)]
            Backend::X11 => Box::new(x11::X11Emulation::new(display_selector)?),
            #[cfg(rdp)]
            Backend::Xdp => {
                Box::new(xdg_desktop_portal::DesktopPortalEmulation::new(display_selector).await?)
            }
            #[cfg(windows)]
            Backend::Windows => Box::new(windows::WindowsEmulation::new(display_selector)?),
            #[cfg(target_os = "macos")]
            Backend::MacOs => Box::new(macos::MacOSEmulation::new(display_selector)?),
            Backend::Dummy => Box::new(dummy::DummyEmulation::new()),
        };
        Ok(Self {
            backend,
            emulation,
            handles: HashSet::new(),
            pressed_buttons: HashMap::new(),
            pressed_keys: HashMap::new(),
        })
    }

    pub async fn new(
        backend: Option<Backend>,
        display_selector: Option<String>,
    ) -> Result<InputEmulation, EmulationCreationError> {
        if let Some(backend) = backend {
            let b = Self::with_backend(backend, display_selector.as_deref()).await;
            if b.is_ok() {
                log::info!("using emulation backend: {backend}");
            }
            return b;
        }

        for backend in [
            #[cfg(wlroots)]
            Backend::Wlroots,
            #[cfg(libei)]
            Backend::Libei,
            #[cfg(rdp)]
            Backend::Xdp,
            #[cfg(x11)]
            Backend::X11,
            #[cfg(windows)]
            Backend::Windows,
            #[cfg(target_os = "macos")]
            Backend::MacOs,
            Backend::Dummy,
        ] {
            match Self::with_backend(backend, display_selector.as_deref()).await {
                Ok(b) => {
                    log::info!("using emulation backend: {backend}");
                    return Ok(b);
                }
                Err(e) if e.cancelled_by_user() => return Err(e),
                Err(e) => log::warn!("{e}"),
            }
        }

        Err(EmulationCreationError::NoAvailableBackend)
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                // prevent double pressed / released keys
                if self.update_pressed_keys(handle, key, state) {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                if self.update_pressed_button(handle, button, state) {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            _ => self.emulation.consume(event, handle).await,
        }
    }

    pub fn center_pointer(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        self.emulation.center_pointer(handle)
    }

    pub fn poll_error(&mut self, cx: &mut Context<'_>) -> Poll<EmulationError> {
        self.emulation.poll_error(cx)
    }

    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.handles.insert(handle) {
            self.pressed_buttons.insert(handle, HashSet::new());
            self.pressed_keys.insert(handle, HashSet::new());
            self.emulation.create(handle).await;
            true
        } else {
            false
        }
    }

    pub async fn destroy(&mut self, handle: EmulationHandle) {
        let _ = self.release_inputs(handle).await;
        if self.handles.remove(&handle) {
            self.pressed_buttons.remove(&handle);
            self.pressed_keys.remove(&handle);
            self.emulation.destroy(handle).await
        }
    }

    pub async fn terminate(&mut self) {
        for handle in self.handles.iter().cloned().collect::<Vec<_>>() {
            self.destroy(handle).await
        }
        self.emulation.terminate().await
    }

    pub async fn release_inputs(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        let mut first_error = None;
        if let Some(keys) = self.pressed_keys.get_mut(&handle) {
            let keys = keys.drain().collect::<Vec<_>>();
            for key in keys {
                let event = Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state: 0,
                });
                if let Err(error) = self.emulation.consume(event, handle).await {
                    first_error.get_or_insert(error);
                }
                if let Ok(key) = input_event::scancode::Linux::try_from(key) {
                    log::warn!("releasing stuck key: {key:?}");
                }
            }
        }

        if let Some(buttons) = self.pressed_buttons.get_mut(&handle) {
            let buttons = buttons.drain().collect::<Vec<_>>();
            for button in buttons {
                let event = Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button,
                    state: 0,
                });
                if let Err(error) = self.emulation.consume(event, handle).await {
                    first_error.get_or_insert(error);
                }
                log::warn!("releasing stuck pointer button: {button:#x}");
            }
        }

        let event = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        if let Err(error) = self.emulation.consume(event, handle).await {
            first_error.get_or_insert(error);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn update_pressed_button(&mut self, handle: EmulationHandle, button: u32, state: u32) -> bool {
        let Some(pressed_buttons) = self.pressed_buttons.get_mut(&handle) else {
            return false;
        };

        if state == 0 {
            pressed_buttons.remove(&button)
        } else {
            pressed_buttons.insert(button)
        }
    }

    /// update the pressed_keys for the given handle
    /// returns whether the event should be processed
    fn update_pressed_keys(&mut self, handle: EmulationHandle, key: u32, state: u8) -> bool {
        let Some(pressed_keys) = self.pressed_keys.get_mut(&handle) else {
            return false;
        };

        if state == 0 {
            // currently pressed => can release
            pressed_keys.remove(&key)
        } else {
            // currently not pressed => can press
            pressed_keys.insert(key)
        }
    }
}

#[async_trait]
trait Emulation: Send {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError>;
    async fn create(&mut self, handle: EmulationHandle);
    async fn destroy(&mut self, handle: EmulationHandle);
    async fn terminate(&mut self);

    fn center_pointer(&mut self, _handle: EmulationHandle) -> Result<(), EmulationError> {
        Ok(())
    }

    fn poll_error(&mut self, _cx: &mut Context<'_>) -> Poll<EmulationError> {
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::BTN_LEFT;

    #[tokio::test]
    async fn release_drains_keyboard_and_pointer_state_together() {
        let mut emulation = InputEmulation::with_backend(Backend::Dummy, None)
            .await
            .unwrap();
        let handle = 7;
        assert!(emulation.create(handle).await);

        emulation
            .consume(
                Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: 28,
                    state: 1,
                }),
                handle,
            )
            .await
            .unwrap();
        emulation
            .consume(
                Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button: BTN_LEFT,
                    state: 1,
                }),
                handle,
            )
            .await
            .unwrap();

        assert_eq!(emulation.pressed_keys[&handle], HashSet::from([28]));
        assert_eq!(
            emulation.pressed_buttons[&handle],
            HashSet::from([BTN_LEFT])
        );

        emulation.release_inputs(handle).await.unwrap();

        assert!(emulation.pressed_keys[&handle].is_empty());
        assert!(emulation.pressed_buttons[&handle].is_empty());
    }
}
