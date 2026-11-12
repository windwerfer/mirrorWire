use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use mirror_proto::MDNS_SERVICE;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presenter {
    pub name: String,
    pub addr: SocketAddr,
    pub fingerprint: String,
    pub resolution: Option<String>,
}

/// Keeps a presenter announced on the local network while alive.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    pub fn start(name: &str, port: u16, fingerprint: &str, resolution: &str) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let host = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "mirror".into());
        let props = [("fp", fingerprint), ("v", "1"), ("res", resolution)];
        let info = ServiceInfo::new(
            MDNS_SERVICE,
            name,
            &format!("{host}.local."),
            "",
            port,
            &props[..],
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Continuously browses for presenters; poll with `poll` from any loop.
pub struct Browser {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
    found: Vec<Presenter>,
}

impl Browser {
    pub fn start() -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let events = daemon.browse(MDNS_SERVICE)?;
        Ok(Self { daemon, events, found: Vec::new() })
    }

    pub fn poll(&mut self) -> &[Presenter] {
        while let Ok(event) = self.events.try_recv() {
            self.apply(event);
        }
        &self.found
    }

    pub fn browse_for(mut self, wait: Duration) -> Vec<Presenter> {
        let deadline = Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(event) => self.apply(event),
                Err(_) => break,
            }
        }
        std::mem::take(&mut self.found)
    }

    fn apply(&mut self, event: ServiceEvent) {
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let Some(fp) = info.get_property_val_str("fp") else { return };
                let mut addrs: Vec<IpAddr> = info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                addrs.sort_by_key(|a| (!a.is_ipv4(), a.is_loopback()));
                let Some(ip) = addrs.first().copied() else { return };
                let name = info
                    .get_fullname()
                    .trim_end_matches(MDNS_SERVICE)
                    .trim_end_matches('.')
                    .to_owned();
                let presenter = Presenter {
                    name,
                    addr: SocketAddr::new(ip, info.get_port()),
                    fingerprint: fp.to_owned(),
                    resolution: info.get_property_val_str("res").map(str::to_owned),
                };
                self.found.retain(|p| p.fingerprint != presenter.fingerprint);
                self.found.push(presenter);
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                let name = fullname.trim_end_matches(MDNS_SERVICE).trim_end_matches('.');
                self.found.retain(|p| p.name != name);
            }
            _ => {}
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(MDNS_SERVICE);
        let _ = self.daemon.shutdown();
    }
}
