//! End-to-end regression tests for metrics pipeline via TCP transport.
//!
//! Validates that metric features added in GasTown era work end-to-end against tc31-xar-base:
//! - Task cycle time metrics with jitter/min/max/avg (commit 8b1b9a6)
//! - ADS connection health metrics (dc974ec)
//! - Custom metric definitions via config (41342b5, d60728f)
//! - PLC CPU / memory utilization metrics (740333e)
//! - Prometheus/OTEL Collector/Datadog exporters (6cda780)
//!
//! Prerequisites:
//!   - docker and docker-compose installed
//!   - TC_RUNTIME_IMAGE env var set for tc-runtime profile
//!   - Stack brought up via `./scripts/run-runtime-tests.sh --profile tc-runtime`
//!
//! The test waits up to 30 seconds for metrics to appear in /tmp/otlp.jsonl.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const MAX_WAIT_SECS: u64 = 30;
const POLL_INTERVAL_MS: u64 = 500;
const OTLP_OUTPUT: &str = "/tmp/otlp.jsonl";

/// Helper: Read OTLP JSONL file and return all valid records.
///
/// Each line is expected to be a JSON object with resourceMetrics.
/// Invalid lines are silently skipped.
fn read_otlp_metrics(path: &str) -> std::io::Result<Vec<serde_json::Value>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut records = Vec::new();
    for line in reader.lines() {
        if let Ok(line) = line {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                records.push(json);
            }
        }
    }

    Ok(records)
}

/// Helper: Extract all metric names from an OTLP resourceMetrics record.
fn extract_metric_names(record: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();

    if let Some(rm_array) = record
        .get("resourceMetrics")
        .and_then(|rm| rm.as_array())
    {
        for rm in rm_array {
            if let Some(sm_array) = rm
                .get("scopeMetrics")
                .and_then(|sm| sm.as_array())
            {
                for sm in sm_array {
                    if let Some(metrics_array) = sm
                        .get("metrics")
                        .and_then(|m| m.as_array())
                    {
                        for metric in metrics_array {
                            if let Some(name) = metric.get("name").and_then(|n| n.as_str()) {
                                names.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    names
}

/// Wait for metrics matching a predicate. Returns records if found within MAX_WAIT_SECS.
/// Panics if timeout exceeded.
fn wait_for_metrics<F>(predicate: F) -> Vec<serde_json::Value>
where
    F: Fn(&[String]) -> bool,
{
    let start = Instant::now();
    let max_duration = Duration::from_secs(MAX_WAIT_SECS);

    loop {
        if start.elapsed() > max_duration {
            panic!(
                "Timeout waiting for metrics in {} (waited {}s)",
                OTLP_OUTPUT, MAX_WAIT_SECS
            );
        }

        if Path::new(OTLP_OUTPUT).exists() {
            if let Ok(records) = read_otlp_metrics(OTLP_OUTPUT) {
                let all_names: Vec<String> =
                    records.iter().flat_map(|r| extract_metric_names(r)).collect();

                if predicate(&all_names) {
                    return records;
                }
            }
        }

        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
    }
}

/// Test: Task cycle time metrics with jitter/min/max/avg (commit 8b1b9a6)
///
/// Validates:
/// - Metric name: plc.task.cycle_time.avg (or similar task cycle metrics)
/// - Metric appears in /tmp/otlp.jsonl with proper structure
#[test]
#[ignore] // Requires docker stack to be running: ./scripts/run-runtime-tests.sh --profile tc-runtime
fn test_task_cycle_time_metrics_e2e() {
    let found_metrics = wait_for_metrics(|names| {
        names.iter().any(|n| n.starts_with("plc.task.cycle_time"))
    });

    let all_names: Vec<String> = found_metrics
        .iter()
        .flat_map(|r| extract_metric_names(r))
        .collect();

    let cycle_time_metrics: Vec<_> = all_names
        .iter()
        .filter(|n| n.starts_with("plc.task.cycle_time"))
        .collect();

    assert!(
        !cycle_time_metrics.is_empty(),
        "Expected task cycle time metrics (e.g., plc.task.cycle_time.avg) in OTLP output"
    );

    println!(
        "✓ Found task cycle time metrics: {:?}",
        cycle_time_metrics
    );
}

/// Test: ADS connection health metrics (commit dc974ec)
///
/// Validates:
/// - Metric names: ads.connections.active, ads.connections.accepted, ads.connections.rejected, etc.
/// - Metrics appear in /tmp/otlp.jsonl with proper structure
#[test]
#[ignore] // Requires docker stack to be running: ./scripts/run-runtime-tests.sh --profile tc-runtime
fn test_ads_connection_health_metrics_e2e() {
    let found_metrics = wait_for_metrics(|names| {
        names.iter().any(|n| n.starts_with("ads.connections"))
    });

    let all_names: Vec<String> = found_metrics
        .iter()
        .flat_map(|r| extract_metric_names(r))
        .collect();

    let ads_metrics: Vec<_> = all_names
        .iter()
        .filter(|n| n.starts_with("ads.connections"))
        .collect();

    assert!(
        !ads_metrics.is_empty(),
        "Expected ADS connection health metrics (e.g., ads.connections.active) in OTLP output"
    );

    println!("✓ Found ADS health metrics: {:?}", ads_metrics);
}

/// Test: Custom metric definitions via config (commits 41342b5, d60728f)
///
/// Validates:
/// - Custom metrics defined in config.example.json are exported
/// - Expected metric names: plc.motor.temperature, plc.parts_produced
/// - Metrics appear in /tmp/otlp.jsonl with proper structure
#[test]
#[ignore] // Requires docker stack to be running: ./scripts/run-runtime-tests.sh --profile tc-runtime
fn test_custom_metric_definitions_e2e() {
    let found_metrics = wait_for_metrics(|names| {
        names.iter().any(|n| {
            n == "plc.motor.temperature"
                || n == "plc.parts_produced"
                || n.starts_with("plc.motor.")
                || n.starts_with("plc.parts_")
        })
    });

    let all_names: Vec<String> = found_metrics
        .iter()
        .flat_map(|r| extract_metric_names(r))
        .collect();

    let custom_metrics: Vec<_> = all_names
        .iter()
        .filter(|n| n.starts_with("plc.motor.") || n.starts_with("plc.parts_"))
        .collect();

    assert!(
        !custom_metrics.is_empty(),
        "Expected custom metrics (plc.motor.temperature, plc.parts_produced) in OTLP output"
    );

    println!("✓ Found custom metrics: {:?}", custom_metrics);
}

/// Test: PLC CPU / memory utilization metrics (commit 740333e)
///
/// Validates:
/// - Metric names: plc.cpu.estimated_load, plc.memory.available, plc.memory.used
/// - Metrics appear in /tmp/otlp.jsonl with proper structure
#[test]
#[ignore] // Requires docker stack to be running: ./scripts/run-runtime-tests.sh --profile tc-runtime
fn test_plc_system_metrics_e2e() {
    let found_metrics = wait_for_metrics(|names| {
        names
            .iter()
            .any(|n| n.starts_with("plc.cpu.") || n.starts_with("plc.memory."))
    });

    let all_names: Vec<String> = found_metrics
        .iter()
        .flat_map(|r| extract_metric_names(r))
        .collect();

    let system_metrics: Vec<_> = all_names
        .iter()
        .filter(|n| n.starts_with("plc.cpu.") || n.starts_with("plc.memory."))
        .collect();

    assert!(
        !system_metrics.is_empty(),
        "Expected PLC system metrics (plc.cpu.*, plc.memory.*) in OTLP output"
    );

    println!("✓ Found system metrics: {:?}", system_metrics);
}

/// Test: Prometheus/OTEL Collector/Datadog exporters (commit 6cda780)
///
/// Validates:
/// - OTLP metrics are properly formatted for OTEL Collector (this validates export)
/// - Records have resourceMetrics structure with proper attributes
#[test]
#[ignore] // Requires docker stack to be running: ./scripts/run-runtime-tests.sh --profile tc-runtime
fn test_metrics_export_format_e2e() {
    let found_metrics = wait_for_metrics(|names| !names.is_empty());

    // Validate OTLP structure
    assert!(!found_metrics.is_empty(), "Expected at least one metric record");

    let first_record = &found_metrics[0];

    // Must have resourceMetrics at top level
    assert!(
        first_record.get("resourceMetrics").is_some(),
        "Missing 'resourceMetrics' in OTLP record"
    );

    let resource_metrics = first_record.get("resourceMetrics").unwrap();
    assert!(resource_metrics.is_array(), "resourceMetrics must be an array");

    let rm_array = resource_metrics.as_array().unwrap();
    assert!(!rm_array.is_empty(), "resourceMetrics array must not be empty");

    let first_rm = &rm_array[0];

    // Validate resource
    assert!(
        first_rm.get("resource").is_some(),
        "Missing 'resource' in resourceMetrics"
    );

    // Validate scopeMetrics
    assert!(
        first_rm.get("scopeMetrics").is_some(),
        "Missing 'scopeMetrics' in resourceMetrics"
    );

    let scope_metrics = first_rm.get("scopeMetrics").unwrap();
    assert!(scope_metrics.is_array(), "scopeMetrics must be an array");

    let sm_array = scope_metrics.as_array().unwrap();
    assert!(!sm_array.is_empty(), "scopeMetrics array must not be empty");

    let first_sm = &sm_array[0];

    // Validate metrics array
    assert!(
        first_sm.get("metrics").is_some(),
        "Missing 'metrics' in scopeMetrics"
    );

    let metrics = first_sm.get("metrics").unwrap();
    assert!(metrics.is_array(), "metrics must be an array");

    let metrics_array = metrics.as_array().unwrap();
    assert!(
        !metrics_array.is_empty(),
        "metrics array must not be empty"
    );

    let first_metric = &metrics_array[0];

    // Validate metric has name, description, unit
    assert!(
        first_metric.get("name").is_some(),
        "Metric must have 'name'"
    );
    assert!(
        first_metric.get("description").is_some(),
        "Metric must have 'description'"
    );
    assert!(
        first_metric.get("unit").is_some(),
        "Metric must have 'unit'"
    );

    // Metric must have one of: gauge, sum, histogram
    let has_value_type = first_metric.get("gauge").is_some()
        || first_metric.get("sum").is_some()
        || first_metric.get("histogram").is_some();

    assert!(
        has_value_type,
        "Metric must have gauge, sum, or histogram"
    );

    println!("✓ Metrics export format is valid OTLP");
}
