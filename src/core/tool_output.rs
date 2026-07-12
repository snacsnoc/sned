//! Tool output formatting for the agent loop.
//!
//! This module handles formatting of tool results, summaries, heat maps,
//! and edit statistics for display to the user.

use crate::core::tools::SnedTool;
use std::collections::HashSet;

pub fn format_tool_summary(tool_name: &str, params: &serde_json::Value) -> String {
    let tool = SnedTool::from_name(tool_name);
    let (verb, path) = match tool {
        Some(SnedTool::ReadFile) => (
            "read",
            params
                .get("paths")
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    params
                        .get("paths")
                        .and_then(|p| p.as_str())
                        .map(String::from)
                }),
        ),
        Some(SnedTool::WriteToFile) => (
            "created",
            params
                .get("path")
                .and_then(|p| p.as_str())
                .map(String::from),
        ),
        Some(SnedTool::EditFile) => (
            "edited",
            params
                .get("files")
                .and_then(|f| f.as_array())
                .and_then(|a| a.first())
                .and_then(|f| f.get("path"))
                .and_then(|p| p.as_str())
                .map(String::from),
        ),
        Some(SnedTool::ReplaceSymbol) => (
            "replaced",
            params
                .get("path")
                .and_then(|p| p.as_str())
                .map(String::from)
                .or_else(|| {
                    params
                        .get("replacements")
                        .and_then(|r| r.as_array())
                        .and_then(|a| a.first())
                        .and_then(|r| r.get("path"))
                        .and_then(|p| p.as_str())
                        .map(String::from)
                }),
        ),
        Some(SnedTool::RenameSymbol) => (
            "renamed",
            params
                .get("paths")
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
        Some(SnedTool::ExecuteCommand) => {
            // Handle all three parameter forms: "commands" (array), "command" (singular), "script"
            let cmd_text = if let Some(commands) = params.get("commands").and_then(|v| v.as_array())
            {
                // Primary form: array of commands, join with " && "
                let cmds: Vec<&str> = commands
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .collect();
                cmds.join(" && ")
            } else if let Some(cmd) = params.get("command").and_then(|v| v.as_str()) {
                // Legacy fallback: singular command string
                cmd.to_string()
            } else if let Some(script) = params.get("script").and_then(|v| v.as_str()) {
                // Alternative: script field
                if script.len() > 120 {
                    let end = script.floor_char_boundary(117);
                    format!("{}...", &script[..end])
                } else {
                    script.to_string()
                }
            } else {
                // No command found - avoid printing empty "▶ " line
                return format!("  ▶ {tool_name}");
            };

            let truncated = if cmd_text.len() > 120 {
                let end = cmd_text.floor_char_boundary(117);
                format!("{}...", &cmd_text[..end])
            } else {
                cmd_text
            };
            return format!("  ▶ {truncated}");
        }
        Some(SnedTool::SearchFiles) => (
            "searched",
            params
                .get("path")
                .and_then(|p| p.as_str())
                .map(String::from),
        ),
        Some(SnedTool::ListFiles) => (
            "listed",
            params
                .get("path")
                .and_then(|p| p.as_str())
                .map(String::from),
        ),
        _ => return tool_name.to_string(),
    };
    let Some(path_str) = path else {
        return format!("  {verb}");
    };
    let hyperlinked = crate::cli::colors::hyperlink_path(&path_str);
    format!("  ▶ {verb} {hyperlinked}")
}

#[must_use]
pub fn path_from_read_file_header(text: &str) -> Option<&str> {
    let first_line = text.lines().next()?;
    if let Some(rest) = first_line.strip_prefix("[File: ") {
        rest.split(", Hash: ").next()
    } else {
        None
    }
}

/// Preserves path components so files with duplicate basenames remain distinguishable.
#[must_use]
pub fn normalize_path_for_matching(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." if components.last().is_some_and(|last| *last != "..") => {
                components.pop();
            }
            _ => components.push(component),
        }
    }
    components.join("/")
}

fn path_components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|component| !component.is_empty())
        .collect()
}

fn unique_paths_with_basename(paths: &[String], basename: &str) -> usize {
    paths
        .iter()
        .map(|path| normalize_path_for_matching(path))
        .filter(|path| path.rsplit('/').next() == Some(basename))
        .collect::<HashSet<_>>()
        .len()
}

fn paths_match(
    read_path: &str,
    edited_path: &str,
    known_read_paths: &[String],
    edited_paths: &[String],
) -> bool {
    let read_path = normalize_path_for_matching(read_path);
    let edited_path = normalize_path_for_matching(edited_path);
    if read_path == edited_path {
        return true;
    }

    let read_components = path_components(&read_path);
    let edited_components = path_components(&edited_path);
    let qualified_suffix = (edited_components.len() > 1
        && read_components.ends_with(&edited_components))
        || (read_components.len() > 1 && edited_components.ends_with(&read_components));
    if qualified_suffix {
        return true;
    }

    let Some(read_basename) = read_components.last() else {
        return false;
    };
    if edited_components.last() != Some(read_basename) {
        return false;
    }

    unique_paths_with_basename(known_read_paths, read_basename) == 1
        && unique_paths_with_basename(edited_paths, read_basename) == 1
}

#[must_use]
pub fn summarize_matching_sections(
    text: &str,
    edited_paths: &[String],
    known_read_paths: &[String],
) -> String {
    let sections: Vec<&str> = text.split("\n---\n").collect();
    let mut result = Vec::new();
    for section in &sections {
        let matches = path_from_read_file_header(section).is_some_and(|read_path| {
            edited_paths.iter().any(|edited_path| {
                paths_match(read_path, edited_path, known_read_paths, edited_paths)
            })
        });
        if matches {
            result.push(summarize_single_section(section));
        } else {
            result.push(section.to_string());
        }
    }
    result.join("\n---\n")
}

#[must_use]
pub fn summarize_single_section(section: &str) -> String {
    let file_hash = section
        .lines()
        .next()
        .and_then(|l| {
            if let Some(rest) = l.strip_prefix("[File: ") {
                rest.split(", Hash: ")
                    .last()
                    .and_then(|h| h.strip_suffix(']'))
            } else if let Some(rest) = l.strip_prefix("[File Hash: ") {
                rest.strip_suffix(']')
            } else {
                None
            }
        })
        .unwrap_or("unknown");
    let line_count = section.lines().count().saturating_sub(1);
    let size_kb = section.len() / 1024;

    let anchored_lines: Vec<&str> = section
        .lines()
        .skip(1)
        .filter(|l| l.contains('§'))
        .take(MAX_PRESERVED_ANCHORS)
        .collect();

    let mut out = format!("[Context pruned: {line_count} lines, ~{size_kb}KB. Hash: {file_hash}]");

    if anchored_lines.is_empty() {
        out.push_str(" Re-read with read_file if you need current anchors.");
    } else {
        out.push_str("\nPreserved anchors (copy EXACTLY for edit_file):\n");
        out.push_str(&anchored_lines.join("\n"));
        out.push_str(
            "\nRe-read with read_file for full content or to see lines beyond the preserved set.",
        );
    }

    out
}

const MAX_PRESERVED_ANCHORS: usize = 80;

#[must_use]
pub fn extract_edit_stats_detailed(result: &str) -> (String, String, i32, i32) {
    let mut files_changed = 0;
    let mut total_added = 0;
    let mut total_removed = 0;
    let file_path = String::new();

    for line in result.lines() {
        if line.starts_with("Edited ")
            && line.contains("file(s):")
            && let Some(count_str) = line.split_whitespace().nth(1)
        {
            files_changed = count_str.parse().unwrap_or(0);
        }
        if line.contains("Applied ")
            && line.contains("edit(s) successfully")
            && let Some(stats_start) = line.find(" (+")
            && let Some(stats_end) = line.find(" lines)")
        {
            let stats = &line[stats_start + 2..stats_end];
            if let Some(comma_pos) = stats.find(", -") {
                let added: i32 = stats[..comma_pos].trim().parse().unwrap_or(0);
                let removed: i32 = stats[comma_pos + 3..].trim().parse().unwrap_or(0);
                total_added += added;
                total_removed += removed;
            }
        }
    }

    let stats = if files_changed > 0 {
        format!("{files_changed} file(s) (+{total_added}, -{total_removed})")
    } else {
        result.lines().next().unwrap_or("").to_string()
    };

    (stats, file_path, total_added, total_removed)
}

#[must_use]
pub fn format_heat_map(edit_files: &[(String, i32, i32)]) -> String {
    if edit_files.is_empty() {
        return String::new();
    }

    let mut sorted: Vec<_> = edit_files.iter().collect();
    sorted.sort_by(|a, b| {
        let total_a = a.1.abs() + a.2.abs();
        let total_b = b.1.abs() + b.2.abs();
        total_b.cmp(&total_a)
    });

    let display: Vec<_> = if sorted.len() <= 5 {
        sorted.iter().collect()
    } else {
        sorted.iter().take(5).collect()
    };

    let files_str: Vec<String> = display
        .iter()
        .map(|(path, added, removed)| {
            let hyperlinked = crate::cli::colors::hyperlink_path(path);
            format!("{hyperlinked} (+{added}, -{removed})")
        })
        .collect();

    let more_str = if sorted.len() > 5 {
        format!("  ...and {} more", sorted.len() - 5)
    } else {
        String::new()
    };

    let file_count_word = if sorted.len() == 1 { "file" } else { "files" };
    let count_prefix = format!("🔥 {} {}: ", sorted.len(), file_count_word);
    format!("{}{}{}", count_prefix, files_str.join("  "), more_str)
}

/// Strip the hash anchor prefix (Word§) from a single line.
/// Returns the line unchanged if it doesn't look like an anchored line.
fn strip_anchor(line: &str) -> &str {
    if let Some(idx) = line.find('§') {
        // Verify the prefix is a single-word anchor (no whitespace before §)
        let prefix = &line[..idx];
        if !prefix.is_empty() && !prefix.contains(char::is_whitespace) {
            return &line[idx + '§'.len_utf8()..];
        }
    }
    line
}

#[must_use]
pub fn format_tool_result(result: &str, max_lines: usize) -> String {
    // Strip hash anchors (Word§line content) from display — they're agent-internal
    // for edit_file, not user-facing. The § delimiter separates the anchor word
    // from the actual file content.
    //
    // Single pass: strip anchors and count lines, stopping early once we know
    // truncation is needed. Only allocate the final output string.
    let mut output = String::new();

    for (line_count, line) in result.lines().enumerate() {
        let stripped = strip_anchor(line);

        if line_count == max_lines {
            let remaining = result.lines().count() - max_lines;
            return format!("{output}\n... {remaining} more lines");
        }

        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(stripped);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_tool_summary_execute_command_singular() {
        let params = serde_json::json!({
            "command": "cargo test"
        });
        let summary = format_tool_summary("execute_command", &params);
        assert!(summary.contains("▶"));
        assert!(summary.contains("cargo test"));
    }

    #[test]
    fn test_format_tool_summary_execute_command_array() {
        let params = serde_json::json!({
            "commands": ["cd project", "cargo build", "cargo test"]
        });
        let summary = format_tool_summary("execute_command", &params);
        assert!(summary.contains("▶"));
        assert!(summary.contains("cd project && cargo build && cargo test"));
    }

    #[test]
    fn test_format_tool_summary_execute_command_script() {
        let params = serde_json::json!({
            "script": "for i in 1 2 3; do echo $i; done"
        });
        let summary = format_tool_summary("execute_command", &params);
        assert!(summary.contains("▶"));
        assert!(summary.contains("for i in 1 2 3; do echo $i; done"));
    }

    #[test]
    fn test_format_tool_summary_execute_command_empty_params() {
        let params = serde_json::json!({});
        let summary = format_tool_summary("execute_command", &params);
        // Should show tool name instead of empty "▶ " line
        assert!(summary.contains("▶"));
        assert!(summary.contains("execute_command"));
        assert!(!summary.ends_with("▶ "));
    }

    #[test]
    fn test_format_tool_summary_execute_command_truncation() {
        let long_cmd = "a".repeat(150);
        let params = serde_json::json!({
            "command": long_cmd
        });
        let summary = format_tool_summary("execute_command", &params);
        assert!(summary.contains("▶"));
        assert!(summary.contains("..."));
        assert!(summary.len() < 150);
    }

    #[test]
    fn test_format_tool_summary_read_file() {
        let params = serde_json::json!({
            "paths": ["src/main.rs"]
        });
        let summary = format_tool_summary("read_file", &params);
        assert!(summary.contains("▶"));
        assert!(summary.contains("read"));
        assert!(summary.contains("src/main.rs"));
    }

    #[test]
    fn test_format_tool_summary_edit_file() {
        let params = serde_json::json!({
            "files": [{"path": "src/lib.rs"}]
        });
        let summary = format_tool_summary("edit_file", &params);
        assert!(summary.contains("▶"));
        assert!(summary.contains("edited"));
        assert!(summary.contains("src/lib.rs"));
    }

    #[test]
    fn test_format_tool_summary_search_files_uses_path() {
        let params = serde_json::json!({
            "path": "src/core",
            "regex": "PlanState"
        });
        let summary = format_tool_summary("search_files", &params);
        assert!(summary.contains("searched"));
        assert!(summary.contains("src/core"));
    }

    #[test]
    fn test_format_tool_summary_list_files_uses_path() {
        let params = serde_json::json!({
            "path": "src/providers"
        });
        let summary = format_tool_summary("list_files", &params);
        assert!(summary.contains("listed"));
        assert!(summary.contains("src/providers"));
    }

    #[test]
    fn test_format_tool_summary_unknown_tool() {
        let params = serde_json::json!({});
        let summary = format_tool_summary("unknown_tool", &params);
        assert_eq!(summary, "unknown_tool");
    }

    #[test]
    fn test_strip_anchor_with_valid_prefix() {
        assert_eq!(strip_anchor("TranslucentMismatch§/*"), "/*");
        assert_eq!(strip_anchor("Apple§void main() {"), "void main() {");
    }

    #[test]
    fn test_strip_anchor_without_anchor() {
        assert_eq!(strip_anchor("just a line"), "just a line");
        assert_eq!(strip_anchor(""), "");
    }

    #[test]
    fn test_strip_anchor_preserves_mid_line_delimiter() {
        assert_eq!(strip_anchor("foo § bar"), "foo § bar");
    }

    #[test]
    fn test_strip_anchor_preserves_whitespace_prefix() {
        assert_eq!(strip_anchor("  Word§content"), "  Word§content");
    }

    #[test]
    fn test_format_tool_result_strips_anchors() {
        let result = "TranslucentMismatch§/*\nWarehouseSetter§ * Tetris clone";
        let formatted = format_tool_result(result, 10);
        assert_eq!(formatted, "/*\n * Tetris clone");
    }

    #[test]
    fn test_format_tool_result_no_truncation() {
        let result = "line one\nline two\nline three";
        let formatted = format_tool_result(result, 10);
        assert_eq!(formatted, "line one\nline two\nline three");
    }

    #[test]
    fn test_format_tool_result_with_truncation() {
        let result = "a\nb\nc\nd\ne\nf\ng\nh";
        let formatted = format_tool_result(result, 3);
        assert_eq!(formatted, "a\nb\nc\n... 5 more lines");
    }

    #[test]
    fn test_format_tool_result_empty() {
        assert_eq!(format_tool_result("", 10), "");
    }
}
