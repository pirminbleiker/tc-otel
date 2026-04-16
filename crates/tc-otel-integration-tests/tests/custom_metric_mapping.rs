//! Integration tests for custom metric definitions via config (to-754.5)
//!
//! Tests the mapping of PLC symbols to OTEL metric names/descriptions/units/kinds
//! via configuration. Covers:
//! - Config parsing of custom_metrics definitions
//! - MetricMapper applies matching symbol → metric overrides
//! - Unmatched symbols pass through unchanged
//! - Kind override (e.g., symbol value exported as Sum instead of Gauge)
//! - Validation rejects duplicate symbols and empty metric names
//! - Round-trip: config → mapper → apply → MetricRecord → OTLP payload

use tc_otel_core::config::{CustomMetricDef, MetricKindConfig, MetricsConfig};
use tc_otel_core::{MetricEntry, MetricKind, MetricMapper, MetricRecord};
use tc_otel_export::OtelExporter;

// ─── Config parsing tests ────────────────────────────────────────

#[test]
fn test_custom_metrics_config_parses_from_json() {
    let json = r#"{
        "cycle_time_enabled": true,
        "cycle_time_window": 1000,
        "custom_metrics": [
            {
                "symbol": "GVL.motor.temperature",
                "metric_name": "plc.motor.temperature",
                "description": "Motor 1 winding temperature",
                "unit": "Cel",
                "kind": "gauge"
            },
            {
                "symbol": "GVL.partsProduced",
                "metric_name": "plc.parts_produced",
                "description": "Total parts produced",
                "unit": "{count}",
                "kind": "sum",
                "is_monotonic": true
            }
        ]
    }"#;

    let config: MetricsConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.custom_metrics.len(), 2);

    assert_eq!(config.custom_metrics[0].symbol, "GVL.motor.temperature");
    assert_eq!(
        config.custom_metrics[0].metric_name,
        "plc.motor.temperature"
    );
    assert_eq!(
        config.custom_metrics[0].description,
        "Motor 1 winding temperature"
    );
    assert_eq!(config.custom_metrics[0].unit, "Cel");
    assert_eq!(config.custom_metrics[0].kind, MetricKindConfig::Gauge);
    assert!(!config.custom_metrics[0].is_monotonic);

    assert_eq!(config.custom_metrics[1].symbol, "GVL.partsProduced");
    assert_eq!(config.custom_metrics[1].kind, MetricKindConfig::Sum);
    assert!(config.custom_metrics[1].is_monotonic);
}

#[test]
fn test_custom_metrics_defaults_to_empty() {
    let json = r#"{}"#;
    let config: MetricsConfig = serde_json::from_str(json).unwrap();
    assert!(config.custom_metrics.is_empty());
}

#[test]
fn test_custom_metric_def_defaults() {
    let json = r#"{
        "symbol": "GVL.temp",
        "metric_name": "plc.temp"
    }"#;
    let def: CustomMetricDef = serde_json::from_str(json).unwrap();
    assert_eq!(def.symbol, "GVL.temp");
    assert_eq!(def.metric_name, "plc.temp");
    assert_eq!(def.description, "");
    assert_eq!(def.unit, "");
    assert_eq!(def.kind, MetricKindConfig::Gauge);
    assert!(!def.is_monotonic);
}

#[test]
fn test_metric_kind_config_all_variants() {
    let gauge: MetricKindConfig = serde_json::from_str(r#""gauge""#).unwrap();
    let sum: MetricKindConfig = serde_json::from_str(r#""sum""#).unwrap();
    let histogram: MetricKindConfig = serde_json::from_str(r#""histogram""#).unwrap();
    assert_eq!(gauge, MetricKindConfig::Gauge);
    assert_eq!(sum, MetricKindConfig::Sum);
    assert_eq!(histogram, MetricKindConfig::Histogram);
}

#[test]
fn test_metric_kind_config_to_metric_kind() {
    assert_eq!(MetricKindConfig::Gauge.to_metric_kind(), MetricKind::Gauge);
    assert_eq!(MetricKindConfig::Sum.to_metric_kind(), MetricKind::Sum);
    assert_eq!(
        MetricKindConfig::Histogram.to_metric_kind(),
        MetricKind::Histogram
    );
}

// ─── MetricMapper construction tests ─────────────────────────────

#[test]
fn test_mapper_from_empty_config() {
    let config = MetricsConfig::default();
    let mapper = MetricMapper::from_config(&config);
    assert_eq!(mapper.len(), 0);
}

#[test]
fn test_mapper_from_config_with_definitions() {
    let config = MetricsConfig {
        custom_metrics: vec![
            CustomMetricDef {
                symbol: "GVL.motor.temp".to_string(),
                metric_name: "plc.motor.temperature".to_string(),
                description: "Motor temperature".to_string(),
                unit: "Cel".to_string(),
                kind: MetricKindConfig::Gauge,
                is_monotonic: false,
                ..CustomMetricDef::default()
            },
            CustomMetricDef {
                symbol: "GVL.count".to_string(),
                metric_name: "plc.count".to_string(),
                description: "Part count".to_string(),
                unit: "{count}".to_string(),
                kind: MetricKindConfig::Sum,
                is_monotonic: true,
                ..CustomMetricDef::default()
            },
        ],
        ..MetricsConfig::default()
    };

    let mapper = MetricMapper::from_config(&config);
    assert_eq!(mapper.len(), 2);
}

// ─── MetricMapper.apply() tests ──────────────────────────────────

#[test]
fn test_mapper_applies_matching_symbol() {
    let config = MetricsConfig {
        custom_metrics: vec![CustomMetricDef {
            symbol: "GVL.motor.temp".to_string(),
            metric_name: "plc.motor.temperature".to_string(),
            description: "Motor 1 winding temperature".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        }],
        ..MetricsConfig::default()
    };

    let mapper = MetricMapper::from_config(&config);

    let mut entry = MetricEntry::gauge("raw.metric".to_string(), 72.5);
    entry.attributes.insert(
        "plc.symbol".to_string(),
        serde_json::json!("GVL.motor.temp"),
    );

    let applied = mapper.apply(&mut entry);
    assert!(applied);
    assert_eq!(entry.name, "plc.motor.temperature");
    assert_eq!(entry.description, "Motor 1 winding temperature");
    assert_eq!(entry.unit, "Cel");
    assert_eq!(entry.kind, MetricKind::Gauge);
    assert_eq!(entry.value, 72.5); // value preserved
}

#[test]
fn test_mapper_no_match_passes_through() {
    let config = MetricsConfig {
        custom_metrics: vec![CustomMetricDef {
            symbol: "GVL.motor.temp".to_string(),
            metric_name: "plc.motor.temperature".to_string(),
            description: "Motor temperature".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        }],
        ..MetricsConfig::default()
    };

    let mapper = MetricMapper::from_config(&config);

    let mut entry = MetricEntry::gauge("plc.other.metric".to_string(), 50.0);
    entry.attributes.insert(
        "plc.symbol".to_string(),
        serde_json::json!("GVL.unknown.var"),
    );

    let applied = mapper.apply(&mut entry);
    assert!(!applied);
    assert_eq!(entry.name, "plc.other.metric"); // unchanged
}

#[test]
fn test_mapper_no_symbol_attribute_passes_through() {
    let config = MetricsConfig {
        custom_metrics: vec![CustomMetricDef {
            symbol: "GVL.motor.temp".to_string(),
            metric_name: "plc.motor.temperature".to_string(),
            description: "".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        }],
        ..MetricsConfig::default()
    };

    let mapper = MetricMapper::from_config(&config);

    let mut entry = MetricEntry::gauge("plc.some.metric".to_string(), 10.0);
    // No plc.symbol attribute at all

    let applied = mapper.apply(&mut entry);
    assert!(!applied);
    assert_eq!(entry.name, "plc.some.metric"); // unchanged
}

#[test]
fn test_mapper_overrides_kind_to_sum() {
    let config = MetricsConfig {
        custom_metrics: vec![CustomMetricDef {
            symbol: "GVL.partsProduced".to_string(),
            metric_name: "plc.parts_produced".to_string(),
            description: "Total parts produced".to_string(),
            unit: "{count}".to_string(),
            kind: MetricKindConfig::Sum,
            is_monotonic: true,
            ..CustomMetricDef::default()
        }],
        ..MetricsConfig::default()
    };

    let mapper = MetricMapper::from_config(&config);

    // Incoming metric is a Gauge (default from ADS), but config says it should be Sum
    let mut entry = MetricEntry::gauge("raw.counter".to_string(), 42.0);
    entry.attributes.insert(
        "plc.symbol".to_string(),
        serde_json::json!("GVL.partsProduced"),
    );

    let applied = mapper.apply(&mut entry);
    assert!(applied);
    assert_eq!(entry.name, "plc.parts_produced");
    assert_eq!(entry.kind, MetricKind::Sum);
    assert!(entry.is_monotonic);
    assert_eq!(entry.value, 42.0); // value preserved
}

#[test]
fn test_mapper_preserves_existing_attributes() {
    let config = MetricsConfig {
        custom_metrics: vec![CustomMetricDef {
            symbol: "GVL.axis1.pos".to_string(),
            metric_name: "plc.axis.position".to_string(),
            description: "Axis 1 position".to_string(),
            unit: "mm".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        }],
        ..MetricsConfig::default()
    };

    let mapper = MetricMapper::from_config(&config);

    let mut entry = MetricEntry::gauge("raw.pos".to_string(), 150.5);
    entry
        .attributes
        .insert("plc.symbol".to_string(), serde_json::json!("GVL.axis1.pos"));
    entry
        .attributes
        .insert("plc.data_type".to_string(), serde_json::json!("LREAL"));
    entry.task_name = "MotionTask".to_string();
    entry.hostname = "plc-01".to_string();

    mapper.apply(&mut entry);

    // Original attributes preserved
    assert_eq!(
        entry.attributes["plc.symbol"],
        serde_json::json!("GVL.axis1.pos")
    );
    assert_eq!(
        entry.attributes["plc.data_type"],
        serde_json::json!("LREAL")
    );
    assert_eq!(entry.task_name, "MotionTask");
    assert_eq!(entry.hostname, "plc-01");
}

#[test]
fn test_mapper_with_multiple_definitions() {
    let config = MetricsConfig {
        custom_metrics: vec![
            CustomMetricDef {
                symbol: "GVL.motor.temp".to_string(),
                metric_name: "plc.motor.temperature".to_string(),
                description: "Motor temperature".to_string(),
                unit: "Cel".to_string(),
                kind: MetricKindConfig::Gauge,
                is_monotonic: false,
                ..CustomMetricDef::default()
            },
            CustomMetricDef {
                symbol: "GVL.partsProduced".to_string(),
                metric_name: "plc.parts_produced".to_string(),
                description: "Total parts".to_string(),
                unit: "{count}".to_string(),
                kind: MetricKindConfig::Sum,
                is_monotonic: true,
                ..CustomMetricDef::default()
            },
            CustomMetricDef {
                symbol: "GVL.axis1.pos".to_string(),
                metric_name: "plc.axis.position".to_string(),
                description: "Axis 1 position".to_string(),
                unit: "mm".to_string(),
                kind: MetricKindConfig::Gauge,
                is_monotonic: false,
                ..CustomMetricDef::default()
            },
        ],
        ..MetricsConfig::default()
    };

    let mapper = MetricMapper::from_config(&config);
    assert_eq!(mapper.len(), 3);

    // First symbol
    let mut e1 = MetricEntry::gauge("raw1".to_string(), 72.5);
    e1.attributes.insert(
        "plc.symbol".to_string(),
        serde_json::json!("GVL.motor.temp"),
    );
    assert!(mapper.apply(&mut e1));
    assert_eq!(e1.name, "plc.motor.temperature");

    // Second symbol
    let mut e2 = MetricEntry::gauge("raw2".to_string(), 42.0);
    e2.attributes.insert(
        "plc.symbol".to_string(),
        serde_json::json!("GVL.partsProduced"),
    );
    assert!(mapper.apply(&mut e2));
    assert_eq!(e2.name, "plc.parts_produced");
    assert_eq!(e2.kind, MetricKind::Sum);

    // Third symbol
    let mut e3 = MetricEntry::gauge("raw3".to_string(), 150.0);
    e3.attributes
        .insert("plc.symbol".to_string(), serde_json::json!("GVL.axis1.pos"));
    assert!(mapper.apply(&mut e3));
    assert_eq!(e3.name, "plc.axis.position");
    assert_eq!(e3.unit, "mm");
}

// ─── Validation tests ────────────────────────────────────────────

#[test]
fn test_mapper_validate_rejects_duplicate_symbols() {
    let defs = vec![
        CustomMetricDef {
            symbol: "GVL.motor.temp".to_string(),
            metric_name: "plc.motor.temperature".to_string(),
            description: "".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        },
        CustomMetricDef {
            symbol: "GVL.motor.temp".to_string(), // duplicate!
            metric_name: "plc.motor.temp2".to_string(),
            description: "".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        },
    ];

    let errors = MetricMapper::validate(&defs);
    assert!(!errors.is_empty());
    assert!(errors[0].contains("duplicate"));
}

#[test]
fn test_mapper_validate_rejects_empty_symbol() {
    let defs = vec![CustomMetricDef {
        symbol: "".to_string(),
        metric_name: "plc.temp".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        ..CustomMetricDef::default()
    }];

    let errors = MetricMapper::validate(&defs);
    assert!(!errors.is_empty());
    assert!(errors[0].contains("symbol"));
}

#[test]
fn test_mapper_validate_rejects_empty_metric_name() {
    let defs = vec![CustomMetricDef {
        symbol: "GVL.temp".to_string(),
        metric_name: "".to_string(),
        description: "".to_string(),
        unit: "".to_string(),
        kind: MetricKindConfig::Gauge,
        is_monotonic: false,
        ..CustomMetricDef::default()
    }];

    let errors = MetricMapper::validate(&defs);
    assert!(!errors.is_empty());
    assert!(errors[0].contains("metric_name"));
}

#[test]
fn test_mapper_validate_accepts_valid_defs() {
    let defs = vec![
        CustomMetricDef {
            symbol: "GVL.temp".to_string(),
            metric_name: "plc.temp".to_string(),
            description: "".to_string(),
            unit: "".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        },
        CustomMetricDef {
            symbol: "GVL.count".to_string(),
            metric_name: "plc.count".to_string(),
            description: "".to_string(),
            unit: "".to_string(),
            kind: MetricKindConfig::Sum,
            is_monotonic: true,
            ..CustomMetricDef::default()
        },
    ];

    let errors = MetricMapper::validate(&defs);
    assert!(errors.is_empty());
}

// ─── End-to-end: config → mapper → MetricRecord → OTLP ──────────

#[test]
fn test_end_to_end_custom_metric_to_otlp() {
    // Step 1: Parse config
    let json = r#"{
        "custom_metrics": [
            {
                "symbol": "GVL.motor.temp",
                "metric_name": "plc.motor.temperature",
                "description": "Motor 1 winding temperature",
                "unit": "Cel",
                "kind": "gauge"
            }
        ]
    }"#;
    let config: MetricsConfig = serde_json::from_str(json).unwrap();
    let mapper = MetricMapper::from_config(&config);

    // Step 2: Create a raw metric entry (as if from ADS)
    let mut entry = MetricEntry::gauge("raw.value".to_string(), 72.5);
    entry.attributes.insert(
        "plc.symbol".to_string(),
        serde_json::json!("GVL.motor.temp"),
    );
    entry.project_name = "ProductionLine".to_string();
    entry.app_name = "HydraulicPress".to_string();
    entry.hostname = "plc-01".to_string();

    // Step 3: Apply mapping
    assert!(mapper.apply(&mut entry));

    // Step 4: Convert to MetricRecord
    let record = MetricRecord::from_metric_entry(entry);
    assert_eq!(record.name, "plc.motor.temperature");
    assert_eq!(record.description, "Motor 1 winding temperature");
    assert_eq!(record.unit, "Cel");
    assert_eq!(record.kind, MetricKind::Gauge);
    assert_eq!(record.value, 72.5);

    // Step 5: Build OTLP payload
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(metric["name"], "plc.motor.temperature");
    assert_eq!(metric["description"], "Motor 1 winding temperature");
    assert_eq!(metric["unit"], "Cel");
    assert!(metric.get("gauge").is_some());
    assert_eq!(metric["gauge"]["dataPoints"][0]["asDouble"], 72.5);
}

#[test]
fn test_end_to_end_counter_mapping_to_otlp() {
    let config = MetricsConfig {
        custom_metrics: vec![CustomMetricDef {
            symbol: "GVL.partsProduced".to_string(),
            metric_name: "plc.parts_produced".to_string(),
            description: "Total parts produced".to_string(),
            unit: "{count}".to_string(),
            kind: MetricKindConfig::Sum,
            is_monotonic: true,
            ..CustomMetricDef::default()
        }],
        ..MetricsConfig::default()
    };

    let mapper = MetricMapper::from_config(&config);

    let mut entry = MetricEntry::gauge("raw.counter".to_string(), 12345.0);
    entry.attributes.insert(
        "plc.symbol".to_string(),
        serde_json::json!("GVL.partsProduced"),
    );
    entry.project_name = "Factory".to_string();
    entry.app_name = "Assembly".to_string();

    mapper.apply(&mut entry);

    let record = MetricRecord::from_metric_entry(entry);
    let exporter = OtelExporter::new("http://localhost:4318/v1/metrics".to_string(), 100, 3);
    let payload_str = exporter.build_otel_metrics_payload(&[record]).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_str).unwrap();

    let metric = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(metric["name"], "plc.parts_produced");
    assert!(metric.get("sum").is_some());
    assert_eq!(metric["sum"]["isMonotonic"], true);
    assert_eq!(metric["sum"]["dataPoints"][0]["asDouble"], 12345.0);
}

// ─── Config serialization round-trip ─────────────────────────────

#[test]
fn test_custom_metrics_config_round_trip() {
    let config = MetricsConfig {
        cycle_time_enabled: true,
        cycle_time_window: 500,
        custom_metrics: vec![CustomMetricDef {
            symbol: "GVL.motor.temp".to_string(),
            metric_name: "plc.motor.temperature".to_string(),
            description: "Motor temperature".to_string(),
            unit: "Cel".to_string(),
            kind: MetricKindConfig::Gauge,
            is_monotonic: false,
            ..CustomMetricDef::default()
        }],
        ..Default::default()
    };

    let json = serde_json::to_string(&config).unwrap();
    let parsed: MetricsConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(config, parsed);
}

// ─── Backward compatibility ──────────────────────────────────────

#[test]
fn test_existing_metrics_config_still_parses() {
    // Config without custom_metrics (existing configs in the wild)
    let json = r#"{
        "cycle_time_enabled": false,
        "cycle_time_window": 2000
    }"#;
    let config: MetricsConfig = serde_json::from_str(json).unwrap();
    assert!(!config.cycle_time_enabled);
    assert_eq!(config.cycle_time_window, 2000);
    assert!(config.custom_metrics.is_empty());
}
