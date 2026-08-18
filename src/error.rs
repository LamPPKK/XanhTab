use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("authentication required")]
    Unauthorized,
    #[error("invalid or expired CSRF token")]
    InvalidCsrf,
    #[error("pairing secret is invalid or has already been used")]
    InvalidPairing,
    #[error("control lease is held by another client")]
    LeaseConflict,
    #[error("session is not active")]
    SessionNotActive,
    #[error("session transition is not allowed from the current state")]
    InvalidTransition,
    #[error("resource not found")]
    NotFound,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),
    #[error("internal error")]
    Internal,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorPayload<'a>,
}

#[derive(Serialize)]
struct ErrorPayload<'a> {
    code: &'a str,
    message: String,
}

impl AppError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "AUTH_REQUIRED"),
            Self::InvalidCsrf => (StatusCode::FORBIDDEN, "CSRF_INVALID"),
            Self::InvalidPairing => (StatusCode::UNAUTHORIZED, "PAIRING_INVALID"),
            Self::LeaseConflict => (StatusCode::CONFLICT, "LEASE_CONFLICT"),
            Self::SessionNotActive => (StatusCode::CONFLICT, "SESSION_INACTIVE"),
            Self::InvalidTransition => (StatusCode::CONFLICT, "SESSION_TRANSITION_INVALID"),
            Self::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            Self::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "REQUEST_INVALID"),
            Self::ServiceUnavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, code) = self.status_and_code();
        let message = self.to_string();
        (
            status,
            Json(ErrorBody {
                error: ErrorPayload { code, message },
            }),
        )
            .into_response()
    }
}
