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

/// Serve a static UI asset bundled into the binary at compile time.
///
/// Allow-listed against a static map so an arbitrary path cannot be served.
async fn serve_asset(Path(rest): Path<String>) -> Result<impl IntoResponse, StatusCode> {
    let (body, ct): (&'static [u8], &'static str) = match rest.as_str() {
        "app.js" => (
            include_bytes!("web/ui/app.js"),
            "application/javascript; charset=utf-8",
        ),
        "styles.css" => (
            include_bytes!("web/ui/styles.css"),
            "text/css; charset=utf-8",
        ),
        "lib/util.js" => (
            include_bytes!("web/ui/lib/util.js"),
            "application/javascript; charset=utf-8",
        ),
        "lib/domains.js" => (
            include_bytes!("web/ui/lib/domains.js"),
            "application/javascript; charset=utf-8",
        ),
        "lib/charts.js" => (
            include_bytes!("web/ui/lib/charts.js"),
            "application/javascript; charset=utf-8",
        ),
        "views/dashboard.js" => (
            include_bytes!("web/ui/views/dashboard.js"),
            "application/javascript; charset=utf-8",
        ),
        "views/config.js" => (
            include_bytes!("web/ui/views/config.js"),
            "application/javascript; charset=utf-8",
        ),
        "views/symbols.js" => (
            include_bytes!("web/ui/views/symbols.js"),
            "application/javascript; charset=utf-8",
        ),
        "vendor/uplot.min.js" => (
            include_bytes!("web/ui/vendor/uplot.min.js"),
            "application/javascript; charset=utf-8",
        ),
        "vendor/uplot.min.css" => (
            include_bytes!("web/ui/vendor/uplot.min.css"),
            "text/css; charset=utf-8",
        ),
        _ => return Err(StatusCode::NOT_FOUND),
    };
    Ok((
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    ))
}

// --- Domains aggregation ---

#[derive(Serialize)]
struct DomainInfo {
    ams_net_id: String,
    friendly_name: String,
    sources: Vec<&'static str>,
    task_count: usize,
    metric_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    router_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbols_cached: Option<bool>,
}

async fn get_domains(State(state): State<WebState>) -> Json<Vec<DomainInfo>> {
    use std::collections::BTreeMap;

    // First pass: collect every AMS Net ID seen across config + registry.
    let mut acc: BTreeMap<String, DomainAcc> = BTreeMap::new();

    {
        let settings = state.current_settings.read().unwrap();
        for t in &settings.diagnostics.targets {
            let entry = acc.entry(t.ams_net_id.clone()).or_default();
            entry.diagnostics = true;
            // Manual task names act as a friendly hint when no router_host
            // is configured for any custom_metric on this domain.
            if entry.task_name_hint.is_none() {
                if let Some(name) = t.task_names.values().next() {
                    entry.task_name_hint = Some(name.clone());
                }
            }
        }
        for m in &settings.metrics.custom_metrics {
            let Some(id) = m.ams_net_id.as_deref() else {
                continue;
            };
            let entry = acc.entry(id.to_string()).or_default();
            entry.metric_count += 1;
            if entry.router_host.is_none() {
                if let Some(host) = m.ams_router_host.as_deref() {
                    entry.router_host = Some(host.to_string());
                }
            }
        }
    }

    for (key, _) in state.task_registry.all_tasks() {
        let entry = acc.entry(key.ams_net_id.clone()).or_default();
        entry.registered = true;
        entry.task_count += 1;
    }

    // Second pass: optional symbol-cache info from the client bridge.
    #[cfg(feature = "client-bridge")]
    if let Some(bridge) = state.client_bridge.as_ref() {
        let cache = bridge.cache();
        for (id, entry) in acc.iter_mut() {
            if let Some(netid) = parse_net_id(id) {
                let key = tc_otel_client::cache::TargetKey(netid);
                if let Some(tree) = cache.get(key) {
                    entry.symbols_cached = Some(true);
                    entry.symbol_count = Some(tree.len());
                } else {
                    entry.symbols_cached = Some(false);
                    entry.symbol_count = Some(0);
                }
            }
        }
    }

    let out: Vec<DomainInfo> = acc
        .into_iter()
        .map(|(ams_net_id, a)| {
            let mut sources = Vec::new();
            if a.diagnostics {
                sources.push("diagnostics");
            }
            if a.metric_count > 0 {
                sources.push("metrics");
            }
            if a.registered {
                sources.push("registered");
            }
            let friendly_name = a
                .router_host
                .clone()
                .or(a.task_name_hint.clone())
                .unwrap_or_else(|| ams_net_id.clone());
            DomainInfo {
                ams_net_id,
                friendly_name,
                sources,
                task_count: a.task_count,
                metric_count: a.metric_count,
                router_host: a.router_host,
                symbol_count: a.symbol_count,
                symbols_cached: a.symbols_cached,
            }
        })
        .collect();

    Json(out)
}

#[derive(Default)]
struct DomainAcc {
    diagnostics: bool,
    registered: bool,
    metric_count: usize,
    task_count: usize,
    router_host: Option<String>,
    task_name_hint: Option<String>,
    symbol_count: Option<usize>,
    symbols_cached: Option<bool>,
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
        .route("/assets/*path", get(serve_asset))
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/api/connections", get(connections))
        .route("/api/tasks", get(tasks))
        .route("/api/domains", get(get_domains))
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

const DASHBOARD_HTML: &str = include_str!("web/ui/index.html");

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

    #[tokio::test]
    async fn test_domains_endpoint_aggregates_sources() {
        let state = test_state();

        // Seed the config with a diagnostics target and a custom metric so
        // the handler has both `diagnostics` and `metrics` sources to merge.
        {
            let mut s = state.current_settings.write().unwrap();
            s.diagnostics.targets.push(tc_otel_core::config::DiagnosticsTargetConfig {
                ams_net_id: "10.20.30.40.1.1".to_string(),
                poll_interval_ms: 200,
                exceed_counter: true,
                rt_usage: true,
                task_ports: vec![350],
                rt_port: 200,
                task_names: std::collections::HashMap::from([("350".into(), "PlcTask".into())]),
            });
            s.metrics.custom_metrics.push(tc_otel_core::config::CustomMetricDef {
                symbol: "MAIN.x".into(),
                metric_name: "test.x".into(),
                ams_net_id: Some("10.20.30.40.1.1".into()),
                ams_router_host: Some("plc-a".into()),
                source: tc_otel_core::config::CustomMetricSource::Poll,
                ..Default::default()
            });
            // A second metric on a different domain that has no diagnostics.
            s.metrics.custom_metrics.push(tc_otel_core::config::CustomMetricDef {
                symbol: "MAIN.y".into(),
                metric_name: "test.y".into(),
                ams_net_id: Some("99.99.99.99.1.1".into()),
                ams_router_host: Some("plc-b".into()),
                source: tc_otel_core::config::CustomMetricSource::Notification,
                ..Default::default()
            });
        }

        // Register a task on the first domain so `registered` source flips on.
        state.task_registry.register(
            RegistrationKey {
                ams_net_id: "10.20.30.40.1.1".to_string(),
                ams_source_port: 851,
                task_index: 0,
            },
            TaskMetadata {
                task_name: "PlcTask".into(),
                app_name: "App".into(),
                project_name: "Proj".into(),
                online_change_count: 0,
            },
        );

        let app = router(state);
        let resp = app
            .oneshot(Request::get("/api/domains").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.len(), 2);

        // BTreeMap orders by AMS Net ID lexicographically.
        let first = &json[0];
        assert_eq!(first["ams_net_id"], "10.20.30.40.1.1");
        assert_eq!(first["friendly_name"], "plc-a");
        assert_eq!(first["task_count"], 1);
        assert_eq!(first["metric_count"], 1);
        let sources: Vec<&str> = first["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(sources.contains(&"diagnostics"));
        assert!(sources.contains(&"metrics"));
        assert!(sources.contains(&"registered"));

        let second = &json[1];
        assert_eq!(second["ams_net_id"], "99.99.99.99.1.1");
        assert_eq!(second["friendly_name"], "plc-b");
        assert_eq!(second["task_count"], 0);
        assert_eq!(second["metric_count"], 1);
    }

    #[tokio::test]
    async fn test_domains_endpoint_empty() {
        let app = router(test_state());
        let resp = app
            .oneshot(Request::get("/api/domains").body(Body::empty()).unwrap())
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
    async fn test_assets_serves_known_file() {
        let app = router(test_state());
        let resp = app
            .oneshot(Request::get("/assets/styles.css").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("text/css"));
    }

    #[tokio::test]
    async fn test_assets_rejects_unknown_file() {
        let app = router(test_state());
        let resp = app
            .oneshot(Request::get("/assets/../Cargo.toml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // Either NOT_FOUND from our allow-list, or a normalising redirect that
        // ultimately can't match `/assets/...`. Anything other than 200 is OK.
        assert_ne!(resp.status(), StatusCode::OK);
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
        let bridge = crate::client_bridge::ClientBridge::new(
            tx,
            cache,
            "localhost".to_string(),
            1883,
            "AdsOverMqtt".to_string(),
        );
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
        let bridge = crate::client_bridge::ClientBridge::new(
            tx,
            cache,
            "localhost".to_string(),
            1883,
            "AdsOverMqtt".to_string(),
        );
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
        let bridge = crate::client_bridge::ClientBridge::new(
            tx,
            cache.clone(),
            "localhost".to_string(),
            1883,
            "AdsOverMqtt".to_string(),
        );
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
        let bridge = crate::client_bridge::ClientBridge::new(
            tx,
            cache,
            "localhost".to_string(),
            1883,
            "AdsOverMqtt".to_string(),
        );
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
