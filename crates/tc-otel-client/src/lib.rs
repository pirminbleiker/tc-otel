//! # tc-otel-client
//!
//! Active ADS client for tc-otel. Drives poll + notification subscriptions
//! against PLC targets, and exposes a cached symbol browse for the UI.
//!
//! All I/O goes through the transport-agnostic `tc_otel_ads::dispatcher::AmsDispatcher`,
//! so the same client code works whether the target is reachable over MQTT
//! (via the `AdsOverMqtt` topic scheme) or direct TCP (via port 48898).
//! Callers don't see the transport choice — they configure the dispatcher
//! once, register the PLC NetIDs, and issue reads.
//!
//! Modules:
//! - [`browse`]: decode + populate [`browse::SymbolTree`] from ADS symbol
//!   uploads. No I/O in the parser functions; `upload_via_dispatcher` is the
//!   convenience that issues the three SYM_* reads.
//! - [`cache`]: per-target [`cache::SymbolTreeCache`] with explicit
//!   invalidation.
//! - [`client`]: thin convenience layer over [`tc_otel_ads::dispatcher::AmsDispatcher`]
//!   implementing [`client::SymbolReader`] for the poll path.
//! - [`poll`]: [`poll::Poller`] — periodic ADS Read, emits [`tc_otel_core::models::MetricEntry`].
//! - [`notify`]: [`notify::Notifier`] — AddDeviceNotification subscriptions
//!   and stamp fan-out.

pub mod browse;
pub mod cache;
pub mod client;
pub mod error;
pub mod notify;
pub mod poll;

pub use error::{ClientError, Result};
pub use tc_otel_ads::ams::{
    AmsNetId, ADS_CMD_ADD_NOTIFICATION, ADS_CMD_DEL_NOTIFICATION, ADS_CMD_NOTIFICATION,
    ADS_CMD_READ,
};
