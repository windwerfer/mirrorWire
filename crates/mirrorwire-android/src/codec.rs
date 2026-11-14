//! H.264 decode through the platform MediaCodec (NDK `AMediaCodec`), output
//! to an `AImageReader` (YUV_420_888) so every vendor's layout arrives as
//! planes with explicit strides.
//!
//! Decoder choice: the Java shell lists the device's H.264 decoders
//! ([`crate::codec_list`]); hardware decoders are tried first, then
//! software. A decoder that fails to start, or errors three times in a row,
//! is skipped and the reason is kept for the HUD.
//!
//! Output is drained on every call *and* by [`Decoder::poll`] when no new
//! frame arrives: MediaCodec returns pictures asynchronously, often after
//! the call that queued their input has returned.

use crate::codec_list::{self, CodecEntry};
use crate::h264;
use crate::yuv::{self, Plane};
use anyhow::{anyhow, bail, Result};
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MIME: &str = "video/avc";
/// Used when the Java shell's list is missing (should not happen).
const SOFTWARE_DECODERS: [&str; 2] = ["c2.android.avc.decoder", "OMX.google.h264.decoder"];
/// Image reader depth: one being converted, one arriving, spares for the codec.
const READER_IMAGES: i32 = 4;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What one call did, for the HUD counters.
#[derive(Default, Clone, Copy, Debug)]
pub struct Counts {
    pub queued: u64,
    pub input_full: u64,
    pub rendered: u64,
    /// Pictures decoded but dropped unseen because a newer frame was already waiting.
    pub superseded: u64,
}

/// Result of feeding one access unit.
pub enum Fed {
    /// Queued; a picture may already be out.
    Queued(Option<DecodedImage>),
    /// No input buffer free even after draining output: the access unit was
    /// dropped, so later frames lack a reference. Ask for a keyframe.
    InputFull(Option<DecodedImage>),
    /// Waiting for a keyframe to open the decoder.
    NotOpen,
}

struct Active {
    codec: MediaCodec,
    reader: ImageReader,
    _window: NativeWindow,
    entry: CodecEntry,
}

pub struct Decoder {
    width: i32,
    height: i32,
    active: Option<Active>,
    /// Decoders still to try, best first.
    candidates: VecDeque<CodecEntry>,
    /// Why better decoders were skipped (shown in the HUD).
    skipped: Vec<String>,
    errors: u32,
    label: String,
    spare: Vec<u8>,
    counts: Counts,
    /// Output buffers rendered whose image has not been acquired yet.
    unseen: u32,
    /// Queue time of inputs by pts, to measure input → output latency.
    in_flight: VecDeque<(i64, Instant)>,
    last_latency_ms: Option<f64>,
    last_convert_ms: f64,
}

impl Decoder {
    pub fn new(width: u32, height: u32, decoders: &[CodecEntry]) -> Self {
        let mut candidates: VecDeque<CodecEntry> = codec_list::ranked(decoders).into();
        if candidates.is_empty() {
            candidates.extend(SOFTWARE_DECODERS.iter().map(|n| CodecEntry {
                name: (*n).to_owned(),
                hardware: false,
                low_latency: false,
            }));
        }
        Self {
            width: width.max(16) as i32,
            height: height.max(16) as i32,
            active: None,
            candidates,
            skipped: Vec::new(),
            errors: 0,
            label: "MediaCodec (waiting for keyframe)".into(),
            spare: Vec::new(),
            counts: Counts::default(),
            unseen: 0,
            in_flight: VecDeque::new(),
            last_latency_ms: None,
            last_convert_ms: 0.0,
        }
    }

    /// `hardware decoder c2.qti.avc.decoder, low-latency` or
    /// `SOFTWARE decoder c2.android.avc.decoder (hardware c2.x failed: …)`.
    pub fn name(&self) -> &str {
        &self.label
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// Time from queueing an access unit to MediaCodec handing out its picture.
    pub fn latency_ms(&self) -> Option<f64> {
        self.last_latency_ms
    }

    /// YUV → RGBA conversion time of the last picture.
    pub fn convert_ms(&self) -> f64 {
        self.last_convert_ms
    }

    fn update_label(&mut self) {
        let Some(a) = &self.active else { return };
        let kind = if a.entry.hardware {
            "hardware"
        } else {
            "SOFTWARE"
        };
        let mut s = format!("{kind} decoder {}", a.entry.name);
        if a.entry.low_latency {
            s.push_str(", low-latency");
        }
        if let Some(why) = self.skipped.last() {
            s.push_str(&format!(" ({why})"));
        }
        self.label = s;
    }

    fn open_one(&self, entry: &CodecEntry, keyframe: &[u8]) -> Result<Active> {
        let (sps, pps) = h264::parameter_sets(keyframe);
        let reader = ImageReader::new(
            self.width,
            self.height,
            ImageFormat::YUV_420_888,
            READER_IMAGES,
        )
        .map_err(|e| anyhow!("AImageReader: {e:?}"))?;
        let window = reader
            .window()
            .map_err(|e| anyhow!("AImageReader window: {e:?}"))?;
        let codec =
            MediaCodec::from_codec_name(&entry.name).ok_or_else(|| anyhow!("cannot create"))?;
        let mut format = MediaFormat::new();
        format.set_str("mime", MIME);
        format.set_i32("width", self.width);
        format.set_i32("height", self.height);
        format.set_i32("low-latency", 1); // KEY_LOW_LATENCY, Android 11+; ignored before
        format.set_i32("priority", 0); // realtime
        for key in codec_list::vendor_low_latency_keys(&entry.name) {
            format.set_i32(key, 1);
        }
        if let Some(sps) = &sps {
            format.set_buffer("csd-0", sps);
        }
        if let Some(pps) = &pps {
            format.set_buffer("csd-1", pps);
        }
        codec
            .configure(&format, Some(&window), MediaCodecDirection::Decoder)
            .map_err(|e| anyhow!("configure: {e:?}"))?;
        codec.start().map_err(|e| anyhow!("start: {e:?}"))?;
        Ok(Active {
            codec,
            reader,
            _window: window,
            entry: entry.clone(),
        })
    }

    /// Opens the best decoder that starts, skipping the ones that don't.
    fn open(&mut self, keyframe: &[u8]) -> Result<()> {
        while let Some(entry) = self.candidates.front().cloned() {
            match self.open_one(&entry, keyframe) {
                Ok(active) => {
                    tracing::info!(
                        "decoder {} ({}x{}) started",
                        entry.name,
                        self.width,
                        self.height
                    );
                    self.active = Some(active);
                    self.errors = 0;
                    self.unseen = 0;
                    self.in_flight.clear();
                    self.update_label();
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("decoder {} failed to start: {e:#}", entry.name);
                    self.skip_current(format!("{} failed: {e:#}", entry.name));
                }
            }
        }
        let why = self.skipped.last().cloned().unwrap_or_default();
        self.label = format!("no H.264 decoder works ({why})");
        bail!("no H.264 decoder could be started ({why})")
    }

    fn skip_current(&mut self, why: String) {
        self.candidates.pop_front();
        self.active = None;
        self.skipped.push(why);
    }

    /// Feed one access unit; returns a picture when one is ready. With
    /// `want_picture == false` (a newer frame is already queued) finished
    /// pictures are released without being shown or converted: the decoder
    /// still sees every frame, but the CPU-heavy YUV → RGBA copy is skipped,
    /// so a slow phone catches up instead of falling further behind.
    pub fn decode(
        &mut self,
        au: &[u8],
        keyframe: bool,
        pts_us: u64,
        want_picture: bool,
    ) -> Result<Fed> {
        if self.active.is_none() {
            if !keyframe {
                return Ok(Fed::NotOpen);
            }
            self.open(au)?;
        }
        match self.decode_inner(au, pts_us, want_picture) {
            Ok(fed) => {
                self.errors = 0;
                Ok(fed)
            }
            Err(e) => {
                self.errors += 1;
                if self.errors >= 3 {
                    let name = self
                        .active
                        .as_ref()
                        .map(|a| a.entry.name.clone())
                        .unwrap_or_default();
                    tracing::warn!("decoder {name} keeps failing ({e:#}); trying the next one");
                    self.skip_current(format!("{name} failed: {e:#}"));
                    self.errors = 0;
                }
                Err(e)
            }
        }
    }

    /// Picks up pictures that finished after the last [`Decoder::decode`]
    /// returned. Call this when no new frame arrived for a few milliseconds.
    pub fn poll(&mut self) -> Result<Option<DecodedImage>> {
        if self.active.is_none() {
            return Ok(None);
        }
        self.drain(Duration::ZERO, true)
    }

    fn decode_inner(&mut self, au: &[u8], pts_us: u64, want: bool) -> Result<Fed> {
        let pts = pts_us as i64;
        // Make room first: a decoder whose output is not drained stops taking input.
        let mut picture = self.drain(Duration::ZERO, want)?;
        let mut queued = self.queue(au, pts, Duration::from_millis(15))?;
        if !queued {
            if let Some(p) = self.drain(Duration::from_millis(10), want)? {
                self.recycle_picture(picture.replace(p));
            }
            queued = self.queue(au, pts, Duration::from_millis(15))?;
        }
        if !queued {
            self.counts.input_full += 1;
            return Ok(Fed::InputFull(picture));
        }
        self.counts.queued += 1;
        if want {
            if let Some(p) = self.drain(Duration::from_millis(5), true)? {
                self.recycle_picture(picture.replace(p));
            }
        }
        Ok(Fed::Queued(picture))
    }

    fn recycle_picture(&mut self, old: Option<DecodedImage>) {
        if let Some(old) = old {
            self.spare = old.rgba;
        }
    }

    fn queue(&mut self, au: &[u8], pts: i64, wait: Duration) -> Result<bool> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| anyhow!("decoder not open"))?;
        let codec = &active.codec;
        match codec
            .dequeue_input_buffer(wait)
            .map_err(|e| anyhow!("dequeue input: {e:?}"))?
        {
            DequeuedInputBufferResult::Buffer(mut input) => {
                let buf = input.buffer_mut();
                if au.len() > buf.len() {
                    bail!("access unit {} B > input buffer {} B", au.len(), buf.len());
                }
                for (d, s) in buf.iter_mut().zip(au) {
                    d.write(*s);
                }
                codec
                    .queue_input_buffer(input, 0, au.len(), pts as u64, 0)
                    .map_err(|e| anyhow!("queue input: {e:?}"))?;
                if self.in_flight.len() >= 32 {
                    self.in_flight.pop_front();
                }
                self.in_flight.push_back((pts, Instant::now()));
                Ok(true)
            }
            DequeuedInputBufferResult::TryAgainLater => Ok(false),
        }
    }

    /// Takes every finished output buffer. With `want` the newest one is
    /// rendered to the image reader and converted; older ones in the same
    /// batch (and all of them without `want`) are released unrendered.
    /// `wait` is how long to wait for the first buffer.
    fn drain(&mut self, wait: Duration, want: bool) -> Result<Option<DecodedImage>> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| anyhow!("decoder not open"))?;
        let codec = &active.codec;
        let mut wait = wait;
        let mut rendered_now = 0u32;
        let mut newest = None;
        loop {
            match codec
                .dequeue_output_buffer(wait)
                .map_err(|e| anyhow!("dequeue output: {e:?}"))?
            {
                DequeuedOutputBufferInfoResult::Buffer(out) => {
                    let pts = out.info().presentation_time_us();
                    if let Some(older) = newest.replace(out) {
                        codec
                            .release_output_buffer(older, false)
                            .map_err(|e| anyhow!("release output: {e:?}"))?;
                        self.counts.superseded += 1;
                    }
                    wait = Duration::ZERO;
                    while let Some(&(p, t)) = self.in_flight.front() {
                        if p > pts {
                            break;
                        }
                        self.in_flight.pop_front();
                        if p == pts {
                            self.last_latency_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
                        }
                    }
                }
                DequeuedOutputBufferInfoResult::OutputFormatChanged => {
                    let f = codec.output_format();
                    tracing::info!(
                        "decoder output {}x{} colour-format {:?}",
                        f.i32("width").unwrap_or(0),
                        f.i32("height").unwrap_or(0),
                        f.i32("color-format")
                    );
                }
                DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
                DequeuedOutputBufferInfoResult::TryAgainLater => break,
            }
        }
        if let Some(out) = newest {
            codec
                .release_output_buffer(out, want)
                .map_err(|e| anyhow!("release output: {e:?}"))?;
            if want {
                rendered_now += 1;
            } else {
                self.counts.superseded += 1;
            }
        }
        self.counts.rendered += rendered_now as u64;
        self.unseen = self.unseen.saturating_add(rendered_now);
        if self.unseen == 0 {
            return Ok(None);
        }
        // A rendered picture reaches the reader asynchronously; wait a little
        // only right after rendering, otherwise poll() picks it up later.
        let tries = if rendered_now > 0 { 10 } else { 1 };
        for _ in 0..tries {
            match active.reader.acquire_latest_image() {
                Ok(AcquireResult::Image(img)) => {
                    self.unseen = 0;
                    let rgba = std::mem::take(&mut self.spare);
                    let t = Instant::now();
                    let picture = convert(&img, rgba)?;
                    self.last_convert_ms = t.elapsed().as_secs_f64() * 1000.0;
                    return Ok(Some(picture));
                }
                Ok(AcquireResult::MaxImagesAcquired) => {
                    bail!("image reader: all {READER_IMAGES} images in use")
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => bail!("acquire image: {e:?}"),
            }
        }
        Ok(None)
    }

    /// Give a used RGBA buffer back to avoid an allocation per frame.
    pub fn recycle(&mut self, buf: Vec<u8>) {
        self.spare = buf;
    }
}

/// YUV planes of `img` → RGBA, cropped to the visible area (decoders pad
/// 1080 to 1088 rows).
fn convert(img: &Image, mut rgba: Vec<u8>) -> Result<DecodedImage> {
    let full_w = img.width().map_err(|e| anyhow!("{e:?}"))?.max(0) as usize;
    let full_h = img.height().map_err(|e| anyhow!("{e:?}"))?.max(0) as usize;
    let (w, h) = match img.crop_rect() {
        Ok(c) if c.right > c.left && c.bottom > c.top => (
            (c.right.max(0) as usize).min(full_w),
            (c.bottom.max(0) as usize).min(full_h),
        ),
        _ => (full_w, full_h),
    };
    let plane = |i: i32| -> Result<Plane<'_>> {
        Ok(Plane {
            data: img.plane_data(i).map_err(|e| anyhow!("plane {i}: {e:?}"))?,
            row_stride: img
                .plane_row_stride(i)
                .map_err(|e| anyhow!("{e:?}"))?
                .max(0) as usize,
            pixel_stride: img
                .plane_pixel_stride(i)
                .map_err(|e| anyhow!("{e:?}"))?
                .max(1) as usize,
        })
    };
    let (y, u, v) = (plane(0)?, plane(1)?, plane(2)?);
    yuv::to_rgba(w, h, y, u, v, &mut rgba);
    Ok(DecodedImage {
        width: w as u32,
        height: h as u32,
        rgba,
    })
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Some(a) = &self.active {
            let _ = a.codec.stop();
        }
    }
}
