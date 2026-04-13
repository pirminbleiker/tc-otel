//! OTEL receiver endpoints for HTTP and gRPC

use crate::error::*;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use tc_otel_core::LogEntry;
use tokio::sync::mpsc;
use tower_http::cors::CorsLayer;

/// OTEL HTTP Receiver for HTTP/JSON OTLP endpoint
pub struct OtelHttpReceiver {
    host: String,
    port: u16,
    log_tx: mpsc::Sender<LogEntry>,
}

impl OtelHttpReceiver {
    pub fn new(host: String, port: u16, log_tx: mpsc::Sender<LogEntry>) -> Self {
        Self { host, port, log_tx }
    }

    /// Start the HTTP receiver
    pub async fn start(&self) -> Result<()> {
        let log_tx = self.log_tx.clone();
        let router = Router::new()
            .route("/v1/logs", post(handle_logs_request))
            .layer(CorsLayer::permissive())
            .with_state(log_tx);

        let addr = format!("{}:{}", self.host, self.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| OtelError::ReceiverError(e.to_string()))?;

        tracing::info!("OTEL HTTP receiver listening on {}", addr);

        axum::serve(listener, router)
            .await
            .map_err(|e| OtelError::ReceiverError(e.to_string()))?;

        Ok(())
    }
}

/// OTEL gRPC Receiver for OTLP/gRPC endpoint
pub struct OtelGrpcReceiver {
    host: String,
    port: u16,
    log_tx: mpsc::Sender<LogEntry>,
}

impl OtelGrpcReceiver {
    pub fn new(host: String, port: u16, log_tx: mpsc::Sender<LogEntry>) -> Self {
        Self { host, port, log_tx }
    }

    /// Start the gRPC receiver
    pub async fn start(&self) -> Result<()> {
        let svc = crate::grpc::LogsServiceImpl::new(self.log_tx.clone());
        let server = crate::grpc::LogsServiceServer::new(svc);

        let addr = format!("{}:{}", self.host, self.port)
            .parse()
            .map_err(|e| OtelError::ReceiverError(format!("Invalid address: {e}")))?;

        tracing::info!("OTEL gRPC receiver listening on {}", addr);

        tonic::transport::Server::builder()
            .add_service(server)
            .serve(addr)
            .await
            .map_err(|e| OtelError::ReceiverError(e.to_string()))?;

        Ok(())
    }
}

/// Handle incoming OTEL logs request
async fn handle_logs_request(
    State(log_tx): State<mpsc::Sender<LogEntry>>,
    Json(_payload): Json<serde_json::Value>,
) -> (StatusCode, String) {
    // TODO: Parse OTEL LogsData format
    // For now, accept and acknowledge
    match log_tx.try_send(LogEntry::new(
        "otel".to_string(),
        "unknown".to_string(),
        "test".to_string(),
        "default".to_string(),
        tc_otel_core::LogLevel::Info,
    )) {
        Ok(_) => (StatusCode::OK, "Received".to_string()),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Channel full".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_otel_http_receiver_creation() {
        let (tx, _rx) = mpsc::channel(100);
        let receiver = OtelHttpReceiver::new("127.0.0.1".to_string(), 4318, tx);
        assert_eq!(receiver.port, 4318);
        assert_eq!(receiver.host, "127.0.0.1");
    }

    #[test]
    fn test_otel_http_receiver_custom_host() {
        let (tx, _rx) = mpsc::channel(100);
        let receiver = OtelHttpReceiver::new("0.0.0.0".to_string(), 8080, tx);
        assert_eq!(receiver.host, "0.0.0.0");
        assert_eq!(receiver.port, 8080);
    }

    #[test]
    fn test_otel_grpc_receiver_creation() {
        let (tx, _rx) = mpsc::channel(100);
        let receiver = OtelGrpcReceiver::new("127.0.0.1".to_string(), 4317, tx);
        assert_eq!(receiver.port, 4317);
        assert_eq!(receiver.host, "127.0.0.1");
    }

    #[test]
    fn test_otel_grpc_receiver_config() {
        let (tx, _rx) = mpsc::channel(100);
        let receiver = OtelGrpcReceiver::new("localhost".to_string(), 9090, tx);
        assert_eq!(receiver.host, "localhost");
        assert_eq!(receiver.port, 9090);
    }

    #[test]
    fn test_multiple_receivers_same_channel() {
        let (tx, _rx) = mpsc::channel(100);

        let http_receiver = OtelHttpReceiver::new("127.0.0.1".to_string(), 4318, tx.clone());
        let grpc_receiver = OtelGrpcReceiver::new("127.0.0.1".to_string(), 4317, tx.clone());

        assert_eq!(http_receiver.port, 4318);
        assert_eq!(grpc_receiver.port, 4317);
    }
}
