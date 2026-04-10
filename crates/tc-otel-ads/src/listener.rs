//! ADS TCP listener for receiving log entries from TwinCAT PLCs

use crate::error::*;
use crate::parser::AdsParser;
use crate::protocol::AdsLogEntry;
use std::net::SocketAddr;
use std::sync::Arc;
use tc_otel_core::LogEntry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{timeout, Duration};

/// Connection timeout: 5 minutes
const CONNECTION_TIMEOUT_SECS: u64 = 300;

/// Maximum concurrent connections (security: DoS prevention)
const DEFAULT_MAX_CONNECTIONS: usize = 100;

/// ADS TCP listener for accepting connections from TwinCAT PLCs
pub struct AdsListener {
    host: String,
    port: u16,
    log_tx: mpsc::Sender<LogEntry>,
    max_connections: usize,
    connection_semaphore: Arc<Semaphore>,
}

impl AdsListener {
    /// Create a new ADS listener with default max connections
    pub fn new(host: String, port: u16, log_tx: mpsc::Sender<LogEntry>) -> Self {
        Self::with_max_connections(host, port, log_tx, DEFAULT_MAX_CONNECTIONS)
    }

    /// Create a new ADS listener with custom max connections
    pub fn with_max_connections(
        host: String,
        port: u16,
        log_tx: mpsc::Sender<LogEntry>,
        max_connections: usize,
    ) -> Self {
        Self {
            host,
            port,
            log_tx,
            max_connections,
            connection_semaphore: Arc::new(Semaphore::new(max_connections)),
        }
    }

    /// Start listening for incoming ADS connections
    pub async fn start(&self) -> Result<()> {
        let addr = format!("{}:{}", self.host, self.port);
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| AdsError::BufferError(format!("Failed to bind: {}", e)))?;

        tracing::info!(
            "ADS listener started on {} (max {} concurrent connections)",
            addr,
            self.max_connections
        );

        loop {
            let (socket, peer_addr) = listener
                .accept()
                .await
                .map_err(|e| AdsError::BufferError(format!("Accept error: {}", e)))?;

            // Acquire connection permit (respects max_connections limit)
            let semaphore = self.connection_semaphore.clone();
            let permit = semaphore.acquire_owned().await;
            let log_tx = self.log_tx.clone();

            tokio::spawn(async move {
                let _permit = permit.ok(); // Hold permit until connection ends
                if let Err(e) = Self::handle_connection(socket, peer_addr, log_tx).await {
                    tracing::error!("Connection error from {}: {}", peer_addr, e);
                }
            });
        }
    }

    /// Handle a single client connection
    async fn handle_connection(
        mut socket: TcpStream,
        peer_addr: SocketAddr,
        log_tx: mpsc::Sender<LogEntry>,
    ) -> Result<()> {
        tracing::debug!("New connection from {}", peer_addr);

        let mut buffer = vec![0u8; 64 * 1024]; // 64 KB buffer

        loop {
            // Security: Enforce read timeout to prevent slow-read DoS attacks
            let read_result = timeout(
                Duration::from_secs(CONNECTION_TIMEOUT_SECS),
                socket.read(&mut buffer),
            )
            .await;

            let n = match read_result {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    return Err(AdsError::BufferError(format!("Read error: {}", e)));
                }
                Err(_) => {
                    tracing::warn!(
                        "Connection timeout from {} after {} seconds",
                        peer_addr,
                        CONNECTION_TIMEOUT_SECS
                    );
                    break;
                }
            };

            if n == 0 {
                // Connection closed
                tracing::debug!("Connection closed from {}", peer_addr);
                break;
            }

            let message_data = &buffer[..n];

            // Parse ADS log entry
            match AdsParser::parse(message_data) {
                Ok(ads_entry) => {
                    // Convert ADS entry to LogEntry
                    let log_entry = Self::ads_to_log_entry(ads_entry, peer_addr);

                    // Send to dispatcher
                    if let Err(e) = log_tx.try_send(log_entry) {
                        tracing::warn!("Failed to send log entry: {}", e);
                    }

                    // Send acknowledgment
                    if let Err(e) = socket.write_all(&[1]).await {
                        tracing::error!("Failed to send ACK: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse ADS message from {}: {}", peer_addr, e);

                    // Send error response (0 byte)
                    if let Err(e) = socket.write_all(&[0]).await {
                        tracing::error!("Failed to send error response: {}", e);
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Convert an ADS log entry to a core LogEntry
    fn ads_to_log_entry(ads_entry: AdsLogEntry, peer_addr: SocketAddr) -> LogEntry {
        let source = peer_addr.ip().to_string();
        let hostname = format!("plc-{}", peer_addr.port());

        let mut entry = LogEntry::new(
            source,
            hostname,
            ads_entry.message,
            ads_entry.logger,
            ads_entry.level,
        );

        entry.plc_timestamp = ads_entry.plc_timestamp;
        entry.clock_timestamp = ads_entry.clock_timestamp;
        entry.task_index = ads_entry.task_index;
        entry.task_name = ads_entry.task_name;
        entry.task_cycle_counter = ads_entry.task_cycle_counter;
        entry.app_name = ads_entry.app_name;
        entry.project_name = ads_entry.project_name;
        entry.online_change_count = ads_entry.online_change_count;
        entry.arguments = ads_entry.arguments;
        entry.context = ads_entry.context;

        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_ads_to_log_entry_conversion() {
        let ads_entry = AdsLogEntry {
            version: crate::protocol::AdsProtocolVersion::V1,
            message: "Test message".to_string(),
            logger: "test.logger".to_string(),
            level: tc_otel_core::LogLevel::Info,
            plc_timestamp: Utc::now(),
            clock_timestamp: Utc::now(),
            task_index: 1,
            task_name: "Task1".to_string(),
            task_cycle_counter: 100,
            app_name: "TestApp".to_string(),
            project_name: "TestProject".to_string(),
            online_change_count: 0,
            arguments: std::collections::HashMap::new(),
            context: std::collections::HashMap::new(),
        };

        let peer_addr = "192.168.1.100:50123".parse::<SocketAddr>().unwrap();
        let entry = AdsListener::ads_to_log_entry(ads_entry, peer_addr);

        assert_eq!(entry.source, "192.168.1.100");
        assert_eq!(entry.task_index, 1);
        assert_eq!(entry.project_name, "TestProject");
    }
}
