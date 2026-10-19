//! Errors: core's [`ApiError`] plus the few statuses it has no variant for (a `400` or `502`
//! with a route-specific code). Same `{"error","message"}` JSON body either way.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use canvas_cloud_core::error::{ApiError, ErrorLog};
use serde_json::json;

#[derive(Debug)]
pub(crate) enum Error {
    Api(ApiError),
    Coded {
        status: StatusCode,
        code: &'static str,
        message: String,
    },
}

impl Error {
    pub(crate) fn coded(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self::Coded {
            status,
            code,
            message: message.into(),
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
            } => {
                let mut resp =
                    (status, Json(json!({ "error": code, "message": message }))).into_response();
                resp.extensions_mut().insert(ErrorLog {
                    code,
                    message: None,
                });
                resp
            }
        }
    }
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// `401 invalid_credentials` — the one answer for an unknown email, a wrong password, an
/// account without a password and an over-long password.
pub(crate) fn invalid_credentials() -> ApiError {
    ApiError::UnauthorizedWith {
        code: "invalid_credentials",
        message: "invalid email or password",
    }
}

/// `403 session_required` — the route manages credentials, so only a signed-in session (not a
/// personal access token or an app acting for the user) may call it.
pub(crate) fn session_required() -> ApiError {
    ApiError::Denied {
        code: "session_required",
        message: "this requires a signed-in session (not an access token or app key)",
    }
}
