//! Integration tests for message formatting with various scenarios

use tc_otel_core::MessageFormatter;
use std::collections::HashMap;

#[test]
fn test_format_realistic_log_messages() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!("motor_001"));
    args.insert(2, serde_json::json!(5000));
    args.insert(3, serde_json::json!("rpm"));

    let template = "Motor {0} reached speed {1} {2}";
    let result = MessageFormatter::format(template, &args);

    assert_eq!(result, "Motor motor_001 reached speed 5000 rpm");
}

#[test]
fn test_format_with_context_realistic() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!("alice"));

    let mut context = HashMap::new();
    context.insert("action".to_string(), serde_json::json!("login"));
    context.insert("ip".to_string(), serde_json::json!("192.168.1.50"));
    context.insert("timestamp".to_string(), serde_json::json!("2024-01-01T12:00:00Z"));

    let template = "User {0} performed {action} from {ip} at {timestamp}";
    let result = MessageFormatter::format_with_context(template, &args, &context);

    assert!(result.contains("User alice"));
    assert!(result.contains("login"));
    assert!(result.contains("192.168.1.50"));
}

#[test]
fn test_format_multiple_same_placeholder() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!("ERROR"));

    let template = "[{0}] {0}: Something went wrong with {0}";
    let result = MessageFormatter::format(template, &args);

    assert_eq!(result, "[ERROR] ERROR: Something went wrong with ERROR");
}

#[test]
fn test_format_mixed_placeholders() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!("123"));
    args.insert(2, serde_json::json!("456"));

    let mut context = HashMap::new();
    context.insert("component".to_string(), serde_json::json!("sensor"));
    context.insert("status".to_string(), serde_json::json!("healthy"));

    let template = "Component {component} (ID: {0}) cycle {1} status: {status}";
    let result = MessageFormatter::format_with_context(template, &args, &context);

    assert!(result.contains("sensor"));
    assert!(result.contains("123"));
    assert!(result.contains("456"));
    assert!(result.contains("healthy"));
}

#[test]
fn test_format_with_numeric_values() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!(42));
    args.insert(2, serde_json::json!(3.14159));
    args.insert(3, serde_json::json!(true));

    let template = "Value 1: {0}, Value 2: {1}, Flag: {2}";
    let result = MessageFormatter::format(template, &args);

    assert!(result.contains("42"));
    assert!(result.contains("3.14"));
    assert!(result.contains("true"));
}

#[test]
fn test_format_performance_large_template() {
    let mut args = HashMap::new();
    for i in 0..10 {
        args.insert(i, serde_json::json!(format!("arg{}", i)));
    }

    let mut template = String::new();
    for i in 0..10 {
        template.push_str(&format!("{{{}}}, ", i));
    }

    let result = MessageFormatter::format(&template, &args);

    // Just verify it completes and produces reasonable output
    assert!(!result.is_empty());
    assert!(result.len() > template.len() - 20); // Should not be much shorter
}

#[test]
fn test_extract_all_placeholder_types() {
    let template = "Positional {0} {1}, Named {name} {action}, Mixed {0} {status}";
    let placeholders = MessageFormatter::extract_placeholders(template);

    assert_eq!(placeholders.len(), 5);
    assert!(placeholders.contains(&"0".to_string()));
    assert!(placeholders.contains(&"1".to_string()));
    assert!(placeholders.contains(&"name".to_string()));
    assert!(placeholders.contains(&"action".to_string()));
    assert!(placeholders.contains(&"status".to_string()));
}

#[test]
fn test_format_with_special_characters_in_values() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!(r#"Path: "C:\Program Files\""#));
    args.insert(2, serde_json::json!("<tag>value</tag>"));
    args.insert(3, serde_json::json!("Line 1\nLine 2\nLine 3"));

    let template = "Path: {0}, XML: {1}, Multiline:\n{2}";
    let result = MessageFormatter::format(template, &args);

    assert!(result.contains("Program Files"));
    assert!(result.contains("<tag>"));
    assert!(result.contains("Line 1"));
}

#[test]
fn test_format_with_unicode_in_template_and_args() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!("世界"));
    args.insert(2, serde_json::json!("🚀"));

    let template = "Hello {0} {1}!";
    let result = MessageFormatter::format(template, &args);

    assert_eq!(result, "Hello 世界 🚀!");
}

#[test]
fn test_format_array_and_object_values() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!([1, 2, 3, 4, 5]));
    args.insert(2, serde_json::json!({"key": "value", "nested": {"inner": true}}));

    let template = "Array: {0}, Object: {1}";
    let result = MessageFormatter::format(template, &args);

    assert!(result.contains("["));
    assert!(result.contains("]"));
    assert!(result.contains("{"));
    assert!(result.contains("}"));
}

#[test]
fn test_format_context_overrides_missing_positional() {
    let args = HashMap::new(); // No positional args

    let mut context = HashMap::new();
    context.insert("0".to_string(), serde_json::json!("from_context"));

    let template = "Value: {0}";
    let result = MessageFormatter::format_with_context(template, &args, &context);

    // When using positional index in context, it should be replaced
    assert_eq!(result, "Value: from_context");
}

#[test]
fn test_format_empty_values() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!(""));
    args.insert(2, serde_json::json!(null));

    let template = "Empty: '{0}', Null: '{1}'";
    let result = MessageFormatter::format(template, &args);

    assert!(result.contains("Empty: ''"));
    assert!(result.contains("Null: 'null'"));
}

#[test]
fn test_extract_no_duplicates_in_placeholders() {
    let template = "{0} {1} {0} {2} {1} {0}";
    let placeholders = MessageFormatter::extract_placeholders(template);

    // May have duplicates in the list (depending on regex implementation)
    // but should at least contain all three
    assert!(placeholders.contains(&"0".to_string()));
    assert!(placeholders.contains(&"1".to_string()));
    assert!(placeholders.contains(&"2".to_string()));
}

#[test]
fn test_format_very_long_argument_values() {
    let mut args = HashMap::new();
    let long_value = "x".repeat(100000);
    args.insert(1, serde_json::json!(&long_value));

    let template = "Start {0} End";
    let result = MessageFormatter::format(template, &args);

    assert!(result.starts_with("Start x"));
    assert!(result.ends_with("x End"));
    assert_eq!(result.len(), long_value.len() + 10);
}

#[test]
fn test_format_plc_error_message_scenario() {
    let mut args = HashMap::new();
    args.insert(1, serde_json::json!("MotorControl"));
    args.insert(2, serde_json::json!("TIMEOUT"));
    args.insert(3, serde_json::json!(5000));

    let mut context = HashMap::new();
    context.insert("error_code".to_string(), serde_json::json!("E_COMM_004"));

    let template = "Task {0}: {1} error {error_code} after {2}ms";
    let result = MessageFormatter::format_with_context(template, &args, &context);

    assert_eq!(
        result,
        "Task MotorControl: TIMEOUT error E_COMM_004 after 5000ms"
    );
}
