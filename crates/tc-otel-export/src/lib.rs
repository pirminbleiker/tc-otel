//! OpenTelemetry (OTEL) OTLP receiver and exporter for tc-otel
//!
//! This crate implements the OTEL protocol endpoints for receiving telemetry
//! from TwinCAT PLCs and exporting logs, metrics, and traces to OTEL backends.
//!
//! Supports:
//! - **Outbound** OTLP/HTTP export for logs, metrics, traces.
//!   Default wire format is OTLP-Protobuf (`Content-Type:
//!   application/x-protobuf`); JSON is opt-in via `format = "json"`.
//! - **Inbound** OTLP/gRPC receiver on port 4317 — **logs only**.
//!   `MetricsServiceServer` and `TraceServiceServer` are intentionally
//!   not implemented (D4 in the OTel-conformance audit): tc-otel
//!   produces metrics and traces from PLC ADS frames, never ingests
//!   them from external OTLP pushers. Re-evaluate only when a real
//!   use case appears.
//! - Log record transformation to OTEL LogRecord format
//! - Batching and retry logic for exports

pub mod error;
pub mod exporter;
pub mod grpc;
pub mod mapping;
pub mod logs_proto;
pub mod metrics_proto;
pub mod proto_common;
pub mod receiver;
pub mod traces_proto;

pub use error::{OtelError, Result};
pub use exporter::OtelExporter;
pub use grpc::{LogsServiceImpl, LogsServiceServer};
pub use mapping::OtelMapping;
pub use receiver::{OtelGrpcReceiver, OtelHttpReceiver};
pub use tc_otel_core::WireFormat;
