//! Firewall detection and an opt-in open-port request for the presenter.
//!
//! The rule (docs/agents/decisions.md, D14): **never ask for elevated
//! privileges without an explicit user click after an explanation.**
//!
//! * The probe that runs when presenting starts only makes firewalld calls
//!   that polkit allows without authentication (`org.fedoraproject.FirewallD1.info`:
//!   `getDefaultZone`, `getActiveZones`). Reading the zone rules (`queryPort`,
//!   `queryService`) needs `…FirewallD1.config.info`, which is
//!   `auth_admin_keep` in firewalld's default (server) policy and firewalld
//!   always lets polkit show a password dialog. So the rules are only read
//!   when polkit says, without user interaction, that this is already
//!   allowed; otherwise the port state is reported as unknown.
//! * Opening the port is a polkit-authenticated D-Bus sequence that runs only
//!   from the "Open port for me…" button ([`UserConfirmed`]). The first
//!   cancelled or denied authorization ends the sequence: one click, at most
//!   one password dialog, no retries.
//!
//! The app itself never runs as root. If firewalld is not available, a short
//! general guide is shown instead.

/// firewalld service name that matches `packaging/linux/mirrorwire.xml`.
pub const FIREWALLD_SERVICE: &str = "mirrorwire";

/// What the probe knows about the presenter port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    /// A firewall rule allows the port.
    Open,
    /// A firewall is active and the port is not allowed.
    Blocked,
    /// No readable answer (no firewall found, or reading rules needs admin rights).
    Unknown,
}

/// What the presenter window should show about the firewall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Nothing (not Linux).
    Hidden,
    /// A quiet "Firewall OK" line: the port is open or a viewer reached us.
    Ok,
    /// We could not check; a calm note with the command, in case a viewer can't connect.
    Unknown,
    /// The port is blocked: a clear notice with the command and the opt-in button.
    Blocked,
}

/// Result of a best-effort firewall probe. Always safe to show.
#[derive(Debug, Clone)]
pub struct FirewallHint {
    /// `true` when we should draw the hint in the presenter window.
    pub show: bool,
    /// `true` when we know a firewall is active and the port is not open.
    pub prominent: bool,
    pub state: PortState,
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

/// Proof that the user clicked "Open port for me…" next to the explanation
/// that it asks for an admin password. The only privileged path takes one;
/// nothing on the startup, reconnect or refresh path creates it.
#[derive(Debug)]
pub struct UserConfirmed(());

impl UserConfirmed {
    /// Call only from the button's click handler.
    pub fn from_button_click() -> Self {
        Self(())
    }
}

/// Outcome of an explicit open-port request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenResult {
    Opened(String),
    /// The password dialog was cancelled or authorization was denied; nothing changed.
    NotChanged,
    Failed(String),
}

/// The calm note after a cancelled or denied password dialog.
pub const NOT_CHANGED_NOTE: &str =
    "Not changed. You can open the port yourself with the command above.";

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
    /// Unprivileged probe; safe on startup, reconnect and refresh (never
    /// triggers a password dialog).
    pub fn for_port(port: u16) -> Self {
        let command = fedora_command(port);
        let guide = guide_text(port);

        #[cfg(not(target_os = "linux"))]
        {
            Self {
                show: false,
                prominent: false,
                state: PortState::Unknown,
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

    /// What to draw. `viewer_reached_us`: a viewer connected or a connection
    /// attempt arrived, so UDP packets do get through whatever the probe said.
    pub fn notice(&self, viewer_reached_us: bool) -> Notice {
        if !cfg!(target_os = "linux") {
            return Notice::Hidden;
        }
        if viewer_reached_us || self.state == PortState::Open {
            return Notice::Ok;
        }
        match self.state {
            PortState::Blocked => Notice::Blocked,
            _ => Notice::Unknown,
        }
    }

    /// Ask firewalld (D-Bus, polkit dialog) to open the port at runtime and
    /// permanently. Only for the button click; at most one password dialog.
    pub fn open_port(port: u16, confirmed: UserConfirmed) -> OpenResult {
        let _ = confirmed;
        #[cfg(not(target_os = "linux"))]
        {
            let _ = port;
            OpenResult::Failed(
                "Opening the firewall from the app is only available on Linux.".into(),
            )
        }
        #[cfg(target_os = "linux")]
        {
            system::with_firewalld(move |fw| open_with(fw, port)).unwrap_or_else(OpenResult::Failed)
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
    let probe = system::with_firewalld(move |fw| probe_with(fw, port))
        .ok()
        .flatten()
        .or_else(system::cmd_probe);
    hint_from_probe(port, command, guide, probe, || {
        probe_ufw(port, UfwPaths::system())
    })
}

/// firewalld as the firewall logic sees it. The real one is D-Bus
/// (`system.rs`); tests use a fake that records which calls were made.
#[cfg(any(test, target_os = "linux"))]
trait Firewalld {
    /// `getDefaultZone` + `getActiveZones`. polkit action `…FirewallD1.info`,
    /// `yes` in firewalld's server and desktop policies: never a dialog.
    /// `Err` when firewalld is not reachable.
    fn zones(&mut self) -> Result<Vec<String>, String>;
    /// polkit `CheckAuthorization` for `…FirewallD1.config.info` **without
    /// user interaction**: `true` only if reading rules is already allowed.
    fn may_read_rules_silently(&mut self) -> bool;
    /// `queryPort` / `queryService` per zone (`…config.info`). Only called
    /// after `may_read_rules_silently` returned `true`.
    fn port_open(&mut self, zones: &[String], port: u16) -> Option<bool>;
    /// One change (`…FirewallD1.config`, may show the password dialog).
    fn apply(&mut self, step: &OpenStep) -> Result<(), StepError>;
}

/// One firewalld change of an open-port request.
#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenStep {
    RuntimePort { zone: String, port: u16 },
    PermanentPort { zone: String, port: u16 },
    RuntimeService { zone: String },
    PermanentService { zone: String },
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum StepError {
    /// Password dialog cancelled, authorization denied, or no answer: stop.
    NotAuthorized,
    Failed(String),
}

/// A D-Bus error text from a firewalld change -> step result.
#[cfg(any(test, target_os = "linux"))]
fn classify_step_error(message: &str) -> Result<(), StepError> {
    if is_already_enabled(message) {
        Ok(())
    } else if is_auth_error(message) {
        Err(StepError::NotAuthorized)
    } else {
        Err(StepError::Failed(message.to_owned()))
    }
}

/// firewalld's `NotAuthorizedException` (dialog cancelled or denied), the bus
/// refusing interactive authorization, or a dialog left open until the call
/// timed out.
#[cfg(any(test, target_os = "linux"))]
fn is_auth_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "notauthorized",
        "not authorized",
        "accessdenied",
        "access denied",
        "interactiveauthorizationrequired",
        "timeout",
        "timed out",
        "noreply",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// Unprivileged probe: zones (`.info`), and the zone rules only if polkit
/// already allows reading them without a dialog.
#[cfg(any(test, target_os = "linux"))]
fn probe_with(fw: &mut impl Firewalld, port: u16) -> Option<Probe> {
    let zones = match fw.zones() {
        Ok(zones) => zones,
        Err(e) => {
            tracing::debug!("firewalld not reachable: {e}");
            return None;
        }
    };
    let port_open = if fw.may_read_rules_silently() {
        fw.port_open(&zones, port)
    } else {
        tracing::debug!("reading firewalld rules needs authorization; not asking");
        None
    };
    Some(Probe {
        running: true,
        port_open,
        can_autofix: true,
    })
}

/// The changes for one click: port (runtime, permanent) per zone, plus the
/// `mirrorwire` service when the default port is used.
#[cfg(any(test, target_os = "linux"))]
fn open_plan(zones: &[String], port: u16) -> Vec<OpenStep> {
    let mut steps = Vec::new();
    for zone in zones {
        steps.push(OpenStep::RuntimePort {
            zone: zone.clone(),
            port,
        });
        steps.push(OpenStep::PermanentPort {
            zone: zone.clone(),
            port,
        });
        if port == mirrorwire_proto::DEFAULT_PORT {
            steps.push(OpenStep::RuntimeService { zone: zone.clone() });
            steps.push(OpenStep::PermanentService { zone: zone.clone() });
        }
    }
    steps
}

/// Run the plan. The first `NotAuthorized` ends it: polkit keeps a granted
/// authorization for the following steps, so a successful click shows one
/// dialog, and a cancelled one is never repeated.
#[cfg(any(test, target_os = "linux"))]
fn open_with(fw: &mut impl Firewalld, port: u16) -> OpenResult {
    let zones = match fw.zones() {
        Ok(zones) if !zones.is_empty() => zones,
        Ok(_) => {
            return OpenResult::Failed(
                "firewalld is running but no zone was found to add the port to.".into(),
            )
        }
        Err(e) => return OpenResult::Failed(e),
    };
    let mut runtime_ok = 0usize;
    let mut permanent_ok = 0usize;
    let mut last_err: Option<String> = None;
    for step in open_plan(&zones, port) {
        match fw.apply(&step) {
            Ok(()) => match step {
                OpenStep::RuntimePort { .. } => runtime_ok += 1,
                OpenStep::PermanentPort { .. } => permanent_ok += 1,
                _ => {}
            },
            Err(StepError::NotAuthorized) => {
                if runtime_ok + permanent_ok == 0 {
                    return OpenResult::NotChanged;
                }
                break;
            }
            Err(StepError::Failed(e)) => {
                if matches!(
                    step,
                    OpenStep::RuntimePort { .. } | OpenStep::PermanentPort { .. }
                ) {
                    last_err = Some(e);
                }
            }
        }
    }
    match open_port_message(port, runtime_ok, permanent_ok, last_err) {
        Ok(msg) => OpenResult::Opened(msg),
        Err(msg) => OpenResult::Failed(msg),
    }
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
    let hint = |state, message: String, can_autofix| FirewallHint {
        show: state != PortState::Open,
        prominent: state == PortState::Blocked,
        state,
        message,
        guide: guide.clone(),
        command: command.clone(),
        can_autofix,
        port,
    };
    match probe {
        Some(Probe { running: true, port_open: Some(true), can_autofix }) => hint(
            PortState::Open,
            format!("Firewall OK: firewalld allows UDP {port}."),
            can_autofix,
        ),
        Some(Probe { running: true, port_open: Some(false), can_autofix }) => hint(
            PortState::Blocked,
            format!(
                "Your firewall (firewalld) blocks UDP {port}. Viewers can find this computer, but they can't connect until the port is opened."
            ),
            can_autofix,
        ),
        Some(Probe { running: true, port_open: None, can_autofix }) => hint(
            PortState::Unknown,
            format!(
                "firewalld is running. Checking its rules needs admin rights, so MirrorWire doesn't look. If a viewer finds this computer but can't connect, UDP {port} is probably blocked."
            ),
            can_autofix,
        ),
        Some(Probe { running: false, .. }) | None => match ufw() {
            Some(state) => ufw_hint(port, state, guide.clone()),
            None => hint(PortState::Unknown, intro(port), false),
        },
    }
}

#[cfg(any(test, target_os = "linux"))]
fn ufw_hint(port: u16, state: UfwState, guide: String) -> FirewallHint {
    let (state, message, command) = match state {
        UfwState::Inactive => (PortState::Unknown, intro(port), fedora_command(port)),
        UfwState::Open => (
            PortState::Open,
            format!("Firewall OK: ufw allows UDP {port}."),
            ufw_command(port),
        ),
        UfwState::Blocked => (
            PortState::Blocked,
            format!(
                "Your firewall (ufw) blocks UDP {port}. Viewers can find this computer, but they can't connect until the port is allowed."
            ),
            ufw_command(port),
        ),
        UfwState::Unknown => (
            PortState::Unknown,
            format!("ufw is active. If a viewer cannot connect, allow UDP {port}."),
            ufw_command(port),
        ),
    };
    FirewallHint {
        show: state != PortState::Open,
        prominent: state == PortState::Blocked,
        state,
        message,
        guide,
        command,
        can_autofix: false,
        port,
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
/// The port is not queried: `--query-port` needs the same authorization as
/// the D-Bus `queryPort` and would show a password dialog.
#[cfg(any(test, target_os = "linux"))]
fn probe_from_state(text: &str, success: bool) -> Option<Probe> {
    let text = text.trim();
    if text.is_empty() && !success {
        return None;
    }
    Some(Probe {
        running: text.eq_ignore_ascii_case("running"),
        port_open: None,
        can_autofix: false,
    })
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
            "firewalld did not accept the request.".into()
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
        assert!(!open.show && open.can_autofix && open.message.starts_with("Firewall OK"));
        assert_eq!(open.notice(false), Notice::Ok);
        let closed = hint(probe(true, Some(false)), Some(UfwState::Open));
        assert!(
            closed.prominent && closed.message.contains("blocks UDP 47800"),
            "ufw is ignored while firewalld runs"
        );
        assert_eq!(closed.state, PortState::Blocked);
        assert_eq!(closed.notice(false), Notice::Blocked);
        assert_eq!(closed.command, "sudo firewall-cmd --add-port=47800/udp");
        let unknown = hint(probe(true, None), None);
        assert!(!unknown.prominent && unknown.message.contains("needs admin rights"));
        assert_eq!(unknown.notice(false), Notice::Unknown);
        // firewalld absent or stopped: ufw decides, else the general intro.
        let ufw = hint(None, Some(UfwState::Blocked));
        assert!(ufw.prominent && ufw.command.contains("ufw allow 47800/udp") && !ufw.can_autofix);
        let nothing = hint(probe(false, None), None);
        assert!(
            nothing.show && !nothing.prominent && nothing.message.starts_with("If a viewer finds")
        );
        assert!(!nothing.can_autofix);
        assert_eq!(nothing.notice(false), Notice::Unknown);
    }

    #[test]
    fn a_viewer_reaching_us_means_the_firewall_is_fine() {
        let blocked = hint_from_probe(
            1,
            String::new(),
            String::new(),
            probe(true, Some(false)),
            || None,
        );
        let unknown = hint_from_probe(1, String::new(), String::new(), probe(true, None), || None);
        assert_eq!(blocked.notice(true), Notice::Ok);
        assert_eq!(unknown.notice(true), Notice::Ok);
    }

    #[test]
    fn ufw_states_map_to_hints() {
        let g = guide_text(5000);
        let open = ufw_hint(5000, UfwState::Open, g.clone());
        assert!(!open.show && open.state == PortState::Open);
        let inactive = ufw_hint(5000, UfwState::Inactive, g.clone());
        assert!(inactive.show && !inactive.prominent && inactive.command.contains("firewall-cmd"));
        let unknown = ufw_hint(5000, UfwState::Unknown, g.clone());
        assert!(!unknown.prominent && unknown.message.contains("allow UDP 5000"));
        assert_eq!(
            ufw_hint(5000, UfwState::Blocked, g).notice(false),
            Notice::Blocked
        );
    }

    #[test]
    fn firewall_cmd_state_parsing_never_queries_the_port() {
        assert_eq!(probe_from_state("", false), None);
        assert!(!probe_from_state("not running\n", false).unwrap().running);
        assert!(!probe_from_state("", true).unwrap().running);
        assert_eq!(
            probe_from_state("running\n", true),
            Some(Probe {
                running: true,
                port_open: None,
                can_autofix: false
            })
        );
    }

    /// Fake firewalld: records calls; `apply` answers from a script.
    #[derive(Default)]
    struct FakeFirewalld {
        reachable: bool,
        rules_readable: bool,
        port_open: Option<bool>,
        zones: Vec<String>,
        /// Answers for `apply`, front first; empty -> Ok.
        answers: std::collections::VecDeque<Result<(), StepError>>,
        port_queries: usize,
        /// Privileged calls, i.e. potential password dialogs.
        applied: Vec<OpenStep>,
    }

    impl FakeFirewalld {
        fn running(zones: &[&str]) -> Self {
            Self {
                reachable: true,
                zones: zones.iter().map(|z| z.to_string()).collect(),
                ..Self::default()
            }
        }
    }

    impl Firewalld for FakeFirewalld {
        fn zones(&mut self) -> Result<Vec<String>, String> {
            if self.reachable {
                Ok(self.zones.clone())
            } else {
                Err("org.freedesktop.DBus.Error.ServiceUnknown".into())
            }
        }
        fn may_read_rules_silently(&mut self) -> bool {
            self.rules_readable
        }
        fn port_open(&mut self, _zones: &[String], _port: u16) -> Option<bool> {
            assert!(
                self.rules_readable,
                "rules read without silent authorization"
            );
            self.port_queries += 1;
            self.port_open
        }
        fn apply(&mut self, step: &OpenStep) -> Result<(), StepError> {
            self.applied.push(step.clone());
            self.answers.pop_front().unwrap_or(Ok(()))
        }
    }

    #[test]
    fn startup_probe_makes_no_privileged_call() {
        // Fedora default (server policy): reading rules would need a password.
        let mut fw = FakeFirewalld::running(&["public"]);
        fw.port_open = Some(true);
        for _ in 0..3 {
            // start, reconnect, refresh
            let p = probe_with(&mut fw, 47_800).unwrap();
            assert_eq!(p.port_open, None, "unknown, not asked");
            assert!(p.can_autofix);
        }
        assert_eq!(fw.port_queries, 0);
        assert!(fw.applied.is_empty());

        // Rules readable without a dialog (desktop policy, or native with a
        // polkit "yes"): the real answer is used.
        fw.rules_readable = true;
        assert_eq!(probe_with(&mut fw, 47_800).unwrap().port_open, Some(true));
        fw.port_open = Some(false);
        assert_eq!(probe_with(&mut fw, 47_800).unwrap().port_open, Some(false));
        assert!(fw.applied.is_empty());

        assert_eq!(probe_with(&mut FakeFirewalld::default(), 47_800), None);
    }

    #[test]
    fn open_plan_covers_every_zone_and_the_service_on_the_default_port() {
        let zones = ["home".to_owned(), "public".to_owned()];
        assert_eq!(open_plan(&zones, 47_800).len(), 8);
        let custom = open_plan(&zones, 50_000);
        assert_eq!(
            custom,
            [
                OpenStep::RuntimePort {
                    zone: "home".into(),
                    port: 50_000
                },
                OpenStep::PermanentPort {
                    zone: "home".into(),
                    port: 50_000
                },
                OpenStep::RuntimePort {
                    zone: "public".into(),
                    port: 50_000
                },
                OpenStep::PermanentPort {
                    zone: "public".into(),
                    port: 50_000
                },
            ]
        );
    }

    #[test]
    fn cancel_means_one_dialog_and_no_further_prompts() {
        let mut fw = FakeFirewalld::running(&["home", "public"]);
        fw.answers.push_back(Err(StepError::NotAuthorized));
        assert_eq!(open_with(&mut fw, 47_800), OpenResult::NotChanged);
        assert_eq!(fw.applied.len(), 1, "stopped at the first cancelled dialog");
        // Afterwards the presenter keeps refreshing (reconnects, restarts):
        // only unprivileged calls.
        for _ in 0..5 {
            probe_with(&mut fw, 47_800);
        }
        assert_eq!(fw.applied.len(), 1);
        // The next explicit click asks again, once.
        fw.answers.push_back(Err(StepError::NotAuthorized));
        assert_eq!(open_with(&mut fw, 47_800), OpenResult::NotChanged);
        assert_eq!(fw.applied.len(), 2);
        assert!(NOT_CHANGED_NOTE.contains("command above"));
    }

    #[test]
    fn granted_click_applies_everything_and_reports() {
        let mut fw = FakeFirewalld::running(&["public"]);
        let result = open_with(&mut fw, 47_800);
        assert_eq!(fw.applied.len(), 4);
        assert!(matches!(result, OpenResult::Opened(m) if m.contains("after a reboot")));

        // The service is optional: its failure does not spoil the result.
        let mut fw = FakeFirewalld::running(&["public"]);
        fw.answers = [
            Ok(()),
            Ok(()),
            Err(StepError::Failed("INVALID_SERVICE".into())),
        ]
        .into();
        assert!(matches!(open_with(&mut fw, 47_800), OpenResult::Opened(_)));

        // Authorization lost half way (dialog for a later step cancelled): stop, report what happened.
        let mut fw = FakeFirewalld::running(&["home", "public"]);
        fw.answers = [
            Ok(()),
            Ok(()),
            Ok(()),
            Ok(()),
            Err(StepError::NotAuthorized),
        ]
        .into();
        assert!(matches!(open_with(&mut fw, 47_800), OpenResult::Opened(_)));
        assert_eq!(fw.applied.len(), 5);

        let mut fw = FakeFirewalld::running(&["public"]);
        fw.answers = [
            Err(StepError::Failed("INVALID_ZONE".into())),
            Err(StepError::Failed("INVALID_ZONE".into())),
        ]
        .into();
        assert_eq!(
            open_with(&mut fw, 50_000),
            OpenResult::Failed("INVALID_ZONE".into())
        );

        assert!(
            matches!(open_with(&mut FakeFirewalld::running(&[]), 1), OpenResult::Failed(m) if m.contains("no zone"))
        );
        assert!(matches!(
            open_with(&mut FakeFirewalld::default(), 1),
            OpenResult::Failed(_)
        ));
    }

    #[test]
    fn dbus_errors_are_classified() {
        assert_eq!(
            classify_step_error("org.fedoraproject.FirewallD1.NotAuthorizedException: Not Authorized(polkit): org.fedoraproject.FirewallD1.config"),
            Err(StepError::NotAuthorized)
        );
        assert_eq!(
            classify_step_error("org.freedesktop.DBus.Error.AccessDenied"),
            Err(StepError::NotAuthorized)
        );
        assert_eq!(
            classify_step_error("Method call timed out"),
            Err(StepError::NotAuthorized)
        );
        assert_eq!(classify_step_error("ALREADY_ENABLED: 47800:udp"), Ok(()));
        assert_eq!(
            classify_step_error("INVALID_ZONE: nope"),
            Err(StepError::Failed("INVALID_ZONE: nope".into()))
        );
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
            .contains("did not accept"));
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
        assert_eq!(hint.notice(false), Notice::Hidden);
    }
}
