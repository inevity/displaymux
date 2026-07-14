use crate::{ClipboardData, ClipboardReason, NativeGeneration};

/// Synchronous contract owned and called by one thread-affine clipboard actor.
pub trait ClipboardBackend: Send + 'static {
    fn generation(&mut self) -> Result<NativeGeneration, ClipboardReason>;
    fn capture(&mut self, max_bytes: usize) -> Result<ClipboardData, ClipboardReason>;
    fn apply(
        &mut self,
        expected_generation: NativeGeneration,
        data: &ClipboardData,
    ) -> Result<NativeGeneration, ClipboardReason>;
    fn shutdown(&mut self) {}
}
