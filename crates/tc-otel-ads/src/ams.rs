//! AMS (Automation Management System) protocol types for TCP-based ADS communication
//!
//! This module provides structures and parsers for the AMS/TCP protocol
//! used to route ADS commands from TwinCAT PLCs over TCP.

use crate::error::{AdsError, Result};
use std::str::FromStr;

pub const AMS_TCP_PORT: u16 = 48898;
pub const ADS_CMD_READ_DEVICE_INFO: u16 = 1;
pub const ADS_CMD_READ: u16 = 2;
pub const ADS_CMD_WRITE: u16 = 3;
pub const ADS_CMD_READ_STATE: u16 = 4;
pub const ADS_CMD_WRITE_CONTROL: u16 = 5;
pub const ADS_CMD_ADD_NOTIFICATION: u16 = 6;
pub const ADS_CMD_DEL_NOTIFICATION: u16 = 7;
pub const ADS_CMD_NOTIFICATION: u16 = 8;
pub const ADS_CMD_READ_WRITE: u16 = 9;
pub const ADS_STATE_REQUEST: u16 = 0x0004;
pub const ADS_STATE_RESPONSE: u16 = 0x0005;

/// AMS Net ID (6 bytes: xxx.xxx.xxx.xxx.1.1)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmsNetId([u8; 6]);

impl AmsNetId {
    /// Parse AMS Net ID from string format "192.168.1.100.1.1"
    pub fn from_str_ref(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 6 {
            return Err(AdsError::ParseError(format!(
                "Invalid AMS Net ID format: expected 6 parts, got {}",
                parts.len()
            )));
        }

        let mut bytes = [0u8; 6];
        for (i, part) in parts.iter().enumerate() {
            bytes[i] = part
                .parse::<u8>()
                .map_err(|_| AdsError::ParseError(format!("Invalid AMS Net ID octet: {}", part)))?;
        }

        Ok(AmsNetId(bytes))
    }

    /// Get the raw bytes
    pub fn bytes(&self) -> &[u8; 6] {
        &self.0
    }
}

impl std::fmt::Display for AmsNetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}.{}.{}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

impl FromStr for AmsNetId {
    type Err = Box<dyn std::error::Error>;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        AmsNetId::from_str_ref(s).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
    }
}

/// AMS Header for TCP/IP transport
#[derive(Debug, Clone)]
pub struct AmsHeader {
    pub target_net_id: AmsNetId,
    pub target_port: u16,
    pub source_net_id: AmsNetId,
    pub source_port: u16,
    pub command_id: u16,
    pub state_flags: u16,
    pub data_length: u32,
    pub error_code: u32,
    pub invoke_id: u32,
}

impl AmsHeader {
    /// Parse AMS header from buffer (32 bytes)
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 32 {
            return Err(AdsError::ParseError(format!(
                "AMS header too short: {} bytes",
                data.len()
            )));
        }

        let target_net_id = AmsNetId([data[0], data[1], data[2], data[3], data[4], data[5]]);
        let target_port = u16::from_le_bytes([data[6], data[7]]);
        let source_net_id = AmsNetId([data[8], data[9], data[10], data[11], data[12], data[13]]);
        let source_port = u16::from_le_bytes([data[14], data[15]]);
        let command_id = u16::from_le_bytes([data[16], data[17]]);
        let state_flags = u16::from_le_bytes([data[18], data[19]]);
        let data_length = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
        let error_code = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
        let invoke_id = u32::from_le_bytes([data[28], data[29], data[30], data[31]]);

        Ok(AmsHeader {
            target_net_id,
            target_port,
            source_net_id,
            source_port,
            command_id,
            state_flags,
            data_length,
            error_code,
            invoke_id,
        })
    }

    /// Serialize AMS header to bytes (32 bytes)
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 32];
        buf[0..6].copy_from_slice(&self.target_net_id.0);
        buf[6..8].copy_from_slice(&self.target_port.to_le_bytes());
        buf[8..14].copy_from_slice(&self.source_net_id.0);
        buf[14..16].copy_from_slice(&self.source_port.to_le_bytes());
        buf[16..18].copy_from_slice(&self.command_id.to_le_bytes());
        buf[18..20].copy_from_slice(&self.state_flags.to_le_bytes());
        buf[20..24].copy_from_slice(&self.data_length.to_le_bytes());
        buf[24..28].copy_from_slice(&self.error_code.to_le_bytes());
        buf[28..32].copy_from_slice(&self.invoke_id.to_le_bytes());
        buf
    }

    /// Create a response header from a request
    pub fn make_response(&self, error_code: u32) -> Self {
        AmsHeader {
            target_net_id: self.source_net_id,
            target_port: self.source_port,
            source_net_id: self.target_net_id,
            source_port: self.target_port,
            command_id: self.command_id,
            state_flags: ADS_STATE_RESPONSE,
            data_length: 4, // Result code
            error_code,
            invoke_id: self.invoke_id,
        }
    }
}

/// ADS Write Request payload
#[derive(Debug, Clone)]
pub struct AdsWriteRequest {
    pub index_group: u32,
    pub index_offset: u32,
    pub data: Vec<u8>,
}

impl AdsWriteRequest {
    /// Parse ADS Write request from buffer (after AMS header)
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 12 {
            return Err(AdsError::ParseError(format!(
                "ADS Write request too short: {} bytes",
                data.len()
            )));
        }

        let index_group = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let index_offset = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let write_length = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;

        if data.len() < 12 + write_length {
            return Err(AdsError::ParseError(format!(
                "ADS Write data incomplete: expected {} bytes",
                write_length
            )));
        }

        Ok(AdsWriteRequest {
            index_group,
            index_offset,
            data: data[12..12 + write_length].to_vec(),
        })
    }

    /// Serialize ADS Write request to bytes
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + self.data.len());
        buf.extend_from_slice(&self.index_group.to_le_bytes());
        buf.extend_from_slice(&self.index_offset.to_le_bytes());
        buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }
}

/// AMS/TCP header (6 bytes: 2 reserved + 4 data length)
#[derive(Debug, Clone)]
pub struct AmsTcpHeader {
    pub reserved: u16,
    pub data_length: u32,
}

impl AmsTcpHeader {
    /// Parse AmsTcpHeader from 6 bytes
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 6 {
            return Err(AdsError::IncompleteMessage {
                expected: 6,
                got: data.len(),
            });
        }

        let reserved = u16::from_le_bytes([data[0], data[1]]);
        let data_length = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);

        Ok(AmsTcpHeader {
            reserved,
            data_length,
        })
    }

    /// Serialize AmsTcpHeader to 6 bytes
    pub fn serialize(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(6);
        result.extend_from_slice(&self.reserved.to_le_bytes());
        result.extend_from_slice(&self.data_length.to_le_bytes());
        result
    }
}

/// Complete AMS/TCP frame with TCP header, AMS header, and payload
#[derive(Debug, Clone)]
pub struct AmsTcpFrame {
    pub tcp_header: AmsTcpHeader,
    pub ams_header: AmsHeader,
    pub payload: Vec<u8>,
}

impl AmsTcpFrame {
    /// Parse a complete AMS/TCP frame from bytes
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 38 {
            return Err(AdsError::IncompleteMessage {
                expected: 38,
                got: data.len(),
            });
        }

        let tcp_header = AmsTcpHeader::parse(&data[0..6])?;
        let ams_header = AmsHeader::parse(&data[6..38])?;

        // tcp_header.data_length is the size of AMS header + payload
        let expected_total = 6 + tcp_header.data_length as usize;
        if data.len() < expected_total {
            return Err(AdsError::IncompleteMessage {
                expected: expected_total,
                got: data.len(),
            });
        }

        let payload = data[38..].to_vec();

        Ok(AmsTcpFrame {
            tcp_header,
            ams_header,
            payload,
        })
    }

    /// Create a response frame
    pub fn make_response(&self, error_code: u32, response_data: Vec<u8>) -> Self {
        let response_data_length = response_data.len() as u32;

        let response_ams_header = self.ams_header.make_response(error_code);
        // Override the data_length in the response header
        let mut response_ams = response_ams_header;
        response_ams.data_length = response_data_length;

        let response_tcp_header = AmsTcpHeader {
            reserved: 0,
            data_length: 32 + response_data_length,
        };

        AmsTcpFrame {
            tcp_header: response_tcp_header,
            ams_header: response_ams,
            payload: response_data,
        }
    }

    /// Serialize the complete frame to bytes
    pub fn serialize(&self) -> Vec<u8> {
        let mut result = Vec::new();
        result.extend(self.tcp_header.serialize());
        result.extend(self.ams_header.serialize());
        result.extend(&self.payload);
        result
    }

    /// Extract ADS Write request if this frame contains one
    pub fn extract_write_request(&self) -> Result<AdsWriteRequest> {
        if self.ams_header.command_id != ADS_CMD_WRITE {
            return Err(AdsError::ParseError(format!(
                "Expected Write command ({}), got {}",
                ADS_CMD_WRITE, self.ams_header.command_id
            )));
        }

        AdsWriteRequest::parse(&self.payload)
    }
}

pub const ADS_LOG_PORT: u16 = 16150;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ams_net_id_from_string() {
        let net_id = AmsNetId::from_str("192.168.1.100.1.1").unwrap();
        assert_eq!(net_id.to_string(), "192.168.1.100.1.1");
    }

    #[test]
    fn test_ams_net_id_bytes() {
        let net_id = AmsNetId::from_str("172.17.0.2.1.1").unwrap();
        assert_eq!(net_id.bytes(), &[172, 17, 0, 2, 1, 1]);
    }

    #[test]
    fn test_ams_header_serialize() {
        let header = AmsHeader {
            target_net_id: AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
            target_port: 16150,
            source_net_id: AmsNetId::from_str("192.168.1.50.1.1").unwrap(),
            source_port: 32768,
            command_id: 3,
            state_flags: 0x0004,
            data_length: 50,
            error_code: 0,
            invoke_id: 1,
        };

        let bytes = header.serialize();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn test_ams_header_roundtrip() {
        let header = AmsHeader {
            target_net_id: AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
            target_port: 16150,
            source_net_id: AmsNetId::from_str("192.168.1.50.1.1").unwrap(),
            source_port: 32768,
            command_id: 3,
            state_flags: 0x0004,
            data_length: 50,
            error_code: 0,
            invoke_id: 1,
        };

        let bytes = header.serialize();
        let parsed = AmsHeader::parse(&bytes).unwrap();

        assert_eq!(parsed.target_port, header.target_port);
        assert_eq!(parsed.command_id, header.command_id);
        assert_eq!(parsed.invoke_id, header.invoke_id);
    }

    #[test]
    fn test_ams_tcp_header_parse() {
        let data = [0x00, 0x00, 0x20, 0x00, 0x00, 0x00];
        let header = AmsTcpHeader::parse(&data).unwrap();
        assert_eq!(header.reserved, 0);
        assert_eq!(header.data_length, 32);
    }

    #[test]
    fn test_ams_tcp_header_serialize() {
        let header = AmsTcpHeader {
            reserved: 0,
            data_length: 100,
        };
        let serialized = header.serialize();
        assert_eq!(serialized.len(), 6);
        assert_eq!(serialized[0..2], [0, 0]);
        assert_eq!(serialized[2..6], 100u32.to_le_bytes());
    }

    #[test]
    fn test_ams_tcp_header_roundtrip() {
        let header = AmsTcpHeader {
            reserved: 0,
            data_length: 65535,
        };
        let serialized = header.serialize();
        let parsed = AmsTcpHeader::parse(&serialized).unwrap();
        assert_eq!(parsed.reserved, header.reserved);
        assert_eq!(parsed.data_length, header.data_length);
    }

    #[test]
    fn test_ads_write_request_serialize() {
        let write_req = AdsWriteRequest {
            index_group: 1,
            index_offset: 2,
            data: vec![0x01, 0x02, 0x03, 0x04],
        };

        let serialized = write_req.serialize();
        let parsed = AdsWriteRequest::parse(&serialized).unwrap();
        assert_eq!(parsed.index_group, 1);
        assert_eq!(parsed.index_offset, 2);
        assert_eq!(parsed.data, vec![0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn test_ams_tcp_frame_parse() {
        let payload_len = 12u32; // 12 bytes of payload
        let total_size = 6 + 32 + payload_len as usize;
        let mut data = vec![0u8; total_size];

        // TCP header
        data[0..2].copy_from_slice(&0u16.to_le_bytes()); // reserved
        data[2..6].copy_from_slice(&(32u32 + payload_len).to_le_bytes()); // data_length in TCP header

        // AMS header
        data[6..12].copy_from_slice(&[192, 168, 1, 100, 1, 1]); // target_net_id
        data[12..14].copy_from_slice(&16150u16.to_le_bytes()); // target_port
        data[14..20].copy_from_slice(&[5, 0, 0, 0, 1, 1]); // source_net_id
        data[20..22].copy_from_slice(&50000u16.to_le_bytes()); // source_port
        data[22..24].copy_from_slice(&ADS_CMD_WRITE.to_le_bytes()); // command_id
        data[24..26].copy_from_slice(&ADS_STATE_REQUEST.to_le_bytes()); // state_flags
        data[26..30].copy_from_slice(&payload_len.to_le_bytes()); // data_length in AMS header
        data[30..34].copy_from_slice(&0u32.to_le_bytes()); // error_code
        data[34..38].copy_from_slice(&12345u32.to_le_bytes()); // invoke_id

        // Payload
        for i in 0..12 {
            data[38 + i] = i as u8;
        }

        let frame = AmsTcpFrame::parse(&data).unwrap();
        assert_eq!(frame.tcp_header.data_length, 32 + payload_len);
        assert_eq!(frame.ams_header.command_id, ADS_CMD_WRITE);
        assert_eq!(frame.ams_header.invoke_id, 12345);
        assert_eq!(frame.payload.len(), 12);
    }

    #[test]
    fn test_ams_tcp_frame_serialize() {
        let ams_header = AmsHeader {
            target_net_id: AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
            target_port: 16150,
            source_net_id: AmsNetId::from_str("5.0.0.0.1.1").unwrap(),
            source_port: 50000,
            command_id: ADS_CMD_WRITE,
            state_flags: ADS_STATE_REQUEST,
            data_length: 0,
            error_code: 0,
            invoke_id: 99999,
        };

        let payload = vec![];
        let frame = AmsTcpFrame {
            tcp_header: AmsTcpHeader {
                reserved: 0,
                data_length: 32 + payload.len() as u32,
            },
            ams_header,
            payload,
        };

        let serialized = frame.serialize();
        let parsed = AmsTcpFrame::parse(&serialized).unwrap();
        assert_eq!(parsed.ams_header.invoke_id, 99999);
        assert_eq!(parsed.ams_header.command_id, ADS_CMD_WRITE);
    }

    #[test]
    fn test_ams_tcp_frame_make_response() {
        let request_frame = AmsTcpFrame {
            tcp_header: AmsTcpHeader {
                reserved: 0,
                data_length: 32 + 12,
            },
            ams_header: AmsHeader {
                target_net_id: AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
                target_port: 16150,
                source_net_id: AmsNetId::from_str("5.0.0.0.1.1").unwrap(),
                source_port: 50000,
                command_id: ADS_CMD_WRITE,
                state_flags: ADS_STATE_REQUEST,
                data_length: 12,
                error_code: 0,
                invoke_id: 77777,
            },
            payload: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        };

        let response_data = vec![0, 0, 0, 0];
        let response_frame = request_frame.make_response(0, response_data.clone());

        assert_eq!(
            response_frame.ams_header.target_net_id,
            request_frame.ams_header.source_net_id
        );
        assert_eq!(
            response_frame.ams_header.source_net_id,
            request_frame.ams_header.target_net_id
        );
        assert_eq!(response_frame.ams_header.state_flags, ADS_STATE_RESPONSE);
        assert_eq!(
            response_frame.ams_header.invoke_id,
            request_frame.ams_header.invoke_id
        );
        assert_eq!(response_frame.payload, response_data);
    }

    #[test]
    fn test_ams_tcp_frame_extract_write_request() {
        let mut payload = vec![0u8; 20];
        payload[0..4].copy_from_slice(&1u32.to_le_bytes());
        payload[4..8].copy_from_slice(&2u32.to_le_bytes());
        payload[8..12].copy_from_slice(&8u32.to_le_bytes());
        payload[12..20].copy_from_slice(b"testdata");

        let frame = AmsTcpFrame {
            tcp_header: AmsTcpHeader {
                reserved: 0,
                data_length: 32 + 20,
            },
            ams_header: AmsHeader {
                target_net_id: AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
                target_port: 16150,
                source_net_id: AmsNetId::from_str("5.0.0.0.1.1").unwrap(),
                source_port: 50000,
                command_id: ADS_CMD_WRITE,
                state_flags: ADS_STATE_REQUEST,
                data_length: 20,
                error_code: 0,
                invoke_id: 12345,
            },
            payload,
        };

        let write_req = frame.extract_write_request().unwrap();
        assert_eq!(write_req.index_group, 1);
        assert_eq!(write_req.index_offset, 2);
        assert_eq!(write_req.data, b"testdata");
    }

    #[test]
    fn test_ams_tcp_frame_extract_write_wrong_command() {
        let frame = AmsTcpFrame {
            tcp_header: AmsTcpHeader {
                reserved: 0,
                data_length: 32,
            },
            ams_header: AmsHeader {
                target_net_id: AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
                target_port: 16150,
                source_net_id: AmsNetId::from_str("5.0.0.0.1.1").unwrap(),
                source_port: 50000,
                command_id: 99,
                state_flags: ADS_STATE_REQUEST,
                data_length: 0,
                error_code: 0,
                invoke_id: 12345,
            },
            payload: vec![],
        };

        let result = frame.extract_write_request();
        assert!(result.is_err());
    }
}
