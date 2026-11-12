mod presenter;
mod viewer;
#[cfg(feature = "gui")]
mod gui;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "mirror", version, about = "Low-latency screen mirroring over LAN")]
struct Cli {
    /// Directory for identity, pairing and portal state (default: ~/.config/mirror).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
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
    /// mirror:// URI from the presenter's QR code. Without it, the GUI shows discovered presenters.
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mdns_sd=warn,wgpu=warn,naga=warn,eframe=warn".into()),
        )
        .init();
    mirror_media::init()?;
    let cli = Cli::parse();
    let config_dir = mirror_net::config_dir(cli.config_dir.as_deref());

    match cli.command {
        Command::Present(args) => presenter::run(args, &config_dir),
        Command::View(args) if args.headless => viewer::run_headless(args, &config_dir),
        Command::View(args) => run_gui(args, &config_dir),
        Command::Discover { seconds } => {
            let found = mirror_net::discovery::Browser::start()?.browse_for(Duration::from_secs_f64(seconds));
            if found.is_empty() {
                println!("no presenters found");
            }
            for p in found {
                println!(
                    "{}  {}  {}  {}",
                    p.name,
                    p.addr,
                    p.resolution.unwrap_or_default(),
                    mirror_net::identity::short_fingerprint(&p.fingerprint)
                );
            }
            Ok(())
        }
    }
}

#[cfg(feature = "gui")]
fn run_gui(args: ViewArgs, config_dir: &std::path::Path) -> Result<()> {
    gui::run(args, config_dir)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_args: ViewArgs, _config_dir: &std::path::Path) -> Result<()> {
    anyhow::bail!("built without the gui feature; use --headless")
}

pub fn default_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "mirror".into())
}
