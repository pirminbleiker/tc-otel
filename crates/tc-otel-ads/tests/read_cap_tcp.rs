//! An oversized ADS READ over a real AMS/TCP socket is answered with an error
//! and leaves the connection usable.

use std::sync::Arc;
use std::time::Duration;
use tc_otel_ads::ams::{ADS_CMD_READ_STATE, ADS_STATE_REQUEST, ADS_STATE_RESPONSE};
use tc_otel_ads::router::AdsRouter;
use tc_otel_ads::{
    AmsHeader, AmsNetId, AmsTransport, TaskRegistry, TcpAmsTransport, ADSERR_DEVICE_INVALIDSIZE,
    ADS_CMD_READ,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

fn request(command_id: u16, invoke_id: u32, body: &[u8]) -> Vec<u8> {
    let header = AmsHeader {
        target_net_id: AmsNetId::from_bytes([10, 0, 0, 1, 1, 1]),
        target_port: 16150,
        source_net_id: AmsNetId::from_bytes([192, 168, 1, 50, 1, 1]),
        source_port: 32768,
        command_id,
        state_flags: ADS_STATE_REQUEST,
        data_length: body.len() as u32,
        error_code: 0,
        invoke_id,
    };
    let ams = [header.serialize(), body.to_vec()].concat();
    let mut out = vec![0u8, 0u8];
    out.extend_from_slice(&(ams.len() as u32).to_le_bytes());
    out.extend_from_slice(&ams);
    out
}

async fn read_reply(sock: &mut TcpStream) -> Vec<u8> {
    let mut prefix = [0u8; 6];
    sock.read_exact(&mut prefix).await.expect("tcp prefix");
    let len = u32::from_le_bytes([prefix[2], prefix[3], prefix[4], prefix[5]]) as usize;
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).await.expect("ams body");
    [prefix.to_vec(), body].concat()
}

#[tokio::test]
async fn oversized_read_is_rejected_and_connection_survives() {
    // Reserve a free loopback port, then hand it to the transport.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (log_tx, _log_rx) = mpsc::channel(16);
    let router = Arc::new(AdsRouter::new(
        16150,
        log_tx,
        None,
        Arc::new(TaskRegistry::new()),
    ));
    let transport = Arc::new(
        TcpAmsTransport::new(
            "127.0.0.1".into(),
            AmsNetId::from_bytes([10, 0, 0, 1, 1, 1]),
            router,
        )
        .with_port(port),
    );
    tokio::spawn(transport.run());

    let mut sock = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)).await {
            sock = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut sock = sock.expect("transport did not start listening");

    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    sock.write_all(&request(ADS_CMD_READ, 1, &body))
        .await
        .unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(5), read_reply(&mut sock))
        .await
        .expect("READ reply timed out");
    assert_eq!(reply.len(), 6 + 40);
    let result = u32::from_le_bytes([reply[38], reply[39], reply[40], reply[41]]);
    assert_eq!(result, ADSERR_DEVICE_INVALIDSIZE);

    // Same connection still answers.
    sock.write_all(&request(ADS_CMD_READ_STATE, 2, &[]))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), read_reply(&mut sock))
        .await
        .expect("READ_STATE reply timed out");
    let hdr = AmsHeader::parse(&reply[6..]).unwrap();
    assert_eq!(hdr.invoke_id, 2);
    assert_eq!(hdr.state_flags, ADS_STATE_RESPONSE);
}
