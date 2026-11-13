//! Firewall detection and a best-effort open-port request for the presenter.
//!
//! On Linux the presenter talks to firewalld over the system bus when it can
//! (Flatpak: `--system-talk-name=org.fedoraproject.FirewallD1`). Opening a port
//! is a polkit-authenticated D-Bus call: the app itself never runs as root.
//! If firewalld is not available, a short general guide is shown instead.

use mirrorwire_proto::DEFAULT_PORT;
use std::fmt;

/// firewalld service name that matches `packaging/linux/mirrorwire.xml`.
pub const FIREWALLD_SERVICE: &str = "mirrorwire";

/// Result of a best-effort firewall probe. Always safe to show.
#[derive(Debug, Clone)]
pub struct FirewallHint {
    /// `true` when we should draw the hint in the presenter window.
    pub show: bool,
    /// `true` when we know a firewall is active and the port is not open.
    pub prominent: bool,
    /// Situation-specific sentence (detected firewall, already open, …).
    pub message: String,
    /// Short general how-to, the same text as in the README.
    pub guide: String,
    /// Primary command to copy (firewalld or ufw, for the actual port).
    pub command: String,
    /// `true` when the Open-port button can ask firewalld over D-Bus.
    pub can_autofix: bool,
    pub port: u16,
}

/// The short general guide shown in the presenter, the viewer timeout, and the README.
pub fn guide_text(port: u16) -> String {
    format!(
        "On Fedora: {fedora}\n\
         To keep it after a reboot: {permanent} && sudo firewall-cmd --reload\n\
         Ubuntu with ufw: {ufw}\n\
         If you use another Linux distribution, look up how to allow a UDP port in its firewall.",
        fedora = fedora_command(port),
        permanent = fedora_permanent_command(port),
        ufw = ufw_command(port),
    )
}

pub fn fedora_command(port: u16) -> String {
    format!("sudo firewall-cmd --add-port={port}/udp")
}

pub fn fedora_permanent_command(port: u16) -> String {
    format!("sudo firewall-cmd --permanent --add-port={port}/udp")
}

pub fn ufw_command(port: u16) -> String {
    format!("sudo ufw allow {port}/udp")
}

pub fn service_command() -> &'static str {
    "sudo firewall-cmd --permanent --add-service=mirrorwire && sudo firewall-cmd --reload"
}

impl FirewallHint {
    pub fn for_port(port: u16) -> Self {
        let command = fedora_command(port);
        let guide = guide_text(port);

        #[cfg(not(target_os = "linux"))]
        {
            let _ = &guide;
            return Self {
                show: false,
                prominent: false,
                message: String::new(),
                guide,
                command,
                can_autofix: false,
                port,
            };
        }

        #[cfg(target_os = "linux")]
        {
            probe_linux(port, command, guide)
        }
    }

    /// Ask firewalld (via D-Bus, polkit prompt) to open the port at runtime
    /// and permanently. The app does not run as root.
    pub fn open_port(port: u16) -> Result<String, String> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = port;
            Err("Opening the firewall from the app is only available on Linux.".into())
        }
        #[cfg(target_os = "linux")]
        {
            open_port_linux(port)
        }
    }
}

#[cfg(target_os = "linux")]
fn intro(port: u16) -> String {
    format!(
        "If a viewer finds this computer but cannot connect, the firewall may be blocking UDP {port}."
    )
}

#[cfg(target_os = "linux")]
fn probe_linux(port: u16, command: String, guide: String) -> FirewallHint {
    match probe_firewalld(port) {
        Some(Probe {
            running: true,
            port_open: Some(true),
            can_autofix,
        }) => FirewallHint {
            show: false,
            prominent: false,
            message: format!("firewalld is running; UDP {port} is already open."),
            guide,
            command,
            can_autofix,
            port,
        },
        Some(Probe {
            running: true,
            port_open: Some(false),
            can_autofix,
        }) => FirewallHint {
            show: true,
            prominent: true,
            message: format!(
                "firewalld is running and UDP {port} is not open. Viewers can discover this computer but the connection will time out until the port is opened."
            ),
            guide,
            command,
            can_autofix,
            port,
        },
        Some(Probe {
            running: true,
            port_open: None,
            can_autofix,
        }) => FirewallHint {
            show: true,
            prominent: true,
            message: format!(
                "firewalld is running. If a viewer cannot connect, UDP {port} may be blocked."
            ),
            guide,
            command,
            can_autofix,
            port,
        },
        Some(Probe { running: false, .. }) | None => {
            if let Some(ufw) = probe_ufw(port, UfwPaths::system()) {
                return ufw_hint(port, ufw, guide);
            }
            FirewallHint {
                show: true,
                prominent: false,
                message: intro(port),
                guide,
                command,
                can_autofix: false,
                port,
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn ufw_hint(port: u16, state: UfwState, guide: String) -> FirewallHint {
    let command = ufw_command(port);
    match state {
        UfwState::Inactive => FirewallHint {
            show: true,
            prominent: false,
            message: intro(port),
            guide,
            command: fedora_command(port),
            can_autofix: false,
            port,
        },
        UfwState::Open => FirewallHint {
            show: false,
            prominent: false,
            message: format!("ufw is active; UDP {port} is already allowed."),
            guide,
            command,
            can_autofix: false,
            port,
        },
        UfwState::Blocked => FirewallHint {
            show: true,
            prominent: true,
            message: format!(
                "ufw is active and UDP {port} is not allowed. Viewers can discover this computer but the connection will time out until the port is opened."
            ),
            guide,
            command,
            can_autofix: false,
            port,
        },
        UfwState::Unknown => FirewallHint {
            show: true,
            prominent: true,
            message: format!(
                "ufw is active. If a viewer cannot connect, allow UDP {port}."
            ),
            guide,
            command,
            can_autofix: false,
            port,
        },
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct Probe {
    running: bool,
    port_open: Option<bool>,
    can_autofix: bool,
}

#[cfg(target_os = "linux")]
fn probe_firewalld(port: u16) -> Option<Probe> {
    if let Some(probe) = dbus_probe(port) {
        return Some(probe);
    }
    // Native installs (and rare cases where D-Bus is blocked but firewall-cmd
    // is on PATH). In a Flatpak this usually fails; that's fine.
    cmd_probe(port)
}

#[cfg(target_os = "linux")]
fn dbus_probe(port: u16) -> Option<Probe> {
    match block_on_dbus(async move { dbus_probe_async(port).await }) {
        Ok(probe) => Some(probe),
        Err(e) => {
            tracing::debug!("firewalld D-Bus probe failed: {e}");
            None
        }
    }
}

#[cfg(target_os = "linux")]
async fn dbus_probe_async(port: u16) -> Result<Probe, String> {
    let conn = system_connection().await?;
    let zones = active_or_default_zones(&conn).await?;
    let open = port_open_in_zones(&conn, &zones, port).await;
    Ok(Probe {
        running: true,
        port_open: open,
        can_autofix: true,
    })
}

#[cfg(target_os = "linux")]
fn cmd_probe(port: u16) -> Option<Probe> {
    let state = run_maybe_host("firewall-cmd", &["--state"])?;
    let text = String::from_utf8_lossy(&state.stdout);
    let text = text.trim();
    if text.is_empty() && !state.status.success() {
        return None;
    }
    if !text.eq_ignore_ascii_case("running") && !state.status.success() {
        return Some(Probe {
            running: false,
            port_open: None,
            can_autofix: false,
        });
    }
    if !text.eq_ignore_ascii_case("running") && text.is_empty() {
        return Some(Probe {
            running: false,
            port_open: None,
            can_autofix: false,
        });
    }
    if !text.is_empty() && !text.eq_ignore_ascii_case("running") {
        return Some(Probe {
            running: false,
            port_open: None,
            can_autofix: false,
        });
    }
    let open = cmd_port_open(port);
    Some(Probe {
        running: true,
        port_open: open,
        can_autofix: false,
    })
}

#[cfg(target_os = "linux")]
fn cmd_port_open(port: u16) -> Option<bool> {
    let arg = format!("--query-port={port}/udp");
    let out = run_maybe_host("firewall-cmd", &[&arg])?;
    let text = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_ascii_lowercase();
    if text == "yes" || (out.status.success() && text.is_empty()) {
        Some(true)
    } else if text == "no" || !out.status.success() {
        Some(false)
    } else {
        None
    }
}

/// Run a command, falling back to `flatpak-spawn --host` when that portal exists.
/// No extra Flatpak permission is requested; the spawn simply fails.
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
fn open_port_linux(port: u16) -> Result<String, String> {
    block_on_dbus(async move { open_port_async(port).await })
}

#[cfg(target_os = "linux")]
async fn open_port_async(port: u16) -> Result<String, String> {
    let conn = system_connection().await?;
    let zones = active_or_default_zones(&conn).await?;
    if zones.is_empty() {
        return Err("firewalld is running but no zone was found to add the port to.".into());
    }

    let mut runtime_ok = 0usize;
    let mut permanent_ok = 0usize;
    let mut last_err: Option<String> = None;
    for zone in &zones {
        match zone_add_port_runtime(&conn, zone, port).await {
            Ok(()) => runtime_ok += 1,
            Err(e) => last_err = Some(e),
        }
        match zone_add_port_permanent(&conn, zone, port).await {
            Ok(()) => permanent_ok += 1,
            Err(e) => last_err = Some(e),
        }
        if port == DEFAULT_PORT {
            let _ = zone_add_service_runtime(&conn, zone).await;
            let _ = zone_add_service_permanent(&conn, zone).await;
        }
    }

    if runtime_ok == 0 && permanent_ok == 0 {
        return Err(last_err.unwrap_or_else(|| {
            "firewalld did not accept the request. A password prompt may have been cancelled."
                .into()
        }));
    }
    if runtime_ok > 0 && permanent_ok > 0 {
        Ok(format!(
            "Opened UDP {port} in firewalld (now, and after a reboot)."
        ))
    } else if runtime_ok > 0 {
        Ok(format!(
            "Opened UDP {port} in firewalld until the next reboot. To keep it: {} && sudo firewall-cmd --reload",
            fedora_permanent_command(port)
        ))
    } else {
        Ok(format!(
            "Added UDP {port} to the permanent firewalld configuration. Run sudo firewall-cmd --reload (or reopen the port) to apply it now."
        ))
    }
}

#[cfg(target_os = "linux")]
const DBUS_DEST: &str = "org.fedoraproject.FirewallD1";
#[cfg(target_os = "linux")]
const DBUS_PATH: &str = "/org/fedoraproject/FirewallD1";
#[cfg(target_os = "linux")]
const DBUS_IFACE: &str = "org.fedoraproject.FirewallD1";
#[cfg(target_os = "linux")]
const DBUS_ZONE_IFACE: &str = "org.fedoraproject.FirewallD1.zone";
#[cfg(target_os = "linux")]
const DBUS_CONFIG_IFACE: &str = "org.fedoraproject.FirewallD1.config";
#[cfg(target_os = "linux")]
const DBUS_CONFIG_PATH: &str = "/org/fedoraproject/FirewallD1/config";
#[cfg(target_os = "linux")]
const DBUS_CONFIG_ZONE_IFACE: &str = "org.fedoraproject.FirewallD1.config.zone";

#[cfg(target_os = "linux")]
async fn system_connection() -> Result<zbus::Connection, String> {
    zbus::connection::Builder::system()
        .method_timeout(std::time::Duration::from_secs(120))
        .build()
        .await
        .map_err(dbus_err)
}

#[cfg(target_os = "linux")]
async fn iface<'a>(
    conn: &'a zbus::Connection,
    path: &'a str,
    interface: &'a str,
) -> Result<zbus::Proxy<'a>, String> {
    zbus::Proxy::new(conn, DBUS_DEST, path, interface)
        .await
        .map_err(dbus_err)
}

#[cfg(target_os = "linux")]
async fn active_or_default_zones(conn: &zbus::Connection) -> Result<Vec<String>, String> {
    let proxy = iface(conn, DBUS_PATH, DBUS_IFACE).await?;
    let default_zone: String = proxy.call("getDefaultZone", &()).await.map_err(dbus_err)?;
    let active: std::collections::HashMap<String, Vec<String>> =
        match proxy.call("getActiveZones", &()).await {
            Ok(map) => map,
            Err(e) => {
                tracing::debug!("firewalld getActiveZones failed: {e}");
                Default::default()
            }
        };
    let mut zones: Vec<String> = active.into_keys().collect();
    if zones.is_empty() {
        if !default_zone.is_empty() {
            zones.push(default_zone);
        }
    } else if !default_zone.is_empty() && !zones.iter().any(|z| z == &default_zone) {
        zones.push(default_zone);
    }
    zones.sort();
    zones.dedup();
    Ok(zones)
}

#[cfg(target_os = "linux")]
async fn port_open_in_zones(conn: &zbus::Connection, zones: &[String], port: u16) -> Option<bool> {
    if zones.is_empty() {
        return None;
    }
    let mut known = false;
    let mut all_open = true;
    for zone in zones {
        match zone_allows(conn, zone, port).await {
            Some(true) => known = true,
            Some(false) => {
                known = true;
                all_open = false;
            }
            None => {}
        }
    }
    known.then_some(all_open)
}

#[cfg(target_os = "linux")]
async fn zone_allows(conn: &zbus::Connection, zone: &str, port: u16) -> Option<bool> {
    let proxy = iface(conn, DBUS_PATH, DBUS_ZONE_IFACE).await.ok()?;
    let port_s = port.to_string();
    let by_port: Option<bool> = match proxy
        .call("queryPort", &(zone, port_s.as_str(), "udp"))
        .await
    {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!("firewalld queryPort({zone}, {port}/udp) failed: {e}");
            None
        }
    };
    if by_port == Some(true) {
        return Some(true);
    }
    if port == DEFAULT_PORT {
        match proxy.call("queryService", &(zone, FIREWALLD_SERVICE)).await {
            Ok(true) => return Some(true),
            Ok(false) => {}
            Err(e) => {
                tracing::debug!("firewalld queryService({zone}, {FIREWALLD_SERVICE}) failed: {e}");
            }
        }
    }
    by_port
}

#[cfg(target_os = "linux")]
async fn zone_add_port_runtime(
    conn: &zbus::Connection,
    zone: &str,
    port: u16,
) -> Result<(), String> {
    let proxy = iface(conn, DBUS_PATH, DBUS_ZONE_IFACE).await?;
    let result: Result<(), zbus::Error> = proxy
        .call("addPort", &(zone, port.to_string(), "udp", 0i32))
        .await;
    accept_already_enabled(result)
}

#[cfg(target_os = "linux")]
async fn zone_add_service_runtime(conn: &zbus::Connection, zone: &str) -> Result<(), String> {
    let proxy = iface(conn, DBUS_PATH, DBUS_ZONE_IFACE).await?;
    let result: Result<(), zbus::Error> = proxy
        .call("addService", &(zone, FIREWALLD_SERVICE, 0i32))
        .await;
    accept_already_enabled(result)
}

#[cfg(target_os = "linux")]
async fn zone_add_port_permanent(
    conn: &zbus::Connection,
    zone: &str,
    port: u16,
) -> Result<(), String> {
    let path = config_zone_path(conn, zone).await?;
    let proxy = iface(conn, path.as_str(), DBUS_CONFIG_ZONE_IFACE).await?;
    let result: Result<(), zbus::Error> = proxy.call("addPort", &(port.to_string(), "udp")).await;
    accept_already_enabled(result)
}

#[cfg(target_os = "linux")]
async fn zone_add_service_permanent(conn: &zbus::Connection, zone: &str) -> Result<(), String> {
    let path = config_zone_path(conn, zone).await?;
    let proxy = iface(conn, path.as_str(), DBUS_CONFIG_ZONE_IFACE).await?;
    let result: Result<(), zbus::Error> = proxy.call("addService", &(FIREWALLD_SERVICE,)).await;
    accept_already_enabled(result)
}

#[cfg(target_os = "linux")]
async fn config_zone_path(conn: &zbus::Connection, zone: &str) -> Result<String, String> {
    let proxy = iface(conn, DBUS_CONFIG_PATH, DBUS_CONFIG_IFACE).await?;
    let path: zbus::OwnedObjectPath = proxy
        .call("getZoneByName", &(zone,))
        .await
        .map_err(dbus_err)?;
    Ok(path.as_str().to_owned())
}

#[cfg(target_os = "linux")]
fn accept_already_enabled(result: Result<(), zbus::Error>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if is_already_enabled(&e) => Ok(()),
        Err(e) => Err(dbus_err(e)),
    }
}

#[cfg(target_os = "linux")]
fn is_already_enabled(err: &zbus::Error) -> bool {
    let s = err.to_string();
    s.contains("ALREADY_ENABLED") || s.to_ascii_lowercase().contains("already enabled")
}

#[cfg(target_os = "linux")]
fn dbus_err(err: impl fmt::Display) -> String {
    let s = err.to_string();
    if s.to_ascii_lowercase().contains("not available")
        || s.to_ascii_lowercase()
            .contains("without a specified timeout")
    {
        return s;
    }
    s
}

/// Run a firewalld D-Bus call on a private current-thread runtime so we never
/// `block_on` the GUI's or presenter's existing tokio runtime.
#[cfg(target_os = "linux")]
fn block_on_dbus<T, F>(fut: F) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    std::thread::Builder::new()
        .name("firewalld-dbus".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime for firewalld")
                .block_on(fut)
        })
        .expect("spawn firewalld-dbus thread")
        .join()
        .expect("firewalld-dbus thread")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum UfwState {
    Inactive,
    Open,
    Blocked,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct UfwPaths<'a> {
    conf: &'a str,
    user_rules: &'a str,
}

impl UfwPaths<'static> {
    fn system() -> Self {
        Self {
            conf: "/etc/ufw/ufw.conf",
            user_rules: "/etc/ufw/user.rules",
        }
    }
}

fn probe_ufw(port: u16, paths: UfwPaths<'_>) -> Option<UfwState> {
    let conf = std::fs::read_to_string(paths.conf).ok()?;
    if !ufw_enabled(&conf) {
        return Some(UfwState::Inactive);
    }
    match std::fs::read_to_string(paths.user_rules) {
        Ok(rules) => Some(if ufw_rules_allow_udp(port, &rules) {
            UfwState::Open
        } else {
            UfwState::Blocked
        }),
        Err(_) => Some(UfwState::Unknown),
    }
}

fn ufw_enabled(conf: &str) -> bool {
    conf.lines().any(|line| {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        lower == "enabled=yes" || lower.starts_with("enabled=yes#")
    })
}

fn ufw_rules_allow_udp(port: u16, rules: &str) -> bool {
    let port_s = port.to_string();
    rules.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            return false;
        }
        let lower = line.to_ascii_lowercase();
        if !lower.contains("udp") {
            return false;
        }
        if !lower.contains("accept") && !lower.contains("allow") {
            return false;
        }
        // iptables: `-p udp --dport 47800`, ufw tuple comments, or `47800:47800`.
        lower.contains(&format!("--dport {port_s}"))
            || lower.contains(&format!("--dport {port_s}:"))
            || lower.contains(&format!(" {port_s}/udp"))
            || lower.contains(&format!("dport {port_s} "))
            || lower.contains(&format!("{port_s}:{port_s}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_command_names_the_service() {
        assert!(service_command().contains("--add-service=mirrorwire"));
        assert_eq!(FIREWALLD_SERVICE, "mirrorwire");
    }

    #[test]
    fn guide_mentions_fedora_permanent_ufw_and_other_distros() {
        let g = guide_text(47_800);
        assert!(g.contains("sudo firewall-cmd --add-port=47800/udp"), "{g}");
        assert!(
            g.contains("sudo firewall-cmd --permanent --add-port=47800/udp"),
            "{g}"
        );
        assert!(
            g.contains("Ubuntu with ufw: sudo ufw allow 47800/udp"),
            "{g}"
        );
        assert!(g.contains("another Linux distribution"), "{g}");
    }

    #[test]
    fn guide_uses_the_actual_port() {
        let g = guide_text(47901);
        assert!(g.contains("47901/udp"), "{g}");
        assert!(!g.contains("47800"), "{g}");
    }

    #[test]
    fn ufw_conf_and_rules_parse() {
        assert!(ufw_enabled("ENABLED=yes\n"));
        assert!(ufw_enabled("  enabled=yes  \n"));
        assert!(!ufw_enabled("ENABLED=no\n"));
        assert!(!ufw_enabled("# ENABLED=yes\n"));
        let allow = "-A ufw-user-input -p udp --dport 47800 -j ACCEPT\n";
        assert!(ufw_rules_allow_udp(47800, allow));
        assert!(!ufw_rules_allow_udp(47901, allow));
        let comment = "### tuple ### allow udp 47800 0.0.0.0/0 any 0.0.0.0/0 in\n";
        // The comment line has "allow" and "udp" and the port, but starts with '#'.
        assert!(!ufw_rules_allow_udp(47800, comment));
        let range = "-A ufw-user-input -p udp --dport 47800:47800 -j ACCEPT\n";
        assert!(ufw_rules_allow_udp(47800, range));
    }

    #[test]
    fn probe_ufw_from_temp_files() {
        let dir = std::env::temp_dir().join(format!(
            "mirrorwire-ufw-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("ufw.conf");
        let rules = dir.join("user.rules");
        std::fs::write(&conf, "ENABLED=yes\n").unwrap();
        std::fs::write(&rules, "-A ufw-user-input -p udp --dport 47800 -j ACCEPT\n").unwrap();
        let paths = UfwPaths {
            conf: conf.to_str().unwrap(),
            user_rules: rules.to_str().unwrap(),
        };
        assert_eq!(probe_ufw(47800, paths), Some(UfwState::Open));
        assert_eq!(probe_ufw(47901, paths), Some(UfwState::Blocked));
        std::fs::write(&conf, "ENABLED=no\n").unwrap();
        assert_eq!(probe_ufw(47800, paths), Some(UfwState::Inactive));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_linux_hint_is_hidden() {
        if cfg!(target_os = "linux") {
            return;
        }
        let hint = FirewallHint::for_port(DEFAULT_PORT);
        assert!(!hint.show);
        assert!(!hint.can_autofix);
    }
}
