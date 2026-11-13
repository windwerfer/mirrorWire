//! Small persisted presenter settings (`settings.json` in the config dir).

use anyhow::{Context, Result};
use mirrorwire_proto::DEFAULT_PORT;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Presenter preferences that survive a restart.
///
/// Unknown fields are ignored so older builds can read a newer file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// UDP port the presenter binds and advertises. `None` means the default
    /// (`DEFAULT_PORT`, 47800). Port `0` is not stored (it is only useful as a
    /// one-shot "pick an ephemeral port" CLI choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Presenter cursor mode: `separate`, `embedded` or `hidden`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Presenter frame-rate cap: `30`, `60` (or another number) or `max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<String>,
}

impl Settings {
    pub fn path(dir: &Path) -> std::path::PathBuf {
        dir.join("settings.json")
    }

    pub fn load(dir: &Path) -> Self {
        let path = Self::path(dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = Self::path(dir);
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Persist the presenter's cursor mode and frame-rate cap for the next start.
pub fn save_capture(config_dir: &Path, cursor: &str, fps: &str) -> Result<()> {
    let mut settings = Settings::load(config_dir);
    if settings.cursor.as_deref() == Some(cursor) && settings.fps.as_deref() == Some(fps) {
        return Ok(());
    }
    settings.cursor = Some(cursor.to_owned());
    settings.fps = Some(fps.to_owned());
    settings.save(config_dir)
}

/// `MIRRORWIRE_PORT` when it is set to a valid `u16` (including `0`).
pub fn port_from_env() -> Option<u16> {
    let raw = std::env::var("MIRRORWIRE_PORT").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<u16>() {
        Ok(port) => Some(port),
        Err(_) => {
            tracing::warn!("invalid MIRRORWIRE_PORT={raw:?}, ignoring");
            None
        }
    }
}

/// Resolve the presenter UDP port.
///
/// Precedence: `cli` (including `--port 0` for an ephemeral bind), then
/// `MIRRORWIRE_PORT`, then `settings.json`, then [`DEFAULT_PORT`].
pub fn resolve_port(cli: Option<u16>, config_dir: &Path) -> u16 {
    if let Some(port) = cli {
        return port;
    }
    if let Some(port) = port_from_env() {
        return port;
    }
    match Settings::load(config_dir).port {
        Some(0) => {
            tracing::warn!("ignoring saved port 0; using the default {DEFAULT_PORT}");
            DEFAULT_PORT
        }
        Some(port) => port,
        None => DEFAULT_PORT,
    }
}

/// Persist a UI-chosen port for the next start (not `0`).
pub fn save_port(config_dir: &Path, port: u16) -> Result<()> {
    if port == 0 {
        anyhow::bail!("port 0 is not saved; it only means 'pick an ephemeral port' for this run");
    }
    let mut settings = Settings::load(config_dir);
    if settings.port == Some(port) {
        return Ok(());
    }
    settings.port = Some(port);
    settings.save(config_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env mutations in this module must be serial: cargo can run tests in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mirrorwire-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("MIRRORWIRE_PORT").ok();
        match value {
            Some(v) => std::env::set_var("MIRRORWIRE_PORT", v),
            None => std::env::remove_var("MIRRORWIRE_PORT"),
        }
        let result = f();
        match prev {
            Some(v) => std::env::set_var("MIRRORWIRE_PORT", v),
            None => std::env::remove_var("MIRRORWIRE_PORT"),
        }
        result
    }

    #[test]
    fn missing_file_is_default() {
        let dir = temp_dir();
        assert_eq!(Settings::load(&dir), Settings::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = temp_dir();
        let settings = Settings { port: Some(47901), cursor: Some("embedded".into()), fps: Some("max".into()) };
        settings.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), settings);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignores_unknown_fields() {
        let dir = temp_dir();
        std::fs::write(Settings::path(&dir), r#"{"port": 47001, "future": true}"#).unwrap();
        assert_eq!(Settings::load(&dir).port, Some(47001));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn capture_settings_keep_the_port() {
        let dir = temp_dir();
        save_port(&dir, 47555).unwrap();
        save_capture(&dir, "separate", "60").unwrap();
        let s = Settings::load(&dir);
        assert_eq!((s.port, s.cursor.as_deref(), s.fps.as_deref()), (Some(47555), Some("separate"), Some("60")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_precedence() {
        let dir = temp_dir();
        save_port(&dir, 47100).unwrap();
        with_env(None, || {
            assert_eq!(resolve_port(Some(0), &dir), 0);
            assert_eq!(resolve_port(Some(47200), &dir), 47200);
        });
        with_env(Some("47300"), || {
            assert_eq!(resolve_port(None, &dir), 47300);
            assert_eq!(resolve_port(Some(47400), &dir), 47400);
        });
        with_env(None, || {
            assert_eq!(resolve_port(None, &dir), 47100);
        });
        with_env(Some("not-a-port"), || {
            assert_eq!(resolve_port(None, &dir), 47100);
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_default_when_nothing_is_set() {
        let dir = temp_dir();
        with_env(None, || {
            assert_eq!(resolve_port(None, &dir), DEFAULT_PORT);
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saved_zero_is_ignored() {
        let dir = temp_dir();
        Settings { port: Some(0), ..Default::default() }.save(&dir).unwrap();
        with_env(None, || {
            assert_eq!(resolve_port(None, &dir), DEFAULT_PORT);
        });
        std::fs::remove_dir_all(&dir).ok();
    }
}
