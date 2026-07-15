use super::{ClipboardBackend, wayland::WaylandClipboardBackend, x11::X11ClipboardBackend};
use crate::{ClipboardData, ClipboardReason, NativeGeneration};

pub enum LinuxClipboardBackend {
    Wayland(WaylandClipboardBackend),
    X11(X11ClipboardBackend),
}

impl LinuxClipboardBackend {
    pub fn connect() -> Result<Self, ClipboardReason> {
        match WaylandClipboardBackend::connect() {
            Ok(backend) => return Ok(Self::Wayland(backend)),
            Err(reason) => tracing::debug!(
                event = "clipboard_backend_probe_failed",
                backend = "wayland_data_control",
                reason = reason.code()
            ),
        }
        X11ClipboardBackend::connect().map(Self::X11)
    }

    pub const fn name(&self) -> &'static str {
        match self {
            Self::Wayland(_) => "wayland_data_control",
            Self::X11(_) => "x11_clipboard",
        }
    }
}

impl ClipboardBackend for LinuxClipboardBackend {
    fn initialize(&mut self) -> Result<(), ClipboardReason> {
        self.generation().map(|_| ())
    }

    fn generation(&mut self) -> Result<NativeGeneration, ClipboardReason> {
        match self {
            Self::Wayland(backend) => backend.generation(),
            Self::X11(backend) => backend.generation(),
        }
    }

    fn capture(&mut self, max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
        match self {
            Self::Wayland(backend) => backend.capture(max_bytes),
            Self::X11(backend) => backend.capture(max_bytes),
        }
    }

    fn apply(
        &mut self,
        expected_generation: NativeGeneration,
        data: &ClipboardData,
    ) -> Result<NativeGeneration, ClipboardReason> {
        match self {
            Self::Wayland(backend) => backend.apply(expected_generation, data),
            Self::X11(backend) => backend.apply(expected_generation, data),
        }
    }

    fn shutdown(&mut self) {
        match self {
            Self::Wayland(backend) => backend.shutdown(),
            Self::X11(backend) => backend.shutdown(),
        }
    }
}
