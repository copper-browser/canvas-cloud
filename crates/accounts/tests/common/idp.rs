//! An in-process mock OIDC provider: discovery, JWKS (an ES256 key generated per run, a fixed
//! RS256 test key and one key of an unsupported type), and a token endpoint that checks the
//! PKCE verifier, the redirect URI and the client authentication, then signs an `id_token`
//! with whatever claims the test asked for.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine as _;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{json, Map, Value};
use url::Url;

/// The confidential client the tests configure (`client_secret_basic`).
pub const CLIENT_ID: &str = "cvc-test";
pub const CLIENT_SECRET: &str = "s3cret value/with+chars";
/// The public client (PKCE only, no secret).
pub const PUBLIC_CLIENT_ID: &str = "cvc-public";

const RSA_PEM: &str = include_str!("../fixtures/rsa-test-key.pem");
const RSA_N: &str = include_str!("../fixtures/rsa-test-key.n");

/// How the mock signs the `id_token`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signer {
    Es256,
    Rs256,
    /// An ES256 key that is not in the JWKS (but claims the published `kid`).
    Rogue,
    /// HS256 with the client secret — an algorithm the server must refuse.
    Hs256,
}

struct Grant {
    claims: Value,
    challenge: String,
    redirect_uri: String,
    client_id: String,
    signer: Signer,
    fail: bool,
}

struct Keys {
    ec: EncodingKey,
    ec_jwk: Value,
    rogue: EncodingKey,
    rsa: EncodingKey,
}

struct Inner {
    issuer: String,
    keys: Keys,
    grants: Mutex<HashMap<String, Grant>>,
    /// Client auth methods seen by the token endpoint (`basic` / `post` / `none`).
    auth_seen: Mutex<Vec<String>>,
}

#[derive(Clone)]
pub struct MockIdp {
    pub issuer: String,
    pub addr: SocketAddr,
    inner: Arc<Inner>,
}

fn ec_key() -> (EncodingKey, Value) {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let raw = kp.public_key_raw();
    assert_eq!(raw.len(), 65);
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let jwk = json!({
        "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": "ec-1",
        "x": b64(&raw[1..33]), "y": b64(&raw[33..65]),
    });
    (EncodingKey::from_ec_der(&kp.serialize_der()), jwk)
}

impl MockIdp {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let issuer = format!("http://127.0.0.1:{}", addr.port());
        let (ec, ec_jwk) = ec_key();
        let (rogue, _) = ec_key();
        let inner = Arc::new(Inner {
            issuer: issuer.clone(),
            keys: Keys {
                ec,
                ec_jwk,
                rogue,
                rsa: EncodingKey::from_rsa_pem(RSA_PEM.as_bytes()).unwrap(),
            },
            grants: Mutex::new(HashMap::new()),
            auth_seen: Mutex::new(Vec::new()),
        });
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(inner.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            issuer,
            addr,
            inner,
        }
    }

    /// "Sign the user in" for the authorization request `authorize` (the `Location` the
    /// server's `/start` redirected to): returns the code the provider would hand back. `claims`
    /// override the defaults (`iss`, `aud`, `sub`, `exp`, `iat`, `nonce`, `email_verified`);
    /// a `null` override removes the claim.
    pub fn issue(&self, authorize: &Url, claims: &Value, signer: Signer) -> String {
        self.issue_inner(authorize, claims, signer, false)
    }

    /// Like [`Self::issue`], but the token endpoint will refuse the code.
    pub fn issue_failing(&self, authorize: &Url) -> String {
        self.issue_inner(authorize, &json!({}), Signer::Es256, true)
    }

    fn issue_inner(
        &self,
        authorize: &Url,
        overrides: &Value,
        signer: Signer,
        fail: bool,
    ) -> String {
        let q: HashMap<String, String> = authorize.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["scope"].split(' ').any(|s| s == "openid"));
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut claims = Map::new();
        claims.insert("iss".into(), json!(self.issuer));
        claims.insert("aud".into(), json!(q["client_id"]));
        claims.insert("sub".into(), json!(format!("sub-{}", &q["state"][..8])));
        claims.insert("exp".into(), json!(now + 300));
        claims.insert("iat".into(), json!(now));
        claims.insert("nonce".into(), json!(q["nonce"]));
        claims.insert("email_verified".into(), json!(true));
        if let Value::Object(o) = overrides {
            for (k, v) in o {
                if v.is_null() {
                    claims.remove(k);
                } else {
                    claims.insert(k.clone(), v.clone());
                }
            }
        }
        let code = format!("code-{}", uuid::Uuid::now_v7().simple());
        self.inner.grants.lock().unwrap().insert(
            code.clone(),
            Grant {
                claims: Value::Object(claims),
                challenge: q["code_challenge"].clone(),
                redirect_uri: q["redirect_uri"].clone(),
                client_id: q["client_id"].clone(),
                signer,
                fail,
            },
        );
        code
    }

    pub fn auth_seen(&self) -> Vec<String> {
        self.inner.auth_seen.lock().unwrap().clone()
    }
}

async fn discovery(State(inner): State<Arc<Inner>>) -> Json<Value> {
    let i = &inner.issuer;
    Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/authorize"),
        "token_endpoint": format!("{i}/token"),
        "jwks_uri": format!("{i}/jwks"),
        "response_types_supported": ["code"],
        "id_token_signing_alg_values_supported": ["RS256", "ES256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
    }))
}

async fn jwks(State(inner): State<Arc<Inner>>) -> Json<Value> {
    Json(json!({ "keys": [
        { "kty": "XYZ", "kid": "unsupported" },
        inner.keys.ec_jwk,
        { "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "rsa-1", "n": RSA_N.trim(), "e": "AQAB" },
    ]}))
}

fn token_error(status: StatusCode, error: &str) -> axum::response::Response {
    (status, Json(json!({ "error": error }))).into_response()
}

fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let v = headers.get("authorization")?.to_str().ok()?;
    let b64 = v.strip_prefix("Basic ")?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let raw = String::from_utf8(raw).ok()?;
    let (id, secret) = raw.split_once(':')?;
    let dec = |s: &str| {
        url::form_urlencoded::parse(format!("x={s}").as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    Some((dec(id), dec(secret)))
}

async fn token(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> axum::response::Response {
    let client_id = if let Some((id, secret)) = basic_auth(&headers) {
        inner.auth_seen.lock().unwrap().push("basic".into());
        if id != CLIENT_ID || secret != CLIENT_SECRET {
            return token_error(StatusCode::UNAUTHORIZED, "invalid_client");
        }
        id
    } else if let Some(secret) = form.get("client_secret") {
        inner.auth_seen.lock().unwrap().push("post".into());
        if form.get("client_id").map(String::as_str) != Some(CLIENT_ID) || secret != CLIENT_SECRET {
            return token_error(StatusCode::UNAUTHORIZED, "invalid_client");
        }
        CLIENT_ID.to_owned()
    } else {
        inner.auth_seen.lock().unwrap().push("none".into());
        if form.get("client_id").map(String::as_str) != Some(PUBLIC_CLIENT_ID) {
            return token_error(StatusCode::UNAUTHORIZED, "invalid_client");
        }
        PUBLIC_CLIENT_ID.to_owned()
    };
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return token_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    }
    let Some(grant) = form
        .get("code")
        .and_then(|c| inner.grants.lock().unwrap().remove(c))
    else {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(<sha2::Sha256 as sha2::Digest>::digest(verifier.as_bytes()));
    if grant.fail
        || challenge != grant.challenge
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
        || client_id != grant.client_id
    {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let hs = EncodingKey::from_secret(CLIENT_SECRET.as_bytes());
    let (alg, kid, key) = match grant.signer {
        Signer::Es256 => (Algorithm::ES256, Some("ec-1"), &inner.keys.ec),
        Signer::Rogue => (Algorithm::ES256, Some("ec-1"), &inner.keys.rogue),
        Signer::Rs256 => (Algorithm::RS256, Some("rsa-1"), &inner.keys.rsa),
        Signer::Hs256 => (Algorithm::HS256, None, &hs),
    };
    let mut header = Header::new(alg);
    header.kid = kid.map(Into::into);
    let id_token = jsonwebtoken::encode(&header, &grant.claims, key).unwrap();
    Json(json!({
        "access_token": "at-opaque",
        "token_type": "Bearer",
        "expires_in": 3600,
        "id_token": id_token,
    }))
    .into_response()
}
