//! Render completion-box markdown as terminal-friendly `ratatui::text::Line`s.
//!
//! The completion text from `attempt_completion` may contain markdown tables,
//! fenced code blocks, inline code, lists, and bold text. This module
//! converts that markdown into a sequence of styled lines suitable for
//! the Task Completed box.

use lru::LruCache;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};

/// Render a completion result as terminal-friendly lines.
///
/// The first line is prefixed with `prefix` (e.g. "🚀 Task Completed: ").
/// Block-level markdown (tables, code blocks, lists, headings) is broken
/// into multiple `Line`s. Inline formatting (bold, italic, inline code)
/// is applied as `Span` styling.
#[must_use]
pub fn render_completion_markdown(prefix: &str, text: &str) -> Vec<Line<'static>> {
    render_markdown_with_code_limit(Some(prefix), text, None)
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
        let width_budget = if first {
            first_width
        } else {
            continuation_width
        };
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
                out.push(Line::from(Span::styled(line.to_string(), Style::default())));
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
    render_markdown_with_code_limit(prefix, text, None)
}

#[must_use]
pub fn render_streamed_markdown(text: &str, interactive_mode: bool) -> Vec<Line<'static>> {
    render_streamed_markdown_cached(text, interactive_mode, None)
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MarkdownRenderTiming {
    pub(crate) total_us: u64,
    pub(crate) syntax_highlight_us: u64,
}

const MARKDOWN_CACHE_ENTRIES: usize = 16;
const MARKDOWN_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MarkdownCacheKey {
    text: String,
    interactive_mode: bool,
    code_line_limit: Option<usize>,
    no_color: bool,
}

struct MarkdownCacheValue {
    rendered: Vec<Line<'static>>,
    resident_bytes: usize,
}

struct MarkdownCache {
    entries: LruCache<MarkdownCacheKey, MarkdownCacheValue>,
    resident_bytes: usize,
}

impl MarkdownCache {
    fn new() -> Self {
        Self {
            entries: LruCache::new(
                NonZeroUsize::new(MARKDOWN_CACHE_ENTRIES)
                    .expect("markdown cache capacity must be nonzero"),
            ),
            resident_bytes: 0,
        }
    }

    fn get(&mut self, key: &MarkdownCacheKey) -> Option<Vec<Line<'static>>> {
        self.entries.get(key).map(|value| value.rendered.clone())
    }

    fn insert(&mut self, key: MarkdownCacheKey, rendered: Vec<Line<'static>>) {
        let resident_bytes = key.text.len().saturating_add(
            rendered
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.len())
                        .sum::<usize>()
                })
                .sum::<usize>(),
        );
        if resident_bytes > MARKDOWN_CACHE_BYTES {
            return;
        }

        if let Some(previous) = self.entries.put(
            key,
            MarkdownCacheValue {
                rendered,
                resident_bytes,
            },
        ) {
            self.resident_bytes = self.resident_bytes.saturating_sub(previous.resident_bytes);
        }
        self.resident_bytes = self.resident_bytes.saturating_add(resident_bytes);

        while self.resident_bytes > MARKDOWN_CACHE_BYTES {
            let Some((_key, value)) = self.entries.pop_lru() else {
                break;
            };
            self.resident_bytes = self.resident_bytes.saturating_sub(value.resident_bytes);
        }
    }
}

fn markdown_cache() -> &'static Mutex<MarkdownCache> {
    static CACHE: OnceLock<Mutex<MarkdownCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(MarkdownCache::new()))
}

static MARKDOWN_CACHE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static MARKDOWN_CACHE_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn markdown_cache_stats() -> (u64, u64) {
    (
        MARKDOWN_CACHE_HITS.load(std::sync::atomic::Ordering::Relaxed),
        MARKDOWN_CACHE_MISSES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

fn render_streamed_markdown_cached(
    text: &str,
    interactive_mode: bool,
    mut syntax_highlight_us: Option<&mut u64>,
) -> Vec<Line<'static>> {
    let code_line_limit = Some(crate::core::agent_types::code_block_display_limit(
        interactive_mode,
    ));
    let key = MarkdownCacheKey {
        text: text.to_string(),
        interactive_mode,
        code_line_limit,
        no_color: std::env::var_os("NO_COLOR").is_some(),
    };
    if let Some(rendered) = markdown_cache()
        .lock()
        .expect("markdown cache poisoned")
        .get(&key)
    {
        MARKDOWN_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return rendered;
    }
    MARKDOWN_CACHE_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let rendered = render_markdown_with_code_limit_timed(
        None,
        text,
        code_line_limit,
        syntax_highlight_us.as_deref_mut(),
    );
    markdown_cache()
        .lock()
        .expect("markdown cache poisoned")
        .insert(key, rendered.clone());
    rendered
}

pub(crate) fn render_streamed_markdown_timed(
    text: &str,
    interactive_mode: bool,
) -> (Vec<Line<'static>>, MarkdownRenderTiming) {
    let started = std::time::Instant::now();
    let mut syntax_highlight_us = 0;
    let rendered =
        render_streamed_markdown_cached(text, interactive_mode, Some(&mut syntax_highlight_us));
    (
        rendered,
        MarkdownRenderTiming {
            total_us: started.elapsed().as_micros() as u64,
            syntax_highlight_us,
        },
    )
}

fn render_markdown_with_code_limit(
    prefix: Option<&str>,
    text: &str,
    code_line_limit: Option<usize>,
) -> Vec<Line<'static>> {
    render_markdown_with_code_limit_timed(prefix, text, code_line_limit, None)
}

fn render_markdown_with_code_limit_timed(
    prefix: Option<&str>,
    text: &str,
    code_line_limit: Option<usize>,
    mut syntax_highlight_us: Option<&mut u64>,
) -> Vec<Line<'static>> {
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
    let mut code_block_language = String::new();
    let mut code_block_text = String::new();
    let mut table_row_open = false;
    let mut table_cell_count = 0usize;
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
        if is_first && let Some(p) = prefix {
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
                Tag::Heading { level: _, .. } => {
                    if !current_text.is_empty() || !current_spans.is_empty() {
                        flush_line(
                            &mut out,
                            &mut current_text,
                            &mut current_spans,
                            *is_first_line,
                            prefix,
                        );
                        *is_first_line = false;
                    }
                    style_stack.push(style_stack.last().unwrap().add_modifier(Modifier::BOLD));
                }
                Tag::Strong => {
                    style_stack.push(style_stack.last().unwrap().add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => {
                    style_stack.push(style_stack.last().unwrap().add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    flush_line(
                        &mut out,
                        &mut current_text,
                        &mut current_spans,
                        *is_first_line,
                        prefix,
                    );
                    *is_first_line = false;
                    in_code_block = true;
                    code_block_language = match &kind {
                        CodeBlockKind::Fenced(language) => language.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    let label = match kind {
                        CodeBlockKind::Fenced(language) if !language.is_empty() => {
                            format!(" {language} ")
                        }
                        CodeBlockKind::Fenced(_) | CodeBlockKind::Indented => " code ".to_string(),
                    };
                    out.push(Line::from(Span::styled(
                        label,
                        Style::default()
                            .fg(crate::cli::tui::theme::PROMPT_FG)
                            .add_modifier(Modifier::BOLD)
                            .bg(crate::cli::tui::theme::BORDER_FG),
                    )));
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
                Tag::TableHead | Tag::TableRow => {
                    table_row_open = true;
                    table_cell_count = 0;
                }
                Tag::TableCell => {
                    if table_cell_count > 0 {
                        current_spans.push(Span::raw(" │ "));
                    }
                    table_cell_count += 1;
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
                    if let Some(limit) = code_line_limit {
                        let code = code_block_text
                            .strip_suffix('\n')
                            .unwrap_or(&code_block_text);
                        let code_lines: Vec<&str> = if code.is_empty() {
                            Vec::new()
                        } else {
                            code.split('\n').collect()
                        };
                        let displayed = code_lines
                            .iter()
                            .take(limit)
                            .copied()
                            .collect::<Vec<_>>()
                            .join("\n");
                        let highlight_started = std::time::Instant::now();
                        let highlighted = crate::cli::syntax_highlight::highlight_code(
                            &displayed,
                            &code_block_language,
                        );
                        if let Some(total) = syntax_highlight_us.as_deref_mut() {
                            *total = total
                                .saturating_add(highlight_started.elapsed().as_micros() as u64);
                        }
                        for line in
                            crate::cli::tui::ansi_converter::ansi_to_ratatui_lines(&highlighted)
                        {
                            let mut spans = Vec::with_capacity(line.spans.len() + 1);
                            spans.push(Span::styled(
                                "│   ",
                                Style::default().add_modifier(Modifier::DIM),
                            ));
                            spans.extend(line.spans);
                            out.push(Line::from(spans));
                        }
                        if code_lines.len() > limit {
                            out.push(Line::from(Span::styled(
                                "│   ... [snipped from streamed display; use /full]",
                                Style::default().add_modifier(Modifier::DIM),
                            )));
                        }
                        code_block_text.clear();
                        code_block_language.clear();
                    }
                    in_code_block = false;
                    out.push(Line::from(Span::styled(
                        "─".repeat(60),
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                }
                TagEnd::Item => {
                    if !current_text.is_empty() || !current_spans.is_empty() {
                        flush_line(
                            &mut out,
                            &mut current_text,
                            &mut current_spans,
                            *is_first_line,
                            prefix,
                        );
                        *is_first_line = false;
                    }
                    pending_list_prefix = None;
                }
                TagEnd::BlockQuote => {
                    pending_list_prefix = None;
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if table_row_open {
                        flush_line(
                            &mut out,
                            &mut current_text,
                            &mut current_spans,
                            *is_first_line,
                            prefix,
                        );
                        *is_first_line = false;
                        table_row_open = false;
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                let piece = t.into_string();
                if in_code_block {
                    if code_line_limit.is_some() {
                        code_block_text.push_str(&piece);
                    } else {
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
                            let style = Style::default().add_modifier(Modifier::DIM);
                            current_spans.push(Span::styled("│   ", style));
                            current_spans.push(Span::styled(line.to_string(), style));
                        }
                    }
                } else {
                    if let Some(p) = pending_list_prefix.take() {
                        current_spans.push(Span::raw(p));
                    }
                    let style = *style_stack.last().unwrap();
                    let mut parts = piece.split('\n');
                    if let Some(first) = parts.next()
                        && !first.is_empty()
                    {
                        current_spans.push(Span::styled(first.to_string(), style));
                    }
                    for part in parts {
                        flush_line(
                            &mut out,
                            &mut current_text,
                            &mut current_spans,
                            *is_first_line,
                            prefix,
                        );
                        *is_first_line = false;
                        if !part.is_empty() {
                            current_spans.push(Span::styled(part.to_string(), style));
                        }
                    }
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
                flush_line(
                    &mut out,
                    &mut current_text,
                    &mut current_spans,
                    *is_first_line,
                    prefix,
                );
                *is_first_line = false;
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
                    "─ ◇ ─",
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
    fn repeated_streamed_markdown_render_uses_equivalent_cached_lines() {
        let text = "cache-regression-unique\n\n**styled**\n\n```rust\nlet cache_value = 42;\n```";
        let (first, _) = render_streamed_markdown_timed(text, true);
        let (second, second_timing) = render_streamed_markdown_timed(text, true);

        assert_eq!(first, second);
        assert_eq!(second_timing.syntax_highlight_us, 0);
    }

    #[test]
    fn plain_text_newlines_render_as_separate_rows() {
        let lines = render_completion_markdown("🚀 ", "first\nsecond\nthird");
        let text: Vec<_> = lines.iter().map(Line::to_string).collect();

        assert_eq!(text.len(), 3);
        assert!(text[0].contains("first"));
        assert!(text[1].contains("second"));
        assert!(text[2].contains("third"));
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
    fn fenced_code_block_renders_as_labeled_dim_container() {
        let md = "```\nlet x = 1;\nlet y = 2;\n```";
        let lines = render_completion_markdown("🚀 ", md);
        let text = collect_text(&lines);
        assert!(text.contains("let x = 1;"), "got: {}", text);
        assert!(text.contains("let y = 2;"), "got: {}", text);
        assert!(
            text.contains(" code "),
            "expected code label, got: {}",
            text
        );
        assert!(text.contains("│   "), "expected code border, got: {}", text);
        assert!(
            text.contains(&"─".repeat(60)),
            "expected bottom border, got: {}",
            text
        );
    }

    #[test]
    fn fenced_code_block_uses_language_label() {
        let lines = render_completion_markdown("🚀 ", "```rust\nlet x = 1;\n```");
        assert!(collect_text(&lines).contains(" rust "));
    }

    #[test]
    fn streamed_fenced_code_is_capped_without_literal_fences() {
        let mut markdown = String::from("```rust\n");
        for line in 1..=61 {
            markdown.push_str(&format!("fn line_{line}() {{}}\n"));
        }
        markdown.push_str("```");

        let rendered = render_streamed_markdown(&markdown, true);
        let text = collect_text(&rendered);

        assert!(text.contains(" rust "));
        assert!(text.contains("fn line_60()"));
        assert!(!text.contains("fn line_61()"));
        assert!(text.contains("[snipped from streamed display; use /full]"));
        assert!(!text.contains("```"));
    }

    #[test]
    fn headings_are_bold_standalone_lines_without_markers() {
        let lines = render_completion_markdown("", "before\n\n## heading");
        let heading_index = lines
            .iter()
            .position(|line| line.spans.iter().any(|span| span.content == "heading"))
            .expect("heading line");
        assert!(
            heading_index > 0,
            "heading must follow the paragraph: {lines:?}"
        );
        assert!(
            lines[heading_index]
                .spans
                .iter()
                .any(|span| span.content == "heading"
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(!collect_text(&lines).contains("## heading"));
    }

    #[test]
    fn markdown_table_renders_as_separate_rows_with_cell_separators() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let lines = render_completion_markdown("🚀 ", md);
        let rows = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rows, ["🚀 a │ b", "1 │ 2", "3 │ 4"]);
    }

    #[test]
    fn markdown_table_preserves_whitespace_around_inline_code() {
        let md = "| Feature | Why |\n|---|---|\n| GPS | Uses `CLLocation.distance(from:)` with a check |";
        let lines = render_markdown(None, md);
        let rows = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                "Feature │ Why",
                "GPS │ Uses `CLLocation.distance(from:)` with a check",
            ]
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
            let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
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
    fn paragraph_after_list_does_not_join_the_final_item() {
        let lines = render_markdown(None, "* first\n* final\n\nAfterward.");
        let rows = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rows, ["• first", "• final", "Afterward."]);
    }

    #[test]
    fn list_items_with_completion_prefix() {
        let md = "* one\n* two\n* three";
        let lines = render_completion_markdown("🚀 ", md);
        // First line gets the "🚀 " prefix + "• " bullet
        // Remaining lines get only the "• " bullet
        assert!(lines.len() >= 3);
        // First line should have both prefixes
        let first_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
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
        let term_width = crate::cli::text_utils::get_terminal_width().max(1);
        let long = "error ".repeat(term_width.div_ceil("error ".len()) + 1);
        let lines = render_error_markdown("✗ Error", &long);
        assert!(
            lines.len() > 1,
            "expected wrap into multiple lines, got {} line(s): {:?}",
            lines.len(),
            lines
        );
        // No emitted line should be wider than the terminal — wrapping
        // must have split the input before any row overflowed.
        for (i, line) in lines.iter().enumerate() {
            let width: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                width <= term_width,
                "line {i} overflowed terminal width {term_width}: width={width} content={:?}",
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            );
        }
    }

    /// The first line must carry the red-bold prefix; continuation
    /// lines must not repeat it.
    #[test]
    fn render_error_markdown_prefix_only_on_first_line() {
        let long =
            "first part of error that fills more than a line of output so it must wrap second part";
        let lines = render_error_markdown("✗ Error", long);
        let first_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
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
        let first_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first_text.contains("first"));
        let middle_text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            middle_text.is_empty(),
            "expected blank middle line, got: {middle_text:?}",
        );
        let last_text: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(last_text.contains("third"));
    }
}
