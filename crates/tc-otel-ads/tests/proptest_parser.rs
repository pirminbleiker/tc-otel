//! Property tests for the wire parsers: arbitrary input never panics and
//! decoded output stays bounded by the input size.

use proptest::prelude::*;
use tc_otel_ads::{AdsParser, AmsHeader};

/// Message-type bytes `parse_all` dispatches on (log v1/v2, registrations,
/// spans, metrics, trace events). Unknown values are covered by the arbitrary strategy.
fn frame_type() -> impl Strategy<Value = u8> {
    prop_oneof![1u8..=12u8, any::<u8>()]
}

proptest! {
    #[test]
    fn parse_all_arbitrary_bytes_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        if let Ok(pr) = AdsParser::parse_all(&data) {
            // Every decoded item consumes at least one input byte.
            let items = pr.entries.len()
                + pr.registrations.len()
                + pr.spans.len()
                + pr.metrics.len()
                + pr.trace_events.len();
            prop_assert!(items <= data.len());
        }
    }

    #[test]
    fn parse_all_structured_prefix_never_panics(
        ty in frame_type(),
        rest in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let mut data = vec![ty];
        data.extend_from_slice(&rest);
        if let Ok(pr) = AdsParser::parse_all(&data) {
            let items = pr.entries.len()
                + pr.registrations.len()
                + pr.spans.len()
                + pr.metrics.len()
                + pr.trace_events.len();
            prop_assert!(items <= data.len());
        }
    }

    #[test]
    fn parse_all_rejects_oversized_input(extra in 1usize..64) {
        // Larger than the 1 MiB message cap is rejected before any decoding.
        let data = vec![1u8; 1_048_576 + extra];
        prop_assert!(AdsParser::parse_all(&data).is_err());
    }

    #[test]
    fn ams_header_parse_never_panics_and_round_trips(data in proptest::collection::vec(any::<u8>(), 0..128)) {
        match AmsHeader::parse(&data) {
            Ok(header) => {
                prop_assert!(data.len() >= 32);
                prop_assert_eq!(header.serialize(), data[..32].to_vec());
            }
            Err(_) => prop_assert!(data.len() < 32),
        }
    }
}
