//! Text formatting utilities for terminal output.
//!
//! Provides word-wrapping and text formatting that respects terminal width.

use std::io::{self, IsTerminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Get the current terminal width, or a sensible default.
pub fn get_terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(80)
}

/// Check if stderr is a TTY.
pub fn stderr_is_tty() -> bool {
    io::stderr().is_terminal()
}

/// Draw a framed box around completion text.
///
/// Creates a visual frame like:
/// ╭─ Title ───────────────────────────────────╮
/// │ Content line 1                             │
/// │ Content line 2                             │
/// ╰────────────────────────────────────────────╯
pub fn draw_completion_box(title: &str, content: &str, width: usize) -> String {
    if width < 10 {
        return format!("{} {}\n{}\n", "✓", title, content);
    }

    let inner_width = width.saturating_sub(4); // Account for borders and padding
    let indent = "  ";

    // Wrap content to fit inside the box
    let wrapped_content = wrap_text(content, inner_width, "");

    // Build the box
    let mut result = String::new();

    // Top border: ╭─ Title ───────────────────╮
    result.push_str(indent);
    result.push('╭');
    result.push('─');
    result.push(' ');
    result.push_str(title);
    result.push(' ');

    let title_len = title.chars().count() + 2; // "+ " around title
    let dash_count = inner_width.saturating_sub(title_len + 1);
    for _ in 0..dash_count {
        result.push('─');
    }
    result.push('╮');
    result.push('\n');

    // Content lines: │ content                     │
    for line in wrapped_content.lines() {
        result.push_str(indent);
        result.push('│');
        result.push(' ');
        result.push_str(line);

        let padding_needed = inner_width.saturating_sub(line.chars().count() + 1);
        for _ in 0..padding_needed {
            result.push(' ');
        }
        result.push('│');
        result.push('\n');
    }

    // Bottom border: ╰────────────────────────────╯
    result.push_str(indent);
    result.push('╰');
    for _ in 0..(inner_width + 1) {
        result.push('─');
    }
    result.push('╯');
    result.push('\n');

    result
}

/// Draw a framed box around error text.
///
/// Creates a visual frame like:
/// ╭─ ✗ Error ─────────────────────────────────╮
/// │ Error message line 1                       │
/// │ Error message line 2                       │
/// ╰────────────────────────────────────────────╯
///
/// Falls back to plain text when width < 10.
pub fn draw_error_box(title: &str, content: &str, width: usize) -> String {
    if width < 10 {
        return format!("{} {}\n{}\n", "✗", title, content);
    }

    let inner_width = width.saturating_sub(4);
    let indent = "  ";

    let wrapped_content = wrap_text(content, inner_width, "");

    let mut result = String::new();

    // Top border: ╭─ ✗ Error ───────────────────╮
    result.push_str(indent);
    result.push('╭');
    result.push('─');
    result.push(' ');
    result.push_str(title);
    result.push(' ');

    let title_len = title.chars().count() + 2;
    let dash_count = inner_width.saturating_sub(title_len + 1);
    for _ in 0..dash_count {
        result.push('─');
    }
    result.push('╮');
    result.push('\n');

    // Content lines: │ content                     │
    for line in wrapped_content.lines() {
        result.push_str(indent);
        result.push('│');
        result.push(' ');
        result.push_str(line);

        let padding_needed = inner_width.saturating_sub(line.chars().count() + 1);
        for _ in 0..padding_needed {
            result.push(' ');
        }
        result.push('│');
        result.push('\n');
    }

    // Bottom border: ╰────────────────────────────╯
    result.push_str(indent);
    result.push('╰');
    for _ in 0..(inner_width + 1) {
        result.push('─');
    }
    result.push('╯');
    result.push('\n');

    result
}

/// Word-wrap text to fit within the specified width.
///
/// - Breaks lines at word boundaries when possible
/// - Preserves existing line breaks
/// - Preserves code blocks (text between ``` markers)
/// - Indents continuation lines with the specified indent
/// - Returns wrapped text with newlines
pub fn wrap_text(text: &str, width: usize, indent: &str) -> String {
    if width == 0 {
        return text.to_string();
    }

    let mut result = String::new();
    let mut in_code_block = false;

    for line in text.lines() {
        // Check for code block markers
        if line.trim().starts_with("```") {
            in_code_block = !in_code_block;
            result.push_str(indent);
            result.push_str(line);
            result.push('\n');
            continue;
        }

        if in_code_block {
            // Preserve code block content as-is
            result.push_str(indent);
            result.push_str(line);
            result.push('\n');
        } else {
            // Wrap prose text
            wrap_line(line, width, indent, &mut result);
        }
    }

    // Remove trailing newline if present
    if result.ends_with('\n') {
        result.pop();
    }

    result
}

/// Wrap a single line of text to fit within the specified width.
fn wrap_line(line: &str, width: usize, indent: &str, output: &mut String) {
    // Preserve leading whitespace for Markdown indentation
    // Only trim trailing whitespace
    let trimmed = line.trim_end();

    if trimmed.is_empty() {
        output.push('\n');
        return;
    }

    let indent_width = indent.len();
    let available_width = width.saturating_sub(indent_width);

    if available_width == 0 {
        output.push_str(indent);
        output.push_str(trimmed);
        output.push('\n');
        return;
    }

    // If the line fits, output it as-is.
    if UnicodeWidthStr::width(trimmed) <= available_width {
        output.push_str(indent);
        output.push_str(trimmed);
        output.push('\n');
        return;
    }

    // Need to wrap: scan once and emit each visual segment as soon as it
    // overflows. This avoids rescanning the remainder of long lines on every
    // wrap iteration.
    let mut segment_start = 0;
    let mut segment_width = 0usize;
    let mut last_space: Option<(usize, usize)> = None;

    fn push_segment(output: &mut String, indent: &str, segment: &str) {
        let segment = segment.trim_end();
        if segment.is_empty() {
            output.push('\n');
        } else {
            output.push_str(indent);
            output.push_str(segment);
            output.push('\n');
        }
    }

    for (byte_idx, ch) in trimmed.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let next_byte = byte_idx + ch.len_utf8();
        segment_width += ch_width;

        if ch == ' ' {
            last_space = Some((next_byte, segment_width));
        }

        if segment_width <= available_width {
            continue;
        }

        if let Some((space_byte, space_width)) = last_space.take()
            && space_byte > segment_start
        {
            push_segment(output, indent, &trimmed[segment_start..space_byte]);
            segment_start = space_byte;
            segment_width = segment_width.saturating_sub(space_width);
            continue;
        }

        if byte_idx > segment_start {
            push_segment(output, indent, &trimmed[segment_start..byte_idx]);
            segment_start = byte_idx;
            segment_width = ch_width;
            last_space = if ch == ' ' {
                Some((next_byte, ch_width))
            } else {
                None
            };
            continue;
        }

        // The first glyph itself is too wide for the available area. Emit it
        // alone so we still make forward progress.
        push_segment(output, indent, &trimmed[segment_start..next_byte]);
        segment_start = next_byte;
        segment_width = 0;
        last_space = None;
    }

    if segment_start < trimmed.len() {
        push_segment(output, indent, &trimmed[segment_start..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_short_line() {
        let result = wrap_text("Hello", 80, "  ");
        assert_eq!(result, "  Hello");
    }

    #[test]
    fn test_wrap_long_line() {
        let result = wrap_text("This is a very long line that should be wrapped", 20, "  ");
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].len() <= 20);
        assert!(lines[1].len() <= 20);
    }

    #[test]
    fn test_wrap_preserves_code_blocks() {
        let input = "Some text\n```\ncode block\nthat is long\n```\nMore text";
        let result = wrap_text(input, 20, "  ");
        assert!(result.contains("```\n  code block"));
    }

    #[test]
    fn test_wrap_preserves_blank_lines() {
        let input = "Line 1\n\nLine 3";
        let result = wrap_text(input, 80, "  ");
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].is_empty());
    }

    #[test]
    fn test_draw_completion_box() {
        let result = draw_completion_box("Task Completed", "Hello World", 40);
        assert!(result.contains("╭─"));
        assert!(result.contains("─╮"));
        assert!(result.contains("│"));
        assert!(result.contains("╰─"));
        assert!(result.contains("─╯"));
        assert!(result.contains("Task Completed"));
        assert!(result.contains("Hello World"));
    }

    #[test]
    fn test_wrap_cjk_no_panic() {
        let result = wrap_text("日本語テストファイルです日本語テスト", 4, "");
        assert!(!result.is_empty());
        for line in result.lines() {
            assert!(line.chars().count() <= 6, "line too wide: {:?}", line);
        }
    }

    #[test]
    fn test_wrap_cjk_line_stays_well_formed() {
        let result = wrap_text("日本語test", 4, "");
        let lines: Vec<&str> = result.lines().collect();
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|line| !line.is_empty()));
    }
}
