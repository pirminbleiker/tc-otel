//! OTEL-specific error types

use thiserror::Error;

pub type Result<T> = std::result::Result<T, OtelError>;

#[derive(Error, Debug)]
pub enum OtelError {
    #[error("Invalid OTEL request: {0}")]
    InvalidRequest(String),

    #[error("OTEL export failed: {0}")]
    ExportFailed(String),

    #[error("OTEL receiver error: {0}")]
    ReceiverError(String),

    #[error("JSON parse error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    HttpError(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("Internal error: {0}")]
    Internal(String),
}
