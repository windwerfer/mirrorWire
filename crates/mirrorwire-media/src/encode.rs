use crate::frame::{EncodedFrame, PixelFormat, RawFrame};
use crate::hw::{self, HwRef};
use anyhow::{anyhow, bail, Context as _, Result};
use bytes::Bytes;
use ffmpeg_next::{codec, encoder, ffi, format::Pixel, software::scaling, Dictionary, Packet};
use mirrorwire_proto::clock::now_us;

/// Default target bitrate for desktop content. Per-frame bits are based on
/// wall-clock PTS, so a static screen (few portal damage frames) still gets
/// a sharp picture instead of `bitrate / configured_fps` scraps.
pub const DEFAULT_BITRATE_KBPS: u32 = 40_000;
pub const DEFAULT_QP: u32 = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    Auto,
    Vaapi,
    X264,
    VideoToolbox,
    /// Pure-Rust FilmCraft encoder (cargo feature `filmcraft`), never picked by `Auto`.
    Filmcraft,
}

impl std::str::FromStr for EncoderBackend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "vaapi" => Self::Vaapi,
            "x264" | "software" => Self::X264,
            "videotoolbox" => Self::VideoToolbox,
            "filmcraft" => Self::Filmcraft,
            _ => bail!("unknown encoder backend {s:?} (auto, vaapi, x264, videotoolbox, filmcraft)"),
        })
    }
}

impl EncoderBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vaapi => "vaapi",
            Self::X264 => "x264",
            Self::VideoToolbox => "videotoolbox",
            Self::Filmcraft => "filmcraft",
        }
    }

    /// Whether this build can open the backend at all (FilmCraft is a cargo feature).
    pub fn compiled_in(self) -> bool {
        !matches!(self, Self::Filmcraft) || cfg!(feature = "filmcraft")
    }

    /// Backends worth offering on this platform in this build (for the GUI selector).
    pub fn selectable() -> Vec<Self> {
        let mut v = vec![Self::Auto];
        if cfg!(target_os = "linux") {
            v.push(Self::Vaapi);
        }
        if cfg!(target_os = "macos") {
            v.push(Self::VideoToolbox);
        }
        v.push(Self::X264);
        if Self::Filmcraft.compiled_in() {
            v.push(Self::Filmcraft);
        }
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// CBR for VAAPI/VideoToolbox, bitrate mode for x264.
    Auto,
    Cbr,
    Vbr,
    Cqp,
    Icq,
}

impl std::str::FromStr for RateControl {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "cbr" => Self::Cbr,
            "vbr" => Self::Vbr,
            "cqp" | "qp" => Self::Cqp,
            "icq" => Self::Icq,
            _ => bail!("unknown rate control {s:?} (auto, cbr, vbr, cqp, icq)"),
        })
    }
}

impl RateControl {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cbr => "cbr",
            Self::Vbr => "vbr",
            Self::Cqp => "cqp",
            Self::Icq => "icq",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Auto, Self::Cbr, Self::Vbr, Self::Cqp, Self::Icq]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderConfig {
    /// `0` means "use the capture size" (resolved by the pipeline).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub backend: EncoderBackend,
    pub rate_control: RateControl,
    pub qp: u32,
    /// GOP length in frames. `None` = 10 seconds at `fps`.
    pub keyint: Option<u32>,
    pub intra_refresh: bool,
    pub vaapi_device: String,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            fps: 60,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            backend: EncoderBackend::Auto,
            rate_control: RateControl::Auto,
            qp: DEFAULT_QP,
            keyint: None,
            intra_refresh: false,
            vaapi_device: "/dev/dri/renderD128".into(),
        }
    }
}

impl EncoderConfig {
    pub fn resolved_size(&self, src_w: u32, src_h: u32) -> (u32, u32) {
        match (self.width, self.height) {
            (0, 0) => (src_w, src_h),
            (0, h) => (src_w, h),
            (w, 0) => (w, src_h),
            (w, h) => (w, h),
        }
    }

    pub fn gop_frames(&self) -> u32 {
        self.keyint
            .filter(|k| *k > 0)
            .unwrap_or_else(|| self.fps.saturating_mul(10).max(1))
    }

    /// Settings that require the encoder to be re-opened (not just an IDR).
    pub fn reopen_needed(&self, other: &Self, out_w: u32, out_h: u32) -> bool {
        let (ew, eh) = self.resolved_size(out_w, out_h);
        ew != out_w
            || eh != out_h
            || self.bitrate_kbps != other.bitrate_kbps
            || self.backend != other.backend
            || self.rate_control != other.rate_control
            || self.qp != other.qp
            || self.keyint != other.keyint
            || self.intra_refresh != other.intra_refresh
            || self.vaapi_device != other.vaapi_device
            || self.fps != other.fps
    }
}

/// Parses `1920x1080` or `1920×1080`.
pub fn parse_size(s: &str) -> Result<(u32, u32)> {
    let s = s.trim().replace('×', "x");
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| anyhow!("size must be WIDTHxHEIGHT, got {s:?}"))?;
    Ok((w.trim().parse()?, h.trim().parse()?))
}

enum Upload {
    Software,
    Vaapi {
        _device: HwRef,
        frames: HwRef,
        vpp: Option<VaapiVpp>,
        dma: Option<DmaImport>,
    },
}

struct VaapiVpp {
    graph: ffmpeg_next::filter::Graph,
    src_format: Pixel,
    src_w: u32,
    src_h: u32,
}

/// DMA-BUF import: a VAAPI frames context at the capture size (the DRM-PRIME
/// buffers are mapped into it as VAAPI surfaces) and a VPP graph that scales
/// and converts those surfaces to NV12 on the GPU.
struct DmaImport {
    frames: HwRef,
    key: (PixelFormat, u32, u32),
    graph: ffmpeg_next::filter::Graph,
}

unsafe impl Send for DmaImport {}

/// The encoder could not use a DMA-BUF frame. The capture should switch to
/// shared memory (`FrameSource::disable_dmabuf`); the frame is dropped.
#[derive(Debug)]
pub struct DmaBufError(pub String);

impl std::fmt::Display for DmaBufError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DmaBufError {}

/// Capture/convert path labels shown in the presenter status and log.
pub mod path {
    /// Compositor GPU buffer imported into VAAPI, converted by VPP, encoded. No CPU copy.
    pub const DMABUF_ZERO_COPY: &str = "dmabuf-zero-copy";
    /// Shared-memory frame uploaded to the GPU, converted by VAAPI VPP.
    pub const SHM_VPP: &str = "shm+upload (vaapi-vpp)";
    /// Shared-memory frame converted to NV12 on the CPU, then uploaded.
    pub const SHM_CPU_UPLOAD: &str = "shm+upload (cpu convert)";
    /// Software encoder: CPU conversion, no GPU.
    pub const SHM_CPU: &str = "shm (cpu)";
}

struct Scaler {
    ctx: scaling::Context,
    src: (PixelFormat, u32, u32),
}

unsafe impl Send for Scaler {}
unsafe impl Send for VaapiVpp {}

/// Low-latency H.264 encoder: no B-frames, no lookahead, one packet out per
/// frame in, IDR on demand. Input is converted to NV12 on the GPU (VAAPI VPP)
/// when that path is available, otherwise on the CPU.
/// The codec doing the actual encoding.
enum Core {
    Ffmpeg(encoder::Video),
    #[cfg(feature = "filmcraft")]
    Filmcraft(Box<crate::filmcraft::FcEncoder>),
}

pub struct Encoder {
    inner: Core,
    upload: Upload,
    scaler: Option<Scaler>,
    staging: ffmpeg_next::frame::Video,
    packed: ffmpeg_next::frame::Video,
    config: EncoderConfig,
    name: &'static str,
    conversion: &'static str,
    rc_summary: String,
    start_us: Option<u64>,
    last_pts: Option<i64>,
    /// A converted frame is ready in `packed`/`staging` (for repeats).
    have_frame: bool,
    /// Why the VPP or DMA-BUF path was dropped, for the status line.
    fallback_note: Option<String>,
    last_pack_us: u64,
    last_convert_us: u64,
    last_upload_us: u64,
    last_encode_us: u64,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        if config.width == 0 || config.height == 0 {
            bail!("encoder size is not resolved yet");
        }
        let order: &[EncoderBackend] = match config.backend {
            EncoderBackend::Auto if cfg!(target_os = "macos") => {
                &[EncoderBackend::VideoToolbox, EncoderBackend::X264]
            }
            EncoderBackend::Auto => &[EncoderBackend::Vaapi, EncoderBackend::X264],
            EncoderBackend::Vaapi => &[EncoderBackend::Vaapi],
            EncoderBackend::X264 => &[EncoderBackend::X264],
            EncoderBackend::VideoToolbox => &[EncoderBackend::VideoToolbox],
            EncoderBackend::Filmcraft => &[EncoderBackend::Filmcraft],
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

    pub fn conversion_path(&self) -> &'static str {
        self.conversion
    }

    pub fn rc_summary(&self) -> &str {
        &self.rc_summary
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    pub fn last_timings_ms(&self) -> (f64, f64) {
        (
            self.last_convert_us as f64 / 1000.0,
            self.last_encode_us as f64 / 1000.0,
        )
    }

    /// pack / colour-convert / upload / encode, in milliseconds.
    pub fn last_breakdown_ms(&self) -> (f64, f64, f64, f64) {
        (
            self.last_pack_us as f64 / 1000.0,
            self.last_convert_us as f64 / 1000.0,
            self.last_upload_us as f64 / 1000.0,
            self.last_encode_us as f64 / 1000.0,
        )
    }

    fn open(config: &EncoderConfig, backend: EncoderBackend) -> Result<Self> {
        if backend == EncoderBackend::Filmcraft {
            return Self::open_filmcraft(config);
        }
        let (codec_name, sw_format) = match backend {
            EncoderBackend::Vaapi => ("h264_vaapi", Pixel::NV12),
            EncoderBackend::X264 => ("libx264", Pixel::YUV420P),
            EncoderBackend::VideoToolbox => ("h264_videotoolbox", Pixel::NV12),
            EncoderBackend::Auto | EncoderBackend::Filmcraft => unreachable!(),
        };
        let codec = encoder::find_by_name(codec_name)
            .ok_or_else(|| anyhow!("{codec_name} not built into this FFmpeg"))?;
        let mut enc = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;

        let bitrate = config.bitrate_kbps as i64 * 1000;
        let gop = config.gop_frames();
        enc.set_width(config.width);
        enc.set_height(config.height);
        // Microsecond time base so rate control follows wall-clock gaps
        // (portal damage frames can arrive far below the configured fps).
        enc.set_time_base((1, 1_000_000));
        enc.set_frame_rate(Some((config.fps as i32, 1)));
        enc.set_max_b_frames(0);
        enc.set_gop(gop);
        unsafe {
            let raw = enc.as_mut_ptr();
            (*raw).profile = ffi::FF_PROFILE_H264_HIGH;
            (*raw).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*raw).flags |= ffi::AV_CODEC_FLAG_CLOSED_GOP as i32;
        }

        let rc = effective_rc(backend, config.rate_control);
        apply_rate_control(&mut enc, backend, rc, bitrate, config.qp)?;

        let mut opts = Dictionary::new();
        let mut upload = match backend {
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
                apply_vaapi_opts(&mut opts, rc, bitrate, config);
                Upload::Vaapi {
                    vpp: None,
                    dma: None,
                    _device: device,
                    frames,
                }
            }
            EncoderBackend::X264 => {
                enc.set_format(sw_format);
                opts.set("preset", "ultrafast");
                opts.set("tune", "zerolatency");
                opts.set("forced-idr", "1");
                opts.set("profile", "high");
                if config.intra_refresh {
                    opts.set("intra-refresh", "1");
                }
                match rc {
                    RateControl::Cqp => {
                        opts.set("crf", &config.qp.to_string());
                    }
                    _ => {
                        opts.set("nal-hrd", "cbr");
                    }
                }
                Upload::Software
            }
            EncoderBackend::VideoToolbox => {
                enc.set_format(sw_format);
                opts.set("realtime", "1");
                opts.set("allow_sw", "0");
                opts.set("prio_speed", "1");
                opts.set("profile", "high");
                if matches!(rc, RateControl::Cqp) {
                    opts.set("quality", &format_vt_quality(config.qp));
                }
                Upload::Software
            }
            EncoderBackend::Auto | EncoderBackend::Filmcraft => unreachable!(),
        };

        let inner = enc
            .open_as_with(codec, opts)
            .with_context(|| format!("opening {codec_name}"))?;

        let mut conversion = path::SHM_CPU;
        if let Upload::Vaapi {
            _device: ref device,
            ..
        } = upload
        {
            match VaapiVpp::try_new(device, Pixel::BGRA, config.width, config.height) {
                Ok(_) => {
                    // Built for the output size; rebuilt on the first frame if
                    // the capture size differs. Probe succeeded, so the path
                    // is available.
                    conversion = path::SHM_VPP;
                }
                Err(e) => {
                    tracing::info!(
                        "VAAPI VPP (hwupload/scale_vaapi) unavailable ({e:#}); using CPU BGRx→NV12"
                    );
                    conversion = path::SHM_CPU_UPLOAD;
                }
            }
        }

        // Attach a real VPP graph only when the path is enabled; the probe
        // graph above was dropped. The first convert() builds one for the
        // source size.
        if conversion == path::SHM_VPP {
            if let Upload::Vaapi { vpp, _device, .. } = &mut upload {
                *vpp = None; // built lazily for the actual capture size/format
                let _ = _device;
            }
        }

        let rc_summary = describe_rc(&inner, codec_name, rc, config, conversion);
        tracing::info!("{rc_summary}");

        Ok(Self {
            inner: Core::Ffmpeg(inner),
            upload,
            scaler: None,
            staging: ffmpeg_next::frame::Video::new(sw_format, config.width, config.height),
            packed: ffmpeg_next::frame::Video::empty(),
            config: config.clone(),
            name: codec_name,
            conversion,
            rc_summary,
            start_us: None,
            last_pts: None,
            have_frame: false,
            fallback_note: None,
            last_pack_us: 0,
            last_convert_us: 0,
            last_upload_us: 0,
            last_encode_us: 0,
        })
    }

    #[cfg(feature = "filmcraft")]
    fn open_filmcraft(config: &EncoderConfig) -> Result<Self> {
        let fc = crate::filmcraft::FcEncoder::new(config)?;
        let rc_summary = fc.summary.clone();
        tracing::info!("{rc_summary}");
        Ok(Self {
            inner: Core::Filmcraft(Box::new(fc)),
            upload: Upload::Software,
            scaler: None,
            staging: ffmpeg_next::frame::Video::new(Pixel::YUV420P, config.width, config.height),
            packed: ffmpeg_next::frame::Video::empty(),
            config: config.clone(),
            name: crate::filmcraft::NAME,
            conversion: path::SHM_CPU,
            rc_summary,
            start_us: None,
            last_pts: None,
            have_frame: false,
            fallback_note: None,
            last_pack_us: 0,
            last_convert_us: 0,
            last_upload_us: 0,
            last_encode_us: 0,
        })
    }

    #[cfg(not(feature = "filmcraft"))]
    fn open_filmcraft(_config: &EncoderConfig) -> Result<Self> {
        bail!("this build has no FilmCraft encoder (cargo feature `filmcraft`)")
    }

    /// Whether this encoder can take DMA-BUF frames (VAAPI only).
    pub fn accepts_dmabuf(&self) -> bool {
        matches!(self.upload, Upload::Vaapi { .. })
    }

    /// Why a faster path (VPP, DMA-BUF) was dropped, if it was.
    pub fn fallback_note(&self) -> Option<&str> {
        self.fallback_note.as_deref()
    }

    pub fn encode(&mut self, raw: &RawFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        if raw.dmabuf.is_some() {
            self.convert_dmabuf(raw)?;
        } else {
            self.convert(raw)?;
        }
        self.have_frame = true;
        self.submit(raw.seq, raw.capture_us, force_keyframe)
    }

    /// Encode the last converted frame again with a new timestamp. Used when
    /// the compositor only sends frames on damage, so the viewer keeps getting
    /// a steady (cheap, mostly skip-block) stream. Returns nothing before the
    /// first real frame.
    pub fn encode_repeat(&mut self, seq: u64, capture_us: u64, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        if !self.have_frame {
            return Ok(Vec::new());
        }
        self.last_pack_us = 0;
        self.last_convert_us = 0;
        self.submit(seq, capture_us, force_keyframe)
    }

    fn submit(&mut self, seq: u64, capture_us: u64, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let t_upload0 = now_us();
        let start = *self.start_us.get_or_insert(capture_us);
        let mut pts = capture_us.saturating_sub(start) as i64;
        // Repeats and real frames interleave; keep PTS strictly increasing.
        if let Some(last) = self.last_pts {
            if pts <= last {
                pts = last + 1;
            }
        }
        self.last_pts = Some(pts);
        let kind = if force_keyframe {
            ffmpeg_next::picture::Type::I
        } else {
            ffmpeg_next::picture::Type::None
        };

        let mut out = Vec::with_capacity(1);
        #[cfg(feature = "filmcraft")]
        if let Core::Filmcraft(fc) = &mut self.inner {
            self.last_upload_us = 0;
            let t_enc0 = now_us();
            let aus = fc.encode(&self.staging, pts, force_keyframe)?;
            for (data, keyframe) in aus {
                out.push(EncodedFrame { seq, keyframe, data: Bytes::from(data), capture_us, encoded_us: now_us() });
            }
            self.last_encode_us = now_us().saturating_sub(t_enc0);
            return Ok(out);
        }
        // libx264 does its work inside send_frame, so the encode timer starts
        // there for software; for VAAPI it starts after the upload.
        let mut t_send = None;
        match &self.upload {
            Upload::Software => {
                self.staging.set_pts(Some(pts));
                self.staging.set_kind(kind);
                t_send = Some(now_us());
                ffmpeg_of(&mut self.inner)?.send_frame(&self.staging)?;
                self.last_upload_us = 0;
            }
            Upload::Vaapi { frames, .. } => {
                let frames_ptr = frames.as_ptr();
                if is_vaapi_pixel(self.packed.format()) {
                    // Already a GPU surface (VPP output, from shared memory or DMA-BUF).
                    self.packed.set_pts(Some(pts));
                    self.packed.set_kind(kind);
                    ffmpeg_of(&mut self.inner)?.send_frame(&self.packed)?;
                    self.last_upload_us = 0; // upload (if any) happened inside VPP
                } else {
                    // Prefer the VPP/CPU output in `packed` when it holds NV12
                    // system memory, otherwise the CPU scaler output in `staging`.
                    self.transfer_to_vaapi_ptr(frames_ptr, pts, kind)?;
                    self.last_upload_us = now_us().saturating_sub(t_upload0);
                }
            }
        }
        let t_enc0 = t_send.unwrap_or_else(now_us);

        let mut packet = Packet::empty();
        let inner = ffmpeg_of(&mut self.inner)?;
        while inner.receive_packet(&mut packet).is_ok() {
            if let Some(data) = packet.data() {
                out.push(EncodedFrame {
                    seq,
                    keyframe: packet.is_key(),
                    data: Bytes::copy_from_slice(data),
                    capture_us,
                    encoded_us: now_us(),
                });
            }
        }
        self.last_encode_us = now_us().saturating_sub(t_enc0);
        Ok(out)
    }

    /// Map a DMA-BUF into VAAPI and convert it with VPP. Any failure is a
    /// `DmaBufError`, so the caller switches the capture to shared memory.
    fn convert_dmabuf(&mut self, raw: &RawFrame) -> Result<()> {
        match self.try_convert_dmabuf(raw) {
            Ok(()) => {
                self.conversion = path::DMABUF_ZERO_COPY;
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!("DMA-BUF import failed: {msg}");
                if let Upload::Vaapi { dma, .. } = &mut self.upload {
                    *dma = None;
                }
                self.fallback_note = Some(format!("DMA-BUF off: {msg}"));
                Err(DmaBufError(msg).into())
            }
        }
    }

    #[cfg(unix)]
    fn try_convert_dmabuf(&mut self, raw: &RawFrame) -> Result<()> {
        let dmabuf = raw.dmabuf.as_ref().ok_or_else(|| anyhow!("frame has no DMA-BUF"))?;
        let device = match &self.upload {
            Upload::Vaapi { _device, .. } => _device.as_ptr(),
            Upload::Software => bail!("encoder {} cannot import DMA-BUF (needs VAAPI)", self.name),
        };
        let (out_w, out_h) = (self.config.width, self.config.height);
        let key = (raw.format, raw.width, raw.height);
        let rebuild = match &self.upload {
            Upload::Vaapi { dma: Some(d), .. } => d.key != key,
            _ => true,
        };
        if rebuild {
            let frames = hw::create_frames_sized(
                device,
                ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                raw.format.to_ffmpeg().into(),
                raw.width,
                raw.height,
                0,
            )
            .context("VAAPI frames context for DMA-BUF import")?;
            let graph = VaapiVpp::build_graph(
                device,
                VppInput::Vaapi(frames.as_ptr()),
                raw.width,
                raw.height,
                out_w,
                out_h,
            )?;
            tracing::info!(
                "DMA-BUF import: {:?} {}x{} modifier {:#x} → VAAPI VPP → nv12 {out_w}x{out_h}",
                raw.format,
                raw.width,
                raw.height,
                dmabuf.modifier
            );
            if let Upload::Vaapi { dma, .. } = &mut self.upload {
                *dma = Some(DmaImport { frames, key, graph });
            }
        }
        let Upload::Vaapi { dma: Some(import), .. } = &mut self.upload else {
            bail!("DMA-BUF import state missing");
        };

        let t_map = now_us();
        let mapped = map_dmabuf(import.frames.as_ptr(), raw, dmabuf)?;
        self.last_pack_us = now_us().saturating_sub(t_map);

        let t_vpp = now_us();
        let mut src = import.graph.get("in").ok_or_else(|| anyhow!("vpp missing in"))?;
        src.source().add(&mapped)?;
        drop(mapped);
        let mut sink = import.graph.get("out").ok_or_else(|| anyhow!("vpp missing out"))?;
        let mut out = ffmpeg_next::frame::Video::empty();
        sink.sink().frame(&mut out)?;
        if !is_vaapi_pixel(out.format()) {
            bail!("VPP returned system memory ({})", format_pixel(out.format()));
        }
        self.last_convert_us = now_us().saturating_sub(t_vpp);
        self.packed = out;
        Ok(())
    }

    #[cfg(not(unix))]
    fn try_convert_dmabuf(&mut self, _raw: &RawFrame) -> Result<()> {
        bail!("DMA-BUF is only supported on Linux")
    }

    fn transfer_to_vaapi_ptr(
        &mut self,
        frames: *mut ffi::AVBufferRef,
        pts: i64,
        kind: ffmpeg_next::picture::Type,
    ) -> Result<()> {
        unsafe {
            let mut hw_frame = ffmpeg_next::frame::Video::empty();
            let ret = ffi::av_hwframe_get_buffer(frames, hw_frame.as_mut_ptr(), 0);
            if ret < 0 {
                bail!("av_hwframe_get_buffer: {}", ffmpeg_next::Error::from(ret));
            }
            let src = if matches!(self.packed.format(), Pixel::NV12 | Pixel::YUV420P | Pixel::YUVJ420P)
            {
                self.packed.as_ptr()
            } else {
                self.staging.as_ptr()
            };
            let ret = ffi::av_hwframe_transfer_data(hw_frame.as_mut_ptr(), src, 0);
            if ret < 0 {
                bail!("av_hwframe_transfer_data: {}", ffmpeg_next::Error::from(ret));
            }
            hw_frame.set_pts(Some(pts));
            hw_frame.set_kind(kind);
            ffmpeg_of(&mut self.inner)?.send_frame(&hw_frame)?;
        }
        Ok(())
    }

    fn convert(&mut self, raw: &RawFrame) -> Result<()> {
        // After a DMA-BUF fallback the label still says zero-copy; shared
        // memory frames use VPP when the device has it.
        if self.conversion == path::DMABUF_ZERO_COPY {
            self.conversion = if self.vpp_failed() { path::SHM_CPU_UPLOAD } else { path::SHM_VPP };
        }
        if self.conversion == path::SHM_VPP {
            match self.convert_vaapi_vpp(raw) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(
                        "VAAPI VPP convert failed ({e:#}); falling back to CPU BGRx→NV12 for this session"
                    );
                    self.fallback_note = Some(format!("VPP off: {e:#}"));
                    self.conversion = path::SHM_CPU_UPLOAD;
                    if let Upload::Vaapi { vpp, .. } = &mut self.upload {
                        *vpp = None;
                    }
                }
            }
        }
        self.convert_cpu(raw)
    }

    fn vpp_failed(&self) -> bool {
        self.fallback_note.as_deref().is_some_and(|n| n.starts_with("VPP off"))
            || !matches!(self.upload, Upload::Vaapi { .. })
    }

    fn convert_vaapi_vpp(&mut self, raw: &RawFrame) -> Result<()> {
        let pix = raw.format.to_ffmpeg();
        let (out_w, out_h) = (self.config.width, self.config.height);
        let need_new = match &self.upload {
            Upload::Vaapi { vpp: Some(v), .. } => {
                v.src_format != pix || v.src_w != raw.width || v.src_h != raw.height
            }
            Upload::Vaapi { vpp: None, .. } => true,
            _ => return Err(anyhow!("no VAAPI device for VPP")),
        };
        if need_new {
            let device = match &self.upload {
                Upload::Vaapi { _device, .. } => _device.as_ptr(),
                _ => unreachable!(),
            };
            let built = VaapiVpp::build_from_ptr(device, pix, raw.width, raw.height, out_w, out_h)?;
            if let Upload::Vaapi { vpp, .. } = &mut self.upload {
                *vpp = Some(built);
            }
            tracing::info!(
                "VAAPI VPP: {} {}x{} → nv12 {out_w}x{out_h}",
                format_pixel(pix),
                raw.width,
                raw.height
            );
        }
        let t_pack = now_us();
        fill_packed(&mut self.packed, raw)?;
        self.last_pack_us = now_us().saturating_sub(t_pack);
        let vpp = match &mut self.upload {
            Upload::Vaapi { vpp: Some(v), .. } => v,
            _ => bail!("VPP graph missing"),
        };
        let t_vpp = now_us();
        let mut src = vpp.graph.get("in").ok_or_else(|| anyhow!("vpp missing in"))?;
        src.source().add(&self.packed)?;
        let mut sink = vpp
            .graph
            .get("out")
            .ok_or_else(|| anyhow!("vpp missing out"))?;
        let mut out = ffmpeg_next::frame::Video::empty();
        sink.sink().frame(&mut out)?;
        self.last_convert_us = now_us().saturating_sub(t_vpp);
        if !is_vaapi_pixel(out.format()) {
            tracing::warn!(
                "VAAPI VPP delivered system memory ({}); this is a GPU download, will re-upload",
                format_pixel(out.format())
            );
        }
        self.packed = out;
        Ok(())
    }

    fn convert_cpu(&mut self, raw: &RawFrame) -> Result<()> {
        let t0 = now_us();
        self.last_pack_us = 0;
        if is_vaapi_pixel(self.packed.format()) {
            // Drop a stale GPU surface from an earlier VPP/DMA-BUF frame so
            // `submit` uploads the new CPU output instead.
            self.packed = ffmpeg_next::frame::Video::empty();
        }
        let key = (raw.format, raw.width, raw.height);
        if self.scaler.as_ref().is_none_or(|s| s.src != key) {
            let same = raw.width == self.config.width && raw.height == self.config.height;
            let flags = if same {
                scaling::Flags::FAST_BILINEAR
                    | scaling::Flags::ACCURATE_RND
                    | scaling::Flags::FULL_CHR_H_INT
                    | scaling::Flags::FULL_CHR_H_INP
            } else {
                scaling::Flags::LANCZOS
                    | scaling::Flags::ACCURATE_RND
                    | scaling::Flags::FULL_CHR_H_INT
                    | scaling::Flags::FULL_CHR_H_INP
            };
            let ctx = scaling::Context::get(
                raw.format.to_ffmpeg(),
                raw.width,
                raw.height,
                self.staging.format(),
                self.config.width,
                self.config.height,
                flags,
            )?;
            if !same {
                tracing::info!(
                    "CPU scale {}x{} → {}x{} (LANCZOS); prefer omitting --size to keep the native capture size",
                    raw.width,
                    raw.height,
                    self.config.width,
                    self.config.height
                );
            }
            self.scaler = Some(Scaler { ctx, src: key });
        }
        let needed = raw.stride * raw.height as usize;
        if raw.data.len() < needed {
            bail!("raw frame too small: {} < {needed}", raw.data.len());
        }
        let scaler = self.scaler.as_mut().expect("scaler initialized");
        let src = [
            raw.data.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        ];
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
        self.last_convert_us = now_us().saturating_sub(t0);
        Ok(())
    }
}

fn fill_packed(dst: &mut ffmpeg_next::frame::Video, raw: &RawFrame) -> Result<()> {
    let pix = raw.format.to_ffmpeg();
    if dst.width() != raw.width || dst.height() != raw.height || dst.format() != pix {
        *dst = ffmpeg_next::frame::Video::new(pix, raw.width, raw.height);
    }
    let needed = raw.stride * raw.height as usize;
    if raw.data.len() < needed {
        bail!("raw frame too small: {} < {needed}", raw.data.len());
    }
    let dst_stride = dst.stride(0);
    let height = raw.height as usize;
    let row = raw.width as usize * 4;
    if dst_stride == raw.stride {
        dst.data_mut(0)[..needed].copy_from_slice(&raw.data[..needed]);
    } else {
        let plane = dst.data_mut(0);
        for y in 0..height {
            let s = y * raw.stride;
            let d = y * dst_stride;
            plane[d..d + row].copy_from_slice(&raw.data[s..s + row]);
        }
    }
    Ok(())
}

impl VaapiVpp {
    fn try_new(device: &HwRef, src: Pixel, w: u32, h: u32) -> Result<Self> {
        Self::build(device, src, w, h, w, h)
    }

    fn build(device: &HwRef, src: Pixel, src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Result<Self> {
        Self::build_from_ptr(device.as_ptr(), src, src_w, src_h, dst_w, dst_h)
    }

    fn build_from_ptr(
        device: *mut ffi::AVBufferRef,
        src: Pixel,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<Self> {
        let graph = Self::build_graph(device, VppInput::System(src), src_w, src_h, dst_w, dst_h)?;
        Ok(Self {
            graph,
            src_format: src,
            src_w,
            src_h,
        })
    }

    /// buffer → [hwupload] → scale_vaapi (to NV12, output size) → buffersink.
    /// System-memory input gets an hwupload; VAAPI input (mapped DMA-BUF)
    /// goes straight to VPP.
    fn build_graph(
        device: *mut ffi::AVBufferRef,
        input: VppInput,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<ffmpeg_next::filter::Graph> {
        let Some(buffer) = ffmpeg_next::filter::find("buffer") else {
            bail!("libavfilter buffer is not in this FFmpeg");
        };
        let Some(sink) = ffmpeg_next::filter::find("buffersink") else {
            bail!("libavfilter buffersink is not in this FFmpeg");
        };
        let vpp = ffmpeg_next::filter::find("scale_vaapi")
            .or_else(|| ffmpeg_next::filter::find("vpp_vaapi"))
            .ok_or_else(|| anyhow!("scale_vaapi/vpp_vaapi is not in this FFmpeg"))?;
        let vpp_name = vpp.name().to_owned();

        let mut graph = ffmpeg_next::filter::Graph::new();
        let vpp_args = format!("w={dst_w}:h={dst_h}:format=nv12");
        let mut in_ctx = match input {
            VppInput::System(src) => {
                // Pixel formats by number: FFmpeg names (bgr0) differ from
                // the Rust enum names (BGRZ).
                let buf_args = format!(
                    "video_size={src_w}x{src_h}:pix_fmt={}:time_base=1/1000000:pixel_aspect=1/1",
                    ffi::AVPixelFormat::from(src) as i32
                );
                graph.add(&buffer, "in", &buf_args)?
            }
            VppInput::Vaapi(frames) => alloc_vaapi_buffersrc(&mut graph, &buffer, src_w, src_h, frames)?,
        };
        let mut vpp_ctx = alloc_hw_filter(&mut graph, &vpp, "vpp", &vpp_args, device)?;
        let mut out_ctx = graph.add(&sink, "out", "")?;
        // Keep the result on a VAAPI surface so we do not download and
        // re-upload through system memory.
        unsafe {
            let fmts = [
                ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                ffi::AVPixelFormat::AV_PIX_FMT_NONE,
            ];
            let bytes = std::slice::from_raw_parts(
                fmts.as_ptr() as *const u8,
                std::mem::size_of_val(&fmts),
            );
            let ret = ffi::av_opt_set_bin(
                out_ctx.as_mut_ptr().cast(),
                c"pix_fmts".as_ptr(),
                bytes.as_ptr(),
                (std::mem::size_of::<ffi::AVPixelFormat>() * (fmts.len() - 1)) as i32,
                ffi::AV_OPT_SEARCH_CHILDREN,
            );
            if ret < 0 {
                tracing::warn!("could not pin buffersink to VAAPI ({})", ffmpeg_next::Error::from(ret));
            }
        }
        if matches!(input, VppInput::System(_)) {
            let Some(hwupload) = ffmpeg_next::filter::find("hwupload") else {
                bail!("hwupload filter is not in this FFmpeg");
            };
            let mut up_ctx = alloc_hw_filter(&mut graph, &hwupload, "upload", "", device)?;
            in_ctx.link(0, &mut up_ctx, 0);
            up_ctx.link(0, &mut vpp_ctx, 0);
        } else {
            in_ctx.link(0, &mut vpp_ctx, 0);
        }
        vpp_ctx.link(0, &mut out_ctx, 0);
        graph
            .validate()
            .with_context(|| format!("configuring {vpp_name} graph"))?;
        Ok(graph)
    }
}

#[derive(Clone, Copy)]
enum VppInput {
    System(Pixel),
    Vaapi(*mut ffi::AVBufferRef),
}

/// A buffer source that takes VAAPI frames from `frames`.
fn alloc_vaapi_buffersrc(
    graph: &mut ffmpeg_next::filter::Graph,
    buffer: &ffmpeg_next::filter::Filter,
    w: u32,
    h: u32,
    frames: *mut ffi::AVBufferRef,
) -> Result<ffmpeg_next::filter::Context> {
    unsafe {
        let ctx = ffi::avfilter_graph_alloc_filter(graph.as_mut_ptr(), buffer.as_ptr(), c"in".as_ptr());
        if ctx.is_null() {
            bail!("allocating buffer source");
        }
        let par = ffi::av_buffersrc_parameters_alloc();
        if par.is_null() {
            bail!("av_buffersrc_parameters_alloc failed");
        }
        (*par).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        (*par).width = w as i32;
        (*par).height = h as i32;
        (*par).time_base = ffi::AVRational { num: 1, den: 1_000_000 };
        (*par).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
        (*par).hw_frames_ctx = frames; // av_buffersrc_parameters_set takes its own reference
        let ret = ffi::av_buffersrc_parameters_set(ctx, par);
        ffi::av_free(par.cast());
        if ret < 0 {
            bail!("buffer source parameters: {}", ffmpeg_next::Error::from(ret));
        }
        let ret = ffi::avfilter_init_str(ctx, std::ptr::null());
        if ret < 0 {
            bail!("init buffer source: {}", ffmpeg_next::Error::from(ret));
        }
        Ok(ffmpeg_next::filter::Context::wrap(ctx))
    }
}

/// Keeps our duplicate of the DMA-BUF fd alive for as long as FFmpeg holds
/// the DRM-PRIME frame (until the mapped VAAPI surface is released).
#[cfg(unix)]
struct DrmHolder {
    desc: ffi::AVDRMFrameDescriptor,
    _fd: std::os::fd::OwnedFd,
}

#[cfg(unix)]
unsafe extern "C" fn free_drm_holder(opaque: *mut std::ffi::c_void, _data: *mut u8) {
    if !opaque.is_null() {
        drop(Box::from_raw(opaque as *mut DrmHolder));
    }
}

/// Wrap a DMA-BUF as an `AV_PIX_FMT_DRM_PRIME` frame and map it into a VAAPI
/// surface from `frames` (`av_hwframe_map`, no copy). Needs FFmpeg built with
/// libdrm; otherwise this fails with "Function not implemented".
#[cfg(unix)]
fn map_dmabuf(
    frames: *mut ffi::AVBufferRef,
    raw: &RawFrame,
    dmabuf: &crate::frame::DmaBufFrame,
) -> Result<ffmpeg_next::frame::Video> {
    use std::os::fd::AsRawFd;
    let fd = dmabuf.fd.try_clone().context("duplicating DMA-BUF fd")?;
    let size = if dmabuf.size > 0 {
        dmabuf.size
    } else {
        (dmabuf.offset as usize).saturating_add((dmabuf.pitch as usize).saturating_mul(raw.height as usize))
    };
    // SAFETY: AVDRMFrameDescriptor is plain old data; all-zero is valid.
    let mut desc: ffi::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
    desc.nb_objects = 1;
    desc.objects[0].fd = fd.as_raw_fd();
    desc.objects[0].size = size;
    desc.objects[0].format_modifier = dmabuf.modifier;
    desc.nb_layers = 1;
    desc.layers[0].format = raw.format.drm_fourcc();
    desc.layers[0].nb_planes = 1;
    desc.layers[0].planes[0].object_index = 0;
    desc.layers[0].planes[0].offset = dmabuf.offset as isize;
    desc.layers[0].planes[0].pitch = dmabuf.pitch as isize;

    let holder = Box::into_raw(Box::new(DrmHolder { desc, _fd: fd }));
    unsafe {
        let buf = ffi::av_buffer_create(
            std::ptr::addr_of_mut!((*holder).desc).cast(),
            std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
            Some(free_drm_holder),
            holder.cast(),
            0,
        );
        if buf.is_null() {
            drop(Box::from_raw(holder));
            bail!("av_buffer_create failed");
        }
        let mut drm = ffmpeg_next::frame::Video::empty();
        let f = drm.as_mut_ptr();
        (*f).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*f).width = raw.width as i32;
        (*f).height = raw.height as i32;
        (*f).buf[0] = buf; // the frame now owns the holder
        (*f).data[0] = std::ptr::addr_of_mut!((*holder).desc).cast();

        let mut mapped = ffmpeg_next::frame::Video::empty();
        let m = mapped.as_mut_ptr();
        (*m).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        (*m).hw_frames_ctx = ffi::av_buffer_ref(frames);
        let ret = ffi::av_hwframe_map(m, f, ffi::AV_HWFRAME_MAP_READ as i32);
        if ret < 0 {
            bail!(
                "VAAPI could not import the DMA-BUF (modifier {:#x}): {}",
                dmabuf.modifier,
                ffmpeg_next::Error::from(ret)
            );
        }
        Ok(mapped)
    }
}

fn alloc_hw_filter(
    graph: &mut ffmpeg_next::filter::Graph,
    filter: &ffmpeg_next::filter::Filter,
    name: &str,
    args: &str,
    device: *mut ffi::AVBufferRef,
) -> Result<ffmpeg_next::filter::Context> {
    use std::ffi::CString;
    let cname = CString::new(name)?;
    let cargs = if args.is_empty() {
        None
    } else {
        Some(CString::new(args)?)
    };
    unsafe {
        let ctx = ffi::avfilter_graph_alloc_filter(
            graph.as_mut_ptr(),
            filter.as_ptr(),
            cname.as_ptr(),
        );
        if ctx.is_null() {
            bail!("allocating {name} filter");
        }
        (*ctx).hw_device_ctx = ffi::av_buffer_ref(device);
        let ret = ffi::avfilter_init_str(
            ctx,
            cargs.as_ref().map(|s| s.as_ptr()).unwrap_or(std::ptr::null()),
        );
        if ret < 0 {
            bail!("init {name}: {}", ffmpeg_next::Error::from(ret));
        }
        Ok(ffmpeg_next::filter::Context::wrap(ctx))
    }
}

/// FFmpeg's own name for a pixel format ("bgr0", not the Rust enum's "BGRZ").
fn format_pixel(p: Pixel) -> String {
    let name = unsafe { ffi::av_get_pix_fmt_name(p.into()) };
    if name.is_null() {
        return format!("{p:?}").to_ascii_lowercase();
    }
    unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned()
}

fn ffmpeg_of(inner: &mut Core) -> Result<&mut encoder::Video> {
    match inner {
        Core::Ffmpeg(e) => Ok(e),
        #[cfg(feature = "filmcraft")]
        Core::Filmcraft(_) => bail!("not an FFmpeg encoder"),
    }
}

fn is_vaapi_pixel(p: Pixel) -> bool {
    matches!(p, Pixel::VAAPI)
}

fn effective_rc(backend: EncoderBackend, rc: RateControl) -> RateControl {
    match (backend, rc) {
        (_, RateControl::Auto) => RateControl::Cbr,
        (EncoderBackend::X264, RateControl::Icq) => RateControl::Cqp,
        (EncoderBackend::VideoToolbox, RateControl::Icq) => RateControl::Cqp,
        (_, other) => other,
    }
}

fn apply_rate_control(
    enc: &mut encoder::video::Video,
    backend: EncoderBackend,
    rc: RateControl,
    bitrate: i64,
    qp: u32,
) -> Result<()> {
    let bitrate = bitrate.max(100_000);
    let maxrate = match rc {
        RateControl::Vbr => bitrate + bitrate / 4,
        _ => bitrate,
    };
    let buffer = (bitrate * 2).min(i32::MAX as i64) as i32;
    match rc {
        RateControl::Cqp | RateControl::Icq => {
            enc.set_bit_rate(0);
            enc.set_max_bit_rate(0);
            unsafe {
                let raw = enc.as_mut_ptr();
                (*raw).qmin = qp as i32;
                (*raw).qmax = qp as i32;
                (*raw).rc_buffer_size = 0;
            }
        }
        _ => {
            enc.set_bit_rate(bitrate as usize);
            enc.set_max_bit_rate(maxrate as usize);
            unsafe {
                let raw = enc.as_mut_ptr();
                (*raw).rc_buffer_size = buffer;
                (*raw).rc_initial_buffer_occupancy = buffer * 3 / 4;
            }
        }
    }
    let _ = backend;
    Ok(())
}

fn apply_vaapi_opts(opts: &mut Dictionary, rc: RateControl, bitrate: i64, config: &EncoderConfig) {
    let mode = match rc {
        RateControl::Cbr => "CBR",
        RateControl::Vbr => "VBR",
        RateControl::Cqp => "CQP",
        RateControl::Icq => "ICQ",
        RateControl::Auto => "CBR",
    };
    opts.set("rc_mode", mode);
    opts.set("async_depth", "1");
    opts.set("profile", "high");
    opts.set("low_power", "0");
    if config.intra_refresh {
        opts.set("idr_interval", "0");
        opts.set("int_ref_type", "1");
    }
    match rc {
        RateControl::Cqp => {
            opts.set("qp", &config.qp.to_string());
        }
        RateControl::Icq => {
            opts.set("quality", &config.qp.to_string());
        }
        RateControl::Vbr => {
            opts.set("maxrate", &bitrate.to_string());
            opts.set("b", &bitrate.to_string());
        }
        _ => {
            opts.set("b", &bitrate.to_string());
            opts.set("maxrate", &bitrate.to_string());
        }
    }
}

fn format_vt_quality(qp: u32) -> String {
    // VideoToolbox quality is 0..1; map QP 15..40 to ~0.85..0.4.
    let q = (52.0 - qp as f64) / 52.0;
    format!("{:.3}", q.clamp(0.2, 1.0))
}

fn describe_rc(
    enc: &encoder::Video,
    name: &str,
    rc: RateControl,
    config: &EncoderConfig,
    conversion: &str,
) -> String {
    let (bit_rate, max_rate, buf, gop, profile, qmin, qmax) = unsafe {
        let raw = enc.as_ptr();
        (
            (*raw).bit_rate,
            (*raw).rc_max_rate,
            (*raw).rc_buffer_size,
            (*raw).gop_size,
            (*raw).profile,
            (*raw).qmin,
            (*raw).qmax,
        )
    };
    let profile = match profile {
        x if x == ffi::FF_PROFILE_H264_HIGH => "high".to_owned(),
        x if x == ffi::FF_PROFILE_H264_MAIN => "main".to_owned(),
        x if x == ffi::FF_PROFILE_H264_BASELINE => "baseline".to_owned(),
        x => format!("{x}"),
    };
    format!(
        "encoder {name} {}x{}  rc={} ({})  bitrate={:.2}/{:.2} Mbit/s  vbv={:.2} Mbit  qp={}/qmin={qmin}/qmax={qmax}  profile={profile}  gop={gop}  bframes=0  convert={conversion}",
        config.width,
        config.height,
        rc.as_str(),
        if matches!(rc, RateControl::Cqp | RateControl::Icq) {
            "quality"
        } else {
            "bitrate"
        },
        bit_rate as f64 / 1e6,
        max_rate as f64 / 1e6,
        buf as f64 / 1e6,
        config.qp,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_x_and_times() {
        assert_eq!(parse_size("1920x1080").unwrap(), (1920, 1080));
        assert_eq!(parse_size("1920×1080").unwrap(), (1920, 1080));
    }

    #[test]
    fn resolved_size_defaults_to_capture() {
        let mut c = EncoderConfig::default();
        assert_eq!(c.resolved_size(1920, 1080), (1920, 1080));
        c.width = 1280;
        c.height = 720;
        assert_eq!(c.resolved_size(1920, 1080), (1280, 720));
    }

    #[test]
    fn ffmpeg_pixel_names_are_ffmpeg_names() {
        // The VPP buffer source once got "bgrz" (Rust enum name), which FFmpeg
        // rejects, so every BGRx capture fell back to the CPU path.
        assert_eq!(format_pixel(Pixel::BGRZ), "bgr0");
        assert_eq!(format_pixel(Pixel::BGRA), "bgra");
        assert_eq!(format_pixel(Pixel::RGBZ), "rgb0");
    }

    #[test]
    fn default_bitrate_is_forty_mbps() {
        assert_eq!(DEFAULT_BITRATE_KBPS, 40_000);
        assert_eq!(EncoderConfig::default().bitrate_kbps, 40_000);
        assert_eq!(EncoderConfig::default().width, 0);
    }
}
