//! ADS (Automation Device Specification) binary protocol parser for Log4TC
//!
//! This crate handles parsing and serialization of the legacy ADS binary protocol
//! used for communication between TwinCAT PLC and the Log4TC service.
//!
//! The ADS protocol is a proprietary Beckhoff protocol for device communication.
//! Log4TC uses ADS for receiving log entries from TwinCAT PLCs.

pub mod ads_client;
pub mod ams;
pub mod ams_server;
pub mod connection_manager;
pub mod diagnostics;
pub mod diagnostics_observer;
pub mod diagnostics_poller;
pub mod diagnostics_push;
pub mod error;
pub mod health_metrics;
pub mod listener;
pub mod mqtt_health_metrics;
pub mod parser;
pub mod protocol;
pub mod registry;
pub mod router;
pub mod symbol;
pub mod transport;

pub use ads_client::{
    build_read_request_frame, build_read_response_frame, AdsClient, AdsReadRequest, AdsReadResponse,
};
pub use ams::{
    AdsWriteRequest, AmsHeader, AmsNetId, AmsTcpFrame, AmsTcpHeader, ADS_CMD_READ, ADS_CMD_WRITE,
    ADS_LOG_PORT, ADS_STATE_REQUEST, ADS_STATE_RESPONSE, AMS_TCP_PORT,
};
pub use ams_server::AmsTcpServer;
pub use connection_manager::{
    ConnectionConfig, ConnectionManager, ConnectionPermit, ConnectionRejection,
};
pub use diagnostics::{
    DiagEvent, IG_PUSH_DIAG, IO_PUSH_CYCLE_EXCEED_EDGE, IO_PUSH_RT_VIOLATION_EDGE,
    IO_PUSH_SNAPSHOT, PUSH_WIRE_VERSION,
};
pub use diagnostics_push::EdgeKind;
pub use error::{AdsError, Result};
pub use health_metrics::AdsHealthCollector;
pub use listener::AdsListener;
pub use mqtt_health_metrics::MqttHealthCollector;
pub use parser::AdsParser;
pub use protocol::{
    AdsLogEntry, AdsMetricEntry, AdsProtocolVersion, AdsSpanEntry, AdsSpanEvent, RegistrationKey,
    RegistrationMessage, TaskMetadata,
};
pub use registry::TaskRegistry;
pub use symbol::{
    parse_symbol_table, AdsSymbolEntry, AdsSymbolUploadInfo, ADSIGRP_SYM_UPLOAD,
    ADSIGRP_SYM_UPLOADINFO,
};
pub use transport::{AmsTransport, TcpAmsTransport};
