mod applog;
#[cfg(feature = "gui")]
mod gui;
mod presenter;
mod viewer;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "mirrorwire",
    version,
    about = "Low-latency screen mirroring over LAN"
)]
struct Cli {
    /// Directory for identity, pairing and portal state (default: ~/.config/mirrorwire).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    /// GUI renderer: `auto` (wgpu, then OpenGL), `gl` (OpenGL/glow), or `wgpu`.
    /// Defaults to `gl` on macOS x86_64 (no Metal on older Intel Macs) and `auto` elsewhere.
    /// Overrides the MIRRORWIRE_RENDERER environment variable.
    #[arg(long, global = true, value_enum, value_name = "gl|wgpu|auto")]
    renderer: Option<RendererChoice>,

    /// Without a subcommand the start screen opens (Present / View).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum RendererChoice {
    /// Try wgpu first and fall back to OpenGL if it fails.
    Auto,
    /// OpenGL via eframe's glow backend. Default on macOS x86_64.
    #[value(alias = "glow", alias = "opengl")]
    Gl,
    /// wgpu (Metal on macOS, Vulkan or GL on Linux).
    Wgpu,
}

#[derive(Subcommand)]
enum Command {
    /// Share this screen.
    Present(presenter::PresentArgs),
    /// Show a presenter's screen.
    View(ViewArgs),
    /// List presenters announced on the local network.
    Discover {
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
    },
}

#[derive(Args, Default)]
pub struct ViewArgs {
    /// mirrorwire:// URI from the presenter's QR code. Without it, the GUI shows discovered presenters.
    pub uri: Option<String>,
    /// Decode and report statistics without opening a window.
    #[arg(long)]
    pub headless: bool,
    /// Stop after this many seconds (headless mode).
    #[arg(long)]
    pub duration: Option<f64>,
    /// Use hardware decoding (VideoToolbox on macOS, VAAPI on Linux).
    #[arg(long)]
    pub hwdec: bool,
    /// Convert decoded frames to RGBA in headless mode too, to measure that cost.
    #[arg(long)]
    pub rgba: bool,
    /// Drop this percentage of received datagrams, to test loss recovery.
    #[arg(long, default_value_t = 0.0)]
    pub simulate_loss: f64,
    /// Name shown to the presenter.
    #[arg(long)]
    pub name: Option<String>,
}

fn main() -> Result<()> {
    let log = applog::init();
    mirrorwire_media::init()?;
    let cli = Cli::parse();
    let config_dir = mirrorwire_net::config_dir(cli.config_dir.as_deref());
    let renderer = resolve_renderer(cli.renderer);

    match cli.command {
        None => run_gui(GuiLaunch::Home, ViewArgs::default(), &config_dir, renderer, log, None),
        Some(Command::Present(args)) => presenter::run(args, &config_dir),
        Some(Command::View(args)) if args.headless => viewer::run_headless(args, &config_dir),
        Some(Command::View(args)) => run_gui(GuiLaunch::View, args, &config_dir, renderer, log, None),
        Some(Command::Discover { seconds }) => {
            let found = mirrorwire_net::discovery::Browser::start()?
                .browse_for(Duration::from_secs_f64(seconds));
            if found.is_empty() {
                println!("no presenters found");
            }
            for p in found {
                let addrs = if p.addrs.is_empty() {
                    p.addr.to_string()
                } else {
                    p.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",")
                };
                println!(
                    "{}  {}  {}  {}",
                    p.name,
                    addrs,
                    p.resolution.unwrap_or_default(),
                    mirrorwire_net::identity::short_fingerprint(&p.fingerprint)
                );
            }
            Ok(())
        }
    }
}

pub enum GuiLaunch {
    /// Start screen with Present / View buttons.
    Home,
    /// Go straight to the viewer (CLI `view`).
    View,
}

fn default_renderer() -> RendererChoice {
    if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        RendererChoice::Gl
    } else {
        RendererChoice::Auto
    }
}

pub fn resolve_renderer(cli: Option<RendererChoice>) -> RendererChoice {
    if let Some(choice) = cli {
        return choice;
    }
    match std::env::var("MIRRORWIRE_RENDERER") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "gl" | "glow" | "opengl" => RendererChoice::Gl,
            "wgpu" => RendererChoice::Wgpu,
            "auto" => RendererChoice::Auto,
            other => {
                tracing::warn!("unknown MIRRORWIRE_RENDERER={other:?}, using the default");
                default_renderer()
            }
        },
        Err(_) => default_renderer(),
    }
}

#[cfg(feature = "gui")]
fn run_gui(
    launch: GuiLaunch,
    args: ViewArgs,
    config_dir: &std::path::Path,
    renderer: RendererChoice,
    log: applog::LogStore,
    port: Option<u16>,
) -> Result<()> {
    gui::run(launch, args, config_dir, renderer, log, port.unwrap_or(mirrorwire_proto::DEFAULT_PORT))
}

#[cfg(not(feature = "gui"))]
fn run_gui(
    _launch: GuiLaunch,
    _args: ViewArgs,
    _config_dir: &std::path::Path,
    _renderer: RendererChoice,
    _log: applog::LogStore,
    _port: Option<u16>,
) -> Result<()> {
    anyhow::bail!("built without the gui feature; use --headless or a subcommand")
}

pub fn default_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "mirrorwire".into())
}
