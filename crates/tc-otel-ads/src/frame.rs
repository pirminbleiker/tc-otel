//! AMS protocol frame codec (transport-agnostic)
//!
//! Provides a higher-level `AmsFrame` type that encapsulates the 32-byte AMS header
//! plus variable-length ADS payload, with encode/decode for both TCP and MQTT transports.
//!
//! The AMS frame format is:
//! - 32-byte AMS header (encodes source/target addresses, command, state, etc.)
//! - Variable-length ADS payload (command-specific data)
//!
//! TCP transport wraps this with a 6-byte prefix (reserved + data length).
//! MQTT transport uses the raw frame directly.

use crate::ams::AmsHeader;
use crate::error::{AdsError, Result};

/// AMS protocol frame: 32-byte header + variable-length ADS payload.
///
/// Transport-agnostic abstraction over raw AMS frames.
/// Can be encoded/decoded for both TCP (with 6-byte prefix) and MQTT (raw).
#[derive(Debug, Clone)]
pub struct AmsFrame {
    pub header: AmsHeader,
    pub payload: Vec<u8>,
}

impl AmsFrame {
    /// Decode from raw bytes (32-byte AMS header + payload).
    ///
    /// Used by MQTT directly. Used by TCP after stripping the 6-byte AMS/TCP prefix.
    ///
    /// # Errors
    ///
    /// Returns `ParseError` if the buffer is shorter than 32 bytes or if the header
    /// cannot be parsed.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 32 {
            return Err(AdsError::ParseError(format!(
                "AMS frame too short: {} bytes (minimum 32)",
                bytes.len()
            )));
        }

        let header = AmsHeader::parse(bytes)?;
        let payload = if bytes.len() > 32 {
            bytes[32..].to_vec()
        } else {
            Vec::new()
        };

        Ok(AmsFrame { header, payload })
    }

    /// Encode to raw bytes (header + payload).
    ///
    /// Returns the 32-byte header followed by the payload bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut result = self.header.serialize();
        result.extend(&self.payload);
        result
    }

    /// Wrap with the 6-byte AMS/TCP transport prefix.
    ///
    /// Layout: 2 bytes reserved (0x0000) + 4 bytes data length (LE u32).
    /// The data length is the combined size of the 32-byte AMS header + payload.
    ///
    /// Used by TCP transport for both reads and writes.
    pub fn encode_with_tcp_header(&self) -> Vec<u8> {
        let frame_bytes = self.encode();
        let data_length = frame_bytes.len() as u32;

        let mut result = Vec::with_capacity(6 + frame_bytes.len());
        result.extend_from_slice(&0u16.to_le_bytes()); // reserved
        result.extend_from_slice(&data_length.to_le_bytes()); // data length
        result.extend(&frame_bytes);
        result
    }

    /// Build a response frame from this request.
    ///
    /// Automatically:
    /// - swaps source and target NetId+port
    /// - sets state_flags to 0x0005 (response | TCP)
    /// - clears error_code to 0
    /// - echoes invoke_id
    /// - uses the provided payload
    pub fn build_response(&self, payload: Vec<u8>) -> Self {
        AmsFrame {
            header: crate::ams::AmsHeader {
                target_net_id: self.header.source_net_id,
                target_port: self.header.source_port,
                source_net_id: self.header.target_net_id,
                source_port: self.header.target_port,
                command_id: self.header.command_id,
                state_flags: 0x0005, // response | TCP
                data_length: payload.len() as u32,
                error_code: 0,
                invoke_id: self.header.invoke_id,
            },
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ams::{AmsNetId, ADS_STATE_REQUEST};
    use std::str::FromStr;

    fn create_test_frame() -> AmsFrame {
        AmsFrame {
            header: AmsHeader {
                target_net_id: AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
                target_port: 16150,
                source_net_id: AmsNetId::from_str("5.0.0.0.1.1").unwrap(),
                source_port: 50000,
                command_id: 3,
                state_flags: ADS_STATE_REQUEST,
                data_length: 10,
                error_code: 0,
                invoke_id: 12345,
            },
            payload: vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A],
        }
    }

    #[test]
    fn decode_roundtrip() {
        let original = create_test_frame();
        let encoded = original.encode();
        let decoded = AmsFrame::decode(&encoded).unwrap();

        assert_eq!(decoded.header.target_port, original.header.target_port);
        assert_eq!(decoded.header.source_port, original.header.source_port);
        assert_eq!(decoded.header.command_id, original.header.command_id);
        assert_eq!(decoded.header.invoke_id, original.header.invoke_id);
        assert_eq!(decoded.payload, original.payload);
    }

    #[test]
    fn decode_too_short() {
        let result = AmsFrame::decode(&[0x01, 0x02, 0x03]);
        assert!(result.is_err());

        match result.unwrap_err() {
            AdsError::ParseError(msg) => {
                assert!(msg.contains("AMS frame too short"));
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn tcp_prefix_layout() {
        let frame = create_test_frame();
        let encoded = frame.encode_with_tcp_header();

        // Check minimum length: 6 bytes (TCP prefix) + 32 bytes (AMS header) + payload
        let payload_len = frame.payload.len();
        let expected_len = 6 + 32 + payload_len;
        assert_eq!(encoded.len(), expected_len);

        // Check reserved field (first 2 bytes)
        assert_eq!(&encoded[0..2], &[0x00, 0x00]);

        // Check data length field (next 4 bytes, little-endian)
        let data_length =
            u32::from_le_bytes([encoded[2], encoded[3], encoded[4], encoded[5]]);
        assert_eq!(data_length as usize, 32 + payload_len);
    }

    #[test]
    fn tcp_prefix_strip_decode() {
        let frame = create_test_frame();
        let with_tcp = frame.encode_with_tcp_header();

        // Strip the 6-byte TCP prefix and decode
        let decoded_from_strip = AmsFrame::decode(&with_tcp[6..]).unwrap();

        // Decode directly (no TCP prefix)
        let decoded_direct = AmsFrame::decode(&frame.encode()).unwrap();

        assert_eq!(
            decoded_from_strip.header.invoke_id,
            decoded_direct.header.invoke_id
        );
        assert_eq!(decoded_from_strip.payload, decoded_direct.payload);
    }

    #[test]
    fn build_response_swaps_addresses() {
        let request = create_test_frame();
        let response = request.build_response(vec![0xFF, 0xFF]);

        assert_eq!(
            response.header.target_net_id,
            request.header.source_net_id
        );
        assert_eq!(
            response.header.source_net_id,
            request.header.target_net_id
        );
        assert_eq!(response.header.target_port, request.header.source_port);
        assert_eq!(response.header.source_port, request.header.target_port);
    }

    #[test]
    fn build_response_sets_response_flag() {
        let request = create_test_frame();
        let response = request.build_response(vec![0xFF]);

        assert_eq!(response.header.state_flags, 0x0005);
    }

    #[test]
    fn build_response_echoes_invoke_id() {
        let request = create_test_frame();
        let response = request.build_response(vec![0xFF]);

        assert_eq!(response.header.invoke_id, request.header.invoke_id);
    }
}
