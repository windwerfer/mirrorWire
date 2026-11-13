//! firewalld D-Bus calls and `firewall-cmd` processes: the parts of the
//! firewall helper that need a real system bus / host. The decisions they
//! feed (`probe_from_state`, `merge_zones`, `open_port_message`, …) live in
//! the parent module and are unit-tested there.

use super::{
    all_zones_open, is_already_enabled, merge_zones, open_port_message, parse_query_port,
    probe_from_state, Probe, FIREWALLD_SERVICE,
};
use mirrorwire_proto::DEFAULT_PORT;

pub(super) fn probe_firewalld(port: u16) -> Option<Probe> {
    if let Some(probe) = dbus_probe(port) {
        return Some(probe);
    }
    // Native installs (and rare cases where D-Bus is blocked but firewall-cmd
    // is on PATH). In a Flatpak this usually fails; that's fine.
    cmd_probe(port)
}

fn dbus_probe(port: u16) -> Option<Probe> {
    match block_on_dbus(async move { dbus_probe_async(port).await }) {
        Ok(probe) => Some(probe),
        Err(e) => {
            tracing::debug!("firewalld D-Bus probe failed: {e}");
            None
        }
    }
}

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

fn cmd_probe(port: u16) -> Option<Probe> {
    let state = run_maybe_host("firewall-cmd", &["--state"])?;
    let text = String::from_utf8_lossy(&state.stdout).into_owned();
    probe_from_state(&text, state.status.success(), || cmd_port_open(port))
}

fn cmd_port_open(port: u16) -> Option<bool> {
    let arg = format!("--query-port={port}/udp");
    let out = run_maybe_host("firewall-cmd", &[&arg])?;
    parse_query_port(&String::from_utf8_lossy(&out.stdout), out.status.success())
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

pub(super) fn open_port(port: u16) -> Result<String, String> {
    block_on_dbus(async move { open_port_async(port).await })
}

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
    open_port_message(port, runtime_ok, permanent_ok, last_err)
}

const DBUS_DEST: &str = "org.fedoraproject.FirewallD1";
const DBUS_PATH: &str = "/org/fedoraproject/FirewallD1";
const DBUS_IFACE: &str = "org.fedoraproject.FirewallD1";
const DBUS_ZONE_IFACE: &str = "org.fedoraproject.FirewallD1.zone";
const DBUS_CONFIG_IFACE: &str = "org.fedoraproject.FirewallD1.config";
const DBUS_CONFIG_PATH: &str = "/org/fedoraproject/FirewallD1/config";
const DBUS_CONFIG_ZONE_IFACE: &str = "org.fedoraproject.FirewallD1.config.zone";

async fn system_connection() -> Result<zbus::Connection, String> {
    zbus::connection::Builder::system()
        .method_timeout(std::time::Duration::from_secs(120))
        .build()
        .await
        .map_err(dbus_err)
}

async fn iface<'a>(
    conn: &'a zbus::Connection,
    path: &'a str,
    interface: &'a str,
) -> Result<zbus::Proxy<'a>, String> {
    zbus::Proxy::new(conn, DBUS_DEST, path, interface)
        .await
        .map_err(dbus_err)
}

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
    Ok(merge_zones(default_zone, active.into_keys()))
}

async fn port_open_in_zones(conn: &zbus::Connection, zones: &[String], port: u16) -> Option<bool> {
    let mut answers = Vec::with_capacity(zones.len());
    for zone in zones {
        answers.push(zone_allows(conn, zone, port).await);
    }
    all_zones_open(answers)
}

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

async fn zone_add_service_runtime(conn: &zbus::Connection, zone: &str) -> Result<(), String> {
    let proxy = iface(conn, DBUS_PATH, DBUS_ZONE_IFACE).await?;
    let result: Result<(), zbus::Error> = proxy
        .call("addService", &(zone, FIREWALLD_SERVICE, 0i32))
        .await;
    accept_already_enabled(result)
}

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

async fn zone_add_service_permanent(conn: &zbus::Connection, zone: &str) -> Result<(), String> {
    let path = config_zone_path(conn, zone).await?;
    let proxy = iface(conn, path.as_str(), DBUS_CONFIG_ZONE_IFACE).await?;
    let result: Result<(), zbus::Error> = proxy.call("addService", &(FIREWALLD_SERVICE,)).await;
    accept_already_enabled(result)
}

async fn config_zone_path(conn: &zbus::Connection, zone: &str) -> Result<String, String> {
    let proxy = iface(conn, DBUS_CONFIG_PATH, DBUS_CONFIG_IFACE).await?;
    let path: zbus::OwnedObjectPath = proxy
        .call("getZoneByName", &(zone,))
        .await
        .map_err(dbus_err)?;
    Ok(path.as_str().to_owned())
}

fn accept_already_enabled(result: Result<(), zbus::Error>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if is_already_enabled(&e.to_string()) => Ok(()),
        Err(e) => Err(dbus_err(e)),
    }
}

fn dbus_err(err: impl std::fmt::Display) -> String {
    err.to_string()
}

/// Run a firewalld D-Bus call on a private current-thread runtime so we never
/// `block_on` the GUI's or presenter's existing tokio runtime.
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
