//! Socket-to-OTLP end-to-end tests.

mod common;

use common::wire;
use tc_otel_ads::{AdsParser, TraceWireEvent};

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
