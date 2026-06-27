//! Text formatting utilities for terminal output.
//!
//! Provides word-wrapping and text formatting that respects terminal width.

use std::io::{self, IsTerminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Get the current terminal width, or a sensible default.
#[must_use] 
pub fn get_terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(80)
}

/// Check if stderr is a TTY.
#[must_use] 
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
#[must_use] 
pub fn draw_completion_box(title: &str, content: &str, width: usize) -> String {
    draw_box(title, content, width, "✓")
}

/// Draw a framed box around error text.
///
/// Creates a visual frame like:
/// ╭─ ✗ Error ─────────────────────────────────╮
/// │ Error message line 1                       │
/// │ Error message line 2                       │
/// ╰────────────────────────────────────────────╯
#[must_use] 
pub fn draw_error_box(title: &str, content: &str, width: usize) -> String {
    draw_box(title, content, width, "✗")
}

/// Truncate `text` so its display width does not exceed `max_width` cols.
/// Walks char boundaries so multibyte/CJK glyphs are never split.
fn truncate_to_width(text: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max_width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Each line must be exactly `width` cols so the right border sits
/// flush with the terminal edge. Wrapping at `inner_width - 1`
/// reserves the single space after `│`; padding fills the rest.
fn draw_box(title: &str, content: &str, width: usize, symbol: &str) -> String {
    // Below ~10 cols the borders and padding can no longer coexist,
    // so emit a plain fallback instead of a malformed box.
    if width < 10 {
        return format!("{symbol} {title}\n{content}\n");
    }

    let inner_width = width.saturating_sub(4);
    let indent = "  ";
    let content_width = inner_width.saturating_sub(1);
    let wrapped_content = wrap_text(content, content_width, "");

    // Truncate the title so the top border never exceeds the requested
    // width: CJK/emoji titles and narrow widths would otherwise overflow.
    let title_budget = width.saturating_sub(7);
    let truncated_title = truncate_to_width(title, title_budget);
    let title_dw = UnicodeWidthStr::width(truncated_title.as_str());

    let mut result = String::new();

    result.push_str(indent);
    result.push('╭');
    result.push('─');
    result.push(' ');
    result.push_str(&truncated_title);
    result.push(' ');

    let dash_count = width - title_dw - 7;
    for _ in 0..dash_count {
        result.push('─');
    }
    result.push('╮');
    result.push('\n');

    for line in wrapped_content.lines() {
        result.push_str(indent);
        result.push('│');
        result.push(' ');
        result.push_str(line);

        let padding_needed = content_width.saturating_sub(UnicodeWidthStr::width(line));
        for _ in 0..padding_needed {
            result.push(' ');
        }
        result.push('│');
        result.push('\n');
    }

    result.push_str(indent);
    result.push('╰');
    for _ in 0..inner_width {
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
#[must_use] 
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

    /// Every line of the box must equal the requested display width so the
    /// right border sits flush with the terminal edge.
    #[test]
    fn test_draw_completion_box_all_lines_exact_width() {
        for width in [40usize, 80, 100, 120] {
            let result = draw_completion_box("Done", "Short content.", width);
            for (i, line) in result.lines().enumerate() {
                let w = UnicodeWidthStr::width(line);
                assert_eq!(
                    w,
                    width,
                    "line {i} (display width={w}) != width={width}: |{line}|",
                );
            }
        }
    }

    /// Content that fills the inner width must not overflow the box.
    /// Regression: prior to this fix, wrap_text was called with the
    /// full inner width and the post-wrap padding saturated to zero,
    /// pushing the right border out by 3+ columns.
    #[test]
    fn test_draw_completion_box_content_fills_inner_width() {
        // inner_width = width - 4 = 36 for width=40
        let content_36 = "a".repeat(36);
        let result = draw_completion_box("T", &content_36, 40);
        for (i, line) in result.lines().enumerate() {
            let w = UnicodeWidthStr::width(line);
            assert_eq!(
                w,
                40,
                "line {i} overflowed: |{line}| (display width={w})",
            );
        }
    }

    /// Wrapping multi-line content must keep every row at exact width.
    #[test]
    fn test_draw_completion_box_wrapped_multiline_exact_width() {
        let content = "this is a fairly long message that should wrap to multiple lines when drawn inside the box at width 40";
        let result = draw_completion_box("Title", content, 40);
        for (i, line) in result.lines().enumerate() {
            let w = UnicodeWidthStr::width(line);
            assert_eq!(
                w,
                40,
                "line {i} wrong width: |{line}| (display width={w})",
            );
        }
    }

    /// CJK content has chars < display width; padding must use display width
    /// or the right border overflows.
    #[test]
    fn test_draw_completion_box_cjk_content_exact_width() {
        let result = draw_completion_box("T", "日本語", 40);
        for (i, line) in result.lines().enumerate() {
            let w = UnicodeWidthStr::width(line);
            assert_eq!(
                w,
                40,
                "line {i} overflowed: |{line}| (display width={w})",
            );
        }
    }

    /// CJK title pushes the top border past the requested width when title
    /// width is measured in chars; measuring in display cols keeps it aligned.
    #[test]
    fn test_draw_completion_box_cjk_title_exact_width() {
        let result = draw_completion_box("タスク", "body", 40);
        for (i, line) in result.lines().enumerate() {
            let w = UnicodeWidthStr::width(line);
            assert_eq!(
                w,
                40,
                "line {i} overflowed: |{line}| (display width={w})",
            );
        }
    }

    /// Same invariant as the completion-box test, but exercising draw_error_box
    /// to confirm both wrappers share the fixed helper path.
    #[test]
    fn test_draw_error_box_all_lines_exact_width() {
        for width in [40usize, 80, 100] {
            let result = draw_error_box("Error", "Bad things happened.", width);
            for (i, line) in result.lines().enumerate() {
                let w = UnicodeWidthStr::width(line);
                assert_eq!(
                    w,
                    width,
                    "line {i} (display width={w}) != width={width}: |{line}|",
                );
            }
        }
    }

    /// Narrow widths with CJK titles previously overflowed the top border;
    /// the title is now truncated so every line is exactly `width` cols.
    #[test]
    fn test_draw_completion_box_cjk_title_narrow_width() {
        for width in [10usize, 12, 15, 20] {
            let result = draw_completion_box("タスク", "body", width);
            for (i, line) in result.lines().enumerate() {
                let w = UnicodeWidthStr::width(line);
                assert_eq!(
                    w,
                    width,
                    "width={width} line {i} overflowed: |{line}| (display width={w})",
                );
            }
        }
    }

    /// Long ASCII title at narrow width must also stay inside the box.
    #[test]
    fn test_draw_completion_box_long_ascii_title_narrow_width() {
        for width in [10usize, 15, 20] {
            let result = draw_completion_box("Long Task Title", "body", width);
            for (i, line) in result.lines().enumerate() {
                let w = UnicodeWidthStr::width(line);
                assert_eq!(
                    w,
                    width,
                    "width={width} line {i} overflowed: |{line}| (display width={w})",
                );
            }
        }
    }

    /// Below the threshold, the fallback path is used and width does
    /// not apply. This guards the width-10 boundary.
    #[test]
    fn test_draw_completion_box_below_width_threshold() {
        let result = draw_completion_box("Done", "Hi", 5);
        assert!(result.contains("✓ Done"));
        assert!(result.contains("Hi"));
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
