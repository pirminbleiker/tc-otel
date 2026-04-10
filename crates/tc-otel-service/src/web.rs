//! Web UI server for status, diagnostics, and PLC tag browsing
//!
//! Provides a REST API and embedded SPA dashboard for monitoring the tc-otel
//! service. Users can browse PLC tags and subscribe to up to 500 variables
//! for real-time metric collection.

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tc_otel_ads::{AdsSymbolEntry, ConnectionManager, TaskRegistry};
use tc_otel_core::WebConfig;

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

/// Manages tag subscriptions with a configurable maximum
#[derive(Debug)]
pub struct SubscriptionManager {
    subscriptions: RwLock<HashSet<String>>,
    max_subscriptions: usize,
}

impl SubscriptionManager {
    pub fn new(max_subscriptions: usize) -> Self {
        Self {
            subscriptions: RwLock::new(HashSet::new()),
            max_subscriptions,
        }
    }

    pub fn subscribe(&self, tag: &str) -> Result<(), SubscriptionError> {
        let mut subs = self.subscriptions.write().unwrap();
        if subs.contains(tag) {
            return Ok(()); // Already subscribed
        }
        if subs.len() >= self.max_subscriptions {
            return Err(SubscriptionError::LimitReached {
                max: self.max_subscriptions,
            });
        }
        subs.insert(tag.to_string());
        Ok(())
    }

    pub fn unsubscribe(&self, tag: &str) -> bool {
        self.subscriptions.write().unwrap().remove(tag)
    }

    pub fn list(&self) -> Vec<String> {
        let subs = self.subscriptions.read().unwrap();
        let mut tags: Vec<String> = subs.iter().cloned().collect();
        tags.sort();
        tags
    }

    pub fn count(&self) -> usize {
        self.subscriptions.read().unwrap().len()
    }

    pub fn max(&self) -> usize {
        self.max_subscriptions
    }

    pub fn clear(&self) {
        self.subscriptions.write().unwrap().clear();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubscriptionError {
    LimitReached { max: usize },
}

impl std::fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscriptionError::LimitReached { max } => {
                write!(f, "subscription limit reached (max {})", max)
            }
        }
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
    pub subscriptions: Arc<SubscriptionManager>,
    pub symbols: Arc<SymbolStore>,
    pub service_name: String,
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
    subscriptions_active: usize,
    subscriptions_max: usize,
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
struct SubscriptionsResponse {
    count: usize,
    max: usize,
    tags: Vec<String>,
}

#[derive(Deserialize)]
pub struct SubscribeRequest {
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
pub struct UnsubscribeRequest {
    pub tags: Vec<String>,
}

#[derive(Serialize)]
struct SubscribeResponse {
    subscribed: Vec<String>,
    count: usize,
    max: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

#[derive(Serialize)]
struct UnsubscribeResponse {
    unsubscribed: Vec<String>,
    count: usize,
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
        subscriptions_active: state.subscriptions.count(),
        subscriptions_max: state.subscriptions.max(),
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

async fn get_subscriptions(State(state): State<WebState>) -> Json<SubscriptionsResponse> {
    Json(SubscriptionsResponse {
        count: state.subscriptions.count(),
        max: state.subscriptions.max(),
        tags: state.subscriptions.list(),
    })
}

async fn subscribe(
    State(state): State<WebState>,
    Json(req): Json<SubscribeRequest>,
) -> Result<Json<SubscribeResponse>, (StatusCode, Json<serde_json::Value>)> {
    if req.tags.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "tags array must not be empty"})),
        ));
    }

    let mut subscribed = Vec::new();
    let mut errors = Vec::new();

    for tag in &req.tags {
        if tag.is_empty() {
            errors.push("empty tag name skipped".to_string());
            continue;
        }
        match state.subscriptions.subscribe(tag) {
            Ok(()) => subscribed.push(tag.clone()),
            Err(SubscriptionError::LimitReached { max }) => {
                errors.push(format!(
                    "subscription limit reached (max {}), '{}' not added",
                    max, tag
                ));
            }
        }
    }

    Ok(Json(SubscribeResponse {
        subscribed,
        count: state.subscriptions.count(),
        max: state.subscriptions.max(),
        errors,
    }))
}

async fn unsubscribe(
    State(state): State<WebState>,
    Json(req): Json<UnsubscribeRequest>,
) -> Json<UnsubscribeResponse> {
    let mut unsubscribed = Vec::new();
    for tag in &req.tags {
        if state.subscriptions.unsubscribe(tag) {
            unsubscribed.push(tag.clone());
        }
    }
    Json(UnsubscribeResponse {
        unsubscribed,
        count: state.subscriptions.count(),
    })
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

async fn dashboard() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(DASHBOARD_HTML),
    )
}

/// Build the axum router for the web UI
pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/api/connections", get(connections))
        .route("/api/tasks", get(tasks))
        .route("/api/subscriptions", get(get_subscriptions))
        .route("/api/subscriptions", post(subscribe))
        .route("/api/subscriptions", delete(unsubscribe))
        .route("/api/symbols", get(get_symbols))
        .route("/api/symbols/:name", get(get_symbol_by_name))
        .with_state(state)
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

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
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
.tag-input{display:flex;gap:.5rem;margin-bottom:1rem}
.tag-input input{flex:1;padding:.5rem;background:#0f172a;border:1px solid #475569;border-radius:.25rem;color:#e2e8f0;font-size:.9rem}
.tag-input button,.btn{padding:.5rem 1rem;background:#2563eb;color:#fff;border:none;border-radius:.25rem;cursor:pointer;font-size:.9rem}
.tag-input button:hover,.btn:hover{background:#1d4ed8}
.btn-danger{background:#dc2626}.btn-danger:hover{background:#b91c1c}
.tag-list{display:flex;flex-wrap:wrap;gap:.5rem}
.tag{background:#334155;padding:.25rem .75rem;border-radius:1rem;font-size:.85rem;display:flex;align-items:center;gap:.5rem}
.tag .remove{cursor:pointer;color:#ef4444;font-weight:700}
.status-bar{display:flex;justify-content:space-between;align-items:center;margin-bottom:1rem;color:#64748b;font-size:.8rem}
#error{color:#ef4444;margin-bottom:1rem;display:none}
</style>
</head>
<body>
<div class="container">
<h1>tc-otel Dashboard</h1>
<div id="error"></div>
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
<h2>Tag Subscriptions <span id="sub-count"></span></h2>
<div class="tag-input">
<input id="tag-input" placeholder="Enter tag name (e.g. GVL.bMotorRunning)" onkeydown="if(event.key==='Enter')addTag()">
<button onclick="addTag()">Subscribe</button>
<button class="btn-danger" onclick="clearTags()">Clear All</button>
</div>
<div class="tag-list" id="tag-list"></div>
</div>
</div>

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
    const[st,cn,tk,sb]=await Promise.all([
      fetchJson('/api/status'),fetchJson('/api/connections'),
      fetchJson('/api/tasks'),fetchJson('/api/subscriptions')
    ]);
    document.getElementById('stats').innerHTML=`
      <div class="card"><div class="label">Status</div><div class="value ok">${st.status}</div></div>
      <div class="card"><div class="label">Uptime</div><div class="value">${fmtUptime(st.uptime_secs)}</div></div>
      <div class="card"><div class="label">Logs Received</div><div class="value">${st.logs_received.toLocaleString()}</div></div>
      <div class="card"><div class="label">Logs Dispatched</div><div class="value">${st.logs_dispatched.toLocaleString()}</div></div>
      <div class="card"><div class="label">Logs Failed</div><div class="value ${st.logs_failed>0?'warn':''}">${st.logs_failed.toLocaleString()}</div></div>
      <div class="card"><div class="label">Active Connections</div><div class="value">${st.connections_active}</div></div>
      <div class="card"><div class="label">Registered Tasks</div><div class="value">${st.registered_tasks}</div></div>
      <div class="card"><div class="label">Subscriptions</div><div class="value">${st.subscriptions_active}/${st.subscriptions_max}</div></div>`;
    document.getElementById('uptime').textContent='Uptime: '+fmtUptime(st.uptime_secs);
    document.getElementById('conn-body').innerHTML=cn.length?cn.map(c=>`<tr><td>${c.ip}</td><td>${c.count}</td></tr>`).join(''):'<tr><td colspan="2" style="color:#64748b">No active connections</td></tr>';
    document.getElementById('task-body').innerHTML=tk.length?tk.map(t=>`<tr><td>${t.ams_net_id}</td><td>${t.ams_source_port}</td><td>${t.task_name}</td><td>${t.app_name}</td><td>${t.project_name}</td><td>${t.online_change_count}</td></tr>`).join(''):'<tr><td colspan="6" style="color:#64748b">No registered tasks</td></tr>';
    document.getElementById('sub-count').textContent=`(${sb.count}/${sb.max})`;
    document.getElementById('tag-list').innerHTML=sb.tags.map(t=>`<span class="tag">${t}<span class="remove" onclick="removeTag('${t}')">&times;</span></span>`).join('');
    document.getElementById('last-update').textContent='Last update: '+new Date().toLocaleTimeString();
  }catch(e){showError('Refresh failed: '+e.message)}
}

async function addTag(){
  const input=document.getElementById('tag-input');
  const tag=input.value.trim();
  if(!tag)return;
  try{
    await fetchJson('/api/subscriptions',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({tags:[tag]})});
    input.value='';
    refresh();
  }catch(e){showError('Subscribe failed: '+e.message)}
}

async function removeTag(tag){
  try{
    await fetchJson('/api/subscriptions',{method:'DELETE',headers:{'Content-Type':'application/json'},body:JSON.stringify({tags:[tag]})});
    refresh();
  }catch(e){showError('Unsubscribe failed: '+e.message)}
}

async function clearTags(){
  const sb=await fetchJson('/api/subscriptions');
  if(sb.tags.length===0)return;
  try{
    await fetchJson('/api/subscriptions',{method:'DELETE',headers:{'Content-Type':'application/json'},body:JSON.stringify({tags:sb.tags})});
    refresh();
  }catch(e){showError('Clear failed: '+e.message)}
}

refresh();
refreshTimer=setInterval(refresh,5000);
</script>
</body>
</html>"#;

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
            subscriptions: Arc::new(SubscriptionManager::new(500)),
            symbols: Arc::new(SymbolStore::new()),
            service_name: "test-service".to_string(),
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
    async fn test_subscribe_and_list() {
        let state = test_state();
        let app = router(state);

        // Subscribe
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/subscriptions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"tags":["GVL.bMotorRunning","GVL.nCycleCount"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["subscribed"].as_array().unwrap().len(), 2);
        assert_eq!(json["count"], 2);

        // List
        let resp = app
            .oneshot(
                Request::get("/api/subscriptions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 2);
        assert_eq!(json["max"], 500);
    }

    #[tokio::test]
    async fn test_unsubscribe() {
        let state = test_state();
        state.subscriptions.subscribe("GVL.bTest").unwrap();

        let app = router(state);
        let resp = app
            .oneshot(
                Request::delete("/api/subscriptions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"tags":["GVL.bTest"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 0);
        assert_eq!(json["unsubscribed"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_subscribe_empty_tags_rejected() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::post("/api/subscriptions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"tags":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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

    // --- Unit tests for SubscriptionManager ---

    #[test]
    fn test_subscription_manager_basic() {
        let mgr = SubscriptionManager::new(3);
        assert_eq!(mgr.count(), 0);
        assert!(mgr.subscribe("tag1").is_ok());
        assert!(mgr.subscribe("tag2").is_ok());
        assert_eq!(mgr.count(), 2);
    }

    #[test]
    fn test_subscription_manager_limit() {
        let mgr = SubscriptionManager::new(2);
        assert!(mgr.subscribe("tag1").is_ok());
        assert!(mgr.subscribe("tag2").is_ok());
        assert_eq!(
            mgr.subscribe("tag3"),
            Err(SubscriptionError::LimitReached { max: 2 })
        );
        assert_eq!(mgr.count(), 2);
    }

    #[test]
    fn test_subscription_manager_duplicate() {
        let mgr = SubscriptionManager::new(2);
        assert!(mgr.subscribe("tag1").is_ok());
        assert!(mgr.subscribe("tag1").is_ok()); // duplicate OK
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn test_subscription_manager_unsubscribe() {
        let mgr = SubscriptionManager::new(10);
        mgr.subscribe("tag1").unwrap();
        mgr.subscribe("tag2").unwrap();
        assert!(mgr.unsubscribe("tag1"));
        assert!(!mgr.unsubscribe("tag1")); // already removed
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn test_subscription_manager_clear() {
        let mgr = SubscriptionManager::new(10);
        mgr.subscribe("a").unwrap();
        mgr.subscribe("b").unwrap();
        mgr.subscribe("c").unwrap();
        mgr.clear();
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn test_subscription_manager_list_sorted() {
        let mgr = SubscriptionManager::new(10);
        mgr.subscribe("zebra").unwrap();
        mgr.subscribe("alpha").unwrap();
        mgr.subscribe("middle").unwrap();
        let list = mgr.list();
        assert_eq!(list, vec!["alpha", "middle", "zebra"]);
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
}
