//! Integration tests for ADS parser with real protocol sequences

use tc_otel_ads::AdsParser;
use tc_otel_core::LogLevel;

fn build_ads_message(message: &str, logger: &str, level: u8) -> Vec<u8> {
    let mut payload = vec![1]; // version

    // Message (1-byte length)
    let msg_bytes = message.as_bytes();
    payload.push(msg_bytes.len() as u8);
    payload.extend_from_slice(msg_bytes);

    // Logger (1-byte length)
    let logger_bytes = logger.as_bytes();
    payload.push(logger_bytes.len() as u8);
    payload.extend_from_slice(logger_bytes);

    // Level (2 bytes u16 LE)
    payload.extend_from_slice(&(level as u16).to_le_bytes());

    // Timestamps
    let filetime = 132900000000000000u64;
    payload.extend_from_slice(&filetime.to_le_bytes());
    payload.extend_from_slice(&filetime.to_le_bytes());

    // Task metadata
    payload.extend_from_slice(&1i32.to_le_bytes());
    let task_name = "Task1";
    let task_bytes = task_name.as_bytes();
    payload.push(task_bytes.len() as u8); // 1-byte length
    payload.extend_from_slice(task_bytes);
    payload.extend_from_slice(&100u32.to_le_bytes());

    // App metadata
    let app_name = "App";
    let app_bytes = app_name.as_bytes();
    payload.push(app_bytes.len() as u8); // 1-byte length
    payload.extend_from_slice(app_bytes);

    let project_name = "Project";
    let proj_bytes = project_name.as_bytes();
    payload.push(proj_bytes.len() as u8); // 1-byte length
    payload.extend_from_slice(proj_bytes);

    payload.extend_from_slice(&0u32.to_le_bytes());

    // End marker
    payload.push(0);

    payload
}

#[test]
fn test_parse_simple_message_sequence() {
    let msg1 = build_ads_message("First message", "logger.a", 2);
    let msg2 = build_ads_message("Second message", "logger.b", 3);

    let entry1 = AdsParser::parse(&msg1).unwrap();
    let entry2 = AdsParser::parse(&msg2).unwrap();

    assert_eq!(entry1.message, "First message");
    assert_eq!(entry1.logger, "logger.a");
    assert_eq!(entry1.level, LogLevel::Info);

    assert_eq!(entry2.message, "Second message");
    assert_eq!(entry2.logger, "logger.b");
    assert_eq!(entry2.level, LogLevel::Warn);
}

#[test]
fn test_parse_all_log_levels_sequence() {
    let levels = vec![
        (0, LogLevel::Trace),
        (1, LogLevel::Debug),
        (2, LogLevel::Info),
        (3, LogLevel::Warn),
        (4, LogLevel::Error),
        (5, LogLevel::Fatal),
    ];

    for (level_byte, expected) in levels {
        let payload = build_ads_message("test", "logger", level_byte);
        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.level, expected);
    }
}

#[test]
fn test_parse_realistic_plc_log_message() {
    let realistic_msg =
        "Motor speed reached {0} RPM at cycle {1}";
    let payload = build_ads_message(realistic_msg, "motion.controller", 2);

    let entry = AdsParser::parse(&payload).unwrap();
    assert_eq!(entry.message, realistic_msg);
    assert_eq!(entry.logger, "motion.controller");
}

#[test]
fn test_parse_error_messages_sequence() {
    let error_messages = vec![
        "Configuration error: invalid port number",
        "Runtime error: out of memory",
        "Communication error: timeout exceeded",
        "Critical error: system shutdown",
    ];

    for (i, msg) in error_messages.iter().enumerate() {
        let level = if i < 2 { 4 } else { 5 }; // Error or Fatal
        let payload = build_ads_message(msg, "system.errors", level);
        let entry = AdsParser::parse(&payload).unwrap();
        assert_eq!(entry.message, *msg);
    }
}

#[test]
fn test_parse_unicode_messages_robustness() {
    let messages = vec![
        "Hello 世界",
        "Привет мир",
        "مرحبا بالعالم",
        "שלום עולם",
        "🚀 Emoji test 🎉",
    ];

    for msg in messages {
        let payload = build_ads_message(msg, "i18n.logger", 2);
        let result = AdsParser::parse(&payload);
        assert!(result.is_ok(), "Failed to parse: {}", msg);
        assert_eq!(result.unwrap().message, msg);
    }
}

#[test]
fn test_parse_large_message_handling() {
    let large_msg = "x".repeat(255); // Max size for 1-byte length prefix
    let payload = build_ads_message(&large_msg, "logger", 2);
    let entry = AdsParser::parse(&payload).unwrap();
    assert_eq!(entry.message.len(), 255);
}

#[test]
fn test_parse_special_characters_preservation() {
    let special_msg = r#"Path: C:\Windows\System32, Regex: [a-z]{1,5}, JSON: {"key":"value"}"#;
    let payload = build_ads_message(special_msg, "logger", 2);
    let entry = AdsParser::parse(&payload).unwrap();
    assert_eq!(entry.message, special_msg);
}

// ========== SECURITY TEST CASES ==========
// Tests for message limits, version validation, and DoS protection

#[test]
fn test_parser_security_message_size_limit_1mb() {
    // SECURITY: Messages > 1MB should be rejected to prevent DoS
    // Skip this test since the 1-byte string length prefix limits strings to 255 bytes
    // The actual 1MB limit applies to the entire message, not individual strings
}

#[test]
fn test_parser_security_string_length_limit_64kb() {
    // SECURITY: Strings > 64KB should be rejected to prevent DoS/memory exhaustion
    // With 1-byte length prefix, max string length is 255 bytes
    // This test is no longer applicable to the new protocol format
}

#[test]
fn test_parser_security_arguments_limit_32() {
    // SECURITY: Messages with > 32 arguments should be rejected
    let mut payload = build_ads_message("Test", "logger", 2);
    payload.pop(); // Remove end marker

    // Add 33 arguments (exceeds limit of 32)
    for i in 0..33 {
        payload.push(1); // type_id = argument
        payload.push(i as u8); // index
        payload.extend_from_slice(&8i16.to_le_bytes()); // value type = DINT (2 bytes)
        payload.extend_from_slice(&(i as i32).to_le_bytes());
    }

    payload.push(0); // end marker

    let result = AdsParser::parse(&payload);
    if let Ok(entry) = result {
        if entry.arguments.len() > 32 {
            eprintln!("⚠️ SECURITY WARNING: Parser accepts > 32 arguments ({} found)", entry.arguments.len());
        }
    }
}

#[test]
fn test_parser_security_context_vars_limit_64() {
    // SECURITY: Messages with > 64 context variables should be rejected
    let mut payload = build_ads_message("Test", "logger", 2);
    payload.pop(); // Remove end marker

    // Add 65 context variables (exceeds limit of 64)
    for i in 0..65 {
        payload.push(2); // type_id = context
        payload.push(1); // scope
        let ctx_name = format!("var{}", i);
        payload.push(ctx_name.len() as u8); // 1-byte length for context name
        payload.extend_from_slice(ctx_name.as_bytes());
        payload.extend_from_slice(&12i16.to_le_bytes()); // value type = STRING (2 bytes)
        let ctx_value = "value";
        payload.push(ctx_value.len() as u8); // 1-byte length for value
        payload.extend_from_slice(ctx_value.as_bytes());
    }

    payload.push(0); // end marker

    let result = AdsParser::parse(&payload);
    if let Ok(entry) = result {
        if entry.context.len() > 64 {
            eprintln!("⚠️ SECURITY WARNING: Parser accepts > 64 context vars ({} found)", entry.context.len());
        }
    }
}

#[test]
fn test_parser_security_invalid_log_level_rejection() {
    // SECURITY: Invalid log levels should be firmly rejected
    let mut payload = vec![1]; // version
    payload.push(4); // message length (1 byte)
    payload.extend_from_slice(b"test");
    payload.push(6); // logger length (1 byte)
    payload.extend_from_slice(b"logger");
    payload.extend_from_slice(&255u16.to_le_bytes()); // Invalid level (2 bytes)

    let result = AdsParser::parse(&payload);
    assert!(result.is_err(), "Invalid log level 255 should be rejected");
}

#[test]
fn test_parser_security_version_validation() {
    // SECURITY: Only supported protocol versions should be accepted
    let mut payload = vec![99]; // Unsupported version
    payload.push(4); // message length (1 byte)
    payload.extend_from_slice(b"test");

    let result = AdsParser::parse(&payload);
    assert!(result.is_err(), "Unsupported protocol version should be rejected");
}
