use crate::frame::RawFrame;
use anyhow::Result;

#[cfg(all(target_os = "linux", feature = "portal"))]
pub mod portal;
pub mod testpattern;

/// A pull-based source of raw frames. `next_frame` blocks until a frame is
/// available and returns `None` when the source has ended.
pub trait FrameSource: Send {
    fn next_frame(&mut self) -> Result<Option<RawFrame>>;

    /// Hands a frame buffer back so the source can reuse the allocation.
    fn recycle(&mut self, _buffer: Vec<u8>) {}

    fn describe(&self) -> String;
}
