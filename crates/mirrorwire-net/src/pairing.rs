//! Connection details travel as a `mirrorwire://host:port?fp=..&code=..&name=..`
//! URI, shown as a QR code by the presenter and scanned or pasted by the viewer.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectInfo {
    pub addr: SocketAddr,
    pub fingerprint: String,
    pub pairing_code: Option<String>,
    pub name: Option<String>,
}

impl ConnectInfo {
    pub fn to_uri(&self) -> String {
        let mut url = url::Url::parse(&format!("mirrorwire://{}", self.addr)).expect("valid socket addr");
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("fp", &self.fingerprint);
            if let Some(code) = &self.pairing_code {
                q.append_pair("code", code);
            }
            if let Some(name) = &self.name {
                q.append_pair("name", name);
            }
        }
        url.to_string()
    }

    pub fn parse(uri: &str) -> Result<Self> {
        let url = url::Url::parse(uri.trim()).context("invalid mirrorwire URI")?;
        if url.scheme() != "mirrorwire" {
            bail!("expected mirrorwire:// URI");
        }
        let host = url.host_str().ok_or_else(|| anyhow!("URI has no host"))?;
        let ip: IpAddr = host.trim_matches(|c| c == '[' || c == ']').parse()?;
        let port = url.port().ok_or_else(|| anyhow!("URI has no port"))?;
        let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        let fingerprint = params
            .get("fp")
            .cloned()
            .ok_or_else(|| anyhow!("URI has no fp parameter"))?;
        if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("fingerprint must be 64 hex characters");
        }
        Ok(Self {
            addr: SocketAddr::new(ip, port),
            fingerprint,
            pairing_code: params.get("code").cloned(),
            name: params.get("name").cloned(),
        })
    }
}

pub fn qr_terminal(data: &str) -> Result<String> {
    let code = qrcode::QrCode::new(data.as_bytes())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

/// QR modules as a square boolean matrix (true = dark), for drawing in a GUI.
pub fn qr_matrix(data: &str) -> Result<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(data.as_bytes())?;
    let width = code.width();
    let modules = code
        .to_colors()
        .into_iter()
        .map(|c| c == qrcode::Color::Dark)
        .collect();
    Ok((width, modules))
}

pub fn random_pairing_code() -> Result<String> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("random: {e}"))?;
    Ok(format!("{:06}", u32::from_le_bytes(buf) % 1_000_000))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedViewer {
    pub fingerprint: String,
    pub name: String,
}

/// Viewers that have presented a valid pairing code once and are trusted since.
pub struct PairedViewers {
    path: PathBuf,
    entries: Vec<PairedViewer>,
}

impl PairedViewers {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("paired_viewers.json");
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn contains(&self, fingerprint: &str) -> bool {
        self.entries.iter().any(|e| e.fingerprint == fingerprint)
    }

    pub fn add(&mut self, fingerprint: &str, name: &str) -> Result<()> {
        if self.contains(fingerprint) {
            return Ok(());
        }
        self.entries.push(PairedViewer {
            fingerprint: fingerprint.to_owned(),
            name: name.to_owned(),
        });
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_vec_pretty(&self.entries)?)?;
        Ok(())
    }
}

pub fn primary_ipv4() -> Option<IpAddr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .filter(|i| !i.is_loopback())
        .map(|i| i.ip())
        .find(IpAddr::is_ipv4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let info = ConnectInfo {
            addr: "192.168.1.20:47800".parse().unwrap(),
            fingerprint: "ab".repeat(32),
            pairing_code: Some("012345".into()),
            name: Some("T480s laptop".into()),
        };
        let uri = info.to_uri();
        assert!(uri.starts_with("mirrorwire://192.168.1.20:47800?"));
        assert_eq!(ConnectInfo::parse(&uri).unwrap(), info);
        assert!(qr_terminal(&uri).unwrap().lines().count() > 10);
    }

    #[test]
    fn rejects_bad_fingerprint() {
        assert!(ConnectInfo::parse("mirrorwire://127.0.0.1:1?fp=xyz").is_err());
    }
}
