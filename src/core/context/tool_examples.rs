//! Tool-call examples scoped to the active tool profile.
//!
//! Examples are conceptual: they describe the tool name and argument shape
//! that the model should produce. sned receives tool calls via the provider's
//! native transport, not as text brackets.

use crate::core::tools::SnedTool;
use crate::core::tools::definitions::ToolProfile;

const COMMON_TOOL_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS
- File workflow: inspect first with read_file, then make the smallest file change with the matching file tool, then re-read or run a focused check.
- inspect/read: tool=read_file args={\"paths\": [\"src/main.rs\"]}
- search/find: tool=search_files args={\"regex\": \"fn handle_error\", \"path\": \"src\"}
- edit existing: tool=edit_file args={\"files\": [{\"path\": \"src/main.rs\", \"edits\": [{\"edit_type\": \"replace\", \"anchor\": \"Import§use std::io;\", \"text\": \"use std::io;\\nuse std::fs;\"}]}]} (copy the exact full anchor returned by the immediately preceding read_file call; never invent the prefix)
- create or overwrite a complete file: tool=write_to_file args={\"path\": \"src/generated.rs\", \"content\": \"...complete desired file contents...\"}
- run/test: tool=execute_command args={\"commands\": [\"cargo test --no-fail-fast\"]} (commands is a literal JSON array, not a string containing an array)
- complex run-only logic: tool=execute_command args={\"script\": \"...\", \"language\": \"python\"}
- edit_file uses text for replacement text. Its optional content field is only an array of exact interior lines for a duplicate-anchor fingerprint; it is not the replacement string.
- Use file tools for workspace changes. Use execute_command for inspection, builds, tests, and other execution; do not replace a file edit with shell redirection, a heredoc, or an ad-hoc Python/sed rewrite.
- retry after tool failure: read the complete error, correct the named argument, and call the same tool again. For a stale or unknown edit anchor, call read_file again before retrying.
";

const WRITE_ONLY_TOOL_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS
- create or overwrite a complete file: tool=write_to_file args={\"path\": \"src/generated.rs\", \"content\": \"...complete desired file contents...\"}
- File tools are the workspace actions available in this turn. After writing, call attempt_completion with a concise result.
";

const CORE_EDIT_TOOL_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS
- File workflow: inspect first with read_file, then make the smallest file change with the matching file tool, then re-read or run a focused check.
- inspect/read: tool=read_file args={\"paths\": [\"src/main.rs\"]}
- search/find: tool=search_files args={\"regex\": \"fn handle_error\", \"path\": \"src\"}
- edit existing: tool=edit_file args={\"files\": [{\"path\": \"src/main.rs\", \"edits\": [{\"edit_type\": \"replace\", \"anchor\": \"Import§use std::io;\", \"text\": \"use std::io;\\nuse std::fs;\"}]}]} (copy the exact full anchor returned by the immediately preceding read_file call; never invent the prefix)
- create or overwrite a complete file: tool=write_to_file args={\"path\": \"src/generated.rs\", \"content\": \"...complete desired file contents...\"}
- edit_file uses text for replacement text. Its optional content field is only an array of exact interior lines for a duplicate-anchor fingerprint; it is not the replacement string.
- retry after a stale or unknown edit anchor: call read_file again before retrying.
";

const PLAN_TOOL_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS
- inspect/read: tool=read_file args={\"paths\": [\"src/main.rs\"]}
- search/find: tool=search_files args={\"regex\": \"fn handle_error\", \"path\": \"src\"}
- run/test: tool=execute_command args={\"commands\": [\"cargo test --no-fail-fast\"]} (commands is a literal JSON array, not a string containing an array)
- PLAN MODE is read-only: gather evidence and finish with plan_mode_respond; do not modify files.
";

/// Returns examples that only mention tools in the active profile.
#[must_use]
pub fn tool_examples_for_model(
    model_id: Option<&str>,
    profile: Option<ToolProfile>,
) -> Option<&'static str> {
    let _ = model_id;
    let profile = profile?;
    let has = |tool| profile.tools().contains(&tool);

    match profile {
        ToolProfile::DirectAnswer | ToolProfile::AnswerOnly => None,
        ToolProfile::WriteOnly => has(SnedTool::WriteToFile).then_some(WRITE_ONLY_TOOL_EXAMPLES),
        ToolProfile::CoreEdit | ToolProfile::Symbol => (has(SnedTool::ReadFile)
            && has(SnedTool::SearchFiles)
            && has(SnedTool::EditFile)
            && has(SnedTool::WriteToFile))
        .then_some(CORE_EDIT_TOOL_EXAMPLES),
        ToolProfile::Validate | ToolProfile::Full => (has(SnedTool::ReadFile)
            && has(SnedTool::SearchFiles)
            && has(SnedTool::EditFile)
            && has(SnedTool::WriteToFile)
            && has(SnedTool::ExecuteCommand))
        .then_some(COMMON_TOOL_EXAMPLES),
        ToolProfile::Plan => (has(SnedTool::ReadFile)
            && has(SnedTool::SearchFiles)
            && has(SnedTool::ExecuteCommand)
            && has(SnedTool::PlanModeRespond))
        .then_some(PLAN_TOOL_EXAMPLES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_examples_for_model_full_profile() {
        let result = tool_examples_for_model(Some("qwen3.6-35b-a3b"), Some(ToolProfile::Full));
        assert!(result.is_some());
        assert!(result.unwrap().contains("EXAMPLE TOOL CALLS"));
    }

    #[test]
    fn test_tool_examples_for_model_without_profile_are_omitted() {
        assert!(tool_examples_for_model(None, None).is_none());
    }

    #[test]
    fn test_tool_examples_never_name_unavailable_tools() {
        let direct = tool_examples_for_model(None, Some(ToolProfile::DirectAnswer));
        assert!(direct.is_none());

        let write_only = tool_examples_for_model(None, Some(ToolProfile::WriteOnly))
            .expect("write-only examples");
        assert!(write_only.contains("write_to_file"));
        assert!(!write_only.contains("read_file"));
        assert!(!write_only.contains("execute_command"));

        let validate =
            tool_examples_for_model(None, Some(ToolProfile::Validate)).expect("validate examples");
        assert!(!validate.contains("get_function"));
        assert!(!validate.contains("get_file_skeleton"));

        let plan = tool_examples_for_model(None, Some(ToolProfile::Plan)).expect("plan examples");
        assert!(plan.contains("plan_mode_respond"));
        assert!(!plan.contains("edit_file"));
        assert!(!plan.contains("write_to_file"));
    }
}
