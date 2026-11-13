use crate::encode::{DmaBufError, Encoder, EncoderConfig, RateControl};
use crate::frame::{EncodedFrame, RawFrame};
use crate::source::FrameSource;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// When the compositor sends nothing new (it only sends frames on damage),
/// the last frame is encoded again after this long, so the stream never goes
/// quiet: a floor of 10 frames per second. Repeats are cheap skip frames.
pub const REPEAT_AFTER: Duration = Duration::from_millis(100);

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
    /// Ask the encode loop to hand back the encoder so the capture source can
    /// be reopened (new cursor mode or frame rate), see [`CaptureExit`].
    pub restart_source: Arc<AtomicBool>,
    /// Encoder bitrate the network allows right now (kbit/s, adaptive
    /// bitrate minus FEC room); 0 = no limit, use the setting.
    pub bitrate_cap_kbps: Arc<AtomicU32>,
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

    pub fn request_source_restart(&self) {
        self.restart_source.store(true, Ordering::Release);
    }

    pub fn set_bitrate_cap(&self, kbps: u32) {
        self.bitrate_cap_kbps.store(kbps, Ordering::Release);
    }

    pub fn bitrate_cap(&self) -> u32 {
        self.bitrate_cap_kbps.load(Ordering::Acquire)
    }
}

/// Why [`continue_capture_encode`] returned.
pub enum CaptureExit {
    /// Stopped, or the source ended.
    Finished,
    /// [`Controls::request_source_restart`] was called: reopen the source and
    /// continue with this encoder.
    RestartSource(Box<Encoder>),
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
                    let _ = encode_one(
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
                            let _ = encode_one(
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

/// Encode one captured frame. Returns `false` when the frame was dropped
/// because the encoder could not use its DMA-BUF (the source is told to
/// switch to shared memory, and the next frame is a keyframe).
fn encode_one(
    encoder: &mut Encoder,
    mut raw: RawFrame,
    source: &mut dyn FrameSource,
    sink: &mut dyn VideoSink,
    controls: &Controls,
    first_for_viewer: &mut bool,
) -> Result<bool> {
    if raw.dmabuf.is_some() && !encoder.accepts_dmabuf() {
        source.disable_dmabuf(&format!("encoder {} cannot import DMA-BUF", encoder.name()));
        return Ok(false);
    }
    let force = *first_for_viewer || controls.keyframe_requested.swap(false, Ordering::AcqRel);
    let encoded = match encoder.encode(&raw, force) {
        Ok(e) => e,
        Err(e) if e.downcast_ref::<DmaBufError>().is_some() => {
            source.disable_dmabuf(&format!("{e:#}"));
            if force {
                controls.request_keyframe();
            }
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    *first_for_viewer = false;
    let (pack_ms, convert_ms, upload_ms, encode_ms) = encoder.last_breakdown_ms();
    if force {
        tracing::info!(
            "encoded IDR {}x{} pack={pack_ms:.2} convert={convert_ms:.2} upload={upload_ms:.2} encode={encode_ms:.2} ms via {}",
            raw.width,
            raw.height,
            encoder.conversion_path()
        );
    }
    // Close our DMA-BUF fd now rather than when the buffer is recycled.
    raw.dmabuf = None;
    source.recycle(raw.data);
    for frame in encoded {
        sink.send(frame)?;
    }
    Ok(true)
}

/// Re-encode the last frame (static screen). Honours keyframe requests, so a
/// viewer that lost a packet recovers without waiting for screen damage.
fn encode_repeat(
    encoder: &mut Encoder,
    seq: u64,
    sink: &mut dyn VideoSink,
    controls: &Controls,
) -> Result<bool> {
    let force = controls.keyframe_requested.swap(false, Ordering::AcqRel);
    let encoded = encoder.encode_repeat(seq, mirrorwire_proto::clock::now_us(), force)?;
    let any = !encoded.is_empty();
    for frame in encoded {
        sink.send(frame)?;
    }
    Ok(any)
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

/// What one pass of the encode loop did, for the presenter's fps counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// A new frame from the capture source was encoded.
    Captured,
    /// The last frame was encoded again because the source sent nothing new.
    Repeated,
}

/// Continue capturing and encoding, starting from an already-opened encoder and
/// an optional held frame (so a static screen can be sent when a viewer appears).
/// While a viewer is connected and the source is quiet, the last frame is
/// repeated every [`REPEAT_AFTER`].
#[allow(clippy::too_many_arguments)]
pub fn continue_capture_encode(
    source: &mut dyn FrameSource,
    mut held: Option<RawFrame>,
    mut encoder: Encoder,
    sink: &mut dyn VideoSink,
    controls: &Controls,
    settings: Arc<Mutex<EncoderConfig>>,
    mut on_reopen: impl FnMut(&Encoder) -> Result<()>,
    mut on_tick: impl FnMut(&Encoder, u32, u32, Tick),
) -> Result<CaptureExit> {
    let mut first_for_viewer = true;
    let mut track = BitrateTrack::default();
    let mut last_sent = Instant::now();
    let mut last_size = (0u32, 0u32);
    let mut repeat_seq = u64::MAX / 2;
    loop {
        if controls.stopped() {
            break;
        }
        if controls.restart_source.swap(false, Ordering::AcqRel) {
            if let Some(old) = held.take() {
                source.recycle(old.data);
            }
            return Ok(CaptureExit::RestartSource(Box::new(encoder)));
        }
        let poll = REPEAT_AFTER
            .saturating_sub(last_sent.elapsed())
            .clamp(Duration::from_millis(5), Duration::from_millis(50));
        match source.poll_frame(poll)? {
            Some(raw) => {
                maybe_replace_encoder(&mut encoder, &raw, &settings, controls, &mut track, &mut on_reopen, &mut first_for_viewer)?;
                if sink.wants_frames() {
                    if let Some(old) = held.take() {
                        source.recycle(old.data);
                    }
                    let (cw, ch) = (raw.width, raw.height);
                    if encode_one(&mut encoder, raw, source, sink, controls, &mut first_for_viewer)? {
                        last_sent = Instant::now();
                        last_size = (cw, ch);
                        on_tick(&encoder, cw, ch, Tick::Captured);
                    }
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
                if !sink.wants_frames() {
                    first_for_viewer = true;
                    continue;
                }
                if first_for_viewer {
                    if let Some(raw) = held.take() {
                        maybe_replace_encoder(&mut encoder, &raw, &settings, controls, &mut track, &mut on_reopen, &mut first_for_viewer)?;
                        let (cw, ch) = (raw.width, raw.height);
                        if encode_one(&mut encoder, raw, source, sink, controls, &mut first_for_viewer)? {
                            last_sent = Instant::now();
                            last_size = (cw, ch);
                            on_tick(&encoder, cw, ch, Tick::Captured);
                        }
                    }
                } else if last_sent.elapsed() >= REPEAT_AFTER {
                    repeat_seq = repeat_seq.wrapping_add(1);
                    if encode_repeat(&mut encoder, repeat_seq, sink, controls)? {
                        on_tick(&encoder, last_size.0, last_size.1, Tick::Repeated);
                    }
                    last_sent = Instant::now();
                }
            }
        }
    }
    Ok(CaptureExit::Finished)
}

/// What the encode loop does about a changed bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateAction {
    Keep,
    /// Change the running encoder's bitrate (x264; no keyframe).
    Live,
    /// Reopen the encoder (VAAPI & co.; costs a keyframe).
    Reopen,
}

/// Network-driven changes reopen an encoder at most this often.
pub const ADAPTIVE_REOPEN_GAP: Duration = Duration::from_secs(5);

/// Decide how to reach `wanted` kbit/s from `current`. A change of the
/// user's setting is applied at once; network-driven changes are applied
/// live when the encoder can, else by reopening, but only every
/// [`ADAPTIVE_REOPEN_GAP`] and only for a change of 15 % or more.
pub fn bitrate_action(
    current: u32,
    wanted: u32,
    user_changed: bool,
    live_ok: bool,
    since_reopen: Option<Duration>,
) -> BitrateAction {
    if current == wanted {
        return BitrateAction::Keep;
    }
    if user_changed {
        return if live_ok { BitrateAction::Live } else { BitrateAction::Reopen };
    }
    if live_ok {
        return BitrateAction::Live;
    }
    let rel = (wanted as f64 - current as f64).abs() / current.max(1) as f64;
    let rested = since_reopen.is_none_or(|d| d >= ADAPTIVE_REOPEN_GAP);
    if rel >= 0.15 && rested {
        BitrateAction::Reopen
    } else {
        BitrateAction::Keep
    }
}

/// The encoder bitrate for `user` kbit/s (the setting) under `cap` (0 = none).
pub fn effective_bitrate(user: u32, cap: u32) -> u32 {
    if cap == 0 {
        user
    } else {
        user.min(cap).max(100)
    }
}

/// Encode-loop memory for bitrate decisions.
#[derive(Debug, Default)]
struct BitrateTrack {
    user_kbps: Option<u32>,
    last_reopen: Option<Instant>,
}

fn maybe_replace_encoder(
    encoder: &mut Encoder,
    raw: &RawFrame,
    settings: &Arc<Mutex<EncoderConfig>>,
    controls: &Controls,
    track: &mut BitrateTrack,
    on_reopen: &mut impl FnMut(&Encoder) -> Result<()>,
    first_for_viewer: &mut bool,
) -> Result<bool> {
    let mut want = settings.lock().expect("encoder settings").clone();
    let user_kbps = want.bitrate_kbps;
    let user_changed = track.user_kbps.is_some_and(|u| u != user_kbps);
    track.user_kbps = Some(user_kbps);
    want.bitrate_kbps = effective_bitrate(user_kbps, controls.bitrate_cap());
    let (out_w, out_h) = want.resolved_size(raw.width, raw.height);
    let other_change = encoder.output_size() != (out_w, out_h)
        || encoder.config().rate_control != want.rate_control
        || encoder.config().qp != want.qp
        || encoder.config().keyint != want.keyint
        || encoder.config().intra_refresh != want.intra_refresh
        || encoder.config().backend != want.backend
        || encoder.config().fps != want.fps
        || encoder.config().vaapi_device != want.vaapi_device;
    if !other_change {
        let quality_mode = matches!(want.rate_control, RateControl::Cqp | RateControl::Icq);
        let current = encoder.config().bitrate_kbps;
        let action = if quality_mode && !user_changed {
            BitrateAction::Keep
        } else {
            bitrate_action(
                current,
                want.bitrate_kbps,
                user_changed,
                encoder.supports_live_bitrate(),
                track.last_reopen.map(|t| t.elapsed()),
            )
        };
        match action {
            BitrateAction::Keep => return Ok(false),
            BitrateAction::Live => {
                if encoder.set_bitrate_live(want.bitrate_kbps) {
                    tracing::debug!("encoder bitrate {current} -> {} kbit/s (live)", want.bitrate_kbps);
                    return Ok(false);
                }
            }
            BitrateAction::Reopen => {
                tracing::info!("encoder bitrate {current} -> {} kbit/s (reopen)", want.bitrate_kbps);
            }
        }
    }
    let fresh = match open_encoder_for(raw, &want) {
        Ok(e) => e,
        Err(e) if encoder.config().backend != want.backend => {
            // A live switch to an encoder that can't open here (no VAAPI,
            // feature not built…) must not end the stream: keep the old one.
            tracing::warn!(
                "switching to encoder {} failed ({e:#}); staying on {}",
                want.backend.as_str(),
                encoder.name()
            );
            settings.lock().expect("encoder settings").backend = encoder.config().backend;
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    *encoder = fresh;
    track.last_reopen = Some(Instant::now());
    on_reopen(encoder)?;
    *first_for_viewer = true;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::EncoderBackend;
    use crate::frame::PixelFormat;
    use std::time::Instant;

    /// One frame, then silence (like KWin on a static screen), then the end.
    struct OneShot {
        sent: bool,
        until: Instant,
    }

    impl FrameSource for OneShot {
        fn next_frame(&mut self) -> Result<Option<RawFrame>> {
            unreachable!("pipeline polls")
        }
        fn poll_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>> {
            if !self.sent {
                self.sent = true;
                return Ok(Some(RawFrame {
                    seq: 0,
                    width: 64,
                    height: 64,
                    stride: 256,
                    format: PixelFormat::Bgrx,
                    data: vec![128; 256 * 64],
                    capture_us: mirrorwire_proto::clock::now_us(),
                    dmabuf: None,
                }));
            }
            std::thread::sleep(timeout);
            Ok(None)
        }
        fn is_finished(&self) -> bool {
            self.sent && Instant::now() >= self.until
        }
        fn describe(&self) -> String {
            "one shot".into()
        }
    }

    struct Count(usize);
    impl VideoSink for Count {
        fn send(&mut self, _frame: EncodedFrame) -> Result<()> {
            self.0 += 1;
            Ok(())
        }
    }

    #[test]
    fn bitrate_changes_are_live_or_rate_limited() {
        use BitrateAction::*;
        let s = |n| Some(Duration::from_secs(n));
        assert_eq!(bitrate_action(8000, 8000, true, true, None), Keep);
        // The user's setting applies at once.
        assert_eq!(bitrate_action(8000, 4000, true, false, s(0)), Reopen);
        assert_eq!(bitrate_action(8000, 4000, true, true, s(0)), Live);
        // Network changes: live when possible (x264)…
        assert_eq!(bitrate_action(8000, 7900, false, true, s(0)), Live);
        // …else a reopen only every 5 s and for a 15 % change.
        assert_eq!(bitrate_action(8000, 7000, false, false, s(10)), Keep);
        assert_eq!(bitrate_action(8000, 6000, false, false, s(2)), Keep);
        assert_eq!(bitrate_action(8000, 6000, false, false, s(5)), Reopen);
        assert_eq!(bitrate_action(8000, 9500, false, false, None), Reopen);
        assert_eq!(effective_bitrate(40_000, 0), 40_000);
        assert_eq!(effective_bitrate(40_000, 12_000), 12_000);
        assert_eq!(effective_bitrate(5_000, 12_000), 5_000);
        assert_eq!(effective_bitrate(5_000, 1), 100);
    }

    #[test]
    fn network_cap_changes_x264_without_a_keyframe() {
        crate::init().unwrap();
        let cfg = EncoderConfig {
            backend: EncoderBackend::X264,
            bitrate_kbps: 8000,
            ..EncoderConfig::default()
        };
        let settings = Arc::new(Mutex::new(cfg.clone()));
        let mut source = OneShot { sent: false, until: Instant::now() };
        let raw = source.poll_frame(Duration::ZERO).unwrap().unwrap();
        let mut encoder = open_encoder_for(&raw, &cfg).unwrap();
        let controls = Controls::default();
        let mut track = BitrateTrack::default();
        let (mut reopened, mut first) = (0, false);
        let mut step = |enc: &mut Encoder, track: &mut BitrateTrack| {
            maybe_replace_encoder(enc, &raw, &settings, &controls, track, &mut |_| { reopened += 1; Ok(()) }, &mut first).unwrap()
        };
        assert!(!step(&mut encoder, &mut track));
        controls.set_bitrate_cap(3000);
        assert!(!step(&mut encoder, &mut track), "no reopen for a network change");
        assert_eq!(encoder.config().bitrate_kbps, 3000);
        // The user's new maximum still applies (and is capped by the network).
        settings.lock().unwrap().bitrate_kbps = 2000;
        step(&mut encoder, &mut track);
        assert_eq!(encoder.config().bitrate_kbps, 2000);
        controls.set_bitrate_cap(0);
        step(&mut encoder, &mut track);
        assert_eq!(encoder.config().bitrate_kbps, 2000);
        drop(step);
        assert_eq!(reopened, 0);
    }

    #[test]
    fn static_source_gets_repeated_frames() {
        crate::init().unwrap();
        let cfg = EncoderConfig {
            backend: EncoderBackend::X264,
            ..EncoderConfig::default()
        };
        let mut source = OneShot {
            sent: false,
            until: Instant::now() + Duration::from_millis(450),
        };
        let first = source.poll_frame(Duration::ZERO).unwrap().unwrap();
        let encoder = open_encoder_for(&first, &cfg).unwrap();
        let mut sink = Count(0);
        let (mut captured, mut repeated) = (0, 0);
        continue_capture_encode(
            &mut source,
            Some(first),
            encoder,
            &mut sink,
            &Controls::default(),
            Arc::new(Mutex::new(cfg)),
            |_| Ok(()),
            |_, _, _, tick| match tick {
                Tick::Captured => captured += 1,
                Tick::Repeated => repeated += 1,
            },
        )
        .unwrap();
        assert_eq!(captured, 1, "the held frame is sent once as a real frame");
        assert!(repeated >= 2, "expected repeats every {REPEAT_AFTER:?}, got {repeated}");
        assert_eq!(sink.0, captured + repeated);
    }
}
