//! Model-family detection for prompt and tool-call routing.
//!
//! Detection is provider-agnostic: works on the raw model id string
//! without assuming any specific vendor prefix shape.

/// Returns true when `model_id` matches the Qwen family.
///
/// Recognizes:
/// - bare ids: `qwen3.6-27b`, `qwen-max`, `qwq-preview`, `qwen2.5-coder-7b`
/// - routed ids: `Qwen/Qwen3.6-27B`, `vendor/qwen3-coder`
/// - hosted ids: `hosted-qwen3-coder-custom`
///
/// Qwen hosts routinely add routing prefixes and suffixes, so the requested
/// wire model is matched case-insensitively without assuming a provider path.
#[must_use]
pub fn is_qwen_model(model_id: &str) -> bool {
    if model_id.is_empty() {
        return false;
    }
    let lower = model_id.to_lowercase();
    if lower.contains("qwen") {
        return true;
    }
    for segment in lower.split(['/', '|']) {
        if segment_is_qwq(segment) || segment.split('-').any(segment_is_qwq) {
            return true;
        }
    }
    false
}

/// Returns true when a single `/`-separated path segment matches the
/// Qwen family pattern.
fn segment_is_qwq(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let name_len = if bytes.len() >= 3 && &bytes[0..3] == b"qwq" {
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
        assert!(is_qwen_model("qwen3.6-27b"));
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
        assert!(is_qwen_model("Qwen/Qwen3.6-27B"));
        assert!(is_qwen_model("vendor/qwen3-coder"));
        assert!(is_qwen_model("hosted-qwen3-coder-custom"));
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
        assert!(!is_qwen_model("qwerty"));
    }
}
