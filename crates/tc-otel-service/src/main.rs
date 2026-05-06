//! tc-otel - OpenTelemetry bridge for Beckhoff TwinCAT PLCs
//!
//! Receives ADS data via AMS/TCP (port 48898) and exports logs, metrics,
//! and traces via OpenTelemetry to any compatible backend.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tc_otel_core::AppSettings;

#[cfg(feature = "client-bridge")]
mod client_bridge;
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

    // Load config first so the tracing filter can honour `logging.log_level`.
    // Errors here predate the subscriber being installed, so they go to
    // stderr via the `?` operator's `Display` impl on the anyhow chain.
    let settings = AppSettings::from_json_file(&args.config).context(format!(
        "Failed to load config from {}",
        args.config.display()
    ))?;

    // Apply the configured level to every tc-otel crate so that switching
    // `log_level` in config actually affects what's emitted. RUST_LOG can
    // still override on a per-target basis if set.
    let cfg_level = settings.logging.log_level.as_str();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"))
        .add_directive(format!("tc_otel={cfg_level}").parse()?)
        .add_directive(format!("tc_otel_ads={cfg_level}").parse()?)
        .add_directive(format!("tc_otel_core={cfg_level}").parse()?)
        .add_directive(format!("tc_otel_service={cfg_level}").parse()?);

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let transport_desc = match &settings.receiver.transport {
        tc_otel_core::config::TransportConfig::Tcp(_) => {
            format!("AMS/TCP :{}", settings.receiver.ams_tcp_port)
        }
        tc_otel_core::config::TransportConfig::Mqtt(m) => {
            format!("MQTT broker={} topic={}", m.broker, m.topic_prefix)
        }
        tc_otel_core::config::TransportConfig::LocalRouter(lr) => {
            format!(
                "local AMS router client → {}:{} (register port {})",
                lr.router_host, lr.router_port, settings.receiver.ads_port
            )
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
