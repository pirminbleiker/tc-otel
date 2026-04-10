//! Message template formatting and parsing

use std::collections::HashMap;

/// Message formatter for template-based message formatting
pub struct MessageFormatter;

impl MessageFormatter {
    pub fn format(template: &str, arguments: &HashMap<usize, serde_json::Value>) -> String {
        if !template.contains('{') {
            return template.to_string();
        }

        let empty_context = HashMap::new();
        Self::format_with_context(template, arguments, &empty_context)
    }

    /// Format a message with both positional and named arguments.
    /// Named placeholders like {time} are matched to arguments by order of appearance
    /// (Serilog/MessageTemplates style): first placeholder → arg[0], second → arg[1], etc.
    /// Numeric placeholders like {0}, {1} are matched by index directly.
    pub fn format_with_context(
        template: &str,
        arguments: &HashMap<usize, serde_json::Value>,
        context: &HashMap<String, serde_json::Value>,
    ) -> String {
        if !template.contains('{') {
            return template.to_string();
        }

        // Single-pass: scan for `{...}` and build result directly.
        // Track seen named placeholders so repeated occurrences reuse the same arg index.
        let mut result = String::with_capacity(template.len() + 64);
        let bytes = template.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        let mut positional_index: usize = 0;
        // Map named placeholder → assigned arg_index (small, stack-friendly)
        let mut seen_names: [(&str, usize); 16] = [("", 0); 16];
        let mut seen_count: usize = 0;

        while i < len {
            if bytes[i] == b'{' {
                // Find closing brace
                if let Some(end) = memchr_brace(bytes, i + 1) {
                    let key = &template[i + 1..end];
                    let mut replaced = false;

                    if let Ok(index) = key.parse::<usize>() {
                        // Numeric placeholder {0}, {1} → PLC args are 1-based
                        let arg_index = index + 1;
                        if let Some(value) = arguments.get(&arg_index) {
                            write_value(&mut result, value);
                            replaced = true;
                        }
                        // Only advance positional_index for first occurrence
                        if !seen_names[..seen_count].iter().any(|(n, _)| *n == key) && seen_count < 16 {
                            seen_names[seen_count] = (key, arg_index);
                            seen_count += 1;
                            positional_index += 1;
                        }
                    } else if !key.is_empty() {
                        // Named placeholder — check if we've seen it before
                        let arg_index = if let Some((_, idx)) = seen_names[..seen_count].iter().find(|(n, _)| *n == key) {
                            *idx
                        } else {
                            // First occurrence: assign next positional arg
                            let idx = positional_index + 1;
                            if seen_count < 16 {
                                seen_names[seen_count] = (key, idx);
                                seen_count += 1;
                            }
                            positional_index += 1;
                            idx
                        };

                        if let Some(value) = arguments.get(&arg_index) {
                            write_value(&mut result, value);
                            replaced = true;
                        } else if let Some(value) = context.get(key) {
                            write_value(&mut result, value);
                            replaced = true;
                        }
                    }

                    if replaced {
                        i = end + 1;
                    } else {
                        // No replacement found, keep original placeholder
                        result.push_str(&template[i..=end]);
                        i = end + 1;
                    }
                } else {
                    // No closing brace, copy literal
                    result.push('{');
                    i += 1;
                }
            } else {
                // Fast path: copy until next '{' or end
                let start = i;
                while i < len && bytes[i] != b'{' {
                    i += 1;
                }
                result.push_str(&template[start..i]);
            }
        }

        result
    }

    /// Extract placeholders from a template
    pub fn extract_placeholders(template: &str) -> Vec<String> {
        let bytes = template.as_bytes();
        let len = bytes.len();
        let mut placeholders = Vec::new();
        let mut i = 0;

        while i < len {
            if bytes[i] == b'{' {
                if let Some(end) = memchr_brace(bytes, i + 1) {
                    let key = &template[i + 1..end];
                    if !key.is_empty() {
                        placeholders.push(key.to_string());
                    }
                    i = end + 1;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }

        placeholders
    }

    /// Convert a JSON value to string representation
    fn value_to_string(value: &serde_json::Value) -> String {
        let mut s = String::with_capacity(32);
        write_value(&mut s, value);
        s
    }
}

/// Find closing '}' starting from position
#[inline]
fn memchr_brace(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Write JSON value directly to string buffer without intermediate allocation
#[inline]
fn write_value(buf: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => buf.push_str("null"),
        serde_json::Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => {
            use std::fmt::Write;
            let _ = write!(buf, "{}", n);
        }
        serde_json::Value::String(s) => buf.push_str(s),
        serde_json::Value::Array(arr) => {
            buf.push('[');
            for (i, v) in arr.iter().enumerate() {
                if i > 0 { buf.push_str(", "); }
                write_value(buf, v);
            }
            buf.push(']');
        }
        serde_json::Value::Object(obj) => {
            buf.push('{');
            for (i, (k, v)) in obj.iter().enumerate() {
                if i > 0 { buf.push_str(", "); }
                buf.push_str(k);
                buf.push('=');
                write_value(buf, v);
            }
            buf.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_positional_args() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!("world"));
        args.insert(1, serde_json::json!(42));

        let result = MessageFormatter::format("Hello {0}, answer is {1}", &args);
        assert_eq!(result, "Hello world, answer is 42");
    }

    #[test]
    fn test_format_with_context() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!("user123"));

        let mut context = HashMap::new();
        context.insert("action".to_string(), serde_json::json!("login"));

        let result = MessageFormatter::format_with_context(
            "User {0} performed {action}",
            &args,
            &context,
        );
        assert_eq!(result, "User user123 performed login");
    }

    #[test]
    fn test_extract_placeholders() {
        let template = "Hello {0}, action is {action}, count is {1}";
        let placeholders = MessageFormatter::extract_placeholders(template);

        assert_eq!(placeholders.len(), 3);
        assert!(placeholders.contains(&"0".to_string()));
        assert!(placeholders.contains(&"action".to_string()));
        assert!(placeholders.contains(&"1".to_string()));
    }

    #[test]
    fn test_value_formatting() {
        assert_eq!(MessageFormatter::value_to_string(&serde_json::json!(null)), "null");
        assert_eq!(MessageFormatter::value_to_string(&serde_json::json!(true)), "true");
        assert_eq!(MessageFormatter::value_to_string(&serde_json::json!(123)), "123");
        assert_eq!(MessageFormatter::value_to_string(&serde_json::json!("text")), "text");
    }

    #[test]
    fn test_format_partial_placeholders() {
        let args = HashMap::new();
        let result = MessageFormatter::format("Missing {0} here {1}", &args);
        assert_eq!(result, "Missing {0} here {1}");
    }

    #[test]
    fn test_format_no_args() {
        let args = HashMap::new();
        let result = MessageFormatter::format("Simple message without placeholders", &args);
        assert_eq!(result, "Simple message without placeholders");
    }

    #[test]
    fn test_format_extra_args() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!("used"));
        args.insert(1, serde_json::json!("unused"));
        args.insert(2, serde_json::json!("also_unused"));

        let result = MessageFormatter::format("Only {0} is used", &args);
        assert_eq!(result, "Only used is used");
    }

    #[test]
    fn test_format_out_of_order_indices() {
        let mut args = HashMap::new();
        args.insert(2, serde_json::json!("third"));
        args.insert(0, serde_json::json!("first"));
        args.insert(1, serde_json::json!("second"));

        let result = MessageFormatter::format("Order: {0}, {1}, {2}", &args);
        assert_eq!(result, "Order: first, second, third");
    }

    #[test]
    fn test_format_repeated_placeholders() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!("hello"));

        let result = MessageFormatter::format("{0} {0} {0}", &args);
        assert_eq!(result, "hello hello hello");
    }

    #[test]
    fn test_format_named_args_only() {
        let args = HashMap::new();
        let mut context = HashMap::new();
        context.insert("name".to_string(), serde_json::json!("Alice"));
        context.insert("action".to_string(), serde_json::json!("logged in"));

        let result = MessageFormatter::format_with_context(
            "{name} {action}",
            &args,
            &context,
        );
        assert_eq!(result, "Alice logged in");
    }

    #[test]
    fn test_format_arg_type_coercion() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!(42));
        args.insert(1, serde_json::json!(3.14));
        args.insert(2, serde_json::json!(true));
        args.insert(3, serde_json::json!(null));

        let result = MessageFormatter::format("Int: {0}, Float: {1}, Bool: {2}, Null: {3}", &args);
        assert_eq!(result, "Int: 42, Float: 3.14, Bool: true, Null: null");
    }

    #[test]
    fn test_format_special_chars_in_args() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!("line1\nline2"));
        args.insert(1, serde_json::json!("\"quoted\""));
        args.insert(2, serde_json::json!("path\\to\\file"));

        let result = MessageFormatter::format("Newline: {0}, Quote: {1}, Path: {2}", &args);
        assert!(result.contains("line1\nline2"));
        assert!(result.contains("\"quoted\""));
    }

    #[test]
    fn test_format_array_argument() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!([1, 2, 3]));

        let result = MessageFormatter::format("Array: {0}", &args);
        assert!(result.contains("["));
        assert!(result.contains("]"));
        assert!(result.contains("1"));
        assert!(result.contains("2"));
        assert!(result.contains("3"));
    }

    #[test]
    fn test_format_object_argument() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!({"key": "value"}));

        let result = MessageFormatter::format("Object: {0}", &args);
        assert!(result.contains("{"));
        assert!(result.contains("}"));
        assert!(result.contains("key"));
    }

    #[test]
    fn test_extract_mixed_placeholders() {
        let template = "User {0} performed {action} with {count}";
        let placeholders = MessageFormatter::extract_placeholders(template);

        assert_eq!(placeholders.len(), 3);
        assert!(placeholders.contains(&"0".to_string()));
        assert!(placeholders.contains(&"action".to_string()));
        assert!(placeholders.contains(&"count".to_string()));
    }

    #[test]
    fn test_extract_no_placeholders() {
        let template = "Just a plain message";
        let placeholders = MessageFormatter::extract_placeholders(template);

        assert_eq!(placeholders.len(), 0);
    }

    #[test]
    fn test_extract_numeric_placeholders() {
        let template = "{0} {1} {2} {3} {4}";
        let placeholders = MessageFormatter::extract_placeholders(template);

        assert_eq!(placeholders.len(), 5);
        for i in 0..5 {
            assert!(placeholders.contains(&i.to_string()));
        }
    }

    #[test]
    fn test_format_empty_template() {
        let args = HashMap::new();
        let result = MessageFormatter::format("", &args);
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_context_overrides_positional() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!("positional"));

        let mut context = HashMap::new();
        context.insert("name".to_string(), serde_json::json!("context_value"));

        let result = MessageFormatter::format_with_context(
            "Positional: {0}, Named: {name}",
            &args,
            &context,
        );
        assert!(result.contains("Positional: positional"));
        assert!(result.contains("Named: context_value"));
    }

    #[test]
    fn test_format_special_placeholder_chars() {
        let mut args = HashMap::new();
        args.insert(0, serde_json::json!("value"));

        let result = MessageFormatter::format("Placeholder: {0}", &args);
        assert_eq!(result, "Placeholder: value");
    }

    #[test]
    fn test_value_to_string_number_precision() {
        let large_number = serde_json::json!(1234567890.123456789);
        let result = MessageFormatter::value_to_string(&large_number);
        assert!(result.contains("1234567890"));
    }

    #[test]
    fn test_format_very_long_message() {
        let mut args = HashMap::new();
        let long_arg = "x".repeat(10000);
        args.insert(0, serde_json::json!(&long_arg));

        let template = "Start {0} End";
        let result = MessageFormatter::format(template, &args);
        assert!(result.starts_with("Start x"));
        assert!(result.ends_with("x End"));
        assert_eq!(result.len(), 10000 + "Start  End".len());
    }

    #[test]
    fn test_extract_placeholder_with_spaces() {
        let template = "Test { 0 } here";
        let placeholders = MessageFormatter::extract_placeholders(template);

        if !placeholders.is_empty() {
            assert!(placeholders[0].contains("0"));
        }
    }

    #[test]
    fn test_format_numeric_string_context() {
        let args = HashMap::new();
        let mut context = HashMap::new();
        context.insert("123".to_string(), serde_json::json!("numeric_key"));

        let result = MessageFormatter::format_with_context(
            "Value: {123}",
            &args,
            &context,
        );
        // Numeric placeholders like {123} are treated as positional arguments, not context keys
        assert_eq!(result, "Value: {123}");
    }

    #[test]
    fn test_format_with_empty_context_and_args() {
        let args = HashMap::new();
        let context = HashMap::new();

        let result = MessageFormatter::format_with_context(
            "No substitution here",
            &args,
            &context,
        );
        assert_eq!(result, "No substitution here");
    }
}
