//! Log and metric dispatchers with batched async export
//!
//! Records are collected in a batch buffer and flushed either when the batch
//! is full or after a timeout - whichever comes first. This minimizes
//! HTTP overhead and CPU usage.

use anyhow::Result;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tc_otel_core::{
    AppSettings, LogEntry, LogRecord, MessageFormatter, MetricEntry, MetricMapper, MetricRecord,
};
use tc_otel_export::OtelExporter;
use tokio::sync::{mpsc, watch, Semaphore};

use crate::scope_resolver::ScopeResolver;

/// Maximum concurrent in-flight HTTP POSTs per dispatcher.
/// Each batch flush is dispatched as a detached `tokio::spawn`; this
/// semaphore caps how many can run in parallel so we don't melt the
/// backend or saturate the local socket pool. Set high enough that
/// a 100 k records/sec workload (≈ 50 POSTs/sec at batch_size=2000)
/// has headroom without queuing in the scheduler.
const MAX_INFLIGHT_FLUSHES: usize = 16;

/// Log dispatcher - converts LogEntries and sends them to a batched export worker
#[derive(Clone)]
pub struct LogDispatcher {
    export_tx: mpsc::Sender<LogRecord>,
    /// Local IPC hostname (from `gethostname`) — backfilled into every
    /// LogEntry whose `hostname` is empty before encoding. The PLC's
    /// AMS Net ID is *not* the OTel `host.name`; that is the host the
    /// data was collected from, i.e. this IPC.
    host_name: Arc<String>,
    /// Fallback `service.name` from `settings.service.name`, applied
    /// when an inbound LogEntry has no project_name set (early frames
    /// before the registration message arrives).
    default_service_name: Arc<String>,
    /// Resolves `entry.logger` → `LogRecord.scope_name` via the
    /// PLC's symbol table (Phase 1B will plug in the real ADS
    /// lookup; Phase 1A ships a no-op resolver that just passes the
    /// raw namespace through, so behaviour is unchanged until the
    /// lookup is wired). Service.rs invalidates per-net_id cache on
    /// every fresh registration.
    scope_resolver: ScopeResolver,
}

impl LogDispatcher {
    /// Convenience constructor that defaults the scope resolver to a
    /// no-op. Used by the existing dispatcher unit-tests; the
    /// service-layer wires `with_scope_resolver` instead so it can
    /// share one resolver across pillars in Phase 1B.
    #[allow(dead_code)]
    pub async fn new(
        settings: &AppSettings,
        config_rx: Option<watch::Receiver<AppSettings>>,
    ) -> Result<Self> {
        Self::with_scope_resolver(settings, config_rx, ScopeResolver::noop()).await
    }

    /// Build a dispatcher with an explicit `ScopeResolver`. Service.rs
    /// uses this overload to share one resolver instance with the
    /// span / metric paths and to invalidate the cache on each
    /// registration message — the default `LogDispatcher::new`
    /// constructs a no-op resolver suitable for tests.
    pub async fn with_scope_resolver(
        settings: &AppSettings,
        config_rx: Option<watch::Receiver<AppSettings>>,
        scope_resolver: ScopeResolver,
    ) -> Result<Self> {
        // ENV override for endpoint, otherwise use config
        let endpoint = std::env::var("TCOTEL_EXPORT_ENDPOINT")
            .unwrap_or_else(|_| settings.export.endpoint.clone());

        let batch_size = settings.export.batch_size;
        let flush_interval = Duration::from_millis(settings.export.flush_interval_ms);
        let format = settings.export.format;

        // Bounded channel for backpressure
        let (export_tx, export_rx) = mpsc::channel::<LogRecord>(settings.service.channel_capacity);

        // Spawn background batch worker
        tokio::spawn(Self::batch_worker(
            export_rx,
            endpoint,
            batch_size,
            flush_interval,
            format,
            config_rx,
        ));

        let host_name = Arc::new(local_host_name());
        let default_service_name = Arc::new(settings.service.name.clone());

        tracing::info!(
            "Dispatcher ready (batch={}, flush={}ms, format={:?}, host_name={}, default_service_name={})",
            batch_size,
            flush_interval.as_millis(),
            format,
            host_name,
            default_service_name,
        );

        Ok(Self {
            export_tx,
            host_name,
            default_service_name,
            scope_resolver,
        })
    }

    /// Read-only accessor used by the service layer to consult the same
    /// hostname when wiring the SpanDispatcher.
    pub fn host_name(&self) -> Arc<String> {
        self.host_name.clone()
    }

    /// Dispatch a log entry - formats and sends to export worker (non-blocking)
    pub async fn dispatch(&self, mut entry: LogEntry) -> Result<()> {
        // Backfill the local IPC hostname when emitter sites leave it
        // empty (e.g. AMS receivers no longer synthesise a `plc-<netid>`
        // value — that's the PLC identity, not where data was collected).
        if entry.hostname.is_empty() {
            entry.hostname = (*self.host_name).clone();
        }
        if entry.project_name.is_empty() {
            entry.project_name = (*self.default_service_name).clone();
        }

        // Format message only if template has placeholders
        let body = if entry.message.contains('{') {
            MessageFormatter::format_with_context(&entry.message, &entry.arguments, &entry.context)
        } else {
            entry.message.clone()
        };

        // Resolve `scope.name` from the PLC's symbol table. Cache-first;
        // only the first record per `(net_id, namespace)` per app
        // version touches ADS. The OCC observation drops the cache
        // for this net_id whenever the PLC's `OnlineChangeCnt` bumps,
        // so post-update records re-resolve cleanly. Net_id, OCC, and
        // app_port captured before `entry` moves into `from_log_entry`
        // (app_port = the PLC runtime port the resolver connects to).
        let net_id = entry.ams_net_id.clone();
        let occ = entry.online_change_count;
        let app_port = entry.ams_app_port;
        if !net_id.is_empty() {
            self.scope_resolver.observe_occ(&net_id, occ).await;
        }
        let mut record = LogRecord::from_log_entry(entry);
        if !net_id.is_empty() && !record.scope_name.is_empty() && app_port != 0 {
            let outcome = self
                .scope_resolver
                .resolve(&net_id, &record.scope_name, app_port)
                .await;
            // Resolver collapsed instance path → FB type. Ship the
            // concrete FB instance path it landed on as
            // `plc.instance_path` so Motor 1 vs Motor 2 stays
            // distinguishable when both bucket under
            // `scope.name=FB_Motor`. (We deliberately store the FB
            // instance, NOT the trailing FB_Log variable — the
            // caller cares about the owning FB, not the framework
            // wrap.) Raw fallback: no attribute, scope.name keeps
            // the user's literal namespace.
            if let Some(path) = outcome.instance_path {
                record.log_attributes.insert(
                    "plc.instance_path".to_string(),
                    serde_json::Value::String(path),
                );
            }
            record.scope_name = outcome.scope_name;
        }
        // Preserve the trace suffix that from_log_entry appended, if any,
        // so Grafana's derivedFields regex still finds trace_id in the body.
        let body = if !record.trace_id.is_empty() {
            format!(
                "{} [trace_id={} span_id={}]",
                body, record.trace_id, record.span_id
            )
        } else {
            body
        };
        record.body = serde_json::Value::String(body);

        // Non-blocking send - drops if channel full (backpressure)
        if self.export_tx.try_send(record).is_err() {
            tracing::warn!("Export channel full, dropping log");
        }

        Ok(())
    }

    /// True if the endpoint is an OTLP HTTP logs endpoint (OTel
    /// Collector, Tempo, VictoriaLogs' `/insert/opentelemetry/v1/logs`
    /// path, etc.).
    ///
    /// The default dist `config.json` ships this OTLP-Logs path so the
    /// service is spec-conformant out of the box. The legacy
    /// `/insert/jsonline` endpoint is still supported as a non-default
    /// VL-only fast path: VictoriaLogs ingests JSONL ~3× faster than
    /// OTLP-Logs because there's no per-record `ResourceLogs` /
    /// `ScopeLogs` wrapping overhead. Users who switch the endpoint
    /// to `/insert/jsonline` opt into that throughput at the cost of
    /// portability (the same stream cannot be re-pointed at a generic
    /// OTLP-Logs receiver without re-encoding).
    fn is_otlp_endpoint(endpoint: &str) -> bool {
        endpoint.contains("/v1/logs")
    }

    /// Background worker that batches records and flushes to endpoint.
    /// Supports hot-reload of export config (endpoint, batch_size, flush_interval, format)
    /// via an optional `watch::Receiver<AppSettings>`.
    async fn batch_worker(
        mut rx: mpsc::Receiver<LogRecord>,
        initial_endpoint: String,
        initial_batch_size: usize,
        initial_flush_interval: Duration,
        initial_format: tc_otel_core::WireFormat,
        config_rx: Option<watch::Receiver<AppSettings>>,
    ) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            // Keep one warm connection per concurrent in-flight POST
            // so the next batch doesn't pay a TCP/keep-alive setup
            // cost when the previous one releases its permit.
            .pool_max_idle_per_host(MAX_INFLIGHT_FLUSHES)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut endpoint = initial_endpoint;
        let mut batch_size = initial_batch_size;
        let mut flush_interval = initial_flush_interval;
        let mut format = initial_format;
        let mut config_rx = config_rx;

        let build_log_exporter =
            |endpoint: &str, batch_size: usize, format: tc_otel_core::WireFormat| -> OtelExporter {
                let cfg = tc_otel_export::exporter::ExportConfig {
                    endpoint: endpoint.to_string(),
                    batch_size,
                    format,
                    ..Default::default()
                };
                OtelExporter::with_config(cfg)
            };

        let mut otlp_exporter: Option<Arc<OtelExporter>> = if Self::is_otlp_endpoint(&endpoint) {
            Some(Arc::new(build_log_exporter(&endpoint, batch_size, format)))
        } else {
            None
        };

        let client = Arc::new(client);
        // Cap concurrent in-flight HTTP POSTs. Each batch flush is
        // `tokio::spawn`-ed so the select! receiver loop never blocks
        // on a slow backend — the producer side keeps draining the
        // mpsc channel even while POSTs are in flight, which is what
        // gets us 100k+ records/sec without backpressure drops.
        let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_FLUSHES));

        let mut batch: Vec<LogRecord> = Vec::with_capacity(batch_size);
        let mut interval = tokio::time::interval(flush_interval);

        loop {
            tokio::select! {
                // Receive new record
                Some(record) = rx.recv() => {
                    batch.push(record);
                    if batch.len() >= batch_size {
                        Self::spawn_flush(
                            client.clone(),
                            inflight.clone(),
                            endpoint.clone(),
                            otlp_exporter.clone(),
                            std::mem::replace(&mut batch, Vec::with_capacity(batch_size)),
                        );
                    }
                }
                // Periodic flush
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        Self::spawn_flush(
                            client.clone(),
                            inflight.clone(),
                            endpoint.clone(),
                            otlp_exporter.clone(),
                            std::mem::replace(&mut batch, Vec::with_capacity(batch_size)),
                        );
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
                    let new_endpoint = std::env::var("TCOTEL_EXPORT_ENDPOINT")
                        .unwrap_or(new_export.endpoint);
                    let new_batch_size = new_export.batch_size;
                    let new_flush_interval = Duration::from_millis(new_export.flush_interval_ms);
                    let new_format = new_export.format;

                    let exporter_dirty =
                        new_endpoint != endpoint || new_format != format;
                    if new_endpoint != endpoint {
                        tracing::info!("Hot-reload: export endpoint changed to {}", new_endpoint);
                        endpoint = new_endpoint;
                    }
                    if new_format != format {
                        tracing::info!(
                            "Hot-reload: export format changed from {:?} to {:?}",
                            format,
                            new_format
                        );
                        format = new_format;
                    }
                    if exporter_dirty {
                        otlp_exporter = if Self::is_otlp_endpoint(&endpoint) {
                            Some(Arc::new(build_log_exporter(&endpoint, batch_size, format)))
                        } else {
                            None
                        };
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
                    // Flush remaining inline before exit (no point spawning).
                    if !batch.is_empty() {
                        Self::spawn_flush(
                            client.clone(),
                            inflight.clone(),
                            endpoint.clone(),
                            otlp_exporter.clone(),
                            std::mem::take(&mut batch),
                        );
                    }
                    // Wait for in-flight flushes to finish (best-effort
                    // bounded by the semaphore — every permit eventually
                    // returns when its spawn completes).
                    let _ = inflight.acquire_many(MAX_INFLIGHT_FLUSHES as u32).await;
                    tracing::info!("Export worker stopped.");
                    break;
                }
            }
        }
    }

    /// Spawn a detached task that ships one batch with an inflight
    /// permit. **Bounded** — uses `try_acquire_owned()` so we never
    /// queue up more than `MAX_INFLIGHT_FLUSHES` outstanding tasks;
    /// when the backend is slow and the cap is reached we drop the
    /// batch and warn rather than accumulate `Vec<LogRecord>` in
    /// the scheduler. That keeps memory bounded and avoids the
    /// death-spiral failure mode where a slow VictoriaLogs would
    /// silently turn into runaway task spawning + Tokio overload.
    fn spawn_flush(
        client: Arc<reqwest::Client>,
        inflight: Arc<Semaphore>,
        endpoint: String,
        otlp_exporter: Option<Arc<OtelExporter>>,
        batch: Vec<LogRecord>,
    ) {
        let permit = match inflight.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(
                    "Log flush dropped: {} records — {} concurrent POSTs in flight, backend too slow",
                    batch.len(),
                    MAX_INFLIGHT_FLUSHES,
                );
                return;
            }
        };
        tokio::spawn(async move {
            if let Err(e) =
                Self::flush_batch_owned(&client, &endpoint, otlp_exporter.as_deref(), batch).await
            {
                tracing::error!("Batch export error: {}", e);
            }
            drop(permit);
        });
    }

    /// Flush a batch of records to the endpoint. Owned variant —
    /// runs entirely on its own `tokio::spawn` task so the dispatch
    /// receiver loop never awaits HTTP I/O.
    ///
    /// OTLP endpoints (`/v1/logs`) → delegate to `OtelExporter`.
    /// Everything else → VictoriaLogs JSONL with `application/stream+json`.
    async fn flush_batch_owned(
        client: &reqwest::Client,
        endpoint: &str,
        otlp_exporter: Option<&OtelExporter>,
        batch: Vec<LogRecord>,
    ) -> Result<usize> {
        if let Some(exporter) = otlp_exporter {
            let count = batch.len();
            exporter
                .export_batch(batch)
                .await
                .map_err(|e| anyhow::anyhow!("OTLP export failed: {}", e))?;
            return Ok(count);
        }
        let count = batch.len();
        let mut payload = String::with_capacity(count * 256);
        let payload = &mut payload;

        // Build JSONL directly without intermediate serde_json::Map
        for record in &batch {
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

        // Special sink: print to stdout instead of HTTP POST.
        if endpoint.eq_ignore_ascii_case("stdout") {
            print!("{}", payload);
            return Ok(count);
        }

        let body = std::mem::take(payload);
        let response = client
            .post(endpoint)
            .header("Content-Type", "application/stream+json")
            .body(body)
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

/// Metric dispatcher - converts MetricEntries, batches, and exports via OTLP HTTP
#[derive(Clone)]
pub struct MetricDispatcher {
    export_tx: mpsc::Sender<MetricRecord>,
    mapper: Arc<RwLock<MetricMapper>>,
    /// Fallback `service.name` applied when an inbound `MetricEntry` has
    /// no `project_name` set. Without this, registry-lookup misses (early
    /// PLC frames before the registration message arrives) emit metrics
    /// with `service.name=""` which downstream TSDBs drop, fragmenting
    /// the timeseries identity. Sourced from `settings.service.name`.
    default_service_name: Arc<String>,
    /// Local IPC hostname from `gethostname` — backfilled into every
    /// MetricEntry whose `hostname` is empty before encoding.
    host_name: Arc<String>,
    /// Shared with `LogDispatcher` and `TraceDispatcher` — same
    /// per-(net_id, namespace) cache, same OCC-driven invalidation.
    scope_resolver: ScopeResolver,
}

impl MetricDispatcher {
    pub async fn new(
        settings: &AppSettings,
        config_rx: Option<watch::Receiver<AppSettings>>,
    ) -> Result<Self> {
        Self::with_scope_resolver(settings, config_rx, ScopeResolver::noop()).await
    }

    /// Build a metric dispatcher with the process-wide ScopeResolver
    /// so user metrics (FB_Metrics) bucket by FB type the same way
    /// logs do.
    pub async fn with_scope_resolver(
        settings: &AppSettings,
        config_rx: Option<watch::Receiver<AppSettings>>,
        scope_resolver: ScopeResolver,
    ) -> Result<Self> {
        let initial_cfg = Self::build_export_config(settings);
        let flush_interval = Duration::from_millis(settings.metrics.export_flush_interval_ms);
        let default_service_name = Arc::new(settings.service.name.clone());
        let host_name = Arc::new(local_host_name());

        let (export_tx, export_rx) =
            mpsc::channel::<MetricRecord>(settings.service.channel_capacity);

        let mapper = Arc::new(RwLock::new(MetricMapper::from_config(&settings.metrics)));

        tokio::spawn(Self::batch_worker(
            export_rx,
            initial_cfg.clone(),
            flush_interval,
            config_rx,
            mapper.clone(),
        ));

        tracing::info!(
            "MetricDispatcher ready (batch={}, flush={}ms, format={:?}, default_service_name={}, host_name={}, custom_metrics={})",
            initial_cfg.batch_size,
            flush_interval.as_millis(),
            initial_cfg.format,
            default_service_name,
            host_name,
            mapper.read().unwrap().len()
        );

        Ok(Self {
            export_tx,
            mapper,
            default_service_name,
            host_name,
            scope_resolver,
        })
    }

    /// Resolve the metrics export endpoint from config.
    /// Uses metrics.export_endpoint if set, otherwise derives from the main
    /// export endpoint by replacing /v1/logs with /v1/metrics.
    fn resolve_endpoint(settings: &AppSettings) -> String {
        if let Some(ref ep) = settings.metrics.export_endpoint {
            return ep.clone();
        }
        // Derive from the main export endpoint
        settings
            .export
            .endpoint
            .replace("/v1/logs", "/v1/metrics")
            .replace("/insert/jsonline", "/v1/metrics")
    }

    /// Resolve the metrics wire format. `metrics.export_format` wins
    /// when explicitly set; otherwise inherits the global
    /// `export.format` (which defaults to `Json`).
    fn resolve_format(settings: &AppSettings) -> tc_otel_core::WireFormat {
        settings
            .metrics
            .export_format
            .unwrap_or(settings.export.format)
    }

    /// Build the runtime `tc_otel_export::ExportConfig` for the metrics
    /// pipeline, picking up endpoint + batch + format from app settings.
    fn build_export_config(settings: &AppSettings) -> tc_otel_export::exporter::ExportConfig {
        tc_otel_export::exporter::ExportConfig {
            endpoint: Self::resolve_endpoint(settings),
            batch_size: settings.metrics.export_batch_size,
            max_retries: settings.export.max_retries,
            timeout_secs: settings.export.timeout_secs,
            format: Self::resolve_format(settings),
            ..Default::default()
        }
    }

    /// Dispatch a metric entry - applies custom-metric mapping, converts to
    /// MetricRecord, and sends to the export worker.
    pub async fn dispatch(&self, mut entry: MetricEntry) -> Result<()> {
        self.mapper.read().unwrap().apply(&mut entry);
        // Backfill the global service.name when individual emitter sites
        // (e.g. diagnostics_bridge::with_task) leave it empty. Keeps every
        // metric record from this tc-otel instance under a single
        // `service.name` label so VictoriaMetrics doesn't fragment the
        // timeseries identity.
        if entry.project_name.is_empty() {
            entry.project_name = (*self.default_service_name).clone();
        }
        // Backfill the local IPC hostname so every record carries the
        // OTel `host.name` resource attribute.
        if entry.hostname.is_empty() {
            entry.hostname = (*self.host_name).clone();
        }
        // Capture before `entry` moves into `from_metric_entry` — used
        // by the ScopeResolver to pick the right PLC runtime.
        let net_id = entry.ams_net_id.clone();
        let app_port = entry.ams_app_port;
        let mut record = MetricRecord::from_metric_entry(entry);
        if !net_id.is_empty() && !record.scope_name.is_empty() && app_port != 0 {
            let outcome = self
                .scope_resolver
                .resolve(&net_id, &record.scope_name, app_port)
                .await;
            // Hit → bucket by FB type, attach the instance path so
            // two instances of the same type stay distinguishable.
            if let Some(path) = outcome.instance_path {
                record.attributes.insert(
                    "plc.instance_path".to_string(),
                    serde_json::Value::String(path),
                );
            }
            record.scope_name = outcome.scope_name;
        }

        if self.export_tx.try_send(record).is_err() {
            tracing::warn!("Metric export channel full, dropping metric");
        }

        Ok(())
    }

    /// Background worker that batches metric records and flushes to the OTLP endpoint.
    /// Mirrors `LogDispatcher::batch_worker` — each batch is dispatched
    /// as a `tokio::spawn`-ed task gated by an inflight semaphore so
    /// the receiver loop never blocks on an in-flight HTTP POST.
    async fn batch_worker(
        mut rx: mpsc::Receiver<MetricRecord>,
        initial_cfg: tc_otel_export::exporter::ExportConfig,
        initial_flush_interval: Duration,
        config_rx: Option<watch::Receiver<AppSettings>>,
        mapper: Arc<RwLock<MetricMapper>>,
    ) {
        let mut current_cfg = initial_cfg;
        let mut batch_size = current_cfg.batch_size;
        let mut flush_interval = initial_flush_interval;
        let mut config_rx = config_rx;
        let mut exporter = Arc::new(OtelExporter::with_config(current_cfg.clone()));
        let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_FLUSHES));

        let mut batch: Vec<MetricRecord> = Vec::with_capacity(batch_size);
        let mut interval = tokio::time::interval(flush_interval);

        loop {
            tokio::select! {
                Some(record) = rx.recv() => {
                    batch.push(record);
                    if batch.len() >= batch_size {
                        Self::spawn_metric_flush(
                            exporter.clone(),
                            inflight.clone(),
                            std::mem::replace(&mut batch, Vec::with_capacity(batch_size)),
                        );
                    }
                }
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        Self::spawn_metric_flush(
                            exporter.clone(),
                            inflight.clone(),
                            std::mem::replace(&mut batch, Vec::with_capacity(batch_size)),
                        );
                    }
                }
                result = async {
                    match config_rx.as_mut() {
                        Some(crx) => crx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if result.is_err() {
                        config_rx = None;
                        continue;
                    }
                    let new_settings = config_rx.as_ref().unwrap().borrow().clone();
                    let new_cfg = Self::build_export_config(&new_settings);
                    let new_flush_interval = Duration::from_millis(new_settings.metrics.export_flush_interval_ms);

                    let exporter_changed = new_cfg.endpoint != current_cfg.endpoint
                        || new_cfg.max_retries != current_cfg.max_retries
                        || new_cfg.timeout_secs != current_cfg.timeout_secs
                        || new_cfg.format != current_cfg.format
                        || new_cfg.batch_size != current_cfg.batch_size;
                    if exporter_changed {
                        tracing::info!(
                            "Hot-reload: metrics exporter rebuilt (endpoint={}, format={:?}, batch={}, max_retries={}, timeout_secs={})",
                            new_cfg.endpoint,
                            new_cfg.format,
                            new_cfg.batch_size,
                            new_cfg.max_retries,
                            new_cfg.timeout_secs,
                        );
                        current_cfg = new_cfg;
                        exporter = Arc::new(OtelExporter::with_config(current_cfg.clone()));
                    }
                    if current_cfg.batch_size != batch_size {
                        batch_size = current_cfg.batch_size;
                        batch.reserve(batch_size.saturating_sub(batch.capacity()));
                    }
                    if new_flush_interval != flush_interval {
                        tracing::info!("Hot-reload: metrics flush_interval changed to {}ms", new_flush_interval.as_millis());
                        flush_interval = new_flush_interval;
                        interval = tokio::time::interval(flush_interval);
                    }
                    let new_mapper = MetricMapper::from_config(&new_settings.metrics);
                    let len = new_mapper.len();
                    *mapper.write().unwrap() = new_mapper;
                    tracing::info!("Hot-reload: custom_metrics mapper rebuilt ({} symbols)", len);
                }
                else => {
                    if !batch.is_empty() {
                        Self::spawn_metric_flush(
                            exporter.clone(),
                            inflight.clone(),
                            std::mem::take(&mut batch),
                        );
                    }
                    let _ = inflight.acquire_many(MAX_INFLIGHT_FLUSHES as u32).await;
                    tracing::info!("Metric export worker stopped.");
                    break;
                }
            }
        }
    }

    /// Bounded spawn — see `LogDispatcher::spawn_flush` for rationale.
    fn spawn_metric_flush(
        exporter: Arc<OtelExporter>,
        inflight: Arc<Semaphore>,
        batch: Vec<MetricRecord>,
    ) {
        let permit = match inflight.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(
                    "Metric flush dropped: {} records — {} concurrent POSTs in flight",
                    batch.len(),
                    MAX_INFLIGHT_FLUSHES,
                );
                return;
            }
        };
        tokio::spawn(async move {
            if let Err(e) = exporter.export_metrics_batch(batch).await {
                tracing::error!("Metric batch export error: {}", e);
            }
            drop(permit);
        });
    }
}

/// Read the local IPC's hostname for the OTel `host.name` resource
/// attribute. Falls back to an empty string if the OS query fails — the
/// resource builder skips the attribute when empty rather than emitting
/// `host.name=""`.
fn local_host_name() -> String {
    gethostname::gethostname().into_string().unwrap_or_default()
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
            metrics: tc_otel_core::MetricsConfig::default(),
            diagnostics: tc_otel_core::DiagnosticsConfig::default(),
            traces: tc_otel_core::TracesConfig::default(),
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

    // ─── MetricDispatcher tests ──────────────────────────────────────

    fn test_settings_with_metrics(endpoint: &str) -> AppSettings {
        let mut settings = test_settings_with_endpoint(endpoint);
        settings.metrics.export_enabled = true;
        settings.metrics.export_endpoint = Some(endpoint.to_string());
        settings.metrics.export_batch_size = 2;
        settings.metrics.export_flush_interval_ms = 50;
        settings
    }

    fn make_test_metric_entry() -> MetricEntry {
        MetricEntry::gauge("test.metric".to_string(), 42.0)
    }

    /// Start a test HTTP server that accepts POST to /v1/metrics and counts requests.
    async fn start_metrics_test_server() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        use axum::{routing::post, Router};

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();

        let app = Router::new().route(
            "/v1/metrics",
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
    async fn test_metric_dispatcher_batches_and_flushes() {
        let (addr, counter) = start_metrics_test_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let dispatcher = MetricDispatcher::new(&settings, None).await.unwrap();

        // Send enough entries to trigger batch flush (batch_size=2)
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should have flushed metrics at least once"
        );
    }

    #[tokio::test]
    async fn test_metric_dispatcher_timer_flush() {
        let (addr, counter) = start_metrics_test_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let dispatcher = MetricDispatcher::new(&settings, None).await.unwrap();

        // Send 1 metric (below batch_size=2) — should still flush via timer
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "timer should flush even below batch size"
        );
    }

    #[tokio::test]
    async fn test_metric_dispatcher_endpoint_derivation() {
        // When no explicit metrics endpoint, derive from logs endpoint
        let mut settings = test_settings_with_endpoint("http://collector:4318/v1/logs");
        settings.metrics.export_enabled = true;
        settings.metrics.export_endpoint = None;
        settings.metrics.export_batch_size = 100;
        settings.metrics.export_flush_interval_ms = 50;

        // Verify endpoint derivation logic
        let resolved = MetricDispatcher::resolve_endpoint(&settings);
        assert_eq!(resolved, "http://collector:4318/v1/metrics");
    }

    #[tokio::test]
    async fn test_metric_dispatcher_endpoint_derivation_victoria() {
        // Victoria-logs style endpoint should also be derived
        let mut settings = test_settings_with_endpoint("http://victoria-logs:9428/insert/jsonline");
        settings.metrics.export_enabled = true;
        settings.metrics.export_endpoint = None;

        let resolved = MetricDispatcher::resolve_endpoint(&settings);
        assert_eq!(resolved, "http://victoria-logs:9428/v1/metrics");
    }

    #[tokio::test]
    async fn test_metric_dispatcher_explicit_endpoint() {
        let mut settings = test_settings_with_endpoint("http://collector:4318/v1/logs");
        settings.metrics.export_enabled = true;
        settings.metrics.export_endpoint =
            Some("http://prometheus-gateway:9091/v1/metrics".to_string());

        let resolved = MetricDispatcher::resolve_endpoint(&settings);
        assert_eq!(resolved, "http://prometheus-gateway:9091/v1/metrics");
    }

    #[tokio::test]
    async fn test_metric_dispatcher_converts_entry_to_record() {
        let (addr, counter) = start_metrics_test_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let dispatcher = MetricDispatcher::new(&settings, None).await.unwrap();

        // Dispatch a metric with PLC metadata
        let mut entry = MetricEntry::gauge("plc.temp".to_string(), 72.5);
        entry.project_name = "TestProject".to_string();
        entry.hostname = "plc-01".to_string();
        entry.unit = "Cel".to_string();

        dispatcher.dispatch(entry).await.unwrap();

        // Trigger timer flush
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should have exported the metric"
        );
    }

    #[tokio::test]
    async fn test_metric_dispatcher_config_watch_close_safe() {
        let (addr, counter) = start_metrics_test_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let (_tx, rx) = watch::channel(settings.clone());
        let dispatcher = MetricDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Drop the watch sender
        drop(_tx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Dispatcher should still work
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();
        dispatcher.dispatch(make_test_metric_entry()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            counter.load(Ordering::SeqCst) > 0,
            "should still flush after watch channel closes"
        );
    }

    /// Capture exported request bodies in a shared buffer.
    ///
    /// Uses a `Bytes` extractor (not `String`) because the default wire
    /// format is now OTLP-Protobuf — the body contains binary that is
    /// not valid UTF-8. Substring assertions still work via
    /// `String::from_utf8_lossy`: protobuf wire format encodes string
    /// fields (metric names, units, descriptions) as raw UTF-8 byte
    /// runs, so ASCII metric names appear verbatim in the byte stream.
    async fn start_capturing_metrics_server(
    ) -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
        use axum::{body::Bytes, routing::post, Router};

        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let b = bodies.clone();

        let app = Router::new().route(
            "/v1/metrics",
            post(move |body: Bytes| {
                let b = b.clone();
                async move {
                    b.lock().unwrap().push(body.to_vec());
                    ""
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        (addr, bodies)
    }

    #[tokio::test]
    async fn test_metric_dispatcher_applies_custom_metric_mapping() {
        let (addr, bodies) = start_capturing_metrics_server().await;
        let mut settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        settings.metrics.custom_metrics = vec![CustomMetricDef {
            symbol: "GVL.fMotorTemp".to_string(),
            metric_name: "plc.motor.temperature".to_string(),
            description: "Motor 1 bearing temperature".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        }];

        let dispatcher = MetricDispatcher::new(&settings, None).await.unwrap();

        let mut entry = MetricEntry::gauge("raw.plc.symbol".to_string(), 72.5);
        entry.attributes.insert(
            "plc.symbol".to_string(),
            serde_json::Value::String("GVL.fMotorTemp".to_string()),
        );
        dispatcher.dispatch(entry).await.unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        let captured_bytes: Vec<u8> = bodies.lock().unwrap().concat();
        let captured = String::from_utf8_lossy(&captured_bytes);
        assert!(
            captured.contains("plc.motor.temperature"),
            "exported body should contain mapped metric name; got: {captured:?}"
        );
        assert!(
            captured.contains("Cel"),
            "exported body should contain mapped unit; got: {captured:?}"
        );
        assert!(
            !captured.contains("raw.plc.symbol"),
            "exported body should not contain the pre-mapping name; got: {captured:?}"
        );
    }

    #[tokio::test]
    async fn test_metric_dispatcher_hot_reload_rebuilds_mapper() {
        let (addr, bodies) = start_capturing_metrics_server().await;
        let settings = test_settings_with_metrics(&format!("http://{}/v1/metrics", addr));
        let (tx, rx) = watch::channel(settings.clone());

        let dispatcher = MetricDispatcher::new(&settings, Some(rx)).await.unwrap();

        // Before reload: no mapping, entry goes through unchanged.
        let mut e1 = MetricEntry::gauge("initial.name".to_string(), 1.0);
        e1.attributes.insert(
            "plc.symbol".to_string(),
            serde_json::Value::String("GVL.nCount".to_string()),
        );
        dispatcher.dispatch(e1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Push a new config with a mapping.
        let mut updated = settings.clone();
        updated.metrics.custom_metrics = vec![CustomMetricDef {
            symbol: "GVL.nCount".to_string(),
            metric_name: "plc.parts.produced".to_string(),
            description: "Parts produced".to_string(),
            unit: "{parts}".to_string(),
            kind: MetricKindConfig::Sum,
            is_monotonic: true,
            ..CustomMetricDef::default()
        }];
        tx.send(updated).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        // After reload: same symbol now maps to new name/unit.
        let mut e2 = MetricEntry::gauge("initial.name".to_string(), 2.0);
        e2.attributes.insert(
            "plc.symbol".to_string(),
            serde_json::Value::String("GVL.nCount".to_string()),
        );
        dispatcher.dispatch(e2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let captured_bytes: Vec<u8> = bodies.lock().unwrap().concat();
        let captured = String::from_utf8_lossy(&captured_bytes);
        assert!(
            captured.contains("plc.parts.produced"),
            "body after hot-reload should contain remapped name; got: {captured:?}"
        );
        assert!(
            captured.contains("initial.name"),
            "body before hot-reload should still contain original (unmapped) name; got: {captured:?}"
        );
    }
}
