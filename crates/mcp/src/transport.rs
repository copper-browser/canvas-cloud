//! The MCP Streamable HTTP transport and JSON-RPC 2.0 dispatch (`POST /mcp`).
//!
//! * Each `POST` carries one JSON-RPC message. A request gets one `application/json`
//!   response; a notification or a client response gets `202 Accepted` with no body. Batches
//!   (JSON arrays) were removed from the protocol in 2025-06-18 and are refused (`400`) unless
//!   the client says `MCP-Protocol-Version: 2025-03-26`.
//! * Stateless: no `Mcp-Session-Id` is issued (one sent by the client is ignored), every
//!   request authenticates on its own, and `initialize` is not required before other methods.
//! * `GET` (and every other method) is `405` with `Allow: POST`: this server never opens a
//!   server-to-client stream.
//! * Order of checks: `Origin` (DNS-rebinding guard, `403`) → credential (`401` with
//!   `WWW-Authenticate: Bearer realm="canvas-cloud"`) → `MCP-Protocol-Version` (`400` when
//!   unsupported) → body size (`413`) → JSON (`400` parse error).

use axum::extract::{FromRequestParts as _, Request, State};
use axum::http::header::{ALLOW, CONTENT_LENGTH, ORIGIN};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use canvas_cloud_core::config::Config;
use canvas_cloud_core::identity::{AuthUser, AuthUserAnyScope};
use canvas_cloud_core::state::SharedState;
use serde_json::{json, Map, Value};

use crate::tools;

/// The protocol revision this server implements (and answers with by default).
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// The previous revision, still accepted (negotiated when a client asks for it).
pub const PROTOCOL_VERSION_2025_03_26: &str = "2025-03-26";
/// Every revision `initialize` agrees to, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[PROTOCOL_VERSION, PROTOCOL_VERSION_2025_03_26];
/// Largest accepted `POST /mcp` body: an `apply_ops` call as large as `POST /ops` allows, plus
/// the JSON-RPC envelope.
pub const MAX_BODY_BYTES: usize = canvas_cloud_engine::MAX_OPS_BODY_BYTES + 64 * 1024;

const PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// JSON-RPC 2.0 error codes.
pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
/// Implementation-defined server error (used for the `Origin` refusal).
const SERVER_ERROR: i64 = -32000;

/// A JSON-RPC error object (`{code, message}`).
#[derive(Debug, Clone)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn error_message(id: Value, err: &RpcError) -> Value {
    let mut message = json!({
        "jsonrpc": "2.0",
        "error": { "code": err.code, "message": err.message },
    });
    message["id"] = id;
    message
}

/// An HTTP error whose body is a JSON-RPC error response without an id.
fn http_rpc_error(status: StatusCode, err: &RpcError) -> Response {
    (status, Json(error_message(Value::Null, err))).into_response()
}

fn accepted() -> Response {
    StatusCode::ACCEPTED.into_response()
}

/// `true` when `origin` may call `/mcp` from a browser: it is listed in
/// `cors.allowed_origins` or is `public_url`'s own origin (a web app served by this server).
pub fn origin_allowed(cfg: &Config, origin: &str) -> bool {
    let origin = origin.trim().trim_end_matches('/');
    if origin.is_empty() || origin.eq_ignore_ascii_case("null") {
        return false;
    }
    cfg.cors
        .allowed_origins
        .iter()
        .any(|a| a.trim().trim_end_matches('/').eq_ignore_ascii_case(origin))
        || url::Url::parse(&cfg.public_url)
            .ok()
            .map(|u| u.origin().ascii_serialization())
            .is_some_and(|own| own != "null" && own.eq_ignore_ascii_case(origin))
}

/// `false` when the request carries an `Origin` that may not call `/mcp`.
fn origin_ok(cfg: &Config, headers: &HeaderMap) -> bool {
    headers
        .get(ORIGIN)
        .is_none_or(|value| value.to_str().is_ok_and(|o| origin_allowed(cfg, o)))
}

/// The `MCP-Protocol-Version` header: `None` when absent (a 2025-03-26 client, or an
/// `initialize`), else one of [`SUPPORTED_PROTOCOL_VERSIONS`]; `Err` (the message for a
/// `400`) for anything else.
fn protocol_header(headers: &HeaderMap) -> Result<Option<&'static str>, String> {
    let Some(value) = headers.get(PROTOCOL_HEADER) else {
        return Ok(None);
    };
    let raw = value.to_str().unwrap_or_default().trim();
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == raw)
        .map(|v| Some(*v))
        .ok_or_else(|| {
            format!(
                "unsupported MCP-Protocol-Version {:?} (supported: {})",
                raw.chars().take(32).collect::<String>(),
                SUPPORTED_PROTOCOL_VERSIONS.join(", ")
            )
        })
}

fn too_large() -> Response {
    http_rpc_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        &RpcError::new(
            INVALID_REQUEST,
            format!("request body over {MAX_BODY_BYTES} bytes"),
        ),
    )
}

/// `POST /mcp`.
pub(crate) async fn post_mcp(State(state): State<SharedState>, req: Request) -> Response {
    let (mut parts, body) = req.into_parts();
    if !origin_ok(&state.cfg, &parts.headers) {
        tracing::warn!("mcp request from a disallowed Origin refused");
        return http_rpc_error(
            StatusCode::FORBIDDEN,
            &RpcError::new(SERVER_ERROR, "Forbidden: this Origin may not call /mcp"),
        );
    }
    let user = match AuthUserAnyScope::from_request_parts(&mut parts, &state).await {
        Ok(AuthUserAnyScope(user)) => user,
        Err(e) => return e.into_response(),
    };
    let negotiated = match protocol_header(&parts.headers) {
        Ok(v) => v,
        Err(message) => {
            return http_rpc_error(
                StatusCode::BAD_REQUEST,
                &RpcError::new(INVALID_REQUEST, message),
            )
        }
    };
    let declared_len = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared_len.is_some_and(|n| n > MAX_BODY_BYTES as u64) {
        return too_large();
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return too_large();
    };
    let message: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return http_rpc_error(
                StatusCode::BAD_REQUEST,
                &RpcError::new(PARSE_ERROR, format!("parse error: {e}")),
            )
        }
    };
    match message {
        Value::Array(batch) => {
            if negotiated != Some(PROTOCOL_VERSION_2025_03_26) {
                return http_rpc_error(
                    StatusCode::BAD_REQUEST,
                    &RpcError::new(
                        INVALID_REQUEST,
                        "JSON-RPC batches are not supported in protocol 2025-06-18: send one message per request",
                    ),
                );
            }
            if batch.is_empty() {
                return http_rpc_error(
                    StatusCode::BAD_REQUEST,
                    &RpcError::new(INVALID_REQUEST, "empty batch"),
                );
            }
            let mut replies = Vec::new();
            for item in batch {
                match handle(&state, &user, item).await {
                    Outcome::Reply(v) | Outcome::Invalid(v) => replies.push(v),
                    Outcome::NoReply => {}
                }
            }
            if replies.is_empty() {
                accepted()
            } else {
                Json(Value::Array(replies)).into_response()
            }
        }
        single => match handle(&state, &user, single).await {
            Outcome::Reply(v) => Json(v).into_response(),
            Outcome::Invalid(v) => (StatusCode::BAD_REQUEST, Json(v)).into_response(),
            Outcome::NoReply => accepted(),
        },
    }
}

/// Every method other than `POST` on `/mcp`.
pub(crate) async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(ALLOW, "POST")],
        Json(json!({
            "error": "method_not_allowed",
            "message": "use POST /mcp (MCP Streamable HTTP with JSON responses); this server opens no server-to-client stream",
        })),
    )
        .into_response()
}

enum Outcome {
    /// A JSON-RPC response (result or method-level error).
    Reply(Value),
    /// A JSON-RPC error for a message that is not a valid request (`400` when alone).
    Invalid(Value),
    /// A notification or a client response: `202`, no body.
    NoReply,
}

async fn handle(state: &SharedState, user: &AuthUser, message: Value) -> Outcome {
    let Value::Object(mut obj) = message else {
        return Outcome::Invalid(error_message(
            Value::Null,
            &RpcError::new(INVALID_REQUEST, "expected a JSON-RPC 2.0 message object"),
        ));
    };
    let id = obj.remove("id");
    let id_or_null = || id.clone().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Outcome::Invalid(error_message(
            id_or_null(),
            &RpcError::new(INVALID_REQUEST, "`jsonrpc` must be \"2.0\""),
        ));
    }
    let method = match obj.remove("method") {
        Some(Value::String(m)) => m,
        Some(_) => {
            return Outcome::Invalid(error_message(
                id_or_null(),
                &RpcError::new(INVALID_REQUEST, "`method` must be a string"),
            ))
        }
        // A response to a server request — this server sends none, so there is nothing to do.
        None if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) => {
            return Outcome::NoReply
        }
        None => {
            return Outcome::Invalid(error_message(
                id_or_null(),
                &RpcError::new(INVALID_REQUEST, "missing `method`"),
            ))
        }
    };
    let Some(id) = id else {
        tracing::debug!(%method, "mcp notification");
        return Outcome::NoReply;
    };
    if !(id.is_string() || id.is_i64() || id.is_u64()) {
        return Outcome::Invalid(error_message(
            Value::Null,
            &RpcError::new(INVALID_REQUEST, "`id` must be a string or an integer"),
        ));
    }
    let params = obj.remove("params").unwrap_or(Value::Null);
    let reply = match dispatch(state, user, &method, params).await {
        Ok(result) => {
            let mut reply = json!({ "jsonrpc": "2.0", "id": id });
            reply["result"] = result;
            reply
        }
        Err(err) => {
            tracing::debug!(%method, code = err.code, "mcp request failed");
            error_message(id, &err)
        }
    };
    Outcome::Reply(reply)
}

async fn dispatch(
    state: &SharedState,
    user: &AuthUser,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    let params = match params {
        Value::Null => Map::new(),
        Value::Object(m) => m,
        _ => return Err(RpcError::new(INVALID_PARAMS, "`params` must be an object")),
    };
    match method {
        "initialize" => Ok(initialize(user, &params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::definitions(user.is_read_only()) })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::new(INVALID_PARAMS, "`name` (the tool) is required"))?;
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            tools::call(state, user, name, arguments).await
        }
        other => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("method not found: {other}"),
        )),
    }
}

fn initialize(user: &AuthUser, params: &Map<String, Value>) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .and_then(|r| SUPPORTED_PROTOCOL_VERSIONS.iter().find(|v| **v == r))
        .copied()
        .unwrap_or(PROTOCOL_VERSION);
    let client = params
        .get("clientInfo")
        .and_then(|c| c.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    tracing::info!(
        user_id = %user.user_id,
        client = %client.chars().take(64).collect::<String>(),
        requested = requested.unwrap_or(""),
        protocol = version,
        "mcp session initialized"
    );
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "canvas-cloud",
            "title": "Canvas Cloud",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": tools::INSTRUCTIONS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(origins: &[&str], public_url: &str) -> Config {
        let mut cfg = Config::for_tests("postgres://localhost/unused");
        cfg.cors.allowed_origins = origins.iter().map(|s| (*s).to_owned()).collect();
        cfg.public_url = public_url.to_owned();
        cfg
    }

    #[test]
    fn origins() {
        let c = cfg(
            &["https://app.example.com"],
            "https://canvas.example.com:8443",
        );
        assert!(origin_allowed(&c, "https://app.example.com"));
        assert!(origin_allowed(&c, "https://APP.example.com/"));
        assert!(origin_allowed(&c, "https://canvas.example.com:8443"));
        for bad in [
            "https://canvas.example.com",
            "http://app.example.com",
            "https://evil.example.com",
            "null",
            "",
        ] {
            assert!(!origin_allowed(&c, bad), "{bad}");
        }
        let c = cfg(&[], "https://canvas.example.com");
        assert!(origin_allowed(&c, "https://canvas.example.com"));
        assert!(!origin_allowed(&c, "http://localhost:5173"));
    }

    #[test]
    fn protocol_versions() {
        let mut h = HeaderMap::new();
        assert_eq!(protocol_header(&h).unwrap(), None);
        h.insert(PROTOCOL_HEADER, "2025-03-26".parse().unwrap());
        assert_eq!(protocol_header(&h).unwrap(), Some("2025-03-26"));
        h.insert(PROTOCOL_HEADER, "2025-06-18".parse().unwrap());
        assert_eq!(protocol_header(&h).unwrap(), Some("2025-06-18"));
        h.insert(PROTOCOL_HEADER, "2024-11-05".parse().unwrap());
        assert!(protocol_header(&h).unwrap_err().contains("2024-11-05"));
    }
}
