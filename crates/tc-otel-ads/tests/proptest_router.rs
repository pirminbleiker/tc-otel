//! Property tests for `AdsRouter::dispatch`: arbitrary frames never panic and
//! READ responses stay proportional to the request.
//!
//! The router does not yet cap the READ length, so every strategy keeps the
//! requested length at or below `READ_LEN_CEILING`; lengths above it would
//! allocate up to 4 GiB per frame.

use proptest::prelude::*;
use std::sync::Arc;
use tc_otel_ads::router::AdsRouter;
use tc_otel_ads::TaskRegistry;
use tokio::sync::mpsc;

const READ_LEN_CEILING: u32 = 64 * 1024;
const ADS_CMD_READ: u16 = 2;
const HEADER_LEN: usize = 32;

fn run_dispatch(frame: &[u8]) -> tc_otel_ads::Result<Option<Vec<u8>>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (log_tx, _log_rx) = mpsc::channel(16);
    let router = AdsRouter::new(16150, log_tx, None, Arc::new(TaskRegistry::new()));
    rt.block_on(router.dispatch(frame, Some("127.0.0.1")))
}

/// Clamp the READ length field so the test never asks for a huge allocation.
fn clamp_read_len(mut frame: Vec<u8>) -> Vec<u8> {
    if frame.len() >= 44 && u16::from_le_bytes([frame[16], frame[17]]) == ADS_CMD_READ {
        let len = u32::from_le_bytes([frame[40], frame[41], frame[42], frame[43]]);
        if len > READ_LEN_CEILING {
            frame[40..44].copy_from_slice(&(len % (READ_LEN_CEILING + 1)).to_le_bytes());
        }
    }
    frame
}

fn read_len_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![
        Just(0u32),
        Just(1u32),
        Just(READ_LEN_CEILING - 1),
        Just(READ_LEN_CEILING),
        0..=READ_LEN_CEILING,
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
            clamp_read_len(frame)
        })
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let frame = clamp_read_len(bytes);
        if let Ok(Some(resp)) = run_dispatch(&frame) {
            prop_assert!(resp.len() >= HEADER_LEN);
        }
    }

    #[test]
    fn structured_frames_never_panic(frame in structured_frame()) {
        let result = run_dispatch(&frame);
        if let Ok(Some(resp)) = result {
            prop_assert!(resp.len() >= HEADER_LEN);
            // A response never grows beyond header + result/length words + the
            // (bounded) requested READ length.
            prop_assert!(resp.len() <= HEADER_LEN + 8 + READ_LEN_CEILING as usize + 8);
        }
    }

    #[test]
    fn read_response_length_matches_request(
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
        prop_assert_eq!(resp.len(), HEADER_LEN + 8 + len as usize);
        // Response payload: result = 0, then the echoed length, then zeroes.
        prop_assert_eq!(&resp[HEADER_LEN..HEADER_LEN + 4], &0u32.to_le_bytes());
        prop_assert_eq!(&resp[HEADER_LEN + 4..HEADER_LEN + 8], &len.to_le_bytes());
        prop_assert!(resp[HEADER_LEN + 8..].iter().all(|&b| b == 0));
    }
}
