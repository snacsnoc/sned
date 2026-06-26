//! Render completion-box markdown as terminal-friendly `ratatui::text::Line`s.
//!
//! The completion text from `attempt_completion` may contain markdown tables,
//! fenced code blocks, inline code, lists, and bold text. This module
//! converts that markdown into a sequence of styled lines suitable for
//! the Task Completed box.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Render a completion result as terminal-friendly lines.
///
/// The first line is prefixed with `prefix` (e.g. "🚀 Task Completed: ").
/// Block-level markdown (tables, code blocks, lists, headings) is broken
/// into multiple `Line`s. Inline formatting (bold, italic, inline code)
/// is applied as `Span` styling.
#[must_use] 
pub fn render_completion_markdown(prefix: &str, text: &str) -> Vec<Line<'static>> {
    render_markdown(Some(prefix), text)
}

/// Render an error message as terminal-friendly lines with error styling.
///
/// The first line is prefixed with `prefix` (e.g. "✗ Error") styled in red.
/// The error text is rendered as plain styled lines (no markdown parsing).
/// Lines that exceed the terminal width are word-wrapped.
#[must_use] 
pub fn render_error_markdown(prefix: &str, text: &str) -> Vec<Line<'static>> {
    let wrap_width = crate::cli::text_utils::get_terminal_width();
    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix) + 2;
    let first_width = wrap_width.saturating_sub(prefix_width).max(1);
    let continuation_width = wrap_width.max(1);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut first = true;
    for raw_line in text.split('\n') {
        // Wrap is no-op for empty input, so route blank \n\n segments
        // through here explicitly to keep author-supplied vertical spacing.
        if raw_line.is_empty() {
            out.push(Line::from(""));
            // Do not consume `first` on leading blanks — the prefix must
            // attach to the first non-blank content line, not a blank line.
            continue;
        }

        // The first physical line of the message carries the prefix,
        // so its wrap budget is smaller than continuation lines.
        let width_budget = if first { first_width } else { continuation_width };
        let wrapped = crate::cli::text_utils::wrap_text(raw_line, width_budget, "");

        for (i, line) in wrapped.lines().enumerate() {
            if first && i == 0 {
                first = false;
                out.push(Line::from(vec![
                    Span::styled(
                        format!("{prefix}: "),
                        Style::default()
                            .fg(ratatui::style::Color::Red)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(line.to_string(), Style::default()),
                ]));
            } else {
                out.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default(),
                )));
            }
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            prefix.to_string(),
            Style::default()
                .fg(ratatui::style::Color::Red)
                .add_modifier(Modifier::BOLD),
        )));
    }
    out
}

/// Render arbitrary markdown as terminal-friendly lines.
///
/// If `prefix` is `Some`, the prefix is styled and prepended to the first
/// emitted line (used for the completion box's "🚀 Task Completed: "
/// banner). If `prefix` is `None`, no banner is applied — the lines
/// render as plain styled markdown, suitable for re-rendering streamed
/// agent output after a turn completes.
#[must_use] 
pub fn render_markdown(prefix: Option<&str>, text: &str) -> Vec<Line<'static>> {
    if text.trim().is_empty() {
        let banner = prefix.unwrap_or("");
        return vec![Line::from(Span::styled(
            format!("{banner}{text}"),
            Style::default(),
        ))];
    }

    let parser = Parser::new_ext(
        text,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_FOOTNOTES,
    );
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_text = String::new();

    // Inline style stack: each entry is a base modifier to apply to the next text.
    let mut style_stack: Vec<Style> = vec![Style::default()];
    // Are we currently inside a fenced code block?
    let mut in_code_block = false;
    // Are we currently inside a table?
    let mut in_table = false;
    // Pending list-item prefix to emit at the start of the next text run.
    let mut pending_list_prefix: Option<String> = None;

    fn flush_text(
        _out: &mut Vec<Line<'static>>,
        current_text: &mut String,
        current_spans: &mut Vec<Span<'static>>,
    ) {
        if !current_text.is_empty() {
            current_spans.push(Span::raw(std::mem::take(current_text)));
        }
    }

    fn flush_line(
        out: &mut Vec<Line<'static>>,
        current_text: &mut String,
        current_spans: &mut Vec<Span<'static>>,
        is_first: bool,
        prefix: Option<&str>,
    ) {
        flush_text(out, current_text, current_spans);
        if current_spans.is_empty() && !is_first {
            out.push(Line::from(""));
            return;
        }
        if is_first
            && let Some(p) = prefix
        {
            current_spans.insert(
                0,
                Span::styled(
                    p.to_string(),
                    Style::default().fg(crate::cli::tui::theme::PROMPT_FG),
                ),
            );
        }
        let spans = std::mem::take(current_spans);
        out.push(Line::from(spans));
    }

    let is_first_line = &mut true;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if !*is_first_line {
                        flush_text(&mut out, &mut current_text, &mut current_spans);
                        if !current_spans.is_empty() {
                            // Implicit blank line between block elements.
                        }
                    }
                }
                Tag::Heading { level, .. } => {
                    let prefix_marker = match level {
                        HeadingLevel::H1 => "# ",
                        HeadingLevel::H2 => "## ",
                        HeadingLevel::H3 => "### ",
                        HeadingLevel::H4 => "#### ",
                        HeadingLevel::H5 => "##### ",
                        HeadingLevel::H6 => "###### ",
                    };
                    pending_list_prefix = Some(prefix_marker.to_string());
                    style_stack.push(style_stack.last().unwrap().add_modifier(Modifier::BOLD));
                }
                Tag::Strong => {
                    style_stack.push(style_stack.last().unwrap().add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => {
                    style_stack.push(style_stack.last().unwrap().add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(_) => {
                    flush_line(
                        &mut out,
                        &mut current_text,
                        &mut current_spans,
                        *is_first_line,
                        prefix,
                    );
                    *is_first_line = false;
                    in_code_block = true;
                }
                Tag::Item => {
                    // Flush any accumulated spans from the previous item.
                    // Guard against flushing empty spans to avoid spurious
                    // blank lines between list items.
                    if !current_spans.is_empty() {
                        flush_line(
                            &mut out,
                            &mut current_text,
                            &mut current_spans,
                            *is_first_line,
                            prefix,
                        );
                    } else if *is_first_line && let Some(p) = prefix {
                        // If this is the very first item and we skipped the
                        // flush because spans were empty, still apply the prefix
                        // (e.g., "🚀 ") to the first line by inserting it now.
                        current_spans.push(Span::styled(
                            p.to_string(),
                            Style::default().fg(crate::cli::tui::theme::PROMPT_FG),
                        ));
                    }
                    *is_first_line = false;
                    pending_list_prefix = Some("• ".to_string());
                }
                Tag::Table(_) => {
                    in_table = true;
                }
                Tag::BlockQuote => {
                    pending_list_prefix = Some("│ ".to_string());
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    flush_line(
                        &mut out,
                        &mut current_text,
                        &mut current_spans,
                        *is_first_line,
                        prefix,
                    );
                    *is_first_line = false;
                }
                TagEnd::Heading(_) => {
                    flush_line(
                        &mut out,
                        &mut current_text,
                        &mut current_spans,
                        *is_first_line,
                        prefix,
                    );
                    *is_first_line = false;
                    style_stack.pop();
                    pending_list_prefix = None;
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    style_stack.pop();
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                }
                TagEnd::Item | TagEnd::BlockQuote => {
                    pending_list_prefix = None;
                }
                TagEnd::Table => {
                    in_table = false;
                }
                _ => {}
            },
            Event::Text(t) => {
                let piece = t.into_string();
                if in_code_block {
                    // Code block lines: emit each line as a dim, indented span.
                    flush_text(&mut out, &mut current_text, &mut current_spans);
                    for (i, line) in piece.split('\n').enumerate() {
                        if i > 0 {
                            flush_line(
                                &mut out,
                                &mut current_text,
                                &mut current_spans,
                                *is_first_line,
                                prefix,
                            );
                            *is_first_line = false;
                        }
                        if !current_spans.is_empty() {
                            // Continue building current line
                        }
                        let style = Style::default().add_modifier(Modifier::DIM);
                        current_spans.push(Span::styled(format!("    {line}"), style));
                    }
                } else if in_table {
                    // Strip pipe characters and alignment row markers.
                    let cleaned = piece.replace('|', "  ").trim().to_string();
                    // Skip separator rows like "--- | --- | ---"
                    if cleaned.chars().all(|c| c == '-' || c.is_whitespace())
                        && cleaned.contains('-')
                    {
                        continue;
                    }
                    if !cleaned.is_empty() {
                        if let Some(p) = pending_list_prefix.take() {
                            current_spans.push(Span::raw(p));
                        }
                        current_spans.push(Span::raw(cleaned));
                    }
                } else {
                    if let Some(p) = pending_list_prefix.take() {
                        current_spans.push(Span::raw(p));
                    }
                    let style = *style_stack.last().unwrap();
                    current_spans.push(Span::styled(piece, style));
                }
            }
            Event::Code(c) => {
                if in_code_block {
                    // Already handled by Text path
                } else {
                    let style = Style::default().fg(crate::cli::tui::theme::PROMPT_FG);
                    current_spans.push(Span::styled(format!("`{}`", c.into_string()), style));
                }
            }
            Event::SoftBreak => {
                current_spans.push(Span::raw(" "));
            }
            Event::HardBreak => {
                flush_line(
                    &mut out,
                    &mut current_text,
                    &mut current_spans,
                    *is_first_line,
                    prefix,
                );
                *is_first_line = false;
            }
            Event::Rule => {
                flush_line(
                    &mut out,
                    &mut current_text,
                    &mut current_spans,
                    *is_first_line,
                    prefix,
                );
                *is_first_line = false;
                out.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => {}
        }
    }

    // Flush any remaining content.
    if !current_text.is_empty() || !current_spans.is_empty() {
        flush_line(
            &mut out,
            &mut current_text,
            &mut current_spans,
            *is_first_line,
            prefix,
        );
    }

    if out.is_empty() {
        if let Some(p) = prefix {
            out.push(Line::from(Span::styled(
                p.to_string(),
                Style::default().fg(crate::cli::tui::theme::PROMPT_FG),
            )));
        } else {
            out.push(Line::from(""));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn empty_text_emits_prefix_only_line() {
        let lines = render_completion_markdown("🚀 Task Completed: ", "");
        assert_eq!(lines.len(), 1);
        assert!(collect_text(&lines).contains("🚀 Task Completed:"));
    }

    #[test]
    fn plain_text_appears_unchanged() {
        let lines = render_completion_markdown("🚀 Task Completed: ", "Created the file.");
        let text = collect_text(&lines);
        assert!(text.contains("Created the file."));
        assert!(text.contains("🚀 Task Completed:"));
    }

    #[test]
    fn bold_text_renders_with_bold_modifier() {
        let lines = render_completion_markdown("🚀 ", "**important** thing");
        let found = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.contains("important") && s.style.add_modifier.contains(Modifier::BOLD)
            })
        });
        assert!(found, "expected bold span, got: {:?}", lines);
    }

    #[test]
    fn inline_code_renders_with_prompt_fg() {
        let lines = render_completion_markdown("🚀 ", "Use `ls` to list files");
        let found = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.contains("`ls`") && s.style.fg == Some(crate::cli::tui::theme::PROMPT_FG)
            })
        });
        assert!(
            found,
            "expected inline code with PROMPT_FG, got: {:?}",
            lines
        );
    }

    #[test]
    fn fenced_code_block_renders_as_indented_dim_lines() {
        let md = "```\nlet x = 1;\nlet y = 2;\n```";
        let lines = render_completion_markdown("🚀 ", md);
        let text = collect_text(&lines);
        assert!(text.contains("let x = 1;"), "got: {}", text);
        assert!(text.contains("let y = 2;"), "got: {}", text);
        assert!(text.contains("    "), "expected indentation, got: {}", text);
    }

    #[test]
    fn markdown_table_renders_as_readable_rows_without_pipes() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let lines = render_completion_markdown("🚀 ", md);
        let text = collect_text(&lines);
        assert!(text.contains("a"), "got: {}", text);
        assert!(text.contains("b"), "got: {}", text);
        assert!(text.contains("1"), "got: {}", text);
        assert!(text.contains("3"), "got: {}", text);
        // Pipes should be stripped (replaced with two spaces, then trimmed).
        assert!(
            !text.contains('|'),
            "expected no pipe characters, got: {}",
            text
        );
        // Separator row should be dropped.
        assert!(
            !text.contains("---"),
            "expected separator row to be dropped, got: {}",
            text
        );
    }

    #[test]
    fn prefix_appears_only_on_first_line() {
        let md = "Line one.\n\nLine two.";
        let lines = render_completion_markdown("🚀 ", md);
        let prefix_count = lines
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains("🚀") && s.content.starts_with("🚀"))
            })
            .count();
        assert_eq!(
            prefix_count, 1,
            "prefix should appear once, got: {:?}",
            lines
        );
    }

    #[test]
    fn render_markdown_without_prefix_omits_banner() {
        // Used for re-rendering streamed agent text. The output must
        // not contain a banner — no "🚀 " prefix should be applied.
        //
        // Note: this test deliberately avoids list rendering. The
        // markdown module's list-item marker is not currently emitted
        // (Tag::Item is a no-op); exercising it here would couple
        // this fix to a pre-existing markdown-rendering gap.
        let md = "**bold** text and a heading.\n\nA second paragraph.";
        let lines = render_markdown(None, md);
        let text = collect_text(&lines);
        assert!(text.contains("bold"), "got: {}", text);
        assert!(text.contains("text and a heading"), "got: {}", text);
        assert!(text.contains("A second paragraph"), "got: {}", text);
        // No prefix in any line.
        for (i, line) in lines.iter().enumerate() {
            let joined: String = line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert!(
                !joined.contains("🚀"),
                "line {} unexpectedly contains the banner: {:?}",
                i,
                joined
            );
        }
    }

    #[test]
    fn render_markdown_empty_text_without_prefix_emits_blank_line() {
        let lines = render_markdown(None, "");
        // Either an empty line or a no-prefix placeholder — must not
        // contain any banner glyphs.
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!joined.contains("🚀"), "got: {}", joined);
    }

    #[test]
    fn list_items_render_on_separate_lines_with_bullet() {
        let md = "* one\n* two\n* three";
        let lines = render_markdown(None, md);
        assert_eq!(lines.len(), 3, "expected 3 lines, got: {:?}", lines);
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with("• "),
                "expected bullet prefix, got: {}",
                text
            );
        }
        assert!(collect_text(&lines).contains("one"));
        assert!(collect_text(&lines).contains("two"));
        assert!(collect_text(&lines).contains("three"));
    }

    #[test]
    fn list_items_with_completion_prefix() {
        let md = "* one\n* two\n* three";
        let lines = render_completion_markdown("🚀 ", md);
        // First line gets the "🚀 " prefix + "• " bullet
        // Remaining lines get only the "• " bullet
        assert!(lines.len() >= 3);
        // First line should have both prefixes
        let first_text: String =
            lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first_text.contains("🚀"));
        assert!(first_text.contains("• "));
        // Subsequent lines should have bullet but not the completion prefix
        for line in &lines[1..] {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with("• "),
                "expected bullet prefix on line, got: {}",
                text
            );
        }
    }

    #[test]
    fn nested_list_items_preserve_structure() {
        let md = "1. first\n2. second\n3. third";
        let lines = render_markdown(None, md);
        assert_eq!(lines.len(), 3, "expected 3 lines, got: {:?}", lines);
    }

    /// Regression: prior to this fix, render_error_markdown emitted each
    /// \n-split line verbatim, so a long error message overflowed the
    /// terminal instead of wrapping.
    #[test]
    fn render_error_markdown_wraps_long_error_text() {
        let long = "this is a very long error message that absolutely should be wrapped to fit the terminal width when rendered in the one-shot non-interactive output path";
        let lines = render_error_markdown("✗ Error", long);
        assert!(
            lines.len() > 1,
            "expected wrap into multiple lines, got {} line(s): {:?}",
            lines.len(),
            lines
        );
        // No emitted line should be wider than the terminal — wrapping
        // must have split the input before any row overflowed.
        let term_width = crate::cli::text_utils::get_terminal_width();
        for (i, line) in lines.iter().enumerate() {
            let width: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                width <= term_width,
                "line {i} overflowed terminal width {term_width}: width={width} content={:?}",
                line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
            );
        }
    }

    /// The first line must carry the red-bold prefix; continuation
    /// lines must not repeat it.
    #[test]
    fn render_error_markdown_prefix_only_on_first_line() {
        let long = "first part of error that fills more than a line of output so it must wrap second part";
        let lines = render_error_markdown("✗ Error", long);
        let first_text: String =
            lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first_text.starts_with("✗ Error"));
        for line in &lines[1..] {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                !text.starts_with("✗ Error"),
                "continuation line must not repeat the prefix: {text}"
            );
        }
    }

    /// Explicit blank lines in the error text must be preserved. Prior to this
    /// fix, `wrap_text("", ...)` produced no output, collapsing authored spacing.
    #[test]
    fn render_error_markdown_preserves_blank_lines() {
        let lines = render_error_markdown("✗ Error", "first\n\nthird");
        assert_eq!(lines.len(), 3, "expected 3 lines, got {:?}", lines);
        let first_text: String =
            lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first_text.contains("first"));
        let middle_text: String =
            lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            middle_text.is_empty(),
            "expected blank middle line, got: {middle_text:?}",
        );
        let last_text: String =
            lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(last_text.contains("third"));
    }
}
