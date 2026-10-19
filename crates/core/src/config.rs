//! Configuration: a TOML file (default `/etc/canvas-cloud/canvas-cloud.toml`) overlaid with
//! environment variables `CANVAS_CLOUD_<KEY>`; nested keys use `__`
//! (`CANVAS_CLOUD_TLS__MODE=acme`, `CANVAS_CLOUD_LIMITS__AUTH_PER_MINUTE=20`). List values
//! (`cors.allowed_origins`, `ops.extra_url_schemes`) are comma-separated in the environment.

use std::fmt::{self, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/canvas-cloud/canvas-cloud.toml";
pub const ENV_PREFIX: &str = "CANVAS_CLOUD_";
/// Environment variable naming the config file.
pub const CONFIG_ENV: &str = "CANVAS_CLOUD_CONFIG";
pub const DEFAULT_METRICS_LISTEN: &str = "127.0.0.1:9474";
/// Where state (certificates, ACME cache) lives by default.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/canvas-cloud";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    /// rcgen self-signed certificate; clients pin its SHA-256 fingerprint.
    #[default]
    SelfSigned,
    /// Let's Encrypt via TLS-ALPN-01 (rustls-acme) for `tls.domain`. Needs :443 reachable.
    Acme,
    /// Plain HTTP. Only on loopback or behind a TLS-terminating reverse proxy.
    Off,
}

impl fmt::Display for TlsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SelfSigned => "self-signed",
            Self::Acme => "acme",
            Self::Off => "off",
        })
    }
}

impl TlsMode {
    /// Parses `self-signed` (also `selfsigned`, `self_signed`), `acme` or `off`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "self-signed" | "selfsigned" | "self_signed" => Some(Self::SelfSigned),
            "acme" => Some(Self::Acme),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TlsConfig {
    pub mode: TlsMode,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// Required for `acme`; also used as the certificate SAN / public host when set.
    pub domain: Option<String>,
    pub acme_email: Option<String>,
    pub acme_cache_dir: PathBuf,
    /// Use the Let's Encrypt staging directory (testing).
    pub acme_staging: bool,
}

/// `[cors]`: which browser origins may call the API (bearer tokens, no cookies).
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct CorsConfig {
    /// Exact-match origins (`https://app.example.com`); empty = no CORS headers at all.
    pub allowed_origins: Vec<String>,
}

/// `[ops]`: canvas ops behaviour.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct OpsConfig {
    /// URL schemes accepted in link shapes on top of `http`, `https` and `mailto`
    /// (lower-case, e.g. `["myapp"]`).
    pub extra_url_schemes: Vec<String>,
}

/// One `[[oidc]]` provider (generic authorization code + PKCE). Plain data: core only parses
/// and validates it (and lists it in `GET /v1/info`); the human-auth routes
/// (`/v1/auth/oidc/{id}/…`) live outside core.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OidcProvider {
    /// Path segment in `/v1/auth/oidc/{id}/…` (`[a-z0-9_-]`, ≤ 32).
    pub id: String,
    /// Issuer URL (discovery at `{issuer}/.well-known/openid-configuration`).
    pub issuer: String,
    pub client_id: String,
    /// File holding the client secret (never inline in the config).
    pub client_secret_file: Option<PathBuf>,
    /// Only verified emails in these domains may sign in (empty = any).
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Where the callback may send the browser back to (exact match).
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    /// Human label for sign-in buttons (defaults to `id`).
    pub name: Option<String>,
}

impl OidcProvider {
    /// [`Self::name`], else the id.
    pub fn label(&self) -> &str {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(&self.id)
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Limits {
    /// Requests per minute per client IP on `/v1/auth/*`.
    pub auth_per_minute: u32,
    /// Largest accepted import request body (`POST /v1/apps/self/import`).
    pub import_max_bytes: usize,
    /// Longest single NDJSON line in an import.
    pub import_max_line_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            auth_per_minute: 10,
            import_max_bytes: 512 << 20,
            import_max_line_bytes: 32 << 20,
        }
    }
}

/// Fully validated runtime configuration.
#[derive(Clone)]
#[non_exhaustive]
pub struct Config {
    pub listen: SocketAddr,
    /// Base URL clients use (`https://canvas.example.com[:port]`, no trailing slash).
    pub public_url: String,
    pub database_url: String,
    pub db_max_connections: u32,
    pub master_key: [u8; 32],
    /// Honor `X-Forwarded-For` for client IPs (rate limiting, logs). Only behind a proxy.
    pub trust_proxy: bool,
    pub tls: TlsConfig,
    pub cors: CorsConfig,
    /// Where share-link landing pages (`/join/{token}`) send people, with a `{token}`
    /// placeholder (appended when absent). Unset: `{public_url}/#/join/{token}` when
    /// [`Self::web_root`] is set, else a plain "open this link in your canvas app" page —
    /// see [`Self::effective_web_app_url`].
    pub web_app_url: Option<String>,
    /// A directory of static web-app files (e.g. a built canvas app with a self-contained
    /// `index.html`) served at the root for every GET/HEAD path no route claims.
    pub web_root: Option<PathBuf>,
    pub ops: OpsConfig,
    pub oidc: Vec<OidcProvider>,
    pub limits: Limits,
    /// Prometheus `/metrics` listener; `None` disables it.
    pub metrics_listen: Option<SocketAddr>,
    pub log_format: LogFormat,
    /// `tracing` env-filter directive (overridden by `RUST_LOG` when set).
    pub log_level: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("public_url", &self.public_url)
            .field("database_url", &redact_database_url(&self.database_url))
            .field("db_max_connections", &self.db_max_connections)
            .field("master_key", &"<redacted>")
            .field("trust_proxy", &self.trust_proxy)
            .field("tls", &self.tls)
            .field("cors", &self.cors)
            .field("web_app_url", &self.web_app_url)
            .field("web_root", &self.web_root)
            .field("ops", &self.ops)
            .field(
                "oidc",
                &self.oidc.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            )
            .field("limits", &self.limits)
            .field("metrics_listen", &self.metrics_listen)
            .field("log_format", &self.log_format)
            .field("log_level", &self.log_level)
            .finish()
    }
}

/// `postgres://user:secret@host:5432/db?sslmode=require` → `postgres://user:***@host:5432/db`.
pub fn redact_database_url(url: &str) -> String {
    let no_query = url.split('?').next().unwrap_or_default();
    let Some((scheme, rest)) = no_query.split_once("://") else {
        return "<redacted>".into();
    };
    match rest.rsplit_once('@') {
        Some((userinfo, host)) => {
            let user = userinfo.split(':').next().unwrap_or_default();
            if userinfo.contains(':') {
                format!("{scheme}://{user}:***@{host}")
            } else {
                format!("{scheme}://{user}@{host}")
            }
        }
        None => format!("{scheme}://{rest}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Raw (file + env) representation

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: Option<String>,
    public_url: Option<String>,
    database_url: Option<String>,
    db_max_connections: Option<u32>,
    master_key: Option<String>,
    trust_proxy: Option<bool>,
    web_app_url: Option<String>,
    web_root: Option<PathBuf>,
    metrics_listen: Option<String>,
    log_format: Option<LogFormat>,
    log_level: Option<String>,
    #[serde(default)]
    tls: RawTls,
    #[serde(default)]
    cors: RawCors,
    #[serde(default)]
    ops: RawOps,
    #[serde(default)]
    oidc: Vec<OidcProvider>,
    #[serde(default)]
    limits: RawLimits,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    mode: Option<TlsMode>,
    cert_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
    domain: Option<String>,
    acme_email: Option<String>,
    acme_cache_dir: Option<PathBuf>,
    acme_staging: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCors {
    allowed_origins: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOps {
    extra_url_schemes: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    auth_per_minute: Option<u32>,
    import_max_bytes: Option<usize>,
    import_max_line_bytes: Option<usize>,
}

#[derive(Clone, Copy)]
enum Kind {
    Str,
    Bool,
    Int,
    /// Comma-separated list of strings.
    List,
}

/// Every key that may be overridden from the environment (dotted path, value kind).
const ENV_KEYS: &[(&str, Kind)] = &[
    ("listen", Kind::Str),
    ("public_url", Kind::Str),
    ("database_url", Kind::Str),
    ("db_max_connections", Kind::Int),
    ("master_key", Kind::Str),
    ("trust_proxy", Kind::Bool),
    ("web_app_url", Kind::Str),
    ("web_root", Kind::Str),
    ("metrics_listen", Kind::Str),
    ("log_format", Kind::Str),
    ("log_level", Kind::Str),
    ("tls.mode", Kind::Str),
    ("tls.cert_path", Kind::Str),
    ("tls.key_path", Kind::Str),
    ("tls.domain", Kind::Str),
    ("tls.acme_email", Kind::Str),
    ("tls.acme_cache_dir", Kind::Str),
    ("tls.acme_staging", Kind::Bool),
    ("cors.allowed_origins", Kind::List),
    ("ops.extra_url_schemes", Kind::List),
    ("limits.auth_per_minute", Kind::Int),
    ("limits.import_max_bytes", Kind::Int),
    ("limits.import_max_line_bytes", Kind::Int),
];

/// Environment variable name for a dotted config key: `tls.mode` → `CANVAS_CLOUD_TLS__MODE`.
pub fn env_var_name(key: &str) -> String {
    let mut name = String::with_capacity(ENV_PREFIX.len() + key.len() + 2);
    name.push_str(ENV_PREFIX);
    for c in key.chars() {
        if c == '.' {
            name.push_str("__");
        } else {
            name.push(c.to_ascii_uppercase());
        }
    }
    name
}

fn parse_env_value(key: &str, kind: Kind, raw: &str) -> anyhow::Result<toml::Value> {
    let v = raw.trim();
    Ok(match kind {
        Kind::Str => toml::Value::String(v.to_owned()),
        Kind::Bool => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => toml::Value::Boolean(true),
            "0" | "false" | "no" | "off" => toml::Value::Boolean(false),
            _ => bail!("{}: expected a boolean, got {v:?}", env_var_name(key)),
        },
        Kind::Int => toml::Value::Integer(
            v.replace('_', "")
                .parse::<i64>()
                .with_context(|| format!("{}: expected an integer", env_var_name(key)))?,
        ),
        Kind::List => toml::Value::Array(
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_owned()))
                .collect(),
        ),
    })
}

fn overlay_env(
    table: &mut toml::Table,
    env: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<()> {
    for &(key, kind) in ENV_KEYS {
        let Some(raw) = env(&env_var_name(key)) else {
            continue;
        };
        let value = parse_env_value(key, kind, &raw)?;
        let mut parts = key.split('.').peekable();
        let mut cur = &mut *table;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                cur.insert(part.to_owned(), value);
                break;
            }
            let entry = cur
                .entry(part.to_owned())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            cur = entry
                .as_table_mut()
                .ok_or_else(|| anyhow!("config key `{part}` must be a table"))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Loading

/// Where the config file comes from and whether its absence is an error.
#[derive(Clone, Debug)]
pub struct ConfigSource {
    pub path: PathBuf,
    /// `true` when given via `--config`/`CANVAS_CLOUD_CONFIG`: a missing file is an error.
    /// The default path may be missing (pure-env configuration).
    pub explicit: bool,
}

impl ConfigSource {
    pub fn resolve(cli: Option<PathBuf>) -> Self {
        if let Some(path) = cli {
            return Self {
                path,
                explicit: true,
            };
        }
        match std::env::var_os(CONFIG_ENV) {
            Some(p) if !p.is_empty() => Self {
                path: PathBuf::from(p),
                explicit: true,
            },
            _ => Self {
                path: PathBuf::from(DEFAULT_CONFIG_PATH),
                explicit: false,
            },
        }
    }
}

impl Config {
    /// Load from `source` + the process environment.
    pub fn load(source: &ConfigSource) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(&source.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !source.explicit => String::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading config file {}", source.path.display()))
            }
        };
        Self::from_toml_str(&text, &|k| std::env::var(k).ok())
            .with_context(|| format!("invalid configuration ({})", source.path.display()))
    }

    /// Parse TOML text overlaid with env values from `env` (injectable for tests).
    pub fn from_toml_str(text: &str, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut table: toml::Table = toml::from_str(text).context("parsing TOML")?;
        overlay_env(&mut table, env)?;
        let raw: RawConfig = toml::Value::Table(table)
            .try_into()
            .context("decoding configuration")?;
        Self::from_raw(raw)
    }

    #[allow(clippy::too_many_lines)]
    fn from_raw(raw: RawConfig) -> anyhow::Result<Self> {
        let listen: SocketAddr = raw
            .listen
            .as_deref()
            .unwrap_or("0.0.0.0:443")
            .parse()
            .context("`listen` must be an ip:port socket address")?;

        let database_url = raw
            .database_url
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("`database_url` is required (or CANVAS_CLOUD_DATABASE_URL)"))?;
        if !(database_url.starts_with("postgres://") || database_url.starts_with("postgresql://")) {
            bail!("`database_url` must start with postgres:// or postgresql://");
        }

        let master_key = parse_master_key(raw.master_key.as_deref().ok_or_else(|| {
            anyhow!("`master_key` is required (generate one with `canvas-cloud gen-config`)")
        })?)?;

        let tls_mode = raw.tls.mode.unwrap_or_default();
        let domain = raw
            .tls
            .domain
            .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
            .filter(|d| !d.is_empty());
        if tls_mode == TlsMode::Acme && domain.is_none() {
            bail!("`tls.mode = \"acme\"` requires `tls.domain`");
        }
        let state_dir = Path::new(DEFAULT_STATE_DIR);
        let tls = TlsConfig {
            mode: tls_mode,
            cert_path: raw
                .tls
                .cert_path
                .unwrap_or_else(|| state_dir.join("tls/cert.pem")),
            key_path: raw
                .tls
                .key_path
                .unwrap_or_else(|| state_dir.join("tls/key.pem")),
            domain,
            acme_email: raw.tls.acme_email.filter(|s| !s.trim().is_empty()),
            acme_cache_dir: raw
                .tls
                .acme_cache_dir
                .unwrap_or_else(|| state_dir.join("acme")),
            acme_staging: raw.tls.acme_staging.unwrap_or(false),
        };

        let public_url = match raw.public_url.filter(|s| !s.trim().is_empty()) {
            Some(u) => normalize_public_url(&u, tls.mode)?,
            None => default_public_url(listen, &tls),
        };

        let defaults = Limits::default();
        let limits = Limits {
            auth_per_minute: raw
                .limits
                .auth_per_minute
                .unwrap_or(defaults.auth_per_minute),
            import_max_bytes: raw
                .limits
                .import_max_bytes
                .unwrap_or(defaults.import_max_bytes),
            import_max_line_bytes: raw
                .limits
                .import_max_line_bytes
                .unwrap_or(defaults.import_max_line_bytes),
        };
        if limits.auth_per_minute == 0 {
            bail!("`limits.auth_per_minute` must be at least 1");
        }
        if limits.import_max_bytes < 1024 || limits.import_max_bytes > 16 << 30 {
            bail!("`limits.import_max_bytes` must be between 1 KiB and 16 GiB");
        }
        if limits.import_max_line_bytes < 1024
            || limits.import_max_line_bytes > limits.import_max_bytes
        {
            bail!("`limits.import_max_line_bytes` must be between 1 KiB and `limits.import_max_bytes`");
        }

        let metrics_listen = match raw.metrics_listen.as_deref().map(str::trim) {
            None => Some(DEFAULT_METRICS_LISTEN.parse().expect("valid default")),
            Some("" | "off" | "none" | "false") => None,
            Some(s) => Some(
                s.parse()
                    .context("`metrics_listen` must be an ip:port socket address or \"off\"")?,
            ),
        };

        let db_max_connections = raw.db_max_connections.unwrap_or(20);
        if db_max_connections == 0 {
            bail!("`db_max_connections` must be at least 1");
        }

        let cors = CorsConfig {
            allowed_origins: raw
                .cors
                .allowed_origins
                .unwrap_or_default()
                .iter()
                .map(|o| normalize_origin(o))
                .collect::<anyhow::Result<_>>()?,
        };
        let ops = OpsConfig {
            extra_url_schemes: raw
                .ops
                .extra_url_schemes
                .unwrap_or_default()
                .iter()
                .map(|s| validate_scheme(s))
                .collect::<anyhow::Result<_>>()?,
        };
        let web_app_url = raw
            .web_app_url
            .filter(|s| !s.trim().is_empty())
            .map(|u| validate_web_app_url(&u))
            .transpose()?;
        let web_root = raw.web_root.filter(|p| !p.as_os_str().is_empty());
        let mut seen = std::collections::HashSet::new();
        for p in &raw.oidc {
            validate_oidc(p)?;
            if !seen.insert(p.id.clone()) {
                bail!("duplicate `[[oidc]]` id {:?}", p.id);
            }
        }

        Ok(Self {
            listen,
            public_url,
            database_url,
            db_max_connections,
            master_key,
            trust_proxy: raw.trust_proxy.unwrap_or(false),
            tls,
            cors,
            web_app_url,
            web_root,
            ops,
            oidc: raw.oidc,
            limits,
            metrics_listen,
            log_format: raw.log_format.unwrap_or_default(),
            log_level: raw
                .log_level
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "info".into()),
        })
    }

    /// A self-contained config for tests and tooling: random master key, TLS off, ephemeral
    /// port, no metrics listener. Mutate fields as needed.
    pub fn for_tests(database_url: &str) -> Self {
        Self {
            listen: "127.0.0.1:0".parse().expect("valid"),
            public_url: "http://localhost:8480".into(),
            database_url: database_url.to_owned(),
            db_max_connections: 10,
            master_key: crate::ids::random_bytes::<32>(),
            trust_proxy: false,
            tls: TlsConfig {
                mode: TlsMode::Off,
                cert_path: PathBuf::from("cert.pem"),
                key_path: PathBuf::from("key.pem"),
                domain: None,
                acme_email: None,
                acme_cache_dir: PathBuf::from("acme"),
                acme_staging: true,
            },
            cors: CorsConfig::default(),
            web_app_url: None,
            web_root: None,
            ops: OpsConfig::default(),
            oidc: Vec::new(),
            limits: Limits::default(),
            metrics_listen: None,
            log_format: LogFormat::Pretty,
            log_level: "info".into(),
        }
    }

    /// Host part of `public_url` (no brackets for IPv6).
    pub fn public_host(&self) -> String {
        url::Url::parse(&self.public_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_owned()))
            .unwrap_or_else(|| "localhost".into())
    }

    /// Where share links open, with a `{token}` placeholder: `web_app_url`, else
    /// `{public_url}/#/join/{token}` (the served web app's join route) when `web_root` is set,
    /// else `None`.
    pub fn effective_web_app_url(&self) -> Option<String> {
        self.web_app_url.clone().or_else(|| {
            self.web_root
                .as_ref()
                .map(|_| format!("{}/#/join/{WEB_APP_TOKEN}", self.public_url))
        })
    }

    /// The share-link landing target for `token` ([`Self::effective_web_app_url`] with
    /// `{token}` substituted).
    pub fn web_app_link(&self, token: &str) -> Option<String> {
        self.effective_web_app_url()
            .map(|u| u.replace(WEB_APP_TOKEN, token))
    }

    /// `web_root` must be a directory holding `index.html` (checked by `serve` and `doctor`,
    /// not at parse time, so a config can be validated away from its files).
    pub fn check_web_root(&self) -> anyhow::Result<()> {
        let Some(root) = &self.web_root else {
            return Ok(());
        };
        if !root.is_dir() {
            bail!("`web_root` {} is not a directory", root.display());
        }
        if !root.join("index.html").is_file() {
            bail!("`web_root` {} has no index.html", root.display());
        }
        Ok(())
    }
}

/// Placeholder in `web_app_url`.
pub const WEB_APP_TOKEN: &str = "{token}";

fn default_public_url(listen: SocketAddr, tls: &TlsConfig) -> String {
    let scheme = if tls.mode == TlsMode::Off {
        "http"
    } else {
        "https"
    };
    match &tls.domain {
        Some(d) if tls.mode == TlsMode::Acme || listen.port() == 443 => format!("https://{d}"),
        Some(d) => format!("{scheme}://{d}:{}", listen.port()),
        None if (scheme == "https" && listen.port() == 443)
            || (scheme == "http" && listen.port() == 80) =>
        {
            format!("{scheme}://localhost")
        }
        None => format!("{scheme}://localhost:{}", listen.port()),
    }
}

/// Accepts `host`, `host:port`, `https://host[:port][/...]`; returns `scheme://host[:port]`
/// (lower-case, no path). A bare host gets `https://` (`http://` when TLS is off).
pub fn normalize_public_url(input: &str, mode: TlsMode) -> anyhow::Result<String> {
    let s = input.trim();
    let with_scheme = if s.contains("://") {
        s.to_owned()
    } else if mode == TlsMode::Off {
        format!("http://{s}")
    } else {
        format!("https://{s}")
    };
    let u =
        url::Url::parse(&with_scheme).context("`public_url` must look like https://host[:port]")?;
    if !matches!(u.scheme(), "http" | "https") {
        bail!("`public_url` must be an http:// or https:// URL");
    }
    if !u.username().is_empty() || u.password().is_some() {
        bail!("`public_url` must not contain credentials");
    }
    let host = u
        .host_str()
        .filter(|h| !h.is_empty())
        .context("`public_url` has no host")?;
    Ok(match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    }
    .to_ascii_lowercase())
}

/// A CORS origin: `scheme://host[:port]`, exact (lower-case, no trailing slash).
pub fn normalize_origin(input: &str) -> anyhow::Result<String> {
    let s = input.trim().trim_end_matches('/');
    let u = url::Url::parse(s)
        .with_context(|| format!("`cors.allowed_origins`: {s:?} is not an origin"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        bail!("`cors.allowed_origins`: {s:?} must be http(s)://host[:port]");
    }
    let origin = u.origin().ascii_serialization();
    if !s.eq_ignore_ascii_case(&origin) {
        bail!("`cors.allowed_origins`: {s:?} must be a bare origin like {origin}");
    }
    Ok(origin)
}

/// An extra URL scheme for link shapes: `[a-z][a-z0-9+.-]*`, ≤ 32 characters.
pub fn validate_scheme(input: &str) -> anyhow::Result<String> {
    let s = input.trim().trim_end_matches(':').to_ascii_lowercase();
    let mut chars = s.chars();
    let ok = s.len() <= 32
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'));
    if !ok {
        bail!("`ops.extra_url_schemes`: {input:?} is not a URL scheme");
    }
    if matches!(s.as_str(), "javascript" | "data" | "vbscript" | "file") {
        bail!("`ops.extra_url_schemes`: {s:?} is not allowed");
    }
    Ok(s)
}

fn validate_web_app_url(input: &str) -> anyhow::Result<String> {
    let s = input.trim();
    let mut out = s.to_owned();
    if !out.contains(WEB_APP_TOKEN) {
        out.push_str(WEB_APP_TOKEN);
    }
    let probe = out.replace(WEB_APP_TOKEN, "token");
    let u = url::Url::parse(&probe).context("`web_app_url` must be an http(s) URL")?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        bail!("`web_app_url` must be an http(s) URL");
    }
    Ok(out)
}

fn validate_oidc(p: &OidcProvider) -> anyhow::Result<()> {
    let id_ok = !p.id.is_empty()
        && p.id.len() <= 32
        && p.id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !id_ok {
        bail!(
            "`[[oidc]] id` {:?} must be [a-z0-9_-], 1-32 characters",
            p.id
        );
    }
    let issuer = url::Url::parse(&p.issuer)
        .with_context(|| format!("`[[oidc]]` {}: issuer must be a URL", p.id))?;
    if issuer.scheme() != "https" && !matches!(issuer.host_str(), Some("localhost" | "127.0.0.1")) {
        bail!("`[[oidc]]` {}: issuer must be https", p.id);
    }
    if p.client_id.trim().is_empty() {
        bail!("`[[oidc]]` {}: client_id is required", p.id);
    }
    for r in &p.redirect_uris {
        url::Url::parse(r)
            .with_context(|| format!("`[[oidc]]` {}: redirect_uri {r:?} is not a URL", p.id))?;
    }
    Ok(())
}

/// Decode a base64 / base64url master key that must be exactly 32 bytes.
pub fn parse_master_key(s: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = crate::ids::b64_decode_any(s).context("`master_key` must be base64url")?;
    bytes.as_slice().try_into().map_err(|_| {
        anyhow!(
            "`master_key` must decode to exactly 32 bytes (got {})",
            bytes.len()
        )
    })
}

/// Options for [`generate_toml`].
#[derive(Clone, Debug)]
pub struct GenerateOptions {
    pub public_url: Option<String>,
    pub database_url: String,
    pub listen: String,
    pub tls_mode: TlsMode,
    pub domain: Option<String>,
    pub acme_email: Option<String>,
    pub master_key: Option<String>,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub acme_cache_dir: PathBuf,
}

/// Render a complete, commented config file (a fresh master key unless provided).
pub fn generate_toml(opts: &GenerateOptions) -> anyhow::Result<String> {
    let master_key = opts
        .master_key
        .clone()
        .unwrap_or_else(|| crate::ids::b64url(&crate::ids::random_bytes::<32>()));
    parse_master_key(&master_key)?;
    let q = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let path_q = |p: &Path| q(&p.to_string_lossy());
    let mut out = String::with_capacity(2048);
    out.push_str("# canvas-cloud configuration. Keep this file private (0600): it holds the\n");
    out.push_str("# master key that wraps every canvas's document key.\n");
    out.push_str(
        "# Every key can be overridden by CANVAS_CLOUD_<KEY> (nested: CANVAS_CLOUD_TLS__MODE).\n\n",
    );
    writeln!(out, "listen = {}", q(&opts.listen))?;
    if let Some(u) = &opts.public_url {
        out.push_str("# Base URL clients use (shown in docs, agent guides and OIDC redirects)\n");
        writeln!(
            out,
            "public_url = {}",
            q(&normalize_public_url(u, opts.tls_mode)?)
        )?;
    }
    writeln!(out, "database_url = {}", q(&opts.database_url))?;
    out.push_str("db_max_connections = 20\n");
    out.push_str("# Losing the master key makes every stored canvas unreadable. Back it up.\n");
    writeln!(out, "master_key = {}", q(&master_key))?;
    out.push_str("trust_proxy = false\n");
    out.push_str(
        "# Where /join/{token} share-link pages send people (e.g. a hosted canvas app).\n",
    );
    out.push_str("# web_app_url = \"https://canvas.example.com/join/{token}\"\n");
    out.push_str(
        "# Serve a built web app (a directory with index.html) at the root; share links then\n\
         # open {public_url}/#/join/{token} unless web_app_url says otherwise.\n",
    );
    out.push_str("# web_root = \"/opt/canvas-app/dist/web\"\n");
    writeln!(out, "metrics_listen = {}", q(DEFAULT_METRICS_LISTEN))?;
    out.push_str("log_format = \"json\"\nlog_level = \"info\"\n\n");
    out.push_str("[tls]\n# \"self-signed\" | \"acme\" | \"off\" (off only on loopback or behind a TLS proxy)\n");
    writeln!(out, "mode = {}", q(&opts.tls_mode.to_string()))?;
    writeln!(out, "cert_path = {}", path_q(&opts.cert_path))?;
    writeln!(out, "key_path = {}", path_q(&opts.key_path))?;
    if let Some(d) = &opts.domain {
        writeln!(out, "domain = {}", q(d))?;
    }
    if let Some(e) = &opts.acme_email {
        writeln!(out, "acme_email = {}", q(e))?;
    }
    writeln!(out, "acme_cache_dir = {}\n", path_q(&opts.acme_cache_dir))?;
    out.push_str("[cors]\n# Other browser origins allowed to call the API (the web_root app is same-origin\n# and needs none), e.g. [\"http://localhost:5173\", \"http://127.0.0.1:5173\"]\n");
    out.push_str("allowed_origins = []\n\n");
    out.push_str("[ops]\n# URL schemes accepted in link shapes besides http, https and mailto\n");
    out.push_str("extra_url_schemes = []\n\n");
    out.push_str("[limits]\nauth_per_minute = 10\n");
    // Round-trip check so we never write a file we cannot load.
    Config::from_toml_str(&out, &|_| None).context("generated config failed validation")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const BASE: &str = r#"
        listen = "0.0.0.0:8443"
        public_url = "https://Canvas.Example.com:8443/some/path"
        database_url = "postgres://cc:hunter2@localhost:5432/canvas_cloud?sslmode=require"
        master_key = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
        [tls]
        mode = "self-signed"
    "#;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn parses_file_with_defaults() {
        let c = Config::from_toml_str(BASE, &env(&[])).unwrap();
        assert_eq!(c.listen, "0.0.0.0:8443".parse().unwrap());
        assert_eq!(c.public_url, "https://canvas.example.com:8443");
        assert_eq!(c.public_host(), "canvas.example.com");
        assert_eq!(c.master_key[31], 31);
        assert_eq!(c.tls.mode, TlsMode::SelfSigned);
        assert_eq!(
            c.tls.cert_path,
            PathBuf::from("/var/lib/canvas-cloud/tls/cert.pem")
        );
        assert_eq!(c.limits.auth_per_minute, 10);
        assert_eq!(c.limits.import_max_bytes, 512 << 20);
        assert_eq!(
            c.metrics_listen,
            Some(DEFAULT_METRICS_LISTEN.parse().unwrap())
        );
        assert_eq!(c.log_format, LogFormat::Json);
        assert_eq!(c.cors.allowed_origins, Vec::<String>::new());
        assert_eq!(c.ops.extra_url_schemes, Vec::<String>::new());
        assert!(c.web_app_url.is_none());
        assert!(c.oidc.is_empty());
    }

    #[test]
    fn env_overrides_top_level_nested_and_lists() {
        let c = Config::from_toml_str(
            BASE,
            &env(&[
                ("CANVAS_CLOUD_DATABASE_URL", "postgres://localhost/other"),
                ("CANVAS_CLOUD_TLS__MODE", "acme"),
                ("CANVAS_CLOUD_TLS__DOMAIN", "Canvas.Example.org."),
                ("CANVAS_CLOUD_LIMITS__AUTH_PER_MINUTE", "20"),
                ("CANVAS_CLOUD_METRICS_LISTEN", "off"),
                ("CANVAS_CLOUD_LOG_FORMAT", "pretty"),
                (
                    "CANVAS_CLOUD_CORS__ALLOWED_ORIGINS",
                    "https://a.example.com, http://localhost:5173/",
                ),
                ("CANVAS_CLOUD_OPS__EXTRA_URL_SCHEMES", "MyApp,zed"),
                (
                    "CANVAS_CLOUD_WEB_APP_URL",
                    "https://app.example.com/join/{token}",
                ),
                // Unknown CANVAS_CLOUD_* vars (e.g. installer inputs) are ignored.
                ("CANVAS_CLOUD_BINARY", "/tmp/x"),
            ]),
        )
        .unwrap();
        assert_eq!(c.database_url, "postgres://localhost/other");
        assert_eq!(c.tls.mode, TlsMode::Acme);
        assert_eq!(c.tls.domain.as_deref(), Some("canvas.example.org"));
        assert_eq!(c.limits.auth_per_minute, 20);
        assert_eq!(c.metrics_listen, None);
        assert_eq!(c.log_format, LogFormat::Pretty);
        assert_eq!(
            c.cors.allowed_origins,
            vec!["https://a.example.com", "http://localhost:5173"]
        );
        assert_eq!(c.ops.extra_url_schemes, vec!["myapp", "zed"]);
        assert_eq!(
            c.web_app_link("T").as_deref(),
            Some("https://app.example.com/join/T")
        );
    }

    #[test]
    fn env_only_config_and_default_public_url() {
        let c = Config::from_toml_str(
            "",
            &env(&[
                ("CANVAS_CLOUD_DATABASE_URL", "postgres://localhost/db"),
                ("CANVAS_CLOUD_MASTER_KEY", &crate::ids::b64url(&[1u8; 32])),
                ("CANVAS_CLOUD_LISTEN", "127.0.0.1:8480"),
                ("CANVAS_CLOUD_TLS__MODE", "off"),
            ]),
        )
        .unwrap();
        assert_eq!(c.public_url, "http://localhost:8480");
        assert_eq!(c.listen.port(), 8480);
    }

    #[test]
    fn web_app_url_placeholder_is_appended() {
        let c = Config::from_toml_str(
            &format!("web_app_url = \"https://app.example.com/j/\"\n{BASE}"),
            &env(&[]),
        )
        .unwrap();
        assert_eq!(
            c.web_app_link("abc").as_deref(),
            Some("https://app.example.com/j/abc")
        );
        assert!(Config::from_toml_str(
            &format!("web_app_url = \"javascript:alert(1)\"\n{BASE}"),
            &env(&[])
        )
        .is_err());
    }

    #[test]
    fn web_root_defaults_the_share_link_target() {
        let c = Config::from_toml_str(
            &format!("web_root = \"/srv/canvas-app\"\n{BASE}"),
            &env(&[]),
        )
        .unwrap();
        assert_eq!(c.web_root.as_deref(), Some(Path::new("/srv/canvas-app")));
        assert_eq!(
            c.effective_web_app_url().as_deref(),
            Some("https://canvas.example.com:8443/#/join/{token}")
        );
        assert_eq!(
            c.web_app_link("T").as_deref(),
            Some("https://canvas.example.com:8443/#/join/T")
        );
        // An explicit web_app_url wins; env sets web_root too.
        let c = Config::from_toml_str(
            &format!("web_app_url = \"https://app.example.com/j/{{token}}\"\n{BASE}"),
            &env(&[("CANVAS_CLOUD_WEB_ROOT", "/srv/web")]),
        )
        .unwrap();
        assert_eq!(c.web_root.as_deref(), Some(Path::new("/srv/web")));
        assert_eq!(
            c.web_app_link("T").as_deref(),
            Some("https://app.example.com/j/T")
        );
        // Neither: no target.
        let c = Config::from_toml_str(BASE, &env(&[])).unwrap();
        assert_eq!(c.web_app_link("T"), None);
        // check_web_root wants a directory with index.html.
        let mut c = Config::for_tests("postgres://localhost/x");
        assert!(c.check_web_root().is_ok());
        let dir = std::env::temp_dir().join(format!("cvc-web-root-{}", crate::ids::uuid_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        c.web_root = Some(dir.clone());
        assert!(c.check_web_root().is_err());
        std::fs::write(dir.join("index.html"), "<!doctype html>").unwrap();
        assert!(c.check_web_root().is_ok());
        c.web_root = Some(dir.join("index.html"));
        assert!(c.check_web_root().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_bad_values() {
        let bad_bool = Config::from_toml_str(BASE, &env(&[("CANVAS_CLOUD_TRUST_PROXY", "maybe")]));
        assert!(bad_bool.is_err());
        let bad_master = Config::from_toml_str(BASE, &env(&[("CANVAS_CLOUD_MASTER_KEY", "AAAA")]));
        assert!(bad_master.is_err());
        let unknown = Config::from_toml_str(&format!("{BASE}\nnope = 1\n"), &env(&[]));
        assert!(
            unknown.is_err(),
            "unknown key after [tls] lands in tls table"
        );
        let acme_no_domain =
            Config::from_toml_str(BASE, &env(&[("CANVAS_CLOUD_TLS__MODE", "acme")]));
        assert!(acme_no_domain.is_err());
        let missing_db = Config::from_toml_str(
            "master_key = \"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8\"",
            &env(&[]),
        );
        assert!(missing_db.is_err());
        let bad_origin = Config::from_toml_str(
            BASE,
            &env(&[(
                "CANVAS_CLOUD_CORS__ALLOWED_ORIGINS",
                "https://a.example.com/path",
            )]),
        );
        assert!(bad_origin.is_err());
        for bad in ["javascript", "9abc", "a b", "data"] {
            let r =
                Config::from_toml_str(BASE, &env(&[("CANVAS_CLOUD_OPS__EXTRA_URL_SCHEMES", bad)]));
            assert!(r.is_err(), "{bad}");
        }
    }

    #[test]
    fn oidc_providers_parse_and_validate() {
        let text = format!(
            "{BASE}\n[[oidc]]\nid = \"google\"\nissuer = \"https://accounts.google.com\"\nclient_id = \"x\"\nallowed_domains = [\"example.com\"]\n"
        );
        // `[[oidc]]` after `[tls]` is its own table array.
        let c = Config::from_toml_str(&text, &env(&[])).unwrap();
        assert_eq!(c.oidc.len(), 1);
        assert_eq!(c.oidc[0].id, "google");
        assert_eq!(c.oidc[0].label(), "google");
        assert_eq!(c.oidc[0].allowed_domains, ["example.com"]);
        let bad = text.replace("\"google\"", "\"Goo gle\"");
        assert!(Config::from_toml_str(&bad, &env(&[])).is_err());
        let named = format!(
            "{text}name = \"Google\"\nclient_secret_file = \"/etc/canvas-cloud/google.secret\"\nredirect_uris = [\"https://canvas.example.com/\"]\n"
        );
        let c = Config::from_toml_str(&named, &env(&[])).unwrap();
        assert_eq!(c.oidc[0].label(), "Google");
        assert_eq!(c.oidc[0].redirect_uris, ["https://canvas.example.com/"]);
        assert!(c.oidc[0].client_secret_file.is_some());
        // Unknown keys are refused (typos would silently weaken a sign-in policy).
        let typo = format!("{text}allowed_domain = [\"x.com\"]\n");
        assert!(Config::from_toml_str(&typo, &env(&[])).is_err());
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = Config::from_toml_str(BASE, &env(&[])).unwrap();
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("hunter2"));
        assert!(!dbg.contains("AAECAwQF"));
        assert!(dbg.contains("postgres://cc:***@localhost:5432/canvas_cloud"));
    }

    #[test]
    fn public_url_normalization() {
        assert_eq!(
            normalize_public_url("1.2.3.4", TlsMode::SelfSigned).unwrap(),
            "https://1.2.3.4"
        );
        assert_eq!(
            normalize_public_url("A.B:9", TlsMode::Off).unwrap(),
            "http://a.b:9"
        );
        assert_eq!(
            normalize_public_url("https://A.B:9/x", TlsMode::Off).unwrap(),
            "https://a.b:9"
        );
        assert_eq!(
            normalize_public_url("[2001:db8::1]:8443", TlsMode::Acme).unwrap(),
            "https://[2001:db8::1]:8443"
        );
        assert!(normalize_public_url("https://user@host", TlsMode::Acme).is_err());
        assert!(normalize_public_url("ftp://host", TlsMode::Acme).is_err());
        assert!(normalize_public_url("", TlsMode::Acme).is_err());
    }

    #[test]
    fn generated_config_loads() {
        let text = generate_toml(&GenerateOptions {
            public_url: Some("198.51.100.4".into()),
            database_url: "postgres://localhost/canvas_cloud".into(),
            listen: "0.0.0.0:443".into(),
            tls_mode: TlsMode::SelfSigned,
            domain: None,
            acme_email: None,
            master_key: None,
            cert_path: "/var/lib/canvas-cloud/tls/cert.pem".into(),
            key_path: "/var/lib/canvas-cloud/tls/key.pem".into(),
            acme_cache_dir: "/var/lib/canvas-cloud/acme".into(),
        })
        .unwrap();
        let c = Config::from_toml_str(&text, &env(&[])).unwrap();
        assert_eq!(c.public_url, "https://198.51.100.4");
        let again = generate_toml(&GenerateOptions {
            public_url: None,
            database_url: "postgres://localhost/canvas_cloud".into(),
            listen: "127.0.0.1:8480".into(),
            tls_mode: TlsMode::Off,
            domain: None,
            acme_email: None,
            master_key: None,
            cert_path: "c".into(),
            key_path: "k".into(),
            acme_cache_dir: "a".into(),
        })
        .unwrap();
        assert_ne!(
            Config::from_toml_str(&again, &env(&[])).unwrap().master_key,
            c.master_key,
            "fresh master key each time"
        );
    }

    #[test]
    fn env_names() {
        assert_eq!(
            env_var_name("tls.acme_email"),
            "CANVAS_CLOUD_TLS__ACME_EMAIL"
        );
        assert_eq!(env_var_name("database_url"), "CANVAS_CLOUD_DATABASE_URL");
    }
}
