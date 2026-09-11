//! Certificat TLS auto-signé (la caméra du téléphone exige HTTPS).

use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

pub struct TlsFiles {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// Charge `cert.pem`/`key.pem` depuis `dir`, ou les génère.
pub fn load_or_create(dir: &Path, extra_names: &[String]) -> Result<TlsFiles> {
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if cert_path.is_file() && key_path.is_file() {
        return Ok(TlsFiles {
            cert_pem: std::fs::read(&cert_path)?,
            key_pem: std::fs::read(&key_path)?,
        });
    }
    std::fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
    let mut names: Vec<String> = vec!["localhost".into(), "streame.local".into()];
    names.extend(extra_names.iter().cloned());
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(names).context("génération du certificat")?;
    let cert_pem = cert.pem().into_bytes();
    let key_pem = key_pair.serialize_pem().into_bytes();
    std::fs::write(&cert_path, &cert_pem)?;
    std::fs::write(&key_path, &key_pem)?;
    info!("certificat TLS auto-signé généré dans {}", dir.display());
    Ok(TlsFiles { cert_pem, key_pem })
}
