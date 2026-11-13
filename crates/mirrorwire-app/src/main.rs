mod applog;
mod capture_prefs;
#[cfg(feature = "gui")]
mod gui;
mod presenter;
mod session;
mod viewer;
#[cfg(feature = "gui")]
mod yuv_glow;

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

    /// UDP port the presenter binds and advertises (default: 47800, or
    /// MIRRORWIRE_PORT, or the port saved in the GUI). 0 picks an ephemeral port.
    #[arg(long, global = true)]
    port: Option<u16>,

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

#[derive(Args)]
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
    /// Defaults to on on macOS; falls back to software if the GPU cannot
    /// decode H.264 (a 2011 HD 6770M on OCLP often cannot).
    #[arg(long, default_value_t = cfg!(target_os = "macos"))]
    pub hwdec: bool,
    /// Force the software H.264 decoder.
    #[arg(long, overrides_with = "hwdec")]
    pub software_decode: bool,
    /// Convert decoded frames to RGBA in headless mode too, to measure that cost.
    #[arg(long)]
    pub rgba: bool,
    /// Drop this percentage of received datagrams, to test loss recovery.
    #[arg(long, default_value_t = 0.0)]
    pub simulate_loss: f64,
    /// Mean length of a loss burst in packets for --simulate-loss (1 =
    /// independent losses; e.g. 4 to mimic Wi-Fi bursts at the same average).
    #[arg(long, default_value_t = 1.0)]
    pub simulate_burst: f64,
    /// Name shown to the presenter.
    #[arg(long)]
    pub name: Option<String>,
}

impl Default for ViewArgs {
    fn default() -> Self {
        Self {
            uri: None,
            headless: false,
            duration: None,
            hwdec: cfg!(target_os = "macos"),
            software_decode: false,
            rgba: false,
            simulate_loss: 0.0,
            simulate_burst: 1.0,
            name: None,
        }
    }
}

impl ViewArgs {
    pub fn want_hwdec(&self) -> bool {
        if self.software_decode {
            false
        } else {
            self.hwdec
        }
    }
}

fn main() -> Result<()> {
    let log = applog::init();
    mirrorwire_media::init()?;
    let cli = Cli::parse();
    let config_dir = mirrorwire_net::config_dir(cli.config_dir.as_deref());
    let renderer = resolve_renderer(cli.renderer);

    let port = mirrorwire_net::settings::resolve_port(cli.port, &config_dir);

    match cli.command {
        None => run_gui(
            GuiLaunch::Home,
            ViewArgs::default(),
            &config_dir,
            renderer,
            log,
            Some(port),
        ),
        Some(Command::Present(mut args)) => {
            args.port = port;
            let cli_cursor = if args.hide_cursor { Some(capture_prefs::CursorPref::Hidden) } else { args.cursor };
            args.capture = capture_prefs::resolve(cli_cursor, args.fps, args.fec, &config_dir);
            presenter::run(args, &config_dir)
        }
        Some(Command::View(args)) if args.headless => viewer::run_headless(args, &config_dir),
        Some(Command::View(args)) => run_gui(
            GuiLaunch::View,
            args,
            &config_dir,
            renderer,
            log,
            Some(port),
        ),
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
                    p.addrs
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
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
    gui::run(
        launch,
        args,
        config_dir,
        renderer,
        log,
        port.unwrap_or(mirrorwire_proto::DEFAULT_PORT),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("mirrorwire").chain(args.iter().copied()))
    }

    #[test]
    fn present_flags_reach_the_capture_preferences() {
        let cli = parse(&["--port", "0", "present", "--source", "test", "--fec", "20%", "--fps", "max", "--cursor", "embedded"]).unwrap();
        assert_eq!(cli.port, Some(0));
        let Some(Command::Present(args)) = cli.command else { panic!("not present") };
        assert_eq!(args.fec, Some(mirrorwire_proto::fec::FecMode::Fixed(20)));
        assert_eq!(args.fps, Some(capture_prefs::FpsPref::Max));
        assert_eq!(args.cursor, Some(capture_prefs::CursorPref::Embedded));
        // CLI values beat whatever is saved (empty config dir here).
        let dir = std::env::temp_dir().join(format!("mirrorwire-cli-{}", std::process::id()));
        let prefs = capture_prefs::resolve(args.cursor, args.fps, args.fec, &dir);
        assert_eq!(prefs.fec, mirrorwire_proto::fec::FecMode::Fixed(20));
        assert_eq!(prefs.cursor, capture_prefs::CursorPref::Embedded);
    }

    #[test]
    fn invalid_values_are_rejected_by_the_parser() {
        assert!(parse(&["present", "--fec", "150"]).is_err());
        assert!(parse(&["present", "--fps", "0"]).is_err());
        assert!(parse(&["present", "--cursor", "sideways"]).is_err());
        assert!(parse(&["--renderer", "vulkan"]).is_err());
    }

    #[test]
    fn view_flags_and_hwdec() {
        let cli = parse(&["view", "--headless", "--duration", "5", "--simulate-loss", "2", "--simulate-burst", "4", "--software-decode", "mirrorwire://x"]).unwrap();
        let Some(Command::View(args)) = cli.command else { panic!("not view") };
        assert!(args.headless);
        assert_eq!((args.duration, args.simulate_loss, args.simulate_burst), (Some(5.0), 2.0, 4.0));
        assert!(!args.want_hwdec(), "--software-decode wins over --hwdec");
        assert!(ViewArgs { hwdec: true, ..ViewArgs::default() }.want_hwdec());
        assert_eq!(resolve_renderer(Some(RendererChoice::Wgpu)), RendererChoice::Wgpu);
        let cli = parse(&["--renderer", "glow"]).unwrap();
        assert_eq!(cli.renderer, Some(RendererChoice::Gl));
        assert!(cli.command.is_none(), "no subcommand opens the start screen");
    }
}
