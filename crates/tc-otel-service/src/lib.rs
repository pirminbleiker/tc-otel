//! tc-otel service library — shared components for integration testing

#[cfg(feature = "client-bridge")]
pub mod client_bridge;
pub mod cycle_time;
pub mod diagnostics_bridge;
pub mod span_dispatcher;
pub mod system_metrics;
pub mod trace_dispatcher;
