use crate::frame::RawFrame;
use anyhow::Result;
use mirrorwire_proto::cursor::CursorEvent;
use std::sync::Arc;
use std::time::Duration;

/// How the capture source delivers the mouse cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorCapture {
    /// Not captured at all.
    Hidden,
    /// Drawn into the video frames by the compositor (the classic way).
    Embedded,
    /// Left out of the video; position and image reported separately
    /// through a [`CursorSink`] ("cursor as coordinates").
    Metadata,
}

impl CursorCapture {
    /// The name used in `stream_info.cursor`, the status and the HUD.
    pub fn as_str(self) -> &'static str {
        match self {
            CursorCapture::Hidden => mirrorwire_proto::control::CURSOR_HIDDEN,
            CursorCapture::Embedded => mirrorwire_proto::control::CURSOR_EMBEDDED,
            CursorCapture::Metadata => mirrorwire_proto::control::CURSOR_SEPARATE,
        }
    }
}

/// Receives cursor changes from the capture thread. Must not block.
pub type CursorSink = Arc<dyn Fn(CursorEvent) + Send + Sync>;

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

    /// The encoder cannot use DMA-BUF frames (no VAAPI, or the import
    /// failed): switch the capture to shared memory. No-op for sources that
    /// never produce DMA-BUFs.
    fn disable_dmabuf(&mut self, _reason: &str) {}

    fn describe(&self) -> String;

    /// How the cursor is delivered by this source (after any fallback).
    fn cursor_capture(&self) -> CursorCapture {
        CursorCapture::Embedded
    }

    /// Why the requested cursor mode could not be used, if it could not.
    fn cursor_note(&self) -> Option<String> {
        None
    }

    /// Maximum frame rate agreed with the compositor, once known.
    fn negotiated_fps(&self) -> Option<u32> {
        None
    }
}
