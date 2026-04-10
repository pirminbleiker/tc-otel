//! Integration tests for configuration parsing and handling

use tc_otel_core::{AppSettings, ExportConfig, LoggingConfig, LogFormat, ReceiverConfig, ServiceConfig, OutputConfig};
use serde_json::json;
use std::path::PathBuf;

#[test]
fn test_config_complete_structure() {
    let config = AppSettings {
        logging: LoggingConfig {
            log_level: "info".to_string(),
            format: LogFormat::Json,
            output_path: Some(PathBuf::from("/var/log/tc-otel.log")),
        },
        receiver: ReceiverConfig {
            host: "0.0.0.0".to_string(),
            http_port: 4318,
            grpc_port: 4317,
            max_body_size: 4 * 1024 * 1024,
            request_timeout_secs: 30,
            ..Default::default()
        },
        export: ExportConfig::default(),
        outputs: vec![],
        service: ServiceConfig {
            name: "Log4TcService".to_string(),
            display_name: "Log4TC Logging Service".to_string(),
            worker_threads: None,
            channel_capacity: 10000,
            shutdown_timeout_secs: 30,
        },
    };

    assert_eq!(config.logging.log_level, "info");
    assert_eq!(config.receiver.http_port, 4318);
    assert_eq!(config.service.channel_capacity, 10000);
}

#[test]
fn test_config_with_multiple_outputs() {
    let outputs = vec![
        OutputConfig {
            output_type: "console".to_string(),
            settings: json!({"format": "json"}),
        },
        OutputConfig {
            output_type: "file".to_string(),
            settings: json!({
                "path": "/var/log/tc-otel.log",
                "max_size": 10485760,
                "max_backups": 5
            }),
        },
        OutputConfig {
            output_type: "otel".to_string(),
            settings: json!({
                "endpoint": "http://otel-collector:4318",
                "batch_size": 100,
                "timeout_secs": 30
            }),
        },
    ];

    let config = AppSettings {
        logging: LoggingConfig::default(),
        receiver: ReceiverConfig::default(),
        outputs,
        service: ServiceConfig::default(),
    };

    assert_eq!(config.outputs.len(), 3);
    assert_eq!(config.outputs[0].output_type, "console");
    assert_eq!(config.outputs[1].output_type, "file");
    assert_eq!(config.outputs[2].output_type, "otel");
}

#[test]
fn test_receiver_config_various_hosts() {
    let hosts = vec!["127.0.0.1", "0.0.0.0", "::1", "localhost", "192.168.1.100"];

    for host in hosts {
        let config = ReceiverConfig {
            host: host.to_string(),
            http_port: 4318,
            grpc_port: 4317,
            max_body_size: 4 * 1024 * 1024,
            request_timeout_secs: 30,
            ..Default::default()
        };

        assert_eq!(config.host, host);
    }
}

#[test]
fn test_service_config_worker_thread_options() {
    let configs = vec![
        (None, "default thread count"),
        (Some(1), "single thread"),
        (Some(4), "4 threads"),
        (Some(8), "8 threads"),
        (Some(16), "16 threads"),
    ];

    for (threads, label) in configs {
        let config = ServiceConfig {
            name: "Service".to_string(),
            display_name: label.to_string(),
            worker_threads: threads,
            channel_capacity: 10000,
            shutdown_timeout_secs: 30,
        };

        assert_eq!(config.worker_threads, threads);
    }
}

#[test]
fn test_logging_config_text_vs_json() {
    let text_config = LoggingConfig {
        log_level: "debug".to_string(),
        format: LogFormat::Text,
        output_path: Some(PathBuf::from("/var/log/text.log")),
    };

    let json_config = LoggingConfig {
        log_level: "info".to_string(),
        format: LogFormat::Json,
        output_path: Some(PathBuf::from("/var/log/json.log")),
    };

    assert_eq!(text_config.format, LogFormat::Text);
    assert_eq!(json_config.format, LogFormat::Json);
}

#[test]
fn test_output_config_http_exporter() {
    let config = OutputConfig {
        output_type: "http".to_string(),
        settings: json!({
            "endpoint": "https://collector.example.com/v1/logs",
            "batch_size": 100,
            "max_retries": 3,
            "retry_delay_ms": 100,
            "timeout_secs": 30,
            "headers": {
                "authorization": "Bearer token_value",
                "x-custom-header": "custom_value"
            }
        }),
    };

    assert_eq!(config.output_type, "http");
    assert_eq!(config.settings["endpoint"], "https://collector.example.com/v1/logs");
    assert_eq!(config.settings["batch_size"], 100);
}

#[test]
fn test_output_config_file_exporter() {
    let config = OutputConfig {
        output_type: "file".to_string(),
        settings: json!({
            "path": "/var/log/application.log",
            "max_size_bytes": 104857600,
            "max_backup_count": 10,
            "compression": "gzip"
        }),
    };

    assert_eq!(config.output_type, "file");
    assert_eq!(config.settings["path"], "/var/log/application.log");
    assert_eq!(config.settings["max_size_bytes"], 104857600);
}

#[test]
fn test_receiver_config_high_buffer_size() {
    let config = ReceiverConfig {
        host: "0.0.0.0".to_string(),
        http_port: 4318,
        grpc_port: 4317,
        max_body_size: 512 * 1024 * 1024, // 512 MB
        request_timeout_secs: 60,
        ..Default::default()
    };

    assert_eq!(config.max_body_size, 512 * 1024 * 1024);
}

#[test]
fn test_service_config_graceful_shutdown_values() {
    let configs = vec![
        (10u64, "quick"),
        (30u64, "standard"),
        (60u64, "extended"),
        (120u64, "long"),
    ];

    for (timeout_secs, label) in configs {
        let config = ServiceConfig {
            name: "Service".to_string(),
            display_name: label.to_string(),
            worker_threads: Some(4),
            channel_capacity: 10000,
            shutdown_timeout_secs: timeout_secs,
        };

        assert_eq!(config.shutdown_timeout_secs, timeout_secs);
    }
}

#[test]
fn test_config_serialization_roundtrip() {
    let original = ReceiverConfig {
        host: "192.168.1.100".to_string(),
        http_port: 8080,
        grpc_port: 9090,
        max_body_size: 50 * 1024 * 1024,
        request_timeout_secs: 45,
        ..Default::default()
    };

    let json = serde_json::to_string(&original).unwrap();
    let deserialized: ReceiverConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(original.host, deserialized.host);
    assert_eq!(original.http_port, deserialized.http_port);
    assert_eq!(original.grpc_port, deserialized.grpc_port);
    assert_eq!(original.max_body_size, deserialized.max_body_size);
    assert_eq!(original.request_timeout_secs, deserialized.request_timeout_secs);
}

#[test]
fn test_output_config_complex_settings() {
    let settings = json!({
        "primary": {
            "endpoint": "http://primary-collector:4317",
            "priority": 1
        },
        "secondary": {
            "endpoint": "http://secondary-collector:4317",
            "priority": 2
        },
        "fallback": {
            "endpoint": "http://fallback-collector:4317",
            "priority": 3
        },
        "circuit_breaker": {
            "failure_threshold": 5,
            "timeout_secs": 60
        }
    });

    let config = OutputConfig {
        output_type: "failover".to_string(),
        settings,
    };

    assert!(config.settings["primary"].is_object());
    assert!(config.settings["circuit_breaker"].is_object());
}

#[test]
fn test_config_defaults_reasonable() {
    let logging = LoggingConfig {
        log_level: "info".to_string(),
        format: LogFormat::Json,
        output_path: None,
    };
    let receiver = ReceiverConfig::default();
    let service = ServiceConfig::default();

    // Verify defaults are reasonable
    assert_eq!(receiver.http_port, 4318);
    assert_eq!(receiver.grpc_port, 4317);
    assert!(receiver.max_body_size > 0);
    assert!(service.channel_capacity > 0);
    assert!(service.shutdown_timeout_secs > 0);
}

#[test]
fn test_logging_path_with_env_vars() {
    // Config might support env var expansion like ${VAR}
    let config = LoggingConfig {
        log_level: "info".to_string(),
        format: LogFormat::Text,
        output_path: Some(PathBuf::from("/var/log/tc-otel/app.log")),
    };

    assert!(config.output_path.is_some());
    let path = config.output_path.unwrap();
    assert!(path.to_string_lossy().contains("tc-otel"));
}

#[test]
fn test_multiple_otel_exporters_config() {
    let outputs = vec![
        OutputConfig {
            output_type: "otel_http".to_string(),
            settings: json!({
                "endpoint": "http://otel-collector:4318/v1/logs",
                "protocol": "http"
            }),
        },
        OutputConfig {
            output_type: "otel_grpc".to_string(),
            settings: json!({
                "endpoint": "otel-collector:4317",
                "protocol": "grpc"
            }),
        },
    ];

    assert_eq!(outputs[0].output_type, "otel_http");
    assert_eq!(outputs[1].output_type, "otel_grpc");
    assert_eq!(outputs[0].settings["protocol"], "http");
    assert_eq!(outputs[1].settings["protocol"], "grpc");
}
