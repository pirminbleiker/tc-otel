use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("ads protocol error: {0}")]
    Ads(String),

    #[error("symbol not found: {0}")]
    SymbolNotFound(String),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("target not configured: {0}")]
    UnknownTarget(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ClientError>;
