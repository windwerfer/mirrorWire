//! firewalld over the system D-Bus and the `firewall-cmd --state` fallback:
//! the parts of the firewall helper that need a real system bus / host. The
//! decisions (what to call, when to stop) live in the parent module and are
//! unit-tested there against a fake [`Firewalld`].
//!
//! polkit actions (firewalld's `dbus_polkit_require_auth`; firewalld always
//! asks polkit with "allow user interaction", so any call that is not `yes`
//! for the caller shows a password dialog):
//! - `getDefaultZone`, `getActiveZones`: `…FirewallD1.info`, `yes` in the
//!   shipped server and desktop policies.
//! - `queryPort`, `queryService`: `…FirewallD1.config.info`, `auth_admin_keep`
//!   in the server policy (the default symlink) and `yes` in the desktop one.
//! - `addPort`, `addService`, `config.getZoneByName`, `config.zone.add*`:
//!   `…FirewallD1.config`, `auth_admin_keep` everywhere.

use super::{
    all_zones_open, classify_step_error, merge_zones, probe_from_state, Firewalld, OpenStep, Probe,
    StepError, FIREWALLD_SERVICE,
};
use mirrorwire_proto::DEFAULT_PORT;
use std::collections::HashMap;
use std::time::Duration;

const DBUS_DEST: &str = "org.fedoraproject.FirewallD1";
const DBUS_PATH: &str = "/org/fedoraproject/FirewallD1";
const DBUS_IFACE: &str = "org.fedoraproject.FirewallD1";
const DBUS_ZONE_IFACE: &str = "org.fedoraproject.FirewallD1.zone";
const DBUS_CONFIG_IFACE: &str = "org.fedoraproject.FirewallD1.config";
const DBUS_CONFIG_PATH: &str = "/org/fedoraproject/FirewallD1/config";
const DBUS_CONFIG_ZONE_IFACE: &str = "org.fedoraproject.FirewallD1.config.zone";
const PK_DEST: &str = "org.freedesktop.PolicyKit1";
const PK_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const PK_IFACE: &str = "org.freedesktop.PolicyKit1.Authority";
const PK_CONFIG_INFO: &str = "org.fedoraproject.FirewallD1.config.info";

/// The real firewalld: one system-bus connection on a private runtime.
pub(super) struct SystemFirewalld {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
}

/// Connect and run `f` on a dedicated thread with its own current-thread
/// runtime, so we never `block_on` inside the GUI's or presenter's runtime.
pub(super) fn with_firewalld<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut SystemFirewalld) -> T + Send + 'static,
{
    std::thread::Builder::new()
        .name("firewalld-dbus".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            let conn = rt.block_on(async {
                zbus::connection::Builder::system()
                    // Long enough for someone to type a password.
                    .method_timeout(Duration::from_secs(120))
                    .build()
                    .await
                    .map_err(|e| e.to_string())
            })?;
            let mut fw = SystemFirewalld { rt, conn };
            Ok(f(&mut fw))
        })
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "firewalld thread panicked".to_owned())?
}

impl Firewalld for SystemFirewalld {
    fn zones(&mut self) -> Result<Vec<String>, String> {
        let conn = &self.conn;
        self.rt.block_on(async {
            let proxy = proxy(conn, DBUS_PATH, DBUS_IFACE).await?;
            let default_zone: String = proxy
                .call("getDefaultZone", &())
                .await
                .map_err(|e| e.to_string())?;
            let active: HashMap<String, Vec<String>> = match proxy.call("getActiveZones", &()).await
            {
                Ok(map) => map,
                Err(e) => {
                    tracing::debug!("firewalld getActiveZones failed: {e}");
                    HashMap::new()
                }
            };
            Ok(merge_zones(default_zone, active.into_keys()))
        })
    }

    fn may_read_rules_silently(&mut self) -> bool {
        let conn = &self.conn;
        let check = async {
            let name = conn
                .unique_name()
                .ok_or_else(|| "no unique bus name".to_owned())?
                .to_string();
            let proxy = zbus::Proxy::new(conn, PK_DEST, PK_PATH, PK_IFACE)
                .await
                .map_err(|e| e.to_string())?;
            let mut subject: HashMap<&str, zbus::Variant<'_>> = HashMap::new();
            subject.insert("name", zbus::Variant::from(name.as_str()));
            let details: HashMap<&str, &str> = HashMap::new();
            // flags 0: no user interaction. Never a dialog.
            let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = proxy
                .call(
                    "CheckAuthorization",
                    &(
                        ("system-bus-name", subject),
                        PK_CONFIG_INFO,
                        details,
                        0u32,
                        "",
                    ),
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok::<bool, String>(authorized)
        };
        match self
            .rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(3), check).await })
        {
            Ok(Ok(authorized)) => authorized,
            Ok(Err(e)) => {
                // In the Flatpak the bus proxy only lets us talk to firewalld.
                tracing::debug!("polkit check not available: {e}");
                false
            }
            Err(_) => false,
        }
    }

    fn port_open(&mut self, zones: &[String], port: u16) -> Option<bool> {
        let conn = &self.conn;
        self.rt.block_on(async {
            let proxy = proxy(conn, DBUS_PATH, DBUS_ZONE_IFACE).await.ok()?;
            let mut answers = Vec::with_capacity(zones.len());
            for zone in zones {
                answers.push(zone_allows(&proxy, zone, port).await);
            }
            all_zones_open(answers)
        })
    }

    fn apply(&mut self, step: &OpenStep) -> Result<(), StepError> {
        let conn = &self.conn;
        let result = self.rt.block_on(async {
            match step {
                OpenStep::RuntimePort { zone, port } => {
                    let proxy = proxy(conn, DBUS_PATH, DBUS_ZONE_IFACE).await?;
                    proxy
                        .call::<_, _, String>(
                            "addPort",
                            &(zone.as_str(), port.to_string(), "udp", 0i32),
                        )
                        .await
                        .map(drop)
                        .map_err(|e| e.to_string())
                }
                OpenStep::RuntimeService { zone } => {
                    let proxy = proxy(conn, DBUS_PATH, DBUS_ZONE_IFACE).await?;
                    proxy
                        .call::<_, _, String>(
                            "addService",
                            &(zone.as_str(), FIREWALLD_SERVICE, 0i32),
                        )
                        .await
                        .map(drop)
                        .map_err(|e| e.to_string())
                }
                OpenStep::PermanentPort { zone, port } => {
                    let proxy = config_zone(conn, zone).await?;
                    proxy
                        .call::<_, _, ()>("addPort", &(port.to_string(), "udp"))
                        .await
                        .map_err(|e| e.to_string())
                }
                OpenStep::PermanentService { zone } => {
                    let proxy = config_zone(conn, zone).await?;
                    proxy
                        .call::<_, _, ()>("addService", &(FIREWALLD_SERVICE,))
                        .await
                        .map_err(|e| e.to_string())
                }
            }
        });
        match result {
            Ok(()) => Ok(()),
            Err(e) => classify_step_error(&e),
        }
    }
}

async fn proxy<'a>(
    conn: &'a zbus::Connection,
    path: &'a str,
    interface: &'a str,
) -> Result<zbus::Proxy<'a>, String> {
    zbus::proxy::Builder::new(conn)
        .destination(DBUS_DEST)
        .path(path)
        .interface(interface)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| e.to_string())
}

async fn config_zone<'a>(
    conn: &'a zbus::Connection,
    zone: &str,
) -> Result<zbus::Proxy<'a>, String> {
    let config = proxy(conn, DBUS_CONFIG_PATH, DBUS_CONFIG_IFACE).await?;
    let path: zbus::OwnedObjectPath = config
        .call("getZoneByName", &(zone,))
        .await
        .map_err(|e| e.to_string())?;
    zbus::proxy::Builder::new(conn)
        .destination(DBUS_DEST)
        .path(path)
        .interface(DBUS_CONFIG_ZONE_IFACE)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| e.to_string())
}

async fn zone_allows(proxy: &zbus::Proxy<'_>, zone: &str, port: u16) -> Option<bool> {
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

/// `firewall-cmd --state` (native installs where D-Bus failed). Only the
/// state: `--query-port` would need authorization.
pub(super) fn cmd_probe() -> Option<Probe> {
    let state = run_maybe_host("firewall-cmd", &["--state"])?;
    probe_from_state(
        &String::from_utf8_lossy(&state.stdout),
        state.status.success(),
    )
}

/// Run a command, falling back to `flatpak-spawn --host` when that portal exists.
/// No extra Flatpak permission is requested; the spawn simply fails.
fn run_maybe_host(program: &str, args: &[&str]) -> Option<std::process::Output> {
    if let Ok(out) = std::process::Command::new(program).args(args).output() {
        return Some(out);
    }
    let mut cmd = std::process::Command::new("flatpak-spawn");
    cmd.arg("--host").arg(program).args(args);
    cmd.output().ok()
}
