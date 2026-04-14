//! End-to-end trace regression test
//!
//! Verifies that traces flow from PLC (tc31-xar-base) through tc-otel service
//! to the OTEL collector's file exporter (/tmp/otlp.jsonl).
//!
//! Requirements:
//! - Runtime stack must be up (tc-runtime profile)
//! - PLC running tc31-xar-base with trace-emitting workload
//! - /tmp/otlp.jsonl written by OTEL collector file exporter
//!
//! Test assertions:
//! - Traces pipeline produces ≥1 resourceSpans record
//! - Each span has required fields: traceId, spanId, name, startTimeUnixNano
//! - Log-trace correlation: logs with matching traceId/spanId to spans exist

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

/// Helper to parse OTLP JSONL file and extract resourceSpans
fn read_otlp_traces(path: &PathBuf) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let mut traces = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)?;

        // Each line may be resourceSpans or resourceLogs or resourceMetrics
        // We're interested in resourceSpans
        if value.get("resourceSpans").is_some() {
            traces.push(value);
        }
    }

    Ok(traces)
}

/// Helper to parse OTLP JSONL file and extract resourceLogs
fn read_otlp_logs(path: &PathBuf) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let mut logs = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)?;

        // Each line may be resourceSpans or resourceLogs or resourceMetrics
        // We're interested in resourceLogs
        if value.get("resourceLogs").is_some() {
            logs.push(value);
        }
    }

    Ok(logs)
}

/// Test: Verify traces appear in OTLP collector output
///
/// Precondition: Runtime stack with tc-runtime profile running, with PLC emitting traces.
/// This test waits for trace output and validates structure.
#[test]
#[ignore] // Run only with runtime stack up
fn test_traces_e2e_export() {
    let otlp_file = PathBuf::from("/tmp/otlp.jsonl");

    // Wait up to 30 seconds for traces to appear in OTLP file
    let mut found_traces = false;
    for attempt in 0..30 {
        if otlp_file.exists() {
            match read_otlp_traces(&otlp_file) {
                Ok(traces) if !traces.is_empty() => {
                    found_traces = true;
                    println!("Found {} trace records", traces.len());
                    break;
                }
                _ => {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        } else {
            thread::sleep(Duration::from_secs(1));
        }
        if attempt % 5 == 0 {
            eprintln!("Waiting for traces... (attempt {}/30)", attempt + 1);
        }
    }

    assert!(
        found_traces,
        "No trace records found in /tmp/otlp.jsonl after 30 seconds. \
         Ensure runtime stack is up with tc-runtime profile and PLC is emitting traces."
    );

    // Now validate the traces
    let traces = read_otlp_traces(&otlp_file).expect("Failed to parse traces");
    assert!(
        !traces.is_empty(),
        "Expected at least 1 trace record in /tmp/otlp.jsonl"
    );

    // Check first trace record structure
    let first_trace = &traces[0];
    let resource_spans = first_trace
        .get("resourceSpans")
        .expect("Missing resourceSpans");

    assert!(
        resource_spans.is_array(),
        "resourceSpans should be an array"
    );
    let spans_array = resource_spans.as_array().unwrap();

    assert!(
        !spans_array.is_empty(),
        "resourceSpans should contain at least one scopeSpans"
    );

    // Validate spans within first scopeSpans
    let first_scope_spans = &spans_array[0];
    let scope_spans = first_scope_spans
        .get("scopeSpans")
        .expect("Missing scopeSpans");

    assert!(scope_spans.is_array(), "scopeSpans should be an array");
    let scope_spans_array = scope_spans.as_array().unwrap();

    assert!(
        !scope_spans_array.is_empty(),
        "scopeSpans should contain at least one span scope"
    );

    // Get spans from first scope
    let first_scope = &scope_spans_array[0];
    let spans = first_scope
        .get("spans")
        .expect("Missing spans in scopeSpans");

    assert!(spans.is_array(), "spans should be an array");
    let spans_list = spans.as_array().unwrap();

    assert!(
        !spans_list.is_empty(),
        "spans array should not be empty in first scope"
    );

    // Validate required fields on first span
    let first_span = &spans_list[0];

    // Required fields: traceId, spanId, name, startTimeUnixNano
    assert!(
        first_span.get("traceId").is_some(),
        "Span missing required field: traceId"
    );
    assert!(
        first_span.get("spanId").is_some(),
        "Span missing required field: spanId"
    );
    assert!(
        first_span.get("name").is_some(),
        "Span missing required field: name"
    );
    assert!(
        first_span.get("startTimeUnixNano").is_some(),
        "Span missing required field: startTimeUnixNano"
    );

    // Validate field types
    assert!(
        first_span.get("traceId").unwrap().is_string(),
        "traceId should be a string"
    );
    assert!(
        first_span.get("spanId").unwrap().is_string(),
        "spanId should be a string"
    );
    assert!(
        first_span.get("name").unwrap().is_string(),
        "name should be a string"
    );
    assert!(
        first_span.get("startTimeUnixNano").unwrap().is_string()
            || first_span.get("startTimeUnixNano").unwrap().is_number(),
        "startTimeUnixNano should be a string or number"
    );

    println!(
        "Trace validation passed. First span: {}",
        first_span.get("name").unwrap()
    );
}

/// Test: Verify trace-log correlation
///
/// Looks for logs with matching traceId/spanId to spans in the same batch.
/// This validates that logs emitted within a trace context carry span information.
#[test]
#[ignore] // Run only with runtime stack up
fn test_traces_e2e_log_correlation() {
    let otlp_file = PathBuf::from("/tmp/otlp.jsonl");

    // Wait for both traces and logs
    let mut found_logs = false;
    let mut found_traces = false;

    for attempt in 0..30 {
        if otlp_file.exists() {
            match (read_otlp_traces(&otlp_file), read_otlp_logs(&otlp_file)) {
                (Ok(traces), Ok(logs)) if !traces.is_empty() && !logs.is_empty() => {
                    found_traces = !traces.is_empty();
                    found_logs = !logs.is_empty();
                    println!(
                        "Found {} trace records and {} log records",
                        traces.len(),
                        logs.len()
                    );
                    break;
                }
                _ => {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        } else {
            thread::sleep(Duration::from_secs(1));
        }
        if attempt % 5 == 0 {
            eprintln!(
                "Waiting for traces and logs... (attempt {}/30)",
                attempt + 1
            );
        }
    }

    assert!(
        found_traces,
        "No trace records found. Ensure runtime stack is up with tc-runtime profile."
    );
    assert!(
        found_logs,
        "No log records found. Ensure PLC is emitting logs."
    );

    let traces = read_otlp_traces(&otlp_file).expect("Failed to parse traces");
    let logs = read_otlp_logs(&otlp_file).expect("Failed to parse logs");

    // Extract all trace IDs and span IDs from spans
    let mut span_ids: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    for trace_record in &traces {
        if let Some(resource_spans) = trace_record.get("resourceSpans").and_then(|v| v.as_array()) {
            for scope_spans in resource_spans {
                if let Some(scopes) = scope_spans.get("scopeSpans").and_then(|v| v.as_array()) {
                    for scope in scopes {
                        if let Some(spans) = scope.get("spans").and_then(|v| v.as_array()) {
                            for span in spans {
                                if let (Some(trace_id), Some(span_id)) =
                                    (span.get("traceId"), span.get("spanId"))
                                {
                                    if let (Some(tid_str), Some(sid_str)) =
                                        (trace_id.as_str(), span_id.as_str())
                                    {
                                        span_ids.insert((tid_str.to_string(), sid_str.to_string()));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    println!(
        "Extracted {} unique (traceId, spanId) pairs from spans",
        span_ids.len()
    );

    // Now look for logs that reference these spans
    let mut correlated_count = 0;

    for log_record in &logs {
        if let Some(resource_logs) = log_record.get("resourceLogs").and_then(|v| v.as_array()) {
            for scope_logs in resource_logs {
                if let Some(scopes) = scope_logs.get("scopeLogs").and_then(|v| v.as_array()) {
                    for scope in scopes {
                        if let Some(log_records) =
                            scope.get("logRecords").and_then(|v| v.as_array())
                        {
                            for log in log_records {
                                // Check if log has traceId and spanId attributes
                                let log_trace_id =
                                    log.get("traceId").and_then(|v| v.as_str()).unwrap_or("");
                                let log_span_id =
                                    log.get("spanId").and_then(|v| v.as_str()).unwrap_or("");

                                // Also check in attributes (body field may have them)
                                if !log_trace_id.is_empty()
                                    && !log_span_id.is_empty()
                                    && span_ids.contains(&(
                                        log_trace_id.to_string(),
                                        log_span_id.to_string(),
                                    ))
                                {
                                    correlated_count += 1;
                                    println!(
                                        "Found correlated log: traceId={}, spanId={}",
                                        log_trace_id, log_span_id
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    assert!(
        correlated_count > 0,
        "No logs found with traceId/spanId matching any exported spans. \
         Either no logs were emitted within trace context, or trace-log correlation failed."
    );

    println!(
        "Trace-log correlation test passed: {} logs matched span context",
        correlated_count
    );
}

/// Test: Validate trace span fields are non-empty and well-formed
#[test]
#[ignore] // Run only with runtime stack up
fn test_traces_e2e_span_field_validation() {
    let otlp_file = PathBuf::from("/tmp/otlp.jsonl");

    // Wait for traces
    let mut found_traces = false;
    for _attempt in 0..30 {
        if otlp_file.exists() {
            match read_otlp_traces(&otlp_file) {
                Ok(traces) if !traces.is_empty() => {
                    found_traces = true;
                    break;
                }
                _ => {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        } else {
            thread::sleep(Duration::from_secs(1));
        }
    }

    assert!(found_traces, "No traces found in /tmp/otlp.jsonl");

    let traces = read_otlp_traces(&otlp_file).expect("Failed to parse traces");

    // Validate at least one span has all required fields non-empty
    let mut valid_spans = 0;

    for trace_record in &traces {
        if let Some(resource_spans) = trace_record.get("resourceSpans").and_then(|v| v.as_array()) {
            for scope_spans in resource_spans {
                if let Some(scopes) = scope_spans.get("scopeSpans").and_then(|v| v.as_array()) {
                    for scope in scopes {
                        if let Some(spans) = scope.get("spans").and_then(|v| v.as_array()) {
                            for span in spans {
                                let trace_id =
                                    span.get("traceId").and_then(|v| v.as_str()).unwrap_or("");
                                let span_id =
                                    span.get("spanId").and_then(|v| v.as_str()).unwrap_or("");
                                let name = span.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let start_time = span.get("startTimeUnixNano");

                                if !trace_id.is_empty()
                                    && !span_id.is_empty()
                                    && !name.is_empty()
                                    && start_time.is_some()
                                {
                                    valid_spans += 1;
                                    println!(
                                        "Valid span: name={}, traceId={}, spanId={}",
                                        name, trace_id, span_id
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    assert!(
        valid_spans > 0,
        "Expected at least one span with all required fields present and non-empty"
    );

    println!(
        "Span field validation passed: {} valid spans found",
        valid_spans
    );
}
