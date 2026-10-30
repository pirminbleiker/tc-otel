//! Property tests for `AdsRouter::dispatch`: arbitrary frames never panic,
//! responses never exceed `MAX_AMS_FRAME`, and READ lengths above
//! `MAX_READ_LEN` are answered with `ADSERR_DEVICE_INVALIDSIZE` and an empty
//! body instead of an allocation.

use proptest::prelude::*;
use std::sync::Arc;
use tc_otel_ads::router::AdsRouter;
use tc_otel_ads::{
    TaskRegistry, ADSERR_DEVICE_INVALIDSIZE, ADS_CMD_READ, MAX_AMS_FRAME, MAX_READ_LEN,
};
use tokio::sync::mpsc;

const HEADER_LEN: usize = 32;

fn run_dispatch(frame: &[u8]) -> tc_otel_ads::Result<Option<Vec<u8>>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (log_tx, _log_rx) = mpsc::channel(16);
    let router = AdsRouter::new(16150, log_tx, None, Arc::new(TaskRegistry::new()));
    rt.block_on(router.dispatch(frame, Some("127.0.0.1")))
}

fn read_len_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![
        Just(0u32),
        Just(1u32),
        Just(MAX_READ_LEN - 1),
        Just(MAX_READ_LEN),
        Just(MAX_READ_LEN + 1),
        Just(u32::MAX),
        any::<u32>(),
    ]
}

/// A well-formed 32-byte AMS header followed by an index group/offset/length body.
fn structured_frame() -> impl Strategy<Value = Vec<u8>> {
    (
        any::<[u8; 32]>(),
        any::<u16>(),
        any::<u16>(),
        any::<u32>(),
        any::<u32>(),
        read_len_strategy(),
        proptest::collection::vec(any::<u8>(), 0..64),
    )
        .prop_map(|(mut header, cmd, flags, group, offset, len, tail)| {
            header[16..18].copy_from_slice(&cmd.to_le_bytes());
            header[18..20].copy_from_slice(&flags.to_le_bytes());
            let mut frame = header.to_vec();
            frame.extend_from_slice(&group.to_le_bytes());
            frame.extend_from_slice(&offset.to_le_bytes());
            frame.extend_from_slice(&len.to_le_bytes());
            frame.extend_from_slice(&tail);
            frame
        })
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        if let Ok(Some(resp)) = run_dispatch(&bytes) {
            prop_assert!(resp.len() >= HEADER_LEN);
            prop_assert!(resp.len() <= MAX_AMS_FRAME);
        }
    }

    #[test]
    fn structured_frames_never_panic(frame in structured_frame()) {
        let result = run_dispatch(&frame);
        if let Ok(Some(resp)) = result {
            prop_assert!(resp.len() >= HEADER_LEN);
            prop_assert!(resp.len() <= MAX_AMS_FRAME);
        }
    }

    #[test]
    fn read_response_respects_cap(
        mut header in any::<[u8; 32]>(),
        group in any::<u32>(),
        offset in any::<u32>(),
        len in read_len_strategy(),
    ) {
        header[16..18].copy_from_slice(&ADS_CMD_READ.to_le_bytes());
        let mut frame = header.to_vec();
        frame.extend_from_slice(&group.to_le_bytes());
        frame.extend_from_slice(&offset.to_le_bytes());
        frame.extend_from_slice(&len.to_le_bytes());

        let resp = run_dispatch(&frame).expect("READ is answered").expect("READ has a response");
        prop_assert!(resp.len() <= MAX_AMS_FRAME);
        if len > MAX_READ_LEN {
            // Rejected: result 0x0705, no data, header + result + length word.
            prop_assert_eq!(resp.len(), HEADER_LEN + 8);
            prop_assert_eq!(&resp[HEADER_LEN..HEADER_LEN + 4], &ADSERR_DEVICE_INVALIDSIZE.to_le_bytes());
            prop_assert_eq!(&resp[HEADER_LEN + 4..HEADER_LEN + 8], &0u32.to_le_bytes());
        } else {
            prop_assert_eq!(resp.len(), HEADER_LEN + 8 + len as usize);
            // Response payload: result = 0, then the echoed length, then zeroes.
            prop_assert_eq!(&resp[HEADER_LEN..HEADER_LEN + 4], &0u32.to_le_bytes());
            prop_assert_eq!(&resp[HEADER_LEN + 4..HEADER_LEN + 8], &len.to_le_bytes());
            prop_assert!(resp[HEADER_LEN + 8..].iter().all(|&b| b == 0));
        }
    }
}
