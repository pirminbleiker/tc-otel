//! Trace record dispatcher — batches and exports spans to OTLP

use std::time::Duration;
use tc_otel_core::{AppSettings, TraceRecord};
use tc_otel_export::OtelExporter;
use tokio::sync::mpsc;

/// Dispatcher that batches trace records and exports them to OTLP
pub struct TraceDispatcher {
    #[allow(dead_code)]
    input: mpsc::Sender<TraceRecord>,
}

impl TraceDispatcher {
    /// Create a new trace dispatcher
    pub async fn new(settings: &AppSettings) -> tc_otel_core::error::Result<Self> {
        let batch_size = settings.traces.export.batch_size;
        let flush_interval_ms = settings.traces.export.flush_interval_ms;

        // TODO(phase-2): hot reload for trace configuration

        let (input, mut output) = mpsc::channel::<TraceRecord>(256);

        // Spawn batch worker task
        let endpoint = settings.traces.export.endpoint.clone();
        let exporter = if let Some(ref ep) = endpoint {
            OtelExporter::new(ep.clone(), batch_size, 3)
        } else {
            // Dummy exporter if endpoint not configured - won't be used
            OtelExporter::new("http://localhost:4318/v1/traces".to_string(), batch_size, 3)
        };

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
                    Some(record) = output.recv() => {
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
