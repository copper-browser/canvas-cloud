//! TLS: self-signed certificates (rcgen, pinned by fingerprint), ACME (rustls-acme,
//! TLS-ALPN-01) and plain HTTP behind a proxy. All rustls usage goes through the `ring`
//! provider.

use std::fs;
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context as _};
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use futures::StreamExt as _;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

use crate::config::{Config, TlsMode};

/// Validity of generated self-signed certificates. Clients pin the exact leaf fingerprint,
/// so a long lifetime avoids silently breaking every pinned client on renewal.
pub const SELF_SIGNED_DAYS: i64 = 3650;

/// The process-wide rustls crypto provider (`ring`).
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Install `ring` as the rustls default provider (idempotent).
pub fn install_default_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// SAN list for a self-signed certificate: the public host (+ `tls.domain`), plus localhost.
pub fn self_signed_names(cfg: &Config) -> Vec<String> {
    let mut names = vec![cfg.public_host()];
    if let Some(d) = &cfg.tls.domain {
        names.push(d.clone());
    }
    for extra in ["localhost", "127.0.0.1"] {
        names.push(extra.to_owned());
    }
    names.dedup();
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    names
}

/// Generate a self-signed ECDSA P-256 certificate for `names` (IP literals become IP SANs).
/// Writes the key with mode 0600 and the cert with 0644, creating parent dirs.
pub fn generate_self_signed(
    names: &[String],
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<()> {
    let primary = names
        .first()
        .context("at least one certificate name is required")?;
    let mut params = rcgen::CertificateParams::new(names.to_vec()).context("certificate names")?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, primary.as_str());
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "canvas-cloud");
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(SELF_SIGNED_DAYS);
    params.serial_number = Some(rcgen::SerialNumber::from_slice(
        &crate::ids::random_bytes::<16>(),
    ));
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.is_ca = rcgen::IsCa::NoCa;
    let key = rcgen::KeyPair::generate().context("generating key pair")?;
    let cert = params.self_signed(&key).context("signing certificate")?;
    write_atomic(key_path, key.serialize_pem().as_bytes(), 0o600)?;
    write_atomic(cert_path, cert.pem().as_bytes(), 0o644)?;
    Ok(())
}

pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(mode);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    Ok(())
}

/// For `self-signed` mode: create the certificate if either file is missing.
/// Returns `true` when a new certificate was generated.
pub fn ensure_self_signed(cfg: &Config) -> anyhow::Result<bool> {
    if cfg.tls.mode != TlsMode::SelfSigned {
        return Ok(false);
    }
    if cfg.tls.cert_path.exists() && cfg.tls.key_path.exists() {
        return Ok(false);
    }
    generate_self_signed(
        &self_signed_names(cfg),
        &cfg.tls.cert_path,
        &cfg.tls.key_path,
    )?;
    tracing::info!(cert = %cfg.tls.cert_path.display(), "generated self-signed certificate");
    Ok(true)
}

/// First certificate (the leaf) in a PEM file, DER-encoded.
pub fn read_leaf_der(cert_path: &Path) -> anyhow::Result<CertificateDer<'static>> {
    CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("reading {}", cert_path.display()))?
        .next()
        .with_context(|| format!("no certificate in {}", cert_path.display()))?
        .with_context(|| format!("parsing {}", cert_path.display()))
}

/// Lower-case hex SHA-256 of the leaf certificate DER (what clients pin).
pub fn leaf_fingerprint(cert_path: &Path) -> anyhow::Result<String> {
    Ok(crate::ids::sha256_hex(&read_leaf_der(cert_path)?))
}

/// Facts about a certificate file (for `doctor`).
#[derive(Debug, Clone)]
pub struct CertInfo {
    pub fingerprint: String,
    pub subject: String,
    pub names: Vec<String>,
    pub not_before: time::OffsetDateTime,
    pub not_after: time::OffsetDateTime,
}

pub fn cert_info(cert_path: &Path) -> anyhow::Result<CertInfo> {
    let der = read_leaf_der(cert_path)?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der)
        .map_err(|e| anyhow::anyhow!("parsing certificate: {e}"))?;
    let mut names = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for n in &san.value.general_names {
            match n {
                x509_parser::extensions::GeneralName::DNSName(d) => names.push((*d).to_owned()),
                x509_parser::extensions::GeneralName::IPAddress(b) => match b.len() {
                    4 => names.push(IpAddr::from(<[u8; 4]>::try_from(*b)?).to_string()),
                    16 => names.push(IpAddr::from(<[u8; 16]>::try_from(*b)?).to_string()),
                    _ => {}
                },
                _ => {}
            }
        }
    }
    let ts = |t: x509_parser::time::ASN1Time| {
        time::OffsetDateTime::from_unix_timestamp(t.timestamp())
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
    };
    Ok(CertInfo {
        fingerprint: crate::ids::sha256_hex(&der),
        subject: cert.subject().to_string(),
        names,
        not_before: ts(cert.validity().not_before),
        not_after: ts(cert.validity().not_after),
    })
}

/// rustls server config from PEM cert chain + key, ALPN h2 + http/1.1.
pub fn server_config_from_files(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<Arc<ServerConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("reading {}", cert_path.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing {}", cert_path.display()))?;
    if certs.is_empty() {
        bail!("no certificates in {}", cert_path.display());
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .with_context(|| format!("reading private key {}", key_path.display()))?;
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate/key mismatch or unsupported key")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Serve `app` on `cfg.listen` per `cfg.tls.mode` until `handle` shuts it down.
pub async fn serve(app: Router, cfg: &Config, handle: Handle<SocketAddr>) -> anyhow::Result<()> {
    let make_service = app.into_make_service_with_connect_info::<SocketAddr>();
    match cfg.tls.mode {
        TlsMode::Off => {
            tracing::warn!(listen = %cfg.listen, "TLS is OFF: plain HTTP (only behind a TLS-terminating proxy)");
            axum_server::bind(cfg.listen)
                .handle(handle)
                .serve(make_service)
                .await
                .context("http server")?;
        }
        TlsMode::SelfSigned => {
            ensure_self_signed(cfg)?;
            let config = server_config_from_files(&cfg.tls.cert_path, &cfg.tls.key_path)?;
            tracing::info!(
                listen = %cfg.listen,
                fingerprint = %leaf_fingerprint(&cfg.tls.cert_path)?,
                "serving HTTPS (self-signed)"
            );
            axum_server::bind_rustls(cfg.listen, RustlsConfig::from_config(config))
                .handle(handle)
                .serve(make_service)
                .await
                .context("https server")?;
        }
        TlsMode::Acme => {
            let acceptor = acme_acceptor(cfg)?;
            tracing::info!(listen = %cfg.listen, domain = ?cfg.tls.domain, "serving HTTPS (ACME)");
            axum_server::bind(cfg.listen)
                .acceptor(acceptor)
                .handle(handle)
                .serve(make_service)
                .await
                .context("https server (acme)")?;
        }
    }
    Ok(())
}

/// Build the ACME acceptor and spawn the certificate order/renewal driver.
fn acme_acceptor(cfg: &Config) -> anyhow::Result<rustls_acme::axum::AxumAcceptor> {
    let domain = cfg
        .tls
        .domain
        .clone()
        .context("tls.domain is required for ACME")?;
    let cache_dir: PathBuf = cfg.tls.acme_cache_dir.clone();
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating ACME cache dir {}", cache_dir.display()))?;
    let mut acme = rustls_acme::AcmeConfig::new([domain.as_str()])
        .cache(rustls_acme::caches::DirCache::new(cache_dir))
        .directory_lets_encrypt(!cfg.tls.acme_staging);
    if let Some(email) = &cfg.tls.acme_email {
        acme = acme.contact_push(format!("mailto:{email}"));
    }
    let mut state = acme.state();
    let mut server_config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(state.resolver());
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = state.axum_acceptor(Arc::new(server_config));
    tokio::spawn(async move {
        while let Some(event) = state.next().await {
            match event {
                Ok(ok) => tracing::info!(event = ?ok, "acme"),
                Err(err) => tracing::error!(error = ?err, "acme"),
            }
        }
    });
    Ok(acceptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_roundtrip() {
        let dir = std::env::temp_dir().join(format!("cvc-tls-{}", crate::ids::random_token()));
        let (cert, key) = (dir.join("tls/cert.pem"), dir.join("tls/key.pem"));
        let names = vec!["203.0.113.5".to_owned(), "localhost".to_owned()];
        generate_self_signed(&names, &cert, &key).unwrap();
        let info = cert_info(&cert).unwrap();
        assert_eq!(info.fingerprint, leaf_fingerprint(&cert).unwrap());
        assert_eq!(info.fingerprint.len(), 64);
        assert!(info.names.contains(&"203.0.113.5".to_owned()));
        assert!(info.names.contains(&"localhost".to_owned()));
        assert!(info.not_after > time::OffsetDateTime::now_utc() + time::Duration::days(3000));
        server_config_from_files(&cert, &key).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&key).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
