//! `canvas-cloud doctor` and the `/healthz` probe (self-signed: the presented certificate must
//! be the configured one; no external tools needed).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{bail, Context as _};
use canvas_cloud_core::config::{redact_database_url, Config, ConfigSource, TlsMode};
use canvas_cloud_core::{db, ids, tls};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Report {
    failed: bool,
}

impl Report {
    fn line(&mut self, level: Level, check: &str, detail: impl std::fmt::Display) {
        let tag = match level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => {
                self.failed = true;
                "FAIL"
            }
        };
        println!("  {tag}  {check:<12} {detail}");
    }
}

/// Run every check, printing one line each. Returns `true` when nothing failed.
pub async fn run(source: &ConfigSource) -> bool {
    println!("canvas-cloud doctor ({})", crate::version());
    let mut r = Report { failed: false };

    let cfg = match Config::load(source) {
        Ok(cfg) => {
            r.line(Level::Ok, "config", source.path.display());
            cfg
        }
        Err(err) => {
            r.line(Level::Fail, "config", format!("{err:#}"));
            return false;
        }
    };
    check_config_file_mode(&mut r, source);

    check_database(&mut r, &cfg).await;
    check_tls(&mut r, &cfg);
    check_listener(&mut r, &cfg).await;
    check_web(&mut r, &cfg);
    check_sign_in(&mut r, &cfg);

    match cfg.metrics_listen {
        Some(bind) if !bind.ip().is_loopback() => r.line(
            Level::Warn,
            "metrics",
            format!("{bind} is not loopback — make sure it is firewalled"),
        ),
        Some(bind) => r.line(Level::Ok, "metrics", format!("http://{bind}/metrics")),
        None => r.line(Level::Ok, "metrics", "disabled"),
    }
    if cfg.trust_proxy && cfg.tls.mode != TlsMode::Off {
        r.line(
            Level::Warn,
            "proxy",
            "trust_proxy = true while serving TLS directly: clients can spoof X-Forwarded-For",
        );
    }

    println!(
        "{}",
        if r.failed {
            "doctor: problems found"
        } else {
            "doctor: all checks passed"
        }
    );
    !r.failed
}

/// The sign-in providers (already validated by the config load): one line each, so an
/// operator sees what the sign-in buttons will be and where a delegated one sends people.
fn check_sign_in(r: &mut Report, cfg: &Config) {
    if cfg.oidc.is_empty() && cfg.delegated_sign_in.is_empty() {
        r.line(
            Level::Ok,
            "sign-in",
            "password only (no [[oidc]] or [[delegated_sign_in]])",
        );
        return;
    }
    for p in &cfg.oidc {
        r.line(
            Level::Ok,
            "sign-in",
            format!("oidc {} \"{}\" → {}", p.id, p.label(), p.issuer),
        );
    }
    for d in &cfg.delegated_sign_in {
        r.line(
            Level::Ok,
            "sign-in",
            format!(
                "delegated {} \"{}\" → {} (the delegate mints sessions with an app key that has allow_sessions and trust_email)",
                d.id,
                d.label(),
                d.start_url
            ),
        );
    }
}

fn check_config_file_mode(r: &mut Report, source: &ConfigSource) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(meta) = std::fs::metadata(&source.path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                r.line(
                    Level::Warn,
                    "config",
                    format!("mode {mode:o}: the file holds secrets, chmod 600 it"),
                );
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (r, source);
}

async fn check_database(r: &mut Report, cfg: &Config) {
    let shown = redact_database_url(&cfg.database_url);
    let pool = match tokio::time::timeout(Duration::from_secs(10), db::connect(cfg)).await {
        Ok(Ok(pool)) => pool,
        Ok(Err(err)) => {
            r.line(Level::Fail, "database", format!("{err:#}"));
            return;
        }
        Err(_) => {
            r.line(
                Level::Fail,
                "database",
                format!("timed out connecting to {shown}"),
            );
            return;
        }
    };
    match sqlx::query_scalar::<_, String>("SHOW server_version")
        .fetch_one(&pool)
        .await
    {
        Ok(v) => r.line(Level::Ok, "database", format!("PostgreSQL {v} ({shown})")),
        Err(err) => r.line(Level::Fail, "database", err),
    }
    match db::pending_migrations(&pool).await {
        Ok((applied, pending)) if pending.is_empty() => {
            r.line(
                Level::Ok,
                "migrations",
                format!("{applied} applied, none pending"),
            );
        }
        Ok((applied, pending)) => r.line(
            Level::Fail,
            "migrations",
            format!(
                "{applied} applied, pending: {} — run `canvas-cloud migrate`",
                pending.join(", ")
            ),
        ),
        Err(err) => r.line(Level::Fail, "migrations", format!("{err:#}")),
    }
    let counts: Result<(i64, i64, i64), _> = sqlx::query_as(
        "SELECT (SELECT count(*) FROM users),
                (SELECT count(*) FROM sessions WHERE expires_at > now()),
                (SELECT count(*) FROM apps WHERE revoked_at IS NULL)",
    )
    .fetch_one(&pool)
    .await;
    let policy = canvas_cloud_core::identity::SignupPolicy::load(&pool).await;
    if let (Ok((users, sessions, apps)), Ok(policy)) = (counts, policy) {
        r.line(
            Level::Ok,
            "accounts",
            format!(
                "{users} users, {sessions} live sessions, {apps} linked apps, signup {}",
                if policy.first_user {
                    "open (no password account yet)"
                } else if policy.allow_signup {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
        );
    }
    check_app_sessions(r, cfg, &pool).await;
    pool.close().await;
}

/// Which linked apps may sign their users in (mint `cvs_` sessions): an app key with that
/// power can act as anyone it can link by email, so an operator should see the list.
async fn check_app_sessions(r: &mut Report, cfg: &Config, pool: &sqlx::PgPool) {
    let Ok(apps) = canvas_cloud_core::identity::list_apps(pool).await else {
        return;
    };
    let mut minting = 0_usize;
    for a in apps.iter().filter(|a| a.revoked_at.is_none()) {
        if a.can_mint_sessions() {
            minting += 1;
            r.line(
                Level::Ok,
                "app-sessions",
                format!(
                    "app \"{}\" ({}) may sign its users in; {} live session(s) minted",
                    a.name, a.id, a.live_sessions
                ),
            );
        } else if a.allow_sessions {
            r.line(
                Level::Warn,
                "app-sessions",
                format!(
                    "app \"{}\" ({}) has allow_sessions but not trust_email: it cannot sign users in",
                    a.name, a.id
                ),
            );
        }
    }
    if minting == 0 {
        if cfg.delegated_sign_in.is_empty() {
            r.line(Level::Ok, "app-sessions", "no linked app may sign users in");
        } else {
            r.line(
                Level::Warn,
                "app-sessions",
                "[[delegated_sign_in]] is configured but no linked app may sign users in \
                 (canvas-cloud apps set <app> --allow-sessions true)",
            );
        }
    }
}

fn check_tls(r: &mut Report, cfg: &Config) {
    match cfg.tls.mode {
        TlsMode::Off => r.line(
            Level::Warn,
            "tls",
            "off — plain HTTP; only valid behind a TLS-terminating proxy",
        ),
        TlsMode::Acme => {
            let dir = &cfg.tls.acme_cache_dir;
            let domain = cfg.tls.domain.as_deref().unwrap_or("?");
            if dir.is_dir() {
                r.line(
                    Level::Ok,
                    "tls",
                    format!("acme for {domain}, cache {}", dir.display()),
                );
            } else {
                r.line(
                    Level::Warn,
                    "tls",
                    format!(
                        "acme for {domain}: cache dir {} does not exist yet",
                        dir.display()
                    ),
                );
            }
            if cfg.listen.port() != 443 {
                r.line(
                    Level::Warn,
                    "tls",
                    "ACME TLS-ALPN-01 needs the server reachable on port 443",
                );
            }
        }
        TlsMode::SelfSigned => match tls::cert_info(&cfg.tls.cert_path) {
            Ok(info) => {
                let now = time::OffsetDateTime::now_utc();
                let days_left = (info.not_after - now).whole_days();
                let level = if days_left < 0 {
                    Level::Fail
                } else if days_left < 30 {
                    Level::Warn
                } else {
                    Level::Ok
                };
                r.line(
                    level,
                    "tls",
                    format!(
                        "self-signed, fp={}, {} days left, names: {}",
                        info.fingerprint,
                        days_left,
                        info.names.join(", ")
                    ),
                );
                let host = cfg.public_host();
                if !info.names.iter().any(|n| n.eq_ignore_ascii_case(&host)) {
                    r.line(
                        Level::Warn,
                        "tls",
                        format!("certificate does not name public host {host}"),
                    );
                }
                if let Err(err) =
                    tls::server_config_from_files(&cfg.tls.cert_path, &cfg.tls.key_path)
                {
                    r.line(Level::Fail, "tls", format!("{err:#}"));
                }
            }
            Err(err) => r.line(
                Level::Fail,
                "tls",
                format!("{err:#} — run `canvas-cloud tls-init`"),
            ),
        },
    }
}

async fn check_listener(r: &mut Report, cfg: &Config) {
    match probe_healthz(cfg).await {
        Ok(detail) => r.line(Level::Ok, "listener", format!("{} {detail}", cfg.listen)),
        Err(err) => {
            // Distinguish "nothing listening" from "something else is on the port".
            let bindable = std::net::TcpListener::bind(cfg.listen).is_ok();
            let hint = if bindable {
                "nothing is listening — is the service running? (systemctl status canvas-cloud)"
            } else {
                "port is in use but /healthz did not answer correctly"
            };
            r.line(
                Level::Fail,
                "listener",
                format!("{}: {err:#} — {hint}", cfg.listen),
            );
        }
    }
}

fn check_web(r: &mut Report, cfg: &Config) {
    match &cfg.web_root {
        Some(root) => match cfg.check_web_root() {
            Ok(()) => r.line(Level::Ok, "web", format!("serving {} at /", root.display())),
            Err(err) => r.line(Level::Fail, "web", format!("{err:#}")),
        },
        None => r.line(Level::Ok, "web", "no web_root (API only)"),
    }
    match cfg.effective_web_app_url() {
        Some(url) => r.line(Level::Ok, "share links", format!("open {url}")),
        None => r.line(
            Level::Ok,
            "share links",
            "no web_app_url: /join/{token} shows a plain page",
        ),
    }
    if !cfg.cors.allowed_origins.is_empty() {
        r.line(
            Level::Ok,
            "cors",
            format!("allowed origins: {}", cfg.cors.allowed_origins.join(", ")),
        );
    }
}

/// Wait up to `wait_secs` for `/healthz` to answer.
pub async fn wait_healthy(cfg: &Config, wait_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
    loop {
        if probe_healthz(cfg).await.is_ok() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Address to probe for a listen address (`0.0.0.0` → loopback).
fn probe_addr(listen: SocketAddr) -> SocketAddr {
    let ip = match listen.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, listen.port())
}

/// GET /healthz on the local listener. Self-signed: the presented leaf must match the
/// configured certificate's fingerprint.
pub async fn probe_healthz(cfg: &Config) -> anyhow::Result<String> {
    probe_healthz_at(cfg, probe_addr(cfg.listen)).await
}

pub async fn probe_healthz_at(cfg: &Config, addr: SocketAddr) -> anyhow::Result<String> {
    let tcp = tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr))
        .await
        .context("connect timed out")?
        .with_context(|| format!("connecting to {addr}"))?;
    match cfg.tls.mode {
        TlsMode::Off => {
            let status = http_get_healthz(tcp, "localhost").await?;
            Ok(format!("answering /healthz over plain HTTP ({status})"))
        }
        TlsMode::SelfSigned | TlsMode::Acme => {
            let expected = if cfg.tls.mode == TlsMode::SelfSigned {
                Some(tls::leaf_fingerprint(&cfg.tls.cert_path)?)
            } else {
                None
            };
            let sni = cfg
                .tls
                .domain
                .clone()
                .unwrap_or_else(|| "localhost".to_owned());
            let (stream, seen) = tls_connect(tcp, &sni, expected.clone()).await?;
            let status = http_get_healthz(stream, &sni).await?;
            Ok(match expected {
                Some(_) => format!("answering /healthz over HTTPS ({status}), fingerprint matches"),
                None => format!("answering /healthz over HTTPS ({status}), leaf fp={seen}"),
            })
        }
    }
}

/// TLS-connect with fingerprint pinning (`expected = None` accepts any certificate, for a
/// liveness probe). Returns the stream and the observed leaf fingerprint.
pub async fn tls_connect(
    tcp: tokio::net::TcpStream,
    sni: &str,
    expected: Option<String>,
) -> anyhow::Result<(
    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    String,
)> {
    let provider = tls::provider();
    let seen = Arc::new(Mutex::new(None));
    let verifier = PinnedVerifier {
        expected,
        provider: provider.clone(),
        seen: seen.clone(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let name = ServerName::try_from(sni.to_owned()).context("invalid TLS server name")?;
    let stream = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp),
    )
    .await
    .context("TLS handshake timed out")?
    .context("TLS handshake")?;
    let fp = seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_default();
    Ok((stream, fp))
}

async fn http_get_healthz<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    host: &str,
) -> anyhow::Result<String> {
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: canvas-cloud-doctor\r\n\r\n");
    tokio::time::timeout(PROBE_TIMEOUT, async {
        s.write_all(req.as_bytes()).await?;
        s.flush().await?;
        let mut buf = Vec::with_capacity(512);
        let mut chunk = [0u8; 512];
        loop {
            let n = s.read(&mut chunk).await?;
            if n == 0 || buf.len() > 8192 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.ends_with(b"ok") {
                break;
            }
        }
        anyhow::Ok(buf)
    })
    .await
    .context("reading /healthz timed out")?
    .map(|buf| {
        let text = String::from_utf8_lossy(&buf);
        let status = text.lines().next().unwrap_or_default().to_owned();
        (status, text.ends_with("ok"))
    })
    .and_then(|(status, body_ok)| {
        if status.starts_with("HTTP/1.1 200") && body_ok {
            Ok(status)
        } else {
            bail!("unexpected response: {status:?}")
        }
    })
}

#[derive(Debug)]
struct PinnedVerifier {
    expected: Option<String>,
    provider: Arc<CryptoProvider>,
    seen: Arc<Mutex<Option<String>>>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = ids::sha256_hex(end_entity);
        let matches = self.expected.as_deref().is_none_or(|e| e == fp);
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(fp);
        if matches {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate fingerprint does not match the configured certificate".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
