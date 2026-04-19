//! Integration test for [`tc_otel_ads::dispatcher::AmsDispatcher`]'s outbound
//! TCP path, using an in-process mock PLC peer bound to a random localhost
//! port. Does not require any external infrastructure.

use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::ams::{AmsHeader, AmsNetId, ADS_CMD_READ};
use tc_otel_ads::dispatcher::{AmsDispatcher, TransportKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const AMS_STATE_RESPONSE: u16 = 0x0001;

/// Spawn a mock PLC AMS/TCP peer on `127.0.0.1:<ephemeral>`. Returns the
/// bound `SocketAddr` and a oneshot receiver that fires once the peer has
/// seen at least one request (carrying the request's invoke-id for the test
/// to assert).
///
/// The mock accepts one connection, reads AMS/TCP frames (6-byte prefix +
/// 32-byte AMS header + body), echoes the invoke-id back with the
/// `response` state-flag set and the caller-supplied payload.
async fn spawn_mock_tcp_peer(
    peer_netid: AmsNetId,
    response_payload: Vec<u8>,
) -> (std::net::SocketAddr, oneshot::Receiver<u32>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let (seen_tx, seen_rx) = oneshot::channel::<u32>();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut seen_tx = Some(seen_tx);
        loop {
            let mut prefix = [0u8; 6];
            if sock.read_exact(&mut prefix).await.is_err() {
                break;
            }
            let total = u32::from_le_bytes([prefix[2], prefix[3], prefix[4], prefix[5]]) as usize;
            let mut body = vec![0u8; total];
            if sock.read_exact(&mut body).await.is_err() {
                break;
            }
            let hdr = AmsHeader::parse(&body[..32]).expect("header parse");

            let resp_header = AmsHeader {
                target_net_id: hdr.source_net_id,
                target_port: hdr.source_port,
                source_net_id: peer_netid,
                source_port: hdr.target_port,
                command_id: hdr.command_id,
                state_flags: AMS_STATE_RESPONSE,
                data_length: response_payload.len() as u32,
                error_code: 0,
                invoke_id: hdr.invoke_id,
            }
            .serialize();
            let mut out = Vec::with_capacity(6 + resp_header.len() + response_payload.len());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(
                &((resp_header.len() + response_payload.len()) as u32).to_le_bytes(),
            );
            out.extend_from_slice(&resp_header);
            out.extend_from_slice(&response_payload);
            if sock.write_all(&out).await.is_err() {
                break;
            }

            if let Some(tx) = seen_tx.take() {
                let _ = tx.send(hdr.invoke_id);
            }
        }
    });

    (addr, seen_rx)
}

#[tokio::test]
async fn dispatcher_tcp_outbound_round_trip() {
    let our_netid = AmsNetId([10, 20, 30, 40, 1, 1]);
    let peer_netid = AmsNetId([99, 88, 77, 66, 1, 1]);

    let (peer_addr, seen_rx) = spawn_mock_tcp_peer(peer_netid, vec![0xDE, 0xAD, 0xBE, 0xEF]).await;

    let dispatcher = Arc::new(AmsDispatcher::new(our_netid, 30010));
    dispatcher.add_tcp_peer(peer_netid, peer_addr).await;

    // Route table learned the TCP entry.
    assert_eq!(
        dispatcher.routes().get(peer_netid),
        Some(TransportKind::Tcp)
    );

    let resp = dispatcher
        .send_request(
            peer_netid,
            851,
            ADS_CMD_READ,
            &[0x01, 0x02, 0x03],
            Duration::from_secs(3),
        )
        .await
        .expect("send_request");
    assert_eq!(resp, vec![0xDE, 0xAD, 0xBE, 0xEF]);

    let invoke_id = tokio::time::timeout(Duration::from_secs(1), seen_rx)
        .await
        .expect("peer didn't report request")
        .expect("oneshot dropped");
    assert!(invoke_id >= 1);
}

#[tokio::test]
async fn dispatcher_tcp_connection_reuses_across_multiple_requests() {
    let our_netid = AmsNetId([10, 20, 30, 40, 1, 1]);
    let peer_netid = AmsNetId([99, 88, 77, 66, 1, 1]);

    // Accept one TCP connection, respond to every request, and fail
    // subsequent accepts (no more than one connection per test run).
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept 1");
        let mut reqs = 0u8;
        loop {
            let mut prefix = [0u8; 6];
            if sock.read_exact(&mut prefix).await.is_err() {
                return;
            }
            let total = u32::from_le_bytes([prefix[2], prefix[3], prefix[4], prefix[5]]) as usize;
            let mut body = vec![0u8; total];
            if sock.read_exact(&mut body).await.is_err() {
                return;
            }
            reqs += 1;
            let hdr = AmsHeader::parse(&body[..32]).expect("header parse");

            let resp_header = AmsHeader {
                target_net_id: hdr.source_net_id,
                target_port: hdr.source_port,
                source_net_id: peer_netid,
                source_port: hdr.target_port,
                command_id: hdr.command_id,
                state_flags: AMS_STATE_RESPONSE,
                data_length: 1,
                error_code: 0,
                invoke_id: hdr.invoke_id,
            }
            .serialize();
            let mut out = Vec::with_capacity(6 + resp_header.len() + 1);
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&((resp_header.len() + 1) as u32).to_le_bytes());
            out.extend_from_slice(&resp_header);
            out.push(reqs);
            let _ = sock.write_all(&out).await;
        }
    });

    let dispatcher = Arc::new(AmsDispatcher::new(our_netid, 30010));
    dispatcher.add_tcp_peer(peer_netid, addr).await;

    // Three requests back-to-back on the same peer. The mock listener only
    // calls `accept()` once — if the dispatcher dialed a new connection per
    // request, the second request would hang forever waiting for a listener
    // that has already handed its socket to session A. So completing three
    // round-trips within the send_request timeout proves reuse.
    for expected in [1u8, 2, 3] {
        let resp = dispatcher
            .send_request(peer_netid, 851, ADS_CMD_READ, &[], Duration::from_secs(3))
            .await
            .expect("send_request");
        assert_eq!(resp, vec![expected]);
    }
}

#[tokio::test]
async fn dispatcher_tcp_redials_after_peer_disconnect() {
    let our_netid = AmsNetId([10, 20, 30, 40, 1, 1]);
    let peer_netid = AmsNetId([99, 88, 77, 66, 1, 1]);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        // Session A: accept, respond to one request, then disconnect.
        {
            let (mut sock, _) = listener.accept().await.expect("accept A");
            let mut prefix = [0u8; 6];
            sock.read_exact(&mut prefix).await.expect("read A prefix");
            let total = u32::from_le_bytes([prefix[2], prefix[3], prefix[4], prefix[5]]) as usize;
            let mut body = vec![0u8; total];
            sock.read_exact(&mut body).await.expect("read A body");
            let hdr = AmsHeader::parse(&body[..32]).expect("header A");
            let resp = AmsHeader {
                target_net_id: hdr.source_net_id,
                target_port: hdr.source_port,
                source_net_id: peer_netid,
                source_port: hdr.target_port,
                command_id: hdr.command_id,
                state_flags: AMS_STATE_RESPONSE,
                data_length: 1,
                error_code: 0,
                invoke_id: hdr.invoke_id,
            }
            .serialize();
            let mut out = Vec::with_capacity(6 + resp.len() + 1);
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&((resp.len() + 1) as u32).to_le_bytes());
            out.extend_from_slice(&resp);
            out.push(0xAA);
            sock.write_all(&out).await.expect("write A");
            // Drop `sock` — the dispatcher's reader task should see EOF.
        }

        // Session B: accept again (the dispatcher should redial) and respond
        // with a different marker.
        let (mut sock, _) = listener.accept().await.expect("accept B");
        let mut prefix = [0u8; 6];
        sock.read_exact(&mut prefix).await.expect("read B prefix");
        let total = u32::from_le_bytes([prefix[2], prefix[3], prefix[4], prefix[5]]) as usize;
        let mut body = vec![0u8; total];
        sock.read_exact(&mut body).await.expect("read B body");
        let hdr = AmsHeader::parse(&body[..32]).expect("header B");
        let resp = AmsHeader {
            target_net_id: hdr.source_net_id,
            target_port: hdr.source_port,
            source_net_id: peer_netid,
            source_port: hdr.target_port,
            command_id: hdr.command_id,
            state_flags: AMS_STATE_RESPONSE,
            data_length: 1,
            error_code: 0,
            invoke_id: hdr.invoke_id,
        }
        .serialize();
        let mut out = Vec::with_capacity(6 + resp.len() + 1);
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&((resp.len() + 1) as u32).to_le_bytes());
        out.extend_from_slice(&resp);
        out.push(0xBB);
        sock.write_all(&out).await.expect("write B");
    });

    let dispatcher = Arc::new(AmsDispatcher::new(our_netid, 30010));
    dispatcher.add_tcp_peer(peer_netid, addr).await;

    // Request 1 on session A.
    let resp_a = dispatcher
        .send_request(peer_netid, 851, ADS_CMD_READ, &[], Duration::from_secs(3))
        .await
        .expect("send A");
    assert_eq!(resp_a, vec![0xAA]);

    // Give the reader task a beat to observe the EOF and remove the peer.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Request 2 should trigger a re-dial on session B.
    let resp_b = dispatcher
        .send_request(peer_netid, 851, ADS_CMD_READ, &[], Duration::from_secs(3))
        .await
        .expect("send B");
    assert_eq!(resp_b, vec![0xBB]);
}
