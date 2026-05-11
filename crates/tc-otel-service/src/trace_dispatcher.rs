//! Trace record dispatcher — batches and exports spans to OTLP

use std::time::Duration;
use tc_otel_core::{AppSettings, TraceRecord};
use tc_otel_export::OtelExporter;
use tokio::sync::mpsc;

use crate::scope_resolver::ScopeResolver;

/// Dispatcher that batches trace records and exports them to OTLP
pub struct TraceDispatcher {
    #[allow(dead_code)]
    input: mpsc::Sender<TraceRecord>,
}

impl TraceDispatcher {
    /// Create a new trace dispatcher with a no-op ScopeResolver
    /// (used by unit tests; the service wires the shared resolver).
    pub async fn new(settings: &AppSettings) -> tc_otel_core::error::Result<Self> {
        Self::with_scope_resolver(settings, ScopeResolver::noop()).await
    }

    /// Build a dispatcher with the process-wide `ScopeResolver`. The
    /// batch worker resolves each record's `scope_name` (which the
    /// `SpanDispatcher` set to the raw PLC namespace) to the owning
    /// FB's type name via the symbol table — same lookup the log
    /// path runs, so logs / traces of the same FB share a scope.
    pub async fn with_scope_resolver(
        settings: &AppSettings,
        scope_resolver: ScopeResolver,
    ) -> tc_otel_core::error::Result<Self> {
        let batch_size = settings.traces.export.batch_size;
        let flush_interval_ms = settings.traces.export.flush_interval_ms;

        // TODO(phase-2): hot reload for trace configuration

        let (input, mut output) = mpsc::channel::<TraceRecord>(256);

        // Spawn batch worker task
        let endpoint = settings.traces.export.endpoint.clone();
        // Per-pillar override wins; otherwise inherit the global
        // export.format (defaults to JSON for backward compat).
        let format = settings
            .traces
            .export
            .format
            .unwrap_or(settings.export.format);
        let export_cfg = tc_otel_export::exporter::ExportConfig {
            batch_size,
            max_retries: settings.export.max_retries,
            timeout_secs: settings.export.timeout_secs,
            format,
            endpoint: endpoint
                .clone()
                .unwrap_or_else(|| "http://localhost:4318/v1/traces".to_string()),
            ..Default::default()
        };
        let exporter = OtelExporter::with_config(export_cfg);

        tokio::spawn(async move {
            if endpoint.is_none() {
                tracing::debug!("Traces export endpoint not configured, discarding spans");
                return;
            }

            let mut batch = Vec::with_capacity(batch_size);
            let mut flush_interval =
                tokio::time::interval(Duration::from_millis(flush_interval_ms));

            loop {
                tokio::select! {
                    Some(mut record) = output.recv() => {
                        resolve_scope(&scope_resolver, &mut record).await;
                        batch.push(record);
                        if batch.len() >= batch_size {
                            if let Err(e) = exporter.export_traces_batch(batch.clone()).await {
                                tracing::error!("Failed to export trace batch: {}", e);
                            }
                            batch.clear();
                        }
                    }
                    _ = flush_interval.tick() => {
                        if !batch.is_empty() {
                            if let Err(e) = exporter.export_traces_batch(batch.clone()).await {
                                tracing::error!("Failed to export trace batch: {}", e);
                            }
                            batch.clear();
                        }
                    }
                }
            }
        });

        Ok(Self { input })
    }

    /// Send a trace record for batching and export
    #[allow(dead_code)]
    pub async fn dispatch(&self, record: TraceRecord) -> tc_otel_core::error::Result<()> {
        self.input.send(record).await.map_err(|_| {
            tc_otel_core::error::Error::ConnectionError("trace channel closed".to_string())
        })
    }

    /// Obtain a sender so another component (SpanDispatcher) can feed
    /// finalised TraceRecords into this dispatcher's batch/export pipeline.
    pub fn sender(&self) -> mpsc::Sender<TraceRecord> {
        self.input.clone()
    }
}

/// Map the record's raw PLC namespace (`record.scope_name`) to the
/// owning FB's type via the symbol-table lookup. On a hit the span
/// also gains `plc.instance_path` so two instances of the same FB
/// type (e.g. fbMotor vs fbMotorB) stay distinguishable inside the
/// shared `InstrumentationScope`.
async fn resolve_scope(resolver: &ScopeResolver, record: &mut TraceRecord) {
    if record.scope_name.is_empty() {
        return;
    }
    let net_id = record
        .resource_attributes
        .get("plc.ams_net_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let app_port = record
        .resource_attributes
        .get("plc.ams_app_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u16;
    if net_id.is_empty() || app_port == 0 {
        return;
    }
    let outcome = resolver.resolve(&net_id, &record.scope_name, app_port).await;
    if let Some(path) = outcome.instance_path {
        record.span_attributes.insert(
            "plc.instance_path".to_string(),
            serde_json::Value::String(path),
        );
    }
    record.scope_name = outcome.scope_name;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_trace_dispatcher_construction() {
        let settings = AppSettings::default();
        let result = TraceDispatcher::new(&settings).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_trace_dispatcher_shutdown_on_channel_close() {
        let settings = AppSettings::default();
        let dispatcher = TraceDispatcher::new(&settings).await.unwrap();
        drop(dispatcher);
        // Should not panic — task should cleanly shut down
    }
}
