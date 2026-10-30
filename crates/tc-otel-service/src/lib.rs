//! tc-otel service library — shared components for integration testing

#[cfg(feature = "client-bridge")]
pub mod client_bridge;
pub mod config_watcher;
pub mod cycle_time;
pub mod diagnostics_bridge;
pub mod dispatcher;
pub mod scope_resolver;
pub mod service;
pub mod span_dispatcher;
pub mod system_metrics;
pub mod trace_dispatcher;
pub mod web;
