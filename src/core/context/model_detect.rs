//! Model-family detection for prompt and tool-call routing.
//!
//! Detection is provider-agnostic: works on the raw model id string
//! without assuming any specific vendor prefix shape.

/// Returns true when `model_id` matches the Qwen family.
///
/// Recognizes:
/// - bare ids: `qwen3.6-35b-a3b`, `qwen-max`, `qwq-preview`, `qwen2.5-coder-7b`
/// - routed ids: `qwen/qwen3.6-35b-a3b`, `openrouter/qwen/qwen-max`
///
/// Match rule (case-insensitive): the id starts with `qwen-`,
/// `qwen<digit>`, `qwen<dot>`, or `qwq-`, OR a `/`-separated segment
/// does. The character immediately after the family name must be a
/// separator (`-`, digit, dot) or end-of-string. This avoids false
/// positives on non-Qwen ids that happen to share the `qwen` prefix
/// (e.g., hypothetical `qwentin`, `qwenxia`).
#[must_use]
pub fn is_qwen_model(model_id: &str) -> bool {
    if model_id.is_empty() {
        return false;
    }
    let lower = model_id.to_lowercase();
    for segment in lower.split('/') {
        if segment_is_qwen(segment) {
            return true;
        }
    }
    false
}

/// Returns true when a single `/`-separated path segment matches the
/// Qwen family pattern.
fn segment_is_qwen(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    // Determine family name length and check the prefix.
    let name_len = if bytes.len() >= 4 && &bytes[0..4] == b"qwen" {
        4
    } else if bytes.len() >= 3 && &bytes[0..3] == b"qwq" {
        3
    } else {
        return false;
    };
    // The character after the family name must be a separator or
    // end-of-string to avoid matching "qwentin", "qwenxia", etc.
    if name_len == bytes.len() {
        return true;
    }
    let after = bytes[name_len];
    after == b'-' || after.is_ascii_digit() || after == b'.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_qwen_model_bare_ids() {
        assert!(is_qwen_model("qwen3.6-35b-a3b"));
        assert!(is_qwen_model("qwen3.5-27b"));
        assert!(is_qwen_model("qwen-max"));
        assert!(is_qwen_model("qwen2.5-coder-7b"));
        assert!(is_qwen_model("qwq-preview"));
        assert!(is_qwen_model("qwen2-7b-instruct"));
        assert!(is_qwen_model("qwen-vl-max"));
    }

    #[test]
    fn test_is_qwen_model_routed_ids() {
        assert!(is_qwen_model("qwen/qwen3.6-35b-a3b"));
        assert!(is_qwen_model("openrouter/qwen/qwen3.6-35b-a3b"));
        assert!(is_qwen_model("qwen/qwen3.5-27b"));
        assert!(is_qwen_model("some/route/qwq-32b"));
    }

    #[test]
    fn test_is_qwen_model_case_insensitive() {
        assert!(is_qwen_model("QWEN3.6-35B-A3B"));
        assert!(is_qwen_model("Qwen/Max")); // vendor "qwen" routes a model called "max" — treated as Qwen family
    }

    #[test]
    fn test_is_qwen_model_negative() {
        assert!(!is_qwen_model("gpt-4o"));
        assert!(!is_qwen_model("claude-sonnet-4.5"));
        assert!(!is_qwen_model("deepseek-reasoner"));
        assert!(!is_qwen_model("minimax-M2.7"));
        assert!(!is_qwen_model("google/gemini-2.5-pro"));
        assert!(!is_qwen_model(""));
        // False-positive guards: names that share the "qwen" prefix
        // but are not Qwen-family models.
        assert!(!is_qwen_model("qwentin"));
        assert!(!is_qwen_model("qwenxia"));
        assert!(!is_qwen_model("qwerty"));
    }
}
