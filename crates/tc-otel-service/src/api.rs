//! REST API for PLC symbol browsing
//!
//! Exposes endpoints for the Web UI to discover and query PLC symbols.
//! The actual ADS communication is handled by AdsClient in tc-otel-ads.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tc_otel_ads::{AdsClient, AdsSymbolEntry, MAX_SUBSCRIPTIONS_PER_PLC};
use tokio::sync::RwLock;

/// Shared application state for the API.
#[derive(Clone)]
pub struct ApiState {
    /// ADS client for PLC communication (None if not configured)
    pub ads_client: Option<Arc<AdsClient>>,
    /// Currently selected symbols for monitoring (names)
    pub selected_symbols: Arc<RwLock<Vec<String>>>,
}

impl ApiState {
    pub fn new(ads_client: Option<Arc<AdsClient>>) -> Self {
        Self {
            ads_client,
            selected_symbols: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

/// Build the API router with all symbol browsing endpoints.
pub fn symbol_router(state: ApiState) -> Router {
    Router::new()
        .route("/api/plc/symbols", get(list_symbols))
        .route("/api/plc/symbols/:name", get(get_symbol))
        .route("/api/plc/browse", post(browse_symbols))
        .route("/api/plc/subscriptions", get(list_subscriptions))
        .route("/api/plc/subscriptions", post(update_subscriptions))
        .with_state(state)
}

// --- Request/Response types ---

#[derive(Debug, Serialize, Deserialize)]
struct SymbolListResponse {
    symbols: Vec<AdsSymbolEntry>,
    total: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct BrowseResponse {
    symbol_count: usize,
    message: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Deserialize)]
struct SymbolSearchQuery {
    prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionRequest {
    symbols: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubscriptionResponse {
    selected: Vec<String>,
    count: usize,
    max: usize,
}

// --- Handlers ---

/// GET /api/plc/symbols?prefix=MAIN.
///
/// List all discovered symbols, optionally filtered by name prefix.
async fn list_symbols(
    State(state): State<ApiState>,
    Query(query): Query<SymbolSearchQuery>,
) -> impl IntoResponse {
    let client = match &state.ads_client {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(SymbolListResponse {
                    symbols: vec![],
                    total: 0,
                }),
            )
                .into_response()
        }
    };

    let cached = client.cached_symbols().await;
    match cached {
        Some(table) => {
            let symbols: Vec<AdsSymbolEntry> = match &query.prefix {
                Some(prefix) => table.search(prefix).into_iter().cloned().collect(),
                None => table.entries().to_vec(),
            };
            let total = symbols.len();
            Json(SymbolListResponse { symbols, total }).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "No symbol table cached. POST /api/plc/browse to refresh.".to_string(),
            }),
        )
            .into_response(),
    }
}

/// GET /api/plc/symbols/:name
///
/// Get a specific symbol by exact name.
async fn get_symbol(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let client = match &state.ads_client {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "ADS client not configured".to_string(),
                }),
            )
                .into_response()
        }
    };

    let cached = client.cached_symbols().await;
    match cached {
        Some(table) => match table.get(&name) {
            Some(entry) => Json(entry.clone()).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Symbol '{}' not found", name),
                }),
            )
                .into_response(),
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "No symbol table cached. POST /api/plc/browse to refresh.".to_string(),
            }),
        )
            .into_response(),
    }
}

/// POST /api/plc/browse
///
/// Trigger a symbol table refresh by connecting to the PLC and reading its symbol table.
async fn browse_symbols(State(state): State<ApiState>) -> impl IntoResponse {
    let client = match &state.ads_client {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "ADS client not configured".to_string(),
                }),
            )
                .into_response()
        }
    };

    match client.browse_symbols().await {
        Ok(table) => {
            let count = table.len();
            Json(BrowseResponse {
                symbol_count: count,
                message: format!("Successfully discovered {} symbols", count),
            })
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("Failed to browse PLC symbols: {}", e),
            }),
        )
            .into_response(),
    }
}

/// GET /api/plc/subscriptions
///
/// List currently selected symbols for monitoring.
async fn list_subscriptions(State(state): State<ApiState>) -> impl IntoResponse {
    let selected = state.selected_symbols.read().await;
    Json(SubscriptionResponse {
        count: selected.len(),
        selected: selected.clone(),
        max: MAX_SUBSCRIPTIONS_PER_PLC,
    })
}

/// POST /api/plc/subscriptions
///
/// Update the list of selected symbols for monitoring.
/// Enforces the 500 tag subscription limit per PLC.
async fn update_subscriptions(
    State(state): State<ApiState>,
    Json(request): Json<SubscriptionRequest>,
) -> impl IntoResponse {
    if request.symbols.len() > MAX_SUBSCRIPTIONS_PER_PLC {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!(
                    "Too many subscriptions: {} requested, max {} per PLC",
                    request.symbols.len(),
                    MAX_SUBSCRIPTIONS_PER_PLC
                ),
            }),
        )
            .into_response();
    }

    let mut selected = state.selected_symbols.write().await;
    *selected = request.symbols;

    Json(SubscriptionResponse {
        count: selected.len(),
        selected: selected.clone(),
        max: MAX_SUBSCRIPTIONS_PER_PLC,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http::header;
    use tc_otel_ads::AdsClientConfig;
    use tower::ServiceExt;

    fn test_state() -> ApiState {
        ApiState::new(None)
    }

    #[tokio::test]
    async fn test_list_symbols_no_client() {
        let app = symbol_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/plc/symbols")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_get_symbol_no_client() {
        let app = symbol_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/plc/symbols/MAIN.x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_browse_no_client() {
        let app = symbol_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/plc/browse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_list_subscriptions_empty() {
        let app = symbol_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/plc/subscriptions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: SubscriptionResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.count, 0);
        assert_eq!(resp.max, 500);
        assert!(resp.selected.is_empty());
    }

    #[tokio::test]
    async fn test_update_subscriptions() {
        let state = test_state();
        let app = symbol_router(state.clone());

        let request_body = serde_json::json!({
            "symbols": ["MAIN.nCounter", "MAIN.fTemp"]
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/plc/subscriptions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: SubscriptionResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.count, 2);
        assert_eq!(resp.selected, vec!["MAIN.nCounter", "MAIN.fTemp"]);
    }

    #[tokio::test]
    async fn test_update_subscriptions_exceeds_limit() {
        let app = symbol_router(test_state());

        let too_many: Vec<String> = (0..501).map(|i| format!("SYM.var{}", i)).collect();
        let request_body = serde_json::json!({ "symbols": too_many });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/plc/subscriptions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_subscriptions_at_limit() {
        let app = symbol_router(test_state());

        let at_limit: Vec<String> = (0..500).map(|i| format!("SYM.var{}", i)).collect();
        let request_body = serde_json::json!({ "symbols": at_limit });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/plc/subscriptions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: SubscriptionResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.count, 500);
    }

    /// Integration test: browse + list + get with a mock PLC
    #[tokio::test]
    async fn test_browse_and_query_with_mock_plc() {
        use std::str::FromStr;
        use tc_otel_ads::ams::{
            AmsHeader, AmsNetId, AmsTcpFrame, AmsTcpHeader, ADS_STATE_RESPONSE,
        };
        use tc_otel_ads::symbol::{
            AdsReadResponse, AdsSymbolEntry, SymbolUploadInfo, ADSIGRP_SYM_UPLOAD,
            ADSIGRP_SYM_UPLOADINFO,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let entries = vec![
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 0,
                size: 4,
                data_type: 3,
                flags: 8,
                name: "MAIN.nCounter".to_string(),
                type_name: "INT".to_string(),
                comment: "Cycle counter".to_string(),
            },
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 4,
                size: 4,
                data_type: 4,
                flags: 8,
                name: "MAIN.fTemp".to_string(),
                type_name: "REAL".to_string(),
                comment: "".to_string(),
            },
        ];

        let total_length: u32 = entries.iter().map(|e| e.serialize().len() as u32).sum();
        let upload_info = SymbolUploadInfo {
            symbol_count: 2,
            symbol_length: total_length,
            data_type_count: 0,
            data_type_length: 0,
            extra_count: 0,
            extra_length: 0,
        };

        // Spawn mock PLC
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let plc_addr = listener.local_addr().unwrap();
        let entries_clone = entries.clone();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            loop {
                let mut tcp_hdr = [0u8; 6];
                if stream.read_exact(&mut tcp_hdr).await.is_err() {
                    break;
                }
                let tcp_header = AmsTcpHeader::parse(&tcp_hdr).unwrap();
                let mut ams_data = vec![0u8; tcp_header.data_length as usize];
                if stream.read_exact(&mut ams_data).await.is_err() {
                    break;
                }
                let ams_header = AmsHeader::parse(&ams_data[..32]).unwrap();
                let read_req =
                    tc_otel_ads::symbol::AdsReadRequest::parse(&ams_data[32..]).unwrap();

                let response_data = if read_req.index_group == ADSIGRP_SYM_UPLOADINFO {
                    AdsReadResponse {
                        result: 0,
                        data: upload_info.serialize(),
                    }
                    .serialize()
                } else if read_req.index_group == ADSIGRP_SYM_UPLOAD {
                    let mut table_data = Vec::new();
                    for e in &entries_clone {
                        table_data.extend(e.serialize());
                    }
                    AdsReadResponse {
                        result: 0,
                        data: table_data,
                    }
                    .serialize()
                } else {
                    AdsReadResponse {
                        result: 0x0706,
                        data: vec![],
                    }
                    .serialize()
                };

                let resp_frame = AmsTcpFrame {
                    tcp_header: AmsTcpHeader {
                        reserved: 0,
                        data_length: 32 + response_data.len() as u32,
                    },
                    ams_header: AmsHeader {
                        target_net_id: ams_header.source_net_id,
                        target_port: ams_header.source_port,
                        source_net_id: ams_header.target_net_id,
                        source_port: ams_header.target_port,
                        command_id: ams_header.command_id,
                        state_flags: ADS_STATE_RESPONSE,
                        data_length: response_data.len() as u32,
                        error_code: 0,
                        invoke_id: ams_header.invoke_id,
                    },
                    payload: response_data,
                };
                if stream.write_all(&resp_frame.serialize()).await.is_err() {
                    break;
                }
            }
        });

        // Create ADS client pointing at mock PLC
        let config = AdsClientConfig::new(
            plc_addr,
            AmsNetId::from_str("10.0.0.1.1.1").unwrap(),
            AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
            851,
        );
        let client = Arc::new(tc_otel_ads::AdsClient::new(config));
        let state = ApiState::new(Some(client));
        let app = symbol_router(state);

        // 1. Browse (triggers PLC connection)
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/plc/browse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let browse_resp: BrowseResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(browse_resp.symbol_count, 2);

        // 2. List all symbols
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/plc/symbols")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list_resp: SymbolListResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(list_resp.total, 2);

        // 3. Get specific symbol
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/plc/symbols/MAIN.nCounter")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let symbol: AdsSymbolEntry = serde_json::from_slice(&body).unwrap();
        assert_eq!(symbol.name, "MAIN.nCounter");
        assert_eq!(symbol.type_name, "INT");

        // 4. Search by prefix
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/plc/symbols?prefix=MAIN.")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let search_resp: SymbolListResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(search_resp.total, 2);
    }
}
