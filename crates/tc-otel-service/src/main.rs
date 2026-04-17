//! tc-otel - OpenTelemetry bridge for Beckhoff TwinCAT PLCs
//!
//! Receives ADS data via AMS/TCP (port 48898) and exports logs, metrics,
//! and traces via OpenTelemetry to any compatible backend.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tc_otel_core::AppSettings;

mod config_watcher;
pub mod cycle_time;
mod diagnostics_bridge;
mod dispatcher;
mod service;
mod span_dispatcher;
pub mod system_metrics;
mod trace_dispatcher;
pub mod web;

use service::TcOtelService;

#[derive(Parser, Debug)]
#[command(name = "tc-otel")]
#[command(about = "tc-otel - OpenTelemetry bridge for TwinCAT PLCs")]
#[command(version)]
struct Args {
    /// Path to configuration file (JSON)
    #[arg(short, long, default_value = "config.json")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tc_otel_service=info".parse()?),
        )
        .with_writer(std::io::stderr)
        .init();

    let settings = AppSettings::from_json_file(&args.config).context(format!(
        "Failed to load config from {}",
        args.config.display()
    ))?;

    let transport_desc = match &settings.receiver.transport {
        tc_otel_core::config::TransportConfig::Tcp(_) => {
            format!("AMS/TCP :{}", settings.receiver.ams_tcp_port)
        }
        tc_otel_core::config::TransportConfig::Mqtt(m) => {
            format!("MQTT broker={} topic={}", m.broker, m.topic_prefix)
        }
    };
    tracing::info!(
        "tc-otel starting: {} (Net ID {}), export → {}",
        transport_desc,
        settings.receiver.ams_net_id,
        settings.export.endpoint,
    );

    let service = TcOtelService::new(settings)
        .await?
        .with_config_watch(args.config);
    service.run().await?;

    Ok(())
}
