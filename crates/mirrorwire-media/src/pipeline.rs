use crate::encode::Encoder;
use crate::frame::EncodedFrame;
use crate::source::FrameSource;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Destination for encoded frames. The QUIC transport implements this; a
/// Miracast/RTSP sink could implement it the same way.
pub trait VideoSink: Send {
    fn send(&mut self, frame: EncodedFrame) -> Result<()>;

    /// Whether anyone is receiving; capture keeps running but encoding is skipped when false.
    fn wants_frames(&self) -> bool {
        true
    }
}

#[derive(Clone, Default)]
pub struct Controls {
    pub keyframe_requested: Arc<AtomicBool>,
    pub stop: Arc<AtomicBool>,
}

impl Controls {
    pub fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// Pulls frames from `source`, encodes them, and pushes them into `sink` until
/// the source ends or `controls.stop()` is called.
pub fn run_capture_encode(
    source: &mut dyn FrameSource,
    encoder: &mut Encoder,
    sink: &mut dyn VideoSink,
    controls: &Controls,
) -> Result<()> {
    let mut first = true;
    while !controls.stopped() {
        let Some(raw) = source.next_frame()? else { break };
        if !sink.wants_frames() {
            source.recycle(raw.data);
            first = true;
            continue;
        }
        let force = first || controls.keyframe_requested.swap(false, Ordering::AcqRel);
        first = false;
        let encoded = encoder.encode(&raw, force)?;
        source.recycle(raw.data);
        for frame in encoded {
            sink.send(frame)?;
        }
    }
    Ok(())
}
