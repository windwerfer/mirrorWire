//! H.264 decode through the platform MediaCodec (NDK `AMediaCodec`), output
//! to an `AImageReader` (YUV_420_888) so every vendor's layout arrives as
//! planes with explicit strides. Hardware decoder first; if it fails to start
//! or keeps erroring, Google's software decoder (present on every device).

use crate::h264;
use crate::yuv::{self, Plane};
use anyhow::{anyhow, bail, Context, Result};
use ndk::media::image_reader::{AcquireResult, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use std::time::Duration;

const MIME: &str = "video/avc";
const SOFTWARE_DECODERS: [&str; 2] = ["c2.android.avc.decoder", "OMX.google.h264.decoder"];

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

struct Active {
    codec: MediaCodec,
    reader: ImageReader,
    _window: NativeWindow,
}

pub struct Decoder {
    width: i32,
    height: i32,
    active: Option<Active>,
    software: bool,
    errors: u32,
    name: String,
    spare: Vec<u8>,
}

impl Decoder {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(16) as i32,
            height: height.max(16) as i32,
            active: None,
            software: false,
            errors: 0,
            name: "MediaCodec (waiting for keyframe)".into(),
            spare: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn open(&mut self, keyframe: &[u8]) -> Result<()> {
        let (sps, pps) = h264::parameter_sets(keyframe);
        let reader = ImageReader::new(self.width, self.height, ImageFormat::YUV_420_888, 4)
            .map_err(|e| anyhow!("AImageReader: {e:?}"))?;
        let window = reader.window().map_err(|e| anyhow!("AImageReader window: {e:?}"))?;
        let (codec, name) = if self.software {
            SOFTWARE_DECODERS
                .iter()
                .find_map(|n| MediaCodec::from_codec_name(n).map(|c| (c, format!("MediaCodec software ({n})"))))
                .ok_or_else(|| anyhow!("no software H.264 decoder on this device"))?
        } else {
            (
                MediaCodec::from_decoder_type(MIME).ok_or_else(|| anyhow!("no H.264 decoder"))?,
                "MediaCodec (hardware)".to_owned(),
            )
        };
        let mut format = MediaFormat::new();
        format.set_str("mime", MIME);
        format.set_i32("width", self.width);
        format.set_i32("height", self.height);
        format.set_i32("low-latency", 1); // Android 11+, ignored before
        format.set_i32("priority", 0); // realtime
        if let Some(sps) = &sps {
            format.set_buffer("csd-0", sps);
        }
        if let Some(pps) = &pps {
            format.set_buffer("csd-1", pps);
        }
        codec
            .configure(&format, Some(&window), MediaCodecDirection::Decoder)
            .map_err(|e| anyhow!("configure {name}: {e:?}"))?;
        codec.start().map_err(|e| anyhow!("start {name}: {e:?}"))?;
        tracing::info!("{name} {}x{} started", self.width, self.height);
        self.name = name;
        self.active = Some(Active { codec, reader, _window: window });
        Ok(())
    }

    /// Feed one access unit; returns a picture when one is ready.
    pub fn decode(&mut self, au: &[u8], keyframe: bool, pts_us: u64) -> Result<Option<DecodedImage>> {
        if self.active.is_none() {
            if !keyframe {
                return Ok(None);
            }
            if let Err(e) = self.open(au) {
                if self.software {
                    return Err(e);
                }
                tracing::warn!("hardware decoder failed ({e:#}); trying the software decoder");
                self.software = true;
                self.open(au).context("software decoder")?;
            }
        }
        match self.decode_inner(au, pts_us) {
            Ok(img) => {
                self.errors = 0;
                Ok(img)
            }
            Err(e) => {
                self.errors += 1;
                if self.errors >= 3 && !self.software {
                    tracing::warn!("hardware decode keeps failing ({e:#}); switching to software");
                    self.active = None;
                    self.software = true;
                    self.errors = 0;
                }
                Err(e)
            }
        }
    }

    fn decode_inner(&mut self, au: &[u8], pts_us: u64) -> Result<Option<DecodedImage>> {
        let active = self.active.as_ref().ok_or_else(|| anyhow!("decoder not open"))?;
        let codec = &active.codec;
        match codec
            .dequeue_input_buffer(Duration::from_millis(30))
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
                    .queue_input_buffer(input, 0, au.len(), pts_us, 0)
                    .map_err(|e| anyhow!("queue input: {e:?}"))?;
            }
            DequeuedInputBufferResult::TryAgainLater => {
                tracing::debug!("decoder input full, dropping access unit");
            }
        }
        let mut rendered = false;
        let mut wait = Duration::from_millis(10);
        loop {
            match codec
                .dequeue_output_buffer(wait)
                .map_err(|e| anyhow!("dequeue output: {e:?}"))?
            {
                DequeuedOutputBufferInfoResult::Buffer(out) => {
                    codec
                        .release_output_buffer(out, true)
                        .map_err(|e| anyhow!("release output: {e:?}"))?;
                    rendered = true;
                    wait = Duration::ZERO;
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
        // The rendered picture reaches the ImageReader asynchronously.
        let tries = if rendered { 15 } else { 1 };
        for _ in 0..tries {
            match active.reader.acquire_latest_image() {
                Ok(AcquireResult::Image(img)) => {
                    let w = img.width().map_err(|e| anyhow!("{e:?}"))?.max(0) as usize;
                    let h = img.height().map_err(|e| anyhow!("{e:?}"))?.max(0) as usize;
                    let plane = |i: i32| -> Result<Plane<'_>> {
                        Ok(Plane {
                            data: img.plane_data(i).map_err(|e| anyhow!("plane {i}: {e:?}"))?,
                            row_stride: img.plane_row_stride(i).map_err(|e| anyhow!("{e:?}"))?.max(0) as usize,
                            pixel_stride: img.plane_pixel_stride(i).map_err(|e| anyhow!("{e:?}"))?.max(1) as usize,
                        })
                    };
                    let (y, u, v) = (plane(0)?, plane(1)?, plane(2)?);
                    let mut rgba = std::mem::take(&mut self.spare);
                    yuv::to_rgba(w, h, y, u, v, &mut rgba);
                    return Ok(Some(DecodedImage { width: w as u32, height: h as u32, rgba }));
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

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Some(a) = &self.active {
            let _ = a.codec.stop();
        }
    }
}
