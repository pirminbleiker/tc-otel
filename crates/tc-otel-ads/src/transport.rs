//! Transport abstraction for AMS frames
//!
//! This module provides a trait-based abstraction for different transport mechanisms
//! (TCP, MQTT, etc.) that can deliver AMS frames to the ADS parser pipeline.

use crate::ams::AmsNetId;
use async_trait::async_trait;

/// AMS transport abstraction for pluggable frame delivery
#[async_trait]
pub trait AmsTransport: Send + Sync + 'static {
    /// Start the transport (listen for connections/subscribe to topics).
    /// This method should run indefinitely until an error occurs or shutdown is requested.
    async fn run(self: std::sync::Arc<Self>) -> crate::Result<()>;

    /// Send an AMS response frame to a destination.
    /// Implementations route the frame to the appropriate transport destination
    /// (TCP connection, MQTT topic, etc.).
    async fn send(&self, dest: AmsNetId, frame: Vec<u8>) -> crate::Result<()>;

    /// Get the local AMS Net ID used by this transport.
    fn local_net_id(&self) -> AmsNetId;
}

pub mod mqtt;
pub mod tcp;

pub use mqtt::{MqttAmsTransport, MqttTransportConfig};
pub use tcp::TcpAmsTransport;
