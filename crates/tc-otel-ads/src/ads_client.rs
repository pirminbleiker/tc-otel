//! ADS client for outbound TCP connections to TwinCAT PLCs
//!
//! Connects to a PLC's AMS/TCP port (48898) and sends ADS commands
//! to browse the symbol table.

use crate::ams::{
    AmsHeader, AmsNetId, AmsTcpFrame, AmsTcpHeader, ADS_CMD_READ, ADS_STATE_REQUEST,
    ADS_STATE_RESPONSE,
};
use crate::error::{AdsError, Result};
use crate::symbol::{
    AdsReadRequest, AdsReadResponse, SymbolTable, SymbolUploadInfo, ADSIGRP_SYM_UPLOAD,
    ADSIGRP_SYM_UPLOADINFO, MAX_SUBSCRIPTIONS_PER_PLC,
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;

/// Configuration for the ADS client.
#[derive(Debug, Clone)]
pub struct AdsClientConfig {
    /// Target PLC address (IP:port, default port is 48898)
    pub target_addr: SocketAddr,
    /// AMS Net ID of the local machine (source)
    pub source_net_id: AmsNetId,
    /// AMS Net ID of the target PLC
    pub target_net_id: AmsNetId,
    /// ADS port on the target PLC (typically 851 for first runtime)
    pub target_ads_port: u16,
    /// Source ADS port (arbitrary, used for routing responses)
    pub source_ads_port: u16,
    /// Connection timeout
    pub connect_timeout: Duration,
    /// Read/write timeout per operation
    pub operation_timeout: Duration,
}

impl AdsClientConfig {
    pub fn new(
        target_addr: SocketAddr,
        source_net_id: AmsNetId,
        target_net_id: AmsNetId,
        target_ads_port: u16,
    ) -> Self {
        Self {
            target_addr,
            source_net_id,
            target_net_id,
            target_ads_port,
            source_ads_port: 32768, // Default ephemeral port
            connect_timeout: Duration::from_secs(5),
            operation_timeout: Duration::from_secs(10),
        }
    }
}

/// ADS client for communicating with a TwinCAT PLC over AMS/TCP.
///
/// Connects to the PLC, sends ADS READ commands to browse the symbol table,
/// and caches the result for API queries.
pub struct AdsClient {
    config: AdsClientConfig,
    invoke_id: AtomicU32,
    cached_symbols: Arc<RwLock<Option<SymbolTable>>>,
}

impl AdsClient {
    pub fn new(config: AdsClientConfig) -> Self {
        Self {
            config,
            invoke_id: AtomicU32::new(1),
            cached_symbols: Arc::new(RwLock::new(None)),
        }
    }

    fn next_invoke_id(&self) -> u32 {
        self.invoke_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send an ADS READ request and return the response data.
    async fn ads_read(
        &self,
        stream: &mut TcpStream,
        index_group: u32,
        index_offset: u32,
        read_length: u32,
    ) -> Result<AdsReadResponse> {
        let invoke_id = self.next_invoke_id();

        // Build ADS READ request payload
        let read_req = AdsReadRequest {
            index_group,
            index_offset,
            read_length,
        };
        let payload = read_req.serialize();

        // Build AMS header
        let ams_header = AmsHeader {
            target_net_id: self.config.target_net_id,
            target_port: self.config.target_ads_port,
            source_net_id: self.config.source_net_id,
            source_port: self.config.source_ads_port,
            command_id: ADS_CMD_READ,
            state_flags: ADS_STATE_REQUEST,
            data_length: payload.len() as u32,
            error_code: 0,
            invoke_id,
        };

        // Build AMS/TCP frame
        let frame = AmsTcpFrame {
            tcp_header: AmsTcpHeader {
                reserved: 0,
                data_length: 32 + payload.len() as u32,
            },
            ams_header,
            payload,
        };

        let frame_bytes = frame.serialize();

        // Send request
        tokio::time::timeout(self.config.operation_timeout, stream.write_all(&frame_bytes))
            .await
            .map_err(|_| AdsError::IoError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "write timeout",
            )))?
            .map_err(AdsError::IoError)?;

        // Read response: first 6 bytes (AMS/TCP header)
        let mut tcp_header_buf = [0u8; 6];
        tokio::time::timeout(
            self.config.operation_timeout,
            stream.read_exact(&mut tcp_header_buf),
        )
        .await
        .map_err(|_| AdsError::IoError(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "read timeout",
        )))?
        .map_err(AdsError::IoError)?;

        let resp_tcp_header = AmsTcpHeader::parse(&tcp_header_buf)?;

        // Read the rest (AMS header + payload)
        let mut ams_data = vec![0u8; resp_tcp_header.data_length as usize];
        tokio::time::timeout(
            self.config.operation_timeout,
            stream.read_exact(&mut ams_data),
        )
        .await
        .map_err(|_| AdsError::IoError(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "read timeout",
        )))?
        .map_err(AdsError::IoError)?;

        // Parse AMS header (32 bytes)
        if ams_data.len() < 32 {
            return Err(AdsError::IncompleteMessage {
                expected: 32,
                got: ams_data.len(),
            });
        }
        let resp_ams_header = AmsHeader::parse(&ams_data[..32])?;

        // Verify response
        if resp_ams_header.state_flags != ADS_STATE_RESPONSE {
            return Err(AdsError::ParseError(format!(
                "Expected response (0x{:04X}), got state flags 0x{:04X}",
                ADS_STATE_RESPONSE, resp_ams_header.state_flags
            )));
        }
        if resp_ams_header.invoke_id != invoke_id {
            return Err(AdsError::ParseError(format!(
                "Invoke ID mismatch: sent {}, got {}",
                invoke_id, resp_ams_header.invoke_id
            )));
        }
        if resp_ams_header.error_code != 0 {
            return Err(AdsError::ParseError(format!(
                "AMS error code: 0x{:08X}",
                resp_ams_header.error_code
            )));
        }

        // Parse ADS READ response from the payload
        let resp_payload = &ams_data[32..];
        AdsReadResponse::parse(resp_payload)
    }

    /// Connect to the PLC and browse its symbol table.
    pub async fn browse_symbols(&self) -> Result<SymbolTable> {
        let mut stream = tokio::time::timeout(
            self.config.connect_timeout,
            TcpStream::connect(self.config.target_addr),
        )
        .await
        .map_err(|_| AdsError::IoError(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connect timeout",
        )))?
        .map_err(AdsError::IoError)?;

        // Step 1: Get upload info (symbol count + total data size)
        tracing::info!(
            target_net_id = %self.config.target_net_id,
            "Requesting symbol upload info from PLC"
        );
        let info_resp = self.ads_read(&mut stream, ADSIGRP_SYM_UPLOADINFO, 0, 24).await?;
        if info_resp.result != 0 {
            return Err(AdsError::ParseError(format!(
                "ADS read error: 0x{:08X}",
                info_resp.result
            )));
        }
        let upload_info = SymbolUploadInfo::parse(&info_resp.data)?;
        tracing::info!(
            symbol_count = upload_info.symbol_count,
            symbol_length = upload_info.symbol_length,
            "Symbol upload info received"
        );

        // Step 2: Download the full symbol table
        let table_resp = self
            .ads_read(
                &mut stream,
                ADSIGRP_SYM_UPLOAD,
                0,
                upload_info.symbol_length,
            )
            .await?;
        if table_resp.result != 0 {
            return Err(AdsError::ParseError(format!(
                "ADS read error: 0x{:08X}",
                table_resp.result
            )));
        }

        let table = SymbolTable::parse(&table_resp.data, upload_info.symbol_count)?;
        tracing::info!(
            parsed_count = table.len(),
            "Symbol table parsed successfully"
        );

        // Cache the result
        let mut cache = self.cached_symbols.write().await;
        *cache = Some(table.clone());

        Ok(table)
    }

    /// Get the cached symbol table (from last browse), if available.
    pub async fn cached_symbols(&self) -> Option<SymbolTable> {
        self.cached_symbols.read().await.clone()
    }

    /// Get the client configuration.
    pub fn config(&self) -> &AdsClientConfig {
        &self.config
    }

    /// Get the max subscriptions limit.
    pub fn max_subscriptions(&self) -> usize {
        MAX_SUBSCRIPTIONS_PER_PLC
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::AdsSymbolEntry;
    use std::str::FromStr;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    fn test_config(addr: SocketAddr) -> AdsClientConfig {
        AdsClientConfig {
            target_addr: addr,
            source_net_id: AmsNetId::from_str("10.0.0.1.1.1").unwrap(),
            target_net_id: AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
            target_ads_port: 851,
            source_ads_port: 32768,
            connect_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn test_client_config_new() {
        let addr: SocketAddr = "192.168.1.100:48898".parse().unwrap();
        let config = AdsClientConfig::new(
            addr,
            AmsNetId::from_str("10.0.0.1.1.1").unwrap(),
            AmsNetId::from_str("192.168.1.100.1.1").unwrap(),
            851,
        );
        assert_eq!(config.target_addr, addr);
        assert_eq!(config.target_ads_port, 851);
        assert_eq!(config.source_ads_port, 32768);
    }

    #[test]
    fn test_client_max_subscriptions() {
        let addr: SocketAddr = "127.0.0.1:48898".parse().unwrap();
        let client = AdsClient::new(test_config(addr));
        assert_eq!(client.max_subscriptions(), 500);
    }

    #[test]
    fn test_invoke_id_increments() {
        let addr: SocketAddr = "127.0.0.1:48898".parse().unwrap();
        let client = AdsClient::new(test_config(addr));
        let id1 = client.next_invoke_id();
        let id2 = client.next_invoke_id();
        let id3 = client.next_invoke_id();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[tokio::test]
    async fn test_cached_symbols_initially_none() {
        let addr: SocketAddr = "127.0.0.1:48898".parse().unwrap();
        let client = AdsClient::new(test_config(addr));
        assert!(client.cached_symbols().await.is_none());
    }

    /// Helper: spawn a mock PLC that responds to ADS READ requests
    async fn spawn_mock_plc(
        upload_info: SymbolUploadInfo,
        symbol_entries: Vec<AdsSymbolEntry>,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            loop {
                // Read AMS/TCP header (6 bytes)
                let mut tcp_hdr = [0u8; 6];
                if stream.read_exact(&mut tcp_hdr).await.is_err() {
                    break;
                }
                let tcp_header = AmsTcpHeader::parse(&tcp_hdr).unwrap();

                // Read AMS header + payload
                let mut ams_data = vec![0u8; tcp_header.data_length as usize];
                if stream.read_exact(&mut ams_data).await.is_err() {
                    break;
                }

                let ams_header = AmsHeader::parse(&ams_data[..32]).unwrap();
                let payload = &ams_data[32..];

                // Parse the ADS READ request
                let read_req = crate::symbol::AdsReadRequest::parse(payload).unwrap();

                // Build response data based on index group
                let response_data = if read_req.index_group == ADSIGRP_SYM_UPLOADINFO {
                    // Return upload info
                    let resp = AdsReadResponse {
                        result: 0,
                        data: upload_info.serialize(),
                    };
                    resp.serialize()
                } else if read_req.index_group == ADSIGRP_SYM_UPLOAD {
                    // Return symbol table
                    let mut table_data = Vec::new();
                    for entry in &symbol_entries {
                        table_data.extend(entry.serialize());
                    }
                    let resp = AdsReadResponse {
                        result: 0,
                        data: table_data,
                    };
                    resp.serialize()
                } else {
                    let resp = AdsReadResponse {
                        result: 0x0706, // unknown
                        data: vec![],
                    };
                    resp.serialize()
                };

                // Build response frame
                let resp_ams_header = AmsHeader {
                    target_net_id: ams_header.source_net_id,
                    target_port: ams_header.source_port,
                    source_net_id: ams_header.target_net_id,
                    source_port: ams_header.target_port,
                    command_id: ams_header.command_id,
                    state_flags: ADS_STATE_RESPONSE,
                    data_length: response_data.len() as u32,
                    error_code: 0,
                    invoke_id: ams_header.invoke_id,
                };

                let resp_tcp_header = AmsTcpHeader {
                    reserved: 0,
                    data_length: 32 + response_data.len() as u32,
                };

                let resp_frame = AmsTcpFrame {
                    tcp_header: resp_tcp_header,
                    ams_header: resp_ams_header,
                    payload: response_data,
                };

                if stream.write_all(&resp_frame.serialize()).await.is_err() {
                    break;
                }
            }
        });

        addr
    }

    #[tokio::test]
    async fn test_browse_symbols_end_to_end() {
        let entries = vec![
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 0,
                size: 4,
                data_type: 3,
                flags: 0x0008,
                name: "MAIN.nCounter".to_string(),
                type_name: "INT".to_string(),
                comment: "Cycle counter".to_string(),
            },
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 4,
                size: 4,
                data_type: 4,
                flags: 0x0008,
                name: "MAIN.fTemperature".to_string(),
                type_name: "REAL".to_string(),
                comment: "Process temperature".to_string(),
            },
            AdsSymbolEntry {
                index_group: 0x4020,
                index_offset: 8,
                size: 1,
                data_type: 33,
                flags: 0x0008,
                name: "GVL.bEnabled".to_string(),
                type_name: "BOOL".to_string(),
                comment: String::new(),
            },
        ];

        // Calculate total symbol data length
        let total_length: u32 = entries.iter().map(|e| e.serialize().len() as u32).sum();

        let upload_info = SymbolUploadInfo {
            symbol_count: entries.len() as u32,
            symbol_length: total_length,
            data_type_count: 0,
            data_type_length: 0,
            extra_count: 0,
            extra_length: 0,
        };

        let addr = spawn_mock_plc(upload_info, entries).await;

        let client = AdsClient::new(test_config(addr));
        let table = client.browse_symbols().await.unwrap();

        assert_eq!(table.len(), 3);
        assert_eq!(table.get("MAIN.nCounter").unwrap().type_name, "INT");
        assert_eq!(table.get("MAIN.fTemperature").unwrap().size, 4);
        assert_eq!(table.get("GVL.bEnabled").unwrap().data_type, 33);

        // Verify caching
        let cached = client.cached_symbols().await.unwrap();
        assert_eq!(cached.len(), 3);
    }

    #[tokio::test]
    async fn test_browse_empty_symbol_table() {
        let upload_info = SymbolUploadInfo {
            symbol_count: 0,
            symbol_length: 0,
            data_type_count: 0,
            data_type_length: 0,
            extra_count: 0,
            extra_length: 0,
        };

        let addr = spawn_mock_plc(upload_info, vec![]).await;
        let client = AdsClient::new(test_config(addr));
        let table = client.browse_symbols().await.unwrap();
        assert!(table.is_empty());
    }

    #[tokio::test]
    async fn test_connect_timeout() {
        // Use a non-routable address to trigger connection timeout
        let addr: SocketAddr = "192.0.2.1:48898".parse().unwrap();
        let mut config = test_config(addr);
        config.connect_timeout = Duration::from_millis(100);

        let client = AdsClient::new(config);
        let result = client.browse_symbols().await;
        assert!(result.is_err());
    }
}
