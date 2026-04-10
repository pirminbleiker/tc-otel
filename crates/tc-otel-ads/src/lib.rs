//! ADS (Automation Device Specification) binary protocol parser for Log4TC
//!
//! This crate handles parsing and serialization of the legacy ADS binary protocol
//! used for communication between TwinCAT PLC and the Log4TC service.
//!
//! The ADS protocol is a proprietary Beckhoff protocol for device communication.
//! Log4TC uses ADS for receiving log entries from TwinCAT PLCs.

pub mod protocol;
pub mod parser;
pub mod error;
pub mod listener;
pub mod ams;
pub mod ams_server;
pub mod registry;

pub use protocol::{AdsLogEntry, AdsProtocolVersion, RegistrationMessage, RegistrationKey, TaskMetadata};
pub use parser::AdsParser;
pub use error::{Result, AdsError};
pub use listener::AdsListener;
pub use ams::{
    AmsNetId, AmsHeader, AmsTcpHeader, AdsWriteRequest, AmsTcpFrame,
    AMS_TCP_PORT, ADS_CMD_WRITE, ADS_STATE_REQUEST, ADS_STATE_RESPONSE, ADS_LOG_PORT,
};
pub use ams_server::AmsTcpServer;
pub use registry::TaskRegistry;
