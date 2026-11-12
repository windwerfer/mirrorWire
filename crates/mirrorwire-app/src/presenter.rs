use anyhow::{bail, Result};
use clap::{Args, ValueEnum};
use mirrorwire_media::encode::{
    parse_size, Encoder, EncoderBackend, EncoderConfig, RateControl, DEFAULT_BITRATE_KBPS,
    DEFAULT_QP,
};
use mirrorwire_media::frame::EncodedFrame;
use mirrorwire_media::pipeline::{
    continue_capture_encode, open_encoder_for, wait_first_frame, Controls, VideoSink,
};
use mirrorwire_media::source::testpattern::TestPattern;
use mirrorwire_media::source::FrameSource;
#[cfg(feature = "gui")]
use mirrorwire_net::diag::FirewallHint;
use mirrorwire_net::diag::IncomingAttempt;
use mirrorwire_net::discovery::Advertisement;
use mirrorwire_net::identity::{short_fingerprint, Identity};
use mirrorwire_net::pairing::{lan_ipv4s, primary_ipv4, qr_terminal, random_pairing_code, ConnectInfo};
use mirrorwire_net::transport::{OutgoingFrame, Presenter, PresenterConfig};
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
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Target bitrate in kbit/s (default: 40000 = 40 Mbit/s).
    #[arg(long, default_value_t = DEFAULT_BITRATE_KBPS)]
    bitrate_kbps: u32,
    /// Target bitrate in Mbit/s; overrides --bitrate-kbps when set.
    #[arg(long)]
    bitrate_mbps: Option<f64>,
    /// auto, vaapi, x264, videotoolbox
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
    #[arg(long, default_value_t = mirrorwire_proto::DEFAULT_PORT)]
    port: u16,
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
    /// Hide the mouse cursor in the capture (portal source).
    #[arg(long)]
    hide_cursor: bool,
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
            fps: self.fps,
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
            fps: 60,
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
    pub pack_ms: f64,
    pub convert_ms: f64,
    pub upload_ms: f64,
    pub encode_ms: f64,
    pub path: String,
    pub capture_w: u32,
    pub capture_h: u32,
    pub source_fps: f64,
}

#[cfg(feature = "gui")]
impl Default for EncodeHud {
    fn default() -> Self {
        Self {
            pack_ms: 0.0,
            convert_ms: 0.0,
            upload_ms: 0.0,
            encode_ms: 0.0,
            path: String::new(),
            capture_w: 0,
            capture_h: 0,
            source_fps: 0.0,
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

    pub fn poll(&self) -> PresentStatus {
        let mut s = self.status.lock().expect("present status").clone();
        if let Some(rx) = self.connected.lock().expect("connected watch").as_ref() {
            s.viewer_connected = *rx.borrow();
        }
        s.attempts = self.attempts.lock().expect("attempts").clone();
        s.hud = self.hud.lock().expect("hud").to_hud();
        s.settings = self.settings.lock().expect("encoder settings").clone();
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

fn open_source(args: &PresentArgs, config_dir: &Path) -> Result<Box<dyn FrameSource>> {
    match args.source {
        SourceKind::Test => {
            let (w, h) = if let Some(s) = &args.size {
                parse_size(s).unwrap_or((args.width, args.height))
            } else {
                (args.width, args.height)
            };
            Ok(Box::new(TestPattern::new(w, h, args.fps, args.frames)))
        }
        SourceKind::Portal => open_portal(args, config_dir),
    }
}

#[cfg(all(target_os = "linux", feature = "portal"))]
fn open_portal(args: &PresentArgs, config_dir: &Path) -> Result<Box<dyn FrameSource>> {
    use mirrorwire_media::source::portal::{PortalOptions, PortalSource};
    Ok(Box::new(PortalSource::open(PortalOptions {
        restore_token_path: config_dir.join("portal_restore_token"),
        show_cursor: !args.hide_cursor,
        allow_virtual: true,
        fps: args.fps,
    })?))
}

#[cfg(not(all(target_os = "linux", feature = "portal")))]
fn open_portal(_args: &PresentArgs, _config_dir: &Path) -> Result<Box<dyn FrameSource>> {
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

fn stream_info_from(encoder: &Encoder, fps: u32) -> StreamInfo {
    let (w, h) = encoder.output_size();
    StreamInfo {
        width: w,
        height: h,
        fps,
        codec: "h264".into(),
        encoder: encoder.name().into(),
        bitrate_kbps: encoder.config().bitrate_kbps,
        intra_refresh: encoder.config().intra_refresh,
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
    mut on_ready: impl FnMut(ReadyInfo, watch::Receiver<bool>) -> Result<()>,
) -> Result<()> {
    let mut source = open_source(&args, &config_dir)?;
    let Some(first) = wait_first_frame(source.as_mut(), &controls)? else {
        bail!("capture source ended before the first frame");
    };
    let cfg = settings.lock().expect("encoder settings").clone();
    let encoder = open_encoder_for(&first, &cfg)?;

    let _guard = runtime.enter();
    let pairing_code = if args.no_pairing {
        None
    } else {
        Some(random_pairing_code()?)
    };
    let keyframe_controls = controls.clone();
    let presenter = Presenter::start(
        &identity,
        PresenterConfig {
            bind: SocketAddr::new(
                args.bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                args.port,
            ),
            stream_info: stream_info_from(&encoder, args.fps),
            pairing_code: pairing_code.clone(),
            config_dir,
            attempts: attempts.clone(),
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
    let hud_reopen = hud.clone();
    let hud_tick = hud.clone();
    let result = continue_capture_encode(
        source.as_mut(),
        Some(first),
        encoder,
        &mut sink,
        &controls,
        settings,
        move |enc| {
            if let Ok(mut h) = hud_reopen.lock() {
                let (cw, ch) = (h.capture_w, h.capture_h);
                h.note_frame(enc, cw, ch);
            }
            Ok(())
        },
        move |enc, cw, ch| {
            if let Ok(mut h) = hud_tick.lock() {
                h.note_frame(enc, cw, ch);
            }
        },
    );
    presenter.close();
    drop(sink);
    result
}

/// Timings the encode loop updates; the GUI polls them.
#[derive(Debug, Clone, Default)]
struct EncodeHudShim {
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
    window_start: Option<std::time::Instant>,
    source_fps: f64,
}

impl EncodeHudShim {
    fn note_frame(&mut self, enc: &mirrorwire_media::encode::Encoder, capture_w: u32, capture_h: u32) {
        let (p, c, u, e) = enc.last_breakdown_ms();
        self.pack_ms = p;
        self.convert_ms = c;
        self.upload_ms = u;
        self.encode_ms = e;
        self.path = enc.conversion_path().to_owned();
        let (w, h) = enc.output_size();
        self.output_w = w;
        self.output_h = h;
        self.capture_w = capture_w;
        self.capture_h = capture_h;
        self.rc_summary = enc.rc_summary().to_owned();
        let now = std::time::Instant::now();
        let start = *self.window_start.get_or_insert(now);
        self.frames_window += 1;
        let elapsed = now.saturating_duration_since(start);
        if elapsed >= std::time::Duration::from_secs(1) {
            self.source_fps = self.frames_window as f64 / elapsed.as_secs_f64();
            tracing::info!(
                "encoder {}  {}x{}  source {:.1} fps  pack {:.2} ms  convert {:.2} ms  upload {:.2} ms  encode {:.2} ms",
                self.path,
                self.capture_w,
                self.capture_h,
                self.source_fps,
                self.pack_ms,
                self.convert_ms,
                self.upload_ms,
                self.encode_ms
            );
            self.frames_window = 0;
            self.window_start = Some(now);
        }
    }

    #[cfg(feature = "gui")]
    fn to_hud(&self) -> EncodeHud {
        EncodeHud {
            pack_ms: self.pack_ms,
            convert_ms: self.convert_ms,
            upload_ms: self.upload_ms,
            encode_ms: self.encode_ms,
            path: self.path.clone(),
            capture_w: self.capture_w,
            capture_h: self.capture_h,
            source_fps: self.source_fps,
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
        |ready, _connected| {
            println!("{}", qr_terminal(&ready.uri)?);
            println!(
                "presenter \"{}\" listening on {} (UDP {})",
                ready.name, ready.listen_addr, ready.listen_addr.port()
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
            println!("{}", ready.rc_summary);
            println!("colour conversion {}", ready.conversion);
            println!("fingerprint {}", short_fingerprint(&ready.fingerprint));
            if let Some(code) = &ready.pairing_code {
                println!("pairing code {code}");
            }
            println!("{}", ready.uri);
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
        args.encoder_config().unwrap_or_else(|_| EncoderConfig::default()),
    ));
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let hud = Arc::new(Mutex::new(EncodeHudShim::default()));
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
