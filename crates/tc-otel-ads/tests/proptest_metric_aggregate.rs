//! Property tests for `decode_metric_aggregate`: arbitrary frames never panic
//! and the decoded sample payload is bounded by the input size.

use proptest::prelude::*;
use tc_otel_ads::diagnostics::{
    DiagEvent, PUSH_METRIC_AGG_EVENT_TYPE, PUSH_METRIC_AGG_HEADER_SIZE, PUSH_WIRE_VERSION,
};
use tc_otel_ads::diagnostics_push::decode_metric_aggregate;

fn check(bytes: &[u8]) -> Result<(), TestCaseError> {
    if let Some(DiagEvent::MetricAggregateBatch {
        samples,
        sample_size,
        ..
    }) = decode_metric_aggregate(bytes)
    {
        // The body carries `sample_count * sample_size` bytes, all inside `bytes`.
        prop_assert!((samples.len() as u64) * (sample_size as u64) <= bytes.len() as u64);
    }
    Ok(())
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        check(&bytes)?;
    }

    #[test]
    fn valid_header_prefix_never_panics(
        body_schema in 0u8..=8,
        header_rest in proptest::collection::vec(any::<u8>(), PUSH_METRIC_AGG_HEADER_SIZE - 4),
        tail in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let mut bytes = vec![PUSH_WIRE_VERSION, PUSH_METRIC_AGG_EVENT_TYPE, 0, body_schema];
        bytes.extend_from_slice(&header_rest);
        bytes.extend_from_slice(&tail);
        check(&bytes)?;
    }

    #[test]
    fn valid_header_with_random_flags_and_sizes_never_panics(
        flags in any::<u8>(),
        body_schema in 0u8..=8,
        sample_size in prop_oneof![Just(0u32), Just(1u32), Just(8u32), 0u32..70_000, any::<u32>()],
        sample_count in prop_oneof![0u32..64, any::<u32>()],
        name_len in any::<u8>(),
        unit_len in any::<u8>(),
        stat_mask in any::<u8>(),
        tail in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let mut bytes = vec![0u8; PUSH_METRIC_AGG_HEADER_SIZE];
        bytes[0] = PUSH_WIRE_VERSION;
        bytes[1] = PUSH_METRIC_AGG_EVENT_TYPE;
        bytes[2] = flags;
        bytes[3] = body_schema;
        bytes[4..8].copy_from_slice(&sample_size.to_le_bytes());
        bytes[8..12].copy_from_slice(&sample_count.to_le_bytes());
        bytes[0x11] = stat_mask;
        bytes[0x30] = name_len;
        bytes[0x31] = unit_len;
        bytes.extend_from_slice(&tail);
        check(&bytes)?;
    }
}
