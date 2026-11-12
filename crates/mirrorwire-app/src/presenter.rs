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
        SourceKind::Test => Ok(Box::new(TestPattern::new(args.width, args.height, args.fps, args.frames))),
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

pub fn run(args: PresentArgs, config_dir: &Path) -> Result<()> {
    let identity = Identity::load_or_create(config_dir)?;
    let name = args.name.clone().unwrap_or_else(crate::default_name);
    let mut source = open_source(&args, config_dir)?;
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let _guard = runtime.enter();

    let controls = Controls::default();
    let pairing_code = if args.no_pairing { None } else { Some(random_pairing_code()?) };
    let keyframe_controls = controls.clone();
    let presenter = Presenter::start(
        &identity,
        PresenterConfig {
            bind: SocketAddr::new(args.bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), args.port),
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
            config_dir: config_dir.to_path_buf(),
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
    println!("{}", qr_terminal(&uri)?);
    println!("presenter \"{name}\" listening on {}", presenter.local_addr);
    println!("fingerprint {}", short_fingerprint(&identity.fingerprint()));
    if let Some(code) = &pairing_code {
        println!("pairing code {code}");
    }
    println!("{uri}");
    if let Some(path) = &args.uri_file {
        std::fs::write(path, &uri)?;
    }

    let _advert = if args.no_mdns {
        None
    } else {
        match Advertisement::start(&name, presenter.local_addr.port(), &identity.fingerprint(), &format!("{}x{}", args.width, args.height)) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!("mDNS announcement failed: {e:#}");
                None
            }
        }
    };

    let stop = controls.clone();
    ctrlc_like(move || stop.stop());

    let mut sink = QuicSink {
        frames: presenter.frames.clone(),
        connected: presenter.viewer_connected.clone(),
        controls: controls.clone(),
        next_id: 0,
    };
    let result = run_capture_encode(source.as_mut(), &mut encoder, &mut sink, &controls);
    presenter.close();
    drop(sink);
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));
    result
}

fn ctrlc_like(on_signal: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
        if let Ok(rt) = rt {
            rt.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
            });
            on_signal();
        }
    });
}
