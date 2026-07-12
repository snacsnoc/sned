//! Model-family-specific tool-call example strings.
//!
//! Examples are conceptual: they describe the tool name and argument
//! shape that the model should produce. sned receives tool calls via
//! the provider's native transport (OpenAI-compatible `tool_calls`
//! field), not as text brackets. The model should emit arguments
//! matching the tool's JSON schema field names exactly.

const QWEN_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS (Qwen)
- inspect/read: tool=read_file args={\"paths\": [\"src/main.rs\"]}
- search/find: tool=search_files args={\"regex\": \"fn handle_error\", \"path\": \"src\"}
- edit/write: tool=edit_file args={\"files\": [{\"path\": \"src/main.rs\", \"edits\": [{\"edit_type\": \"replace\", \"anchor\": \"Import§use std::io;\", \"text\": \"use std::io;\\nuse std::fs;\"}]}]}
- run/test: tool=execute_command args={\"commands\": [\"cargo test --no-fail-fast\"]}
- retry after tool failure: re-read the error, fix the failing argument (path, anchor, command), and call the same tool again with corrected JSON.
";

/// Returns tool-call examples for the active model family, or `None`
/// if no model-specific examples should be injected.
///
/// Currently: Qwen-family → `Some(examples)`; everything else → `None`.
/// Non-Qwen prompts remain byte-identical to pre-patch output.
#[must_use]
pub fn tool_examples_for_model(model_id: Option<&str>) -> Option<&'static str> {
    match model_id {
        Some(m) if super::model_detect::is_qwen_model(m) => Some(QWEN_EXAMPLES),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_examples_for_model_qwen() {
        let result = tool_examples_for_model(Some("qwen3.6-35b-a3b"));
        assert!(result.is_some());
        assert!(result.unwrap().contains("EXAMPLE TOOL CALLS (Qwen)"));
    }

    #[test]
    fn test_tool_examples_for_model_qwen_routed() {
        let result = tool_examples_for_model(Some("qwen/qwen3.5-27b"));
        assert!(result.is_some());
        assert!(result.unwrap().contains("EXAMPLE TOOL CALLS (Qwen)"));
    }

    #[test]
    fn test_tool_examples_for_model_generic_with_id() {
        assert!(tool_examples_for_model(Some("gpt-4o")).is_none());
        assert!(tool_examples_for_model(Some("claude-sonnet-4.5")).is_none());
    }

    #[test]
    fn test_tool_examples_for_model_none() {
        assert!(tool_examples_for_model(None).is_none());
    }
}
