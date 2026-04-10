//! Log dispatcher with batched async export to Victoria-Logs
//!
//! Logs are collected in a batch buffer and flushed either when the batch
//! is full or after a timeout - whichever comes first. This minimizes
//! HTTP overhead and CPU usage.

use anyhow::Result;
use std::time::Duration;
use tc_otel_core::{AppSettings, LogEntry, LogRecord, MessageFormatter};
use tokio::sync::mpsc;

/// Log dispatcher - converts LogEntries and sends them to a batched export worker
#[derive(Clone)]
pub struct LogDispatcher {
    export_tx: mpsc::Sender<LogRecord>,
}

impl LogDispatcher {
    pub async fn new(settings: &AppSettings) -> Result<Self> {
        // ENV override for endpoint, otherwise use config
        let endpoint = std::env::var("LOG4TC_EXPORT_ENDPOINT")
            .unwrap_or_else(|_| settings.export.endpoint.clone());

        let batch_size = settings.export.batch_size;
        let flush_interval = Duration::from_millis(settings.export.flush_interval_ms);

        // Bounded channel for backpressure
        let (export_tx, export_rx) = mpsc::channel::<LogRecord>(settings.service.channel_capacity);

        // Spawn background batch worker
        tokio::spawn(Self::batch_worker(
            export_rx,
            endpoint,
            batch_size,
            flush_interval,
        ));

        tracing::info!(
            "Dispatcher ready (batch={}, flush={}ms)",
            batch_size,
            flush_interval.as_millis()
        );

        Ok(Self { export_tx })
    }

    /// Dispatch a log entry - formats and sends to export worker (non-blocking)
    pub async fn dispatch(&self, entry: LogEntry) -> Result<()> {
        // Format message only if template has placeholders
        let body = if entry.message.contains('{') {
            MessageFormatter::format_with_context(&entry.message, &entry.arguments, &entry.context)
        } else {
            entry.message.clone()
        };

        let mut record = LogRecord::from_log_entry(entry);
        record.body = serde_json::Value::String(body);

        // Non-blocking send - drops if channel full (backpressure)
        if self.export_tx.try_send(record).is_err() {
            tracing::warn!("Export channel full, dropping log");
        }

        Ok(())
    }

    /// Background worker that batches records and flushes to endpoint
    async fn batch_worker(
        mut rx: mpsc::Receiver<LogRecord>,
        endpoint: String,
        batch_size: usize,
        flush_interval: Duration,
    ) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut batch: Vec<LogRecord> = Vec::with_capacity(batch_size);
        // Reusable payload buffer to avoid re-allocation across flushes
        let mut payload_buf = String::with_capacity(batch_size * 256);
        let mut interval = tokio::time::interval(flush_interval);
        let mut total_sent: u64 = 0;
        let mut total_errors: u64 = 0;

        loop {
            tokio::select! {
                // Receive new record
                Some(record) = rx.recv() => {
                    batch.push(record);
                    if batch.len() >= batch_size {
                        match Self::flush_batch(&client, &endpoint, &batch, &mut payload_buf).await {
                            Ok(n) => total_sent += n as u64,
                            Err(e) => {
                                total_errors += 1;
                                tracing::error!("Batch export error: {}", e);
                            }
                        }
                        batch.clear();
                    }
                }
                // Periodic flush
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        match Self::flush_batch(&client, &endpoint, &batch, &mut payload_buf).await {
                            Ok(n) => total_sent += n as u64,
                            Err(e) => {
                                total_errors += 1;
                                tracing::error!("Batch export error: {}", e);
                            }
                        }
                        batch.clear();
                    }
                }
                // Channel closed
                else => {
                    // Flush remaining
                    if !batch.is_empty() {
                        let _ = Self::flush_batch(&client, &endpoint, &batch, &mut payload_buf).await;
                    }
                    tracing::info!("Export worker stopped. Total sent: {}, errors: {}", total_sent, total_errors);
                    break;
                }
            }
        }
    }

    /// Flush a batch of records to the endpoint as JSONL — builds payload directly
    async fn flush_batch(
        client: &reqwest::Client,
        endpoint: &str,
        batch: &[LogRecord],
        payload: &mut String,
    ) -> Result<usize> {
        let count = batch.len();
        payload.clear();

        // Build JSONL directly without intermediate serde_json::Map
        for record in batch {
            payload.push('{');
            // _msg
            payload.push_str("\"_msg\":");
            push_json_value(payload, &record.body);
            // _time
            payload.push_str(",\"_time\":\"");
            payload.push_str(&record.timestamp.to_rfc3339());
            payload.push('"');
            // level
            payload.push_str(",\"level\":\"");
            push_json_escaped(payload, &record.severity_text.to_lowercase());
            payload.push('"');
            // severity_number
            payload.push_str(",\"severity_number\":");
            {
                use std::fmt::Write;
                let _ = write!(payload, "{}", record.severity_number);
            }

            // Scope attributes (logger name)
            for (k, v) in &record.scope_attributes {
                payload.push(',');
                push_json_key_value(payload, k, v);
            }

            // Resource attributes (service, host, task)
            for (k, v) in &record.resource_attributes {
                payload.push(',');
                push_json_key_value(payload, k, v);
            }

            // Log attributes (context, args, plc metadata)
            for (k, v) in &record.log_attributes {
                payload.push(',');
                push_json_key_value(payload, k, v);
            }

            payload.push_str("}\n");
        }

        let response = client
            .post(endpoint)
            .header("Content-Type", "application/stream+json")
            .body(payload.clone())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HTTP error: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Export failed: HTTP {} - {}", status, body));
        }

        Ok(count)
    }
}

/// Write a JSON key:value pair directly to buffer
#[inline]
fn push_json_key_value(buf: &mut String, key: &str, value: &serde_json::Value) {
    buf.push('"');
    push_json_escaped(buf, key);
    buf.push_str("\":");
    push_json_value(buf, value);
}

/// Write a JSON value directly to buffer
fn push_json_value(buf: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => buf.push_str("null"),
        serde_json::Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => {
            use std::fmt::Write;
            let _ = write!(buf, "{}", n);
        }
        serde_json::Value::String(s) => {
            buf.push('"');
            push_json_escaped(buf, s);
            buf.push('"');
        }
        // Fallback for complex types
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            if let Ok(s) = serde_json::to_string(value) {
                buf.push_str(&s);
            }
        }
    }
}

/// Escape a string for JSON (handles \n, \r, \t, \", \\)
#[inline]
fn push_json_escaped(buf: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(buf, "\\u{:04x}", c as u32);
            }
            c => buf.push(c),
        }
    }
}
