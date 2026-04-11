//! Configuration structures and loading

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Application-wide configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    pub logging: LoggingConfig,
    pub receiver: ReceiverConfig,
    #[serde(default)]
    pub export: ExportConfig,
    pub outputs: Vec<OutputConfig>,
    pub service: ServiceConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
}

/// Logging configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// TLS/SSL configuration for the receiver
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS for the receiver
    #[serde(default)]
    pub enabled: bool,
    /// Path to server certificate (PEM)
    #[serde(default)]
    pub cert_path: Option<PathBuf>,
    /// Path to server private key (PEM)
    #[serde(default)]
    pub key_path: Option<PathBuf>,
    /// Path to CA certificate bundle for verifying client certs
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,
    /// Require client certificate (mTLS)
    #[serde(default)]
    pub require_client_cert: bool,
    /// Path to client certificate (for outbound mTLS)
    #[serde(default)]
    pub client_cert_path: Option<PathBuf>,
    /// Path to client private key (for outbound mTLS)
    #[serde(default)]
    pub client_key_path: Option<PathBuf>,
    /// Skip server certificate verification (INSECURE — rejected in validation)
    #[serde(default)]
    pub insecure_skip_verify: bool,
    /// Minimum TLS version (default: "TLSv1_2")
    #[serde(default = "default_min_tls_version")]
    pub min_version: String,
    /// Maximum TLS version (default: "TLSv1_3")
    #[serde(default = "default_max_tls_version")]
    pub max_version: String,
    /// Allowed cipher suites (empty = use defaults)
    #[serde(default)]
    pub ciphers: Vec<String>,
}

fn default_min_tls_version() -> String {
    "TLSv1_2".to_string()
}

fn default_max_tls_version() -> String {
    "TLSv1_3".to_string()
}

/// Known weak cipher patterns that must be rejected
const WEAK_CIPHER_PATTERNS: &[&str] = &[
    "DES", "RC4", "RC2", "MD5", "NULL", "EXPORT", "anon", "ADH", "AECDH",
];

/// Valid TLS versions in order
const VALID_TLS_VERSIONS: &[&str] = &["TLSv1_0", "TLSv1_1", "TLSv1_2", "TLSv1_3"];

/// Minimum acceptable TLS version (TLSv1_2)
const MIN_ACCEPTABLE_TLS_VERSION: &str = "TLSv1_2";

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cert_path: None,
            key_path: None,
            ca_cert_path: None,
            require_client_cert: false,
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
            min_version: default_min_tls_version(),
            max_version: default_max_tls_version(),
            ciphers: Vec::new(),
        }
    }
}

impl TlsConfig {
    /// Validate the TLS configuration. Returns a list of errors if invalid.
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if !self.enabled {
            return Ok(());
        }

        // insecure_skip_verify is never allowed
        if self.insecure_skip_verify {
            errors.push(
                "insecure_skip_verify=true is not allowed: \
                 certificate verification must not be disabled"
                    .to_string(),
            );
        }

        // Server cert and key are required when TLS is enabled
        if self.cert_path.is_none() {
            errors.push("TLS enabled but cert_path is not set".to_string());
        }
        if self.key_path.is_none() {
            errors.push("TLS enabled but key_path is not set".to_string());
        }

        // Client cert requirement needs client cert paths
        if self.require_client_cert && self.ca_cert_path.is_none() {
            errors.push(
                "require_client_cert=true but ca_cert_path is not set: \
                 CA certificate is needed to verify client certificates"
                    .to_string(),
            );
        }

        // Validate TLS versions
        if let Err(e) = Self::validate_tls_version(&self.min_version) {
            errors.push(format!("min_version: {}", e));
        }
        if let Err(e) = Self::validate_tls_version(&self.max_version) {
            errors.push(format!("max_version: {}", e));
        }

        // Ensure min_version meets minimum acceptable threshold
        if let Err(e) = Self::check_minimum_acceptable_version(&self.min_version) {
            errors.push(e);
        }

        // Ensure min <= max
        if Self::tls_version_ord(&self.min_version) > Self::tls_version_ord(&self.max_version) {
            errors.push(format!(
                "min_version ({}) is greater than max_version ({})",
                self.min_version, self.max_version
            ));
        }

        // Validate cipher suites
        for cipher in &self.ciphers {
            if let Err(e) = Self::validate_cipher_suite(cipher) {
                errors.push(e);
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Validate a TLS version string
    pub fn validate_tls_version(version: &str) -> std::result::Result<(), String> {
        if VALID_TLS_VERSIONS.contains(&version) {
            Ok(())
        } else {
            Err(format!(
                "invalid TLS version '{}': must be one of {:?}",
                version, VALID_TLS_VERSIONS
            ))
        }
    }

    /// Check that a TLS version meets the minimum acceptable threshold (TLSv1_2)
    pub fn check_minimum_acceptable_version(version: &str) -> std::result::Result<(), String> {
        let ord = Self::tls_version_ord(version);
        let min_ord = Self::tls_version_ord(MIN_ACCEPTABLE_TLS_VERSION);
        if ord < min_ord {
            Err(format!(
                "TLS version '{}' is below minimum acceptable version ({})",
                version, MIN_ACCEPTABLE_TLS_VERSION
            ))
        } else {
            Ok(())
        }
    }

    /// Validate a cipher suite name — rejects known weak ciphers
    pub fn validate_cipher_suite(cipher: &str) -> std::result::Result<(), String> {
        let upper = cipher.to_uppercase();
        for weak in WEAK_CIPHER_PATTERNS {
            if upper.contains(weak) {
                return Err(format!(
                    "weak cipher suite '{}': contains banned pattern '{}'",
                    cipher, weak
                ));
            }
        }
        Ok(())
    }

    /// Return numeric ordering for TLS version strings
    fn tls_version_ord(version: &str) -> u8 {
        match version {
            "TLSv1_0" => 0,
            "TLSv1_1" => 1,
            "TLSv1_2" => 2,
            "TLSv1_3" => 3,
            _ => 0,
        }
    }
}

/// Receiver configuration (OTEL listener)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Maximum concurrent connections (default 100)
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Idle connection timeout in seconds (default 300)
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    /// Maximum connections per IP address (default 10)
    #[serde(default = "default_max_connections_per_ip")]
    pub max_connections_per_ip: usize,
    /// Max new connections per second from a single IP (default 10)
    #[serde(default = "default_rate_limit_per_sec_per_ip")]
    pub rate_limit_per_sec_per_ip: usize,
    /// Keep-alive heartbeat interval in seconds (default 60)
    #[serde(default = "default_keepalive_interval_secs")]
    pub keepalive_interval_secs: u64,
    /// Send buffer size limit per connection in bytes (default 1MB)
    #[serde(default = "default_send_buffer_size")]
    pub send_buffer_size: usize,
    /// Enforce HTTPS-only for endpoints
    #[serde(default)]
    pub https_only: bool,
    /// TLS configuration
    #[serde(default)]
    pub tls: TlsConfig,
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

fn default_max_connections() -> usize {
    100
}

fn default_idle_timeout_secs() -> u64 {
    300
}

fn default_max_connections_per_ip() -> usize {
    10
}

fn default_rate_limit_per_sec_per_ip() -> usize {
    10
}

fn default_keepalive_interval_secs() -> u64 {
    60
}

fn default_send_buffer_size() -> usize {
    1_048_576 // 1 MB
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
            max_connections: default_max_connections(),
            idle_timeout_secs: default_idle_timeout_secs(),
            max_connections_per_ip: default_max_connections_per_ip(),
            rate_limit_per_sec_per_ip: default_rate_limit_per_sec_per_ip(),
            keepalive_interval_secs: default_keepalive_interval_secs(),
            send_buffer_size: default_send_buffer_size(),
            https_only: false,
            tls: TlsConfig::default(),
        }
    }
}

impl ReceiverConfig {
    /// Validate an endpoint URL against security policy.
    /// When `https_only` is true, only HTTPS endpoints are accepted.
    /// When TLS is enabled, HTTP endpoints are rejected (downgrade prevention).
    pub fn validate_endpoint(&self, endpoint: &str) -> std::result::Result<(), String> {
        let lower = endpoint.to_lowercase();

        if self.https_only && lower.starts_with("http://") {
            return Err(format!(
                "endpoint '{}' uses HTTP but https_only is enabled: \
                 only HTTPS endpoints are allowed",
                endpoint
            ));
        }

        if self.tls.enabled && lower.starts_with("http://") {
            return Err(format!(
                "endpoint '{}' uses HTTP but TLS is enabled: \
                 cannot downgrade from HTTPS to HTTP",
                endpoint
            ));
        }

        Ok(())
    }

    /// Validate the entire receiver configuration
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors = Vec::new();

        // Validate TLS config
        if let Err(tls_errors) = self.tls.validate() {
            errors.extend(tls_errors);
        }

        // When https_only is set, TLS should be enabled
        if self.https_only && !self.tls.enabled {
            errors.push(
                "https_only=true but TLS is not enabled: \
                 enable TLS to enforce HTTPS-only mode"
                    .to_string(),
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Output plugin configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(rename = "Type")]
    pub output_type: String,
    #[serde(flatten)]
    pub settings: serde_json::Value,
}

/// Export configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

fn default_export_endpoint() -> String {
    "http://victoria-logs:9428/insert/jsonline".to_string()
}
fn default_batch_size() -> usize {
    2000
}
fn default_flush_interval_ms() -> u64 {
    1000
}
fn default_export_timeout_secs() -> u64 {
    10
}
fn default_max_retries() -> usize {
    3
}

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

/// Web UI configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebConfig {
    /// Enable the web UI (default: true)
    #[serde(default = "default_web_enabled")]
    pub enabled: bool,
    /// Web UI listening address (default: "127.0.0.1")
    #[serde(default = "default_web_host")]
    pub host: String,
    /// Web UI listening port (default: 8080)
    #[serde(default = "default_web_port")]
    pub port: u16,
    /// Maximum number of tag subscriptions (default: 500)
    #[serde(default = "default_max_subscriptions")]
    pub max_subscriptions: usize,
}

fn default_web_enabled() -> bool {
    true
}
fn default_web_host() -> String {
    "127.0.0.1".to_string()
}
fn default_web_port() -> u16 {
    8080
}
fn default_max_subscriptions() -> usize {
    500
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: default_web_enabled(),
            host: default_web_host(),
            port: default_web_port(),
            max_subscriptions: default_max_subscriptions(),
        }
    }
}

/// Metrics configuration (cycle time tracking, etc.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Enable task cycle time tracking (default: true)
    #[serde(default = "default_cycle_time_enabled")]
    pub cycle_time_enabled: bool,
    /// Rolling window size for cycle time statistics (default: 1000)
    #[serde(default = "default_cycle_time_window")]
    pub cycle_time_window: usize,
    /// Enable metrics export to OTLP collector (default: false)
    #[serde(default)]
    pub export_enabled: bool,
    /// OTLP metrics export endpoint (e.g., "http://localhost:4318/v1/metrics")
    /// If unset, derived from the main export endpoint by replacing /v1/logs with /v1/metrics
    #[serde(default)]
    pub export_endpoint: Option<String>,
    /// Batch size for metrics export (default: 1000)
    #[serde(default = "default_metrics_batch_size")]
    pub export_batch_size: usize,
    /// Flush interval for metrics export in milliseconds (default: 5000)
    #[serde(default = "default_metrics_flush_interval_ms")]
    pub export_flush_interval_ms: u64,
}

fn default_cycle_time_enabled() -> bool {
    true
}
fn default_cycle_time_window() -> usize {
    1000
}
fn default_metrics_batch_size() -> usize {
    1000
}
fn default_metrics_flush_interval_ms() -> u64 {
    5000
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            cycle_time_enabled: default_cycle_time_enabled(),
            cycle_time_window: default_cycle_time_window(),
            export_enabled: false,
            export_endpoint: None,
            export_batch_size: default_metrics_batch_size(),
            export_flush_interval_ms: default_metrics_flush_interval_ms(),
        }
    }
}

/// Service configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        serde_json::from_str(&content)
            .map_err(|e| crate::error::Error::ConfigError(format!("Failed to parse config: {}", e)))
    }

    /// Load configuration from a TOML file
    pub fn from_toml_file(path: &std::path::Path) -> crate::error::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content)
            .map_err(|e| crate::error::Error::ConfigError(format!("Failed to parse config: {}", e)))
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
        assert_eq!(
            config.output_path.unwrap().to_string_lossy(),
            "/var/log/tc-otel.log"
        );
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
            ..Default::default()
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
        assert_eq!(
            config.request_timeout_secs,
            deserialized.request_timeout_secs
        );
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
        let configs = [
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
            web: WebConfig::default(),
            metrics: MetricsConfig::default(),
        };

        assert_eq!(settings.logging.log_level, "info");
        assert!(settings.outputs.is_empty());
    }

    #[test]
    fn test_metrics_config_defaults() {
        let config = MetricsConfig::default();
        assert!(config.cycle_time_enabled);
        assert_eq!(config.cycle_time_window, 1000);
    }

    #[test]
    fn test_metrics_config_serde_defaults() {
        let json = "{}";
        let config: MetricsConfig = serde_json::from_str(json).unwrap();
        assert!(config.cycle_time_enabled);
        assert_eq!(config.cycle_time_window, 1000);
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
            ..Default::default()
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
            ..Default::default()
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
