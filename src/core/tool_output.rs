//! Tool output formatting for the agent loop.
//!
//! This module handles formatting of tool results, summaries, heat maps,
//! and edit statistics for display to the user.

use crate::core::tools::SnedTool;
use std::collections::HashSet;

/// One styled line emitted as part of a tool-result digest in the TUI.
///
/// Used by [`format_tool_result_digest`] so the agent loop can render
/// per-line without re-parsing the summary string. The status line is
/// always emitted first and carries `fg = status_fg`; continuation lines
/// are rendered dim and indented two spaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestLine {
    pub text: String,
    pub fg: Option<ratatui::style::Color>,
    pub dim: bool,
}

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

/// Render a one-line digest (plus an optional dim continuation) that
/// summarizes a tool result for the TUI transcript.
///
/// This replaces the previous behaviour of dumping the first raw line of
/// the tool result body under the `✓` status glyph, which produced
/// confusing transcripts for `read_file` (showing `  ✓ .venv/` instead
/// of `  ✓ read .gitignore (12 lines)`) and for `execute_command`
/// (showing `  ✓ EXIT=1` because the user echoed `$?` in their shell).
///
/// Per-tool rules:
/// - `read_file`:       `  ✓ read <path> (<N> lines)` / `  ✗ read <path> (<N> lines)`
/// - `list_files`:      `  ✓ listed <path> (<N> entries)`
/// - `search_files`:    `  ✓ searched <path> (<N> matches)`
/// - `execute_command`: `  ✓ <command>` / `  ✗ <command>`
/// - everything else:   `  ✓ <first body line>` plus at most one dim
///   continuation. `format_tool_result` already appends its own
///   `... N more lines` marker, so we do not add a second one here.
///
/// `status_fg` is the theme colour for the status line; `dim_fg` is
/// the colour used for continuation lines.
#[must_use]
pub fn format_tool_result_digest(
    tool_name: &str,
    params: &serde_json::Value,
    result_text: &str,
    is_error: bool,
    status_fg: ratatui::style::Color,
    dim_fg: ratatui::style::Color,
) -> Vec<DigestLine> {
    let tool = SnedTool::from_name(tool_name);
    let status_glyph = if is_error { "✗" } else { "✓" };

    match tool {
        Some(SnedTool::ReadFile) => {
            let path = params
                .get("paths")
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .or_else(|| params.get("paths").and_then(|p| p.as_str()))
                .unwrap_or("?");
            let line_count = result_text.lines().count();
            vec![DigestLine {
                text: format!(
                    "  {status_glyph} read {path} ({line_count} {})",
                    if line_count == 1 { "line" } else { "lines" }
                ),
                fg: Some(status_fg),
                dim: false,
            }]
        }
        Some(SnedTool::ListFiles) => {
            let path = params
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let entry_count = count_non_blank_lines(result_text);
            let label = if is_error { "failed" } else { "listed" };
            vec![DigestLine {
                text: format!(
                    "  {status_glyph} {label} {path} ({entry_count} {})",
                    if entry_count == 1 { "entry" } else { "entries" }
                ),
                fg: Some(status_fg),
                dim: false,
            }]
        }
        Some(SnedTool::SearchFiles) => {
            let path = params
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let match_count = count_non_blank_lines(result_text);
            let label = if is_error { "failed" } else { "searched" };
            vec![DigestLine {
                text: format!(
                    "  {status_glyph} {label} {path} ({match_count} {})",
                    if match_count == 1 { "match" } else { "matches" }
                ),
                fg: Some(status_fg),
                dim: false,
            }]
        }
        Some(SnedTool::ExecuteCommand) => {
            let cmd_text = if let Some(commands) = params.get("commands").and_then(|v| v.as_array())
            {
                let cmds: Vec<&str> = commands
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .collect();
                cmds.join(" && ")
            } else if let Some(cmd) = params.get("command").and_then(|v| v.as_str()) {
                cmd.to_string()
            } else if let Some(script) = params.get("script").and_then(|v| v.as_str()) {
                script.to_string()
            } else {
                tool_name.to_string()
            };
            let truncated = if cmd_text.len() > 120 {
                let end = cmd_text.floor_char_boundary(117);
                format!("{}...", &cmd_text[..end])
            } else {
                cmd_text
            };
            vec![DigestLine {
                text: format!("  {status_glyph} {truncated}"),
                fg: Some(status_fg),
                dim: false,
            }]
        }
        _ => {
            // Generic fallback: first body line + at most one dim
            // continuation. `format_tool_result` already appends its own
            // `... N more lines` marker, so we do not emit a second one.
            let mut out = Vec::new();
            let mut display_lines = result_text.lines();
            let first = display_lines.next().unwrap_or("").trim_end();
            out.push(DigestLine {
                text: format!("  {status_glyph} {first}"),
                fg: Some(status_fg),
                dim: false,
            });
            if let Some(next) = display_lines.next() {
                let trimmed = next.trim_end();
                if !trimmed.is_empty() {
                    out.push(DigestLine {
                        text: format!("    {trimmed}"),
                        fg: Some(dim_fg),
                        dim: true,
                    });
                }
            }
            out
        }
    }
}

fn count_non_blank_lines(text: &str) -> usize {
    text.lines().filter(|line| !line.trim().is_empty()).count()
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
pub fn extract_edit_stats_detailed(result: &str) -> (String, i32, i32) {
    let mut files_changed = 0;
    let mut total_added = 0;
    let mut total_removed = 0;

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

    (stats, total_added, total_removed)
}

#[must_use]
pub fn format_heat_map(edit_files: &[(String, i32, i32)]) -> String {
    format_heat_map_with_paths(edit_files, crate::cli::colors::hyperlink_path)
}

#[must_use]
pub fn format_heat_map_plain(edit_files: &[(String, i32, i32)]) -> String {
    format_heat_map_with_paths(edit_files, str::to_string)
}

fn format_heat_map_with_paths(
    edit_files: &[(String, i32, i32)],
    format_path: impl Fn(&str) -> String,
) -> String {
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
            format!("{} (+{added}, -{removed})", format_path(path))
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
    fn test_format_heat_map_plain_has_no_terminal_escapes() {
        let heat_map = format_heat_map_plain(&[("src/lib.rs".to_string(), 3, 1)]);

        assert_eq!(heat_map, "🔥 1 file: src/lib.rs (+3, -1)");
        assert!(!heat_map.contains('\x1b'));
    }

    #[test]
    fn test_extract_edit_stats_detailed_returns_parsed_stats() {
        let result = "Edited 2 file(s):\nApplied 3 edit(s) successfully (+4, -2 lines)\nApplied 1 edit(s) successfully (+1, -3 lines)";

        assert_eq!(
            extract_edit_stats_detailed(result),
            ("2 file(s) (+5, -5)".to_string(), 5, 5)
        );
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

    fn digest_text(lines: &[DigestLine]) -> Vec<String> {
        lines.iter().map(|line| line.text.clone()).collect()
    }

    #[test]
    fn test_format_tool_result_digest_read_file_shows_path_and_line_count() {
        let params = serde_json::json!({
            "paths": [".gitignore"],
        });
        let body = ".venv/\n__pycache__/\n*.pyc";
        let lines = format_tool_result_digest(
            "read_file",
            &params,
            body,
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(digest_text(&lines), vec!["  ✓ read .gitignore (3 lines)"]);
    }

    #[test]
    fn test_format_tool_result_digest_read_file_error() {
        let params = serde_json::json!({
            "paths": ["missing.txt"],
        });
        let lines = format_tool_result_digest(
            "read_file",
            &params,
            "Error reading missing.txt: not found",
            true,
            ratatui::style::Color::Red,
            ratatui::style::Color::Gray,
        );
        assert_eq!(
            digest_text(&lines),
            vec!["  ✗ read missing.txt (1 line)"]
        );
    }

    #[test]
    fn test_format_tool_result_digest_execute_command_shows_command_text() {
        let params = serde_json::json!({
            "command": "rm -rf data/normalized && python scripts/rollup.py",
        });
        let body = "Sold listings file not found\nEXIT=1\n";
        let lines = format_tool_result_digest(
            "execute_command",
            &params,
            body,
            true,
            ratatui::style::Color::Red,
            ratatui::style::Color::Gray,
        );
        assert_eq!(
            digest_text(&lines),
            vec!["  ✗ rm -rf data/normalized && python scripts/rollup.py"]
        );
    }

    #[test]
    fn test_format_tool_result_digest_execute_command_truncates_long_command() {
        let long_cmd: String = "a".repeat(200);
        let params = serde_json::json!({"command": long_cmd.clone()});
        let lines = format_tool_result_digest(
            "execute_command",
            &params,
            "",
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.starts_with("  ✓ "));
        assert!(lines[0].text.ends_with("..."));
        // Status prefix is "  ✓ " (6 bytes for the 3-byte UTF-8 glyph);
        // truncated command body is up to 120 bytes (117 chars + "...").
        assert!(lines[0].text.len() <= 130);
        assert!(lines[0].text.len() < long_cmd.len());
    }

    #[test]
    fn test_format_tool_result_digest_list_files_shows_entry_count() {
        let params = serde_json::json!({"path": "src/core"});
        let body = "agent_loop.rs\ntools/\n\nmod.rs\n";
        let lines = format_tool_result_digest(
            "list_files",
            &params,
            body,
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(
            digest_text(&lines),
            vec!["  ✓ listed src/core (3 entries)"]
        );
    }

    #[test]
    fn test_format_tool_result_digest_search_files_shows_match_count() {
        let params = serde_json::json!({"path": "src", "regex": "PlanState"});
        let body = "src/foo.rs:1: PlanState::default()\nsrc/bar.rs:2: PlanState::new()\n";
        let lines = format_tool_result_digest(
            "search_files",
            &params,
            body,
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(
            digest_text(&lines),
            vec!["  ✓ searched src (2 matches)"]
        );
    }

    #[test]
    fn test_format_tool_result_digest_generic_first_line_plus_one_continuation() {
        let body = "first line\nsecond line\nthird line\nfourth line";
        let lines = format_tool_result_digest(
            "diagnostics_scan",
            &serde_json::json!({}),
            body,
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(
            digest_text(&lines),
            vec!["  ✓ first line".to_string(), "    second line".to_string()]
        );
    }

    #[test]
    fn test_format_tool_result_digest_generic_skips_blank_continuation() {
        let body = "first line\n   \nthird line";
        let lines = format_tool_result_digest(
            "diagnostics_scan",
            &serde_json::json!({}),
            body,
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(digest_text(&lines), vec!["  ✓ first line"]);
    }

    #[test]
    fn test_format_tool_result_digest_does_not_emit_more_lines_marker() {
        // The helper intentionally does not append `... N more lines`
        // because `format_tool_result` already does that when called
        // from the generic tool-result render path. Regression guard.
        let body = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj";
        let lines = format_tool_result_digest(
            "diagnostics_scan",
            &serde_json::json!({}),
            body,
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        for line in &lines {
            assert!(
                !line.text.contains("more lines"),
                "digest should not contain its own truncation marker: {line:?}"
            );
        }
    }

    #[test]
    fn test_format_tool_result_digest_pluralizes_counts() {
        // 1-line read uses singular "line", not "1 lines".
        let lines = format_tool_result_digest(
            "read_file",
            &serde_json::json!({"paths": [".env"]}),
            "SECRET=1",
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(digest_text(&lines), vec!["  ✓ read .env (1 line)"]);

        // 1-entry list_files uses singular "entry".
        let lines = format_tool_result_digest(
            "list_files",
            &serde_json::json!({"path": "."}),
            "only-one.rs",
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(digest_text(&lines), vec!["  ✓ listed . (1 entry)"]);

        // 1-match search_files uses singular "match".
        let lines = format_tool_result_digest(
            "search_files",
            &serde_json::json!({"path": "src"}),
            "src/foo.rs:1: PlanState",
            false,
            ratatui::style::Color::Green,
            ratatui::style::Color::Gray,
        );
        assert_eq!(digest_text(&lines), vec!["  ✓ searched src (1 match)"]);
    }
}
