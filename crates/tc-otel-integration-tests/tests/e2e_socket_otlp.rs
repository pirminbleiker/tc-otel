//! Socket-to-OTLP end-to-end tests.
//!
//! Each test starts the real service on an ephemeral port, drives it with a
//! fake PLC over AMS/TCP and observes the OTLP/HTTP requests it exports.

mod common;

use std::time::Duration;

use common::plc::{FakePlc, Observed, DEFAULT_ADS_PORT};
use common::service::{ServiceOptions, TestService};
use common::sink::OtlpSink;
use common::wire;
use tc_otel_ads::{AdsParser, TraceWireEvent};
use tc_otel_core::config::WireFormat;

const WAIT: Duration = Duration::from_secs(10);
const SERVICE_NET_ID: &str = "0.0.0.0.1.1";

async fn start(format: WireFormat) -> (TestService, OtlpSink) {
    let sink = OtlpSink::start().await;
    let service = TestService::start(ServiceOptions {
        logs_endpoint: sink.logs_url(),
        traces_endpoint: Some(sink.traces_url()),
        format,
    })
    .await;
    (service, sink)
}

async fn plc(service: &TestService, net_id: &str) -> FakePlc {
    FakePlc::connect(
        service.ams_addr(),
        net_id,
        851,
        (SERVICE_NET_ID, DEFAULT_ADS_PORT),
    )
    .await
    .expect("connect fake PLC")
}

async fn logs_round_trip(format: WireFormat) {
    let (service, sink) = start(format).await;
    let mut plc = plc(&service, "10.10.10.10.1.1").await;

    let result = plc
        .write_log_data(&wire::v2_log(4, 1, 7, "hello", "e2e.logger"))
        .await
        .expect("write");
    assert_eq!(result, 0, "WRITE ACK result");

    let logs = sink.wait_for_logs(1, WAIT).await;
    assert_eq!(logs.len(), 1);
    let log = &logs[0];
    assert_eq!(log.body, "hello");
    assert_eq!(log.scope, "e2e.logger");
    assert_eq!(log.severity_text, "ERROR");
    assert_eq!(log.severity_number, 17);
    assert_eq!(log.resource["plc.ams_net_id"], "10.10.10.10.1.1");
    assert_eq!(log.attributes["task.cycle"], "7");
    assert_eq!(log.attributes["tc.task.index"], "1");
}

#[tokio::test]
async fn logs_protobuf() {
    logs_round_trip(WireFormat::Protobuf).await;
}

#[tokio::test]
async fn logs_json() {
    logs_round_trip(WireFormat::Json).await;
}

async fn send_three(plc: &mut FakePlc, tag: &str) -> u32 {
    let data = wire::concat(
        &(0..3u32)
            .map(|i| wire::v2_log(2, 1, i, &format!("{tag}-{i}"), "e2e.sep"))
            .collect::<Vec<_>>(),
    );
    plc.write_log_data(&data).await.expect("write")
}

#[tokio::test]
async fn two_plcs_stay_separated() {
    let (service, sink) = start(WireFormat::Protobuf).await;
    let net_a = "10.10.10.10.1.1";
    let net_b = "10.10.10.11.1.1";
    let mut a = plc(&service, net_a).await;
    let mut b = plc(&service, net_b).await;

    let (ra, rb) = tokio::join!(send_three(&mut a, "a"), send_three(&mut b, "b"));
    assert_eq!((ra, rb), (0, 0));

    let logs = sink.wait_for_logs(6, WAIT).await;
    assert_eq!(logs.len(), 6);
    for (net, tag) in [(net_a, "a"), (net_b, "b")] {
        let own: Vec<_> = logs
            .iter()
            .filter(|l| l.resource["plc.ams_net_id"] == net)
            .collect();
        assert_eq!(own.len(), 3, "records under {net}");
        assert!(
            own.iter().all(|l| l.body.starts_with(tag)),
            "foreign record under {net}: {own:?}"
        );
    }
}

#[tokio::test]
async fn plc_reconnect_exports_both_batches() {
    let (service, sink) = start(WireFormat::Protobuf).await;
    let net = "10.10.10.10.1.1";

    let mut first = plc(&service, net).await;
    assert_eq!(
        first
            .write_log_data(&wire::v2_log(2, 1, 1, "before", "e2e.reconnect"))
            .await
            .unwrap(),
        0
    );
    drop(first);

    let mut second = plc(&service, net).await;
    assert_eq!(
        second
            .write_log_data(&wire::v2_log(2, 1, 2, "after", "e2e.reconnect"))
            .await
            .unwrap(),
        0
    );

    let logs = sink.wait_for_logs(2, WAIT).await;
    let mut bodies: Vec<_> = logs.iter().map(|l| l.body.as_str()).collect();
    bodies.sort_unstable();
    assert_eq!(bodies, ["after", "before"]);
}

#[tokio::test]
async fn malformed_frame_does_not_stop_service() {
    let (service, sink) = start(WireFormat::Protobuf).await;

    // AMS/TCP prefix declaring 4 body bytes, far shorter than an AMS header.
    let mut bad = plc(&service, "10.10.10.10.1.1").await;
    bad.send_raw(&[0, 0, 4, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef])
        .await
        .unwrap();
    // Today the connection stays open; the outcome is not asserted, only that
    // the service survives.
    let _: Observed = bad.expect_closed(Duration::from_millis(500)).await;
    assert!(
        service.is_running(),
        "service stopped after malformed frame"
    );

    let mut good = plc(&service, "10.10.10.12.1.1").await;
    assert_eq!(
        good.write_log_data(&wire::v2_log(2, 1, 1, "still alive", "e2e.malformed"))
            .await
            .unwrap(),
        0
    );
    let logs = sink.wait_for_logs(1, WAIT).await;
    assert_eq!(logs[0].body, "still alive");
}

#[tokio::test]
async fn span_begin_end_exports_one_span() {
    let (service, sink) = start(WireFormat::Protobuf).await;
    let mut plc = plc(&service, "10.10.10.10.1.1").await;
    let trace_id = [0x11u8; 16];
    let span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];

    let data = wire::concat(&[
        wire::span_begin(3, 1, trace_id, span_id, "op", 1_000),
        wire::span_end(1, span_id, 2_000),
    ]);
    assert_eq!(plc.write_log_data(&data).await.unwrap(), 0);

    let spans = sink.wait_for_spans(1, WAIT).await;
    assert_eq!(spans.len(), 1, "exactly one span: {spans:?}");
    assert_eq!(spans[0].name, "op");
    assert_eq!(spans[0].trace_id_hex, hex::encode(trace_id));
    assert_eq!(spans[0].span_id_hex, hex::encode(span_id));
}

#[tokio::test]
async fn small_read_returns_8_plus_len_bytes() {
    let (service, _sink) = start(WireFormat::Protobuf).await;
    let mut plc = plc(&service, "10.10.10.10.1.1").await;

    let payload = plc.read(1, 0, 4).await.expect("read");
    assert_eq!(payload.len(), 8 + 4);
}

#[test]
fn wire_encoders_parse() {
    let trace_id = [0x11u8; 16];
    let span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];

    let log = AdsParser::parse_all(&wire::v2_log(4, 1, 7, "hello", "e2e.logger")).unwrap();
    assert_eq!(log.entries.len(), 1);
    assert_eq!(log.entries[0].message, "hello");
    assert_eq!(log.entries[0].logger, "e2e.logger");

    let begin =
        AdsParser::parse_all(&wire::span_begin(3, 1, trace_id, span_id, "op", 1_000)).unwrap();
    assert_eq!(begin.trace_events.len(), 1);
    match &begin.trace_events[0] {
        TraceWireEvent::Begin {
            name,
            trace_id: t,
            span_id: s,
            ..
        } => {
            assert_eq!(name, "op");
            assert_eq!(*t, trace_id);
            assert_eq!(*s, span_id);
        }
        other => panic!("expected Begin, got {other:?}"),
    }

    let end = AdsParser::parse_all(&wire::span_end(1, span_id, 2_000)).unwrap();
    assert_eq!(end.trace_events.len(), 1);
    match &end.trace_events[0] {
        TraceWireEvent::End { span_id: s, .. } => assert_eq!(*s, span_id),
        other => panic!("expected End, got {other:?}"),
    }

    let mixed = wire::concat(&[
        wire::v2_log(4, 1, 8, "a", "l"),
        wire::span_begin(3, 1, trace_id, span_id, "op", 1_000),
        wire::span_end(1, span_id, 2_000),
    ]);
    let all = AdsParser::parse_all(&mixed).unwrap();
    assert_eq!(all.entries.len(), 1);
    assert_eq!(all.trace_events.len(), 2);
}
