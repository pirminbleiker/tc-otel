//! Web UI server for status, diagnostics, and PLC tag browsing
//!
//! Provides REST API endpoints and an embedded SPA dashboard for monitoring
//! tc-otel service health, PLC connections, and tag subscriptions.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tc_otel_ads::{ConnectionManager, TaskRegistry};
use tc_otel_core::{AppSettings, DiagnosticStats, SubscriptionManager};

/// Shared state for web handlers
pub struct WebState {
    pub stats: Arc<DiagnosticStats>,
    pub settings: AppSettings,
    pub conn_manager: Arc<ConnectionManager>,
    pub registry: Arc<TaskRegistry>,
    pub subscriptions: Arc<SubscriptionManager>,
}

/// Build the axum router with all web UI routes
pub fn build_router(state: Arc<WebState>) -> Router {
    Router::new()
        // Dashboard
        .route("/", get(dashboard_handler))
        // API endpoints
        .route("/api/health", get(health_handler))
        .route("/api/status", get(status_handler))
        .route("/api/config", get(config_handler))
        .route("/api/connections", get(connections_handler))
        .route("/api/tasks", get(tasks_handler))
        .route("/api/subscriptions", get(subscriptions_list_handler))
        .route("/api/subscriptions", post(subscriptions_add_handler))
        .route("/api/subscriptions", delete(subscriptions_remove_handler))
        .with_state(state)
}

/// Start the web server
pub async fn start_web_server(
    host: &str,
    port: u16,
    state: Arc<WebState>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app = build_router(state);
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Web UI listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

// --- Health ---

async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

// --- Status ---

#[derive(Serialize)]
struct StatusResponse {
    service_name: String,
    #[serde(flatten)]
    diagnostics: tc_otel_core::DiagnosticSnapshot,
    active_connections: usize,
    registered_tasks: usize,
    active_subscriptions: usize,
    max_subscriptions: usize,
}

async fn status_handler(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let snap = state.stats.snapshot();
    let resp = StatusResponse {
        service_name: state.settings.service.name.clone(),
        diagnostics: snap,
        active_connections: state.conn_manager.active_connections(),
        registered_tasks: state.registry.len(),
        active_subscriptions: state.subscriptions.count(),
        max_subscriptions: state.subscriptions.max(),
    };
    Json(resp)
}

// --- Config ---

#[derive(Serialize)]
struct ConfigResponse {
    receiver_host: String,
    ams_net_id: String,
    ams_tcp_port: u16,
    http_port: u16,
    grpc_port: u16,
    export_endpoint: String,
    batch_size: usize,
    max_connections: usize,
    web_port: u16,
}

async fn config_handler(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let resp = ConfigResponse {
        receiver_host: state.settings.receiver.host.clone(),
        ams_net_id: state.settings.receiver.ams_net_id.clone(),
        ams_tcp_port: state.settings.receiver.ams_tcp_port,
        http_port: state.settings.receiver.http_port,
        grpc_port: state.settings.receiver.grpc_port,
        export_endpoint: state.settings.export.endpoint.clone(),
        batch_size: state.settings.export.batch_size,
        max_connections: state.settings.receiver.max_connections,
        web_port: state.settings.web.port,
    };
    Json(resp)
}

// --- Connections ---

#[derive(Serialize)]
struct ConnectionInfo {
    ip: String,
    count: usize,
}

#[derive(Serialize)]
struct ConnectionsResponse {
    total: usize,
    connections: Vec<ConnectionInfo>,
}

async fn connections_handler(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let ips = state.conn_manager.connected_ips();
    let total = state.conn_manager.active_connections();
    let connections = ips
        .into_iter()
        .map(|(ip, count)| ConnectionInfo {
            ip: ip.to_string(),
            count,
        })
        .collect();
    Json(ConnectionsResponse { total, connections })
}

// --- Tasks ---

#[derive(Serialize)]
struct TaskInfo {
    ams_net_id: String,
    ams_source_port: u16,
    task_index: u8,
    task_name: String,
    app_name: String,
    project_name: String,
}

async fn tasks_handler(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let tasks: Vec<TaskInfo> = state
        .registry
        .all_tasks()
        .into_iter()
        .map(|(key, meta)| TaskInfo {
            ams_net_id: key.ams_net_id,
            ams_source_port: key.ams_source_port,
            task_index: key.task_index,
            task_name: meta.task_name,
            app_name: meta.app_name,
            project_name: meta.project_name,
        })
        .collect();
    Json(tasks)
}

// --- Subscriptions ---

#[derive(Serialize)]
struct SubscriptionsResponse {
    count: usize,
    max: usize,
    tags: Vec<String>,
}

async fn subscriptions_list_handler(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let tags = state.subscriptions.list();
    Json(SubscriptionsResponse {
        count: tags.len(),
        max: state.subscriptions.max(),
        tags,
    })
}

#[derive(Deserialize)]
struct SubscribeRequest {
    tag: String,
}

async fn subscriptions_add_handler(
    State(state): State<Arc<WebState>>,
    Json(req): Json<SubscribeRequest>,
) -> impl IntoResponse {
    match state.subscriptions.subscribe(req.tag) {
        Ok(()) => {
            let tags = state.subscriptions.list();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "count": tags.len(),
                    "max": state.subscriptions.max(),
                    "tags": tags,
                })),
            )
        }
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn subscriptions_remove_handler(
    State(state): State<Arc<WebState>>,
    Json(req): Json<SubscribeRequest>,
) -> impl IntoResponse {
    match state.subscriptions.unsubscribe(&req.tag) {
        Ok(()) => {
            let tags = state.subscriptions.list();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "count": tags.len(),
                    "max": state.subscriptions.max(),
                    "tags": tags,
                })),
            )
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

// --- Dashboard ---

async fn dashboard_handler() -> impl IntoResponse {
    Html(include_str!("dashboard.html"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tc_otel_ads::ConnectionConfig;
    use tower::ServiceExt;

    fn test_state() -> Arc<WebState> {
        Arc::new(WebState {
            stats: DiagnosticStats::new(),
            settings: AppSettings {
                logging: tc_otel_core::LoggingConfig {
                    log_level: "info".to_string(),
                    format: tc_otel_core::LogFormat::Text,
                    output_path: None,
                },
                receiver: tc_otel_core::ReceiverConfig::default(),
                export: tc_otel_core::ExportConfig::default(),
                outputs: vec![],
                service: tc_otel_core::ServiceConfig::default(),
                web: tc_otel_core::WebConfig::default(),
            },
            conn_manager: Arc::new(ConnectionManager::new(ConnectionConfig::default())),
            registry: Arc::new(TaskRegistry::new()),
            subscriptions: Arc::new(SubscriptionManager::new(500)),
        })
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_status_endpoint() {
        let state = test_state();
        state.stats.inc_logs_received();
        state.stats.inc_logs_received();

        let app = build_router(state);
        let response = app
            .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["logs_received"], 2);
        assert_eq!(json["service_name"], "tc-otel");
    }

    #[tokio::test]
    async fn test_config_endpoint() {
        let state = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ams_tcp_port"], 48898);
    }

    #[tokio::test]
    async fn test_connections_endpoint() {
        let state = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::get("/api/connections")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], 0);
    }

    #[tokio::test]
    async fn test_tasks_endpoint() {
        let state = test_state();
        state.registry.register(
            tc_otel_ads::RegistrationKey {
                ams_net_id: "1.2.3.4.1.1".to_string(),
                ams_source_port: 851,
                task_index: 0,
            },
            tc_otel_ads::TaskMetadata {
                task_name: "PlcTask".to_string(),
                app_name: "MainApp".to_string(),
                project_name: "Project1".to_string(),
                online_change_count: 1,
            },
        );

        let app = build_router(state);
        let response = app
            .oneshot(Request::get("/api/tasks").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["task_name"], "PlcTask");
    }

    #[tokio::test]
    async fn test_subscriptions_crud() {
        let state = test_state();
        let app = build_router(state);

        // List (empty)
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/subscriptions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 0);

        // Add
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/subscriptions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"tag":"GVL.sensor_temp"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 1);

        // Remove
        let response = app
            .clone()
            .oneshot(
                Request::delete("/api/subscriptions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"tag":"GVL.sensor_temp"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 0);
    }

    #[tokio::test]
    async fn test_subscription_limit_returns_conflict() {
        let state = Arc::new(WebState {
            stats: DiagnosticStats::new(),
            settings: AppSettings {
                logging: tc_otel_core::LoggingConfig {
                    log_level: "info".to_string(),
                    format: tc_otel_core::LogFormat::Text,
                    output_path: None,
                },
                receiver: tc_otel_core::ReceiverConfig::default(),
                export: tc_otel_core::ExportConfig::default(),
                outputs: vec![],
                service: tc_otel_core::ServiceConfig::default(),
                web: tc_otel_core::WebConfig::default(),
            },
            conn_manager: Arc::new(ConnectionManager::new(ConnectionConfig::default())),
            registry: Arc::new(TaskRegistry::new()),
            subscriptions: Arc::new(SubscriptionManager::new(1)), // limit = 1
        });

        let app = build_router(state);

        // First subscribe succeeds
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/subscriptions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"tag":"tag1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Second subscribe fails with conflict
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/subscriptions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"tag":"tag2"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_dashboard_returns_html() {
        let state = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("tc-otel"));
    }
}
