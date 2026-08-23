use crate::{ClipboardData, ClipboardReason, NativeGeneration};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(target_os = "linux")]
pub use linux::LinuxClipboardBackend;
#[cfg(target_os = "macos")]
pub use macos::MacOsClipboardBackend;
#[cfg(target_os = "windows")]
pub use windows::WindowsClipboardBackend;

/// Synchronous contract owned and called by one thread-affine clipboard actor.
pub trait ClipboardBackend: Send + 'static {
    fn initialize(&mut self) -> Result<NativeGeneration, ClipboardReason> {
        self.generation()
    }

    fn generation(&mut self) -> Result<NativeGeneration, ClipboardReason>;
    fn capture(&mut self, max_bytes: usize) -> Result<ClipboardData, ClipboardReason>;
    fn apply(
        &mut self,
        expected_generation: NativeGeneration,
        data: &ClipboardData,
    ) -> Result<NativeGeneration, ClipboardReason>;
    fn shutdown(&mut self) {}
}
