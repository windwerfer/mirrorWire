//! Process log: stderr, a file the user can photograph or attach, and a
//! ring buffer the GUI can show and copy.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

const MAX_LINES: usize = 200;

static LOG: OnceLock<LogStore> = OnceLock::new();

#[derive(Clone)]
pub struct LogStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    lines: VecDeque<String>,
    pending: Vec<u8>,
    file: Option<File>,
    path: PathBuf,
    open_error: Option<String>,
}

impl LogStore {
    fn open(path: PathBuf) -> Self {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let (file, open_error) = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => (Some(f), None),
            Err(e) => (
                None,
                Some(format!("could not open log file {}: {e}", path.display())),
            ),
        };
        let store = Self {
            inner: Arc::new(Mutex::new(Inner {
                lines: VecDeque::new(),
                pending: Vec::new(),
                file,
                path,
                open_error,
            })),
        };
        store.push_line(format!(
            "---- session {} ----",
            mirrorwire_net::diag::local_stamp()
        ));
        store
    }

    pub fn path(&self) -> PathBuf {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).path.clone()
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn open_error(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open_error
            .clone()
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn snapshot(&self) -> String {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        for line in &g.lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    fn push_line(&self, line: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        write_and_keep(&mut g, line);
    }

    fn write_bytes(&self, buf: &[u8]) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.pending.extend_from_slice(buf);
        while let Some(pos) = g.pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = g.pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw).trim_end().to_string();
            if !line.is_empty() {
                write_and_keep(&mut g, line);
            }
        }
    }
}

fn write_and_keep(g: &mut Inner, line: String) {
    if let Some(file) = g.file.as_mut() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
    g.lines.push_back(line);
    while g.lines.len() > MAX_LINES {
        g.lines.pop_front();
    }
}

struct TeeWriter {
    store: LogStore,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        self.store.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        Ok(())
    }
}

/// Install the process subscriber (stderr + file + ring buffer) and return the store.
pub fn init() -> LogStore {
    let store = LogStore::open(default_log_file());
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "info,mdns_sd=warn,wgpu=warn,naga=warn,eframe=warn".into()
    });
    let writer = store.clone();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(stderr_is_tty())
        .with_timer(LocalTimer)
        .with_writer(move || TeeWriter { store: writer.clone() })
        .try_init();
    let _ = LOG.set(store.clone());
    if let Some(err) = store.open_error() {
        tracing::warn!("{err}");
    }
    tracing::info!("logging to {}", store.path().display());
    tracing::info!("mirrorwire {} on {}", env!("CARGO_PKG_VERSION"), crate::session::session_from_env());
    store
}

pub fn default_log_file() -> PathBuf {
    log_dir().join("mirrorwire.log")
}

pub fn log_dir() -> PathBuf {
    if let Ok(p) = std::env::var("MIRRORWIRE_LOG_DIR") {
        return PathBuf::from(p);
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .map(|h| h.join("Library/Logs/MirrorWire"))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .map(|d| d.join("mirrorwire"))
            .unwrap_or_else(|| PathBuf::from(".mirrorwire"))
    }
}

fn stderr_is_tty() -> bool {
    #[cfg(unix)]
    unsafe {
        libc::isatty(libc::STDERR_FILENO) != 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

struct LocalTimer;

impl tracing_subscriber::fmt::time::FormatTime for LocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", mirrorwire_net::diag::local_stamp())
    }
}
