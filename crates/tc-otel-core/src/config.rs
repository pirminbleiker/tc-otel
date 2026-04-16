//! Configuration structures and loading

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Application-wide configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
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
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
    #[serde(default)]
    pub traces: TracesConfig,
}

/// Logging configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LoggingConfig {
    pub log_level: String,
    pub format: LogFormat,
    pub output_path: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            format: LogFormat::Json,
            output_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

/// TLS/SSL configuration for the receiver
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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

/// TCP transport configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TcpTransportConfig {
    /// Listening address (default "0.0.0.0")
    #[serde(default = "default_tcp_host")]
    pub host: String,
    /// Listening port (default 48898)
    #[serde(default = "default_ams_tcp_port")]
    pub port: u16,
    /// Maximum concurrent connections (default 100)
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

fn default_tcp_host() -> String {
    "0.0.0.0".to_string()
}

impl Default for TcpTransportConfig {
    fn default() -> Self {
        Self {
            host: default_tcp_host(),
            port: default_ams_tcp_port(),
            max_connections: default_max_connections(),
        }
    }
}

/// MQTT TLS configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MqttTlsConfig {
    /// Path to CA certificate (PEM format)
    pub ca_cert_path: PathBuf,
    /// Path to client certificate (PEM format, optional for mTLS)
    #[serde(default)]
    pub client_cert_path: Option<PathBuf>,
    /// Path to client private key (PEM format, optional for mTLS)
    #[serde(default)]
    pub client_key_path: Option<PathBuf>,
    /// Skip server certificate verification (INSECURE — not recommended)
    #[serde(default)]
    pub insecure_skip_verify: bool,
}

/// MQTT transport configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MqttTransportConfig {
    /// MQTT broker host (e.g., "localhost")
    pub broker: String,
    /// Topic prefix for AMS frames (default "AdsOverMqtt")
    #[serde(default = "default_mqtt_prefix")]
    pub topic_prefix: String,
    /// MQTT client ID (default "tc-otel")
    #[serde(default = "default_mqtt_client_id")]
    pub client_id: String,
    /// MQTT username (optional)
    #[serde(default)]
    pub username: Option<String>,
    /// MQTT password (optional)
    #[serde(default)]
    pub password: Option<String>,
    /// TLS configuration (optional)
    #[serde(default)]
    pub tls: Option<MqttTlsConfig>,
}

fn default_mqtt_prefix() -> String {
    "AdsOverMqtt".to_string()
}

fn default_mqtt_client_id() -> String {
    "tc-otel".to_string()
}

impl Default for MqttTransportConfig {
    fn default() -> Self {
        Self {
            broker: "localhost:1883".to_string(),
            topic_prefix: default_mqtt_prefix(),
            client_id: default_mqtt_client_id(),
            username: None,
            password: None,
            tls: None,
        }
    }
}

/// Transport configuration (tag-based enum for pluggable transports)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TransportConfig {
    /// TCP transport (AMS/TCP on port 48898)
    Tcp(TcpTransportConfig),
    /// MQTT transport (ADS-over-MQTT)
    Mqtt(MqttTransportConfig),
}

impl Default for TransportConfig {
    fn default() -> Self {
        TransportConfig::Tcp(TcpTransportConfig::default())
    }
}

impl TransportConfig {
    fn as_mqtt_mut(&mut self) -> Option<&mut MqttTransportConfig> {
        match self {
            TransportConfig::Mqtt(mqtt) => Some(mqtt),
            _ => None,
        }
    }

    fn as_mqtt(&self) -> Option<&MqttTransportConfig> {
        match self {
            TransportConfig::Mqtt(mqtt) => Some(mqtt),
            _ => None,
        }
    }
}

/// Receiver configuration (OTEL listener)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// AMS Net ID for the AMS transport (e.g., "0.0.0.0.1.1")
    #[serde(default = "default_ams_net_id")]
    pub ams_net_id: String,
    /// AMS/TCP listening port (deprecated, use transport.tcp.port instead)
    /// This is kept for backward compatibility
    #[serde(default = "default_ams_tcp_port")]
    pub ams_tcp_port: u16,
    /// ADS port for AMS transport (default 16150)
    #[serde(default = "default_ads_port")]
    pub ads_port: u16,
    /// Maximum concurrent connections (deprecated, use transport.tcp.max_connections instead)
    /// This is kept for backward compatibility
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
    /// Transport configuration (TCP, MQTT, etc.)
    #[serde(default)]
    pub transport: TransportConfig,
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
            transport: TransportConfig::default(),
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutputConfig {
    #[serde(rename = "Type")]
    pub output_type: String,
    #[serde(flatten)]
    pub settings: serde_json::Value,
}

/// Export configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: default_web_enabled(),
            host: default_web_host(),
            port: default_web_port(),
        }
    }
}

/// Metric kind for configuration (string representation)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MetricKindConfig {
    #[default]
    Gauge,
    Sum,
    Histogram,
}

impl MetricKindConfig {
    /// Convert to the core MetricKind used in data models
    pub fn to_metric_kind(self) -> crate::models::MetricKind {
        match self {
            MetricKindConfig::Gauge => crate::models::MetricKind::Gauge,
            MetricKindConfig::Sum => crate::models::MetricKind::Sum,
            MetricKindConfig::Histogram => crate::models::MetricKind::Histogram,
        }
    }
}

/// Source of custom metric values: push (mapping only), poll (ADS reads), or notification (ADS subscriptions)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum CustomMetricSource {
    /// PLC pushes the metric via push-diagnostics (mapping only; default)
    #[default]
    Push,
    /// tc-otel issues periodic ADS reads by symbol name
    Poll,
    /// tc-otel opens an AddDeviceNotification subscription; PLC pushes on-change
    Notification,
}

/// Poll configuration for custom metrics (poll_interval_ms)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PollConfig {
    /// Polling interval in milliseconds (default: 1000)
    #[serde(default = "default_poll_interval_ms")]
    pub interval_ms: u64,
}

fn default_poll_interval_ms() -> u64 {
    1000
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_poll_interval_ms(),
        }
    }
}

/// Notification configuration for custom metrics (ADS subscriptions)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NotificationConfig {
    /// Minimum period in milliseconds between notifications (default: 0)
    #[serde(default)]
    pub min_period_ms: u32,
    /// Maximum period in milliseconds between notifications (default: 10000)
    #[serde(default = "default_max_period_ms")]
    pub max_period_ms: u32,
    /// Maximum delay in milliseconds for subscription (default: 5000)
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u32,
    /// Transmission mode: "on_change" (default) or "cyclic"
    #[serde(default)]
    pub transmission_mode: NotificationTransmissionMode,
}

fn default_max_period_ms() -> u32 {
    10000
}

fn default_max_delay_ms() -> u32 {
    5000
}

/// Transmission mode for ADS notifications
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotificationTransmissionMode {
    /// Send notification on value change only
    #[default]
    OnChange,
    /// Send notification periodically
    Cyclic,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            min_period_ms: 0,
            max_period_ms: default_max_period_ms(),
            max_delay_ms: default_max_delay_ms(),
            transmission_mode: NotificationTransmissionMode::OnChange,
        }
    }
}

/// Maps a PLC symbol to an OTEL metric definition
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CustomMetricDef {
    /// PLC symbol path (e.g., "GVL.motor.temperature")
    pub symbol: String,
    /// OTEL metric name (e.g., "plc.motor.temperature")
    pub metric_name: String,
    /// Metric description
    #[serde(default)]
    pub description: String,
    /// Metric unit (e.g., "Cel", "mm/s", "rpm")
    #[serde(default)]
    pub unit: String,
    /// Metric kind: "gauge", "sum", or "histogram"
    #[serde(default)]
    pub kind: MetricKindConfig,
    /// For Sum kind: whether monotonic (counter vs up-down counter)
    #[serde(default)]
    pub is_monotonic: bool,
    /// Source of metric values: push (default), poll, or notification
    #[serde(default)]
    pub source: CustomMetricSource,
    /// AMS Net ID of the target PLC (required for non-push sources)
    #[serde(default)]
    pub ams_net_id: Option<String>,
    /// AMS port of the target PLC (required for non-push sources; default: 851)
    #[serde(default)]
    pub ams_port: Option<u16>,
    /// Poll configuration (required if source == "poll")
    #[serde(default)]
    pub poll: Option<PollConfig>,
    /// Notification configuration (required if source == "notification")
    #[serde(default)]
    pub notification: Option<NotificationConfig>,
}

impl Default for CustomMetricDef {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            metric_name: String::new(),
            description: String::new(),
            unit: String::new(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            source: CustomMetricSource::Push,
            ams_net_id: None,
            ams_port: None,
            poll: None,
            notification: None,
        }
    }
}

/// Metrics configuration (cycle time tracking, custom metric definitions)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MetricsConfig {
    /// Enable task cycle time tracking (default: true)
    #[serde(default = "default_cycle_time_enabled")]
    pub cycle_time_enabled: bool,
    /// Rolling window size for cycle time statistics (default: 1000)
    #[serde(default = "default_cycle_time_window")]
    pub cycle_time_window: usize,
    /// Custom metric definitions mapping PLC symbols to OTEL metric names
    #[serde(default)]
    pub custom_metrics: Vec<CustomMetricDef>,
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
            custom_metrics: Vec::new(),
            export_enabled: false,
            export_endpoint: None,
            export_batch_size: default_metrics_batch_size(),
            export_flush_interval_ms: default_metrics_flush_interval_ms(),
        }
    }
}

/// Distributed trace export configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TracesExportConfig {
    /// OTLP traces export endpoint (e.g., "http://localhost:4318/v1/traces")
    /// If unset, traces are not exported
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Batch size for trace export (default: 100)
    #[serde(default = "default_traces_batch_size")]
    pub batch_size: usize,
    /// Flush interval for trace export in milliseconds (default: 1000)
    #[serde(default = "default_traces_flush_interval_ms")]
    pub flush_interval_ms: u64,
}

fn default_traces_batch_size() -> usize {
    100
}

fn default_traces_flush_interval_ms() -> u64 {
    1000
}

impl Default for TracesExportConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            batch_size: default_traces_batch_size(),
            flush_interval_ms: default_traces_flush_interval_ms(),
        }
    }
}

/// Distributed tracing configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TracesConfig {
    /// Enable distributed tracing (default: false)
    pub enabled: bool,
    /// Time-to-live for pending spans in seconds (default: 10)
    #[serde(default = "default_span_ttl_secs")]
    pub span_ttl_secs: u64,
    /// Maximum number of pending spans across all PLCs (default: 1024)
    #[serde(default = "default_max_pending_spans")]
    pub max_pending_spans: usize,
    /// Trace export configuration
    #[serde(default)]
    pub export: TracesExportConfig,
}

fn default_span_ttl_secs() -> u64 {
    10
}

fn default_max_pending_spans() -> usize {
    1024
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            span_ttl_secs: default_span_ttl_secs(),
            max_pending_spans: default_max_pending_spans(),
            export: TracesExportConfig::default(),
        }
    }
}

/// Service configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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

/// Self-polling diagnostics collector.
///
/// Disabled by default. When enabled, tc-otel runs its own ADS polling loop
/// against each configured PLC target so runtime metrics (task cycle stats,
/// RT usage + latency, cycle-exceed counter) are captured even when no
/// engineering IDE is connected.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsConfig {
    /// Enable the self-polling diagnostics collector.
    #[serde(default)]
    pub enabled: bool,
    /// Targets to poll.
    #[serde(default)]
    pub targets: Vec<DiagnosticsTargetConfig>,
}

/// One PLC to poll for diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsTargetConfig {
    /// AMS Net ID of the PLC, e.g. `172.28.41.37.1.1`.
    pub ams_net_id: String,
    /// Poll period in milliseconds (default: 1000).
    #[serde(default = "default_diagnostics_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// Poll the system cycle-exceed counter.
    #[serde(default = "default_true")]
    pub exceed_counter: bool,
    /// Poll the realtime-usage + system-latency block.
    #[serde(default = "default_true")]
    pub rt_usage: bool,
    /// AMS task ports to poll for per-task cycle stats.
    /// Default covers the I/O idle task plus the first two PLC tasks —
    /// 340, 350, 351.
    #[serde(default = "default_diagnostics_task_ports")]
    pub task_ports: Vec<u16>,
    /// AMS port of the realtime subsystem (default 200 / R0_REALTIME).
    #[serde(default = "default_diagnostics_rt_port")]
    pub rt_port: u16,
    /// Optional manual task-name override, keyed by AMS port number
    /// (as string). TwinCAT 3 runtimes return `SRVNOTSUPP` for
    /// `AdsReadDeviceInfo` on task ports, so reliable names usually need
    /// to come from config. Names from this map take precedence over
    /// anything the auto-discovery turns up; unlisted ports fall back to
    /// `port-<N>`.
    #[serde(default)]
    pub task_names: std::collections::HashMap<String, String>,
}

fn default_diagnostics_poll_interval_ms() -> u64 {
    1000
}
fn default_true() -> bool {
    true
}
fn default_diagnostics_task_ports() -> Vec<u16> {
    vec![340, 350, 351]
}
fn default_diagnostics_rt_port() -> u16 {
    200
}

impl AppSettings {
    pub const MASKED_SENTINEL: &'static str = "***MASKED***";

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

    /// Return config as JSON with secrets masked for safe display in UI.
    pub fn to_masked_json(&self) -> serde_json::Value {
        let mut val = serde_json::to_value(self).unwrap_or(serde_json::json!({}));

        if let Some(receiver) = val.get_mut("receiver") {
            if let Some(transport) = receiver.get_mut("transport") {
                if let Some(type_str) = transport.get("type") {
                    if type_str.as_str() == Some("mqtt") {
                        if let Some(password) = transport.get_mut("password") {
                            if password.is_string() && !password.as_str().unwrap_or("").is_empty() {
                                *password = serde_json::json!(Self::MASKED_SENTINEL);
                            }
                        }
                        if let Some(username) = transport.get_mut("username") {
                            if username.is_string() && !username.as_str().unwrap_or("").is_empty() {
                                *username = serde_json::json!(Self::MASKED_SENTINEL);
                            }
                        }
                    }
                }
            }
        }

        val
    }

    /// Replace masked sentinel values in `self` with the real values from `current`.
    /// Sentinel: the string "***MASKED***".
    /// Empty string = explicit clear, keep as-is.
    pub fn merge_secrets_from(&mut self, current: &AppSettings) {
        if let Some(mqtt) = self.receiver.transport.as_mqtt_mut() {
            if let Some(password_str) = &mqtt.password {
                if password_str == Self::MASKED_SENTINEL {
                    if let Some(current_mqtt) = current.receiver.transport.as_mqtt() {
                        mqtt.password = current_mqtt.password.clone();
                    }
                }
            }
            if let Some(username_str) = &mqtt.username {
                if username_str == Self::MASKED_SENTINEL {
                    if let Some(current_mqtt) = current.receiver.transport.as_mqtt() {
                        mqtt.username = current_mqtt.username.clone();
                    }
                }
            }
        }
    }

    /// Aggregate validation across all sections. Returns list of human-readable errors.
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if let Err(e) = self.receiver.validate() {
            errors.extend(e);
        }

        let valid_ports = 1..=65535u16;

        if !valid_ports.contains(&self.receiver.http_port) {
            errors.push(format!(
                "receiver.http_port {} out of range [1, 65535]",
                self.receiver.http_port
            ));
        }
        if !valid_ports.contains(&self.receiver.grpc_port) {
            errors.push(format!(
                "receiver.grpc_port {} out of range [1, 65535]",
                self.receiver.grpc_port
            ));
        }
        if !valid_ports.contains(&self.receiver.ams_tcp_port) {
            errors.push(format!(
                "receiver.ams_tcp_port {} out of range [1, 65535]",
                self.receiver.ams_tcp_port
            ));
        }
        if !valid_ports.contains(&self.receiver.ads_port) {
            errors.push(format!(
                "receiver.ads_port {} out of range [1, 65535]",
                self.receiver.ads_port
            ));
        }

        match &self.receiver.transport {
            TransportConfig::Tcp(tcp) => {
                if !valid_ports.contains(&tcp.port) {
                    errors.push(format!(
                        "receiver.transport.tcp.port {} out of range [1, 65535]",
                        tcp.port
                    ));
                }
            }
            TransportConfig::Mqtt(_mqtt) => {}
        }

        if !valid_ports.contains(&self.web.port) {
            errors.push(format!(
                "web.port {} out of range [1, 65535]",
                self.web.port
            ));
        }

        if self.export.batch_size == 0 {
            errors.push("export.batch_size must be > 0".to_string());
        }
        if self.export.flush_interval_ms == 0 {
            errors.push("export.flush_interval_ms must be > 0".to_string());
        }
        if self.export.timeout_secs == 0 {
            errors.push("export.timeout_secs must be > 0".to_string());
        }

        if self.service.channel_capacity == 0 {
            errors.push("service.channel_capacity must be > 0".to_string());
        }

        if let Some(worker_threads) = self.service.worker_threads {
            if worker_threads == 0 {
                errors.push("service.worker_threads must be > 0 if set".to_string());
            }
        }

        if self.metrics.cycle_time_window == 0 {
            errors.push("metrics.cycle_time_window must be > 0".to_string());
        }
        if self.metrics.export_batch_size == 0 {
            errors.push("metrics.export_batch_size must be > 0".to_string());
        }
        if self.metrics.export_flush_interval_ms == 0 {
            errors.push("metrics.export_flush_interval_ms must be > 0".to_string());
        }

        if self.traces.enabled {
            if self.traces.span_ttl_secs == 0 {
                errors.push("traces.span_ttl_secs must be > 0".to_string());
            }
            if self.traces.max_pending_spans == 0 {
                errors.push("traces.max_pending_spans must be > 0".to_string());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
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
            diagnostics: DiagnosticsConfig::default(),
            traces: TracesConfig::default(),
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

    #[test]
    fn test_to_masked_json_masks_mqtt_password() {
        let mut settings = AppSettings::default();
        settings.receiver.transport = TransportConfig::Mqtt(MqttTransportConfig {
            broker: "localhost".to_string(),
            password: Some("secret-password".to_string()),
            username: Some("user".to_string()),
            ..MqttTransportConfig::default()
        });

        let masked = settings.to_masked_json();
        let transport = &masked["receiver"]["transport"];

        assert_eq!(
            transport["password"].as_str().unwrap(),
            AppSettings::MASKED_SENTINEL
        );
        assert_eq!(
            transport["username"].as_str().unwrap(),
            AppSettings::MASKED_SENTINEL
        );
    }

    #[test]
    fn test_to_masked_json_preserves_unset_fields() {
        let mut settings = AppSettings::default();
        settings.receiver.transport = TransportConfig::Mqtt(MqttTransportConfig {
            broker: "localhost".to_string(),
            password: None,
            username: None,
            ..MqttTransportConfig::default()
        });

        let masked = settings.to_masked_json();
        let transport = &masked["receiver"]["transport"];

        assert!(transport["password"].is_null());
        assert!(transport["username"].is_null());
    }

    #[test]
    fn test_merge_secrets_restores_masked_password() {
        let mut new_settings = AppSettings::default();
        new_settings.receiver.transport = TransportConfig::Mqtt(MqttTransportConfig {
            broker: "localhost".to_string(),
            password: Some(AppSettings::MASKED_SENTINEL.to_string()),
            username: Some(AppSettings::MASKED_SENTINEL.to_string()),
            ..MqttTransportConfig::default()
        });

        let current_settings = AppSettings {
            receiver: ReceiverConfig {
                transport: TransportConfig::Mqtt(MqttTransportConfig {
                    broker: "localhost".to_string(),
                    password: Some("real-password".to_string()),
                    username: Some("real-user".to_string()),
                    ..MqttTransportConfig::default()
                }),
                ..ReceiverConfig::default()
            },
            ..AppSettings::default()
        };

        new_settings.merge_secrets_from(&current_settings);

        if let TransportConfig::Mqtt(mqtt) = &new_settings.receiver.transport {
            assert_eq!(mqtt.password, Some("real-password".to_string()));
            assert_eq!(mqtt.username, Some("real-user".to_string()));
        } else {
            panic!("Expected Mqtt transport");
        }
    }

    #[test]
    fn test_merge_secrets_preserves_explicit_new_value() {
        let mut new_settings = AppSettings::default();
        new_settings.receiver.transport = TransportConfig::Mqtt(MqttTransportConfig {
            broker: "localhost".to_string(),
            password: Some("new-password".to_string()),
            username: Some("new-user".to_string()),
            ..MqttTransportConfig::default()
        });

        let current_settings = AppSettings {
            receiver: ReceiverConfig {
                transport: TransportConfig::Mqtt(MqttTransportConfig {
                    broker: "localhost".to_string(),
                    password: Some("old-password".to_string()),
                    username: Some("old-user".to_string()),
                    ..MqttTransportConfig::default()
                }),
                ..ReceiverConfig::default()
            },
            ..AppSettings::default()
        };

        new_settings.merge_secrets_from(&current_settings);

        if let TransportConfig::Mqtt(mqtt) = &new_settings.receiver.transport {
            assert_eq!(mqtt.password, Some("new-password".to_string()));
            assert_eq!(mqtt.username, Some("new-user".to_string()));
        } else {
            panic!("Expected Mqtt transport");
        }
    }

    #[test]
    fn test_validate_catches_port_zero() {
        let mut settings = AppSettings::default();
        settings.receiver.http_port = 0;

        let result = settings.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e: &String| e.contains("http_port")));
    }

    #[test]
    fn test_validate_catches_empty_batch_size() {
        let mut settings = AppSettings::default();
        settings.export.batch_size = 0;

        let result = settings.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e: &String| e.contains("batch_size")));
    }

    #[test]
    fn test_validate_catches_empty_channel_capacity() {
        let mut settings = AppSettings::default();
        settings.service.channel_capacity = 0;

        let result = settings.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors
            .iter()
            .any(|e: &String| e.contains("channel_capacity")));
    }

    #[test]
    fn test_validate_passes_valid_config() {
        let settings = AppSettings::default();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn test_traces_config_defaults() {
        let config = TracesConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.span_ttl_secs, 10);
        assert_eq!(config.max_pending_spans, 1024);
        assert!(config.export.endpoint.is_none());
        assert_eq!(config.export.batch_size, 100);
        assert_eq!(config.export.flush_interval_ms, 1000);
    }

    #[test]
    fn test_traces_config_enabled() {
        let config = TracesConfig {
            enabled: true,
            span_ttl_secs: 5,
            max_pending_spans: 2048,
            export: TracesExportConfig {
                endpoint: Some("http://otel-collector:4318/v1/traces".to_string()),
                batch_size: 50,
                flush_interval_ms: 500,
            },
        };

        assert!(config.enabled);
        assert_eq!(config.span_ttl_secs, 5);
        assert_eq!(config.max_pending_spans, 2048);
        assert_eq!(
            config.export.endpoint,
            Some("http://otel-collector:4318/v1/traces".to_string())
        );
        assert_eq!(config.export.batch_size, 50);
        assert_eq!(config.export.flush_interval_ms, 500);
    }

    #[test]
    fn test_traces_config_toml_deserialization() {
        let toml_str = r#"
enabled = true
span_ttl_secs = 15
max_pending_spans = 512

[export]
endpoint = "http://collector:4318/v1/traces"
batch_size = 200
flush_interval_ms = 2000
"#;
        let config: TracesConfig = toml::from_str(toml_str).expect("Failed to parse TOML");
        assert!(config.enabled);
        assert_eq!(config.span_ttl_secs, 15);
        assert_eq!(config.max_pending_spans, 512);
        assert_eq!(
            config.export.endpoint,
            Some("http://collector:4318/v1/traces".to_string())
        );
        assert_eq!(config.export.batch_size, 200);
        assert_eq!(config.export.flush_interval_ms, 2000);
    }

    #[test]
    fn test_traces_config_toml_empty_block_uses_defaults() {
        let toml_str = r#"
[traces]
"#;
        let config: TracesConfig = toml::from_str(toml_str).expect("Failed to parse TOML");
        assert!(!config.enabled);
        assert_eq!(config.span_ttl_secs, 10);
        assert_eq!(config.max_pending_spans, 1024);
        assert!(config.export.endpoint.is_none());
    }

    #[test]
    fn test_validate_traces_enabled_with_zero_ttl() {
        let mut settings = AppSettings::default();
        settings.traces.enabled = true;
        settings.traces.span_ttl_secs = 0;

        let result = settings.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.contains("span_ttl_secs")));
    }

    #[test]
    fn test_validate_traces_enabled_with_zero_max_pending() {
        let mut settings = AppSettings::default();
        settings.traces.enabled = true;
        settings.traces.max_pending_spans = 0;

        let result = settings.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.contains("max_pending_spans")));
    }

    #[test]
    fn test_validate_traces_disabled_ignores_zero_values() {
        let mut settings = AppSettings::default();
        settings.traces.enabled = false;
        settings.traces.span_ttl_secs = 0;
        settings.traces.max_pending_spans = 0;

        assert!(settings.validate().is_ok());
    }
}
