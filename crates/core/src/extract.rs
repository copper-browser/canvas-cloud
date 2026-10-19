//! Request-body extractors with explicit, per-route size limits.

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Request};
use axum::http::header::CONTENT_LENGTH;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// JSON body capped at `LIMIT` bytes (413 above it, 400 on malformed JSON). Unlike
/// `axum::Json` it does not insist on a `Content-Type` header (Swift/curl clients often
/// omit it) and maps errors onto [`ApiError`].
pub struct JsonBody<T, const LIMIT: usize>(pub T);

impl<S, T, const LIMIT: usize> FromRequest<S> for JsonBody<T, LIMIT>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let bytes = read_body(req.into_body_with_len_check(LIMIT)?, LIMIT).await?;
        parse_json(&bytes).map(Self)
    }
}

/// Like [`JsonBody`], but an empty (or whitespace-only) body yields `T::default()`.
pub struct OptionalJsonBody<T, const LIMIT: usize>(pub T);

impl<S, T, const LIMIT: usize> FromRequest<S> for OptionalJsonBody<T, LIMIT>
where
    S: Send + Sync,
    T: DeserializeOwned + Default,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let bytes = read_body(req.into_body_with_len_check(LIMIT)?, LIMIT).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self(T::default()));
        }
        parse_json(&bytes).map(Self)
    }
}

/// Query-string extractor whose rejection is a JSON 400 ([`ApiError::BadRequest`]).
pub struct QueryParams<T>(pub T);

impl<S, T> axum::extract::FromRequestParts<S> for QueryParams<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|q| Self(q.0))
            .map_err(|e| ApiError::bad_request(format!("invalid query string: {}", e.body_text())))
    }
}

trait LenCheck {
    fn into_body_with_len_check(self, limit: usize) -> Result<Body, ApiError>;
}

impl LenCheck for Request {
    fn into_body_with_len_check(self, limit: usize) -> Result<Body, ApiError> {
        check_content_length(&self, limit)?;
        Ok(self.into_body())
    }
}

/// Fast 413 from the `Content-Length` header before reading anything.
pub fn check_content_length(req: &Request, limit: usize) -> Result<(), ApiError> {
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    match declared {
        Some(n) if n > limit as u64 => Err(ApiError::PayloadTooLarge),
        _ => Ok(()),
    }
}

/// A client must finish sending a request body within this long (slow-upload guard).
pub const BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Read a body fully, failing with 413 once more than `limit` bytes arrive and with 400 if
/// it takes longer than [`BODY_READ_TIMEOUT`].
pub async fn read_body(body: Body, limit: usize) -> Result<Bytes, ApiError> {
    let read = tokio::time::timeout(BODY_READ_TIMEOUT, axum::body::to_bytes(body, limit))
        .await
        .map_err(|_| ApiError::bad_request("timed out reading request body"))?;
    read.map_err(|err| {
        let is_limit = std::error::Error::source(&err)
            .is_some_and(<dyn std::error::Error + 'static>::is::<http_body_util::LengthLimitError>)
            || err.to_string().contains("length limit");
        if is_limit {
            ApiError::PayloadTooLarge
        } else {
            ApiError::bad_request("failed to read request body")
        }
    })
}

/// Parse JSON, mapping errors to 400 with serde's message.
pub fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes)
        .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
}
