use crate::encode::{Encoder, EncoderConfig};
use crate::frame::{EncodedFrame, RawFrame};
use crate::source::FrameSource;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Pulls frames from `source`, opens (and re-opens) the encoder to follow
/// capture size and live settings, and pushes encoded frames into `sink`.
///
/// `on_ready` is called after the first frame has determined the output size
/// and the encoder is open, and again whenever the encoder is re-created
/// (resolution or quality settings changed). The first capture frame is held
/// so a static desktop (portal damage-only) can still be sent when a viewer
/// connects.
pub fn run_capture_encode(
    source: &mut dyn FrameSource,
    sink: &mut dyn VideoSink,
    controls: &Controls,
    settings: Arc<Mutex<EncoderConfig>>,
    mut on_ready: impl FnMut(&Encoder) -> Result<()>,
) -> Result<()> {
    let mut encoder: Option<Encoder> = None;
    let mut held: Option<RawFrame> = None;
    let mut first_for_viewer = true;
    let poll = Duration::from_millis(50);

    while !controls.stopped() {
        match source.poll_frame(poll)? {
            Some(raw) => {
                maybe_reopen(&mut encoder, &raw, &settings, &mut on_ready, &mut first_for_viewer)?;
                if sink.wants_frames() {
                    if let Some(old) = held.take() {
                        source.recycle(old.data);
                    }
                    encode_one(
                        encoder.as_mut().expect("encoder"),
                        raw,
                        source,
                        sink,
                        controls,
                        &mut first_for_viewer,
                    )?;
                } else {
                    first_for_viewer = true;
                    if let Some(old) = held.replace(raw) {
                        source.recycle(old.data);
                    }
                }
            }
            None => {
                if source.is_finished() {
                    break;
                }
                if sink.wants_frames() {
                    if first_for_viewer {
                        if let Some(raw) = held.take() {
                            maybe_reopen(
                                &mut encoder,
                                &raw,
                                &settings,
                                &mut on_ready,
                                &mut first_for_viewer,
                            )?;
                            encode_one(
                                encoder.as_mut().expect("encoder"),
                                raw,
                                source,
                                sink,
                                controls,
                                &mut first_for_viewer,
                            )?;
                        }
                    } else if controls.keyframe_requested.load(Ordering::Acquire) {
                        if let Some(raw) = held.as_ref() {
                            // Can't move out of held without clone of pixels; wait for a new frame
                            // or request will stay set until the next captured frame.
                            let _ = raw;
                        }
                    }
                } else {
                    first_for_viewer = true;
                }
            }
        }
    }
    Ok(())
}

fn maybe_reopen(
    encoder: &mut Option<Encoder>,
    raw: &RawFrame,
    settings: &Arc<Mutex<EncoderConfig>>,
    on_ready: &mut impl FnMut(&Encoder) -> Result<()>,
    first_for_viewer: &mut bool,
) -> Result<()> {
    let want = settings.lock().expect("encoder settings").clone();
    let (out_w, out_h) = want.resolved_size(raw.width, raw.height);
    let reopen = match encoder {
        None => true,
        Some(enc) => {
            enc.output_size() != (out_w, out_h)
                || enc.config().bitrate_kbps != want.bitrate_kbps
                || enc.config().rate_control != want.rate_control
                || enc.config().qp != want.qp
                || enc.config().keyint != want.keyint
                || enc.config().intra_refresh != want.intra_refresh
                || enc.config().backend != want.backend
                || enc.config().fps != want.fps
                || enc.config().vaapi_device != want.vaapi_device
        }
    };
    if !reopen {
        return Ok(());
    }
    let mut resolved = want;
    resolved.width = out_w;
    resolved.height = out_h;
    tracing::info!(
        "opening encoder at {out_w}x{out_h} (capture {}x{})",
        raw.width,
        raw.height
    );
    let enc = Encoder::new(resolved)?;
    on_ready(&enc)?;
    *encoder = Some(enc);
    *first_for_viewer = true;
    Ok(())
}

fn encode_one(
    encoder: &mut Encoder,
    raw: RawFrame,
    source: &mut dyn FrameSource,
    sink: &mut dyn VideoSink,
    controls: &Controls,
    first_for_viewer: &mut bool,
) -> Result<()> {
    let force = *first_for_viewer || controls.keyframe_requested.swap(false, Ordering::AcqRel);
    *first_for_viewer = false;
    let encoded = encoder.encode(&raw, force)?;
    let (pack_ms, convert_ms, upload_ms, encode_ms) = encoder.last_breakdown_ms();
    if force {
        tracing::info!(
            "encoded IDR {}x{} pack={pack_ms:.2} convert={convert_ms:.2} upload={upload_ms:.2} encode={encode_ms:.2} ms via {}",
            raw.width,
            raw.height,
            encoder.conversion_path()
        );
    }
    source.recycle(raw.data);
    for frame in encoded {
        sink.send(frame)?;
    }
    Ok(())
}


/// Wait until the source produces a frame (or ends / is stopped).
pub fn wait_first_frame(
    source: &mut dyn FrameSource,
    controls: &Controls,
) -> Result<Option<RawFrame>> {
    let poll = Duration::from_millis(50);
    while !controls.stopped() {
        if let Some(raw) = source.poll_frame(poll)? {
            return Ok(Some(raw));
        }
        if source.is_finished() {
            return Ok(None);
        }
    }
    Ok(None)
}

pub fn open_encoder_for(raw: &RawFrame, settings: &EncoderConfig) -> Result<Encoder> {
    let mut resolved = settings.clone();
    let (w, h) = resolved.resolved_size(raw.width, raw.height);
    resolved.width = w;
    resolved.height = h;
    tracing::info!(
        "opening encoder at {w}x{h} (capture {}x{})",
        raw.width,
        raw.height
    );
    Encoder::new(resolved)
}

/// Continue capturing and encoding, starting from an already-opened encoder and
/// an optional held frame (so a static screen can be sent when a viewer appears).
#[allow(clippy::too_many_arguments)]
pub fn continue_capture_encode(
    source: &mut dyn FrameSource,
    mut held: Option<RawFrame>,
    mut encoder: Encoder,
    sink: &mut dyn VideoSink,
    controls: &Controls,
    settings: Arc<Mutex<EncoderConfig>>,
    mut on_reopen: impl FnMut(&Encoder) -> Result<()>,
    mut on_tick: impl FnMut(&Encoder, u32, u32),
) -> Result<()> {
    let mut first_for_viewer = true;
    let poll = Duration::from_millis(50);
    // If we already hold a frame and a viewer is already connected, encode it first.
    loop {
        if controls.stopped() {
            break;
        }
        match source.poll_frame(poll)? {
            Some(raw) => {
                if maybe_replace_encoder(&mut encoder, &raw, &settings, &mut on_reopen, &mut first_for_viewer)? {
                    // reopened
                }
                if sink.wants_frames() {
                    if let Some(old) = held.take() {
                        source.recycle(old.data);
                    }
                    let (cw, ch) = (raw.width, raw.height);
                    encode_one(&mut encoder, raw, source, sink, controls, &mut first_for_viewer)?;
                    on_tick(&encoder, cw, ch);
                } else {
                    first_for_viewer = true;
                    if let Some(old) = held.replace(raw) {
                        source.recycle(old.data);
                    }
                }
            }
            None => {
                if source.is_finished() {
                    break;
                }
                if sink.wants_frames() {
                    if first_for_viewer {
                        if let Some(raw) = held.take() {
                            maybe_replace_encoder(&mut encoder, &raw, &settings, &mut on_reopen, &mut first_for_viewer)?;
                            let (cw, ch) = (raw.width, raw.height);
                            encode_one(&mut encoder, raw, source, sink, controls, &mut first_for_viewer)?;
                            on_tick(&encoder, cw, ch);
                        }
                    }
                } else {
                    first_for_viewer = true;
                }
            }
        }
    }
    Ok(())
}

fn maybe_replace_encoder(
    encoder: &mut Encoder,
    raw: &RawFrame,
    settings: &Arc<Mutex<EncoderConfig>>,
    on_reopen: &mut impl FnMut(&Encoder) -> Result<()>,
    first_for_viewer: &mut bool,
) -> Result<bool> {
    let want = settings.lock().expect("encoder settings").clone();
    let (out_w, out_h) = want.resolved_size(raw.width, raw.height);
    let reopen = encoder.output_size() != (out_w, out_h)
        || encoder.config().bitrate_kbps != want.bitrate_kbps
        || encoder.config().rate_control != want.rate_control
        || encoder.config().qp != want.qp
        || encoder.config().keyint != want.keyint
        || encoder.config().intra_refresh != want.intra_refresh
        || encoder.config().backend != want.backend
        || encoder.config().fps != want.fps
        || encoder.config().vaapi_device != want.vaapi_device;
    if !reopen {
        return Ok(false);
    }
    *encoder = open_encoder_for(raw, &want)?;
    on_reopen(encoder)?;
    *first_for_viewer = true;
    Ok(true)
}
