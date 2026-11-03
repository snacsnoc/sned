//! Tool-call examples scoped to the active tool profile.
//!
//! Examples are conceptual: they describe the tool name and argument shape
//! that the model should produce. sned receives tool calls via the provider's
//! native transport, not as text brackets.

use crate::core::tools::SnedTool;
use crate::core::tools::definitions::ToolProfile;

const COMMON_TOOL_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS
- File workflow: inspect relevant context and make the requested change.
- inspect/read: tool=read_file args={\"paths\": [\"notes.txt\"]}
- search/find: tool=search_files args={\"regex\": \"TODO\", \"path\": \".\"}
- edit one block (preferred): tool=edit_file args={\"path\": \"settings.conf\", \"old_text\": \"mode=dev\\n\", \"new_text\": \"mode=prod\\n\"} (copy exact source without line numbers or anchor prefixes; old_text must match one block)
- batch or anchored edits: tool=edit_file args={\"files\": [{\"path\": \"settings.conf\", \"edits\": [{\"anchor\": \"Mode§mode=dev\", \"text\": \"mode=prod\"}]}]} (with files[], keep selectors such as anchor/edit_type/text inside edits[]; copy anchors from the current tracked state)
- create or overwrite a complete file: tool=write_to_file args={\"path\": \"notes.txt\", \"content\": \"...complete desired file contents...\"}
- inspect workspace location: tool=execute_command args={\"commands\": [\"pwd\"]} (commands is a literal JSON array, not a string containing an array)
- complex run-only logic: tool=execute_command args={\"script\": \"...\", \"language\": \"python\"}
- dependent shell statements: tool=execute_command args={\"commands\": [\"value='ready'\\nprintf '%s\\\\n' \\\"$value\\\"\"]} (each commands[] entry starts a fresh shell; keep assignment and use in one entry)
- quoted multiline shell text: tool=execute_command args={\"commands\": [\"cat <<'EOF'\\nquoted \\\"value\\\"\\nEOF\"]} (preserve the `\\n` escapes and quoted heredoc delimiter)
- Delete a range: tool=edit_file args={\"files\": [{\"path\": \"notes.txt\", \"edits\": [{\"anchor\": \"Start§obsolete setting\", \"end_anchor\": \"End§end obsolete setting\", \"text\": \"\"}]}]} (inclusive endpoints copied from your read).
- edit_file uses text for replacement source without anchor prefixes; ordinary range deletion does not need the optional content field.
- Use file tools for workspace changes. Use execute_command for inspection, builds, tests, and other execution; do not replace a file edit with shell redirection, a heredoc, or an ad-hoc Python/sed rewrite.
- retry after tool failure: read the error, correct the named argument, and call the same tool again. If the same error repeats after correcting it, stop retrying and report the error and what was attempted. Only stale or unknown anchor recovery requires read_file.
";

const WRITE_ONLY_TOOL_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS
- create or overwrite a complete file: tool=write_to_file args={\"path\": \"notes.txt\", \"content\": \"...complete desired file contents...\"}
- File tools are the workspace actions available in this turn. Report the result in text or call attempt_completion.
";

const CORE_EDIT_TOOL_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS
- File workflow: inspect relevant context and make the requested change.
- inspect/read: tool=read_file args={\"paths\": [\"notes.txt\"]}
- search/find: tool=search_files args={\"regex\": \"TODO\", \"path\": \".\"}
- edit one block (preferred): tool=edit_file args={\"path\": \"settings.conf\", \"old_text\": \"mode=dev\\n\", \"new_text\": \"mode=prod\\n\"} (copy exact source without line numbers or anchor prefixes; old_text must match one block)
- batch or anchored edits: tool=edit_file args={\"files\": [{\"path\": \"settings.conf\", \"edits\": [{\"anchor\": \"Mode§mode=dev\", \"text\": \"mode=prod\"}]}]} (with files[], keep selectors such as anchor/edit_type/text inside edits[]; copy anchors from the current tracked state)
- create or overwrite a complete file: tool=write_to_file args={\"path\": \"notes.txt\", \"content\": \"...complete desired file contents...\"}
- Delete a range: tool=edit_file args={\"files\": [{\"path\": \"notes.txt\", \"edits\": [{\"anchor\": \"Start§obsolete setting\", \"end_anchor\": \"End§end obsolete setting\", \"text\": \"\"}]}]} (inclusive endpoints copied from your read).
- edit_file uses text for replacement source without anchor prefixes; ordinary range deletion does not need the optional content field.
- retry after a stale or unknown edit anchor: call read_file again before retrying. Correct validation-only failures without rereading. If the same error repeats after correcting it, stop retrying and report the error and what was attempted.
";

const PLAN_TOOL_EXAMPLES: &str = "\
EXAMPLE TOOL CALLS
- inspect/read: tool=read_file args={\"paths\": [\"notes.txt\"]}
- search/find: tool=search_files args={\"regex\": \"TODO\", \"path\": \".\"}
- inspect workspace location: tool=execute_command args={\"commands\": [\"pwd\"]} (commands is a literal JSON array, not a string containing an array)
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
        ToolProfile::LeanAct | ToolProfile::Validate | ToolProfile::Full => {
            (has(SnedTool::ReadFile)
                && has(SnedTool::SearchFiles)
                && has(SnedTool::EditFile)
                && has(SnedTool::WriteToFile)
                && has(SnedTool::ExecuteCommand))
            .then_some(COMMON_TOOL_EXAMPLES)
        }
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
    fn test_examples_use_neutral_command_and_flat_edit_shapes() {
        for profile in [ToolProfile::LeanAct, ToolProfile::Plan] {
            let examples = tool_examples_for_model(None, Some(profile)).unwrap();
            assert!(examples.contains("\"commands\": [\"pwd\"]"));
            assert!(!examples.contains("cargo test"));
        }
        let act = tool_examples_for_model(None, Some(ToolProfile::LeanAct)).unwrap();
        assert!(act.contains("\"path\": \"settings.conf\", \"old_text\""));
    }

    #[test]
    fn test_edit_examples_use_current_state_anchors() {
        for profile in [
            ToolProfile::LeanAct,
            ToolProfile::CoreEdit,
            ToolProfile::Symbol,
            ToolProfile::Validate,
            ToolProfile::Full,
        ] {
            let examples = tool_examples_for_model(None, Some(profile)).unwrap();
            assert!(examples.contains("current tracked state"));
            assert!(
                examples.contains("keep selectors such as anchor/edit_type/text inside edits[]")
            );
            assert!(!examples.contains("Word§"));
            assert!(!examples.contains("immediately preceding"));
        }
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
