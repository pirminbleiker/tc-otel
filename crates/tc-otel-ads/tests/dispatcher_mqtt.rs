//! Integration test for [`tc_otel_ads::dispatcher::AmsDispatcher`] against a
//! live MQTT broker.
//!
//! # What this proves
//!
//! The dispatcher is the core of the transport-agnostic routing design: it
//! owns a route table populated from `AdsOverMqtt/<netid>/info` announcements
//! and correlates outgoing requests to their invoke-id-matching responses.
//! This test exercises the full round-trip against a real mosquitto broker
//! with a synthetic "peer" that mimics what a PLC would do.
//!
//! # Requirements
//!
//! A mosquitto broker reachable at `127.0.0.1:1883` (or at the host+port
//! configured via the `TCOTEL_MQTT_BROKER_HOST` / `TCOTEL_MQTT_BROKER_PORT`
//! env vars). The test is gated `#[ignore]` so `cargo test` on a workstation
//! without a broker doesn't fail. Run with:
//!
//! ```bash
//! cargo test -p tc-otel-ads --test dispatcher_mqtt -- --ignored
//! ```

use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::ams::{AmsHeader, AmsNetId, ADS_CMD_READ};
use tc_otel_ads::dispatcher::{AmsDispatcher, TransportKind};
use tokio::sync::oneshot;

const AMS_STATE_RESPONSE: u16 = 0x0001;
const TOPIC_PREFIX: &str = "DispatcherTest";

fn broker_host() -> String {
    std::env::var("TCOTEL_MQTT_BROKER_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn broker_port() -> u16 {
    std::env::var("TCOTEL_MQTT_BROKER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1883)
}

/// Spawns a fake PLC peer that:
/// 1. Announces itself on `{prefix}/{peer_netid}/info` with `online=true`.
/// 2. Subscribes to `{prefix}/{peer_netid}/ams` to receive requests.
/// 3. On each request, parses the AMS header, builds a matching response
///    (same invoke_id, response state flag, 4 extra bytes of payload), and
///    publishes it on `{prefix}/{source_netid}/ams/res`.
///
/// Returns a handle to the client (so the test can keep it alive) and a
/// signal that resolves once the peer has seen at least one request.
async fn spawn_fake_peer(
    client_id: &str,
    peer_netid: AmsNetId,
    response_payload: Vec<u8>,
) -> (AsyncClient, oneshot::Receiver<u32>) {
    let mut opts = MqttOptions::new(client_id, broker_host(), broker_port());
    opts.set_keep_alive(Duration::from_secs(30));
    let (client, mut event_loop) = AsyncClient::new(opts, 32);
    let request_topic = format!("{}/{}/ams", TOPIC_PREFIX, peer_netid);
    let info_topic = format!("{}/{}/info", TOPIC_PREFIX, peer_netid);

    let info_xml = format!(
        r#"<info><online name="fake-peer-{}" osPlatform="0">true</online></info>"#,
        peer_netid
    );

    let (request_seen_tx, request_seen_rx) = oneshot::channel::<u32>();
    let mut request_seen_tx = Some(request_seen_tx);

    let client_for_task = client.clone();
    tokio::spawn(async move {
        loop {
            match event_loop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    let _ = client_for_task
                        .subscribe(&request_topic, QoS::AtMostOnce)
                        .await;
                    let _ = client_for_task
                        .publish(
                            &info_topic,
                            QoS::AtLeastOnce,
                            /* retain */ true,
                            info_xml.as_bytes().to_vec(),
                        )
                        .await;
                }
                Ok(Event::Incoming(Incoming::Publish(publish))) => {
                    if publish.topic == request_topic {
                        // Parse the AMS header and reply. MQTT frames are
                        // AMS header + payload, no TCP prefix.
                        if publish.payload.len() < 32 {
                            continue;
                        }
                        let hdr = match AmsHeader::parse(&publish.payload[..32]) {
                            Ok(h) => h,
                            Err(_) => continue,
                        };

                        // Build response frame: echo payload + append marker.
                        let mut resp_payload = Vec::new();
                        resp_payload.extend_from_slice(&response_payload);

                        let resp_header = AmsHeader {
                            target_net_id: hdr.source_net_id,
                            target_port: hdr.source_port,
                            source_net_id: peer_netid,
                            source_port: hdr.target_port,
                            command_id: hdr.command_id,
                            state_flags: AMS_STATE_RESPONSE,
                            data_length: resp_payload.len() as u32,
                            error_code: 0,
                            invoke_id: hdr.invoke_id,
                        };
                        let mut frame = resp_header.serialize();
                        frame.extend_from_slice(&resp_payload);

                        let res_topic = format!("{}/{}/ams/res", TOPIC_PREFIX, hdr.source_net_id);
                        let _ = client_for_task
                            .publish(&res_topic, QoS::AtMostOnce, false, frame)
                            .await;

                        if let Some(tx) = request_seen_tx.take() {
                            let _ = tx.send(hdr.invoke_id);
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });

    (client, request_seen_rx)
}

/// Clean up retained /info for a given peer on teardown so subsequent test
/// runs start from a clean broker state.
async fn clean_retained_info(peer_netid: AmsNetId, client_id: &str) {
    let mut opts = MqttOptions::new(client_id, broker_host(), broker_port());
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut event_loop) = AsyncClient::new(opts, 8);
    let topic = format!("{}/{}/info", TOPIC_PREFIX, peer_netid);
    tokio::spawn(async move {
        let _ = client
            .publish(topic, QoS::AtLeastOnce, /* retain */ true, Vec::new())
            .await;
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            let _ = event_loop.poll().await;
        })
        .await;
    });
}

#[tokio::test]
#[ignore]
async fn dispatcher_learns_route_from_info_and_roundtrips_request() {
    let our_netid = AmsNetId([10, 20, 30, 40, 1, 1]);
    let peer_netid = AmsNetId([99, 88, 77, 66, 1, 1]);

    // 1. Spawn fake peer. It will publish its /info and subscribe to its ams topic.
    let (_peer_client, request_seen_rx) = spawn_fake_peer(
        "dispatcher-it-peer",
        peer_netid,
        vec![0xAA, 0xBB, 0xCC, 0xDD],
    )
    .await;

    // 2. Wire up the dispatcher.
    let mut dispatcher = AmsDispatcher::new(our_netid, 30010);
    dispatcher
        .attach_mqtt(
            &broker_host(),
            broker_port(),
            "dispatcher-it-disp",
            TOPIC_PREFIX,
        )
        .await
        .expect("attach_mqtt");
    let dispatcher = Arc::new(dispatcher);

    // 3. Wait for the route to be learned (up to 5 s).
    let routes = dispatcher.routes();
    let learned = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if routes.get(peer_netid) == Some(TransportKind::Mqtt) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        learned.is_ok(),
        "route for peer was not learned within 5 s — mosquitto unreachable or peer failed to publish /info"
    );

    // 4. Issue a send_request.
    let response = dispatcher
        .send_request(
            peer_netid,
            851,
            ADS_CMD_READ,
            &[0x10, 0x20, 0x30],
            Duration::from_secs(5),
        )
        .await
        .expect("send_request");

    // 5. Verify the peer saw our request and the response came back.
    let seen_invoke_id = tokio::time::timeout(Duration::from_secs(2), request_seen_rx)
        .await
        .expect("peer didn't see request")
        .expect("peer oneshot dropped");
    assert!(seen_invoke_id >= 1);
    assert_eq!(response, vec![0xAA, 0xBB, 0xCC, 0xDD]);

    // 6. Peer goes offline — dispatcher should forget the route.
    //    Use a dedicated short-lived publisher. Keep its event loop pumping
    //    long enough for the broker to deliver the retained /info update to
    //    our dispatcher's subscription (rumqttc won't actually send the
    //    PUBLISH until the event loop is polled past the initial ConnAck).
    let mut opts = MqttOptions::new("dispatcher-it-offline", broker_host(), broker_port());
    opts.set_keep_alive(Duration::from_secs(5));
    let (offline_client, mut offline_eventloop) = AsyncClient::new(opts, 8);
    let info_topic = format!("{}/{}/info", TOPIC_PREFIX, peer_netid);

    let pump_task = tokio::spawn(async move {
        // Pump until cancellation/timeout. The client's publish is enqueued
        // before this task starts so it will fire on the first poll cycle.
        for _ in 0..80 {
            match tokio::time::timeout(Duration::from_millis(50), offline_eventloop.poll()).await {
                Ok(Ok(_)) => {}
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    });

    offline_client
        .publish(
            &info_topic,
            QoS::AtLeastOnce,
            true,
            br#"<info><online name="fake-peer" osPlatform="0">false</online></info>"#.to_vec(),
        )
        .await
        .expect("offline publish");

    let forgotten = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if routes.get(peer_netid).is_none() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    pump_task.abort();
    assert!(forgotten.is_ok(), "route not forgotten after offline /info");

    // Cleanup retained topics so we don't pollute subsequent runs.
    clean_retained_info(peer_netid, "dispatcher-it-cleanup").await;
}
