//! Core data models and types for Log4TC logging bridge
//!
//! This crate defines the fundamental types and structures used throughout Log4TC,
//! including LogEntry, LogLevel, LogRecord, and configuration structures.

pub mod models;
pub mod error;
pub mod config;
pub mod formatter;

pub use models::{LogEntry, LogLevel, LogRecord};
pub use error::{Result, Error};
pub use config::{AppSettings, LoggingConfig, ReceiverConfig, ExportConfig, OutputConfig, ServiceConfig, LogFormat};
pub use formatter::MessageFormatter;
