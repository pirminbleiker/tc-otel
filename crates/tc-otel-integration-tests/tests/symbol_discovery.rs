//! Integration tests for ADS symbol discovery
//!
//! Tests cover:
//! - Binary parsing of symbol upload info and symbol table responses
//! - ADS READ request/response frame construction
//! - End-to-end mock PLC symbol browsing via TCP
//! - API handler integration with symbol store

use tc_otel_ads::{
    build_read_request_frame, build_read_response_frame, parse_symbol_table, AdsReadRequest,
    AdsReadResponse, AdsSymbolEntry, AdsSymbolUploadInfo, AmsHeader, AmsNetId, AmsTcpFrame,
    AmsTcpHeader, ADS_CMD_READ, ADS_STATE_REQUEST, ADS_STATE_RESPONSE, ADSIGRP_SYM_UPLOAD,
    ADSIGRP_SYM_UPLOADINFO,
};

// --- Binary parsing integration tests ---

#[test]
fn test_symbol_upload_info_full_roundtrip() {
    // Simulate the full binary exchange for ADSIGRP_SYM_UPLOADINFO
    let source = AmsNetId::from_str_ref("10.0.0.1.1.1").unwrap();
    let target = AmsNetId::from_str_ref("5.80.201.232.1.1").unwrap();

    // Build request frame
    let request = build_read_request_frame(source, 32768, target, 851, ADSIGRP_SYM_UPLOADINFO, 0, 8, 1);

    // Verify request frame structure
    assert_eq!(request.ams_header.command_id, ADS_CMD_READ);
    assert_eq!(request.ams_header.state_flags, ADS_STATE_REQUEST);
    let read_req = AdsReadRequest::parse(&request.payload).unwrap();
    assert_eq!(read_req.index_group, ADSIGRP_SYM_UPLOADINFO);
    assert_eq!(read_req.read_length, 8);

    // Build response with symbol info
    let info = AdsSymbolUploadInfo {
        symbol_count: 150,
        symbol_size: 12800,
    };
    let response = build_read_response_frame(&request, 0, &info.serialize());

    // Verify response structure
    assert_eq!(response.ams_header.state_flags, ADS_STATE_RESPONSE);
    assert_eq!(response.ams_header.invoke_id, 1);

    // Parse response payload
    let read_resp = AdsReadResponse::parse(&response.payload).unwrap();
    assert_eq!(read_resp.result, 0);
    let parsed_info = AdsSymbolUploadInfo::parse(&read_resp.data).unwrap();
    assert_eq!(parsed_info.symbol_count, 150);
    assert_eq!(parsed_info.symbol_size, 12800);
}

#[test]
fn test_symbol_table_full_roundtrip() {
    let source = AmsNetId::from_str_ref("10.0.0.1.1.1").unwrap();
    let target = AmsNetId::from_str_ref("5.80.201.232.1.1").unwrap();

    // Create a realistic symbol table
    let symbols = vec![
        AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 0,
            size: 1,
            data_type: 33, // BOOL
            flags: 0x0008,
            name: "MAIN.bMotorRunning".to_string(),
            type_name: "BOOL".to_string(),
            comment: "Motor running status flag".to_string(),
        },
        AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 8,
            size: 8,
            data_type: 5, // LREAL
            flags: 0x0008,
            name: "MAIN.fTemperature".to_string(),
            type_name: "LREAL".to_string(),
            comment: "Motor temperature in Celsius".to_string(),
        },
        AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 16,
            size: 2,
            data_type: 3, // INT
            flags: 0x0008,
            name: "GVL.nCycleCount".to_string(),
            type_name: "INT".to_string(),
            comment: String::new(),
        },
        AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 18,
            size: 81,
            data_type: 30, // STRING
            flags: 0x0008,
            name: "GVL.sMessage".to_string(),
            type_name: "STRING(80)".to_string(),
            comment: "Status message".to_string(),
        },
        AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 100,
            size: 4,
            data_type: 4, // REAL
            flags: 0x0008,
            name: "MAIN.fSpeed".to_string(),
            type_name: "REAL".to_string(),
            comment: "Motor speed in RPM".to_string(),
        },
    ];

    // Serialize the symbol table
    let mut table_data = Vec::new();
    for sym in &symbols {
        table_data.extend(sym.serialize());
    }

    // Build request frame for full table
    let request = build_read_request_frame(
        source,
        32768,
        target,
        851,
        ADSIGRP_SYM_UPLOAD,
        0,
        table_data.len() as u32,
        2,
    );

    // Build response
    let response = build_read_response_frame(&request, 0, &table_data);

    // Parse response and extract symbol table
    let read_resp = AdsReadResponse::parse(&response.payload).unwrap();
    assert_eq!(read_resp.result, 0);

    let parsed_symbols = parse_symbol_table(&read_resp.data).unwrap();
    assert_eq!(parsed_symbols.len(), 5);

    // Verify each symbol
    assert_eq!(parsed_symbols[0].name, "MAIN.bMotorRunning");
    assert_eq!(parsed_symbols[0].type_name, "BOOL");
    assert_eq!(parsed_symbols[0].size, 1);
    assert_eq!(parsed_symbols[0].comment, "Motor running status flag");

    assert_eq!(parsed_symbols[1].name, "MAIN.fTemperature");
    assert_eq!(parsed_symbols[1].type_name, "LREAL");
    assert_eq!(parsed_symbols[1].size, 8);

    assert_eq!(parsed_symbols[2].name, "GVL.nCycleCount");
    assert_eq!(parsed_symbols[2].type_name, "INT");
    assert_eq!(parsed_symbols[2].comment, "");

    assert_eq!(parsed_symbols[3].name, "GVL.sMessage");
    assert_eq!(parsed_symbols[3].type_name, "STRING(80)");
    assert_eq!(parsed_symbols[3].size, 81);

    assert_eq!(parsed_symbols[4].name, "MAIN.fSpeed");
    assert_eq!(parsed_symbols[4].type_name, "REAL");
}

#[test]
fn test_frame_serialization_through_wire() {
    // Simulate serializing a frame to bytes, sending over the wire, and parsing back
    let source = AmsNetId::from_str_ref("192.168.1.50.1.1").unwrap();
    let target = AmsNetId::from_str_ref("192.168.1.100.1.1").unwrap();

    let request = build_read_request_frame(source, 32768, target, 851, ADSIGRP_SYM_UPLOADINFO, 0, 8, 42);

    // Serialize to wire format
    let wire_bytes = request.serialize();

    // Parse from wire format (simulating the receiver side)
    let received = AmsTcpFrame::parse(&wire_bytes).unwrap();
    assert_eq!(received.ams_header.command_id, ADS_CMD_READ);
    assert_eq!(received.ams_header.invoke_id, 42);
    assert_eq!(received.ams_header.target_port, 851);
    assert_eq!(received.ams_header.source_port, 32768);

    // Parse the ADS READ request from the payload
    let read_req = AdsReadRequest::parse(&received.payload).unwrap();
    assert_eq!(read_req.index_group, ADSIGRP_SYM_UPLOADINFO);
    assert_eq!(read_req.index_offset, 0);
    assert_eq!(read_req.read_length, 8);
}

#[test]
fn test_response_frame_serialization_through_wire() {
    let source = AmsNetId::from_str_ref("10.0.0.1.1.1").unwrap();
    let target = AmsNetId::from_str_ref("5.80.201.232.1.1").unwrap();

    let request = build_read_request_frame(source, 32768, target, 851, ADSIGRP_SYM_UPLOAD, 0, 1024, 7);

    let symbol = AdsSymbolEntry {
        index_group: 0x4020,
        index_offset: 0,
        size: 4,
        data_type: 19, // UDINT
        flags: 0,
        name: "MAIN.nCounter".to_string(),
        type_name: "UDINT".to_string(),
        comment: "Main cycle counter".to_string(),
    };
    let table_data = symbol.serialize();
    let response = build_read_response_frame(&request, 0, &table_data);

    // Serialize and re-parse (simulate wire transfer)
    let wire_bytes = response.serialize();
    let received = AmsTcpFrame::parse(&wire_bytes).unwrap();

    assert_eq!(received.ams_header.state_flags, ADS_STATE_RESPONSE);
    assert_eq!(received.ams_header.invoke_id, 7);

    let read_resp = AdsReadResponse::parse(&received.payload).unwrap();
    assert_eq!(read_resp.result, 0);

    let symbols = parse_symbol_table(&read_resp.data).unwrap();
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "MAIN.nCounter");
    assert_eq!(symbols[0].type_name, "UDINT");
}

#[test]
fn test_ads_read_error_response() {
    let source = AmsNetId::from_str_ref("10.0.0.1.1.1").unwrap();
    let target = AmsNetId::from_str_ref("5.80.201.232.1.1").unwrap();

    let request = build_read_request_frame(source, 32768, target, 851, ADSIGRP_SYM_UPLOADINFO, 0, 8, 1);

    // Build response with ADS error code (device not found)
    let response = build_read_response_frame(&request, 0x0706, &[]);

    let read_resp = AdsReadResponse::parse(&response.payload).unwrap();
    assert_eq!(read_resp.result, 0x0706);
    assert!(read_resp.data.is_empty());
}

// --- Mock PLC server integration test ---

#[tokio::test]
async fn test_mock_plc_symbol_browse_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // Set up mock PLC server
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Create symbol table for the mock PLC
    let mock_symbols = vec![
        AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 0,
            size: 1,
            data_type: 33,
            flags: 0x0008,
            name: "MAIN.bReady".to_string(),
            type_name: "BOOL".to_string(),
            comment: "System ready".to_string(),
        },
        AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: 8,
            size: 8,
            data_type: 5,
            flags: 0x0008,
            name: "MAIN.fPosition".to_string(),
            type_name: "LREAL".to_string(),
            comment: "Axis position".to_string(),
        },
    ];

    let mut table_data = Vec::new();
    for sym in &mock_symbols {
        table_data.extend(sym.serialize());
    }

    let table_data_clone = table_data.clone();

    // Spawn mock PLC server
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        // Handle two requests: upload info then upload table

        // Request 1: ADSIGRP_SYM_UPLOADINFO
        let mut tcp_buf = [0u8; 6];
        stream.read_exact(&mut tcp_buf).await.unwrap();
        let tcp_header = AmsTcpHeader::parse(&tcp_buf).unwrap();
        let mut rest = vec![0u8; tcp_header.data_length as usize];
        stream.read_exact(&mut rest).await.unwrap();

        let ams_header = AmsHeader::parse(&rest[..32]).unwrap();
        assert_eq!(ams_header.command_id, ADS_CMD_READ);
        let read_req = AdsReadRequest::parse(&rest[32..]).unwrap();
        assert_eq!(read_req.index_group, ADSIGRP_SYM_UPLOADINFO);

        // Respond with symbol info
        let info = AdsSymbolUploadInfo {
            symbol_count: 2,
            symbol_size: table_data_clone.len() as u32,
        };
        let response_payload = AdsReadResponse {
            result: 0,
            data: info.serialize(),
        };
        let resp_data = response_payload.serialize();

        let resp_ams = AmsHeader {
            target_net_id: ams_header.source_net_id,
            target_port: ams_header.source_port,
            source_net_id: ams_header.target_net_id,
            source_port: ams_header.target_port,
            command_id: ADS_CMD_READ,
            state_flags: ADS_STATE_RESPONSE,
            data_length: resp_data.len() as u32,
            error_code: 0,
            invoke_id: ams_header.invoke_id,
        };

        let resp_tcp = AmsTcpHeader {
            reserved: 0,
            data_length: 32 + resp_data.len() as u32,
        };

        stream.write_all(&resp_tcp.serialize()).await.unwrap();
        stream.write_all(&resp_ams.serialize()).await.unwrap();
        stream.write_all(&resp_data).await.unwrap();

        // Request 2: ADSIGRP_SYM_UPLOAD
        let mut tcp_buf2 = [0u8; 6];
        stream.read_exact(&mut tcp_buf2).await.unwrap();
        let tcp_header2 = AmsTcpHeader::parse(&tcp_buf2).unwrap();
        let mut rest2 = vec![0u8; tcp_header2.data_length as usize];
        stream.read_exact(&mut rest2).await.unwrap();

        let ams_header2 = AmsHeader::parse(&rest2[..32]).unwrap();
        let read_req2 = AdsReadRequest::parse(&rest2[32..]).unwrap();
        assert_eq!(read_req2.index_group, ADSIGRP_SYM_UPLOAD);

        // Respond with full symbol table
        let response_payload2 = AdsReadResponse {
            result: 0,
            data: table_data_clone,
        };
        let resp_data2 = response_payload2.serialize();

        let resp_ams2 = AmsHeader {
            target_net_id: ams_header2.source_net_id,
            target_port: ams_header2.source_port,
            source_net_id: ams_header2.target_net_id,
            source_port: ams_header2.target_port,
            command_id: ADS_CMD_READ,
            state_flags: ADS_STATE_RESPONSE,
            data_length: resp_data2.len() as u32,
            error_code: 0,
            invoke_id: ams_header2.invoke_id,
        };

        let resp_tcp2 = AmsTcpHeader {
            reserved: 0,
            data_length: 32 + resp_data2.len() as u32,
        };

        stream.write_all(&resp_tcp2.serialize()).await.unwrap();
        stream.write_all(&resp_ams2.serialize()).await.unwrap();
        stream.write_all(&resp_data2).await.unwrap();
    });

    // Connect ADS client to mock PLC
    let source = AmsNetId::from_str_ref("10.0.0.1.1.1").unwrap();
    let target = AmsNetId::from_str_ref("5.80.201.232.1.1").unwrap();

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut client = tc_otel_ads::AdsClient::from_stream(stream, source, 32768, target, 851);

    // Read the full symbol table (this calls read_symbol_info + read_symbol_table)
    let symbols = client.read_symbol_table().await.unwrap();

    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[0].name, "MAIN.bReady");
    assert_eq!(symbols[0].type_name, "BOOL");
    assert_eq!(symbols[0].size, 1);
    assert_eq!(symbols[0].comment, "System ready");
    assert_eq!(symbols[1].name, "MAIN.fPosition");
    assert_eq!(symbols[1].type_name, "LREAL");
    assert_eq!(symbols[1].size, 8);
    assert_eq!(symbols[1].comment, "Axis position");

    server_handle.await.unwrap();
}

// --- Large symbol table stress test ---

#[test]
fn test_parse_large_symbol_table() {
    // Generate a realistic large symbol table (close to 500 limit)
    let mut symbols = Vec::new();
    for i in 0..500 {
        symbols.push(AdsSymbolEntry {
            index_group: 0x4020,
            index_offset: i * 8,
            size: 8,
            data_type: 5,
            flags: 0x0008,
            name: format!("GVL.fSensor_{:04}", i),
            type_name: "LREAL".to_string(),
            comment: format!("Sensor {} value", i),
        });
    }

    let mut table_data = Vec::new();
    for sym in &symbols {
        table_data.extend(sym.serialize());
    }

    let parsed = parse_symbol_table(&table_data).unwrap();
    assert_eq!(parsed.len(), 500);

    // Verify first and last entries
    assert_eq!(parsed[0].name, "GVL.fSensor_0000");
    assert_eq!(parsed[499].name, "GVL.fSensor_0499");
}
