//! Connection diagnostics shared by the presenter and the viewer GUI.
//!
//! Stages, timestamps and short advice strings are meant to be shown in the
//! window so a user can photograph them; they are also written to the log.

use crate::pairing::ConnectInfo;
use mirrorwire_proto::DEFAULT_PORT;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the viewer waits for a QUIC handshake with one address.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(7);

const MAX_ATTEMPTS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Active,
    Ok,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStepId {
    Resolve,
    Handshake,
    Cert,
    Pairing,
    Control,
    Decoder,
    FirstDatagram,
    FirstFrame,
}

impl ConnectStepId {
    pub fn all() -> &'static [Self] {
        &[
            Self::Resolve,
            Self::Handshake,
            Self::Cert,
            Self::Pairing,
            Self::Control,
            Self::Decoder,
            Self::FirstDatagram,
            Self::FirstFrame,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Resolve => "Target address",
            Self::Handshake => "QUIC handshake",
            Self::Cert => "Certificate fingerprint",
            Self::Pairing => "Pairing",
            Self::Control => "Control stream",
            Self::Decoder => "Decoder",
            Self::FirstDatagram => "First datagram",
            Self::FirstFrame => "First frame decoded",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectStep {
    pub id: ConnectStepId,
    pub state: StepState,
    pub at: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct ConnectProgress {
    pub steps: Vec<ConnectStep>,
    pub error: Option<String>,
    pub advice: Option<String>,
}

impl ConnectProgress {
    pub fn new() -> Self {
        Self {
            steps: ConnectStepId::all()
                .iter()
                .map(|id| ConnectStep {
                    id: *id,
                    state: StepState::Pending,
                    at: None,
                    detail: String::new(),
                })
                .collect(),
            error: None,
            advice: None,
        }
    }
}

impl Default for ConnectProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe progress the GUI can poll while a connect attempt runs.
#[derive(Clone)]
pub struct ConnectTrace {
    inner: Arc<Mutex<ConnectProgress>>,
}

impl Default for ConnectTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectTrace {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConnectProgress::new())),
        }
    }

    pub fn snapshot(&self) -> ConnectProgress {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn start(&self, id: ConnectStepId, detail: impl Into<String>) {
        let detail = detail.into();
        tracing::info!("connect: {} started: {detail}", id.label());
        self.set(id, StepState::Active, detail);
    }

    pub fn ok(&self, id: ConnectStepId, detail: impl Into<String>) {
        let detail = detail.into();
        tracing::info!("connect: {} ok: {detail}", id.label());
        self.set(id, StepState::Ok, detail);
    }

    pub fn fail(&self, id: ConnectStepId, detail: impl Into<String>) {
        let detail = detail.into();
        tracing::warn!("connect: {} failed: {detail}", id.label());
        self.set(id, StepState::Failed, detail);
    }

    pub fn set_error(&self, error: impl Into<String>, advice: Option<String>) {
        let error = error.into();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.error = Some(error);
        g.advice = advice;
    }

    fn set(&self, id: ConnectStepId, state: StepState, detail: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(step) = g.steps.iter_mut().find(|s| s.id == id) {
            step.state = state;
            step.at = Some(local_stamp());
            step.detail = detail;
        }
    }
}

/// An incoming viewer seen by the presenter, successful or not.
#[derive(Debug, Clone)]
pub struct IncomingAttempt {
    pub at: String,
    pub peer: String,
    pub viewer_name: Option<String>,
    pub ok: bool,
    pub outcome: String,
}

impl IncomingAttempt {
    pub fn new(peer: impl fmt::Display, viewer_name: Option<String>, ok: bool, outcome: impl Into<String>) -> Self {
        Self {
            at: local_stamp(),
            peer: peer.to_string(),
            viewer_name,
            ok,
            outcome: outcome.into(),
        }
    }
}

pub fn push_attempt(log: &Arc<Mutex<Vec<IncomingAttempt>>>, attempt: IncomingAttempt) {
    tracing::info!(
        "presenter attempt from {}{}: {}",
        attempt.peer,
        attempt
            .viewer_name
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default(),
        attempt.outcome
    );
    let mut g = log.lock().unwrap_or_else(|e| e.into_inner());
    g.push(attempt);
    while g.len() > MAX_ATTEMPTS {
        g.remove(0);
    }
}

/// Human-readable advice when QUIC never gets a reply, even though mDNS worked.
pub fn no_answer_message(addrs: &[SocketAddr], cause: Option<&str>) -> String {
    let port = addrs.first().map(|a| a.port()).unwrap_or(DEFAULT_PORT);
    let targets = if addrs.is_empty() {
        format!("the presenter (UDP {port})")
    } else {
        addrs
            .iter()
            .map(|a| format!("{a} (UDP)"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let cause = match cause.map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) if !c.ends_with('.') => format!("{c}. "),
        Some(c) => format!("{c} "),
        None => String::new(),
    };
    format!(
        "No answer from {targets}. {cause}The presenter is visible via mDNS but QUIC packets aren't getting through. Probably a firewall on the presenter. On Fedora: sudo firewall-cmd --add-port={port}/udp"
    )
}

/// True when the error is a pairing/fingerprint/protocol rejection, not a network failure.
pub fn is_logical_reject(err: &anyhow::Error) -> bool {
    let s = format!("{err:#}").to_ascii_lowercase();
    s.contains("fingerprint")
        || s.contains("rejected")
        || s.contains("pairing")
        || s.contains("protocol")
        || s.contains("unsupported")
}

/// True when nothing useful came back from the far end (timeout, reset, …).
pub fn is_unreachable(err: &anyhow::Error) -> bool {
    let s = format!("{err:#}").to_ascii_lowercase();
    s.contains("timed out")
        || s.contains("timeout")
        || s.contains("reset by peer")
        || s.contains("connection reset")
        || s.contains("no answer from")
}

/// Format every address a viewer will try, in order.
pub fn format_candidates(target: &ConnectInfo) -> String {
    target
        .candidates()
        .into_iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Local wall-clock stamp for the GUI and logs (`HH:MM:SS.mmm`).
pub fn local_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let ms = now.subsec_millis();
    match local_hms(secs) {
        Some((h, m, s)) => format!("{h:02}:{m:02}:{s:02}.{ms:03}"),
        None => format!("{secs}.{ms:03}"),
    }
}

#[cfg(unix)]
fn local_hms(secs: i64) -> Option<(i32, i32, i32)> {
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const core::ffi::c_char,
    }
    extern "C" {
        fn localtime_r(timep: *const i64, result: *mut Tm) -> *mut Tm;
    }
    let mut tm = unsafe { std::mem::zeroed::<Tm>() };
    let ptr = unsafe { localtime_r(&secs, &mut tm) };
    if ptr.is_null() {
        None
    } else {
        Some((tm.tm_hour, tm.tm_min, tm.tm_sec))
    }
}

#[cfg(not(unix))]
fn local_hms(_secs: i64) -> Option<(i32, i32, i32)> {
    None
}

/// Result of a best-effort firewalld probe on Linux. Always safe to show.
#[derive(Debug, Clone)]
pub struct FirewallHint {
    /// `true` when we should draw the hint in the presenter window.
    pub show: bool,
    /// `true` when we know firewalld is running and the port is not open.
    pub prominent: bool,
    pub message: String,
    pub command: String,
}

impl FirewallHint {
    pub fn for_port(port: u16) -> Self {
        let command = format!("sudo firewall-cmd --add-port={port}/udp");
        let static_msg = format!(
            "If a viewer finds this computer but cannot connect, the firewall may be blocking UDP {port}. On Fedora: {command}"
        );

        #[cfg(not(target_os = "linux"))]
        #[allow(clippy::needless_return)] // only "needless" when the linux block is compiled out
        {
            let _ = static_msg;
            return Self {
                show: false,
                prominent: false,
                message: String::new(),
                command,
            };
        }

        #[cfg(target_os = "linux")]
        {
            match firewalld_state() {
                Some(state) if state.eq_ignore_ascii_case("running") => {
                    match firewalld_port_open(port) {
                        Some(true) => Self {
                            show: false,
                            prominent: false,
                            message: format!("firewalld is running; UDP {port} is already open."),
                            command,
                        },
                        Some(false) => Self {
                            show: true,
                            prominent: true,
                            message: format!(
                                "firewalld is running and UDP {port} is not open. Viewers can discover this computer but QUIC will time out until you run:\n{command}"
                            ),
                            command,
                        },
                        None => Self {
                            show: true,
                            prominent: true,
                            message: format!(
                                "firewalld is running. If a viewer cannot connect, open UDP {port} with:\n{command}"
                            ),
                            command,
                        },
                    }
                }
                Some(_) => Self {
                    show: false,
                    prominent: false,
                    message: "firewalld is not running.".to_owned(),
                    command,
                },
                None => Self {
                    show: true,
                    prominent: false,
                    message: static_msg,
                    command,
                },
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn firewalld_state() -> Option<String> {
    let out = run_maybe_host("firewall-cmd", &["--state"])?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if !text.is_empty() {
        return Some(text.to_owned());
    }
    if dbus_firewalld_running() {
        Some("running".into())
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn firewalld_port_open(port: u16) -> Option<bool> {
    let arg = format!("--query-port={port}/udp");
    let out = run_maybe_host("firewall-cmd", &[&arg])?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
    if text == "yes" || out.status.success() && text.is_empty() {
        Some(true)
    } else if text == "no" || !out.status.success() {
        Some(false)
    } else {
        None
    }
}

/// Run a command, falling back to `flatpak-spawn --host` when we are in a sandbox
/// that has that portal. No extra Flatpak permission is requested; the spawn
/// simply fails and we treat detection as unavailable.
#[cfg(target_os = "linux")]
fn run_maybe_host(program: &str, args: &[&str]) -> Option<std::process::Output> {
    if let Ok(out) = std::process::Command::new(program).args(args).output() {
        return Some(out);
    }
    let mut cmd = std::process::Command::new("flatpak-spawn");
    cmd.arg("--host").arg(program).args(args);
    cmd.output().ok()
}

#[cfg(target_os = "linux")]
fn dbus_firewalld_running() -> bool {
    let attempts = [
        std::process::Command::new("busctl")
            .args([
                "--system",
                "call",
                "org.fedoraproject.FirewallD1",
                "/org/fedoraproject/FirewallD1",
                "org.fedoraproject.FirewallD1",
                "getDefaultZone",
            ])
            .output(),
        std::process::Command::new("flatpak-spawn")
            .args([
                "--host",
                "busctl",
                "--system",
                "call",
                "org.fedoraproject.FirewallD1",
                "/org/fedoraproject/FirewallD1",
                "org.fedoraproject.FirewallD1",
                "getDefaultZone",
            ])
            .output(),
    ];
    attempts.into_iter().any(|r| r.map(|o| o.status.success()).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn timeout_message_mentions_udp_and_firewall_cmd() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 201, 132)), 47_800);
        let msg = no_answer_message(&[addr], None);
        assert!(msg.contains("192.168.201.132:47800 (UDP)"), "{msg}");
        assert!(msg.contains("sudo firewall-cmd --add-port=47800/udp"), "{msg}");
        assert!(msg.contains("mDNS"), "{msg}");
        assert!(msg.contains("QUIC"), "{msg}");
    }

    #[test]
    fn timeout_message_lists_every_address() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 47_800);
        let b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 47_800);
        let msg = no_answer_message(&[a, b], Some("timed out after 7s"));
        assert!(msg.contains("192.168.1.2:47800 (UDP)"), "{msg}");
        assert!(msg.contains("10.0.0.5:47800 (UDP)"), "{msg}");
        assert!(msg.contains("timed out after 7s"), "{msg}");
    }

    #[test]
    fn logical_reject_is_not_a_firewall_error() {
        let err = anyhow::anyhow!("presenter rejected connection: not paired: pairing code required");
        assert!(is_logical_reject(&err));
        assert!(!is_unreachable(&err));
        let err = anyhow::anyhow!("connecting to 192.168.1.2:47800: timed out");
        assert!(!is_logical_reject(&err));
        assert!(is_unreachable(&err));
    }

    #[test]
    fn local_stamp_has_expected_shape() {
        let s = local_stamp();
        let parts: Vec<_> = s.split([':', '.']).collect();
        assert_eq!(parts.len(), 4, "{s}");
        assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())), "{s}");
    }
}
