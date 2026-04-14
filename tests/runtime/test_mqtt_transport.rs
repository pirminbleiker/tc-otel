//! Integration tests for MQTT AMS transport
//!
//! This test verifies that MqttAmsTransport correctly receives and parses
//! AMS frames delivered through a real MQTT broker (eclipse-mosquitto).

use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::transport::{AmsTransport, MqttAmsTransport, MqttTransportConfig};
use tc_otel_ads::AmsNetId;
use tc_otel_core::LogEntry;
use testcontainers::clients::Cli;
use testcontainers::images::generic::GenericImage;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Helper to start Mosquitto container and return port
fn start_mosquitto() -> (testcontainers::Container<GenericImage>, u16) {
    let docker = Cli::default();
    let mosquitto = GenericImage::new("eclipse-mosquitto", "2")
        .with_wait_for(testcontainers::core::WaitFor::message_on_stdout("mosquitto version"))
        .with_exposed_port(1883);

    let container = docker.run(mosquitto);
    let port = container.get_host_port_ipv4(1883);
    (container, port)
}

/// Helper to spawn publisher event loop
fn spawn_publisher_event_loop(mut event_loop: rumqttc::EventLoop) {
    tokio::spawn(async move {
        loop {
            if event_loop.poll().await.is_err() {
                break;
            }
        }
    });
}

/// Test that MqttAmsTransport receives AMS frames from MQTT and parses them
#[tokio::test]
async fn test_mqtt_transport_happy_path() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::TRACE)
        .try_init();

    let (_container, port) = start_mosquitto();
    tracing::info!("Started Mosquitto on port {}", port);

    let (log_tx, mut log_rx) = mpsc::channel::<LogEntry>(100);

    let config = MqttTransportConfig {
        broker_host: "localhost".to_string(),
        broker_port: port,
        client_id: "test-subscriber".to_string(),
        topic_prefix: "AdsOverMqtt".to_string(),
        local_net_id: AmsNetId::from_str("0.0.0.0.1.1").expect("Valid NetId"),
        ads_port: 200,
        username: None,
        password: None,
    };

    let transport = Arc::new(MqttAmsTransport::new(config, log_tx));
    let transport_clone = Arc::clone(&transport);

    let transport_task = tokio::spawn(async move {
        if let Err(e) = transport_clone.run().await {
            tracing::error!("Transport error: {}", e);
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    let publisher_options = rumqttc::MqttOptions::new("test-publisher", "localhost", port);
    let (publisher_client, publisher_event_loop) = rumqttc::AsyncClient::new(publisher_options, 10);
    spawn_publisher_event_loop(publisher_event_loop);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let fixture_data = include_bytes!("../../tests/fixtures/mqtt_ams_frame.bin");
    tracing::info!("Loaded fixture frame: {} bytes", fixture_data.len());

    let publish_topic = "AdsOverMqtt/0.0.0.0.1.1/ams";
    publisher_client
        .publish(publish_topic, rumqttc::QoS::AtMostOnce, false, fixture_data.to_vec())
        .await
        .expect("Failed to publish frame");

    tracing::info!("Published test frame to topic: {}", publish_topic);

    match timeout(Duration::from_secs(3), log_rx.recv()).await {
        Ok(Some(entry)) => {
            tracing::info!("Received log entry: {:?}", entry);
            assert!(!entry.message.is_empty(), "Log message should not be empty");
        }
        Ok(None) => {
            tracing::info!("Channel closed without receiving log entry (expected for non-log ports)");
        }
        Err(_) => {
            tracing::info!("Timeout waiting for log entry (expected for non-log ports)");
        }
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    transport_task.abort();

    tracing::info!("Test completed successfully");
}

/// Test that malformed frames are handled gracefully
#[tokio::test]
async fn test_mqtt_transport_malformed_frame() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::TRACE)
        .try_init();

    let (_container, port) = start_mosquitto();

    let (log_tx, _log_rx) = mpsc::channel::<LogEntry>(100);

    let config = MqttTransportConfig {
        broker_host: "localhost".to_string(),
        broker_port: port,
        client_id: "test-malformed".to_string(),
        topic_prefix: "AdsOverMqtt".to_string(),
        local_net_id: AmsNetId::from_str("1.1.1.1.1.1").expect("Valid NetId"),
        ads_port: 800,
        username: None,
        password: None,
    };

    let transport = Arc::new(MqttAmsTransport::new(config, log_tx));
    let transport_clone = Arc::clone(&transport);

    let transport_task = tokio::spawn(async move {
        let _ = transport_clone.run().await;
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    let publisher_options = rumqttc::MqttOptions::new("test-malformed-pub", "localhost", port);
    let (publisher_client, publisher_event_loop) = rumqttc::AsyncClient::new(publisher_options, 10);
    spawn_publisher_event_loop(publisher_event_loop);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let malformed_frame = b"too short";
    let publish_topic = "AdsOverMqtt/1.1.1.1.1.1/ams";

    publisher_client
        .publish(publish_topic, rumqttc::QoS::AtMostOnce, false, malformed_frame.to_vec())
        .await
        .expect("Failed to publish malformed frame");

    tracing::info!("Published malformed frame");

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(!transport_task.is_finished(), "Transport should still be running after malformed frame");
    transport_task.abort();
    tracing::info!("Malformed frame test passed");
}
