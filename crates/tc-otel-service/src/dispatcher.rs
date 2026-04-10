//! Log dispatcher with batched async export to Victoria-Logs
//!
//! Logs are collected in a batch buffer and flushed either when the batch
//! is full or after a timeout - whichever comes first. This minimizes
//! HTTP overhead and CPU usage.

use anyhow::Result;
use std::time::Duration;
use tc_otel_core::{AppSettings, LogEntry, LogRecord, MessageFormatter};
use tokio::sync::{mpsc, watch};

/// Log dispatcher - converts LogEntries and sends them to a batched export worker
#[derive(Clone)]
pub struct LogDispatcher {
    export_tx: mpsc::Sender<LogRecord>,
}

impl LogDispatcher {
    pub async fn new(
        settings: &AppSettings,
        config_rx: Option<watch::Receiver<AppSettings>>,
    ) -> Result<Self> {
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
            config_rx,
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

    /// Background worker that batches records and flushes to endpoint.
    /// Supports hot-reload of export config (endpoint, batch_size, flush_interval)
    /// via an optional `watch::Receiver<AppSettings>`.
    async fn batch_worker(
        mut rx: mpsc::Receiver<LogRecord>,
        initial_endpoint: String,
        initial_batch_size: usize,
        initial_flush_interval: Duration,
        config_rx: Option<watch::Receiver<AppSettings>>,
    ) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut endpoint = initial_endpoint;
        let mut batch_size = initial_batch_size;
        let mut flush_interval = initial_flush_interval;
        let mut config_rx = config_rx;

        let mut batch: Vec<LogRecord> = Vec::with_capacity(batch_size);
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
                // Config change notification
                result = async {
                    match config_rx.as_mut() {
                        Some(crx) => crx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if result.is_err() {
                        // Watch channel closed, disable config reload
                        config_rx = None;
                        continue;
                    }
                    let new_export = config_rx.as_ref().unwrap().borrow().export.clone();
                    let new_endpoint = std::env::var("LOG4TC_EXPORT_ENDPOINT")
                        .unwrap_or(new_export.endpoint);
                    let new_batch_size = new_export.batch_size;
                    let new_flush_interval = Duration::from_millis(new_export.flush_interval_ms);

                    if new_endpoint != endpoint {
                        tracing::info!("Hot-reload: export endpoint changed to {}", new_endpoint);
                        endpoint = new_endpoint;
                    }
                    if new_batch_size != batch_size {
                        tracing::info!("Hot-reload: batch_size changed from {} to {}", batch_size, new_batch_size);
                        batch_size = new_batch_size;
                        batch.reserve(new_batch_size.saturating_sub(batch.capacity()));
                    }
                    if new_flush_interval != flush_interval {
                        tracing::info!("Hot-reload: flush_interval changed to {}ms", new_flush_interval.as_millis());
                        flush_interval = new_flush_interval;
                        interval = tokio::time::interval(flush_interval);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tc_otel_core::config::*;
    use tc_otel_core::LogLevel;
    use tokio::sync::watch;

    fn test_settings_with_endpoint(endpoint: &str) -> AppSettings {
        AppSettings {
            logging: LoggingConfig {
                log_level: "info".to_string(),
                format: LogFormat::Text,
                output_path: None,
            },
            receiver: ReceiverConfig::default(),
            export: ExportConfig {
                endpoint: endpoint.to_string(),
                batch_size: 2,
                flush_interval_ms: 50,
                ..Default::default()
            },
            outputs: vec![],
            service: ServiceConfig::default(),
            web: WebConfig::default(),
        }
    }

    fn make_test_entry() -> LogEntry {
        LogEntry::new(
            "test-source".to_string(),
            "test-host".to_string(),
            "test message".to_string(),
            "test.logger".to_string(),
            LogLevel::Info,
        )
    }

    /// Start a test HTTP server that counts POST requests.
    /// Returns (address, request_counter).
    async fn start_test_server() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        use axum::{routing::post, Router};

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();

        let app = Router::new().route(
            "/insert",
            post(move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    ""
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        (addr, counter)
    }

    #[tokio::test]
    async fn test_dispatcher_works_without_config_watch() {
        let (addr, counter) = start_test_server().await;
        let settings = test_settings_with_endpoint(&format!("http://{}/insert", addr));
        let dispatcher = LogDispatcher::new(&settings, None).await.unwrap();

        dispatcher.dispatch(make_test_entry()).await.unwrap();
        dispatcher.dispatch(make_test_entry()).await.unwrap();

        // Wait for batch flush (batch_size=2, should trigger)
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should have flushed at least once"
        );
    }

    #[tokio::test]
    async fn test_dispatcher_hot_reloads_endpoint() {
        let (addr1, counter1) = start_test_server().await;
        let (addr2, counter2) = start_test_server().await;

        let settings = test_settings_with_endpoint(&format!("http://{}/insert", addr1));
        let (tx, rx) = watch::channel(settings.clone());
        let dispatcher = LogDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Send records - should go to server 1
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter1.load(Ordering::SeqCst) > 0,
            "initial endpoint should receive"
        );
        assert_eq!(
            counter2.load(Ordering::SeqCst),
            0,
            "new endpoint should not receive yet"
        );

        // Hot-reload: change endpoint to server 2
        let mut new_settings = settings.clone();
        new_settings.export.endpoint = format!("http://{}/insert", addr2);
        tx.send(new_settings).unwrap();

        // Allow config propagation
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Send more records - should go to server 2
        let c2_before = counter2.load(Ordering::SeqCst);
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter2.load(Ordering::SeqCst) > c2_before,
            "new endpoint should receive after hot-reload"
        );
    }

    #[tokio::test]
    async fn test_dispatcher_hot_reloads_batch_size() {
        let (addr, counter) = start_test_server().await;
        let settings = test_settings_with_endpoint(&format!("http://{}/insert", addr));
        let (tx, rx) = watch::channel(settings.clone());
        let dispatcher = LogDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Initial batch_size is 2. Send 1 record - should NOT flush immediately
        dispatcher.dispatch(make_test_entry()).await.unwrap();

        // Wait for timer flush
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "timer should have flushed the single record"
        );

        // Hot-reload: change batch_size to 1
        let mut new_settings = settings.clone();
        new_settings.export.batch_size = 1;
        tx.send(new_settings).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Now send 1 record - should flush immediately (batch_size=1)
        let count_before = counter.load(Ordering::SeqCst);
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            counter.load(Ordering::SeqCst) > count_before,
            "should flush immediately with batch_size=1"
        );
    }

    #[tokio::test]
    async fn test_dispatcher_config_watch_channel_close_is_safe() {
        let (addr, counter) = start_test_server().await;
        let settings = test_settings_with_endpoint(&format!("http://{}/insert", addr));
        let (tx, rx) = watch::channel(settings.clone());
        let dispatcher = LogDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Drop the sender - should not crash the batch worker
        drop(tx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Dispatcher should still work
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        dispatcher.dispatch(make_test_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should still flush after watch channel closes"
        );
    }
}
