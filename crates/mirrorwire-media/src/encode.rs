use crate::frame::{EncodedFrame, PixelFormat, RawFrame};
use crate::hw::{self, HwRef};
use anyhow::{anyhow, bail, Context as _, Result};
use bytes::Bytes;
use ffmpeg_next::{codec, encoder, ffi, format::Pixel, software::scaling, Dictionary, Packet};
use mirrorwire_proto::clock::now_us;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    Auto,
    Vaapi,
    X264,
    VideoToolbox,
}

impl std::str::FromStr for EncoderBackend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "auto" => Self::Auto,
            "vaapi" => Self::Vaapi,
            "x264" | "software" => Self::X264,
            "videotoolbox" => Self::VideoToolbox,
            _ => bail!("unknown encoder backend {s:?} (auto, vaapi, x264, videotoolbox)"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub backend: EncoderBackend,
    pub intra_refresh: bool,
    pub vaapi_device: String,
}

enum Upload {
    Software,
    Vaapi { _device: HwRef, frames: HwRef },
}

struct Scaler {
    ctx: scaling::Context,
    src: (PixelFormat, u32, u32),
}

unsafe impl Send for Scaler {}

/// Low-latency H.264 encoder: no B-frames, no lookahead, one packet out per
/// frame in, IDR on demand.
pub struct Encoder {
    inner: encoder::Video,
    upload: Upload,
    scaler: Option<Scaler>,
    staging: ffmpeg_next::frame::Video,
    config: EncoderConfig,
    name: &'static str,
    pts: i64,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        let order: &[EncoderBackend] = match config.backend {
            EncoderBackend::Auto if cfg!(target_os = "macos") => {
                &[EncoderBackend::VideoToolbox, EncoderBackend::X264]
            }
            EncoderBackend::Auto => &[EncoderBackend::Vaapi, EncoderBackend::X264],
            EncoderBackend::Vaapi => &[EncoderBackend::Vaapi],
            EncoderBackend::X264 => &[EncoderBackend::X264],
            EncoderBackend::VideoToolbox => &[EncoderBackend::VideoToolbox],
        };
        let mut last_err = anyhow!("no encoder backend tried");
        for &backend in order {
            match Self::open(&config, backend) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    tracing::warn!("encoder {backend:?} unavailable: {e:#}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    fn open(config: &EncoderConfig, backend: EncoderBackend) -> Result<Self> {
        let (codec_name, sw_format) = match backend {
            EncoderBackend::Vaapi => ("h264_vaapi", Pixel::NV12),
            EncoderBackend::X264 => ("libx264", Pixel::YUV420P),
            EncoderBackend::VideoToolbox => ("h264_videotoolbox", Pixel::NV12),
            EncoderBackend::Auto => unreachable!(),
        };
        let codec = encoder::find_by_name(codec_name)
            .ok_or_else(|| anyhow!("{codec_name} not built into this FFmpeg"))?;
        let mut enc = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;

        let bitrate = config.bitrate_kbps as usize * 1000;
        enc.set_width(config.width);
        enc.set_height(config.height);
        enc.set_time_base((1, config.fps as i32));
        enc.set_frame_rate(Some((config.fps as i32, 1)));
        enc.set_bit_rate(bitrate);
        enc.set_max_bit_rate(bitrate);
        enc.set_max_b_frames(0);
        enc.set_gop(if config.intra_refresh { config.fps } else { config.fps * 10 });
        unsafe {
            let raw = enc.as_mut_ptr();
            (*raw).rc_buffer_size = (bitrate / config.fps.max(1) as usize * 2) as i32;
            (*raw).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
        }

        let mut opts = Dictionary::new();
        let upload = match backend {
            EncoderBackend::Vaapi => {
                let device = hw::create_device(
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    Some(&config.vaapi_device),
                )?;
                let frames = hw::create_frames(
                    &device,
                    ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                    ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                    config.width,
                    config.height,
                )?;
                enc.set_format(Pixel::VAAPI);
                unsafe { (*enc.as_mut_ptr()).hw_frames_ctx = frames.new_ref() };
                opts.set("rc_mode", "CBR");
                opts.set("async_depth", "1");
                opts.set("profile", "main");
                if config.intra_refresh {
                    opts.set("idr_interval", "0");
                }
                Upload::Vaapi { _device: device, frames }
            }
            EncoderBackend::X264 => {
                enc.set_format(sw_format);
                opts.set("preset", "ultrafast");
                opts.set("tune", "zerolatency");
                opts.set("forced-idr", "1");
                if config.intra_refresh {
                    opts.set("intra-refresh", "1");
                }
                Upload::Software
            }
            EncoderBackend::VideoToolbox => {
                enc.set_format(sw_format);
                opts.set("realtime", "1");
                opts.set("allow_sw", "0");
                opts.set("prio_speed", "1");
                Upload::Software
            }
            EncoderBackend::Auto => unreachable!(),
        };

        let inner = enc
            .open_as_with(codec, opts)
            .with_context(|| format!("opening {codec_name}"))?;
        Ok(Self {
            inner,
            upload,
            scaler: None,
            staging: ffmpeg_next::frame::Video::new(sw_format, config.width, config.height),
            config: config.clone(),
            name: codec_name,
            pts: 0,
        })
    }

    pub fn encode(&mut self, raw: &RawFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        self.convert(raw)?;
        let pts = self.pts;
        self.pts += 1;
        let kind = if force_keyframe {
            ffmpeg_next::picture::Type::I
        } else {
            ffmpeg_next::picture::Type::None
        };

        match &self.upload {
            Upload::Software => {
                self.staging.set_pts(Some(pts));
                self.staging.set_kind(kind);
                self.inner.send_frame(&self.staging)?;
            }
            Upload::Vaapi { frames, .. } => unsafe {
                let mut hw_frame = ffmpeg_next::frame::Video::empty();
                let ret = ffi::av_hwframe_get_buffer(frames.as_ptr(), hw_frame.as_mut_ptr(), 0);
                if ret < 0 {
                    bail!("av_hwframe_get_buffer: {}", ffmpeg_next::Error::from(ret));
                }
                let ret = ffi::av_hwframe_transfer_data(hw_frame.as_mut_ptr(), self.staging.as_ptr(), 0);
                if ret < 0 {
                    bail!("av_hwframe_transfer_data: {}", ffmpeg_next::Error::from(ret));
                }
                hw_frame.set_pts(Some(pts));
                hw_frame.set_kind(kind);
                self.inner.send_frame(&hw_frame)?;
            },
        }

        let mut out = Vec::with_capacity(1);
        let mut packet = Packet::empty();
        while self.inner.receive_packet(&mut packet).is_ok() {
            if let Some(data) = packet.data() {
                out.push(EncodedFrame {
                    seq: raw.seq,
                    keyframe: packet.is_key(),
                    data: Bytes::copy_from_slice(data),
                    capture_us: raw.capture_us,
                    encoded_us: now_us(),
                });
            }
        }
        Ok(out)
    }

    fn convert(&mut self, raw: &RawFrame) -> Result<()> {
        let key = (raw.format, raw.width, raw.height);
        if self.scaler.as_ref().is_none_or(|s| s.src != key) {
            let ctx = scaling::Context::get(
                raw.format.to_ffmpeg(),
                raw.width,
                raw.height,
                self.staging.format(),
                self.config.width,
                self.config.height,
                scaling::Flags::FAST_BILINEAR,
            )?;
            self.scaler = Some(Scaler { ctx, src: key });
        }
        let needed = raw.stride * raw.height as usize;
        if raw.data.len() < needed {
            bail!("raw frame too small: {} < {needed}", raw.data.len());
        }
        let scaler = self.scaler.as_mut().expect("scaler initialized");
        let src = [raw.data.as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null()];
        let src_stride = [raw.stride as i32, 0, 0, 0];
        unsafe {
            let dst = self.staging.as_mut_ptr();
            let ret = ffi::sws_scale(
                scaler.ctx.as_mut_ptr(),
                src.as_ptr(),
                src_stride.as_ptr(),
                0,
                raw.height as i32,
                (*dst).data.as_ptr(),
                (*dst).linesize.as_ptr(),
            );
            if ret <= 0 {
                bail!("sws_scale failed: {ret}");
            }
        }
        Ok(())
    }
}
