//! Stream parsing and thinking-section detection for model output.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThinkOpenKind {
    /// Opened by ```think — only ``` closes it (code fences inside thinking are preserved)
    CodeFenceThink,
    /// Opened by <think> or <!-- think --> — can close with any end marker including ```
    TagOrUnicode,
}

pub fn classify_think_start(line: &str) -> Option<ThinkOpenKind> {
    let trimmed = line.trim();
    if trimmed == "```think" {
        Some(ThinkOpenKind::CodeFenceThink)
    } else if trimmed.starts_with("<think>") || trimmed.starts_with("<!-- think -->") {
        Some(ThinkOpenKind::TagOrUnicode)
    } else {
        None
    }
}

pub fn is_think_end(line: &str, open_kind: ThinkOpenKind) -> bool {
    let trimmed = line.trim();
    // These end markers are unambiguous — always valid regardless of how thinking started
    let is_explicit_end = trimmed == "</think>" || trimmed == "<!-- /think -->";
    if is_explicit_end {
        return true;
    }
    // ``` is only a think-end if thinking was NOT opened by ```think.
    // When opened by ```think, a bare ``` inside the thinking section is
    // just a code fence and should not terminate thinking.
    match open_kind {
        ThinkOpenKind::TagOrUnicode => trimmed == "```",
        ThinkOpenKind::CodeFenceThink => false,
    }
}

// MAX_TOOL_ARGUMENT_SIZE moved to providers/mod.rs for shared use

/// Result of JSON truncation/repair operation.
pub struct TruncatedJson {
    /// The truncated/repaired JSON string.
    pub value: String,
    /// True if the original JSON was modified (not just truncated).
    pub was_repaired: bool,
}

/// Safely truncate a JSON string to fit within MAX_TOOL_ARGUMENT_SIZE bytes.
/// Ensures UTF-8 boundaries and JSON validity (closes open strings/objects).
/// Returns both the result and whether repair logic was needed.
///
/// **Known limitation:** When truncation lands inside a string value (e.g.,
/// `{"path": "/very/long/pa`), this function rewinds to the last complete
/// key-value pair rather than closing the truncated string. This preserves
/// semantic correctness (no lying data) at the cost of losing partial values.
/// The graceful fallback in `parse_tool_arguments` wraps unparseable JSON in
/// `{"_raw_arguments": "..."}` so no data is lost, just degraded.
pub fn truncate_json_arguments(args: &str, max_size: usize) -> TruncatedJson {
    if args.len() <= max_size {
        return TruncatedJson {
            value: args.to_string(),
            was_repaired: false,
        };
    }

    // First, truncate at a UTF-8 character boundary
    let end = args.floor_char_boundary(max_size);
    let truncated = &args[..end];

    // Count quotes to check if we're inside a string
    // Need to handle escaped quotes: \" doesn't count
    let mut in_string = false;
    let mut escape_next = false;
    for c in truncated.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if c == '\\' {
            escape_next = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
        }
    }

    // If we're inside a string, we need to close it
    if in_string {
        // Find the last quote before the truncation point and truncate there
        let mut last_quote = None;
        let mut escape_next = false;
        for (i, c) in truncated.char_indices() {
            if escape_next {
                escape_next = false;
                continue;
            }
            if c == '\\' {
                escape_next = true;
                continue;
            }
            if c == '"' {
                last_quote = Some(i);
            }
        }

        if let Some(pos) = last_quote {
            // Truncate to just before the last quote, then close the string and structures
            // The last_quote is the OPENING quote of a string value, so we need to add
            // an empty string: two quotes ("")
            let mut result = args[..args.floor_char_boundary(pos)].to_string();
            result.push_str("\"\"");
            // Close any open brackets/braces
            let mut brace_count = 0i32;
            let mut bracket_count = 0i32;
            let mut in_str = false;
            let mut escape_next = false;
            for c in result.chars() {
                if escape_next {
                    escape_next = false;
                    continue;
                }
                if c == '\\' {
                    escape_next = true;
                    continue;
                }
                if c == '"' {
                    in_str = !in_str;
                    continue;
                }
                if !in_str {
                    match c {
                        '{' => brace_count += 1,
                        '}' => brace_count -= 1,
                        '[' => bracket_count += 1,
                        ']' => bracket_count -= 1,
                        _ => {}
                    }
                }
            }
            while bracket_count > 0 {
                result.push(']');
                bracket_count -= 1;
            }
            while brace_count > 0 {
                result.push('}');
                brace_count -= 1;
            }
            return TruncatedJson {
                value: result,
                was_repaired: true,
            };
        } else {
            // No quotes found, return empty object
            return TruncatedJson {
                value: "{}".to_string(),
                was_repaired: true,
            };
        }
    }

    // Count open braces/brackets and close them if needed
    let mut brace_count = 0i32;
    let mut bracket_count = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    for c in truncated.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if c == '\\' {
            escape_next = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if !in_string {
            match c {
                '{' => brace_count += 1,
                '}' => brace_count -= 1,
                '[' => bracket_count += 1,
                ']' => bracket_count -= 1,
                _ => {}
            }
        }
    }

    let needs_closing = bracket_count > 0 || brace_count > 0;
    let mut result = truncated.to_string();
    // Close brackets first, then braces
    while bracket_count > 0 {
        result.push(']');
        bracket_count -= 1;
    }
    while brace_count > 0 {
        result.push('}');
        brace_count -= 1;
    }

    TruncatedJson {
        value: result,
        was_repaired: needs_closing,
    }
}

fn strip_common_indent(lines: &[&str]) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }

    let indent_counts: std::collections::HashMap<usize, usize> = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .fold(
            std::collections::HashMap::with_capacity(4),
            |mut acc, indent| {
                *acc.entry(indent).or_insert(0) += 1;
                acc
            },
        );

    if indent_counts.is_empty() {
        return lines.iter().map(|_| String::new()).collect();
    }

    let min_indent = *indent_counts
        .keys()
        .min()
        .expect("indent_counts checked non-empty but min() returned None");
    let dedent = if min_indent > 0 {
        min_indent
    } else {
        let (dominant_indent, dominant_count) = indent_counts
            .iter()
            .filter(|(indent, _)| **indent > 0)
            .max_by(|(indent_a, count_a), (indent_b, count_b)| {
                count_a.cmp(count_b).then(indent_a.cmp(indent_b))
            })
            .map(|(indent, count)| (*indent, *count))
            .unwrap_or((0, 0));

        let non_empty_count: usize = indent_counts.values().sum();
        let dominant_block_count = lines
            .iter()
            .filter(|line| {
                let indent = line.len() - line.trim_start().len();
                indent >= dominant_indent && !line.trim().is_empty()
            })
            .count();

        if dominant_indent >= 16
            && dominant_count >= 2
            && dominant_block_count * 2 >= non_empty_count
        {
            dominant_indent
        } else {
            0
        }
    };

    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else if line.len() >= dedent {
                line[dedent..].to_string()
            } else {
                line.to_string()
            }
        })
        .collect()
}

pub fn split_model_output(text: &str) -> (Option<String>, Option<String>) {
    let mut thinking: Option<String> = None;
    let mut response: Option<String> = None;
    let mut in_think = false;
    let mut think_open_kind: Option<ThinkOpenKind> = None;
    let mut think_lines: Vec<&str> = Vec::new();
    let mut response_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        if let Some(kind) = classify_think_start(line) {
            in_think = true;
            think_open_kind = Some(kind);
            think_lines.clear();
            continue;
        }
        if let Some(kind) = think_open_kind
            && is_think_end(line, kind)
        {
            in_think = false;
            think_open_kind = None;
            continue;
        }
        if in_think {
            think_lines.push(line);
        } else {
            response_lines.push(line);
        }
    }

    if !think_lines.is_empty() {
        let dedented = strip_common_indent(&think_lines);
        let t = dedented
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    l.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        thinking = if t.is_empty() { None } else { Some(t) };
    }

    if !response_lines.is_empty() {
        let dedented = strip_common_indent(&response_lines);
        let r = dedented
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    l.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        response = if r.is_empty() { None } else { Some(r) };
    }

    (thinking, response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_think_end_explicit_markers_always_work() {
        assert!(is_think_end("</think>", ThinkOpenKind::CodeFenceThink));
        assert!(is_think_end("</think>", ThinkOpenKind::TagOrUnicode));
        assert!(is_think_end(" </think> ", ThinkOpenKind::CodeFenceThink));

        assert!(is_think_end(
            "<!-- /think -->",
            ThinkOpenKind::CodeFenceThink
        ));
        assert!(is_think_end("<!-- /think -->", ThinkOpenKind::TagOrUnicode));
        assert!(is_think_end(
            " <!-- /think --> ",
            ThinkOpenKind::TagOrUnicode
        ));
    }

    #[test]
    fn test_is_think_end_code_fence_depends_on_open_kind() {
        assert!(!is_think_end("```", ThinkOpenKind::CodeFenceThink));
        assert!(is_think_end("```", ThinkOpenKind::TagOrUnicode));
    }

    #[test]
    fn test_is_think_end_not_end_marker() {
        assert!(!is_think_end("some text", ThinkOpenKind::TagOrUnicode));
        assert!(!is_think_end("some text", ThinkOpenKind::CodeFenceThink));
        assert!(!is_think_end("</think>ing", ThinkOpenKind::TagOrUnicode));
    }

    #[test]
    fn test_split_model_output_with_explicit_end_marker() {
        let input = "<think>\nThis is thinking\n</think>\nThis is response";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, Some("This is thinking".to_string()));
        assert_eq!(response, Some("This is response".to_string()));
    }

    #[test]
    fn test_split_model_output_with_comment_tags() {
        let input = "<!-- think -->\nThinking content\n<!-- /think -->\nResponse content";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, Some("Thinking content".to_string()));
        assert_eq!(response, Some("Response content".to_string()));
    }

    #[test]
    fn test_split_model_output_think_then_explicit_end() {
        let input = "<think>\nFirst thought\n</think>\nResponse with marker";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, Some("First thought".to_string()));
        assert_eq!(response, Some("Response with marker".to_string()));
    }

    #[test]
    fn test_truncate_json_arguments_utf8_boundary_safe() {
        let args = r#"{"content": "日本語テストファイルです"}"#;
        let result = truncate_json_arguments(args, 30);
        assert!(result.value.len() <= 30);
        assert!(result.was_repaired);
    }

    #[test]
    fn test_truncate_json_arguments_no_truncate_when_under_limit() {
        let args = r#"{"path": "/tmp/test"}"#;
        let result = truncate_json_arguments(args, 100);
        assert_eq!(result.value, args);
        assert!(!result.was_repaired);
    }

    #[test]
    fn test_truncate_json_arguments_closes_open_braces() {
        let args = r#"{"path": "/tmp/test", "content": "hel"#;
        let result = truncate_json_arguments(args, 20);
        assert!(result.was_repaired);
        assert!(result.value.contains("/tmp/test"));
    }

    #[test]
    fn test_truncate_json_arguments_closes_string_gracefully() {
        let args = r#"{"path": "/very/long/path/to/file.txt", "other": "value"}"#;
        let result = truncate_json_arguments(args, 25);
        assert!(result.value.len() <= 30);
        assert!(result.was_repaired);
        assert!(serde_json::from_str::<serde_json::Value>(&result.value).is_ok());
    }
}
