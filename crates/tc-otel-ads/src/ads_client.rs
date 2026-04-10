//! ADS TCP client for outbound connections to TwinCAT PLCs
//!
//! Provides symbol discovery via ADS READ commands:
//! - `read_symbol_info`: reads symbol count and total table size
//! - `read_symbol_table`: reads and parses the full symbol table
//!
//! Frames all requests as proper AMS/TCP with AMS routing headers.

use crate::ams::{
    AmsHeader, AmsNetId, AmsTcpFrame, AmsTcpHeader, ADS_CMD_READ, ADS_STATE_REQUEST,
    ADS_STATE_RESPONSE, AMS_TCP_PORT,
};
use crate::error::{AdsError, Result};
use crate::symbol::{
    parse_symbol_table, AdsSymbolEntry, AdsSymbolUploadInfo, ADSIGRP_SYM_UPLOAD,
    ADSIGRP_SYM_UPLOADINFO,
};
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// ADS READ request payload
///
/// Binary layout:
/// ```text
/// [index_group: u32 LE]
/// [index_offset: u32 LE]
/// [read_length: u32 LE]
/// ```
#[derive(Debug, Clone)]
pub struct AdsReadRequest {
    pub index_group: u32,
    pub index_offset: u32,
    pub read_length: u32,
}

impl AdsReadRequest {
    /// Serialize to binary (12 bytes)
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&self.index_group.to_le_bytes());
        buf.extend_from_slice(&self.index_offset.to_le_bytes());
        buf.extend_from_slice(&self.read_length.to_le_bytes());
        buf
    }

    /// Parse from binary (12 bytes)
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 12 {
            return Err(AdsError::IncompleteMessage {
                expected: 12,
                got: data.len(),
            });
        }

        Ok(AdsReadRequest {
            index_group: u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            index_offset: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
            read_length: u32::from_le_bytes([data[8], data[9], data[10], data[11]]),
        })
    }
}

/// ADS READ response payload
///
/// Binary layout:
/// ```text
/// [result: u32 LE]      -- ADS error code (0 = success)
/// [read_length: u32 LE]  -- actual bytes read
/// [data: u8 * read_length]
/// ```
#[derive(Debug, Clone)]
pub struct AdsReadResponse {
    pub result: u32,
    pub data: Vec<u8>,
}

impl AdsReadResponse {
    /// Parse from binary
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(AdsError::IncompleteMessage {
                expected: 8,
                got: data.len(),
            });
        }

        let result = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let read_length = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;

        if data.len() < 8 + read_length {
            return Err(AdsError::IncompleteMessage {
                expected: 8 + read_length,
                got: data.len(),
            });
        }

        Ok(AdsReadResponse {
            result,
            data: data[8..8 + read_length].to_vec(),
        })
    }

    /// Serialize to binary
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.data.len());
        buf.extend_from_slice(&self.result.to_le_bytes());
        buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }
}

/// TCP client for ADS communication with a TwinCAT PLC
pub struct AdsClient {
    stream: TcpStream,
    source_net_id: AmsNetId,
    source_port: u16,
    target_net_id: AmsNetId,
    target_port: u16,
    invoke_id: AtomicU32,
}

impl AdsClient {
    /// Connect to a PLC at the given address on the AMS TCP port (48898)
    pub async fn connect(
        addr: &str,
        source_net_id: AmsNetId,
        source_port: u16,
        target_net_id: AmsNetId,
        target_port: u16,
    ) -> Result<Self> {
        let target = format!("{}:{}", addr, AMS_TCP_PORT);
        let stream = TcpStream::connect(&target).await?;
        Ok(AdsClient {
            stream,
            source_net_id,
            source_port,
            target_net_id,
            target_port,
            invoke_id: AtomicU32::new(1),
        })
    }

    /// Create an AdsClient from an existing TcpStream (for testing)
    pub fn from_stream(
        stream: TcpStream,
        source_net_id: AmsNetId,
        source_port: u16,
        target_net_id: AmsNetId,
        target_port: u16,
    ) -> Self {
        AdsClient {
            stream,
            source_net_id,
            source_port,
            target_net_id,
            target_port,
            invoke_id: AtomicU32::new(1),
        }
    }

    /// Send an ADS READ request and receive the response
    async fn ads_read(
        &mut self,
        index_group: u32,
        index_offset: u32,
        read_length: u32,
    ) -> Result<AdsReadResponse> {
        let invoke_id = self.invoke_id.fetch_add(1, Ordering::Relaxed);

        let read_req = AdsReadRequest {
            index_group,
            index_offset,
            read_length,
        };
        let payload = read_req.serialize();

        let ams_header = AmsHeader {
            target_net_id: self.target_net_id,
            target_port: self.target_port,
            source_net_id: self.source_net_id,
            source_port: self.source_port,
            command_id: ADS_CMD_READ,
            state_flags: ADS_STATE_REQUEST,
            data_length: payload.len() as u32,
            error_code: 0,
            invoke_id,
        };

        let tcp_header = AmsTcpHeader {
            reserved: 0,
            data_length: 32 + payload.len() as u32,
        };

        let frame = AmsTcpFrame {
            tcp_header,
            ams_header,
            payload,
        };

        // Send
        let frame_bytes = frame.serialize();
        self.stream.write_all(&frame_bytes).await?;

        // Receive TCP header (6 bytes)
        let mut tcp_buf = [0u8; 6];
        self.stream.read_exact(&mut tcp_buf).await?;
        let resp_tcp = AmsTcpHeader::parse(&tcp_buf)?;

        // Receive rest of the frame (AMS header + payload)
        let mut rest = vec![0u8; resp_tcp.data_length as usize];
        self.stream.read_exact(&mut rest).await?;

        // Parse AMS header
        if rest.len() < 32 {
            return Err(AdsError::IncompleteMessage {
                expected: 32,
                got: rest.len(),
            });
        }
        let resp_ams = AmsHeader::parse(&rest[..32])?;

        // Verify response
        if resp_ams.state_flags != ADS_STATE_RESPONSE {
            return Err(AdsError::ParseError(format!(
                "expected response state flags {:#06x}, got {:#06x}",
                ADS_STATE_RESPONSE, resp_ams.state_flags
            )));
        }
        if resp_ams.error_code != 0 {
            return Err(AdsError::ParseError(format!(
                "ADS error code: {:#010x}",
                resp_ams.error_code
            )));
        }

        // Parse READ response from payload
        AdsReadResponse::parse(&rest[32..])
    }

    /// Read symbol upload info: number of symbols and total table size
    pub async fn read_symbol_info(&mut self) -> Result<AdsSymbolUploadInfo> {
        let response = self.ads_read(ADSIGRP_SYM_UPLOADINFO, 0, 8).await?;
        if response.result != 0 {
            return Err(AdsError::ParseError(format!(
                "ADS READ error for symbol info: {:#010x}",
                response.result
            )));
        }
        AdsSymbolUploadInfo::parse(&response.data)
    }

    /// Read and parse the full symbol table from the PLC
    pub async fn read_symbol_table(&mut self) -> Result<Vec<AdsSymbolEntry>> {
        // First, get the table size
        let info = self.read_symbol_info().await?;

        // Read the full symbol table
        let response = self
            .ads_read(ADSIGRP_SYM_UPLOAD, 0, info.symbol_size)
            .await?;
        if response.result != 0 {
            return Err(AdsError::ParseError(format!(
                "ADS READ error for symbol table: {:#010x}",
                response.result
            )));
        }

        parse_symbol_table(&response.data)
    }
}

/// Build an AMS/TCP frame for an ADS READ request (for testing/external use)
pub fn build_read_request_frame(
    source_net_id: AmsNetId,
    source_port: u16,
    target_net_id: AmsNetId,
    target_port: u16,
    index_group: u32,
    index_offset: u32,
    read_length: u32,
    invoke_id: u32,
) -> AmsTcpFrame {
    let read_req = AdsReadRequest {
        index_group,
        index_offset,
        read_length,
    };
    let payload = read_req.serialize();

    let ams_header = AmsHeader {
        target_net_id,
        target_port,
        source_net_id,
        source_port,
        command_id: ADS_CMD_READ,
        state_flags: ADS_STATE_REQUEST,
        data_length: payload.len() as u32,
        error_code: 0,
        invoke_id,
    };

    let tcp_header = AmsTcpHeader {
        reserved: 0,
        data_length: 32 + payload.len() as u32,
    };

    AmsTcpFrame {
        tcp_header,
        ams_header,
        payload,
    }
}

/// Build an AMS/TCP response frame for an ADS READ response (for mock servers)
pub fn build_read_response_frame(
    request: &AmsTcpFrame,
    result_code: u32,
    data: &[u8],
) -> AmsTcpFrame {
    let response = AdsReadResponse {
        result: result_code,
        data: data.to_vec(),
    };
    let payload = response.serialize();
    request.make_response(0, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_request_serialize_parse() {
        let req = AdsReadRequest {
            index_group: ADSIGRP_SYM_UPLOADINFO,
            index_offset: 0,
            read_length: 8,
        };

        let bytes = req.serialize();
        assert_eq!(bytes.len(), 12);

        let parsed = AdsReadRequest::parse(&bytes).unwrap();
        assert_eq!(parsed.index_group, ADSIGRP_SYM_UPLOADINFO);
        assert_eq!(parsed.index_offset, 0);
        assert_eq!(parsed.read_length, 8);
    }

    #[test]
    fn test_read_request_too_short() {
        let data = [0u8; 8];
        assert!(AdsReadRequest::parse(&data).is_err());
    }

    #[test]
    fn test_read_response_serialize_parse() {
        let resp = AdsReadResponse {
            result: 0,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };

        let bytes = resp.serialize();
        let parsed = AdsReadResponse::parse(&bytes).unwrap();
        assert_eq!(parsed.result, 0);
        assert_eq!(parsed.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_read_response_error_code() {
        let resp = AdsReadResponse {
            result: 0x0706, // ADSERR_DEVICE_SYMBOLNOTFOUND
            data: vec![],
        };

        let bytes = resp.serialize();
        let parsed = AdsReadResponse::parse(&bytes).unwrap();
        assert_eq!(parsed.result, 0x0706);
        assert!(parsed.data.is_empty());
    }

    #[test]
    fn test_read_response_too_short() {
        let data = [0u8; 4];
        assert!(AdsReadResponse::parse(&data).is_err());
    }

    #[test]
    fn test_read_response_data_truncated() {
        // Header says 100 bytes of data but only 4 bytes available
        let mut data = vec![0u8; 12];
        data[0..4].copy_from_slice(&0u32.to_le_bytes()); // result = 0
        data[4..8].copy_from_slice(&100u32.to_le_bytes()); // read_length = 100
                                                           // only 4 bytes of actual data follow
        assert!(AdsReadResponse::parse(&data).is_err());
    }

    #[test]
    fn test_build_read_request_frame() {
        let source = AmsNetId::from_str_ref("10.0.0.1.1.1").unwrap();
        let target = AmsNetId::from_str_ref("5.80.201.232.1.1").unwrap();

        let frame =
            build_read_request_frame(source, 32768, target, 851, ADSIGRP_SYM_UPLOADINFO, 0, 8, 1);

        assert_eq!(frame.ams_header.command_id, ADS_CMD_READ);
        assert_eq!(frame.ams_header.state_flags, ADS_STATE_REQUEST);
        assert_eq!(frame.ams_header.invoke_id, 1);

        // Payload should be 12 bytes (ADS READ request)
        assert_eq!(frame.payload.len(), 12);

        let req = AdsReadRequest::parse(&frame.payload).unwrap();
        assert_eq!(req.index_group, ADSIGRP_SYM_UPLOADINFO);
        assert_eq!(req.read_length, 8);
    }

    #[test]
    fn test_build_read_response_frame() {
        let source = AmsNetId::from_str_ref("10.0.0.1.1.1").unwrap();
        let target = AmsNetId::from_str_ref("5.80.201.232.1.1").unwrap();

        let request_frame =
            build_read_request_frame(source, 32768, target, 851, ADSIGRP_SYM_UPLOADINFO, 0, 8, 42);

        let info = AdsSymbolUploadInfo {
            symbol_count: 10,
            symbol_size: 500,
        };
        let response_frame = build_read_response_frame(&request_frame, 0, &info.serialize());

        // Source/target should be swapped
        assert_eq!(
            response_frame.ams_header.target_net_id,
            request_frame.ams_header.source_net_id
        );
        assert_eq!(
            response_frame.ams_header.source_net_id,
            request_frame.ams_header.target_net_id
        );
        assert_eq!(response_frame.ams_header.state_flags, ADS_STATE_RESPONSE);
        assert_eq!(response_frame.ams_header.invoke_id, 42);

        // Parse the READ response from the payload
        let read_resp = AdsReadResponse::parse(&response_frame.payload).unwrap();
        assert_eq!(read_resp.result, 0);
        let parsed_info = AdsSymbolUploadInfo::parse(&read_resp.data).unwrap();
        assert_eq!(parsed_info.symbol_count, 10);
        assert_eq!(parsed_info.symbol_size, 500);
    }

    #[test]
    fn test_read_request_frame_serialization_roundtrip() {
        let source = AmsNetId::from_str_ref("10.0.0.1.1.1").unwrap();
        let target = AmsNetId::from_str_ref("5.80.201.232.1.1").unwrap();

        let frame =
            build_read_request_frame(source, 32768, target, 851, ADSIGRP_SYM_UPLOAD, 0, 65536, 99);

        let bytes = frame.serialize();
        let parsed = AmsTcpFrame::parse(&bytes).unwrap();

        assert_eq!(parsed.ams_header.command_id, ADS_CMD_READ);
        assert_eq!(parsed.ams_header.invoke_id, 99);

        let req = AdsReadRequest::parse(&parsed.payload).unwrap();
        assert_eq!(req.index_group, ADSIGRP_SYM_UPLOAD);
        assert_eq!(req.read_length, 65536);
    }
}
