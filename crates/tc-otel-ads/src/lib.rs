//! ADS (Automation Device Specification) binary protocol parser for Log4TC
//!
//! This crate handles parsing and serialization of the legacy ADS binary protocol
//! used for communication between TwinCAT PLC and the Log4TC service.
//!
//! The ADS protocol is a proprietary Beckhoff protocol for device communication.
//! Log4TC uses ADS for receiving log entries from TwinCAT PLCs.

pub mod ams;
pub mod ams_server;
pub mod connection_manager;
pub mod error;
pub mod listener;
pub mod parser;
pub mod protocol;
pub mod registry;

pub use ams::{
    AdsWriteRequest, AmsHeader, AmsNetId, AmsTcpFrame, AmsTcpHeader, ADS_CMD_WRITE, ADS_LOG_PORT,
    ADS_STATE_REQUEST, ADS_STATE_RESPONSE, AMS_TCP_PORT,
};
pub use ams_server::AmsTcpServer;
pub use connection_manager::{
    ConnectionConfig, ConnectionManager, ConnectionPermit, ConnectionRejection,
};
pub use error::{AdsError, Result};
pub use listener::AdsListener;
pub use parser::AdsParser;
pub use protocol::{
    AdsLogEntry, AdsProtocolVersion, AdsSpanEntry, AdsSpanEvent, RegistrationKey,
    RegistrationMessage, TaskMetadata,
};
pub use registry::TaskRegistry;
