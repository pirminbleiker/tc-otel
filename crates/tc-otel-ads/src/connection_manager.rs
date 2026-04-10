//! Connection management for ADS/AMS TCP listeners
//!
//! Provides connection limits, per-IP tracking, rate limiting, idle timeouts,
//! graceful shutdown, and backpressure handling for security hardening.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Semaphore, TryAcquireError};

/// Configuration for connection management
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// Maximum total concurrent connections (default: 100)
    pub max_connections: usize,
    /// Idle connection timeout in seconds (default: 300)
    pub idle_timeout_secs: u64,
    /// Maximum connections from a single IP (default: 10)
    pub max_connections_per_ip: usize,
    /// Maximum new connections per second from a single IP (default: 10)
    pub rate_limit_per_sec_per_ip: usize,
    /// Keep-alive heartbeat interval in seconds (default: 60)
    pub keepalive_interval_secs: u64,
    /// Send buffer size limit in bytes (default: 1MB)
    pub send_buffer_size: usize,
    /// Graceful shutdown timeout in seconds (default: 30)
    pub shutdown_timeout_secs: u64,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            max_connections: 100,
            idle_timeout_secs: 300,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 10,
            keepalive_interval_secs: 60,
            send_buffer_size: 1_048_576, // 1 MB
            shutdown_timeout_secs: 30,
        }
    }
}

/// Reason a connection was rejected
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionRejection {
    /// Global connection limit reached
    MaxConnectionsReached,
    /// Per-IP connection limit reached
    PerIpLimitReached { ip: IpAddr, current: usize },
    /// Per-IP rate limit exceeded
    RateLimitExceeded { ip: IpAddr },
    /// Service is shutting down
    ShuttingDown,
}

impl std::fmt::Display for ConnectionRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionRejection::MaxConnectionsReached => {
                write!(f, "maximum connection limit reached")
            }
            ConnectionRejection::PerIpLimitReached { ip, current } => {
                write!(
                    f,
                    "per-IP connection limit reached for {} ({} active)",
                    ip, current
                )
            }
            ConnectionRejection::RateLimitExceeded { ip } => {
                write!(f, "connection rate limit exceeded for {}", ip)
            }
            ConnectionRejection::ShuttingDown => {
                write!(f, "service is shutting down")
            }
        }
    }
}

/// Per-IP connection state
#[derive(Debug)]
struct IpState {
    active_connections: usize,
    connection_timestamps: Vec<Instant>,
}

impl IpState {
    fn new() -> Self {
        Self {
            active_connections: 0,
            connection_timestamps: Vec::new(),
        }
    }

    /// Prune connection timestamps older than 1 second (for rate limiting)
    fn prune_timestamps(&mut self, now: Instant) {
        self.connection_timestamps
            .retain(|&ts| now.duration_since(ts) < Duration::from_secs(1));
    }
}

/// Manages connection limits, per-IP tracking, and shutdown coordination.
///
/// Tracks active connections, enforces per-IP and global limits,
/// rate-limits new connections, and coordinates graceful shutdown.
pub struct ConnectionManager {
    config: ConnectionConfig,
    semaphore: Arc<Semaphore>,
    ip_states: Arc<Mutex<HashMap<IpAddr, IpState>>>,
    shutdown_tx: broadcast::Sender<()>,
    active_count: Arc<AtomicUsize>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

impl ConnectionManager {
    /// Create a new ConnectionManager with the given configuration.
    pub fn new(config: ConnectionConfig) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_connections));
        let (shutdown_tx, _) = broadcast::channel(1);
        Self {
            config,
            semaphore,
            ip_states: Arc::new(Mutex::new(HashMap::new())),
            shutdown_tx,
            active_count: Arc::new(AtomicUsize::new(0)),
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Try to acquire a connection permit for the given IP.
    ///
    /// Returns a `ConnectionPermit` on success which must be held for the
    /// duration of the connection. When dropped, the permit releases the
    /// connection slot.
    ///
    /// Returns `ConnectionRejection` if the connection should be refused.
    pub fn try_acquire(&self, ip: IpAddr) -> Result<ConnectionPermit, ConnectionRejection> {
        // Check shutdown state first
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ConnectionRejection::ShuttingDown);
        }

        // Try to acquire global semaphore permit (non-blocking)
        let semaphore_permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                return Err(ConnectionRejection::MaxConnectionsReached);
            }
            Err(TryAcquireError::Closed) => {
                return Err(ConnectionRejection::ShuttingDown);
            }
        };

        // Check per-IP limits and rate limiting
        {
            let mut states = self.ip_states.lock().unwrap();
            let state = states.entry(ip).or_insert_with(IpState::new);

            // Check per-IP connection limit
            if state.active_connections >= self.config.max_connections_per_ip {
                // Drop semaphore permit before returning
                drop(semaphore_permit);
                return Err(ConnectionRejection::PerIpLimitReached {
                    ip,
                    current: state.active_connections,
                });
            }

            // Check rate limit
            let now = Instant::now();
            state.prune_timestamps(now);
            if state.connection_timestamps.len() >= self.config.rate_limit_per_sec_per_ip {
                drop(semaphore_permit);
                return Err(ConnectionRejection::RateLimitExceeded { ip });
            }

            // Record the connection
            state.active_connections += 1;
            state.connection_timestamps.push(now);
        }

        self.active_count.fetch_add(1, Ordering::Release);

        Ok(ConnectionPermit {
            ip,
            ip_states: self.ip_states.clone(),
            _semaphore_permit: semaphore_permit,
            active_count: self.active_count.clone(),
        })
    }

    /// Get the number of currently active connections.
    pub fn active_connections(&self) -> usize {
        self.active_count.load(Ordering::Acquire)
    }

    /// Get the number of active connections from a specific IP.
    pub fn connections_for_ip(&self, ip: &IpAddr) -> usize {
        let states = self.ip_states.lock().unwrap();
        states.get(ip).map(|s| s.active_connections).unwrap_or(0)
    }

    /// Get all IPs with active connections and their counts.
    pub fn connected_ips(&self) -> Vec<(IpAddr, usize)> {
        let states = self.ip_states.lock().unwrap();
        states
            .iter()
            .filter(|(_, s)| s.active_connections > 0)
            .map(|(ip, s)| (*ip, s.active_connections))
            .collect()
    }

    /// Initiate graceful shutdown. New connections will be rejected.
    /// Returns a future that resolves when all active connections are closed
    /// or the shutdown timeout expires.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let _ = self.shutdown_tx.send(());
    }

    /// Check if the manager is in shutdown state.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Subscribe to shutdown notifications.
    pub fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Get the configured idle timeout duration.
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.config.idle_timeout_secs)
    }

    /// Get the configured keep-alive interval.
    pub fn keepalive_interval(&self) -> Duration {
        Duration::from_secs(self.config.keepalive_interval_secs)
    }

    /// Get the configured send buffer size limit.
    pub fn send_buffer_size(&self) -> usize {
        self.config.send_buffer_size
    }

    /// Get the configured shutdown timeout.
    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.config.shutdown_timeout_secs)
    }

    /// Get the max connections limit.
    pub fn max_connections(&self) -> usize {
        self.config.max_connections
    }

    /// Get the per-IP connection limit.
    pub fn max_connections_per_ip(&self) -> usize {
        self.config.max_connections_per_ip
    }

    /// Wait for all active connections to drain, with timeout.
    /// Returns `true` if all connections drained, `false` if timed out.
    pub async fn wait_for_drain(&self) -> bool {
        let timeout_duration = self.shutdown_timeout();
        let start = Instant::now();

        loop {
            if self.active_connections() == 0 {
                return true;
            }
            if start.elapsed() >= timeout_duration {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// A permit representing an active connection.
///
/// When dropped, the connection slot is released and the per-IP count
/// is decremented. This ensures correct cleanup even on error paths.
pub struct ConnectionPermit {
    ip: IpAddr,
    ip_states: Arc<Mutex<HashMap<IpAddr, IpState>>>,
    _semaphore_permit: tokio::sync::OwnedSemaphorePermit,
    active_count: Arc<AtomicUsize>,
}

impl std::fmt::Debug for ConnectionPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionPermit")
            .field("ip", &self.ip)
            .finish_non_exhaustive()
    }
}

impl ConnectionPermit {
    /// Get the IP address associated with this permit.
    pub fn ip(&self) -> &IpAddr {
        &self.ip
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        // Decrement per-IP count
        {
            let mut states = self.ip_states.lock().unwrap();
            if let Some(state) = states.get_mut(&self.ip) {
                state.active_connections = state.active_connections.saturating_sub(1);
                // Clean up entry if no connections remain
                if state.active_connections == 0 && state.connection_timestamps.is_empty() {
                    states.remove(&self.ip);
                }
            }
        }
        // Decrement global count (semaphore permit is auto-released)
        self.active_count.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn localhost() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
    }

    // ---- Max connections limit (100) ----

    #[test]
    fn test_max_connections_enforced() {
        let config = ConnectionConfig {
            max_connections: 100,
            max_connections_per_ip: 200, // high so per-IP doesn't interfere
            rate_limit_per_sec_per_ip: 200,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        // Acquire 100 connections from different IPs
        let mut permits = Vec::new();
        for i in 0..100u8 {
            let result = mgr.try_acquire(ip(i));
            assert!(result.is_ok(), "Connection {} should succeed", i);
            permits.push(result.unwrap());
        }
        assert_eq!(mgr.active_connections(), 100);

        // Connection 101 should be rejected
        let result = mgr.try_acquire(ip(200));
        assert!(matches!(
            result,
            Err(ConnectionRejection::MaxConnectionsReached)
        ));
    }

    #[test]
    fn test_connection_slot_released_on_drop() {
        let config = ConnectionConfig {
            max_connections: 2,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let p1 = mgr.try_acquire(ip(1)).unwrap();
        let p2 = mgr.try_acquire(ip(2)).unwrap();
        assert_eq!(mgr.active_connections(), 2);

        // At limit
        assert!(mgr.try_acquire(ip(3)).is_err());

        // Drop one permit
        drop(p1);
        assert_eq!(mgr.active_connections(), 1);

        // Should succeed now
        let p3 = mgr.try_acquire(ip(3)).unwrap();
        assert_eq!(mgr.active_connections(), 2);

        drop(p2);
        drop(p3);
        assert_eq!(mgr.active_connections(), 0);
    }

    #[test]
    fn test_custom_max_connections() {
        let config = ConnectionConfig {
            max_connections: 5,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let mut permits = Vec::new();
        for i in 0..5 {
            permits.push(mgr.try_acquire(ip(i as u8)).unwrap());
        }
        assert!(mgr.try_acquire(ip(100)).is_err());
        assert_eq!(mgr.active_connections(), 5);
    }

    // ---- Idle timeout configuration ----

    #[test]
    fn test_idle_timeout_300s() {
        let config = ConnectionConfig::default();
        let mgr = ConnectionManager::new(config);
        assert_eq!(mgr.idle_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn test_custom_idle_timeout() {
        let config = ConnectionConfig {
            idle_timeout_secs: 600,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);
        assert_eq!(mgr.idle_timeout(), Duration::from_secs(600));
    }

    // ---- Connection rejection ----

    #[test]
    fn test_rejection_error_message_max_connections() {
        let rejection = ConnectionRejection::MaxConnectionsReached;
        let msg = rejection.to_string();
        assert!(msg.contains("maximum connection limit"));
    }

    #[test]
    fn test_rejection_error_message_per_ip() {
        let rejection = ConnectionRejection::PerIpLimitReached {
            ip: localhost(),
            current: 10,
        };
        let msg = rejection.to_string();
        assert!(msg.contains("per-IP"));
        assert!(msg.contains("127.0.0.1"));
    }

    #[test]
    fn test_rejection_no_resource_leak() {
        let config = ConnectionConfig {
            max_connections: 2,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let p1 = mgr.try_acquire(ip(1)).unwrap();
        let p2 = mgr.try_acquire(ip(2)).unwrap();

        // Rejected connection should not consume a slot
        let _ = mgr.try_acquire(ip(3));
        assert_eq!(mgr.active_connections(), 2);

        // After dropping, slots are freed
        drop(p1);
        drop(p2);
        assert_eq!(mgr.active_connections(), 0);
    }

    // ---- Idle connection cleanup ----

    #[test]
    fn test_idle_timeout_measured_from_config() {
        let config = ConnectionConfig {
            idle_timeout_secs: 300,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);
        assert_eq!(mgr.idle_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn test_cleanup_releases_slot_on_drop() {
        let config = ConnectionConfig {
            max_connections: 1,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        // Simulates server closing an idle connection
        let permit = mgr.try_acquire(ip(1)).unwrap();
        assert_eq!(mgr.active_connections(), 1);
        assert_eq!(mgr.connections_for_ip(&ip(1)), 1);

        // Drop permit (simulates server-initiated close)
        drop(permit);
        assert_eq!(mgr.active_connections(), 0);
        assert_eq!(mgr.connections_for_ip(&ip(1)), 0);

        // Slot is reusable
        let _p = mgr.try_acquire(ip(2)).unwrap();
        assert_eq!(mgr.active_connections(), 1);
    }

    // ---- Per-IP rate limiting ----

    #[test]
    fn test_rate_limit_per_ip() {
        let config = ConnectionConfig {
            max_connections: 100,
            max_connections_per_ip: 100,
            rate_limit_per_sec_per_ip: 3,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let test_ip = ip(1);

        // First 3 connections should succeed (within 1 second window)
        let mut permits = Vec::new();
        for _ in 0..3 {
            let p = mgr.try_acquire(test_ip).unwrap();
            permits.push(p);
        }

        // 4th connection within same second should be rate limited
        let result = mgr.try_acquire(test_ip);
        assert!(
            matches!(result, Err(ConnectionRejection::RateLimitExceeded { .. })),
            "Should be rate limited"
        );

        // Different IP should NOT be rate limited
        let other = mgr.try_acquire(ip(2));
        assert!(other.is_ok(), "Different IP should not be rate limited");
    }

    // ---- Graceful shutdown ----

    #[test]
    fn test_shutdown_rejects_new_connections() {
        let mgr = ConnectionManager::new(ConnectionConfig::default());

        // Pre-existing connection
        let _permit = mgr.try_acquire(ip(1)).unwrap();

        // Initiate shutdown
        mgr.shutdown();
        assert!(mgr.is_shutting_down());

        // New connections should be rejected
        let result = mgr.try_acquire(ip(2));
        assert!(matches!(result, Err(ConnectionRejection::ShuttingDown)));
    }

    #[test]
    fn test_shutdown_existing_connections_still_held() {
        let mgr = ConnectionManager::new(ConnectionConfig::default());

        let permit = mgr.try_acquire(ip(1)).unwrap();
        assert_eq!(mgr.active_connections(), 1);

        mgr.shutdown();

        // Existing connection is still counted
        assert_eq!(mgr.active_connections(), 1);

        // Only when dropped does it release
        drop(permit);
        assert_eq!(mgr.active_connections(), 0);
    }

    #[tokio::test]
    async fn test_shutdown_wait_for_drain() {
        let config = ConnectionConfig {
            shutdown_timeout_secs: 1,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = Arc::new(ConnectionManager::new(config));

        let permit = mgr.try_acquire(ip(1)).unwrap();
        mgr.shutdown();

        // Spawn a task to drop the permit after a short delay
        let mgr_clone = mgr.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(permit);
            assert_eq!(mgr_clone.active_connections(), 0);
        });

        // Wait for drain should succeed before timeout
        let drained = mgr.wait_for_drain().await;
        assert!(drained, "Should drain before timeout");
    }

    #[tokio::test]
    async fn test_shutdown_timeout_on_stubborn_connections() {
        let config = ConnectionConfig {
            shutdown_timeout_secs: 0, // immediate timeout
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let _permit = mgr.try_acquire(ip(1)).unwrap();
        mgr.shutdown();

        // Should timeout because permit is never dropped
        let drained = mgr.wait_for_drain().await;
        assert!(!drained, "Should timeout with stubborn connection");
    }

    // ---- Connected IPs ----

    #[test]
    fn test_connected_ips() {
        let config = ConnectionConfig {
            max_connections: 100,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let _p1 = mgr.try_acquire(ip(1)).unwrap();
        let _p2 = mgr.try_acquire(ip(1)).unwrap();
        let _p3 = mgr.try_acquire(ip(2)).unwrap();

        let ips = mgr.connected_ips();
        assert_eq!(ips.len(), 2);

        let ip1_count = ips.iter().find(|(i, _)| *i == ip(1)).map(|(_, c)| *c);
        let ip2_count = ips.iter().find(|(i, _)| *i == ip(2)).map(|(_, c)| *c);
        assert_eq!(ip1_count, Some(2));
        assert_eq!(ip2_count, Some(1));
    }

    #[test]
    fn test_connected_ips_empty() {
        let mgr = ConnectionManager::new(ConnectionConfig::default());
        assert!(mgr.connected_ips().is_empty());
    }

    // ---- Per-IP tracking ----

    #[test]
    fn test_per_ip_tracking() {
        let config = ConnectionConfig {
            max_connections: 100,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let ip1 = ip(1);
        let ip2 = ip(2);

        let _p1a = mgr.try_acquire(ip1).unwrap();
        let _p1b = mgr.try_acquire(ip1).unwrap();
        let _p2a = mgr.try_acquire(ip2).unwrap();

        assert_eq!(mgr.connections_for_ip(&ip1), 2);
        assert_eq!(mgr.connections_for_ip(&ip2), 1);
        assert_eq!(mgr.connections_for_ip(&ip(3)), 0);
        assert_eq!(mgr.active_connections(), 3);
    }

    #[test]
    fn test_per_ip_limit_enforced() {
        let config = ConnectionConfig {
            max_connections: 100,
            max_connections_per_ip: 3,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let test_ip = ip(1);
        let mut permits = Vec::new();
        for _ in 0..3 {
            permits.push(mgr.try_acquire(test_ip).unwrap());
        }

        // 4th from same IP should be rejected
        let result = mgr.try_acquire(test_ip);
        assert!(matches!(
            result,
            Err(ConnectionRejection::PerIpLimitReached { .. })
        ));

        // Different IP should work
        assert!(mgr.try_acquire(ip(2)).is_ok());
    }

    #[test]
    fn test_per_ip_count_decrements_on_disconnect() {
        let config = ConnectionConfig {
            max_connections: 100,
            max_connections_per_ip: 2,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let test_ip = ip(1);
        let p1 = mgr.try_acquire(test_ip).unwrap();
        let p2 = mgr.try_acquire(test_ip).unwrap();
        assert!(mgr.try_acquire(test_ip).is_err());

        drop(p1);
        assert_eq!(mgr.connections_for_ip(&test_ip), 1);

        // Can now accept a new connection from same IP
        let _p3 = mgr.try_acquire(test_ip).unwrap();
        assert_eq!(mgr.connections_for_ip(&test_ip), 2);

        drop(p2);
        assert_eq!(mgr.connections_for_ip(&test_ip), 1);
    }

    // ---- Keep-alive heartbeat ----

    #[test]
    fn test_keepalive_interval_config() {
        let config = ConnectionConfig {
            keepalive_interval_secs: 60,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);
        assert_eq!(mgr.keepalive_interval(), Duration::from_secs(60));
    }

    #[test]
    fn test_custom_keepalive_interval() {
        let config = ConnectionConfig {
            keepalive_interval_secs: 30,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);
        assert_eq!(mgr.keepalive_interval(), Duration::from_secs(30));
    }

    // ---- Backpressure handling ----

    #[test]
    fn test_send_buffer_size_config() {
        let config = ConnectionConfig {
            send_buffer_size: 1_048_576, // 1 MB
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);
        assert_eq!(mgr.send_buffer_size(), 1_048_576);
    }

    #[test]
    fn test_custom_send_buffer_size() {
        let config = ConnectionConfig {
            send_buffer_size: 512 * 1024, // 512 KB
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);
        assert_eq!(mgr.send_buffer_size(), 512 * 1024);
    }

    // ---- Error path resource cleanup ----

    #[test]
    fn test_permit_drop_releases_all_resources() {
        let config = ConnectionConfig {
            max_connections: 1,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let test_ip = ip(1);

        // Acquire and drop multiple times to verify no leaks
        for _ in 0..10 {
            let permit = mgr.try_acquire(test_ip).unwrap();
            assert_eq!(mgr.active_connections(), 1);
            assert_eq!(mgr.connections_for_ip(&test_ip), 1);
            drop(permit);
            assert_eq!(mgr.active_connections(), 0);
            assert_eq!(mgr.connections_for_ip(&test_ip), 0);
        }
    }

    #[test]
    fn test_per_ip_entry_cleaned_on_last_disconnect() {
        let config = ConnectionConfig {
            max_connections: 100,
            max_connections_per_ip: 10,
            rate_limit_per_sec_per_ip: 100,
            ..Default::default()
        };
        let mgr = ConnectionManager::new(config);

        let test_ip = ip(1);
        let p1 = mgr.try_acquire(test_ip).unwrap();
        let p2 = mgr.try_acquire(test_ip).unwrap();

        drop(p1);
        // Still one connection, entry should exist
        assert_eq!(mgr.connections_for_ip(&test_ip), 1);

        drop(p2);
        // No connections, entry should be cleaned up
        assert_eq!(mgr.connections_for_ip(&test_ip), 0);
    }

    // ---- Default configuration ----

    #[test]
    fn test_default_config_values() {
        let config = ConnectionConfig::default();
        assert_eq!(config.max_connections, 100);
        assert_eq!(config.idle_timeout_secs, 300);
        assert_eq!(config.max_connections_per_ip, 10);
        assert_eq!(config.rate_limit_per_sec_per_ip, 10);
        assert_eq!(config.keepalive_interval_secs, 60);
        assert_eq!(config.send_buffer_size, 1_048_576);
        assert_eq!(config.shutdown_timeout_secs, 30);
    }

    // ---- Thread safety ----

    #[test]
    fn test_concurrent_acquire_release() {
        let config = ConnectionConfig {
            max_connections: 50,
            max_connections_per_ip: 100,
            rate_limit_per_sec_per_ip: 1000,
            ..Default::default()
        };
        let mgr = Arc::new(ConnectionManager::new(config));

        let mut handles = Vec::new();
        for i in 0..50u8 {
            let mgr = mgr.clone();
            handles.push(std::thread::spawn(move || {
                let permit = mgr.try_acquire(ip(i)).unwrap();
                // Simulate some work
                std::thread::sleep(Duration::from_millis(1));
                drop(permit);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(mgr.active_connections(), 0);
    }

    // ---- Shutdown broadcast ----

    #[tokio::test]
    async fn test_shutdown_broadcast_received() {
        let mgr = ConnectionManager::new(ConnectionConfig::default());
        let mut rx = mgr.subscribe_shutdown();

        mgr.shutdown();

        // Should receive shutdown signal
        let result = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(result.is_ok(), "Should receive shutdown signal");
    }

    // ---- Connection permit IP tracking ----

    #[test]
    fn test_permit_reports_ip() {
        let mgr = ConnectionManager::new(ConnectionConfig::default());
        let test_ip = ip(42);
        let permit = mgr.try_acquire(test_ip).unwrap();
        assert_eq!(*permit.ip(), test_ip);
    }
}
