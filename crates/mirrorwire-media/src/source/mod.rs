use crate::frame::RawFrame;
use anyhow::Result;
use std::time::Duration;

#[cfg(all(target_os = "linux", feature = "portal"))]
pub mod portal;
pub mod testpattern;

/// A pull-based source of raw frames. `next_frame` blocks until a frame is
/// available and returns `None` when the source has ended.
pub trait FrameSource: Send {
    fn next_frame(&mut self) -> Result<Option<RawFrame>>;

    /// Wait up to `timeout` for a frame. `Ok(None)` means no frame arrived in
    /// time but the source is still running. Override this for damage-driven
    /// sources (portal) so a held frame can be sent when a viewer connects.
    fn poll_frame(&mut self, _timeout: Duration) -> Result<Option<RawFrame>> {
        self.next_frame()
    }

    fn is_finished(&self) -> bool {
        false
    }

    /// Hands a frame buffer back so the source can reuse the allocation.
    fn recycle(&mut self, _buffer: Vec<u8>) {}

    fn describe(&self) -> String;
}
