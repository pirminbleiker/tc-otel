//! Core data models and types for Log4TC logging bridge
//!
//! This crate defines the fundamental types and structures used throughout Log4TC,
//! including LogEntry, LogLevel, LogRecord, and configuration structures.

pub mod config;
pub mod diagnostics;
pub mod error;
pub mod formatter;
pub mod models;
pub mod subscriptions;

pub use config::{
    AppSettings, ExportConfig, LogFormat, LoggingConfig, OutputConfig, ReceiverConfig,
    ServiceConfig, TlsConfig, WebConfig,
};
pub use diagnostics::{DiagnosticSnapshot, DiagnosticStats};
pub use error::{Error, Result};
pub use formatter::MessageFormatter;
pub use models::{LogEntry, LogLevel, LogRecord};
pub use subscriptions::{SubscriptionError, SubscriptionManager};
