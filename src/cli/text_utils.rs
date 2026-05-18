//! Text formatting utilities for terminal output.
//!
//! Provides word-wrapping and text formatting that respects terminal width.

use std::io::{self, IsTerminal};

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

    // If the line fits, output it as-is
    if trimmed.chars().count() <= available_width {
        output.push_str(indent);
        output.push_str(trimmed);
        output.push('\n');
        return;
    }

    // Need to wrap: break at word boundaries
    let mut remaining = trimmed;

    while !remaining.is_empty() {
        if remaining.chars().count() <= available_width {
            // Rest fits on one line
            output.push_str(indent);
            output.push_str(remaining);
            output.push('\n');
            break;
        }

        // Find the best break point (at a word boundary)
        let break_point = find_word_break(remaining, available_width);

        output.push_str(indent);
        output.push_str(&remaining[..break_point]);
        output.push('\n');

        remaining = remaining[break_point..].trim_start();
    }
}

/// Find the best position to break a line at a word boundary.
/// Returns the byte index where the line should be broken.
fn find_word_break(text: &str, max_width: usize) -> usize {
    let mut last_space = None;

    for (char_count, (i, c)) in text.char_indices().enumerate() {
        if char_count >= max_width {
            break;
        }
        if c == ' ' {
            last_space = Some(i);
        }
    }

    if let Some(space_pos) = last_space {
        return space_pos + 1;
    }

    // No space found — hard break at char boundary
    text.floor_char_boundary(max_width)
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
    fn test_find_word_break_cjk_boundary() {
        let text = "日本語test";
        let bp = find_word_break(text, 4);
        assert!(
            text.is_char_boundary(bp),
            "break point {} is not a char boundary in {:?}",
            bp,
            text
        );
        let _prefix = &text[..bp];
        let _suffix = &text[bp..];
    }
}
