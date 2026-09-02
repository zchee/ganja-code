//! What a route answers when it cannot answer: a status and a tagged JSON
//! body, upstream's envelope in this protocol's casing.
//!
//! Spec: upstream `packages/opencode/src/server/routes/instance/httpapi/errors.ts`,
//! whose tagged classes (`InvalidRequestError`, `ConflictError`,
//! `UnknownError`, the not-found family) serialize a tag beside a message.
//! Here the tag is spelled `type` in snake_case, because that is the one
//! casing rule this protocol has (see `ganja-protocol`'s
//! session-id-is-snake-case deviation).
//!
//! The mapping is a table, not a judgment call per route:
//! [`EngineError::SessionNotFound`] is `404`, [`EngineError::Busy`] is `409`,
//! a payload that does not parse is `400`, a prompt somebody's own hook refused
//! is `400` too, and everything else the engine can refuse with is `500` — the
//! engine's own message as the body, because the engine already says what went
//! wrong better than a translation would.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ganja_core::EngineError;

/// A refusal on its way to the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// `404` — the named thing does not exist here.
    NotFound(String),
    /// `409` — the engine runs one turn at a time, and one is running.
    Conflict(String),
    /// `400` — the request itself is wrong: an unparseable payload, or a
    /// directory this server does not serve.
    Invalid(String),
    /// `500` — everything else, in the engine's own words.
    Internal(String),
}

impl ApiError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The envelope's tag, upstream's class name in this wire's casing.
    fn tag(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Invalid(_) => "invalid_request",
            Self::Internal(_) => "unknown",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::NotFound(message)
            | Self::Conflict(message)
            | Self::Invalid(message)
            | Self::Internal(message) => message,
        }
    }
}

impl From<EngineError> for ApiError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::SessionNotFound { .. } => Self::NotFound(error.to_string()),
            EngineError::Busy => Self::Conflict(error.to_string()),
            // Nothing went wrong here: a hook this server's own config asked
            // for looked at the prompt and refused it. `500` would report the
            // operator's policy as a fault of the server carrying it out.
            EngineError::HookRefused { .. } => Self::Invalid(error.to_string()),
            // The same shape: `/team` given `/teammate`'s own subcommand is a
            // request that named the wrong command, refused before any turn
            // started, and the sentence carries the line that was meant.
            EngineError::MisdirectedCommand { .. } => Self::Invalid(error.to_string()),
            _ => Self::Internal(error.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "type": self.tag(),
            "message": self.message(),
        });

        (self.status(), Json(body)).into_response()
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
