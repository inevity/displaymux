#[derive(Debug, Error)]
pub enum InputEmulationError {
    #[error("error creating input-emulation: `{0}`")]
    Create(#[from] EmulationCreationError),
    #[error("error emulating input: `{0}`")]
    Emulate(#[from] EmulationError),
}

impl InputEmulationError {
    pub fn is_transient_input_unavailable(&self) -> bool {
        if let Self::Create(error) = self {
            return error.is_transient_input_unavailable();
        }
        #[cfg(windows)]
        if matches!(self, Self::Emulate(EmulationError::WindowsSendInput(_))) {
            return true;
        }
        false
    }
}

#[cfg(any(libei, rdp))]
use ashpd::{Error::Response, desktop::ResponseError};
use std::io;
use thiserror::Error;

#[cfg(wlroots)]
use wayland_client::{
    ConnectError, DispatchError,
    backend::WaylandError,
    globals::{BindError, GlobalError},
};

#[derive(Debug, Error)]
pub enum EmulationError {
    #[error("event stream closed")]
    EndOfStream,
    #[cfg(libei)]
    #[error("libei error: `{0}`")]
    Libei(#[from] reis::Error),
    #[cfg(wlroots)]
    #[error("wayland error: `{0}`")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    #[cfg(any(rdp, libei))]
    #[error("xdg-desktop-portal: `{0}`")]
    Ashpd(#[from] ashpd::Error),
    #[error("io error: `{0}`")]
    Io(#[from] io::Error),
    #[cfg(windows)]
    #[error("Windows SendInput submitted no event: `{0}`")]
    WindowsSendInput(io::Error),
}

#[derive(Debug, Error)]
pub enum EmulationCreationError {
    #[cfg(wlroots)]
    #[error("wlroots backend: `{0}`")]
    Wlroots(#[from] WlrootsEmulationCreationError),
    #[cfg(libei)]
    #[error("libei backend: `{0}`")]
    Libei(#[from] LibeiEmulationCreationError),
    #[cfg(rdp)]
    #[error("xdg-desktop-portal: `{0}`")]
    Xdp(#[from] XdpEmulationCreationError),
    #[cfg(x11)]
    #[error("x11: `{0}`")]
    X11(#[from] X11EmulationCreationError),
    #[cfg(target_os = "macos")]
    #[error("macos: `{0}`")]
    MacOs(#[from] MacOSEmulationCreationError),
    #[cfg(windows)]
    #[error("windows: `{0}`")]
    Windows(#[from] WindowsEmulationCreationError),
    #[error("capture error")]
    NoAvailableBackend,
}

impl EmulationCreationError {
    pub(crate) fn is_transient_input_unavailable(&self) -> bool {
        #[cfg(windows)]
        if matches!(
            self,
            Self::Windows(WindowsEmulationCreationError::InputUnavailable(_))
        ) {
            return true;
        }
        false
    }

    /// request was intentionally denied by the user
    pub(crate) fn cancelled_by_user(&self) -> bool {
        #[cfg(libei)]
        if matches!(
            self,
            EmulationCreationError::Libei(LibeiEmulationCreationError::Ashpd(Response(
                ResponseError::Cancelled,
            )))
        ) {
            return true;
        }
        #[cfg(rdp)]
        if matches!(
            self,
            EmulationCreationError::Xdp(XdpEmulationCreationError::Ashpd(Response(
                ResponseError::Cancelled,
            )))
        ) {
            return true;
        }
        false
    }
}

#[cfg(wlroots)]
#[derive(Debug, Error)]
pub enum WlrootsEmulationCreationError {
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error(transparent)]
    Global(#[from] GlobalError),
    #[error(transparent)]
    Wayland(#[from] WaylandError),
    #[error(transparent)]
    Bind(#[from] WaylandBindError),
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(wlroots)]
#[derive(Debug, Error)]
#[error("wayland protocol \"{protocol}\" not supported: {inner}")]
pub struct WaylandBindError {
    inner: BindError,
    protocol: &'static str,
}

#[cfg(wlroots)]
impl WaylandBindError {
    pub(crate) fn new(inner: BindError, protocol: &'static str) -> Self {
        Self { inner, protocol }
    }
}

#[cfg(libei)]
#[derive(Debug, Error)]
pub enum LibeiEmulationCreationError {
    #[error(transparent)]
    Ashpd(#[from] ashpd::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Reis(#[from] reis::Error),
}

#[cfg(rdp)]
#[derive(Debug, Error)]
pub enum XdpEmulationCreationError {
    #[error(transparent)]
    Ashpd(#[from] ashpd::Error),
    #[error("configured display targeting is not supported by this portal session: {0:?}")]
    DisplayTargetUnsupported(String),
}

#[cfg(x11)]
#[derive(Debug, Error)]
pub enum X11EmulationCreationError {
    #[error("could not open display")]
    OpenDisplay,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Error)]
pub enum MacOSEmulationCreationError {
    #[error("could not create event source")]
    EventSourceCreation,
    #[error("accessibility permission is required")]
    AccessibilityPermission,
    #[error("input control permission is required")]
    InputControlPermission,
    #[error("invalid macOS display selector: {0}")]
    InvalidDisplaySelector(String),
}

#[cfg(windows)]
#[derive(Debug, Error)]
pub enum WindowsEmulationCreationError {
    #[error("input injection unavailable: `{0}`")]
    InputUnavailable(io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_emulation_errors_require_explicit_reenable() {
        let error = InputEmulationError::Emulate(EmulationError::Io(io::Error::other("failed")));

        assert!(!error.is_transient_input_unavailable());
    }

    #[cfg(windows)]
    #[test]
    fn windows_input_denials_are_transient() {
        let runtime_error = InputEmulationError::Emulate(EmulationError::WindowsSendInput(
            io::Error::from_raw_os_error(5),
        ));
        let creation_error = EmulationCreationError::Windows(
            WindowsEmulationCreationError::InputUnavailable(io::Error::from_raw_os_error(5)),
        );

        assert!(runtime_error.is_transient_input_unavailable());
        assert!(creation_error.is_transient_input_unavailable());
        assert!(
            InputEmulationError::Create(creation_error).is_transient_input_unavailable()
        );
    }
}
