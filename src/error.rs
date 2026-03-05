use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::sandbox::SandboxState;

#[derive(thiserror::Error, Debug)]
pub enum DenError {
    #[error("docker: {0}")]
    Docker(#[from] bollard::errors::Error),

    #[error("file not found: {path}")]
    FileNotFound { path: String },

    #[error("sandbox {id} not found")]
    SandboxNotFound { id: String },

    #[error("sandbox {id} in state {current:?}, cannot {operation}")]
    InvalidState {
        id: String,
        current: SandboxState,
        operation: String,
    },

    #[error("no sandboxes available for tier {tier}")]
    PoolExhausted { tier: String },

    #[error("insufficient resources: {reason}")]
    InsufficientResources { reason: String },

    #[error("tier {tier} not found")]
    TierNotFound { tier: String },

    #[error("timeout after {seconds}s")]
    Timeout { seconds: u64 },

    #[error("proxy: {0}")]
    Proxy(String),

    #[error("config: {0}")]
    Config(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for DenError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            DenError::FileNotFound { .. } => (StatusCode::NOT_FOUND, self.to_string()),
            DenError::SandboxNotFound { .. } => (StatusCode::NOT_FOUND, self.to_string()),
            DenError::TierNotFound { .. } => (StatusCode::NOT_FOUND, self.to_string()),
            DenError::PoolExhausted { .. } => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            DenError::InvalidState { .. } => (StatusCode::CONFLICT, self.to_string()),
            DenError::InsufficientResources { .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
            }
            DenError::Timeout { .. } => (StatusCode::GATEWAY_TIMEOUT, self.to_string()),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        (status, axum::Json(json!({ "error": msg }))).into_response()
    }
}
