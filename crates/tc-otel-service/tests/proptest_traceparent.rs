//! Property tests for `parse_w3c_traceparent`.

use proptest::prelude::*;
use tc_otel_service::span_dispatcher::parse_w3c_traceparent;

proptest! {
    #[test]
    fn arbitrary_strings_never_panic(s in any::<String>()) {
        let _ = parse_w3c_traceparent(&s);
    }

    #[test]
    fn well_formed_traceparent_round_trips(
        trace in "[0-9a-f]{32}",
        span in "[0-9a-f]{16}",
        flags in "[0-9a-f]{2}",
    ) {
        let s = format!("00-{trace}-{span}-{flags}");
        let (tid, sid) = parse_w3c_traceparent(&s).expect("well-formed traceparent parses");
        prop_assert_eq!(hex::encode(tid), trace);
        prop_assert_eq!(hex::encode(sid), span);
    }

    #[test]
    fn some_implies_ascii_hex_of_exact_length(s in "[0-9a-zA-Z-]{0,120}|\\PC{0,60}") {
        if parse_w3c_traceparent(&s).is_some() {
            let parts: Vec<&str> = s.split('-').collect();
            prop_assert_eq!(parts.len(), 4);
            prop_assert_eq!(parts[0], "00");
            prop_assert_eq!(parts[1].len(), 32);
            prop_assert_eq!(parts[2].len(), 16);
            prop_assert!(parts[1].bytes().all(|b| b.is_ascii_hexdigit()));
            prop_assert!(parts[2].bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }
}
