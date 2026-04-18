//! # tc-otel-client
//!
//! Active ADS client for tc-otel. Complements the passive observer in `tc-otel-ads`
//! by establishing **outbound** AMS/TCP sessions to PLC targets.
//!
//! Scope:
//! - **browse**: parse and cache PLC symbol tables for UI-driven metric selection.
//! - **poll**:   periodic `ADS Read` per configured `custom_metrics` entry.
//! - **notify**: `ADS AddDeviceNotification` subscriptions per configured entry.
//!
//! Uses the open-source [`ads`] crate for protocol + transport (pure Rust, no
//! Beckhoff router needed). No code shared with `tc-otel-ads` — the observer
//! stack remains unchanged.

pub mod browse;
pub mod cache;
pub mod client;
pub mod error;
pub mod notify;
pub mod poll;

pub use error::{ClientError, Result};

/// Re-export upstream types that consumers need without taking their own
/// dependency on the `ads` crate. Keeps the dependency boundary clean.
pub use ::ads::{AmsAddr, AmsNetId};
