//! Shared helpers for the socket-to-OTLP end-to-end tests.
//!
//! Not every test binary uses every helper, so unused items are allowed.
#![allow(dead_code)]

pub mod plc;
pub mod service;
pub mod sink;
pub mod wire;
