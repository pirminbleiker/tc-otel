//! Security tests for connection limits and timeouts
//!
//! These tests verify the ConnectionManager in tc-otel-ads enforces:
//! - Maximum 100 simultaneous connections
//! - 300s idle connection timeout
//! - Graceful connection rejection
//! - Idle connection cleanup
//! - Per-IP rate limiting
//! - Graceful shutdown
//! - Per-IP connection tracking
//! - Keep-alive heartbeat configuration
//! - Backpressure handling configuration
//! - Error path resource cleanup

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use tc_otel_ads::{ConnectionConfig, ConnectionManager, ConnectionRejection};

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
}

#[test]
fn test_security_connection_limit_max_100() {
    // SECURITY: Maximum 100 simultaneous connections should be enforced
    // This prevents resource exhaustion and DoS attacks
    let config = ConnectionConfig {
        max_connections: 100,
        max_connections_per_ip: 200,
        rate_limit_per_sec_per_ip: 200,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    // 1. Establish connections 1-100 (should succeed)
    let mut permits = Vec::new();
    for i in 0..100u8 {
        let result = mgr.try_acquire(ip(i));
        assert!(result.is_ok(), "Connection {} should succeed", i);
        permits.push(result.unwrap());
    }
    assert_eq!(mgr.active_connections(), 100);

    // 2. Try to establish connection 101 (should be rejected)
    let result = mgr.try_acquire(ip(200));
    assert!(
        matches!(result, Err(ConnectionRejection::MaxConnectionsReached)),
        "Connection 101 should be rejected"
    );

    // 3. Close one connection
    permits.pop();
    assert_eq!(mgr.active_connections(), 99);

    // 4. Establish new connection (should succeed)
    let new_permit = mgr.try_acquire(ip(201));
    assert!(new_permit.is_ok(), "New connection after closing one should succeed");
    assert_eq!(mgr.active_connections(), 100);
}

#[test]
fn test_security_connection_timeout_idle_300s() {
    // SECURITY: Idle connections should be terminated after 300 seconds
    // Prevents resource leaks from abandoned connections
    //
    // The ConnectionManager configures idle_timeout_secs=300 by default.
    // The actual timeout enforcement happens in the listener's read loop
    // via tokio::time::timeout(). Here we verify the configuration.

    let config = ConnectionConfig::default();
    let mgr = ConnectionManager::new(config);

    // Verify default idle timeout is 300 seconds
    assert_eq!(mgr.idle_timeout(), Duration::from_secs(300));

    // Verify custom timeout works
    let custom_config = ConnectionConfig {
        idle_timeout_secs: 600,
        ..Default::default()
    };
    let custom_mgr = ConnectionManager::new(custom_config);
    assert_eq!(custom_mgr.idle_timeout(), Duration::from_secs(600));
}

#[test]
fn test_security_connection_limit_enforcement() {
    // SECURITY: When connection limit is reached, new connections are rejected
    // Error response should be graceful (not panic)
    let config = ConnectionConfig {
        max_connections: 2,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    let _p1 = mgr.try_acquire(ip(1)).unwrap();
    let _p2 = mgr.try_acquire(ip(2)).unwrap();

    // Connection 3 receives clear error
    let result = mgr.try_acquire(ip(3));
    assert!(result.is_err());
    let rejection = result.unwrap_err();
    let msg = rejection.to_string();
    assert!(
        msg.contains("maximum connection limit"),
        "Error should include clear message: got '{}'",
        msg
    );

    // No resource leaks when connection is rejected
    assert_eq!(mgr.active_connections(), 2, "Rejected connection should not consume a slot");
}

#[test]
fn test_security_idle_connection_cleanup() {
    // SECURITY: Idle connections must be cleaned up properly
    // Prevents slow client attacks and socket exhaustion
    let config = ConnectionConfig {
        max_connections: 3,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        idle_timeout_secs: 300,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    // Timeout is measured from configuration
    assert_eq!(mgr.idle_timeout(), Duration::from_secs(300));

    // Server-initiated close (dropping permit) releases resources
    let test_ip = ip(1);
    let permit = mgr.try_acquire(test_ip).unwrap();
    assert_eq!(mgr.connections_for_ip(&test_ip), 1);
    assert_eq!(mgr.active_connections(), 1);

    // Server closes the connection (simulated by dropping permit)
    drop(permit);
    assert_eq!(mgr.connections_for_ip(&test_ip), 0);
    assert_eq!(mgr.active_connections(), 0);

    // Slot is reusable after cleanup
    let _new_permit = mgr.try_acquire(test_ip).unwrap();
    assert_eq!(mgr.active_connections(), 1);
}

#[test]
fn test_security_connection_rate_limiting() {
    // SECURITY: High connection rate from single source should be limited
    // Prevents SYN flood and connection exhaustion attacks
    let config = ConnectionConfig {
        max_connections: 100,
        max_connections_per_ip: 100,
        rate_limit_per_sec_per_ip: 3,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    let attacker_ip = ip(1);
    let legit_ip = ip(2);

    // Connections from same IP are throttled
    let mut permits = Vec::new();
    for _ in 0..3 {
        permits.push(mgr.try_acquire(attacker_ip).unwrap());
    }

    // 4th connection within same second is rate limited
    let result = mgr.try_acquire(attacker_ip);
    assert!(
        matches!(result, Err(ConnectionRejection::RateLimitExceeded { .. })),
        "Should be rate limited"
    );

    // Legitimate traffic from different IP is not blocked
    let legit = mgr.try_acquire(legit_ip);
    assert!(legit.is_ok(), "Legitimate traffic should not be blocked");
}

#[test]
fn test_security_connection_graceful_shutdown() {
    // SECURITY: Service shutdown should gracefully close connections
    // Allows in-flight messages to complete
    let config = ConnectionConfig {
        shutdown_timeout_secs: 5,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    // Establish some connections
    let _p1 = mgr.try_acquire(ip(1)).unwrap();
    let _p2 = mgr.try_acquire(ip(2)).unwrap();
    assert_eq!(mgr.active_connections(), 2);

    // Service stops accepting new connections
    mgr.shutdown();
    assert!(mgr.is_shutting_down());

    let result = mgr.try_acquire(ip(3));
    assert!(
        matches!(result, Err(ConnectionRejection::ShuttingDown)),
        "New connections should be rejected during shutdown"
    );

    // Existing connections are still counted (not forcefully killed)
    assert_eq!(mgr.active_connections(), 2);

    // Shutdown timeout is configured
    assert_eq!(mgr.shutdown_timeout(), Duration::from_secs(5));
}

#[test]
fn test_security_connection_per_ip_tracking() {
    // SECURITY: Track connections per IP address for DDoS defense
    // Allows blocking IPs with excessive connections
    let config = ConnectionConfig {
        max_connections: 100,
        max_connections_per_ip: 3,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    let monitored_ip = ip(1);
    let other_ip = ip(2);

    // Connection count per IP is tracked
    let _p1 = mgr.try_acquire(monitored_ip).unwrap();
    let _p2 = mgr.try_acquire(monitored_ip).unwrap();
    let _p3 = mgr.try_acquire(other_ip).unwrap();

    assert_eq!(mgr.connections_for_ip(&monitored_ip), 2);
    assert_eq!(mgr.connections_for_ip(&other_ip), 1);

    // Block when IP exceeds threshold
    let _p4 = mgr.try_acquire(monitored_ip).unwrap();
    assert_eq!(mgr.connections_for_ip(&monitored_ip), 3);

    let result = mgr.try_acquire(monitored_ip);
    assert!(
        matches!(result, Err(ConnectionRejection::PerIpLimitReached { .. })),
        "Should block when per-IP limit exceeded"
    );

    // Other IPs still work
    let other_result = mgr.try_acquire(other_ip);
    assert!(other_result.is_ok());
}

#[test]
fn test_security_connection_keep_alive_heartbeat() {
    // SECURITY: Keep-alive heartbeats prevent zombie connections
    // Ensures both sides are still responsive

    // Default keep-alive interval is 60 seconds
    let default_mgr = ConnectionManager::new(ConnectionConfig::default());
    assert_eq!(default_mgr.keepalive_interval(), Duration::from_secs(60));

    // Custom keep-alive interval
    let config = ConnectionConfig {
        keepalive_interval_secs: 30,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);
    assert_eq!(mgr.keepalive_interval(), Duration::from_secs(30));
}

#[test]
fn test_security_connection_backpressure_handling() {
    // SECURITY: Backpressure must not cause resource exhaustion
    // When client can't keep up, server must handle gracefully

    // Send buffer has reasonable size limit (default 1MB)
    let default_mgr = ConnectionManager::new(ConnectionConfig::default());
    assert_eq!(default_mgr.send_buffer_size(), 1_048_576);

    // Custom buffer size
    let config = ConnectionConfig {
        send_buffer_size: 512 * 1024,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);
    assert_eq!(mgr.send_buffer_size(), 512 * 1024);

    // Slow clients don't affect fast clients - each connection
    // has independent limits via the ConnectionManager
    let _p1 = mgr.try_acquire(ip(1)).unwrap();
    let _p2 = mgr.try_acquire(ip(2)).unwrap();
    assert_eq!(mgr.active_connections(), 2);
}

#[test]
fn test_security_connection_resource_cleanup_on_error() {
    // SECURITY: When connection has error, all resources must be freed
    // Prevents resource leaks from failed connections
    let config = ConnectionConfig {
        max_connections: 1,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    let test_ip = ip(1);

    // Simulate repeated connect/error/cleanup cycles
    for cycle in 0..10 {
        let permit = mgr.try_acquire(test_ip).unwrap();
        assert_eq!(mgr.active_connections(), 1, "Cycle {}: active should be 1", cycle);
        assert_eq!(mgr.connections_for_ip(&test_ip), 1);

        // Drop permit (simulates error path cleanup)
        drop(permit);

        // All resources freed
        assert_eq!(mgr.active_connections(), 0, "Cycle {}: should be 0 after drop", cycle);
        assert_eq!(mgr.connections_for_ip(&test_ip), 0);
    }

    // Connection slot is released for new connections
    let _final_permit = mgr.try_acquire(test_ip).unwrap();
    assert_eq!(mgr.active_connections(), 1);
}
