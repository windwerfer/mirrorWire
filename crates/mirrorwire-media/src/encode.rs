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
}

impl std::str::FromStr for EncoderBackend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "vaapi" => Self::Vaapi,
            "x264" | "software" => Self::X264,
            "videotoolbox" => Self::VideoToolbox,
            _ => bail!("unknown encoder backend {s:?} (auto, vaapi, x264, videotoolbox)"),
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
        }
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
    },
}

struct VaapiVpp {
    graph: ffmpeg_next::filter::Graph,
    src_format: Pixel,
    src_w: u32,
    src_h: u32,
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
pub struct Encoder {
    inner: encoder::Video,
    upload: Upload,
    scaler: Option<Scaler>,
    staging: ffmpeg_next::frame::Video,
    packed: ffmpeg_next::frame::Video,
    config: EncoderConfig,
    name: &'static str,
    conversion: &'static str,
    rc_summary: String,
    start_us: Option<u64>,
    last_convert_us: u64,
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
            EncoderBackend::Auto => unreachable!(),
        };

        let inner = enc
            .open_as_with(codec, opts)
            .with_context(|| format!("opening {codec_name}"))?;

        let mut conversion = "cpu";
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
                    conversion = "vaapi-vpp";
                }
                Err(e) => {
                    tracing::info!(
                        "VAAPI VPP (hwupload/scale_vaapi) unavailable ({e:#}); using CPU BGRx→NV12"
                    );
                    conversion = "cpu+vaapi-upload";
                }
            }
        }

        // Attach a real VPP graph only when the path is enabled; the probe
        // graph above was dropped. The first convert() builds one for the
        // source size.
        if conversion == "vaapi-vpp" {
            if let Upload::Vaapi { vpp, _device, .. } = &mut upload {
                *vpp = None; // built lazily for the actual capture size/format
                let _ = _device;
            }
        }

        let rc_summary = describe_rc(&inner, codec_name, rc, config, conversion);
        tracing::info!("{rc_summary}");

        Ok(Self {
            inner,
            upload,
            scaler: None,
            staging: ffmpeg_next::frame::Video::new(sw_format, config.width, config.height),
            packed: ffmpeg_next::frame::Video::empty(),
            config: config.clone(),
            name: codec_name,
            conversion,
            rc_summary,
            start_us: None,
            last_convert_us: 0,
            last_encode_us: 0,
        })
    }

    pub fn encode(&mut self, raw: &RawFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let t0 = now_us();
        self.convert(raw)?;
        let t1 = now_us();
        let start = *self.start_us.get_or_insert(raw.capture_us);
        let pts = raw.capture_us.saturating_sub(start) as i64;
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
            Upload::Vaapi { frames, vpp, .. } => {
                let frames_ptr = frames.as_ptr();
                let have_vpp = vpp.is_some() && is_vaapi_pixel(self.packed.format());
                if have_vpp {
                    self.packed.set_pts(Some(pts));
                    self.packed.set_kind(kind);
                    self.inner.send_frame(&self.packed)?;
                } else {
                    self.transfer_to_vaapi_ptr(frames_ptr, pts, kind)?;
                }
            }
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
        let t2 = now_us();
        self.last_convert_us = t1.saturating_sub(t0);
        self.last_encode_us = t2.saturating_sub(t1);
        Ok(out)
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
            let ret = ffi::av_hwframe_transfer_data(hw_frame.as_mut_ptr(), self.staging.as_ptr(), 0);
            if ret < 0 {
                bail!("av_hwframe_transfer_data: {}", ffmpeg_next::Error::from(ret));
            }
            hw_frame.set_pts(Some(pts));
            hw_frame.set_kind(kind);
            self.inner.send_frame(&hw_frame)?;
        }
        Ok(())
    }

    fn convert(&mut self, raw: &RawFrame) -> Result<()> {
        if self.conversion == "vaapi-vpp" {
            match self.convert_vaapi_vpp(raw) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(
                        "VAAPI VPP convert failed ({e:#}); falling back to CPU BGRx→NV12 for this session"
                    );
                    self.conversion = "cpu+vaapi-upload";
                    if let Upload::Vaapi { vpp, .. } = &mut self.upload {
                        *vpp = None;
                    }
                }
            }
        }
        self.convert_cpu(raw)
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
        fill_packed(&mut self.packed, raw)?;
        let vpp = match &mut self.upload {
            Upload::Vaapi { vpp: Some(v), .. } => v,
            _ => bail!("VPP graph missing"),
        };
        let mut src = vpp.graph.get("in").ok_or_else(|| anyhow!("vpp missing in"))?;
        src.source().add(&self.packed)?;
        let mut sink = vpp
            .graph
            .get("out")
            .ok_or_else(|| anyhow!("vpp missing out"))?;
        sink.sink().frame(&mut self.packed)?;
        Ok(())
    }

    fn convert_cpu(&mut self, raw: &RawFrame) -> Result<()> {
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
        let Some(buffer) = ffmpeg_next::filter::find("buffer") else {
            bail!("libavfilter buffer is not in this FFmpeg");
        };
        let Some(sink) = ffmpeg_next::filter::find("buffersink") else {
            bail!("libavfilter buffersink is not in this FFmpeg");
        };
        let Some(hwupload) = ffmpeg_next::filter::find("hwupload") else {
            bail!("hwupload filter is not in this FFmpeg");
        };
        let vpp = ffmpeg_next::filter::find("scale_vaapi")
            .or_else(|| ffmpeg_next::filter::find("vpp_vaapi"))
            .ok_or_else(|| anyhow!("scale_vaapi/vpp_vaapi is not in this FFmpeg"))?;
        let vpp_name = vpp.name().to_owned();

        let mut graph = ffmpeg_next::filter::Graph::new();
        let buf_args = format!(
            "video_size={src_w}x{src_h}:pix_fmt={}:time_base=1/1000000:pixel_aspect=1/1",
            format_pixel(src)
        );
        let vpp_args = format!("w={dst_w}:h={dst_h}:format=nv12");
        let mut in_ctx = graph.add(&buffer, "in", &buf_args)?;
        let mut up_ctx = alloc_hw_filter(&mut graph, &hwupload, "upload", "", device)?;
        let mut vpp_ctx = alloc_hw_filter(&mut graph, &vpp, "vpp", &vpp_args, device)?;
        let mut out_ctx = graph.add(&sink, "out", "")?;
        in_ctx.link(0, &mut up_ctx, 0);
        up_ctx.link(0, &mut vpp_ctx, 0);
        vpp_ctx.link(0, &mut out_ctx, 0);
        graph
            .validate()
            .with_context(|| format!("configuring {vpp_name} graph"))?;
        Ok(Self {
            graph,
            src_format: src,
            src_w,
            src_h,
        })
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

fn format_pixel(p: Pixel) -> String {

    format!("{p:?}").to_ascii_lowercase()
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
    fn default_bitrate_is_forty_mbps() {
        assert_eq!(DEFAULT_BITRATE_KBPS, 40_000);
        assert_eq!(EncoderConfig::default().bitrate_kbps, 40_000);
        assert_eq!(EncoderConfig::default().width, 0);
    }
}
