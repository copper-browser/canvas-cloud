//! Shared helpers: the error type with route-specific codes, paging, subject rules.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use canvas_cloud_core::error::{loggable_message, ApiError, ErrorLog};
use canvas_cloud_core::identity::MAX_SUBJECT_CHARS;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Default and maximum page sizes of the list routes (a linked app pages its users with
/// `limit=500`).
pub const DEFAULT_LIMIT: i64 = 100;
pub const MAX_LIMIT: i64 = 500;

/// A route error: either a plain [`ApiError`] or one with a route-specific machine-readable
/// code and status (`409 email_taken`, `422 bad_line`, …) and optional extra body members.
/// The body is always `{"error","message",…}`.
#[derive(Debug)]
pub enum Error {
    Api(ApiError),
    Coded {
        status: StatusCode,
        code: &'static str,
        message: String,
        extra: Map<String, Value>,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn coded(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self::Coded {
            status,
            code,
            message: message.into(),
            extra: Map::new(),
        }
    }

    /// Adds `key: value` to the error body.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        if let Self::Coded { extra, .. } = &mut self {
            extra.insert(key.to_owned(), value.into());
        }
        self
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::Api(ApiError::bad_request(msg))
    }

    pub fn conflict(code: &'static str, msg: impl Into<String>) -> Self {
        Self::coded(StatusCode::CONFLICT, code, msg)
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::Api(e) => e.status(),
            Self::Coded { status, .. } => *status,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Api(e) => e.code(),
            Self::Coded { code, .. } => code,
        }
    }

    /// The client-facing message (what the response body's `message` says).
    pub fn message(&self) -> String {
        match self {
            Self::Coded { message, .. } => message.clone(),
            Self::Api(ApiError::BadRequest(m)) => m.clone(),
            Self::Api(
                ApiError::UnauthorizedWith { message, .. }
                | ApiError::Denied { message, .. }
                | ApiError::NotFoundCode { message, .. },
            ) => (*message).to_owned(),
            Self::Api(ApiError::Conflict(Value::Object(m))) => m
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("conflict")
                .to_owned(),
            Self::Api(ApiError::Internal(_)) => "internal error".to_owned(),
            Self::Api(e) => e.to_string(),
        }
    }
}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Api(e.into())
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Self::Api(e) => e.into_response(),
            Self::Coded {
                status,
                code,
                message,
                mut extra,
            } => {
                let log = ErrorLog {
                    code,
                    message: Some(loggable_message(&message)),
                };
                extra.insert("error".into(), Value::from(code));
                extra.insert("message".into(), Value::from(message));
                let mut resp = (status, axum::Json(Value::Object(extra))).into_response();
                resp.extensions_mut().insert(log);
                resp
            }
        }
    }
}

/// `?limit=&offset=` (plus the canvases list's `kind`).
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub kind: Option<String>,
}

impl ListQuery {
    /// `(limit, offset)`: limit clamped to `1..=MAX_LIMIT` (default [`DEFAULT_LIMIT`]),
    /// offset at least 0.
    pub fn page(&self) -> (i64, i64) {
        (
            self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            self.offset.unwrap_or(0).max(0),
        )
    }
}

/// A subject an app can later act as: what `X-Canvas-User` can carry — 1–200 printable ASCII
/// characters (space allowed inside), no leading or trailing whitespace.
pub fn check_subject(raw: &str) -> Result<&str> {
    let ok = !raw.is_empty()
        && raw.chars().count() <= MAX_SUBJECT_CHARS
        && raw.bytes().all(|b| (0x20..=0x7e).contains(&b))
        && raw.trim() == raw;
    if ok {
        Ok(raw)
    } else {
        Err(Error::bad_request(
            "subject must be 1-200 printable ASCII characters without surrounding spaces",
        ))
    }
}

/// `""` for a missing/blank display name (what an app user without one stores), else the
/// cleaned name (control characters dropped, trimmed, at most 200 characters).
pub fn clean_display_name(raw: Option<&str>) -> String {
    canvas_cloud_core::identity::clean_name(
        raw,
        canvas_cloud_core::identity::MAX_DISPLAY_NAME_CHARS,
    )
    .unwrap_or_default()
}

/// A stored display name as JSON: `null` when empty.
pub fn display_name_json(name: &str) -> Value {
    if name.is_empty() {
        Value::Null
    } else {
        Value::from(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects() {
        for ok in [
            "a",
            "0189f7d0-0000-7000-8000-000000000001",
            "user 7",
            &"x".repeat(200),
        ] {
            assert!(check_subject(ok).is_ok(), "{ok}");
        }
        for bad in ["", " a", "a ", "tab\tin", "nl\n", "é", &"x".repeat(201)] {
            assert!(check_subject(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn paging() {
        let q = ListQuery {
            limit: Some(10_000),
            offset: Some(-5),
            kind: None,
        };
        assert_eq!(q.page(), (MAX_LIMIT, 0));
        assert_eq!(ListQuery::default().page(), (DEFAULT_LIMIT, 0));
        let q = ListQuery {
            limit: Some(0),
            ..ListQuery::default()
        };
        assert_eq!(q.page().0, 1);
    }

    #[test]
    fn coded_errors_have_the_shared_shape() {
        let e = Error::conflict("canvas_exists", "exists").with("digest", "ab");
        assert_eq!(e.status(), StatusCode::CONFLICT);
        assert_eq!(e.code(), "canvas_exists");
        assert_eq!(e.message(), "exists");
    }
}
