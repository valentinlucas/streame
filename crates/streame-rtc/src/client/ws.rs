//! Réseau côté téléphone : WebSocket de signaling (`wss://<mac>:<port>/ws`) et lecture de
//! `GET /api/config`, tous deux en TLS. La régie génère un certificat auto-signé (voir
//! `certs/`) : il n'est pas vérifiable par une autorité, il est **mémorisé à la première
//! connexion** (« trust on first use ») — l'app conserve son empreinte SHA-256 par régie et la
//! bibliothèque refuse ensuite un certificat différent ([`CertPolicy`]).

use crate::signaling::ServerConfig;
use anyhow::{anyhow, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Délai maximal pour joindre la régie (TCP + TLS + WebSocket). Sans limite, une adresse qui
/// ne répond pas (accès au réseau local refusé sur iOS, isolation Wi-Fi…) bloquait sans fin.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// Empreinte SHA-256 du certificat (DER) de la régie.
pub type Fingerprint = [u8; 32];

/// Empreinte attendue (mémorisée par l'app) et empreinte observée à la dernière poignée de main.
#[derive(Debug, Default)]
pub struct CertPolicy {
    pub expected: Option<Fingerprint>,
    observed: Mutex<Option<Fingerprint>>,
}

impl CertPolicy {
    pub fn new(expected: Option<Fingerprint>) -> Self {
        Self { expected, observed: Mutex::new(None) }
    }

    /// Empreinte du certificat présenté par la régie (même refusé).
    pub fn observed(&self) -> Option<Fingerprint> {
        *self.observed.lock().unwrap()
    }

    /// La régie a présenté un certificat différent de celui mémorisé.
    pub fn mismatch(&self) -> bool {
        matches!((self.expected, self.observed()), (Some(e), Some(o)) if e != o)
    }
}

/// `aa:bb:…` ou `aabb…`, insensible à la casse.
pub fn parse_fingerprint(text: &str) -> Option<Fingerprint> {
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn fingerprint_hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Vérificateur : accepte le certificat à la première connexion, exige ensuite le même.
#[derive(Debug)]
struct PinVerifier {
    policy: Arc<CertPolicy>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        let mut fp = [0u8; 32];
        fp.copy_from_slice(digest.as_ref());
        *self.policy.observed.lock().unwrap() = Some(fp);
        if let Some(expected) = self.policy.expected {
            if expected != fp {
                return Err(rustls::Error::General(
                    "certificat de la régie différent de celui mémorisé".into(),
                ));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Configuration TLS cliente (ring) : certificat vérifié par empreinte selon `policy`.
pub fn tls_config(policy: Arc<CertPolicy>) -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions TLS")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier { policy }))
        .with_no_client_auth();
    Arc::new(cfg)
}

/// Ouvre le WebSocket de signaling.
pub async fn connect(host: &str, port: u16, tls: Arc<rustls::ClientConfig>) -> Result<Socket> {
    let url = format!("wss://{}:{port}/ws", bracket(host));
    let request = url.as_str().into_client_request().context("URL WebSocket")?;
    let (socket, _response) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_async_tls_with_config(request, None, true, Some(Connector::Rustls(tls))),
    )
    .await
    .map_err(|_| anyhow!("pas de réponse de {url} en {} s", CONNECT_TIMEOUT.as_secs()))?
    .with_context(|| format!("connexion WebSocket à {url}"))?;
    Ok(socket)
}

/// Lit `GET /api/config` (JSON) : requête HTTP/1.1 minimale sur TLS, réponse lue jusqu'à la
/// fermeture (`Connection: close`).
pub async fn fetch_config(host: &str, port: u16, tls: Arc<rustls::ClientConfig>) -> Result<ServerConfig> {
    tokio::time::timeout(CONNECT_TIMEOUT, fetch_config_inner(host, port, tls))
        .await
        .map_err(|_| anyhow!("pas de réponse de {host}:{port} en {} s", CONNECT_TIMEOUT.as_secs()))?
}

async fn fetch_config_inner(host: &str, port: u16, tls: Arc<rustls::ClientConfig>) -> Result<ServerConfig> {
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connexion TCP à {host}:{port}"))?;
    let _ = tcp.set_nodelay(true);
    let name = ServerName::try_from(host.to_string()).context("nom de serveur TLS")?;
    let connector = tokio_rustls::TlsConnector::from(tls);
    let mut tls = connector.connect(name, tcp).await.context("poignée de main TLS")?;
    let req = format!(
        "GET /api/config HTTP/1.1\r\nHost: {}:{port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n",
        bracket(host)
    );
    tls.write_all(req.as_bytes()).await?;
    let mut buf = Vec::with_capacity(1024);
    // Le serveur ferme après la réponse ; une erreur de fermeture TLS abrupte est tolérée.
    let _ = tls.read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("réponse HTTP incomplète"))?;
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200 ") {
        return Err(anyhow!("/api/config : {status}"));
    }
    let cfg: ServerConfig = serde_json::from_str(body.trim()).context("JSON de /api/config")?;
    Ok(cfg)
}

/// IPv6 littérale entre crochets dans une URL / un en-tête Host.
fn bracket(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empreinte_aller_retour() {
        let fp: Fingerprint = core::array::from_fn(|i| i as u8 * 7);
        let hex = fingerprint_hex(&fp);
        assert_eq!(parse_fingerprint(&hex), Some(fp));
        let colons: Vec<String> = hex.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_uppercase()).collect();
        assert_eq!(parse_fingerprint(&colons.join(":")), Some(fp));
        assert_eq!(parse_fingerprint("abc"), None);
    }

    #[test]
    fn politique_premiere_connexion_puis_changement() {
        let policy = Arc::new(CertPolicy::new(None));
        let verifier = PinVerifier { policy: policy.clone() };
        let cert = CertificateDer::from(vec![1u8, 2, 3]);
        let name = ServerName::try_from("192.168.1.10").unwrap();
        assert!(verifier.verify_server_cert(&cert, &[], &name, &[], UnixTime::now()).is_ok());
        let seen = policy.observed().expect("empreinte observée");
        assert!(!policy.mismatch());

        let pinned = Arc::new(CertPolicy::new(Some(seen)));
        let verifier = PinVerifier { policy: pinned.clone() };
        assert!(verifier.verify_server_cert(&cert, &[], &name, &[], UnixTime::now()).is_ok());
        let other = CertificateDer::from(vec![9u8, 9, 9]);
        assert!(verifier.verify_server_cert(&other, &[], &name, &[], UnixTime::now()).is_err());
        assert!(pinned.mismatch());
    }
}
