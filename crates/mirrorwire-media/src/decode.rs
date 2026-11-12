use crate::hw::{self, HwRef};
use anyhow::{anyhow, bail, Result};
use ffmpeg_next::{codec, decoder, ffi, format::Pixel, frame, software::scaling, Packet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwDecode {
    None,
    Auto,
}

/// Low-delay H.264 decoder. Each call takes one complete access unit and returns
/// the decoded picture immediately (no frame reordering or frame threading).
pub struct Decoder {
    inner: decoder::Video,
    _hw: Option<HwRef>,
    hw_frame: frame::Video,
    scratch: frame::Video,
    sw_frame: frame::Video,
    name: String,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(hw: HwDecode) -> Result<Self> {
        match hw {
            HwDecode::None => Self::open(None),
            HwDecode::Auto => match open_hw_device() {
                Ok((dev, label)) => match Self::open(Some((dev, label))) {
                    Ok(decoder) => Ok(decoder),
                    Err(e) => {
                        tracing::warn!(
                            "{label} H.264 decode failed to open ({e:#}); falling back to software"
                        );
                        Self::open(None)
                    }
                },
                Err(e) => {
                    let kind = if cfg!(target_os = "macos") {
                        "VideoToolbox"
                    } else {
                        "VAAPI"
                    };
                    tracing::info!(
                        "{kind} H.264 hardware decode unavailable ({e:#}); using software decoder"
                    );
                    Self::open(None)
                }
            },
        }
    }

    fn open(hw: Option<(HwRef, &'static str)>) -> Result<Self> {
        let codec = decoder::find(codec::Id::H264).ok_or_else(|| anyhow!("no H.264 decoder"))?;
        let mut ctx = codec::context::Context::new_with_codec(codec);
        ctx.set_threading(codec::threading::Config {
            kind: codec::threading::Type::Slice,
            count: 0,
        });
        #[allow(clippy::unnecessary_cast)]
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*raw).flags2 |= ffi::AV_CODEC_FLAG2_FAST as i32;
        }

        let (name, device) = if let Some((dev, label)) = hw {
            unsafe { (*ctx.as_mut_ptr()).hw_device_ctx = dev.new_ref() };
            (format!("h264 ({label})"), Some(dev))
        } else {
            ("h264 (software)".to_owned(), None)
        };

        let inner = ctx.decoder().video()?;
        Ok(Self {
            inner,
            _hw: device,
            hw_frame: frame::Video::empty(),
            scratch: frame::Video::empty(),
            sw_frame: frame::Video::empty(),
            name,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_hardware(&self) -> bool {
        self._hw.is_some()
    }

    /// Decodes one access unit. Returns a CPU-side frame (YUV420P or NV12).
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<&frame::Video>> {
        let packet = Packet::copy(data);
        self.inner.send_packet(&packet)?;
        let mut got = false;
        while self.inner.receive_frame(&mut self.scratch).is_ok() {
            std::mem::swap(&mut self.scratch, &mut self.hw_frame);
            got = true;
        }
        if !got {
            return Ok(None);
        }
        if is_hw_format(self.hw_frame.format()) {
            unsafe {
                let ret = ffi::av_hwframe_transfer_data(
                    self.sw_frame.as_mut_ptr(),
                    self.hw_frame.as_ptr(),
                    0,
                );
                if ret < 0 {
                    bail!("hw frame download: {}", ffmpeg_next::Error::from(ret));
                }
            }
            Ok(Some(&self.sw_frame))
        } else {
            Ok(Some(&self.hw_frame))
        }
    }
}

fn is_hw_format(p: Pixel) -> bool {
    matches!(p, Pixel::VAAPI | Pixel::VIDEOTOOLBOX)
}

fn open_hw_device() -> Result<(HwRef, &'static str)> {
    if cfg!(target_os = "macos") {
        Ok((
            hw::create_device(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX, None)?,
            "videotoolbox",
        ))
    } else {
        Ok((
            hw::create_device(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, None)?,
            "vaapi",
        ))
    }
}

/// Converts decoded frames to tightly packed RGBA for display.
pub struct RgbaConverter {
    ctx: Option<(scaling::Context, Pixel, u32, u32)>,
    out: frame::Video,
}

unsafe impl Send for RgbaConverter {}

impl Default for RgbaConverter {
    fn default() -> Self {
        Self {
            ctx: None,
            out: frame::Video::empty(),
        }
    }
}

impl RgbaConverter {
    pub fn convert(&mut self, src: &frame::Video, dst: &mut Vec<u8>) -> Result<(u32, u32)> {
        let key = (src.format(), src.width(), src.height());
        if self
            .ctx
            .as_ref()
            .is_none_or(|(_, f, w, h)| (*f, *w, *h) != key)
        {
            // Same-size YUV→RGBA: bicubic + accurate chroma siting, so 4:2:0
            // desktop text is less muddy. The texture stays at the stream's
            // native width×height; this is not a scale.
            let ctx = scaling::Context::get(
                key.0,
                key.1,
                key.2,
                Pixel::RGBA,
                key.1,
                key.2,
                scaling::Flags::BICUBIC
                    | scaling::Flags::ACCURATE_RND
                    | scaling::Flags::FULL_CHR_H_INT
                    | scaling::Flags::FULL_CHR_H_INP,
            )?;
            self.ctx = Some((ctx, key.0, key.1, key.2));
            self.out = frame::Video::new(Pixel::RGBA, key.1, key.2);
        }
        let (ctx, ..) = self.ctx.as_mut().expect("scaler initialized");
        ctx.run(src, &mut self.out)?;
        let (w, h) = (key.1 as usize, key.2 as usize);
        let stride = self.out.stride(0);
        let data = self.out.data(0);
        dst.clear();
        dst.reserve(w * h * 4);
        for y in 0..h {
            dst.extend_from_slice(&data[y * stride..y * stride + w * 4]);
        }
        Ok((key.1, key.2))
    }
}

/// Returns the luma plane of a decoded frame as (data, stride, width, height).
pub fn luma(frame: &frame::Video) -> (&[u8], usize, usize, usize) {
    (
        frame.data(0),
        frame.stride(0),
        frame.width() as usize,
        frame.height() as usize,
    )
}
