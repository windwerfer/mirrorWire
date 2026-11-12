use anyhow::{bail, Result};
use clap::{Args, ValueEnum};
use mirrorwire_media::encode::{Encoder, EncoderBackend, EncoderConfig};
use mirrorwire_media::frame::EncodedFrame;
use mirrorwire_media::pipeline::{run_capture_encode, Controls, VideoSink};
use mirrorwire_media::source::testpattern::TestPattern;
use mirrorwire_media::source::FrameSource;
use mirrorwire_net::discovery::Advertisement;
use mirrorwire_net::identity::{short_fingerprint, Identity};
use mirrorwire_net::pairing::{primary_ipv4, qr_terminal, random_pairing_code, ConnectInfo};
use mirrorwire_net::transport::{OutgoingFrame, Presenter, PresenterConfig};
use mirrorwire_proto::control::StreamInfo;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "gui")]
use std::sync::Mutex;
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
    #[arg(long, default_value_t = 2000)]
    width: u32,
    #[arg(long, default_value_t = 1000)]
    height: u32,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value_t = 20_000)]
    bitrate_kbps: u32,
    /// auto, vaapi, x264, videotoolbox
    #[arg(long, default_value = "auto")]
    encoder: EncoderBackend,
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
    #[cfg(feature = "gui")]
    pub fn gui_defaults() -> Self {
        Self {
            source: SourceKind::Portal,
            width: 2000,
            height: 1000,
            fps: 60,
            bitrate_kbps: 20_000,
            encoder: EncoderBackend::Auto,
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
pub struct PresentStatus {
    pub phase: PresentPhase,
    pub name: String,
    pub pairing_code: Option<String>,
    pub uri: Option<String>,
    pub fingerprint: Option<String>,
    pub encoder: Option<String>,
    pub source: Option<String>,
    pub listen_addr: Option<String>,
    pub viewer_connected: bool,
    pub error: Option<String>,
}

#[cfg(feature = "gui")]
impl PresentStatus {
    fn new(name: String) -> Self {
        Self {
            phase: PresentPhase::Starting,
            name,
            pairing_code: None,
            uri: None,
            fingerprint: None,
            encoder: None,
            source: None,
            listen_addr: None,
            viewer_connected: false,
            error: None,
        }
    }
}

#[cfg(feature = "gui")]
pub struct PresenterHandle {
    status: Arc<Mutex<PresentStatus>>,
    connected: Arc<Mutex<Option<watch::Receiver<bool>>>>,
    controls: Controls,
    thread: Option<JoinHandle<()>>,
}

#[cfg(feature = "gui")]
impl PresenterHandle {
    pub fn stop(&self) {
        self.controls.stop();
    }

    pub fn poll(&self) -> PresentStatus {
        let mut s = self.status.lock().expect("present status").clone();
        if let Some(rx) = self.connected.lock().expect("connected watch").as_ref() {
            s.viewer_connected = *rx.borrow();
        }
        s
    }
}

#[cfg(feature = "gui")]
impl Drop for PresenterHandle {
    fn drop(&mut self) {
        self.controls.stop();
        // Detach: the thread may be blocked in the portal dialog.
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
        SourceKind::Test => Ok(Box::new(TestPattern::new(
            args.width,
            args.height,
            args.fps,
            args.frames,
        ))),
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
}

/// Shared capture/encode/announce loop used by the CLI and the GUI.
fn present_session(
    args: PresentArgs,
    config_dir: PathBuf,
    identity: Identity,
    runtime: &tokio::runtime::Handle,
    controls: Controls,
    name: String,
    mut on_ready: impl FnMut(ReadyInfo, watch::Receiver<bool>) -> Result<()>,
) -> Result<()> {
    let mut source = open_source(&args, &config_dir)?;
    let mut encoder = Encoder::new(EncoderConfig {
        width: args.width,
        height: args.height,
        fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
        backend: args.encoder,
        intra_refresh: args.intra_refresh,
        vaapi_device: args.vaapi_device.clone(),
    })?;
    tracing::info!("source: {}, encoder: {}", source.describe(), encoder.name());

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
            stream_info: StreamInfo {
                width: args.width,
                height: args.height,
                fps: args.fps,
                codec: "h264".into(),
                encoder: encoder.name().into(),
                bitrate_kbps: args.bitrate_kbps,
                intra_refresh: args.intra_refresh,
            },
            pairing_code: pairing_code.clone(),
            config_dir,
        },
        Arc::new(move || keyframe_controls.request_keyframe()),
    )?;

    let advertise_ip = args
        .advertise_ip
        .or_else(primary_ipv4)
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let info = ConnectInfo {
        addr: SocketAddr::new(advertise_ip, presenter.local_addr.port()),
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
            &format!("{}x{}", args.width, args.height),
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
    let result = run_capture_encode(source.as_mut(), &mut encoder, &mut sink, &controls);
    presenter.close();
    drop(sink);
    result
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

    let result = present_session(
        args,
        config_dir.to_path_buf(),
        identity,
        runtime.handle(),
        controls,
        name,
        |ready, _connected| {
            println!("{}", qr_terminal(&ready.uri)?);
            println!(
                "presenter \"{}\" listening on {}",
                ready.name, ready.listen_addr
            );
            println!("encoder {}  source {}", ready.encoder, ready.source);
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
    on_update: Option<Arc<dyn Fn() + Send + Sync>>,
) -> PresenterHandle {
    let mut args = PresentArgs::gui_defaults();
    args.name = Some(name);
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
    let status = Arc::new(Mutex::new(PresentStatus::new(name.clone())));
    let connected = Arc::new(Mutex::new(None::<watch::Receiver<bool>>));
    let controls = Controls::default();
    let handle_status = status.clone();
    let handle_connected = connected.clone();
    let handle_controls = controls.clone();
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
                notify();
            }
        })
        .expect("spawn present thread");

    PresenterHandle {
        status: handle_status,
        connected: handle_connected,
        controls: handle_controls,
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
