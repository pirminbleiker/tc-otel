//! Integration tests for connection limits and management
//!
//! Tests the ConnectionManager from tc-otel-ads with realistic scenarios
//! including async operations and concurrent access.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::{ConnectionConfig, ConnectionManager, ConnectionRejection};

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
}

#[test]
fn test_max_100_connections_enforced() {
    let config = ConnectionConfig {
        max_connections: 100,
        max_connections_per_ip: 200,
        rate_limit_per_sec_per_ip: 200,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    let mut permits = Vec::new();
    for i in 0..100u8 {
        permits.push(mgr.try_acquire(ip(i)).unwrap());
    }
    assert_eq!(mgr.active_connections(), 100);

    // Connection 101 rejected
    assert!(matches!(
        mgr.try_acquire(ip(200)),
        Err(ConnectionRejection::MaxConnectionsReached)
    ));

    // Release one, acquire one
    permits.pop();
    assert!(mgr.try_acquire(ip(201)).is_ok());
}

#[test]
fn test_idle_timeout_defaults_to_300s() {
    let mgr = ConnectionManager::new(ConnectionConfig::default());
    assert_eq!(mgr.idle_timeout(), Duration::from_secs(300));
}

#[test]
fn test_graceful_rejection_message() {
    let config = ConnectionConfig {
        max_connections: 1,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    let _p = mgr.try_acquire(ip(1)).unwrap();
    let err = mgr.try_acquire(ip(2)).unwrap_err();
    assert!(err.to_string().contains("maximum connection limit"));
}

#[test]
fn test_per_ip_limit_default_10() {
    let config = ConnectionConfig {
        max_connections: 100,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    let test_ip = ip(1);
    let mut permits = Vec::new();
    for _ in 0..10 {
        permits.push(mgr.try_acquire(test_ip).unwrap());
    }

    assert!(matches!(
        mgr.try_acquire(test_ip),
        Err(ConnectionRejection::PerIpLimitReached { .. })
    ));

    // Other IPs unaffected
    assert!(mgr.try_acquire(ip(2)).is_ok());
}

#[test]
fn test_rate_limiting() {
    let config = ConnectionConfig {
        max_connections: 100,
        max_connections_per_ip: 100,
        rate_limit_per_sec_per_ip: 5,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    let test_ip = ip(1);
    let mut permits = Vec::new();
    for _ in 0..5 {
        permits.push(mgr.try_acquire(test_ip).unwrap());
    }

    assert!(matches!(
        mgr.try_acquire(test_ip),
        Err(ConnectionRejection::RateLimitExceeded { .. })
    ));
}

#[test]
fn test_shutdown_rejects_new_connections() {
    let mgr = ConnectionManager::new(ConnectionConfig::default());

    let _p = mgr.try_acquire(ip(1)).unwrap();
    mgr.shutdown();
    assert!(mgr.is_shutting_down());

    assert!(matches!(
        mgr.try_acquire(ip(2)),
        Err(ConnectionRejection::ShuttingDown)
    ));

    // Existing connection still counted
    assert_eq!(mgr.active_connections(), 1);
}

#[tokio::test]
async fn test_shutdown_drain_with_timeout() {
    let config = ConnectionConfig {
        shutdown_timeout_secs: 1,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = Arc::new(ConnectionManager::new(config));

    let permit = mgr.try_acquire(ip(1)).unwrap();
    mgr.shutdown();

    // Drop permit in background after short delay
    let mgr2 = mgr.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(permit);
        assert_eq!(mgr2.active_connections(), 0);
    });

    assert!(mgr.wait_for_drain().await);
}

#[test]
fn test_resource_cleanup_on_drop() {
    let config = ConnectionConfig {
        max_connections: 1,
        max_connections_per_ip: 10,
        rate_limit_per_sec_per_ip: 100,
        ..Default::default()
    };
    let mgr = ConnectionManager::new(config);

    // Acquire and release repeatedly — no leaks
    for _ in 0..20 {
        let p = mgr.try_acquire(ip(1)).unwrap();
        assert_eq!(mgr.active_connections(), 1);
        drop(p);
        assert_eq!(mgr.active_connections(), 0);
    }
}

#[test]
fn test_concurrent_access() {
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
            let _p = mgr.try_acquire(ip(i)).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(mgr.active_connections(), 0);
}

#[test]
fn test_keepalive_config() {
    let mgr = ConnectionManager::new(ConnectionConfig::default());
    assert_eq!(mgr.keepalive_interval(), Duration::from_secs(60));
}

#[test]
fn test_send_buffer_config() {
    let mgr = ConnectionManager::new(ConnectionConfig::default());
    assert_eq!(mgr.send_buffer_size(), 1_048_576);
}

#[test]
fn test_default_config_security_values() {
    let config = ConnectionConfig::default();
    assert_eq!(config.max_connections, 100);
    assert_eq!(config.idle_timeout_secs, 300);
    assert_eq!(config.max_connections_per_ip, 10);
    assert_eq!(config.rate_limit_per_sec_per_ip, 10);
    assert_eq!(config.keepalive_interval_secs, 60);
    assert_eq!(config.send_buffer_size, 1_048_576);
    assert_eq!(config.shutdown_timeout_secs, 30);
}
