//! Firewall detection and a best-effort open-port request for the presenter.
//!
//! On Linux the presenter talks to firewalld over the system bus when it can
//! (Flatpak: `--system-talk-name=org.fedoraproject.FirewallD1`). Opening a port
//! is a polkit-authenticated D-Bus call: the app itself never runs as root.
//! If firewalld is not available, a short general guide is shown instead.

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
            Self {
                show: false,
                prominent: false,
                message: String::new(),
                guide,
                command,
                can_autofix: false,
                port,
            }
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
            system::open_port(port)
        }
    }
}

#[cfg(target_os = "linux")]
mod system;

#[cfg(any(test, target_os = "linux"))]
fn intro(port: u16) -> String {
    format!(
        "If a viewer finds this computer but cannot connect, the firewall may be blocking UDP {port}."
    )
}

#[cfg(target_os = "linux")]
fn probe_linux(port: u16, command: String, guide: String) -> FirewallHint {
    hint_from_probe(port, command, guide, system::probe_firewalld(port), || {
        probe_ufw(port, UfwPaths::system())
    })
}

/// The hint for what the firewalld probe found; `ufw` is only consulted when
/// firewalld is not running.
#[cfg(any(test, target_os = "linux"))]
fn hint_from_probe(
    port: u16,
    command: String,
    guide: String,
    probe: Option<Probe>,
    ufw: impl FnOnce() -> Option<UfwState>,
) -> FirewallHint {
    let hint = |show, prominent, message: String, can_autofix| FirewallHint {
        show,
        prominent,
        message,
        guide: guide.clone(),
        command: command.clone(),
        can_autofix,
        port,
    };
    match probe {
        Some(Probe { running: true, port_open: Some(true), can_autofix }) => hint(
            false,
            false,
            format!("firewalld is running; UDP {port} is already open."),
            can_autofix,
        ),
        Some(Probe { running: true, port_open: Some(false), can_autofix }) => hint(
            true,
            true,
            format!(
                "firewalld is running and UDP {port} is not open. Viewers can discover this computer but the connection will time out until the port is opened."
            ),
            can_autofix,
        ),
        Some(Probe { running: true, port_open: None, can_autofix }) => hint(
            true,
            true,
            format!("firewalld is running. If a viewer cannot connect, UDP {port} may be blocked."),
            can_autofix,
        ),
        Some(Probe { running: false, .. }) | None => match ufw() {
            Some(state) => ufw_hint(port, state, guide.clone()),
            None => hint(true, false, intro(port), false),
        },
    }
}

#[cfg(any(test, target_os = "linux"))]
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

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Probe {
    running: bool,
    port_open: Option<bool>,
    can_autofix: bool,
}

/// `firewall-cmd --state` output -> probe (`None`: no firewall-cmd answer).
/// `query_port` is only run when firewalld is running.
#[cfg(any(test, target_os = "linux"))]
fn probe_from_state(
    text: &str,
    success: bool,
    query_port: impl FnOnce() -> Option<bool>,
) -> Option<Probe> {
    let text = text.trim();
    if text.is_empty() && !success {
        return None;
    }
    if !text.eq_ignore_ascii_case("running") {
        return Some(Probe {
            running: false,
            port_open: None,
            can_autofix: false,
        });
    }
    Some(Probe {
        running: true,
        port_open: query_port(),
        can_autofix: false,
    })
}

/// `firewall-cmd --query-port=P/udp` output -> open?
#[cfg(any(test, target_os = "linux"))]
fn parse_query_port(text: &str, success: bool) -> Option<bool> {
    let text = text.trim().to_ascii_lowercase();
    if text == "yes" || (success && text.is_empty()) {
        Some(true)
    } else if text == "no" || !success {
        Some(false)
    } else {
        None
    }
}

/// Active zones plus the default zone, sorted, without duplicates.
#[cfg(any(test, target_os = "linux"))]
fn merge_zones(default_zone: String, active: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut zones: Vec<String> = active.into_iter().collect();
    if !default_zone.is_empty() {
        zones.push(default_zone);
    }
    zones.sort();
    zones.dedup();
    zones
}

/// Combine per-zone answers: open only if every zone that answered allows
/// the port; `None` when no zone answered.
#[cfg(any(test, target_os = "linux"))]
fn all_zones_open(answers: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    let mut known = false;
    let mut all_open = true;
    for answer in answers.into_iter().flatten() {
        known = true;
        all_open &= answer;
    }
    known.then_some(all_open)
}

/// User-facing result of an open-port request over `zones` zones.
#[cfg(any(test, target_os = "linux"))]
fn open_port_message(
    port: u16,
    runtime_ok: usize,
    permanent_ok: usize,
    last_err: Option<String>,
) -> Result<String, String> {
    match (runtime_ok > 0, permanent_ok > 0) {
        (false, false) => Err(last_err.unwrap_or_else(|| {
            "firewalld did not accept the request. A password prompt may have been cancelled."
                .into()
        })),
        (true, true) => Ok(format!("Opened UDP {port} in firewalld (now, and after a reboot).")),
        (true, false) => Ok(format!(
            "Opened UDP {port} in firewalld until the next reboot. To keep it: {} && sudo firewall-cmd --reload",
            fedora_permanent_command(port)
        )),
        (false, true) => Ok(format!(
            "Added UDP {port} to the permanent firewalld configuration. Run sudo firewall-cmd --reload (or reopen the port) to apply it now."
        )),
    }
}

/// firewalld's answer when a port or service is already in the zone.
#[cfg(any(test, target_os = "linux"))]
fn is_already_enabled(message: &str) -> bool {
    message.contains("ALREADY_ENABLED") || message.to_ascii_lowercase().contains("already enabled")
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UfwState {
    Inactive,
    Open,
    Blocked,
    Unknown,
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, Copy)]
struct UfwPaths<'a> {
    conf: &'a str,
    user_rules: &'a str,
}

#[cfg(any(test, target_os = "linux"))]
impl UfwPaths<'static> {
    fn system() -> Self {
        Self {
            conf: "/etc/ufw/ufw.conf",
            user_rules: "/etc/ufw/user.rules",
        }
    }
}

#[cfg(any(test, target_os = "linux"))]
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

#[cfg(any(test, target_os = "linux"))]
fn ufw_enabled(conf: &str) -> bool {
    conf.lines().any(|line| {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        lower == "enabled=yes" || lower.starts_with("enabled=yes#")
    })
}

#[cfg(any(test, target_os = "linux"))]
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

    fn probe(running: bool, port_open: Option<bool>) -> Option<Probe> {
        Some(Probe {
            running,
            port_open,
            can_autofix: running,
        })
    }

    #[test]
    fn hint_follows_the_firewalld_probe() {
        let hint = |p, ufw: Option<UfwState>| {
            hint_from_probe(
                47_800,
                fedora_command(47_800),
                guide_text(47_800),
                p,
                || ufw,
            )
        };
        let open = hint(probe(true, Some(true)), None);
        assert!(!open.show && open.can_autofix && open.message.contains("already open"));
        let closed = hint(probe(true, Some(false)), Some(UfwState::Open));
        assert!(
            closed.show && closed.prominent && closed.message.contains("not open"),
            "ufw is ignored while firewalld runs"
        );
        assert_eq!(closed.command, "sudo firewall-cmd --add-port=47800/udp");
        let unknown = hint(probe(true, None), None);
        assert!(unknown.show && unknown.prominent && unknown.message.contains("may be blocked"));
        // firewalld absent or stopped: ufw decides, else the general intro.
        let ufw = hint(None, Some(UfwState::Blocked));
        assert!(ufw.prominent && ufw.command.contains("ufw allow 47800/udp") && !ufw.can_autofix);
        let nothing = hint(probe(false, None), None);
        assert!(
            nothing.show && !nothing.prominent && nothing.message.starts_with("If a viewer finds")
        );
        assert!(!nothing.can_autofix);
    }

    #[test]
    fn ufw_states_map_to_hints() {
        let g = guide_text(5000);
        assert!(!ufw_hint(5000, UfwState::Open, g.clone()).show);
        let inactive = ufw_hint(5000, UfwState::Inactive, g.clone());
        assert!(inactive.show && !inactive.prominent && inactive.command.contains("firewall-cmd"));
        let unknown = ufw_hint(5000, UfwState::Unknown, g);
        assert!(unknown.prominent && unknown.message.contains("allow UDP 5000"));
    }

    #[test]
    fn firewall_cmd_output_parsing() {
        let never = || -> Option<bool> { panic!("port queried although firewalld is not running") };
        assert_eq!(probe_from_state("", false, never), None);
        assert_eq!(
            probe_from_state("not running\n", false, never),
            probe(false, None).map(|p| Probe {
                can_autofix: false,
                ..p
            })
        );
        assert!(!probe_from_state("", true, never).unwrap().running);
        let running = probe_from_state("running\n", true, || Some(true)).unwrap();
        assert_eq!(
            running,
            Probe {
                running: true,
                port_open: Some(true),
                can_autofix: false
            }
        );
        assert_eq!(parse_query_port("yes\n", true), Some(true));
        assert_eq!(parse_query_port("", true), Some(true));
        assert_eq!(parse_query_port("no", false), Some(false));
        assert_eq!(parse_query_port("", false), Some(false));
        assert_eq!(parse_query_port("maybe", true), None);
    }

    #[test]
    fn zones_and_open_port_results() {
        assert_eq!(
            merge_zones("public".into(), vec!["home".into(), "public".into()]),
            ["home", "public"]
        );
        assert_eq!(
            merge_zones(String::new(), Vec::<String>::new()),
            Vec::<String>::new()
        );
        assert_eq!(merge_zones("public".into(), Vec::new()), ["public"]);
        assert_eq!(all_zones_open([]), None);
        assert_eq!(all_zones_open([None, Some(true)]), Some(true));
        assert_eq!(all_zones_open([Some(true), Some(false), None]), Some(false));
        assert!(open_port_message(1, 1, 1, None)
            .unwrap()
            .contains("after a reboot"));
        assert!(open_port_message(1, 1, 0, None)
            .unwrap()
            .contains("--permanent --add-port=1/udp"));
        assert!(open_port_message(1, 0, 2, None)
            .unwrap()
            .contains("--reload"));
        assert_eq!(
            open_port_message(1, 0, 0, Some("denied".into())),
            Err("denied".into())
        );
        assert!(open_port_message(1, 0, 0, None)
            .unwrap_err()
            .contains("cancelled"));
        assert!(is_already_enabled(
            "org.fedoraproject.FirewallD1.Exception: ALREADY_ENABLED: 47800:udp"
        ));
        assert!(is_already_enabled("Already enabled"));
        assert!(!is_already_enabled("ACCESS_DENIED"));
    }

    #[test]
    fn non_linux_hint_is_hidden() {
        if cfg!(target_os = "linux") {
            return;
        }
        let hint = FirewallHint::for_port(mirrorwire_proto::DEFAULT_PORT);
        assert!(!hint.show);
        assert!(!hint.can_autofix);
    }
}
