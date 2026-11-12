//! Each install generates one self-signed certificate on first start and keeps
//! it. Peers identify each other by the SHA-256 fingerprint of that certificate,
//! like SSH host keys.

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const CERT_SERVER_NAME: &str = "mirrorwire.local";

#[derive(Clone)]
pub struct Identity {
    pub cert: CertificateDer<'static>,
    key: Vec<u8>,
}

impl Identity {
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let cert_path = dir.join("identity.cert.der");
        let key_path = dir.join("identity.key.der");
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            return Ok(Self {
                cert: CertificateDer::from(cert),
                key,
            });
        }
        let generated = Self::generate()?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        write_private(&key_path, &generated.key)?;
        std::fs::write(&cert_path, generated.cert.as_ref())?;
        tracing::info!("generated new identity in {}", dir.display());
        Ok(generated)
    }

    pub fn generate() -> Result<Self> {
        let ck = rcgen::generate_simple_self_signed(vec![CERT_SERVER_NAME.to_owned()])?;
        Ok(Self {
            cert: ck.cert.der().clone(),
            key: ck.signing_key.serialize_der(),
        })
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }
}

pub fn fingerprint(cert: &[u8]) -> String {
    hex::encode(Sha256::digest(cert))
}

/// First 4 byte groups of a fingerprint, for showing to humans.
pub fn short_fingerprint(fp: &str) -> String {
    fp.as_bytes()
        .chunks(4)
        .take(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(":")
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Ok(std::fs::write(path, data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_identity() {
        let dir = std::env::temp_dir().join(format!("mirrorwire-id-test-{}", std::process::id()));
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint().len(), 64);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
