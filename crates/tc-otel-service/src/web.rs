//! Web UI server for status, diagnostics, and configuration
//!
//! Provides a REST API and embedded SPA dashboard for monitoring the tc-otel
//! service and browsing PLC symbols.

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use chrono;
use schemars;
use serde::Serialize;
#[allow(unused_imports)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tc_otel_ads::{AdsSymbolEntry, ConnectionManager, TaskRegistry};
use tc_otel_core::{AppSettings, WebConfig};

use crate::cycle_time::CycleTimeTracker;

/// Shared diagnostic counters for the service
#[derive(Debug)]
pub struct DiagnosticStats {
    pub start_time: Instant,
    pub logs_received: AtomicU64,
    pub logs_dispatched: AtomicU64,
    pub logs_failed: AtomicU64,
    pub connections_accepted: AtomicU64,
    pub connections_rejected: AtomicU64,
}

impl DiagnosticStats {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            logs_received: AtomicU64::new(0),
            logs_dispatched: AtomicU64::new(0),
            logs_failed: AtomicU64::new(0),
            connections_accepted: AtomicU64::new(0),
            connections_rejected: AtomicU64::new(0),
        }
    }
}

impl Default for DiagnosticStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe store for discovered PLC symbols
#[derive(Debug)]
pub struct SymbolStore {
    symbols: RwLock<Vec<AdsSymbolEntry>>,
}

impl SymbolStore {
    pub fn new() -> Self {
        Self {
            symbols: RwLock::new(Vec::new()),
        }
    }

    /// Replace all symbols with a new set (from a fresh discovery)
    pub fn update(&self, symbols: Vec<AdsSymbolEntry>) {
        *self.symbols.write().unwrap() = symbols;
    }

    /// Get all symbols
    pub fn list(&self) -> Vec<AdsSymbolEntry> {
        self.symbols.read().unwrap().clone()
    }

    /// Look up a symbol by exact name (case-insensitive)
    pub fn find_by_name(&self, name: &str) -> Option<AdsSymbolEntry> {
        let lower = name.to_lowercase();
        self.symbols
            .read()
            .unwrap()
            .iter()
            .find(|s| s.name.to_lowercase() == lower)
            .cloned()
    }

    /// Get the number of stored symbols
    pub fn count(&self) -> usize {
        self.symbols.read().unwrap().len()
    }
}

impl Default for SymbolStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared application state for axum handlers
#[derive(Clone)]
pub struct WebState {
    pub stats: Arc<DiagnosticStats>,
    pub conn_manager: Arc<ConnectionManager>,
    pub task_registry: Arc<TaskRegistry>,
    pub symbols: Arc<SymbolStore>,
    pub cycle_tracker: Arc<CycleTimeTracker>,
    pub service_name: String,
    pub config_path: Arc<std::path::PathBuf>,
    pub current_settings: Arc<RwLock<tc_otel_core::AppSettings>>,
    pub restart_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Handle to the active-client bridge (poll + notification sources).
    /// `None` when the `client-bridge` feature is disabled or no bridge was
    /// spawned. Endpoints under `/api/client/*` short-circuit to 503 when
    /// `None`.
    #[cfg(feature = "client-bridge")]
    pub client_bridge: Option<crate::client_bridge::ClientBridge>,
}

// --- API response types ---

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: String,
    uptime_secs: u64,
}

#[derive(Serialize)]
struct StatusResponse {
    service: String,
    status: &'static str,
    uptime_secs: u64,
    logs_received: u64,
    logs_dispatched: u64,
    logs_failed: u64,
    connections_active: usize,
    connections_accepted: u64,
    connections_rejected: u64,
    registered_tasks: usize,
}

#[derive(Serialize)]
struct ConnectionInfo {
    ip: String,
    count: usize,
}

#[derive(Serialize)]
struct TaskInfo {
    ams_net_id: String,
    ams_source_port: u16,
    task_index: u8,
    task_name: String,
    app_name: String,
    project_name: String,
    online_change_count: u32,
}

#[derive(Serialize)]
struct SymbolInfo {
    name: String,
    type_name: String,
    size: u32,
    index_group: u32,
    index_offset: u32,
    comment: String,
}

impl From<AdsSymbolEntry> for SymbolInfo {
    fn from(s: AdsSymbolEntry) -> Self {
        SymbolInfo {
            name: s.name,
            type_name: s.type_name,
            size: s.size,
            index_group: s.index_group,
            index_offset: s.index_offset,
            comment: s.comment,
        }
    }
}

#[derive(Serialize)]
struct SymbolsResponse {
    count: usize,
    symbols: Vec<SymbolInfo>,
}

// --- Handlers ---

async fn health(State(state): State<WebState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: state.service_name.clone(),
        uptime_secs: state.stats.start_time.elapsed().as_secs(),
    })
}

async fn status(State(state): State<WebState>) -> Json<StatusResponse> {
    Json(StatusResponse {
        service: state.service_name.clone(),
        status: "running",
        uptime_secs: state.stats.start_time.elapsed().as_secs(),
        logs_received: state.stats.logs_received.load(Ordering::Relaxed),
        logs_dispatched: state.stats.logs_dispatched.load(Ordering::Relaxed),
        logs_failed: state.stats.logs_failed.load(Ordering::Relaxed),
        connections_active: state.conn_manager.active_connections(),
        connections_accepted: state.stats.connections_accepted.load(Ordering::Relaxed),
        connections_rejected: state.stats.connections_rejected.load(Ordering::Relaxed),
        registered_tasks: state.task_registry.len(),
    })
}

async fn connections(State(state): State<WebState>) -> Json<Vec<ConnectionInfo>> {
    let ips = state.conn_manager.connected_ips();
    Json(
        ips.into_iter()
            .map(|(ip, count)| ConnectionInfo {
                ip: ip.to_string(),
                count,
            })
            .collect(),
    )
}

async fn tasks(State(state): State<WebState>) -> Json<Vec<TaskInfo>> {
    let all = state.task_registry.all_tasks();
    Json(
        all.into_iter()
            .map(|(key, meta)| TaskInfo {
                ams_net_id: key.ams_net_id,
                ams_source_port: key.ams_source_port,
                task_index: key.task_index,
                task_name: meta.task_name,
                app_name: meta.app_name,
                project_name: meta.project_name,
                online_change_count: meta.online_change_count,
            })
            .collect(),
    )
}

async fn get_symbols(State(state): State<WebState>) -> Json<SymbolsResponse> {
    let symbols: Vec<SymbolInfo> = state.symbols.list().into_iter().map(Into::into).collect();
    Json(SymbolsResponse {
        count: symbols.len(),
        symbols,
    })
}

async fn get_symbol_by_name(
    State(state): State<WebState>,
    Path(name): Path<String>,
) -> Result<Json<SymbolInfo>, StatusCode> {
    state
        .symbols
        .find_by_name(&name)
        .map(|s| Json(s.into()))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn cycle_metrics(
    State(state): State<WebState>,
) -> Json<Vec<crate::cycle_time::CycleTimeStats>> {
    Json(state.cycle_tracker.all_stats())
}

async fn dashboard() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(DASHBOARD_HTML),
    )
}

#[derive(Serialize)]
struct GetConfigResponse {
    config: serde_json::Value,
    restart_pending: bool,
    last_modified: Option<String>,
}

async fn get_config(State(state): State<WebState>) -> Json<GetConfigResponse> {
    let current = state.current_settings.read().unwrap();
    let config_value = current.to_masked_json();

    let last_modified = std::fs::metadata(state.config_path.as_ref())
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| {
                chrono::DateTime::<chrono::Utc>::from(std::time::UNIX_EPOCH + d).to_rfc3339()
            })
        });

    let restart_pending = state.restart_pending.load(Ordering::SeqCst);

    Json(GetConfigResponse {
        config: config_value,
        restart_pending,
        last_modified,
    })
}

#[derive(Serialize)]
struct PostConfigResponse {
    ok: bool,
    hot_reloaded: Vec<String>,
    restart_required: Vec<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

async fn post_config(
    State(state): State<WebState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<PostConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut incoming: AppSettings = serde_json::from_value(payload.clone()).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid config JSON".to_string(),
                detail: Some(e.to_string()),
            }),
        )
    })?;

    let current = state.current_settings.read().unwrap().clone();

    // Merge secrets from current config (restore masked values)
    incoming.merge_secrets_from(&current);

    // Validate the merged config
    if let Err(errors) = incoming.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Config validation failed".to_string(),
                detail: Some(errors.join("; ")),
            }),
        ));
    }

    let validated = incoming;

    // Write to temporary file then atomically rename
    let config_path = state.config_path.as_ref();
    let tmp_path = {
        let parent = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let file_name = config_path.file_name().unwrap_or_default();
        let tmp_name = format!("{}.tmp", file_name.to_string_lossy());
        parent.join(tmp_name)
    };

    let json_str = serde_json::to_string_pretty(&validated).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to serialize config".to_string(),
                detail: Some(e.to_string()),
            }),
        )
    })?;

    std::fs::write(&tmp_path, json_str).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to write config file".to_string(),
                detail: Some(e.to_string()),
            }),
        )
    })?;

    std::fs::rename(&tmp_path, config_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to atomically replace config".to_string(),
                detail: Some(e.to_string()),
            }),
        )
    })?;

    // Compute diff to determine which components need reload/restart
    let diff = tc_otel_core::ConfigDiff::compute(&current, &validated);

    // Update current settings immediately to avoid stale reads
    *state.current_settings.write().unwrap() = validated.clone();

    // Build response arrays from diff flags
    let mut hot_reloaded = vec![];
    let mut restart_required = vec![];

    if diff.logging_changed {
        hot_reloaded.push("logging".to_string());
    }
    if diff.export_changed {
        hot_reloaded.push("export".to_string());
    }
    if diff.metrics_changed {
        hot_reloaded.push("metrics".to_string());
    }
    if diff.diagnostics_changed {
        hot_reloaded.push("diagnostics".to_string());
    }

    if diff.receiver_changed {
        restart_required.push("receiver".to_string());
    }
    if diff.service_changed {
        restart_required.push("service".to_string());
    }
    if diff.outputs_changed {
        restart_required.push("outputs".to_string());
    }
    if diff.web_changed {
        restart_required.push("web".to_string());
    }

    if !restart_required.is_empty() {
        state.restart_pending.store(true, Ordering::SeqCst);
    }

    Ok(Json(PostConfigResponse {
        ok: true,
        hot_reloaded,
        restart_required,
    }))
}

async fn get_config_schema() -> Json<serde_json::Value> {
    let schema = schemars::schema_for!(AppSettings);
    Json(serde_json::to_value(schema).unwrap_or(serde_json::json!({})))
}

/// Build the axum router for the web UI
pub fn router(state: WebState) -> Router {
    let router = Router::new()
        .route("/", get(dashboard))
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/api/connections", get(connections))
        .route("/api/tasks", get(tasks))
        .route("/api/symbols", get(get_symbols))
        .route("/api/symbols/:name", get(get_symbol_by_name))
        .route("/api/cycle-metrics", get(cycle_metrics))
        .route("/api/config", get(get_config))
        .route("/api/config", post(post_config))
        .route("/api/config/schema", get(get_config_schema));
    let router = attach_client_routes(router);
    router.with_state(state)
}

/// Routes for the active-client bridge (poll + notification symbols).
///
/// These exist unconditionally — the handlers return 503 when the
/// `client-bridge` feature is disabled or no bridge was attached to the
/// state. UI code can therefore render a "disabled" banner based on the
/// HTTP response rather than needing a separate build.
fn attach_client_routes(router: Router<WebState>) -> Router<WebState> {
    router
        .route("/api/client/symbols", get(client_get_symbols))
        .route("/api/client/symbols/refresh", post(client_refresh_symbols))
        .route("/api/client/targets", get(client_list_targets))
}

#[cfg(feature = "client-bridge")]
#[derive(Serialize)]
struct ClientSymbolsResponse {
    target: String,
    fetched_at: Option<String>,
    count: usize,
    symbols: Vec<ClientSymbolNode>,
}

#[cfg(feature = "client-bridge")]
#[derive(Serialize)]
struct ClientSymbolNode {
    name: String,
    type_name: String,
    igroup: u32,
    ioffset: u32,
    size: u32,
}

#[derive(serde::Deserialize)]
#[cfg_attr(not(feature = "client-bridge"), allow(dead_code))]
struct SymbolsQuery {
    target: Option<String>,
    filter: Option<String>,
}

#[cfg(feature = "client-bridge")]
async fn client_get_symbols(
    State(state): State<WebState>,
    axum::extract::Query(q): axum::extract::Query<SymbolsQuery>,
) -> Result<Json<ClientSymbolsResponse>, (StatusCode, String)> {
    let bridge = state.client_bridge.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "client-bridge not running".into(),
    ))?;
    let target_str = q.target.ok_or((
        StatusCode::BAD_REQUEST,
        "missing 'target' query param".into(),
    ))?;
    let netid = parse_net_id(&target_str).ok_or((
        StatusCode::BAD_REQUEST,
        format!("invalid AMS Net ID: {target_str}"),
    ))?;
    let key = tc_otel_client::cache::TargetKey(netid);
    let cache = bridge.cache();
    let tree = cache.get(key).ok_or((
        StatusCode::NOT_FOUND,
        format!("no cached symbols for target {target_str}"),
    ))?;
    let fetched_at = cache.fetched_at(key).map(|t| t.to_rfc3339());
    let filter = q.filter.unwrap_or_default();
    let filtered: Vec<ClientSymbolNode> = if filter.is_empty() {
        tree.nodes
            .iter()
            .map(|n| ClientSymbolNode {
                name: n.name.clone(),
                type_name: n.type_name.clone(),
                igroup: n.igroup,
                ioffset: n.ioffset,
                size: n.size,
            })
            .collect()
    } else {
        tree.iter_prefix(&filter)
            .map(|n| ClientSymbolNode {
                name: n.name.clone(),
                type_name: n.type_name.clone(),
                igroup: n.igroup,
                ioffset: n.ioffset,
                size: n.size,
            })
            .collect()
    };
    Ok(Json(ClientSymbolsResponse {
        target: target_str,
        fetched_at,
        count: filtered.len(),
        symbols: filtered,
    }))
}

#[cfg(not(feature = "client-bridge"))]
async fn client_get_symbols(
    State(_state): State<WebState>,
    axum::extract::Query(_q): axum::extract::Query<SymbolsQuery>,
) -> (StatusCode, &'static str) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "client-bridge feature is not compiled in",
    )
}

#[derive(serde::Deserialize)]
#[cfg_attr(not(feature = "client-bridge"), allow(dead_code))]
struct RefreshQuery {
    target: String,
}

#[cfg(feature = "client-bridge")]
#[derive(Serialize)]
struct RefreshResponse {
    target: String,
    invalidated: bool,
}

#[cfg(feature = "client-bridge")]
async fn client_refresh_symbols(
    State(state): State<WebState>,
    axum::extract::Query(q): axum::extract::Query<RefreshQuery>,
) -> Result<Json<RefreshResponse>, (StatusCode, String)> {
    let bridge = state.client_bridge.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "client-bridge not running".into(),
    ))?;
    let netid = parse_net_id(&q.target).ok_or((
        StatusCode::BAD_REQUEST,
        format!("invalid AMS Net ID: {}", q.target),
    ))?;
    let key = tc_otel_client::cache::TargetKey(netid);
    // Invalidate. Next reconcile or an explicit rebuild will repopulate.
    let cache = bridge.cache();
    let had = cache.get(key).is_some();
    cache.invalidate(key);
    Ok(Json(RefreshResponse {
        target: q.target,
        invalidated: had,
    }))
}

#[cfg(not(feature = "client-bridge"))]
async fn client_refresh_symbols(
    State(_state): State<WebState>,
    axum::extract::Query(_q): axum::extract::Query<RefreshQuery>,
) -> (StatusCode, &'static str) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "client-bridge feature is not compiled in",
    )
}

#[cfg(feature = "client-bridge")]
#[derive(Serialize)]
struct ClientTargetInfo {
    ams_net_id: String,
    cached: bool,
    symbol_count: usize,
    fetched_at: Option<String>,
}

#[cfg(feature = "client-bridge")]
async fn client_list_targets(
    State(state): State<WebState>,
) -> Result<Json<Vec<ClientTargetInfo>>, (StatusCode, String)> {
    let bridge = state.client_bridge.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "client-bridge not running".into(),
    ))?;
    // Enumerate targets from the config (ground truth for "what's desired")
    // rather than the cache (which only shows what's been fetched).
    let settings = state.current_settings.read().unwrap();
    use std::collections::HashSet;
    let mut desired: HashSet<String> = HashSet::new();
    for def in &settings.metrics.custom_metrics {
        if matches!(def.source, tc_otel_core::config::CustomMetricSource::Push) {
            continue;
        }
        if let Some(id) = def.ams_net_id.as_deref() {
            desired.insert(id.to_string());
        }
    }
    let cache = bridge.cache();
    let mut out: Vec<ClientTargetInfo> = desired
        .into_iter()
        .map(|id| {
            let key = parse_net_id(&id).map(tc_otel_client::cache::TargetKey);
            let (cached, count, ts) = match key.and_then(|k| cache.get(k).map(|t| (k, t))) {
                Some((k, t)) => (true, t.len(), cache.fetched_at(k).map(|x| x.to_rfc3339())),
                None => (false, 0, None),
            };
            ClientTargetInfo {
                ams_net_id: id,
                cached,
                symbol_count: count,
                fetched_at: ts,
            }
        })
        .collect();
    out.sort_by(|a, b| a.ams_net_id.cmp(&b.ams_net_id));
    Ok(Json(out))
}

#[cfg(not(feature = "client-bridge"))]
async fn client_list_targets(State(_state): State<WebState>) -> (StatusCode, &'static str) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "client-bridge feature is not compiled in",
    )
}

#[cfg(feature = "client-bridge")]
fn parse_net_id(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<_> = s.split('.').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p.parse().ok()?;
    }
    Some(out)
}

/// Start the web server. Returns when shutdown signal is received.
pub async fn start_web_server(
    config: &WebConfig,
    state: WebState,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let app = router(state);
    let addr: std::net::SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Web UI listening on http://{}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
            tracing::info!("Web UI server shutting down");
        })
        .await?;

    Ok(())
}

// --- Embedded SPA Dashboard ---

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>tc-otel Dashboard</title>
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:system-ui,-apple-system,sans-serif;background:#0f172a;color:#e2e8f0;line-height:1.6}
.container{max-width:1200px;margin:0 auto;padding:1rem}
h1{font-size:1.5rem;font-weight:600;margin-bottom:1rem;color:#38bdf8}
h2{font-size:1.1rem;font-weight:500;margin-bottom:.75rem;color:#94a3b8}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(240px,1fr));gap:1rem;margin-bottom:1.5rem}
.card{background:#1e293b;border-radius:.5rem;padding:1rem;border:1px solid #334155}
.card .label{font-size:.75rem;color:#64748b;text-transform:uppercase;letter-spacing:.05em}
.card .value{font-size:1.75rem;font-weight:700;color:#f1f5f9}
.card .value.ok{color:#4ade80}
.card .value.warn{color:#fbbf24}
table{width:100%;border-collapse:collapse;margin-bottom:1rem}
th,td{text-align:left;padding:.5rem .75rem;border-bottom:1px solid #334155}
th{color:#94a3b8;font-weight:500;font-size:.8rem;text-transform:uppercase}
td{font-size:.9rem}
.section{margin-bottom:2rem}
.btn{padding:.5rem 1rem;background:#2563eb;color:#fff;border:none;border-radius:.25rem;cursor:pointer;font-size:.9rem}
.btn:hover{background:#1d4ed8}
.status-bar{display:flex;justify-content:space-between;align-items:center;margin-bottom:1rem;color:#64748b;font-size:.8rem}
#error{color:#ef4444;margin-bottom:1rem;display:none}
</style>
</head>
<body>
<div class="container">
<h1>tc-otel Dashboard</h1>
<nav id="topnav" style="display:flex;gap:.5rem;margin-bottom:1rem"><a href="#/" class="btn" id="nav-dash">Dashboard</a><a href="#/config" class="btn" id="nav-cfg">Config</a><a href="#/symbols" class="btn" id="nav-sym">Symbols</a></nav>
<div id="error"></div>

<div id="dashboard-view">
<div class="status-bar"><span id="last-update">Loading...</span><span id="uptime"></span></div>

<div class="grid" id="stats"></div>

<div class="section">
<h2>Active Connections</h2>
<table><thead><tr><th>IP Address</th><th>Connections</th></tr></thead><tbody id="conn-body"></tbody></table>
</div>

<div class="section">
<h2>Registered PLC Tasks</h2>
<table><thead><tr><th>AMS Net ID</th><th>Port</th><th>Task</th><th>Application</th><th>Project</th><th>Changes</th></tr></thead><tbody id="task-body"></tbody></table>
</div>

<div class="section">
<h2>Task Cycle Time</h2>
<table><thead><tr><th>AMS Net ID</th><th>Task</th><th>Avg (&mu;s)</th><th>Min (&mu;s)</th><th>Max (&mu;s)</th><th>Jitter (&mu;s)</th><th>Samples</th><th>Total Cycles</th></tr></thead><tbody id="cycle-body"></tbody></table>
</div>
</div>

<section id="config-view" style="display:none">
<h2>Configuration</h2>
<div id="config-form-root"></div>
<button id="config-save-btn" class="btn" onclick="saveConfig()">Speichern</button>
<div id="config-toast" class="toast" style="display:none;background:#1e293b;border:1px solid #334155;border-radius:.5rem;padding:1rem;margin-top:1rem"></div>
</section>

<section id="symbols-view" style="display:none">
<h2>PLC Symbol Browser</h2>
<div id="sym-banner" style="color:#fbbf24;background:#422006;border:1px solid #ca8a04;border-radius:.25rem;padding:.5rem;margin-bottom:1rem;display:none"></div>
<div class="section">
  <h2 style="font-size:.9rem">Configured Targets</h2>
  <table style="margin-bottom:1rem"><thead><tr><th>AMS Net ID</th><th>Cached</th><th>Symbols</th><th>Fetched At</th><th></th></tr></thead><tbody id="sym-targets"></tbody></table>
</div>
<div class="section">
  <h2 style="font-size:.9rem">Symbols for <span id="sym-selected-target" style="color:#38bdf8">—</span></h2>
  <div style="display:flex;gap:.5rem;margin-bottom:.5rem;align-items:center">
    <input id="sym-filter" type="text" placeholder="filter by prefix (e.g. MAIN.)" style="flex:1;padding:.4rem .6rem;background:#0f172a;border:1px solid #475569;border-radius:.25rem;color:#e2e8f0">
    <button class="btn" onclick="symRefreshCurrent()">Refresh</button>
  </div>
  <div id="sym-stats" style="font-size:.8rem;color:#64748b;margin-bottom:.5rem"></div>
  <div style="max-height:60vh;overflow-y:auto">
    <table><thead><tr><th>Symbol</th><th>Type</th><th>Size</th><th>IG</th><th>IO</th><th></th></tr></thead><tbody id="sym-rows"></tbody></table>
  </div>
</div>
</section>

</div>
<style id="cfg-style">
.cfg-section{background:#1e293b;border:1px solid #334155;border-radius:.5rem;margin-bottom:1rem;overflow:hidden}
.cfg-section>summary{padding:.75rem 1rem;cursor:pointer;font-weight:600;color:#38bdf8;user-select:none;background:#0f172a}
.cfg-section>summary:hover{background:#1e293b}
.cfg-section[open]>summary{border-bottom:1px solid #334155}
.cfg-body{padding:1rem}
.cfg-field{margin-bottom:.75rem}
.cfg-field>label{display:block;font-size:.85rem;color:#cbd5e1;margin-bottom:.25rem}
.cfg-field>.hint{font-size:.75rem;color:#64748b;margin-bottom:.25rem}
.cfg-field input[type=text],.cfg-field input[type=number],.cfg-field input[type=password],.cfg-field select{width:100%;padding:.4rem .6rem;background:#0f172a;border:1px solid #475569;border-radius:.25rem;color:#e2e8f0;font-size:.9rem;font-family:inherit}
.cfg-field input:focus,.cfg-field select:focus{outline:none;border-color:#38bdf8}
.cfg-field input[type=checkbox]{width:auto;margin-right:.5rem}
.cfg-array{border-left:2px solid #334155;padding-left:.75rem}
.cfg-array-item{background:#0f172a;border:1px solid #334155;border-radius:.25rem;padding:.5rem;margin-bottom:.5rem;position:relative}
.cfg-rm{position:absolute;top:.25rem;right:.25rem;padding:.1rem .5rem;background:#dc2626;color:#fff;border:none;border-radius:.25rem;cursor:pointer;font-size:.75rem}
.cfg-add{padding:.3rem .8rem;background:#16a34a;color:#fff;border:none;border-radius:.25rem;cursor:pointer;font-size:.8rem;margin-top:.25rem}
.cfg-union-tabs{display:flex;gap:.25rem;margin-bottom:.5rem;border-bottom:1px solid #334155}
.cfg-union-tab{padding:.3rem .7rem;background:transparent;border:none;border-bottom:2px solid transparent;color:#94a3b8;cursor:pointer;font-size:.85rem}
.cfg-union-tab.active{color:#38bdf8;border-bottom-color:#38bdf8}
.cfg-union-panel{display:none}.cfg-union-panel.active{display:block}
.toast.ok{background:#052e16;border-color:#16a34a;color:#bbf7d0}
.toast.warn{background:#422006;border-color:#ca8a04;color:#fde68a}
.toast.err{background:#450a0a;border-color:#dc2626;color:#fecaca}
</style>
<script>
(function(){
  const root=document.getElementById('config-form-root');
  const toast=document.getElementById('config-toast');
  const saveBtn=document.getElementById('config-save-btn');
  let rootSchema=null, currentData=null;
  const MASKED='***MASKED***';

  function resolveRef(ref){if(!ref||!ref.startsWith('#/'))return null;const parts=ref.slice(2).split('/');let c=rootSchema;for(const p of parts){if(!c||typeof c!=='object')return null;c=c[p]}return c}
  function resolve(s){
    if(!s||typeof s!=='object')return s;
    // Direct $ref
    if(s.$ref){const r=resolveRef(s.$ref);return r?resolve(Object.assign({},r,Object.fromEntries(Object.entries(s).filter(([k])=>k!=='$ref')))):s}
    // allOf wrapper (schemars emits this when a field has a default)
    if(Array.isArray(s.allOf)&&s.allOf.length>0){
      let merged=Object.fromEntries(Object.entries(s).filter(([k])=>k!=='allOf'));
      for(const part of s.allOf){const r=resolve(part);if(r&&typeof r==='object')merged=Object.assign({},r,merged)}
      return merged;
    }
    return s;
  }

  function titleOf(schema,key){return schema.title||(key?key.replace(/_/g,' ').replace(/\b\w/g,c=>c.toUpperCase()):'')}

  function renderField(schema,value,path,key){
    schema=resolve(schema);
    const desc=schema.description?`<div class="hint">${escape(schema.description)}</div>`:'';
    const lbl=key!=null?`<label>${escape(titleOf(schema,key))}</label>`:'';

    if(schema.enum){
      const opts=schema.enum.map(e=>`<option value="${escape(e)}"${e===value?' selected':''}>${escape(e)}</option>`).join('');
      return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<select data-kind="enum">${opts}</select></div>`;
    }
    if(schema.type==='boolean'){
      return `<div class="cfg-field" data-path="${path}">${desc}<label><input type="checkbox" data-kind="bool"${value?' checked':''}> ${escape(titleOf(schema,key))}</label></div>`;
    }
    if(schema.type==='integer'||schema.type==='number'){
      const min=schema.minimum!=null?` min="${schema.minimum}"`:'';
      const max=schema.maximum!=null?` max="${schema.maximum}"`:'';
      const step=schema.type==='integer'?' step="1"':'';
      const v=value!=null?value:'';
      return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="number" data-kind="num"${min}${max}${step} value="${escape(String(v))}"></div>`;
    }
    if(schema.type==='string'&&schema.format==='password'){
      return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="password" data-kind="pw" placeholder="${value===MASKED?'unchanged':''}" data-orig="${value===MASKED?'1':'0'}"></div>`;
    }
    if(schema.type==='string'){
      const v=value!=null?value:'';
      return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="text" data-kind="str" value="${escape(String(v))}"></div>`;
    }
    if(schema.type==='array'){
      return renderArray(schema,value||[],path,key);
    }
    if(schema.type==='object'||schema.properties){
      return renderObject(schema,value||{},path,key);
    }
    if(schema.oneOf||schema.anyOf){
      return renderUnion(schema,value,path,key);
    }
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="text" data-kind="json" value="${escape(JSON.stringify(value||null))}"></div>`;
  }

  function renderObject(schema,value,path,key){
    const props=schema.properties||{};
    let body='';
    for(const [pk,ps] of Object.entries(props)){
      body+=renderField(ps,value?value[pk]:undefined,path?`${path}.${pk}`:pk,pk);
    }
    if(!path){return body}
    if(key==null){return `<div data-obj="${path}">${body}</div>`}
    return `<div class="cfg-field" data-path="${path}" data-obj="1"><details class="cfg-section" open><summary>${escape(titleOf(schema,key))}</summary><div class="cfg-body">${body}</div></details></div>`;
  }

  function renderArray(schema,items,path,key){
    const itemSchema=resolve(schema.items||{});
    const id='arr-'+path.replace(/[^a-z0-9]/gi,'_');
    const inner=items.map((it,i)=>`<div class="cfg-array-item" data-idx="${i}"><button type="button" class="cfg-rm">×</button>${renderField(itemSchema,it,`${path}[${i}]`,null)}</div>`).join('');
    return `<div class="cfg-field" data-path="${path}" data-arr="1"><label>${escape(titleOf(schema,key))}</label><div class="cfg-array" id="${id}" data-item-schema='${escape(JSON.stringify(itemSchema))}'>${inner}</div><button type="button" class="cfg-add" data-target="${id}" data-path="${path}">+ Add</button></div>`;
  }

  function renderUnion(schema,value,path,key){
    const variants=(schema.oneOf||schema.anyOf).map(resolve);
    // Plain-string-enum union (e.g. schemars emits #[serde(rename_all)] enums
    // as oneOf of {enum:[lit], type:"string"}). Render as a <select>.
    const allStringLit=variants.every(v=>v&&v.type==='string'&&Array.isArray(v.enum)&&v.enum.length===1&&!v.properties);
    if(allStringLit){
      const lits=variants.map(v=>v.enum[0]);
      const opts=lits.map(lit=>`<option value="${escape(lit)}"${lit===value?' selected':''}>${escape(lit)}</option>`).join('');
      const desc=schema.description?`<div class="hint">${escape(schema.description)}</div>`:'';
      return `<div class="cfg-field" data-path="${path}">${key!=null?`<label>${escape(titleOf(schema,key))}</label>`:''}${desc}<select data-kind="enum">${opts}</select></div>`;
    }
    // Determine active variant: match by 'type' discriminator if present
    let active=0;
    if(value&&typeof value==='object'&&value.type){
      variants.forEach((v,i)=>{const t=v.properties&&v.properties.type;if(t&&(t.const===value.type||(t.enum&&t.enum.includes(value.type))))active=i});
    }
    const tabs=variants.map((v,i)=>{
      const t=v.properties&&v.properties.type;
      const name=(t&&(t.const||(t.enum&&t.enum[0])))||v.title||`Variant ${i+1}`;
      return `<button type="button" class="cfg-union-tab${i===active?' active':''}" data-tab="${i}">${escape(name)}</button>`;
    }).join('');
    const panels=variants.map((v,i)=>`<div class="cfg-union-panel${i===active?' active':''}" data-panel="${i}">${renderObject(v,i===active?(value||{}):{},path,null)}</div>`).join('');
    return `<div class="cfg-field" data-path="${path}" data-union="1"><label>${escape(titleOf(schema,key))}</label><div class="cfg-union-tabs">${tabs}</div>${panels}</div>`;
  }

  function escape(s){return String(s==null?'':s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c])}

  function collect(el,path){
    // Walk DOM, build nested object matching original paths
    const out={};
    const fields=el.querySelectorAll('[data-path]');
    fields.forEach(f=>{
      // Only innermost leaves; skip object/array/union wrappers
      if(f.dataset.obj||f.dataset.arr||f.dataset.union)return;
      // Check we're not inside an inactive union panel
      let p=f.parentElement;
      while(p&&p!==el){
        if(p.classList&&p.classList.contains('cfg-union-panel')&&!p.classList.contains('active'))return;
        p=p.parentElement;
      }
      const fp=f.dataset.path;
      const input=f.querySelector('input,select');
      if(!input)return;
      let val;
      const k=input.dataset.kind;
      if(k==='bool')val=input.checked;
      else if(k==='num'){val=input.value===''?null:Number(input.value)}
      else if(k==='pw'){if(input.value==='')val=input.dataset.orig==='1'?MASKED:null;else val=input.value}
      else if(k==='json'){try{val=JSON.parse(input.value)}catch{val=input.value}}
      else val=input.value===''?null:input.value;
      setPath(out,fp,val);
    });
    // Arrays: reconstruct order from DOM
    const arrs=el.querySelectorAll('[data-arr="1"]');
    arrs.forEach(a=>{
      const fp=a.dataset.path;
      const items=[];
      a.querySelectorAll(':scope > .cfg-array > .cfg-array-item').forEach((it,i)=>{
        const sub=collect(it,`${fp}[${i}]`);
        // If item's root was a single leaf (not object), pick the leaf value
        const leafKey=`${fp}[${i}]`;
        if(sub&&typeof sub==='object'&&Object.keys(sub).length===1&&sub[fp]!==undefined){items.push(sub[fp][`[${i}]`]||sub[fp])}
        else items.push(getPath(sub,leafKey)||sub);
      });
      setPath(out,fp,items);
    });
    return out;
  }

  function setPath(obj,path,val){
    const tokens=tokenize(path);
    let c=obj;
    for(let i=0;i<tokens.length-1;i++){
      const t=tokens[i], nxt=tokens[i+1], isArr=typeof nxt==='number';
      if(c[t]==null)c[t]=isArr?[]:{};
      c=c[t];
    }
    c[tokens[tokens.length-1]]=val;
  }
  function getPath(obj,path){const tokens=tokenize(path);let c=obj;for(const t of tokens){if(c==null)return undefined;c=c[t]}return c}
  function tokenize(path){
    const out=[];
    path.replace(/([^.\[\]]+)|\[(\d+)\]/g,(_,name,idx)=>{if(name!=null)out.push(name);else out.push(Number(idx))});
    return out;
  }

  function showToast(msg,cls){toast.style.display='block';toast.className='toast '+cls;toast.textContent=msg;if(cls==='ok')setTimeout(()=>{toast.style.display='none'},6000)}

  async function loadAndRender(){
    try{
      const[cfgResp,schemaResp]=await Promise.all([fetch('/api/config'),fetch('/api/config/schema')]);
      if(!cfgResp.ok||!schemaResp.ok)throw new Error('Load failed: '+cfgResp.status+'/'+schemaResp.status);
      const cfg=await cfgResp.json();
      rootSchema=await schemaResp.json();
      currentData=cfg.config||{};
      root.innerHTML=renderObject(rootSchema,currentData,'',null);
      bindEvents();
      if(cfg.restart_pending)showToast('Restart pending — Änderungen warten auf Prozess-Neustart.','warn');
    }catch(e){showToast('Config laden fehlgeschlagen: '+e.message,'err')}
  }

  function bindEvents(){
    root.addEventListener('click',ev=>{
      const rm=ev.target.closest('.cfg-rm');
      if(rm){const item=rm.closest('.cfg-array-item');if(item)item.remove();ev.preventDefault();return}
      const add=ev.target.closest('.cfg-add');
      if(add){
        const arr=document.getElementById(add.dataset.target);
        const itemSchema=JSON.parse(arr.dataset.itemSchema.replace(/&amp;/g,'&').replace(/&lt;/g,'<').replace(/&gt;/g,'>').replace(/&quot;/g,'"').replace(/&#39;/g,"'"));
        const i=arr.children.length;
        const wrap=document.createElement('div');
        wrap.className='cfg-array-item';wrap.dataset.idx=i;
        wrap.innerHTML=`<button type="button" class="cfg-rm">×</button>`+renderField(itemSchema,null,`${add.dataset.path}[${i}]`,null);
        arr.appendChild(wrap);
        ev.preventDefault();return;
      }
      const tab=ev.target.closest('.cfg-union-tab');
      if(tab){
        const union=tab.closest('[data-union="1"]');
        union.querySelectorAll('.cfg-union-tab').forEach(t=>t.classList.remove('active'));
        union.querySelectorAll('.cfg-union-panel').forEach(p=>p.classList.remove('active'));
        tab.classList.add('active');
        union.querySelector(`.cfg-union-panel[data-panel="${tab.dataset.tab}"]`).classList.add('active');
        ev.preventDefault();return;
      }
    });
  }

  // Normalize `custom_metrics` entries before save. The form always renders
  // all fields (poll + notification blocks regardless of source), but the
  // backend expects at most one of `poll` / `notification` to be populated
  // and rejects null numeric sub-fields. We therefore:
  //   1. Drop the inapplicable variant entirely based on `source`.
  //   2. Strip keys with null values *inside the entry* so serde's
  //      `#[serde(default)]` can fill them in.
  // The rest of the payload is left as-is — other parts of the form were
  // written to emit nulls only in spots where the Rust struct already
  // handles them.
  function stripNullsFromItem(obj){
    if(Array.isArray(obj))return obj.map(stripNullsFromItem);
    if(obj&&typeof obj==='object'){
      const out={};
      for(const [k,v] of Object.entries(obj)){
        if(v==null)continue;
        out[k]=stripNullsFromItem(v);
      }
      return out;
    }
    return obj;
  }
  // Drop or collapse null-valued sub-objects that the backend requires to be
  // absent rather than null. Covers `transport.tls` (contains non-Option
  // `ca_cert_path`) and other Option<Struct> fields the form always renders.
  function normalizeOptionalStructs(payload){
    const r=payload?.receiver;
    if(r){
      // Top-level TLS block: if disabled and paths all null, strip to null.
      const tls=r.tls;
      if(tls&&tls.enabled===false){
        // All path fields are Option<PathBuf>; nulls are fine individually,
        // but the struct itself is also Option<ReceiverTls> — cleaner to null
        // the whole thing when off. Keep enabled=false only.
      }
      // Transport TLS: the Mqtt variant carries a *non-Option* ca_cert_path.
      // If the form emits null there, the whole transport.tls must be dropped
      // (it is Option<MqttTlsConfig> / Option<NatsTlsConfig> on the backend).
      const t=r.transport;
      if(t&&t.tls&&typeof t.tls==='object'){
        const hasAnyPath=['ca_cert_path','client_cert_path','client_key_path']
          .some(k=>t.tls[k]!=null);
        if(!hasAnyPath)t.tls=null;
      }
    }
    return payload;
  }
  function normalizeCustomMetrics(payload){
    const items=payload&&payload.metrics&&Array.isArray(payload.metrics.custom_metrics)
      ?payload.metrics.custom_metrics:null;
    if(!items)return payload;
    payload.metrics.custom_metrics=items.map(item=>{
      if(!item||typeof item!=='object')return item;
      if(item.source==='poll'||item.source==='push')item.notification=null;
      if(item.source==='notification'||item.source==='push')item.poll=null;
      const allNull=o=>o&&typeof o==='object'&&Object.values(o).every(v=>v==null);
      if(allNull(item.poll))item.poll=null;
      if(allNull(item.notification))item.notification=null;
      // Drop leftover nulls inside the item itself (e.g. unset description).
      return stripNullsFromItem(item);
    });
    return payload;
  }

  window.saveConfig=async function(){
    if(!rootSchema){showToast('Schema noch nicht geladen.','err');return}
    saveBtn.disabled=true;
    try{
      const payload=normalizeOptionalStructs(normalizeCustomMetrics(collect(root,'')));
      const r=await fetch('/api/config',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(payload)});
      const res=await r.json();
      if(r.ok){
        const hot=(res.hot_reloaded||[]).join(', ')||'–';
        const rr=(res.restart_required||[]).join(', ');
        const msg=`✓ Gespeichert. Hot-reloaded: ${hot}.`+(rr?` Restart erforderlich: ${rr}.`:'');
        showToast(msg,rr?'warn':'ok');
        currentData=payload;
      }else if(res.errors){showToast('Validierung: '+res.errors.join('; '),'err')}
      else{showToast('Fehler: '+(res.detail||res.error||r.statusText),'err')}
    }catch(e){showToast('Save fehlgeschlagen: '+e.message,'err')}
    finally{saveBtn.disabled=false}
  };

  function route(){
    const h=location.hash;
    const isCfg=h==='#/config';
    const isSym=h==='#/symbols';
    document.getElementById('dashboard-view').style.display=(isCfg||isSym)?'none':'';
    document.getElementById('config-view').style.display=isCfg?'':'none';
    document.getElementById('symbols-view').style.display=isSym?'':'none';
    document.getElementById('nav-cfg').style.background=isCfg?'#1d4ed8':'';
    document.getElementById('nav-sym').style.background=isSym?'#1d4ed8':'';
    document.getElementById('nav-dash').style.background=(isCfg||isSym)?'':'#1d4ed8';
    if(isCfg&&!rootSchema)loadAndRender();
    if(isSym)window.symLoadTargets&&window.symLoadTargets();
  }
  window.addEventListener('hashchange',route);
  route();
})();
</script>

<!-- T8: Symbol browser -->
<script>
(function(){
  let currentTarget=null;
  let lastSymbols=[];

  async function fetchJson(url,opts){
    const r=await fetch(url,opts);
    if(r.status===503){throw new Error('client-bridge is not enabled (HTTP 503)')}
    if(!r.ok){let t='';try{t=await r.text()}catch(_){}throw new Error(t||r.statusText)}
    return r.json();
  }

  function banner(msg,kind){
    const el=document.getElementById('sym-banner');
    if(!msg){el.style.display='none';return}
    el.textContent=msg;
    el.style.display='block';
    el.style.color=kind==='err'?'#fecaca':'#fbbf24';
    el.style.background=kind==='err'?'#450a0a':'#422006';
    el.style.borderColor=kind==='err'?'#dc2626':'#ca8a04';
  }

  function esc(s){return String(s==null?'':s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c])}

  async function loadTargets(){
    banner('','');
    try{
      const list=await fetchJson('/api/client/targets');
      const tb=document.getElementById('sym-targets');
      if(!list.length){tb.innerHTML='<tr><td colspan="5" style="color:#64748b">No custom_metrics with poll/notification source configured.</td></tr>';return}
      tb.innerHTML=list.map(t=>{
        const cachedCell=t.cached?`<span style="color:#4ade80">✓</span>`:`<span style="color:#64748b">—</span>`;
        const when=t.fetched_at||'never';
        const pickLabel=currentTarget===t.ams_net_id?'Selected':'Browse';
        return `<tr><td><code>${esc(t.ams_net_id)}</code></td><td>${cachedCell}</td><td>${t.symbol_count}</td><td style="color:#94a3b8;font-size:.8rem">${esc(when)}</td><td><button class="btn" onclick="symPick('${esc(t.ams_net_id)}')">${pickLabel}</button></td></tr>`;
      }).join('');
    }catch(e){banner('Failed to load targets: '+e.message,'err')}
  }

  window.symLoadTargets=loadTargets;

  window.symPick=async function(target){
    currentTarget=target;
    document.getElementById('sym-selected-target').textContent=target;
    await loadSymbols();
    loadTargets();
  };

  async function loadSymbols(){
    if(!currentTarget){return}
    const filter=document.getElementById('sym-filter').value.trim();
    const url='/api/client/symbols?target='+encodeURIComponent(currentTarget)+(filter?('&filter='+encodeURIComponent(filter)):'');
    try{
      const res=await fetchJson(url);
      lastSymbols=res.symbols;
      document.getElementById('sym-stats').textContent=`${res.count} symbol(s) — fetched ${res.fetched_at||'—'}`;
      renderSymbols();
    }catch(e){
      document.getElementById('sym-rows').innerHTML=`<tr><td colspan="6" style="color:#fecaca">Error: ${esc(e.message)}</td></tr>`;
      document.getElementById('sym-stats').textContent='';
    }
  }

  function renderSymbols(){
    const body=document.getElementById('sym-rows');
    if(!lastSymbols.length){body.innerHTML='<tr><td colspan="6" style="color:#64748b">No symbols match.</td></tr>';return}
    const limit=500;
    const shown=lastSymbols.slice(0,limit);
    body.innerHTML=shown.map(s=>
      `<tr><td><code>${esc(s.name)}</code></td><td>${esc(s.type_name)}</td><td>${s.size}</td><td>0x${s.igroup.toString(16)}</td><td>${s.ioffset}</td><td><button class="btn" style="padding:.2rem .5rem;font-size:.75rem" onclick="navigator.clipboard&&navigator.clipboard.writeText('${esc(s.name)}')">copy</button></td></tr>`
    ).join('') + (lastSymbols.length>limit?`<tr><td colspan="6" style="color:#64748b">… ${lastSymbols.length-limit} more, narrow the filter.</td></tr>`:'');
  }

  window.symRefreshCurrent=async function(){
    if(!currentTarget){return}
    try{
      await fetchJson('/api/client/symbols/refresh?target='+encodeURIComponent(currentTarget),{method:'POST'});
      banner('Cache invalidated — waiting for next reconcile to repopulate.','');
      await loadTargets();
    }catch(e){banner('Refresh failed: '+e.message,'err')}
  };

  document.getElementById('sym-filter').addEventListener('input',()=>{
    clearTimeout(window._symFilterTimer);
    window._symFilterTimer=setTimeout(loadSymbols,200);
  });
})();
</script>
<!-- CONFIG-FORM-JS-INJECTION-POINT -->

<script>
const API='';
let refreshTimer;

async function fetchJson(url,opts){
  const r=await fetch(API+url,opts);
  if(!r.ok)throw new Error(await r.text());
  return r.json();
}

function showError(msg){const e=document.getElementById('error');e.textContent=msg;e.style.display='block';setTimeout(()=>e.style.display='none',5000)}

function fmtUptime(s){const h=Math.floor(s/3600),m=Math.floor((s%3600)/60),sec=s%60;return`${h}h ${m}m ${sec}s`}

async function refresh(){
  try{
    const[st,cn,tk,cy]=await Promise.all([
      fetchJson('/api/status'),fetchJson('/api/connections'),
      fetchJson('/api/tasks'),fetchJson('/api/cycle-metrics')
    ]);
    document.getElementById('stats').innerHTML=`
      <div class="card"><div class="label">Status</div><div class="value ok">${st.status}</div></div>
      <div class="card"><div class="label">Uptime</div><div class="value">${fmtUptime(st.uptime_secs)}</div></div>
      <div class="card"><div class="label">Logs Received</div><div class="value">${st.logs_received.toLocaleString()}</div></div>
      <div class="card"><div class="label">Logs Dispatched</div><div class="value">${st.logs_dispatched.toLocaleString()}</div></div>
      <div class="card"><div class="label">Logs Failed</div><div class="value ${st.logs_failed>0?'warn':''}">${st.logs_failed.toLocaleString()}</div></div>
      <div class="card"><div class="label">Active Connections</div><div class="value">${st.connections_active}</div></div>
      <div class="card"><div class="label">Registered Tasks</div><div class="value">${st.registered_tasks}</div></div>`;
    document.getElementById('uptime').textContent='Uptime: '+fmtUptime(st.uptime_secs);
    document.getElementById('conn-body').innerHTML=cn.length?cn.map(c=>`<tr><td>${c.ip}</td><td>${c.count}</td></tr>`).join(''):'<tr><td colspan="2" style="color:#64748b">No active connections</td></tr>';
    document.getElementById('task-body').innerHTML=tk.length?tk.map(t=>`<tr><td>${t.ams_net_id}</td><td>${t.ams_source_port}</td><td>${t.task_name}</td><td>${t.app_name}</td><td>${t.project_name}</td><td>${t.online_change_count}</td></tr>`).join(''):'<tr><td colspan="6" style="color:#64748b">No registered tasks</td></tr>';
    document.getElementById('cycle-body').innerHTML=cy.length?cy.map(c=>`<tr><td>${c.ams_net_id}</td><td>${c.task_name} [${c.task_index}]</td><td>${c.avg_us.toFixed(1)}</td><td>${c.min_us.toFixed(1)}</td><td>${c.max_us.toFixed(1)}</td><td>${c.jitter_us.toFixed(1)}</td><td>${c.sample_count}</td><td>${c.total_cycles.toLocaleString()}</td></tr>`).join(''):'<tr><td colspan="8" style="color:#64748b">No cycle data yet</td></tr>';
    document.getElementById('last-update').textContent='Last update: '+new Date().toLocaleTimeString();
  }catch(e){showError('Refresh failed: '+e.message)}
}

refresh();
refreshTimer=setInterval(refresh,5000);
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tc_otel_ads::{RegistrationKey, TaskMetadata};
    use tower::ServiceExt;

    fn test_state() -> WebState {
        WebState {
            stats: Arc::new(DiagnosticStats::new()),
            conn_manager: Arc::new(ConnectionManager::new(
                tc_otel_ads::ConnectionConfig::default(),
            )),
            task_registry: Arc::new(TaskRegistry::new()),
            symbols: Arc::new(SymbolStore::new()),
            cycle_tracker: Arc::new(CycleTimeTracker::new(1000)),
            service_name: "test-service".to_string(),
            config_path: Arc::new(std::path::PathBuf::from("/tmp/config.json")),
            current_settings: Arc::new(RwLock::new(AppSettings::default())),
            restart_pending: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "client-bridge")]
            client_bridge: None,
        }
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let app = router(test_state());
        let resp = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["service"], "test-service");
    }

    #[tokio::test]
    async fn test_status_endpoint() {
        let state = test_state();
        state.stats.logs_received.store(42, Ordering::Relaxed);
        state.stats.logs_dispatched.store(40, Ordering::Relaxed);
        state.stats.logs_failed.store(2, Ordering::Relaxed);

        let app = router(state);
        let resp = app
            .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "running");
        assert_eq!(json["logs_received"], 42);
        assert_eq!(json["logs_dispatched"], 40);
        assert_eq!(json["logs_failed"], 2);
    }

    #[tokio::test]
    async fn test_connections_endpoint_empty() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::get("/api/connections")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(json.is_empty());
    }

    #[tokio::test]
    async fn test_tasks_endpoint_with_data() {
        let state = test_state();
        state.task_registry.register(
            RegistrationKey {
                ams_net_id: "5.80.201.232.1.1".to_string(),
                ams_source_port: 851,
                task_index: 0,
            },
            TaskMetadata {
                task_name: "PlcTask".to_string(),
                app_name: "MyApp".to_string(),
                project_name: "MyProject".to_string(),
                online_change_count: 3,
            },
        );

        let app = router(state);
        let resp = app
            .oneshot(Request::get("/api/tasks").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["task_name"], "PlcTask");
        assert_eq!(json[0]["app_name"], "MyApp");
    }

    #[tokio::test]
    async fn test_dashboard_returns_html() {
        let app = router(test_state());
        let resp = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"));
    }

    #[test]
    fn test_diagnostic_stats_defaults() {
        let stats = DiagnosticStats::new();
        assert_eq!(stats.logs_received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.logs_dispatched.load(Ordering::Relaxed), 0);
        assert_eq!(stats.logs_failed.load(Ordering::Relaxed), 0);
        assert_eq!(stats.connections_accepted.load(Ordering::Relaxed), 0);
        assert_eq!(stats.connections_rejected.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_diagnostic_stats_increment() {
        let stats = DiagnosticStats::new();
        stats.logs_received.fetch_add(100, Ordering::Relaxed);
        stats.logs_dispatched.fetch_add(95, Ordering::Relaxed);
        stats.logs_failed.fetch_add(5, Ordering::Relaxed);
        assert_eq!(stats.logs_received.load(Ordering::Relaxed), 100);
        assert_eq!(stats.logs_dispatched.load(Ordering::Relaxed), 95);
        assert_eq!(stats.logs_failed.load(Ordering::Relaxed), 5);
    }

    // --- Symbol store and endpoint tests ---

    #[test]
    fn test_symbol_store_empty() {
        let store = SymbolStore::new();
        assert_eq!(store.count(), 0);
        assert!(store.list().is_empty());
        assert!(store.find_by_name("MAIN.bFlag").is_none());
    }

    #[test]
    fn test_symbol_store_update_and_list() {
        let store = SymbolStore::new();
        store.update(vec![
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 0,
                size: 1,
                data_type: 33,
                flags: 0,
                name: "MAIN.bFlag".to_string(),
                type_name: "BOOL".to_string(),
                comment: String::new(),
            },
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 8,
                size: 8,
                data_type: 5,
                flags: 0,
                name: "MAIN.fSpeed".to_string(),
                type_name: "LREAL".to_string(),
                comment: "Motor speed".to_string(),
            },
        ]);
        assert_eq!(store.count(), 2);
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn test_symbol_store_find_by_name_case_insensitive() {
        let store = SymbolStore::new();
        store.update(vec![AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 0,
            size: 1,
            data_type: 33,
            flags: 0,
            name: "MAIN.bMotorRunning".to_string(),
            type_name: "BOOL".to_string(),
            comment: String::new(),
        }]);

        assert!(store.find_by_name("MAIN.bMotorRunning").is_some());
        assert!(store.find_by_name("main.bmotorrunning").is_some());
        assert!(store.find_by_name("MAIN.BMOTORRUNNING").is_some());
        assert!(store.find_by_name("MAIN.nonexistent").is_none());
    }

    #[test]
    fn test_symbol_store_update_replaces() {
        let store = SymbolStore::new();
        store.update(vec![AdsSymbolEntry {
            index_group: 1,
            index_offset: 0,
            size: 1,
            data_type: 33,
            flags: 0,
            name: "old".to_string(),
            type_name: "BOOL".to_string(),
            comment: String::new(),
        }]);
        assert_eq!(store.count(), 1);

        store.update(vec![
            AdsSymbolEntry {
                index_group: 2,
                index_offset: 0,
                size: 2,
                data_type: 3,
                flags: 0,
                name: "new1".to_string(),
                type_name: "INT".to_string(),
                comment: String::new(),
            },
            AdsSymbolEntry {
                index_group: 2,
                index_offset: 2,
                size: 4,
                data_type: 4,
                flags: 0,
                name: "new2".to_string(),
                type_name: "REAL".to_string(),
                comment: String::new(),
            },
        ]);
        assert_eq!(store.count(), 2);
        assert!(store.find_by_name("old").is_none());
        assert!(store.find_by_name("new1").is_some());
    }

    #[tokio::test]
    async fn test_symbols_endpoint_empty() {
        let app = router(test_state());
        let resp = app
            .oneshot(Request::get("/api/symbols").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 0);
        assert!(json["symbols"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_symbols_endpoint_with_data() {
        let state = test_state();
        state.symbols.update(vec![
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 0,
                size: 1,
                data_type: 33,
                flags: 0x0008,
                name: "MAIN.bMotorRunning".to_string(),
                type_name: "BOOL".to_string(),
                comment: "Motor status".to_string(),
            },
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 8,
                size: 8,
                data_type: 5,
                flags: 0x0008,
                name: "MAIN.fSpeed".to_string(),
                type_name: "LREAL".to_string(),
                comment: String::new(),
            },
        ]);

        let app = router(state);
        let resp = app
            .oneshot(Request::get("/api/symbols").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 2);
        let symbols = json["symbols"].as_array().unwrap();
        assert_eq!(symbols[0]["name"], "MAIN.bMotorRunning");
        assert_eq!(symbols[0]["type_name"], "BOOL");
        assert_eq!(symbols[0]["comment"], "Motor status");
        assert_eq!(symbols[1]["name"], "MAIN.fSpeed");
        assert_eq!(symbols[1]["type_name"], "LREAL");
    }

    #[tokio::test]
    async fn test_symbol_by_name_found() {
        let state = test_state();
        state.symbols.update(vec![AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 0,
            size: 1,
            data_type: 33,
            flags: 0,
            name: "GVL.bTestFlag".to_string(),
            type_name: "BOOL".to_string(),
            comment: "Test flag".to_string(),
        }]);

        let app = router(state);
        let resp = app
            .oneshot(
                Request::get("/api/symbols/GVL.bTestFlag")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["name"], "GVL.bTestFlag");
        assert_eq!(json["type_name"], "BOOL");
        assert_eq!(json["size"], 1);
        assert_eq!(json["comment"], "Test flag");
    }

    #[tokio::test]
    async fn test_cycle_metrics_endpoint_empty() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::get("/api/cycle-metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(json.is_empty());
    }

    #[tokio::test]
    async fn test_cycle_metrics_endpoint_with_data() {
        let state = test_state();
        let t0 = chrono::Utc::now();
        let t1 = t0 + chrono::Duration::milliseconds(1);
        state
            .cycle_tracker
            .record("5.80.201.232.1.1", 0, "PlcTask", 100, t0);
        state
            .cycle_tracker
            .record("5.80.201.232.1.1", 0, "PlcTask", 101, t1);

        let app = router(state);
        let resp = app
            .oneshot(
                Request::get("/api/cycle-metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["task_name"], "PlcTask");
        assert_eq!(json[0]["task_index"], 0);
        assert!(json[0]["avg_us"].as_f64().unwrap() > 0.0);
        assert!(json[0]["min_us"].as_f64().unwrap() > 0.0);
        assert!(json[0]["max_us"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn test_symbol_by_name_not_found() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::get("/api/symbols/NONEXISTENT.var")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- T7: client-bridge routes ----------------------------------------

    #[tokio::test]
    async fn client_symbols_503_when_bridge_absent() {
        // In default builds (feature off) the handler always returns 503.
        // In feature-on builds the state.client_bridge is None by default in
        // tests (test_state() sets it to None), so the handler also returns
        // 503. Either way the response is the same.
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::get("/api/client/symbols?target=10.0.0.1.1.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn client_refresh_503_when_bridge_absent() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::post("/api/client/symbols/refresh?target=10.0.0.1.1.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn client_targets_503_when_bridge_absent() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::get("/api/client/targets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[cfg(feature = "client-bridge")]
    #[tokio::test]
    async fn client_symbols_returns_cached_tree() {
        use std::sync::Arc;
        use tc_otel_client::browse::{SymbolNode, SymbolTree};
        use tc_otel_client::cache::{SymbolTreeCache, TargetKey};

        let cache = Arc::new(SymbolTreeCache::new());
        let mut tree = SymbolTree::default();
        tree.nodes.push(SymbolNode {
            name: "MAIN.fTemp".into(),
            type_name: "LREAL".into(),
            comment: String::new(),
            igroup: 0x4040,
            ioffset: 0,
            size: 8,
            datatype: 5,
            flags: 0,
        });
        let key = TargetKey([10, 0, 0, 1, 1, 1]);
        cache.insert(key, tree);

        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let bridge = crate::client_bridge::ClientBridge::new(tx, cache);
        let mut state = test_state();
        state.client_bridge = Some(bridge);
        let app = router(state);

        let resp = app
            .oneshot(
                Request::get("/api/client/symbols?target=10.0.0.1.1.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 1);
        assert_eq!(json["symbols"][0]["name"], "MAIN.fTemp");
        assert_eq!(json["symbols"][0]["type_name"], "LREAL");
    }

    #[cfg(feature = "client-bridge")]
    #[tokio::test]
    async fn client_symbols_404_for_unknown_target() {
        use std::sync::Arc;
        use tc_otel_client::cache::SymbolTreeCache;

        let cache = Arc::new(SymbolTreeCache::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let bridge = crate::client_bridge::ClientBridge::new(tx, cache);
        let mut state = test_state();
        state.client_bridge = Some(bridge);
        let app = router(state);

        let resp = app
            .oneshot(
                Request::get("/api/client/symbols?target=99.99.99.99.1.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "client-bridge")]
    #[tokio::test]
    async fn client_refresh_invalidates_cache() {
        use std::sync::Arc;
        use tc_otel_client::browse::SymbolTree;
        use tc_otel_client::cache::{SymbolTreeCache, TargetKey};

        let cache = Arc::new(SymbolTreeCache::new());
        let key = TargetKey([10, 0, 0, 1, 1, 1]);
        cache.insert(key, SymbolTree::default());
        assert!(cache.get(key).is_some());

        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let bridge = crate::client_bridge::ClientBridge::new(tx, cache.clone());
        let mut state = test_state();
        state.client_bridge = Some(bridge);
        let app = router(state);

        let resp = app
            .oneshot(
                Request::post("/api/client/symbols/refresh?target=10.0.0.1.1.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(cache.get(key).is_none());
    }

    #[cfg(feature = "client-bridge")]
    #[tokio::test]
    async fn client_symbols_bad_netid_rejected() {
        use std::sync::Arc;
        use tc_otel_client::cache::SymbolTreeCache;

        let cache = Arc::new(SymbolTreeCache::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let bridge = crate::client_bridge::ClientBridge::new(tx, cache);
        let mut state = test_state();
        state.client_bridge = Some(bridge);
        let app = router(state);

        let resp = app
            .oneshot(
                Request::get("/api/client/symbols?target=not-a-netid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
