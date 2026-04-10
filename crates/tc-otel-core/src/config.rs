//! Configuration structures and loading

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Application-wide configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub logging: LoggingConfig,
    pub receiver: ReceiverConfig,
    #[serde(default)]
    pub export: ExportConfig,
    pub outputs: Vec<OutputConfig>,
    pub service: ServiceConfig,
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub log_level: String,
    pub format: LogFormat,
    pub output_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

/// Receiver configuration (OTEL listener)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverConfig {
    /// HTTP/gRPC listening address
    pub host: String,
    /// HTTP listening port (default 4318)
    pub http_port: u16,
    /// gRPC listening port (default 4317)
    pub grpc_port: u16,
    /// Maximum request body size in bytes
    pub max_body_size: usize,
    /// Request timeout in seconds
    pub request_timeout_secs: u64,
    /// AMS Net ID for the TCP server (e.g., "172.17.0.2.1.1")
    #[serde(default = "default_ams_net_id")]
    pub ams_net_id: String,
    /// AMS/TCP listening port (default 48898)
    #[serde(default = "default_ams_tcp_port")]
    pub ams_tcp_port: u16,
    /// ADS port for AMS/TCP server (default 16150)
    #[serde(default = "default_ads_port")]
    pub ads_port: u16,
}

fn default_ams_net_id() -> String {
    "0.0.0.0.1.1".to_string()
}

fn default_ams_tcp_port() -> u16 {
    48898
}

fn default_ads_port() -> u16 {
    16150
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            http_port: 4318,
            grpc_port: 4317,
            max_body_size: 4 * 1024 * 1024, // 4 MB
            request_timeout_secs: 30,
            ams_net_id: "0.0.0.0.1.1".to_string(),
            ams_tcp_port: 48898,
            ads_port: 16150,
        }
    }
}

/// Output plugin configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(rename = "Type")]
    pub output_type: String,
    #[serde(flatten)]
    pub settings: serde_json::Value,
}

/// Export configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportConfig {
    /// Export endpoint URL (e.g. "http://victoria-logs:9428/insert/jsonline")
    #[serde(default = "default_export_endpoint")]
    pub endpoint: String,
    /// Batch size - flush after this many records
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// Flush interval in milliseconds
    #[serde(default = "default_flush_interval_ms")]
    pub flush_interval_ms: u64,
    /// HTTP timeout in seconds
    #[serde(default = "default_export_timeout_secs")]
    pub timeout_secs: u64,
    /// Max retry attempts on failure
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

fn default_export_endpoint() -> String { "http://victoria-logs:9428/insert/jsonline".to_string() }
fn default_batch_size() -> usize { 2000 }
fn default_flush_interval_ms() -> u64 { 1000 }
fn default_export_timeout_secs() -> u64 { 10 }
fn default_max_retries() -> usize { 3 }

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            endpoint: default_export_endpoint(),
            batch_size: default_batch_size(),
            flush_interval_ms: default_flush_interval_ms(),
            timeout_secs: default_export_timeout_secs(),
            max_retries: default_max_retries(),
        }
    }
}

/// Service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Service name
    pub name: String,
    /// Service display name
    pub display_name: String,
    /// Number of worker threads for log processing
    pub worker_threads: Option<usize>,
    /// Channel capacity for buffering logs
    pub channel_capacity: usize,
    /// Graceful shutdown timeout in seconds
    pub shutdown_timeout_secs: u64,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            name: "tc-otel".to_string(),
            display_name: "tc-otel - TwinCAT OpenTelemetry Service".to_string(),
            worker_threads: None,
            channel_capacity: 50000,
            shutdown_timeout_secs: 30,
        }
    }
}

impl AppSettings {
    /// Load configuration from a JSON file
    pub fn from_json_file(path: &std::path::Path) -> crate::error::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).map_err(|e| {
            crate::error::Error::ConfigError(format!("Failed to parse config: {}", e))
        })
    }

    /// Load configuration from a TOML file
    pub fn from_toml_file(path: &std::path::Path) -> crate::error::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content).map_err(|e| {
            crate::error::Error::ConfigError(format!("Failed to parse config: {}", e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_receiver_config_defaults() {
        let config = ReceiverConfig::default();
        assert_eq!(config.http_port, 4318);
        assert_eq!(config.grpc_port, 4317);
    }

    #[test]
    fn test_service_config_defaults() {
        let config = ServiceConfig::default();
        assert_eq!(config.name, "tc-otel");
        assert_eq!(config.channel_capacity, 50000);
    }

    #[test]
    fn test_logging_config_json_format() {
        let config = LoggingConfig {
            log_level: "debug".to_string(),
            format: LogFormat::Json,
            output_path: None,
        };

        assert_eq!(config.log_level, "debug");
        assert_eq!(config.format, LogFormat::Json);
        assert!(config.output_path.is_none());
    }

    #[test]
    fn test_logging_config_text_format() {
        let config = LoggingConfig {
            log_level: "info".to_string(),
            format: LogFormat::Text,
            output_path: Some(std::path::PathBuf::from("/var/log/tc-otel.log")),
        };

        assert_eq!(config.log_level, "info");
        assert_eq!(config.format, LogFormat::Text);
        assert!(config.output_path.is_some());
        assert_eq!(config.output_path.unwrap().to_string_lossy(), "/var/log/tc-otel.log");
    }

    #[test]
    fn test_receiver_config_custom_values() {
        let config = ReceiverConfig {
            host: "0.0.0.0".to_string(),
            http_port: 8080,
            grpc_port: 9090,
            max_body_size: 8 * 1024 * 1024,
            request_timeout_secs: 60,
            ams_net_id: "192.168.1.100.1.1".to_string(),
            ams_tcp_port: 48898,
            ads_port: 16150,
        };

        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.grpc_port, 9090);
        assert_eq!(config.max_body_size, 8 * 1024 * 1024);
        assert_eq!(config.request_timeout_secs, 60);
    }

    #[test]
    fn test_service_config_custom() {
        let config = ServiceConfig {
            name: "CustomService".to_string(),
            display_name: "Custom Display".to_string(),
            worker_threads: Some(8),
            channel_capacity: 50000,
            shutdown_timeout_secs: 60,
        };

        assert_eq!(config.name, "CustomService");
        assert_eq!(config.display_name, "Custom Display");
        assert_eq!(config.worker_threads, Some(8));
        assert_eq!(config.channel_capacity, 50000);
        assert_eq!(config.shutdown_timeout_secs, 60);
    }

    #[test]
    fn test_output_config() {
        use serde_json::json;

        let config = OutputConfig {
            output_type: "console".to_string(),
            settings: json!({
                "format": "json",
                "level": "info"
            }),
        };

        assert_eq!(config.output_type, "console");
        assert!(config.settings.is_object());
        assert_eq!(config.settings["format"], "json");
        assert_eq!(config.settings["level"], "info");
    }

    #[test]
    fn test_log_format_json_serde() {
        let json_str = r#""json""#;
        let format: LogFormat = serde_json::from_str(json_str).unwrap();
        assert_eq!(format, LogFormat::Json);
    }

    #[test]
    fn test_log_format_text_serde() {
        let json_str = r#""text""#;
        let format: LogFormat = serde_json::from_str(json_str).unwrap();
        assert_eq!(format, LogFormat::Text);
    }

    #[test]
    fn test_receiver_config_serialization() {
        let config = ReceiverConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ReceiverConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.host, deserialized.host);
        assert_eq!(config.http_port, deserialized.http_port);
        assert_eq!(config.grpc_port, deserialized.grpc_port);
        assert_eq!(config.max_body_size, deserialized.max_body_size);
        assert_eq!(config.request_timeout_secs, deserialized.request_timeout_secs);
    }

    #[test]
    fn test_service_config_serialization() {
        let config = ServiceConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ServiceConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.name, deserialized.name);
        assert_eq!(config.display_name, deserialized.display_name);
        assert_eq!(config.channel_capacity, deserialized.channel_capacity);
    }

    #[test]
    fn test_output_config_multiple_outputs() {
        let configs = vec![
            OutputConfig {
                output_type: "console".to_string(),
                settings: serde_json::json!({}),
            },
            OutputConfig {
                output_type: "file".to_string(),
                settings: serde_json::json!({"path": "/var/log/out.log"}),
            },
            OutputConfig {
                output_type: "otel".to_string(),
                settings: serde_json::json!({"endpoint": "http://localhost:4317"}),
            },
        ];

        assert_eq!(configs.len(), 3);
        assert_eq!(configs[0].output_type, "console");
        assert_eq!(configs[1].output_type, "file");
        assert_eq!(configs[2].output_type, "otel");
    }

    #[test]
    fn test_app_settings_structure() {
        let settings = AppSettings {
            logging: LoggingConfig {
                log_level: "info".to_string(),
                format: LogFormat::Json,
                output_path: None,
            },
            receiver: ReceiverConfig::default(),
            export: ExportConfig::default(),
            outputs: vec![],
            service: ServiceConfig::default(),
        };

        assert_eq!(settings.logging.log_level, "info");
        assert!(settings.outputs.is_empty());
    }

    #[test]
    fn test_receiver_config_port_validation() {
        // Valid port range
        let config = ReceiverConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1024,
            grpc_port: 1025,
            max_body_size: 1024,
            request_timeout_secs: 30,
            ams_net_id: "127.0.0.1.1.1".to_string(),
            ams_tcp_port: 48898,
            ads_port: 16150,
        };

        assert!(config.http_port > 0);
    }

    #[test]
    fn test_config_with_large_buffer_size() {
        let config = ReceiverConfig {
            host: "127.0.0.1".to_string(),
            http_port: 4318,
            grpc_port: 4317,
            max_body_size: 100 * 1024 * 1024, // 100 MB
            request_timeout_secs: 30,
            ams_net_id: "127.0.0.1.1.1".to_string(),
            ams_tcp_port: 48898,
            ads_port: 16150,
        };

        assert_eq!(config.max_body_size, 100 * 1024 * 1024);
    }

    #[test]
    fn test_output_config_with_complex_settings() {
        use serde_json::json;

        let settings = json!({
            "host": "localhost",
            "port": 5000,
            "protocol": "http",
            "retry": {
                "max_attempts": 3,
                "backoff_ms": 100
            },
            "headers": {
                "authorization": "Bearer token"
            }
        });

        let config = OutputConfig {
            output_type: "http".to_string(),
            settings,
        };

        assert_eq!(config.output_type, "http");
        assert_eq!(config.settings["host"], "localhost");
        assert_eq!(config.settings["port"], 5000);
        assert_eq!(config.settings["retry"]["max_attempts"], 3);
    }
}
