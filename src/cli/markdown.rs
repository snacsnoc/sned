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
pub fn render_completion_markdown(prefix: &str, text: &str) -> Vec<Line<'static>> {
    if text.trim().is_empty() {
        return vec![Line::from(Span::styled(
            format!("{}{}", prefix, text),
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
        prefix: &str,
    ) {
        flush_text(out, current_text, current_spans);
        if current_spans.is_empty() && !is_first {
            out.push(Line::from(""));
            return;
        }
        if is_first {
            current_spans.insert(
                0,
                Span::styled(
                    prefix.to_string(),
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
                Tag::List(_) => {}
                Tag::Item => {}
                Tag::Table(_) => {
                    in_table = true;
                }
                Tag::TableHead => {}
                Tag::TableRow => {}
                Tag::TableCell => {}
                Tag::BlockQuote => {
                    pending_list_prefix = Some("│ ".to_string());
                }
                Tag::Link { .. } => {}
                Tag::Image { .. } => {}
                Tag::MetadataBlock(_) => {}
                Tag::HtmlBlock => {}
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
                TagEnd::Item => {
                    pending_list_prefix = None;
                }
                TagEnd::List(_) => {}
                TagEnd::Table => {
                    in_table = false;
                }
                TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell => {}
                TagEnd::BlockQuote => {
                    pending_list_prefix = None;
                }
                TagEnd::Link | TagEnd::Image => {}
                TagEnd::MetadataBlock(_) => {}
                TagEnd::HtmlBlock => {}
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
                        current_spans.push(Span::styled(format!("    {}", line), style));
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
        out.push(Line::from(Span::styled(
            prefix.to_string(),
            Style::default().fg(crate::cli::tui::theme::PROMPT_FG),
        )));
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
}
