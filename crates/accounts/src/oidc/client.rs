//! The relying-party half of OIDC: discovery (`{issuer}/.well-known/openid-configuration`),
//! a cached JWKS, the authorization-code exchange (PKCE, client secret via HTTP Basic or the
//! form) and `id_token` verification (RS256 / ES256 signature, `iss`, `aud`/`azp`, `exp`,
//! `nbf`, `nonce`).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use canvas_cloud_core::config::OidcProvider;
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

/// Discovery documents and key sets are re-fetched after this long.
const META_TTL: Duration = Duration::from_secs(3600);
/// An unknown `kid` forces a JWKS refetch at most this often (key rotation).
const JWKS_REFETCH_MIN: Duration = Duration::from_secs(30);
/// Largest identity-provider response we read.
const MAX_RESPONSE_BYTES: usize = 1 << 20;
/// Clock skew allowed on `exp` / `nbf` / `iat`.
const LEEWAY_SECS: u64 = 60;

/// Why a step failed: a machine-readable code for the client's redirect fragment plus a
/// human message (never secrets).
#[derive(Debug)]
pub(crate) struct Fail {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl Fail {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::new("provider_unavailable", message)
    }
}

/// What we use from the discovery document.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Discovery {
    pub(crate) issuer: String,
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    pub(crate) jwks_uri: String,
    #[serde(default)]
    pub(crate) token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

/// A JWKS whose keys are parsed one by one (unknown key types are skipped).
#[derive(Deserialize)]
struct RawKeySet {
    keys: Vec<Value>,
}

struct Cached {
    discovery: Discovery,
    discovered_at: Instant,
    jwks: Vec<Jwk>,
    jwks_at: Option<Instant>,
}

/// Per-app OIDC cache (discovery + JWKS per provider) and HTTP client.
#[derive(Default)]
pub(crate) struct Cache {
    providers: Mutex<HashMap<String, Cached>>,
    http: OnceLock<reqwest::Client>,
}

/// `https://`, or `http://` to a loopback host (local identity providers and tests) — the same rule core
/// applies to `issuer`.
fn acceptable_endpoint(raw: &str) -> bool {
    Url::parse(raw).is_ok_and(|u| {
        u.scheme() == "https"
            || (u.scheme() == "http"
                && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
    })
}

fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

impl Cache {
    fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("canvas-cloud/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default()
        })
    }

    async fn read_limited(resp: reqwest::Response) -> Result<Vec<u8>, Fail> {
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err(Fail::unavailable("identity provider response is too large"));
        }
        let mut resp = resp;
        let mut out = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| Fail::unavailable(format!("reading identity provider response: {e}")))?
        {
            if out.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(Fail::unavailable("identity provider response is too large"));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, Fail> {
        let resp = self
            .http()
            .get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| Fail::unavailable(format!("fetching {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Fail::unavailable(format!("{url} answered {status}")));
        }
        let body = Self::read_limited(resp).await?;
        serde_json::from_slice(&body).map_err(|e| Fail::unavailable(format!("{url}: {e}")))
    }

    /// The provider's discovery document (cached for [`META_TTL`]).
    pub(crate) async fn discovery(&self, p: &OidcProvider) -> Result<Discovery, Fail> {
        let mut map = self.providers.lock().await;
        if let Some(c) = map.get(&p.id) {
            if c.discovered_at.elapsed() < META_TTL {
                return Ok(c.discovery.clone());
            }
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            p.issuer.trim_end_matches('/')
        );
        let d: Discovery = self.get_json(&url).await?;
        if !same_issuer(&d.issuer, &p.issuer) {
            return Err(Fail::unavailable(format!(
                "discovery issuer {:?} does not match the configured issuer",
                d.issuer
            )));
        }
        for endpoint in [&d.authorization_endpoint, &d.token_endpoint, &d.jwks_uri] {
            if !acceptable_endpoint(endpoint) {
                return Err(Fail::unavailable(format!(
                    "discovery endpoint {endpoint:?} must be https"
                )));
            }
        }
        map.insert(
            p.id.clone(),
            Cached {
                discovery: d.clone(),
                discovered_at: Instant::now(),
                jwks: Vec::new(),
                jwks_at: None,
            },
        );
        Ok(d)
    }

    /// The provider's signing keys: cached for [`META_TTL`]; `refresh` re-fetches unless the
    /// last fetch was under [`JWKS_REFETCH_MIN`] ago. Keys of unknown types are skipped.
    async fn jwks(&self, p: &OidcProvider, d: &Discovery, refresh: bool) -> Result<Vec<Jwk>, Fail> {
        let mut map = self.providers.lock().await;
        if let Some(c) = map.get(&p.id) {
            if let Some(at) = c.jwks_at {
                let fresh = at.elapsed() < META_TTL && !refresh;
                if fresh || at.elapsed() < JWKS_REFETCH_MIN {
                    return Ok(c.jwks.clone());
                }
            }
        }
        let set: RawKeySet = self.get_json(&d.jwks_uri).await?;
        let keys: Vec<Jwk> = set
            .keys
            .into_iter()
            .filter_map(|k| serde_json::from_value(k).ok())
            .collect();
        let entry = map.entry(p.id.clone()).or_insert_with(|| Cached {
            discovery: d.clone(),
            discovered_at: Instant::now(),
            jwks: Vec::new(),
            jwks_at: None,
        });
        entry.jwks.clone_from(&keys);
        entry.jwks_at = Some(Instant::now());
        Ok(keys)
    }

    /// Exchange the authorization code for tokens; returns the `id_token`.
    pub(crate) async fn exchange(
        &self,
        p: &OidcProvider,
        d: &Discovery,
        code: &str,
        callback_url: &str,
        verifier: &str,
        secret: Option<&str>,
    ) -> Result<String, Fail> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", callback_url),
            ("code_verifier", verifier),
        ];
        let methods = d.token_endpoint_auth_methods_supported.as_deref();
        let supports = |m: &str| methods.is_some_and(|ms| ms.iter().any(|x| x == m));
        let mut req = self
            .http()
            .post(&d.token_endpoint)
            .header("accept", "application/json");
        match secret {
            // RFC 6749 §2.3.1: Basic credentials are form-urlencoded first.
            Some(secret) if methods.is_none() || supports("client_secret_basic") => {
                let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
                req = req.basic_auth(enc(&p.client_id), Some(enc(secret)));
            }
            Some(secret) => {
                form.push(("client_id", &p.client_id));
                form.push(("client_secret", secret));
            }
            None => form.push(("client_id", &p.client_id)),
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let resp = req
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| Fail::new("token_exchange_failed", format!("token endpoint: {e}")))?;
        let status = resp.status();
        let bytes = Self::read_limited(resp).await?;
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if !status.is_success() {
            let err = json["error"].as_str().unwrap_or("unknown");
            return Err(Fail::new(
                "token_exchange_failed",
                format!("the identity provider refused the code ({status}, {err})"),
            ));
        }
        json["id_token"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| Fail::new("token_exchange_failed", "no id_token in the token response"))
    }

    /// Verify `id_token` (signature with the provider's keys, `iss`, `aud`/`azp`, `exp`,
    /// `nbf`, `nonce`) and return its claims.
    pub(crate) async fn verify(
        &self,
        p: &OidcProvider,
        d: &Discovery,
        id_token: &str,
        nonce: &str,
    ) -> Result<IdClaims, Fail> {
        let bad = |m: String| Fail::new("invalid_id_token", m);
        let header = jsonwebtoken::decode_header(id_token)
            .map_err(|e| bad(format!("malformed id_token: {e}")))?;
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
            return Err(bad(format!("unsupported id_token alg {:?}", header.alg)));
        }
        let mut keys = self.jwks(p, d, false).await?;
        let mut candidates = usable_keys(&keys, header.alg, header.kid.as_deref());
        if candidates.is_empty() && header.kid.is_some() {
            keys = self.jwks(p, d, true).await?;
            candidates = usable_keys(&keys, header.alg, header.kid.as_deref());
        }
        if candidates.is_empty() {
            return Err(bad("no matching signing key".into()));
        }
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[d.issuer.as_str()]);
        validation.set_audience(&[p.client_id.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = LEEWAY_SECS;
        validation.validate_nbf = true;
        let mut last_err = None;
        let mut data = None;
        for jwk in candidates {
            let Ok(key) = DecodingKey::from_jwk(jwk) else {
                continue;
            };
            match jsonwebtoken::decode::<IdClaims>(id_token, &key, &validation) {
                Ok(d) => {
                    data = Some(d);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let claims = data
            .ok_or_else(|| {
                bad(format!(
                    "id_token rejected: {}",
                    last_err.map_or_else(|| "no usable key".to_owned(), |e| e.to_string())
                ))
            })?
            .claims;
        let audiences = match &claims.aud {
            Value::Array(a) => a.len(),
            _ => 1,
        };
        match claims.azp.as_deref() {
            Some(azp) if azp != p.client_id => {
                return Err(bad("id_token azp is another client".into()));
            }
            None if audiences > 1 => {
                return Err(bad("id_token has several audiences and no azp".into()));
            }
            _ => {}
        }
        if let Some(iat) = claims.iat {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            #[allow(clippy::cast_possible_wrap)]
            if iat > now + LEEWAY_SECS as i64 {
                return Err(bad("id_token issued in the future".into()));
            }
        }
        if claims.nonce.as_deref() != Some(nonce) {
            return Err(Fail::new("nonce_mismatch", "id_token nonce does not match"));
        }
        Ok(claims)
    }
}

/// Keys that can verify `alg` (and carry `kid` when the header names one).
fn usable_keys<'a>(keys: &'a [Jwk], alg: Algorithm, kid: Option<&str>) -> Vec<&'a Jwk> {
    keys.iter()
        .filter(|k| kid.is_none_or(|kid| k.common.key_id.as_deref() == Some(kid)))
        .filter(|k| {
            !matches!(
                k.common.public_key_use,
                Some(PublicKeyUse::Encryption | PublicKeyUse::Other(_))
            )
        })
        .filter(|k| match (alg, &k.algorithm) {
            (Algorithm::RS256, AlgorithmParameters::RSA(_)) => {
                matches!(k.common.key_algorithm, None | Some(KeyAlgorithm::RS256))
            }
            (Algorithm::ES256, AlgorithmParameters::EllipticCurve(ec)) => {
                ec.curve == EllipticCurve::P256
                    && matches!(k.common.key_algorithm, None | Some(KeyAlgorithm::ES256))
            }
            _ => false,
        })
        .collect()
}

/// The `id_token` claims we read.
#[derive(Debug, Deserialize)]
pub(crate) struct IdClaims {
    #[allow(dead_code)]
    pub(crate) sub: String,
    #[serde(default)]
    pub(crate) aud: Value,
    pub(crate) azp: Option<String>,
    pub(crate) iat: Option<i64>,
    pub(crate) nonce: Option<String>,
    pub(crate) email: Option<String>,
    /// `true`, or the string `"true"` (some providers send strings).
    #[serde(default)]
    pub(crate) email_verified: Value,
    pub(crate) name: Option<String>,
    pub(crate) given_name: Option<String>,
}

impl IdClaims {
    pub(crate) fn email_verified(&self) -> bool {
        match &self.email_verified {
            Value::Bool(b) => *b,
            Value::String(s) => s.eq_ignore_ascii_case("true"),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_must_be_https_or_loopback() {
        assert!(acceptable_endpoint("https://idp.example.com/token"));
        assert!(acceptable_endpoint("http://127.0.0.1:9999/token"));
        assert!(acceptable_endpoint("http://localhost/token"));
        assert!(!acceptable_endpoint("http://idp.example.com/token"));
        assert!(!acceptable_endpoint("ftp://idp.example.com/"));
        assert!(!acceptable_endpoint("not a url"));
    }

    #[test]
    fn issuers_compare_without_trailing_slash() {
        assert!(same_issuer("https://a.example/", "https://a.example"));
        assert!(!same_issuer("https://a.example/x", "https://a.example"));
    }

    #[test]
    fn email_verified_forms() {
        let c = |v: Value| IdClaims {
            sub: "s".into(),
            aud: Value::Null,
            azp: None,
            iat: None,
            nonce: None,
            email: None,
            email_verified: v,
            name: None,
            given_name: None,
        };
        assert!(c(Value::Bool(true)).email_verified());
        assert!(c(Value::String("true".into())).email_verified());
        assert!(!c(Value::Bool(false)).email_verified());
        assert!(!c(Value::Null).email_verified());
        assert!(!c(Value::String("yes".into())).email_verified());
    }
}
