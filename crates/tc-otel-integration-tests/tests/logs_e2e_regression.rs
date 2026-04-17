//! End-to-end regression test for logs pipeline via TCP transport.
//!
//! Validates that log records flow through the tc-otel service, are exported via OTLP,
//! and land in the collector's output file with proper structure and content.
//!
//! Prerequisites:
//!   - docker and docker-compose installed
//!   - TC_RUNTIME_IMAGE env var set (for tc-runtime profile)
//!   - Stack brought up via `./scripts/run-runtime-tests.sh --profile tc-runtime`
//!
//! The test waits up to 30 seconds for logs to appear in /tmp/otlp.jsonl.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::{Duration, Instant};

/// Test: Logs pipeline e2e regression
///
/// This test validates that:
/// 1. The PLC runtime generates log records (spontaneously or via trigger)
/// 2. tc-otel service receives and exports them via OTLP
/// 3. The OTLP collector writes them to /tmp/otlp.jsonl
/// 4. Records have required fields: resourceLogs with non-empty body and severityText
#[test]
#[ignore] // Requires docker stack to be running: ./scripts/run-runtime-tests.sh --profile tc-runtime
fn test_logs_e2e_pipeline() {
    const MAX_WAIT_SECS: u64 = 30;
    const POLL_INTERVAL_MS: u64 = 500;
    const OTLP_OUTPUT: &str = "/tmp/otlp.jsonl";

    let start = Instant::now();
    let max_duration = Duration::from_secs(MAX_WAIT_SECS);

    // Poll until we find at least one valid log record
    let found_log = loop {
        if start.elapsed() > max_duration {
            panic!(
                "Timeout waiting for OTLP logs in {} (waited {}s)",
                OTLP_OUTPUT, MAX_WAIT_SECS
            );
        }

        // Try to read logs without pre-checking existence (avoid TOCTOU)
        if let Ok(log_records) = read_otlp_logs(OTLP_OUTPUT) {
            if !log_records.is_empty() {
                break log_records;
            }
        }

        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
    };

    // Validate first record structure
    let first = &found_log[0];

    // Must have resourceLogs at the top level
    assert!(
        first.get("resourceLogs").is_some(),
        "Missing 'resourceLogs' in OTLP record"
    );

    let resource_logs = first.get("resourceLogs").unwrap();
    assert!(resource_logs.is_array(), "resourceLogs must be an array");

    let resource_logs_arr = resource_logs.as_array().unwrap();
    assert!(
        !resource_logs_arr.is_empty(),
        "resourceLogs array must not be empty"
    );

    let first_resource = &resource_logs_arr[0];

    // Navigate through OTLP structure: resourceLogs[0] -> scopeLogs -> logRecords
    let scope_logs = first_resource
        .get("scopeLogs")
        .and_then(|sl| sl.as_array())
        .expect("Missing or invalid scopeLogs");

    assert!(!scope_logs.is_empty(), "scopeLogs must not be empty");

    let log_records = scope_logs[0]
        .get("logRecords")
        .and_then(|lr| lr.as_array())
        .expect("Missing or invalid logRecords");

    assert!(!log_records.is_empty(), "logRecords must not be empty");

    let log_record = &log_records[0];

    // Validate required fields
    let body = log_record
        .get("body")
        .and_then(|b| b.get("stringValue"))
        .and_then(|sv| sv.as_str());

    assert!(
        body.is_some() && !body.unwrap().is_empty(),
        "Log record must have non-empty 'body.stringValue'"
    );

    let severity_text = log_record.get("severityText").and_then(|st| st.as_str());

    assert!(
        severity_text.is_some() && !severity_text.unwrap().is_empty(),
        "Log record must have non-empty 'severityText'"
    );

    // Additional validation: check for timestamp
    assert!(
        log_record.get("timeUnixNano").is_some(),
        "Log record should have 'timeUnixNano'"
    );

    println!(
        "✓ Found valid log record: body='{}', severity='{}'",
        body.unwrap(),
        severity_text.unwrap()
    );
}

/// Read OTLP JSONL file and return all valid log records.
///
/// Each line is expected to be a JSON object with resourceLogs.
/// Invalid lines are silently skipped.
fn read_otlp_logs(path: &str) -> std::io::Result<Vec<serde_json::Value>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let records = reader
        .lines()
        .filter_map(|line| {
            let line = line.ok()?.trim().to_string();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str::<serde_json::Value>(&line).ok()
        })
        .collect();

    Ok(records)
}

#[cfg(test)]
mod tests {
    /// Helper test to validate OTLP structure parsing
    /// (This is a unit test for the helper, not a runtime test)
    #[test]
    fn test_otlp_structure_parsing() {
        let sample_otlp = serde_json::json!({
            "resourceLogs": [
                {
                    "resource": {
                        "attributes": [
                            {"key": "service.name", "value": {"stringValue": "TC-OTel"}}
                        ]
                    },
                    "scopeLogs": [
                        {
                            "scope": {"name": "tc_otel.logger"},
                            "logRecords": [
                                {
                                    "timeUnixNano": "1715705400000000000",
                                    "severityNumber": 9,
                                    "severityText": "INFO",
                                    "body": {"stringValue": "Test log message"},
                                    "attributes": []
                                }
                            ]
                        }
                    ]
                }
            ]
        });

        let body = sample_otlp
            .get("resourceLogs")
            .and_then(|rl| rl.as_array())
            .and_then(|arr| arr.first())
            .and_then(|r| r.get("scopeLogs"))
            .and_then(|sl| sl.as_array())
            .and_then(|arr| arr.first())
            .and_then(|s| s.get("logRecords"))
            .and_then(|lr| lr.as_array())
            .and_then(|arr| arr.first())
            .and_then(|log| log.get("body"))
            .and_then(|b| b.get("stringValue"));

        assert_eq!(
            body,
            Some(&serde_json::Value::String("Test log message".into()))
        );
    }
}
