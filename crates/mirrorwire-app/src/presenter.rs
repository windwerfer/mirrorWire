use anyhow::{bail, Result};
use clap::{Args, ValueEnum};
use mirrorwire_media::encode::{
    parse_size, Encoder, EncoderBackend, EncoderConfig, RateControl, DEFAULT_BITRATE_KBPS,
    DEFAULT_QP,
};
use mirrorwire_media::frame::EncodedFrame;
use crate::capture_prefs::{parse_fps_arg, CapturePrefs, CursorPref, FpsPref};
use mirrorwire_media::pipeline::{
    continue_capture_encode, open_encoder_for, wait_first_frame, CaptureExit, Controls, Tick,
    VideoSink,
};
use mirrorwire_media::source::testpattern::TestPattern;
use mirrorwire_media::source::{CursorCapture, CursorSink, FrameSource};
use mirrorwire_proto::cursor::CursorEvent;
use mirrorwire_net::diag::FirewallHint;
use mirrorwire_net::diag::IncomingAttempt;
use mirrorwire_net::discovery::Advertisement;
use mirrorwire_net::identity::{short_fingerprint, Identity};
use mirrorwire_net::pairing::{
    lan_ipv4s, primary_ipv4, qr_terminal, random_pairing_code, ConnectInfo,
};
use mirrorwire_net::transport::{FecControl, FecState, OutgoingFrame, Presenter, PresenterConfig};
use mirrorwire_proto::fec::FecMode;
use mirrorwire_proto::control::StreamInfo;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
#[cfg(feature = "gui")]
use std::thread::JoinHandle;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, ValueEnum)]
pub enum SourceKind {
    /// Screen capture through xdg-desktop-portal + PipeWire.
    Portal,
    /// Synthetic moving pattern with an embedded frame counter.
    Test,
}

#[derive(Args)]
pub struct PresentArgs {
    #[arg(long, value_enum, default_value_t = SourceKind::Portal)]
    source: SourceKind,
    /// Output width. 0 (default) follows the native capture size.
    #[arg(long, default_value_t = 0)]
    width: u32,
    /// Output height. 0 (default) follows the native capture size.
    #[arg(long, default_value_t = 0)]
    height: u32,
    /// Output size as WIDTHxHEIGHT. Overrides --width/--height. Default: native capture size.
    #[arg(long, value_name = "WxH")]
    size: Option<String>,
    /// Highest frame rate to ask the compositor for: 30, 60 (default), another
    /// number, or max (the display's refresh rate). Also MIRRORWIRE_FPS or the
    /// saved setting.
    #[arg(long, value_parser = parse_fps_arg, value_name = "30|60|max")]
    pub fps: Option<FpsPref>,
    /// How the mouse cursor is sent: separate (as coordinates, drawn by the
    /// viewer; default), embedded (drawn into the video) or hidden. Also
    /// MIRRORWIRE_CURSOR or the saved setting. separate falls back to embedded
    /// when the portal or the viewer can't do it.
    #[arg(long, value_enum)]
    pub cursor: Option<CursorPref>,
    /// Forward error correction for video packets: auto (default; parity
    /// adapts to the loss the viewer reports), off, or a fixed percentage of
    /// parity packets such as 20. Also MIRRORWIRE_FEC or the saved setting.
    /// Older viewers always get plain packets.
    #[arg(long, value_parser = crate::capture_prefs::parse_fec_arg, value_name = "auto|off|PERCENT")]
    pub fec: Option<FecMode>,
    /// Adaptive bitrate (default on): the bitrate setting is the maximum;
    /// the presenter lowers it when the viewer reports loss or the RTT grows
    /// and raises it again slowly. FEC parity is counted inside it. Also
    /// MIRRORWIRE_ADAPTIVE_BITRATE or the saved setting.
    #[arg(long, value_parser = crate::capture_prefs::parse_on_off_arg, value_name = "on|off")]
    pub adaptive_bitrate: Option<bool>,
    /// Resolved from --fps / --cursor / --fec / env / settings.json in `main`.
    #[arg(skip)]
    pub capture: CapturePrefs,
    /// Target bitrate in kbit/s (default: 40000 = 40 Mbit/s).
    #[arg(long, default_value_t = DEFAULT_BITRATE_KBPS)]
    bitrate_kbps: u32,
    /// Target bitrate in Mbit/s; overrides --bitrate-kbps when set.
    #[arg(long)]
    bitrate_mbps: Option<f64>,
    /// auto, vaapi, x264, videotoolbox, filmcraft (pure-Rust; only in builds with the
    /// `filmcraft` feature, never chosen by auto).
    #[arg(long, default_value = "auto")]
    encoder: EncoderBackend,
    /// Rate control: auto, cbr, vbr, cqp, icq. auto is CBR at the target bitrate.
    #[arg(long, default_value = "auto")]
    rc: RateControl,
    /// QP / ICQ quality when --rc cqp or --rc icq (lower is sharper; default 18).
    #[arg(long, default_value_t = DEFAULT_QP)]
    qp: u32,
    /// Keyframe interval in frames. 0 (default) is 10 seconds at --fps.
    #[arg(long, default_value_t = 0)]
    keyint: u32,
    #[arg(long, default_value = "/dev/dri/renderD128")]
    vaapi_device: String,
    /// Spread intra coding over frames instead of sending large keyframes.
    #[arg(long)]
    intra_refresh: bool,
    /// Set from the global `--port` / `MIRRORWIRE_PORT` / saved settings in `main`.
    #[arg(skip)]
    pub port: u16,
    /// Address to bind (default: all interfaces).
    #[arg(long)]
    bind: Option<IpAddr>,
    /// Address to put in the QR code (default: first non-loopback IPv4).
    #[arg(long)]
    advertise_ip: Option<IpAddr>,
    #[arg(long)]
    name: Option<String>,
    /// Accept any viewer without a pairing code.
    #[arg(long)]
    no_pairing: bool,
    #[arg(long)]
    no_mdns: bool,
    /// Write the mirrorwire:// URI to this file (for scripts and tests).
    #[arg(long)]
    uri_file: Option<PathBuf>,
    /// Stop after this many frames (test source only).
    #[arg(long)]
    frames: Option<u64>,
    /// Hide the mouse cursor in the capture (same as --cursor hidden).
    #[arg(long)]
    pub hide_cursor: bool,
    /// Don't offer DMA-BUF (zero-copy GPU buffers) to the compositor; always
    /// capture through shared memory. For debugging driver problems.
    #[arg(long)]
    no_dmabuf: bool,
}

impl PresentArgs {
    pub fn encoder_config(&self) -> Result<EncoderConfig> {
        let (width, height) = if let Some(s) = &self.size {
            parse_size(s)?
        } else {
            (self.width, self.height)
        };
        let bitrate_kbps = if let Some(mbps) = self.bitrate_mbps {
            (mbps * 1000.0).round().max(1.0) as u32
        } else {
            self.bitrate_kbps
        };
        Ok(EncoderConfig {
            width,
            height,
            // Corrected to the negotiated rate once the capture is running.
            fps: self.capture.fps.request().min(60),
            bitrate_kbps,
            backend: self.encoder,
            rate_control: self.rc,
            qp: self.qp,
            keyint: (self.keyint > 0).then_some(self.keyint),
            intra_refresh: self.intra_refresh,
            vaapi_device: self.vaapi_device.clone(),
        })
    }

    #[cfg(feature = "gui")]
    pub fn gui_defaults() -> Self {
        Self {
            source: SourceKind::Portal,
            width: 0,
            height: 0,
            size: None,
            fps: None,
            cursor: None,
            fec: None,
            adaptive_bitrate: None,
            capture: CapturePrefs::default(),
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            bitrate_mbps: None,
            encoder: EncoderBackend::Auto,
            rc: RateControl::Auto,
            qp: DEFAULT_QP,
            keyint: 0,
            vaapi_device: "/dev/dri/renderD128".into(),
            intra_refresh: false,
            port: mirrorwire_proto::DEFAULT_PORT,
            bind: None,
            advertise_ip: None,
            name: None,
            no_pairing: false,
            no_mdns: false,
            uri_file: None,
            frames: None,
            hide_cursor: false,
            no_dmabuf: false,
        }
    }
}

#[cfg(feature = "gui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentPhase {
    Starting,
    WaitingForCapture,
    Streaming,
    Stopped,
    Failed,
}

#[cfg(feature = "gui")]
#[derive(Debug, Clone)]
pub struct EncodeHud {
    /// Encoder actually running (e.g. "h264_vaapi", "filmcraft-h264enc").
    pub encoder: String,
    /// Mean encode time over the last ~1 s window (steadier than `encode_ms`).
    pub encode_avg_ms: f64,
    pub pack_ms: f64,
    pub convert_ms: f64,
    pub upload_ms: f64,
    pub encode_ms: f64,
    pub path: String,
    pub capture_w: u32,
    pub capture_h: u32,
    pub source_fps: f64,
    /// Re-sent frames per second while the screen was static.
    pub repeat_fps: f64,
    pub fallback_note: Option<String>,
}

#[cfg(feature = "gui")]
impl Default for EncodeHud {
    fn default() -> Self {
        Self {
            encoder: String::new(),
            encode_avg_ms: 0.0,
            pack_ms: 0.0,
            convert_ms: 0.0,
            upload_ms: 0.0,
            encode_ms: 0.0,
            path: String::new(),
            capture_w: 0,
            capture_h: 0,
            source_fps: 0.0,
            repeat_fps: 0.0,
            fallback_note: None,
        }
    }
}

#[cfg(feature = "gui")]
#[derive(Debug, Clone)]
pub struct PresentStatus {
    pub phase: PresentPhase,
    pub name: String,
    pub pairing_code: Option<String>,
    pub uri: Option<String>,
    pub fingerprint: Option<String>,
    pub encoder: Option<String>,
    pub source: Option<String>,
    pub listen_addr: Option<String>,
    pub advertised: Vec<String>,
    pub port: u16,
    pub viewer_connected: bool,
    pub error: Option<String>,
    pub rc_summary: Option<String>,
    pub conversion: Option<String>,
    pub firewall: Option<FirewallHint>,
    pub attempts: Vec<IncomingAttempt>,
    pub hud: EncodeHud,
    pub settings: EncoderConfig,
    pub capture: CaptureStatus,
}

#[cfg(feature = "gui")]
impl PresentStatus {
    fn new(name: String, settings: EncoderConfig, port: u16) -> Self {
        Self {
            phase: PresentPhase::Starting,
            name,
            pairing_code: None,
            uri: None,
            fingerprint: None,
            encoder: None,
            source: None,
            listen_addr: None,
            advertised: Vec::new(),
            port,
            viewer_connected: false,
            error: None,
            rc_summary: None,
            conversion: None,
            firewall: None,
            attempts: Vec::new(),
            hud: EncodeHud::default(),
            settings,
            capture: CaptureStatus::default(),
        }
    }
}

#[cfg(feature = "gui")]
pub struct PresenterHandle {
    status: Arc<Mutex<PresentStatus>>,
    connected: Arc<Mutex<Option<watch::Receiver<bool>>>>,
    controls: Controls,
    settings: Arc<Mutex<EncoderConfig>>,
    attempts: Arc<Mutex<Vec<IncomingAttempt>>>,
    hud: Arc<Mutex<EncodeHudShim>>,
    capture: Arc<Mutex<CaptureCtl>>,
    config_dir: PathBuf,
    thread: Option<JoinHandle<()>>,
}

#[cfg(feature = "gui")]
impl PresenterHandle {
    pub fn stop(&self) {
        self.controls.stop();
    }

    pub fn apply_settings(&self, cfg: EncoderConfig) {
        *self.settings.lock().expect("encoder settings") = cfg;
        self.controls.request_keyframe();
    }

    /// Change the cursor mode or frame-rate cap: saved for the next start,
    /// and the capture is reopened (silently, with the portal restore token)
    /// when it changes what is asked of the compositor.
    pub fn apply_capture(&self, prefs: CapturePrefs) {
        if let Err(e) = mirrorwire_net::settings::save_capture(
            &self.config_dir,
            prefs.cursor.as_str(),
            &prefs.fps.label(),
            &prefs.fec.as_string(),
            prefs.adaptive,
        ) {
            tracing::warn!("could not save capture settings: {e:#}");
        }
        let restart = {
            let mut c = self.capture.lock().expect("capture ctl");
            if c.prefs.fec != prefs.fec {
                tracing::info!("FEC set to {}", prefs.fec.as_string());
            }
            if c.prefs.adaptive != prefs.adaptive {
                tracing::info!("adaptive bitrate {}", if prefs.adaptive { "on" } else { "off" });
            }
            // FEC and adaptive bitrate apply to the next frame; no restart needed.
            c.fec.set_mode(prefs.fec);
            c.fec.set_adaptive(prefs.adaptive);
            c.prefs = prefs;
            c.needs_restart()
        };
        if restart {
            tracing::info!("capture settings changed (cursor {}, fps {}); reopening the capture", prefs.cursor.as_str(), prefs.fps.label());
            self.controls.request_source_restart();
        }
    }

    pub fn refresh_firewall(&self) {
        let port = self.status.lock().expect("present status").port;
        let hint = FirewallHint::for_port(port);
        self.status.lock().expect("present status").firewall = Some(hint);
    }

    pub fn poll(&self) -> PresentStatus {
        let mut s = self.status.lock().expect("present status").clone();
        if let Some(rx) = self.connected.lock().expect("connected watch").as_ref() {
            s.viewer_connected = *rx.borrow();
        }
        s.attempts = self.attempts.lock().expect("attempts").clone();
        s.hud = self.hud.lock().expect("hud").to_hud();
        s.settings = self.settings.lock().expect("encoder settings").clone();
        let c = self.capture.lock().expect("capture ctl");
        s.capture = c.status();
        if !c.source_desc.is_empty() {
            s.source = Some(c.source_desc.clone());
        }
        s
    }
}

#[cfg(feature = "gui")]
impl Drop for PresenterHandle {
    fn drop(&mut self) {
        self.controls.stop();
        let _ = self.thread.take();
    }
}

struct QuicSink {
    frames: mpsc::Sender<OutgoingFrame>,
    connected: watch::Receiver<bool>,
    controls: Controls,
    next_id: u32,
}

impl VideoSink for QuicSink {
    fn send(&mut self, frame: EncodedFrame) -> Result<()> {
        let frame_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let out = OutgoingFrame {
            frame_id,
            keyframe: frame.keyframe,
            data: frame.data,
            capture_us: frame.capture_us,
            encoded_us: frame.encoded_us,
        };
        match self.frames.try_send(out) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!("network backlog, dropped frame {frame_id}");
                self.controls.request_keyframe();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("network task stopped"),
        }
    }

    fn wants_frames(&self) -> bool {
        *self.connected.borrow()
    }
}

/// Live capture settings and what the running source actually does. Shared
/// between the encode thread, the transport's viewer callback and the GUI.
#[derive(Debug, Clone)]
pub struct CaptureCtl {
    /// What the user asked for.
    pub prefs: CapturePrefs,
    /// The connected viewer cannot draw a separate cursor (older build).
    pub legacy_viewer: bool,
    /// What the current source was opened with (before portal fallback).
    asked_cursor: CursorCapture,
    asked_fps: u32,
    /// What the current source delivers.
    active_cursor: CursorCapture,
    cursor_note: Option<String>,
    negotiated_fps: Option<u32>,
    source_desc: String,
    /// FEC setting and link feedback, shared with the transport.
    fec: Arc<FecControl>,
}

impl CaptureCtl {
    fn new(prefs: CapturePrefs) -> Self {
        let fec = FecControl::new(prefs.fec);
        fec.set_adaptive(prefs.adaptive);
        Self {
            fec,
            prefs,
            legacy_viewer: false,
            asked_cursor: CursorCapture::Embedded,
            asked_fps: 0,
            active_cursor: CursorCapture::Embedded,
            cursor_note: None,
            negotiated_fps: None,
            source_desc: String::new(),
        }
    }

    /// The cursor mode to ask the capture source for right now.
    fn wanted_cursor(&self) -> CursorCapture {
        match self.prefs.cursor {
            CursorPref::Separate if self.legacy_viewer => CursorCapture::Embedded,
            CursorPref::Separate => CursorCapture::Metadata,
            CursorPref::Embedded => CursorCapture::Embedded,
            CursorPref::Hidden => CursorCapture::Hidden,
        }
    }

    fn needs_restart(&self) -> bool {
        self.wanted_cursor() != self.asked_cursor || self.prefs.fps.request() != self.asked_fps
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    fn status(&self) -> CaptureStatus {
        let mut note = self.cursor_note.clone();
        if self.legacy_viewer && self.prefs.cursor == CursorPref::Separate {
            note = Some("the connected viewer is an older version: cursor drawn into the video for it".into());
        }
        CaptureStatus {
            prefs: self.prefs,
            cursor_mode: self.active_cursor.as_str().to_owned(),
            cursor_note: note,
            fps_requested: self.asked_fps,
            fps_negotiated: self.negotiated_fps,
            fec: self.fec.snapshot(),
        }
    }
}

/// Capture settings as shown in the presenter window.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub struct CaptureStatus {
    pub prefs: CapturePrefs,
    /// `separate`, `embedded` or `hidden`: what the viewer actually gets.
    pub cursor_mode: String,
    pub cursor_note: Option<String>,
    pub fps_requested: u32,
    pub fps_negotiated: Option<u32>,
    /// FEC setting, applied overhead and what the viewer reports.
    pub fec: FecState,
}

impl Default for CaptureStatus {
    fn default() -> Self {
        CaptureCtl::new(CapturePrefs::default()).status()
    }
}

/// Open the capture source for the current wanted cursor mode and frame
/// rate, and record what it delivers.
fn open_source(
    args: &PresentArgs,
    config_dir: &Path,
    ctl: &Mutex<CaptureCtl>,
    sink: &CursorSink,
) -> Result<Box<dyn FrameSource>> {
    let (cursor, fps) = {
        let c = ctl.lock().expect("capture ctl");
        (c.wanted_cursor(), c.prefs.fps.request())
    };
    let source: Box<dyn FrameSource> = match args.source {
        SourceKind::Test => {
            let (w, h) = if let Some(s) = &args.size {
                parse_size(s).unwrap_or((args.width, args.height))
            } else {
                (args.width, args.height)
            };
            let pattern = TestPattern::new(w, h, fps.min(60), args.frames);
            if cursor == CursorCapture::Metadata {
                Box::new(pattern.with_cursor(sink.clone()))
            } else {
                Box::new(pattern)
            }
        }
        SourceKind::Portal => open_portal(args, config_dir, cursor, fps, sink)?,
    };
    let mut c = ctl.lock().expect("capture ctl");
    c.asked_cursor = cursor;
    c.asked_fps = fps;
    c.active_cursor = source.cursor_capture();
    c.cursor_note = source.cursor_note();
    c.negotiated_fps = None;
    c.source_desc = source.describe();
    tracing::info!(
        "capture: cursor {} (asked {}){}, frame rate up to {fps}",
        c.active_cursor.as_str(),
        cursor.as_str(),
        c.cursor_note.as_deref().map(|n| format!(" [{n}]")).unwrap_or_default()
    );
    Ok(source)
}

/// After the first frame the compositor's rate is known: run the encoder at
/// min(asked, negotiated) and keep a custom keyframe interval in seconds.
fn apply_negotiated_fps(source: &dyn FrameSource, ctl: &Mutex<CaptureCtl>, settings: &Mutex<EncoderConfig>) -> u32 {
    let mut c = ctl.lock().expect("capture ctl");
    c.negotiated_fps = source.negotiated_fps();
    c.source_desc = source.describe();
    let fps = effective_fps(c.asked_fps, c.negotiated_fps);
    let mut s = settings.lock().expect("encoder settings");
    if s.fps != fps {
        if let Some(k) = s.keyint {
            s.keyint = Some((k as u64 * fps as u64 / s.fps.max(1) as u64).max(1) as u32);
        }
        tracing::info!("encoder frame rate {} -> {fps} fps (compositor allows {:?})", s.fps, c.negotiated_fps);
        s.fps = fps;
    }
    fps
}

/// The encoder's nominal rate: what we asked for, capped by what the
/// compositor agreed to; 60 when "max" is asked and nothing is known.
fn effective_fps(asked: u32, negotiated: Option<u32>) -> u32 {
    let asked = asked.max(1);
    match negotiated {
        Some(n) => asked.min(n).max(1),
        None => asked.min(60),
    }
}

#[cfg(all(target_os = "linux", feature = "portal"))]
fn open_portal(
    args: &PresentArgs,
    config_dir: &Path,
    cursor: CursorCapture,
    fps: u32,
    sink: &CursorSink,
) -> Result<Box<dyn FrameSource>> {
    use mirrorwire_media::source::portal::{PortalOptions, PortalSource};
    Ok(Box::new(PortalSource::open(PortalOptions {
        restore_token_path: config_dir.join("portal_restore_token"),
        cursor,
        cursor_sink: Some(sink.clone()),
        allow_virtual: true,
        fps,
        // Only VAAPI can import DMA-BUFs. With `auto` the encoder may still
        // turn out to be x264; the pipeline then asks the source to switch
        // to shared memory.
        dmabuf: !args.no_dmabuf
            && matches!(args.encoder, EncoderBackend::Auto | EncoderBackend::Vaapi)
            && std::env::var_os("MIRRORWIRE_NO_DMABUF").is_none(),
    })?))
}

#[cfg(not(all(target_os = "linux", feature = "portal")))]
fn open_portal(
    _args: &PresentArgs,
    _config_dir: &Path,
    _cursor: CursorCapture,
    _fps: u32,
    _sink: &CursorSink,
) -> Result<Box<dyn FrameSource>> {
    bail!("portal capture is only available on Linux builds with the `portal` feature; use --source test")
}

struct ReadyInfo {
    name: String,
    uri: String,
    pairing_code: Option<String>,
    fingerprint: String,
    encoder: String,
    source: String,
    listen_addr: SocketAddr,
    advertised: Vec<SocketAddr>,
    rc_summary: String,
    conversion: String,
}

fn stream_info_from(encoder: &Encoder, fps: u32, cursor: CursorCapture) -> StreamInfo {
    let (w, h) = encoder.output_size();
    StreamInfo {
        width: w,
        height: h,
        fps,
        codec: "h264".into(),
        encoder: encoder.name().into(),
        bitrate_kbps: encoder.config().bitrate_kbps,
        intra_refresh: encoder.config().intra_refresh,
        cursor: Some(cursor.as_str().to_owned()),
        // Filled in by `Presenter::start` from the FEC setting.
        fec: None,
    }
}

/// Shared capture/encode/announce loop used by the CLI and the GUI.
#[allow(clippy::too_many_arguments)]
fn present_session(
    args: PresentArgs,
    config_dir: PathBuf,
    identity: Identity,
    runtime: &tokio::runtime::Handle,
    controls: Controls,
    name: String,
    settings: Arc<Mutex<EncoderConfig>>,
    attempts: Arc<Mutex<Vec<IncomingAttempt>>>,
    hud: Arc<Mutex<EncodeHudShim>>,
    ctl: Arc<Mutex<CaptureCtl>>,
    mut on_ready: impl FnMut(ReadyInfo, watch::Receiver<bool>) -> Result<()>,
) -> Result<()> {
    // Cursor changes go straight from the capture thread to the network,
    // independent of video frames. Full channel = drop (a newer one follows).
    let (cursor_tx, cursor_rx) = mpsc::channel::<CursorEvent>(256);
    let cursor_sink: CursorSink = Arc::new(move |ev| {
        let _ = cursor_tx.try_send(ev);
    });
    let mut source = open_source(&args, &config_dir, &ctl, &cursor_sink)?;
    let Some(first) = wait_first_frame(source.as_mut(), &controls)? else {
        bail!("capture source ended before the first frame");
    };
    let fps = apply_negotiated_fps(source.as_ref(), &ctl, &settings);
    let cfg = settings.lock().expect("encoder settings").clone();
    let encoder = open_encoder_for(&first, &cfg)?;
    let (active_cursor, fec) = {
        let c = ctl.lock().expect("capture ctl");
        (c.active_cursor, c.fec.clone())
    };

    let _guard = runtime.enter();
    let pairing_code = if args.no_pairing {
        None
    } else {
        Some(random_pairing_code()?)
    };
    let keyframe_controls = controls.clone();
    let viewer_ctl = ctl.clone();
    let viewer_controls = controls.clone();
    let presenter = Presenter::start(
        &identity,
        PresenterConfig {
            bind: SocketAddr::new(
                args.bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                args.port,
            ),
            stream_info: stream_info_from(&encoder, fps, active_cursor),
            pairing_code: pairing_code.clone(),
            config_dir: config_dir.clone(),
            attempts: attempts.clone(),
            // An older viewer can't draw a separate cursor: reopen the
            // capture with the cursor in the video while it is connected.
            on_viewer: Some(Arc::new(move |cursor_ok| {
                let mut c = viewer_ctl.lock().expect("capture ctl");
                c.legacy_viewer = !cursor_ok;
                if c.needs_restart() {
                    tracing::info!(
                        "viewer {} draw a separate cursor; reopening the capture with cursor {}",
                        if cursor_ok { "can" } else { "cannot" },
                        c.wanted_cursor().as_str()
                    );
                    viewer_controls.request_source_restart();
                }
            })),
            cursor_events: Some(cursor_rx),
            fec: fec.clone(),
        },
        Arc::new(move || keyframe_controls.request_keyframe()),
    )?;

    let advertised_ips = if let Some(ip) = args.advertise_ip {
        vec![ip]
    } else {
        let mut ips = lan_ipv4s();
        if ips.is_empty() {
            ips.extend(primary_ipv4());
        }
        if ips.is_empty() {
            ips.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        ips
    };
    let advertised: Vec<SocketAddr> = advertised_ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, presenter.local_addr.port()))
        .collect();
    let info = ConnectInfo {
        addr: advertised[0],
        addrs: advertised[1..].to_vec(),
        fingerprint: identity.fingerprint(),
        pairing_code: pairing_code.clone(),
        name: Some(name.clone()),
    };
    let uri = info.to_uri();
    if let Some(path) = &args.uri_file {
        std::fs::write(path, &uri)?;
    }

    on_ready(
        ReadyInfo {
            name: name.clone(),
            uri,
            pairing_code,
            fingerprint: identity.fingerprint(),
            encoder: encoder.name().to_owned(),
            source: source.describe(),
            listen_addr: presenter.local_addr,
            advertised: advertised.clone(),
            rc_summary: encoder.rc_summary().to_owned(),
            conversion: encoder.conversion_path().to_owned(),
        },
        presenter.viewer_connected.clone(),
    )?;

    let _advert = if args.no_mdns {
        None
    } else {
        match Advertisement::start(
            &name,
            presenter.local_addr.port(),
            &identity.fingerprint(),
            &format!("{}x{}", encoder.output_size().0, encoder.output_size().1),
        ) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!("mDNS announcement failed: {e:#}");
                None
            }
        }
    };

    let mut sink = QuicSink {
        frames: presenter.frames.clone(),
        connected: presenter.viewer_connected.clone(),
        controls: controls.clone(),
        next_id: 0,
    };
    let mut held = Some(first);
    let mut encoder = encoder;
    let result = loop {
        let hud_reopen = hud.clone();
        let hud_tick = hud.clone();
        let (rate_fec, rate_controls, rate_settings) = (fec.clone(), controls.clone(), settings.clone());
        let exit = continue_capture_encode(
            source.as_mut(),
            held.take(),
            encoder,
            &mut sink,
            &controls,
            settings.clone(),
            move |enc| {
                if let Ok(mut h) = hud_reopen.lock() {
                    let (cw, ch) = (h.capture_w, h.capture_h);
                    h.note_frame(enc, cw, ch);
                }
                Ok(())
            },
            move |enc, cw, ch, tick| {
                // Adaptive bitrate: the setting is the ceiling, the link
                // feedback decides the budget, FEC parity comes out of it.
                let ceiling = rate_settings.lock().expect("encoder settings").bitrate_kbps;
                rate_fec.set_ceiling(ceiling);
                rate_controls.set_bitrate_cap(rate_fec.encoder_kbps());
                if let Ok(mut h) = hud_tick.lock() {
                    match tick {
                        Tick::Captured => h.note_frame(enc, cw, ch),
                        Tick::Repeated => h.note_repeat(),
                    }
                }
            },
        );
        match exit {
            Ok(CaptureExit::Finished) => break Ok(()),
            Err(e) => break Err(e),
            Ok(CaptureExit::RestartSource(enc)) => {
                encoder = *enc;
                // Close the old portal session before opening the new one.
                drop(source);
                source = match open_source(&args, &config_dir, &ctl, &cursor_sink) {
                    Ok(s) => s,
                    Err(e) => break Err(e.context("reopening the capture with new settings")),
                };
                match wait_first_frame(source.as_mut(), &controls) {
                    Ok(Some(f)) => held = Some(f),
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e),
                }
                let fps = apply_negotiated_fps(source.as_ref(), &ctl, &settings);
                let cursor = ctl.lock().expect("capture ctl").active_cursor;
                presenter.update_stream_info(|info| {
                    info.fps = fps;
                    info.cursor = Some(cursor.as_str().to_owned());
                });
            }
        }
    };
    presenter.close();
    drop(sink);
    result
}

/// Timings the encode loop updates; the GUI polls them.
#[derive(Debug, Clone, Default)]
struct EncodeHudShim {
    encoder: String,
    encode_sum_ms: f64,
    encode_avg_ms: f64,
    pack_ms: f64,
    convert_ms: f64,
    upload_ms: f64,
    encode_ms: f64,
    path: String,
    output_w: u32,
    output_h: u32,
    capture_w: u32,
    capture_h: u32,
    rc_summary: String,
    frames_window: u32,
    repeats_window: u32,
    window_start: Option<std::time::Instant>,
    source_fps: f64,
    repeat_fps: f64,
    fallback_note: Option<String>,
}

impl EncodeHudShim {
    fn note_frame(
        &mut self,
        enc: &mirrorwire_media::encode::Encoder,
        capture_w: u32,
        capture_h: u32,
    ) {
        let (p, c, u, e) = enc.last_breakdown_ms();
        self.pack_ms = p;
        self.convert_ms = c;
        self.upload_ms = u;
        self.encode_ms = e;
        self.encode_sum_ms += e;
        if self.encoder != enc.name() {
            self.encoder = enc.name().to_owned();
        }
        self.path = enc.conversion_path().to_owned();
        let (w, h) = enc.output_size();
        self.output_w = w;
        self.output_h = h;
        self.capture_w = capture_w;
        self.capture_h = capture_h;
        self.rc_summary = enc.rc_summary().to_owned();
        self.fallback_note = enc.fallback_note().map(str::to_owned);
        self.frames_window += 1;
        self.roll_window();
    }

    /// The last frame was re-sent because the compositor had no new one.
    /// Counted apart from real frames so "source fps" stays honest.
    fn note_repeat(&mut self) {
        self.repeats_window += 1;
        self.roll_window();
    }

    fn roll_window(&mut self) {
        let now = std::time::Instant::now();
        let start = *self.window_start.get_or_insert(now);
        let elapsed = now.saturating_duration_since(start);
        if elapsed >= std::time::Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            self.source_fps = self.frames_window as f64 / secs;
            self.repeat_fps = self.repeats_window as f64 / secs;
            if self.frames_window > 0 {
                self.encode_avg_ms = self.encode_sum_ms / self.frames_window as f64;
            }
            tracing::info!(
                "encoder {}  capture path {}  {}x{}  source {:.1} fps (+{:.1} repeats/s)  pack/map {:.2} ms  convert {:.2} ms  upload {:.2} ms  encode {:.2} ms (avg {:.2} ms){}",
                self.encoder,
                self.path,
                self.capture_w,
                self.capture_h,
                self.source_fps,
                self.repeat_fps,
                self.pack_ms,
                self.convert_ms,
                self.upload_ms,
                self.encode_ms,
                self.encode_avg_ms,
                self.fallback_note.as_deref().map(|n| format!("  [{n}]")).unwrap_or_default()
            );
            self.encode_sum_ms = 0.0;
            self.frames_window = 0;
            self.repeats_window = 0;
            self.window_start = Some(now);
        }
    }

    #[cfg(feature = "gui")]
    fn to_hud(&self) -> EncodeHud {
        EncodeHud {
            encoder: self.encoder.clone(),
            encode_avg_ms: self.encode_avg_ms,
            pack_ms: self.pack_ms,
            convert_ms: self.convert_ms,
            upload_ms: self.upload_ms,
            encode_ms: self.encode_ms,
            path: self.path.clone(),
            capture_w: self.capture_w,
            capture_h: self.capture_h,
            source_fps: self.source_fps,
            repeat_fps: self.repeat_fps,
            fallback_note: self.fallback_note.clone(),
        }
    }
}

pub fn run(args: PresentArgs, config_dir: &Path) -> Result<()> {
    let identity = Identity::load_or_create(config_dir)?;
    let name = args.name.clone().unwrap_or_else(crate::default_name);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let controls = Controls::default();
    let stop = controls.clone();
    ctrlc_like(move || stop.stop());
    let settings = Arc::new(Mutex::new(args.encoder_config()?));
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let hud = Arc::new(Mutex::new(EncodeHudShim::default()));
    let ctl = Arc::new(Mutex::new(CaptureCtl::new(args.capture)));
    let ctl_print = ctl.clone();

    let result = present_session(
        args,
        config_dir.to_path_buf(),
        identity,
        runtime.handle(),
        controls,
        name,
        settings,
        attempts,
        hud,
        ctl,
        |ready, _connected| {
            println!("{}", qr_terminal(&ready.uri)?);
            println!(
                "presenter \"{}\" listening on {} (UDP {})",
                ready.name,
                ready.listen_addr,
                ready.listen_addr.port()
            );
            if !ready.advertised.is_empty() {
                println!(
                    "advertised {}",
                    ready
                        .advertised
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            println!("encoder {}  source {}", ready.encoder, ready.source);
            {
                let c = ctl_print.lock().expect("capture ctl");
                println!(
                    "cursor {}{}  frame rate asked {} fps, compositor allows {}",
                    c.active_cursor.as_str(),
                    c.cursor_note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default(),
                    c.asked_fps,
                    c.negotiated_fps.map(|f| format!("{f} fps")).unwrap_or_else(|| "?".into())
                );
                println!("forward error correction {}", c.prefs.fec.as_string());
            }
            println!("{}", ready.rc_summary);
            println!("colour conversion {}", ready.conversion);
            println!("fingerprint {}", short_fingerprint(&ready.fingerprint));
            if let Some(code) = &ready.pairing_code {
                println!("pairing code {code}");
            }
            println!("{}", ready.uri);
            let hint = FirewallHint::for_port(ready.listen_addr.port());
            if hint.show {
                println!("{}", hint.message);
                if !hint.guide.is_empty() {
                    println!("{}", hint.guide);
                }
            }
            Ok(())
        },
    );
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));
    result
}

/// Start presenting with the GUI defaults (portal capture, pairing, mDNS).
#[cfg(feature = "gui")]
pub fn spawn_for_gui(
    runtime: tokio::runtime::Handle,
    config_dir: PathBuf,
    identity: Identity,
    name: String,
    port: u16,
    on_update: Option<Arc<dyn Fn() + Send + Sync>>,
) -> PresenterHandle {
    let mut args = PresentArgs::gui_defaults();
    args.name = Some(name);
    args.port = port;
    args.capture = crate::capture_prefs::resolve(None, None, None, None, &config_dir);
    spawn(runtime, args, config_dir, identity, on_update)
}

/// Start presenting on a background thread and report status for the GUI.
#[cfg(feature = "gui")]
pub fn spawn(
    runtime: tokio::runtime::Handle,
    args: PresentArgs,
    config_dir: PathBuf,
    identity: Identity,
    on_update: Option<Arc<dyn Fn() + Send + Sync>>,
) -> PresenterHandle {
    let name = args.name.clone().unwrap_or_else(crate::default_name);
    let port = args.port;
    let settings = Arc::new(Mutex::new(
        args.encoder_config()
            .unwrap_or_else(|_| EncoderConfig::default()),
    ));
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let hud = Arc::new(Mutex::new(EncodeHudShim::default()));
    let capture = Arc::new(Mutex::new(CaptureCtl::new(args.capture)));
    let handle_capture = capture.clone();
    let handle_config_dir = config_dir.clone();
    let initial_settings = settings.lock().expect("encoder settings").clone();
    let status = Arc::new(Mutex::new(PresentStatus::new(
        name.clone(),
        initial_settings,
        port,
    )));
    {
        let mut s = status.lock().expect("present status");
        s.firewall = Some(FirewallHint::for_port(port));
    }
    let connected = Arc::new(Mutex::new(None::<watch::Receiver<bool>>));
    let controls = Controls::default();
    let handle_status = status.clone();
    let handle_connected = connected.clone();
    let handle_controls = controls.clone();
    let handle_settings = settings.clone();
    let handle_attempts = attempts.clone();
    let handle_hud = hud.clone();
    let ping = on_update.clone();
    let notify = move || {
        if let Some(cb) = &ping {
            cb();
        }
    };

    let thread = std::thread::Builder::new()
        .name("present".into())
        .spawn({
            let connected = connected.clone();
            let hud_status = hud.clone();
            move || {
                {
                    let mut s = status.lock().expect("present status");
                    s.phase = PresentPhase::WaitingForCapture;
                }
                notify();

                let status_ready = status.clone();
                let connected_slot = connected.clone();
                let ping_ready = on_update.clone();
                let result = present_session(
                    args,
                    config_dir,
                    identity,
                    &runtime,
                    controls.clone(),
                    name,
                    settings,
                    attempts,
                    hud,
                    capture,
                    move |ready, rx| {
                        {
                            let mut s = status_ready.lock().expect("present status");
                            s.phase = PresentPhase::Streaming;
                            s.pairing_code = ready.pairing_code;
                            s.uri = Some(ready.uri);
                            s.fingerprint = Some(ready.fingerprint);
                            s.encoder = Some(ready.encoder);
                            s.source = Some(ready.source);
                            s.listen_addr = Some(ready.listen_addr.to_string());
                            s.advertised = ready.advertised.iter().map(|a| a.to_string()).collect();
                            s.port = ready.listen_addr.port();
                            s.rc_summary = Some(ready.rc_summary);
                            s.conversion = Some(ready.conversion);
                        }
                        *connected_slot.lock().expect("connected slot") = Some(rx);
                        if let Some(cb) = &ping_ready {
                            cb();
                        }
                        Ok(())
                    },
                );

                let mut s = status.lock().expect("present status");
                match result {
                    Ok(()) => {
                        if s.phase != PresentPhase::Failed {
                            s.phase = PresentPhase::Stopped;
                        }
                    }
                    Err(e) => {
                        tracing::error!("presenter: {e:#}");
                        s.phase = PresentPhase::Failed;
                        s.error = Some(format!("{e:#}"));
                    }
                }
                drop(s);
                let _ = hud_status;
                notify();
            }
        })
        .expect("spawn present thread");

    PresenterHandle {
        status: handle_status,
        connected: handle_connected,
        controls: handle_controls,
        settings: handle_settings,
        attempts: handle_attempts,
        hud: handle_hud,
        capture: handle_capture,
        config_dir: handle_config_dir,
        thread: Some(thread),
    }
}

fn ctrlc_like(on_signal: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Ok(rt) = rt {
            rt.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
            });
            on_signal();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_rate_follows_compositor() {
        assert_eq!(effective_fps(60, Some(60)), 60);
        assert_eq!(effective_fps(60, Some(30)), 30);
        assert_eq!(effective_fps(240, Some(144)), 144);
        assert_eq!(effective_fps(240, None), 60);
        assert_eq!(effective_fps(30, None), 30);
    }

    #[test]
    fn old_viewer_forces_embedded_cursor() {
        let mut c = CaptureCtl::new(CapturePrefs::default());
        c.asked_cursor = CursorCapture::Metadata;
        c.asked_fps = 60;
        assert!(!c.needs_restart());
        c.legacy_viewer = true;
        assert_eq!(c.wanted_cursor(), CursorCapture::Embedded);
        assert!(c.needs_restart());
        // A portal without metadata already falls back; no restart loop.
        c.asked_cursor = CursorCapture::Embedded;
        assert!(!c.needs_restart());
        c.prefs.fps = FpsPref::Max;
        assert!(c.needs_restart());
    }

    /// End to end in one process: test-pattern presenter (x264) -> QUIC ->
    /// viewer (software decode, RGBA) with 1 % simulated loss, so FEC,
    /// keyframe recovery, live settings and both status paths all run.
    #[cfg(feature = "gui")]
    #[test]
    fn test_source_streams_to_an_in_process_viewer() {
        use crate::viewer::{self, ViewerOptions};
        use std::time::{Duration, Instant};
        let dir = std::env::temp_dir().join(format!("mirrorwire-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let localhost = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let mut args = PresentArgs::gui_defaults();
        args.source = SourceKind::Test;
        (args.width, args.height) = (640, 360);
        args.encoder = EncoderBackend::X264;
        args.bitrate_kbps = 4000;
        (args.port, args.bind, args.advertise_ip) = (0, localhost, localhost);
        args.no_mdns = true;
        args.name = Some("e2e".into());
        let presenter = spawn(runtime.handle().clone(), args, dir.clone(), Identity::generate().unwrap(), None);

        let deadline = Instant::now() + Duration::from_secs(60);
        let wait = |what: &str, done: &mut dyn FnMut() -> bool| {
            while !done() {
                let s = presenter.poll();
                assert!(s.phase != PresentPhase::Failed, "presenter failed: {:?}", s.error);
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let mut uri = None;
        wait("the presenter URI", &mut || {
            uri = presenter.poll().uri;
            uri.is_some()
        });
        let target = ConnectInfo::parse(&uri.unwrap()).unwrap();
        assert!(target.pairing_code.is_some(), "pairing is on by default");
        let view = viewer::spawn(
            runtime.handle(),
            Arc::new(Identity::generate().unwrap()),
            target,
            ViewerOptions {
                hwdec: false,
                simulate_loss: 0.01,
                simulate_burst: 1.0,
                simulate_bandwidth_kbps: 0,
                rgba: true,
                name: "e2e viewer".into(),
            },
            None,
        );
        // Waits for `n` more frames to reach the display queue.
        let count = |n: usize| {
            let mut shown = 0;
            wait("decoded frames", &mut || {
                while view.frames.try_recv().is_ok() {
                    shown += 1;
                }
                assert!(view.status.lock().unwrap().error.is_none(), "{:?}", view.status.lock().unwrap().error);
                shown >= n
            })
        };
        count(20);

        let status = presenter.poll();
        assert!(status.viewer_connected);
        assert_eq!(status.phase, PresentPhase::Streaming);
        assert_eq!(status.capture.cursor_mode, "separate");
        assert!(status.capture.fec.viewer_fec);
        assert!(status.attempts.iter().any(|a| a.ok), "{:?}", status.attempts);
        // Live changes: fixed FEC (no restart), a lower bitrate (encoder
        // reopens with an IDR), embedded cursor (capture restarts).
        let mut prefs = status.capture.prefs;
        prefs.fec = FecMode::Fixed(30);
        prefs.cursor = CursorPref::Embedded;
        presenter.apply_capture(prefs);
        let mut cfg = status.settings.clone();
        cfg.bitrate_kbps = 3000;
        presenter.apply_settings(cfg);
        count(30);
        let status = presenter.poll();
        assert_eq!(status.capture.fec.mode, FecMode::Fixed(30));
        assert_eq!(status.settings.bitrate_kbps, 3000);
        assert_eq!(mirrorwire_net::settings::Settings::load(&dir).fec.as_deref(), Some("30%"));

        let viewer_status = view.status.lock().unwrap().clone();
        assert!(viewer_status.connected && viewer_status.first_frame);
        view.stop();
        let (totals, _, _) = view.done.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(totals.frames >= 50, "{} frames", totals.frames);
        assert_eq!(totals.marker_mismatch, 0);
        assert!(totals.link.parity_expected > 0, "FEC packets were sent");
        // (Whether the 1 % loss hit any counted packet is left to chance.)
        assert!(totals.link.received <= totals.link.expected);
        presenter.stop();
        wait("the presenter to stop", &mut || {
            matches!(presenter.poll().phase, PresentPhase::Stopped)
        });
        std::fs::remove_dir_all(&dir).ok();
    }
}
