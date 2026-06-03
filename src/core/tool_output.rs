//! Tool output formatting for the agent loop.
//!
//! This module handles formatting of tool results, summaries, heat maps,
//! and edit statistics for display to the user.

use crate::core::tools::SnedTool;

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
                return format!("  ▶ {}", tool_name);
            };

            let truncated = if cmd_text.len() > 120 {
                let end = cmd_text.floor_char_boundary(117);
                format!("{}...", &cmd_text[..end])
            } else {
                cmd_text
            };
            return format!("  ▶ {}", truncated);
        }
        Some(SnedTool::SearchFiles) => (
            "searched",
            params
                .get("paths")
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
        Some(SnedTool::ListFiles) => (
            "listed",
            params
                .get("paths")
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
        _ => return tool_name.to_string(),
    };
    let path_str = match path {
        Some(p) => p,
        None => return format!("  {}", verb),
    };
    let hyperlinked = crate::cli::colors::hyperlink_path(&path_str);
    format!("  ▶ {} {}", verb, hyperlinked)
}

pub fn path_from_read_file_header(text: &str) -> Option<&str> {
    let first_line = text.lines().next()?;
    if let Some(rest) = first_line.strip_prefix("[File: ") {
        rest.split(", Hash: ").next()
    } else {
        None
    }
}

/// Normalizes a path for comparison: extracts the last path component (filename)
/// to handle both absolute paths from read_file headers and relative paths from edit_file.
/// E.g. "/foo/bar/baz.rs" and "baz.rs" both normalize to "baz.rs"
pub fn normalize_path_for_matching(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

pub fn summarize_matching_sections(text: &str, edited_paths: &[String]) -> String {
    let sections: Vec<&str> = text.split("\n---\n").collect();
    let mut result = Vec::new();
    for section in &sections {
        let matches = path_from_read_file_header(section)
            .map(|p| {
                let normalized_p = normalize_path_for_matching(p);
                edited_paths
                    .iter()
                    .any(|ep| normalize_path_for_matching(ep) == normalized_p)
            })
            .unwrap_or(false);
        if matches {
            result.push(summarize_single_section(section));
        } else {
            result.push(section.to_string());
        }
    }
    result.join("\n---\n")
}

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

    let mut out = format!(
        "[Context pruned: {} lines, ~{}KB. Hash: {}]",
        line_count, size_kb, file_hash
    );

    if !anchored_lines.is_empty() {
        out.push_str("\nPreserved anchors (copy EXACTLY for edit_file):\n");
        out.push_str(&anchored_lines.join("\n"));
        out.push_str("\nRe-read with read_file for full content or to see lines beyond the preserved set.");
    } else {
        out.push_str(" Re-read with read_file if you need current anchors.");
    }

    out
}

const MAX_PRESERVED_ANCHORS: usize = 80;

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
        format!(
            "{} file(s) (+{}, -{})",
            files_changed, total_added, total_removed
        )
    } else {
        result.lines().next().unwrap_or("").to_string()
    };

    (stats, file_path, total_added, total_removed)
}

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
            format!("{} (+{}, -{})", hyperlinked, added, removed)
        })
        .collect();

    let more_str = if sorted.len() > 5 {
        format!("  ...and {} more", sorted.len() - 5)
    } else {
        String::new()
    };

    format!(
        "🔥 {} files: {}{}",
        sorted.len(),
        files_str.join("  "),
        more_str
    )
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

pub fn format_tool_result(result: &str, max_lines: usize) -> String {
    // Strip hash anchors (Word§line content) from display — they're agent-internal
    // for edit_file, not user-facing. The § delimiter separates the anchor word
    // from the actual file content.
    //
    // Single pass: strip anchors and count lines, stopping early once we know
    // truncation is needed. Only allocate the final output string.
    let mut output = String::new();
    let mut line_count = 0;

    for line in result.lines() {
        let stripped = strip_anchor(line);

        if line_count == max_lines {
            // We have enough lines; count remaining without building output.
            let remaining = result.lines().count() - max_lines;
            return format!("{output}\n... {remaining} more lines");
        }

        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(stripped);
        line_count += 1;
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
