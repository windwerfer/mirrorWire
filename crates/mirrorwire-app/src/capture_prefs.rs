//! Presenter capture preferences: how the cursor is sent and the frame-rate
//! cap. Resolved like the port: CLI flag, then environment
//! (`MIRRORWIRE_CURSOR`, `MIRRORWIRE_FPS`), then `settings.json`, then the
//! default (separate cursor, 60 fps).

use std::path::Path;

/// How the presenter delivers the mouse cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CursorPref {
    /// Cursor sent as coordinates and drawn by the viewer (lower latency).
    /// Falls back to `embedded` when the portal or the viewer can't do it.
    Separate,
    /// Cursor drawn into the video by the compositor (the old behaviour).
    Embedded,
    /// No cursor at all.
    Hidden,
}

impl CursorPref {
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn as_str(self) -> &'static str {
        match self {
            CursorPref::Separate => "separate",
            CursorPref::Embedded => "embedded",
            CursorPref::Hidden => "hidden",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "separate" | "metadata" | "coordinates" => Some(CursorPref::Separate),
            "embedded" | "video" => Some(CursorPref::Embedded),
            "hidden" | "none" | "off" => Some(CursorPref::Hidden),
            _ => None,
        }
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn all() -> [CursorPref; 3] {
        [
            CursorPref::Separate,
            CursorPref::Embedded,
            CursorPref::Hidden,
        ]
    }
}

/// Highest frame rate the presenter asks the compositor for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpsPref {
    Fixed(u32),
    /// Whatever the compositor allows (usually the display refresh rate).
    Max,
}

/// Frame rate requested for `max`; the compositor caps it at what it can do.
pub const MAX_FPS: u32 = 240;

impl FpsPref {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        if s == "max" {
            return Some(FpsPref::Max);
        }
        match s.parse::<u32>() {
            Ok(n) if (1..=MAX_FPS).contains(&n) => Some(FpsPref::Fixed(n)),
            _ => None,
        }
    }

    /// The `maxFramerate` asked of PipeWire.
    pub fn request(self) -> u32 {
        match self {
            FpsPref::Fixed(n) => n,
            FpsPref::Max => MAX_FPS,
        }
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn label(self) -> String {
        match self {
            FpsPref::Fixed(n) => n.to_string(),
            FpsPref::Max => "max".into(),
        }
    }
}

/// clap value parser for `--fps`.
pub fn parse_fps_arg(s: &str) -> Result<FpsPref, String> {
    FpsPref::parse(s)
        .ok_or_else(|| format!("expected 30, 60, another number 1–{MAX_FPS}, or max; got {s:?}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturePrefs {
    pub cursor: CursorPref,
    pub fps: FpsPref,
}

impl Default for CapturePrefs {
    fn default() -> Self {
        Self {
            cursor: CursorPref::Separate,
            fps: FpsPref::Fixed(60),
        }
    }
}

/// Precedence without side effects (testable): CLI, env, saved, default.
fn pick<T>(
    cli: Option<T>,
    env: Option<&str>,
    saved: Option<&str>,
    parse: impl Fn(&str) -> Option<T>,
    name: &str,
    default: T,
) -> T {
    if let Some(v) = cli {
        return v;
    }
    if let Some(raw) = env.filter(|r| !r.trim().is_empty()) {
        match parse(raw) {
            Some(v) => return v,
            None => tracing::warn!("invalid {name}={raw:?}, ignoring"),
        }
    }
    if let Some(raw) = saved {
        match parse(raw) {
            Some(v) => return v,
            None => tracing::warn!("ignoring unreadable saved {name} {raw:?}"),
        }
    }
    default
}

pub fn resolve_with(
    cli_cursor: Option<CursorPref>,
    cli_fps: Option<FpsPref>,
    env_cursor: Option<&str>,
    env_fps: Option<&str>,
    saved_cursor: Option<&str>,
    saved_fps: Option<&str>,
) -> CapturePrefs {
    let d = CapturePrefs::default();
    CapturePrefs {
        cursor: pick(
            cli_cursor,
            env_cursor,
            saved_cursor,
            CursorPref::parse,
            "MIRRORWIRE_CURSOR",
            d.cursor,
        ),
        fps: pick(
            cli_fps,
            env_fps,
            saved_fps,
            FpsPref::parse,
            "MIRRORWIRE_FPS",
            d.fps,
        ),
    }
}

pub fn resolve(
    cli_cursor: Option<CursorPref>,
    cli_fps: Option<FpsPref>,
    config_dir: &Path,
) -> CapturePrefs {
    let saved = mirrorwire_net::settings::Settings::load(config_dir);
    let env_cursor = std::env::var("MIRRORWIRE_CURSOR").ok();
    let env_fps = std::env::var("MIRRORWIRE_FPS").ok();
    resolve_with(
        cli_cursor,
        cli_fps,
        env_cursor.as_deref(),
        env_fps.as_deref(),
        saved.cursor.as_deref(),
        saved.fps.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values() {
        assert_eq!(FpsPref::parse("60"), Some(FpsPref::Fixed(60)));
        assert_eq!(FpsPref::parse(" MAX "), Some(FpsPref::Max));
        assert_eq!(FpsPref::parse("0"), None);
        assert_eq!(FpsPref::parse("999"), None);
        assert_eq!(FpsPref::Max.request(), MAX_FPS);
        assert_eq!(CursorPref::parse("Embedded"), Some(CursorPref::Embedded));
        assert_eq!(CursorPref::parse("separate"), Some(CursorPref::Separate));
        assert_eq!(CursorPref::parse("sideways"), None);
        for c in CursorPref::all() {
            assert_eq!(CursorPref::parse(c.as_str()), Some(c));
        }
    }

    #[test]
    fn precedence_cli_env_saved_default() {
        assert_eq!(
            resolve_with(None, None, None, None, None, None),
            CapturePrefs::default()
        );
        let saved = resolve_with(None, None, None, None, Some("embedded"), Some("30"));
        assert_eq!(
            saved,
            CapturePrefs {
                cursor: CursorPref::Embedded,
                fps: FpsPref::Fixed(30)
            }
        );
        let env = resolve_with(
            None,
            None,
            Some("hidden"),
            Some("max"),
            Some("embedded"),
            Some("30"),
        );
        assert_eq!(
            env,
            CapturePrefs {
                cursor: CursorPref::Hidden,
                fps: FpsPref::Max
            }
        );
        let cli = resolve_with(
            Some(CursorPref::Separate),
            Some(FpsPref::Fixed(60)),
            Some("hidden"),
            Some("max"),
            None,
            None,
        );
        assert_eq!(cli, CapturePrefs::default());
        // Junk falls through to the next source.
        let junk = resolve_with(
            None,
            None,
            Some("nope"),
            Some("fast"),
            Some("embedded"),
            None,
        );
        assert_eq!(
            junk,
            CapturePrefs {
                cursor: CursorPref::Embedded,
                fps: FpsPref::Fixed(60)
            }
        );
    }
}
