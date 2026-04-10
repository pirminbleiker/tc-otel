//! ADS protocol-specific error types

use thiserror::Error;

pub type Result<T> = std::result::Result<T, AdsError>;

#[derive(Error, Debug)]
pub enum AdsError {
    #[error("Invalid protocol version: {0}")]
    InvalidVersion(u8),

    #[error("Incomplete message: expected {expected}, got {got}")]
    IncompleteMessage { expected: usize, got: usize },

    #[error("Invalid string encoding: {0}")]
    InvalidStringEncoding(String),

    #[error("Invalid timestamp: {0}")]
    InvalidTimestamp(String),

    #[error("Buffer error: {0}")]
    BufferError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Conversion error: {0}")]
    ConversionError(String),
}
