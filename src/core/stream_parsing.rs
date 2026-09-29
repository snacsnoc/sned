//! Stream parsing and thinking-section detection for model output.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkOpenKind {
    /// Opened by ```think — only ``` closes it (code fences inside thinking are preserved)
    CodeFenceThink,
    /// Opened by <think> or <!-- think --> — can close with any end marker including ```
    TagOrUnicode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceDelimiter {
    pub marker: u8,
    pub run_length: usize,
}

/// Parse a Markdown-style fence at the start of a line.
///
/// Sned accepts up to three leading spaces, matching the streaming filter and
/// the final-output parser. The suffix is returned so callers can distinguish
/// an opening fence's info string from a closing fence.
#[must_use]
pub fn parse_fence_start(line: &str) -> Option<(FenceDelimiter, &str)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|&&byte| byte == b' ').count();
    if indent > 3 || indent == bytes.len() {
        return None;
    }

    let marker = match bytes[indent] {
        b'`' | b'~' => bytes[indent],
        _ => return None,
    };
    let run_length = bytes[indent..]
        .iter()
        .take_while(|&&byte| byte == marker)
        .count();
    if run_length < 3 {
        return None;
    }

    Some((
        FenceDelimiter { marker, run_length },
        &line[indent + run_length..],
    ))
}

#[must_use]
pub fn is_fence_closer(line: &str, opener: FenceDelimiter) -> bool {
    let Some((candidate, suffix)) = parse_fence_start(line) else {
        return false;
    };
    candidate.marker == opener.marker
        && candidate.run_length >= opener.run_length
        && suffix.trim().is_empty()
}

#[must_use]
pub fn classify_think_start(line: &str) -> Option<ThinkOpenKind> {
    if let Some((fence, suffix)) = parse_fence_start(line)
        && fence.marker == b'`'
        && fence.run_length == 3
        && suffix.trim() == "think"
    {
        Some(ThinkOpenKind::CodeFenceThink)
    } else if line.trim().starts_with("<think>") || line.trim().starts_with("<!-- think -->") {
        Some(ThinkOpenKind::TagOrUnicode)
    } else {
        None
    }
}

#[must_use]
pub fn is_think_end(line: &str) -> bool {
    let trimmed = line.trim();
    // Explicit tags work for either thinking syntax; only a bare three-backtick
    // line closes fenced thinking.
    trimmed == "</think>" || trimmed == "<!-- /think -->" || trimmed == "```"
}

const THINKING_OPEN_TAGS: [&str; 2] = ["<think>", "<!-- think -->"];
const THINKING_CLOSE_TAGS: [&str; 2] = ["</think>", "<!-- /think -->"];

/// Incremental stream filter that hides provider thinking sections while
/// passing visible model text through chunk by chunk.
#[derive(Debug, Default)]
pub struct ThinkingTagStreamFilter {
    pending: String,
    hidden: String,
    think_open_kind: Option<ThinkOpenKind>,
    fence_marker: Option<FenceDelimiter>,
    at_line_start: bool,
}

impl ThinkingTagStreamFilter {
    pub fn new() -> Self {
        Self {
            at_line_start: true,
            ..Self::default()
        }
    }

    pub fn push(&mut self, chunk: &str) -> String {
        let mut input = std::mem::take(&mut self.pending);
        input.push_str(chunk);
        let mut visible = String::new();
        let mut pos = 0;

        loop {
            if pos == input.len() {
                break;
            }
            let remaining = &input[pos..];

            if self.think_open_kind == Some(ThinkOpenKind::CodeFenceThink) {
                if self.at_line_start {
                    if let Some((line, consumed)) = complete_stream_line(remaining) {
                        if is_think_end(line) {
                            pos += consumed;
                            self.think_open_kind = None;
                            self.at_line_start = true;
                            continue;
                        }
                    } else if could_be_fenced_think_line(remaining, "```") {
                        break;
                    }
                }
                let ch = remaining
                    .chars()
                    .next()
                    .expect("remaining input is not empty");
                self.hidden.push(ch);
                pos += ch.len_utf8();
                self.at_line_start = ch == '\n';
                continue;
            }

            if self.think_open_kind == Some(ThinkOpenKind::TagOrUnicode) {
                if let Some(tag) = complete_prefix(remaining, &THINKING_CLOSE_TAGS) {
                    pos += tag.len();
                    self.think_open_kind = None;
                    continue;
                }
                if has_partial_prefix(remaining, &THINKING_CLOSE_TAGS) {
                    break;
                }
                let ch = remaining
                    .chars()
                    .next()
                    .expect("remaining input is not empty");
                self.hidden.push(ch);
                pos += ch.len_utf8();
                continue;
            }

            if self.at_line_start {
                if self.fence_marker.is_none() {
                    if let Some((line, consumed)) = complete_stream_line(remaining) {
                        if classify_think_start(line) == Some(ThinkOpenKind::CodeFenceThink) {
                            pos += consumed;
                            self.think_open_kind = Some(ThinkOpenKind::CodeFenceThink);
                            self.at_line_start = true;
                            continue;
                        }
                    } else if could_be_fenced_think_line(remaining, "```think") {
                        break;
                    }
                }
                match fence_prefix(remaining, self.fence_marker) {
                    FencePrefix::Complete(fence) => {
                        self.fence_marker = match self.fence_marker {
                            Some(_) => None,
                            None => Some(fence),
                        };
                        self.at_line_start = false;
                    }
                    FencePrefix::Partial => break,
                    FencePrefix::NotFence => self.at_line_start = false,
                }
            }

            if self.fence_marker.is_none() {
                if let Some(tag) = complete_prefix(remaining, &THINKING_OPEN_TAGS) {
                    pos += tag.len();
                    self.think_open_kind = Some(ThinkOpenKind::TagOrUnicode);
                    continue;
                }
                if has_partial_prefix(remaining, &THINKING_OPEN_TAGS) {
                    break;
                }
            }

            let ch = remaining
                .chars()
                .next()
                .expect("remaining input is not empty");
            pos += ch.len_utf8();
            visible.push(ch);
            if ch == '\n' {
                self.at_line_start = true;
            }
        }

        self.pending.push_str(&input[pos..]);

        visible
    }

    pub fn finish(&mut self) -> String {
        if self.think_open_kind == Some(ThinkOpenKind::CodeFenceThink) {
            if is_think_end(&self.pending) {
                self.pending.clear();
                self.think_open_kind = None;
            } else {
                self.hidden.push_str(&self.pending);
                self.pending.clear();
            }
            String::new()
        } else if self.think_open_kind == Some(ThinkOpenKind::TagOrUnicode) {
            self.hidden.push_str(&self.pending);
            self.pending.clear();
            String::new()
        } else if classify_think_start(&self.pending) == Some(ThinkOpenKind::CodeFenceThink) {
            self.pending.clear();
            self.think_open_kind = Some(ThinkOpenKind::CodeFenceThink);
            String::new()
        } else {
            std::mem::take(&mut self.pending)
        }
    }

    pub fn take_hidden(&mut self) -> Option<String> {
        (!self.hidden.is_empty()).then(|| std::mem::take(&mut self.hidden))
    }
}

fn complete_stream_line(input: &str) -> Option<(&str, usize)> {
    let newline = input.find('\n')?;
    let line = input[..newline]
        .strip_suffix('\r')
        .unwrap_or(&input[..newline]);
    Some((line, newline + 1))
}

fn could_be_fenced_think_line(input: &str, marker: &str) -> bool {
    let trimmed = input.trim_start_matches(' ');
    let indent = input.len() - trimmed.len();
    indent <= 3
        && (marker.starts_with(trimmed)
            || (trimmed.starts_with(marker) && trimmed[marker.len()..].trim().is_empty()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FencePrefix {
    Complete(FenceDelimiter),
    Partial,
    NotFence,
}

fn complete_prefix<'a>(input: &str, tags: &'a [&str]) -> Option<&'a str> {
    tags.iter().copied().find(|tag| input.starts_with(tag))
}

fn has_partial_prefix(input: &str, tags: &[&str]) -> bool {
    tags.iter().any(|tag| tag.starts_with(input))
}

fn fence_prefix(input: &str, active_fence: Option<FenceDelimiter>) -> FencePrefix {
    let line_end = input.find('\n');
    let line = line_end.map_or(input, |end| &input[..end]);

    if let Some((candidate, suffix)) = parse_fence_start(line) {
        if let Some(opener) = active_fence {
            if candidate.marker != opener.marker {
                return FencePrefix::NotFence;
            }
            if candidate.run_length >= opener.run_length
                && suffix.trim().is_empty()
                && (line_end.is_some() || !suffix.is_empty())
            {
                return FencePrefix::Complete(candidate);
            }
            if line_end.is_none() && suffix.is_empty() {
                return FencePrefix::Partial;
            }
            return FencePrefix::NotFence;
        }

        if line_end.is_none() && suffix.is_empty() {
            return FencePrefix::Partial;
        }
        return FencePrefix::Complete(candidate);
    }

    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|&&byte| byte == b' ').count();
    if indent > 3 || indent == bytes.len() {
        return if indent <= 3 {
            FencePrefix::Partial
        } else {
            FencePrefix::NotFence
        };
    }
    let rest = &bytes[indent..];
    let marker = rest[0];
    if !matches!(marker, b'`' | b'~')
        || active_fence.is_some_and(|fence| fence.marker != marker)
        || !rest.iter().all(|&byte| byte == marker)
    {
        return FencePrefix::NotFence;
    }
    FencePrefix::Partial
}

fn strip_common_indent(lines: &[&str]) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }

    let indent_counts: std::collections::HashMap<usize, usize> = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .fold(
            std::collections::HashMap::with_capacity(4),
            |mut acc, indent| {
                *acc.entry(indent).or_insert(0) += 1;
                acc
            },
        );

    if indent_counts.is_empty() {
        return lines.iter().map(|_| String::new()).collect();
    }

    let min_indent = *indent_counts
        .keys()
        .min()
        .expect("indent_counts checked non-empty but min() returned None");
    let dedent = if min_indent > 0 {
        min_indent
    } else {
        let (dominant_indent, dominant_count) = indent_counts
            .iter()
            .filter(|(indent, _)| **indent > 0)
            .max_by(|(indent_a, count_a), (indent_b, count_b)| {
                count_a.cmp(count_b).then(indent_a.cmp(indent_b))
            })
            .map_or((0, 0), |(indent, count)| (*indent, *count));

        let non_empty_count: usize = indent_counts.values().sum();
        let dominant_block_count = lines
            .iter()
            .filter(|line| {
                let indent = line.len() - line.trim_start().len();
                indent >= dominant_indent && !line.trim().is_empty()
            })
            .count();

        if dominant_indent >= 16
            && dominant_count >= 2
            && dominant_block_count * 2 >= non_empty_count
        {
            dominant_indent
        } else {
            0
        }
    };

    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else if line.len() - line.trim_start().len() >= dedent {
                // A byte dedent can land inside a multibyte character;
                // keep such lines verbatim rather than panic or split one.
                line.get(dedent..).unwrap_or(line).to_string()
            } else {
                line.to_string()
            }
        })
        .collect()
}

#[must_use]
pub fn split_model_output(text: &str) -> (Option<String>, Option<String>) {
    let mut thinking: Option<String> = None;
    let mut response: Option<String> = None;
    let mut in_think = false;
    let mut think_open_kind: Option<ThinkOpenKind> = None;
    let mut literal_fence: Option<FenceDelimiter> = None;
    let mut think_lines: Vec<&str> = Vec::new();
    let mut response_lines: Vec<&str> = Vec::new();

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(opener) = literal_fence {
            response_lines.push(line);
            if is_fence_closer(line, opener) {
                literal_fence = None;
            }
            continue;
        }
        if !in_think && let Some((fence, suffix)) = parse_fence_start(line) {
            let is_thinking_fence =
                fence.marker == b'`' && fence.run_length == 3 && suffix.trim() == "think";
            if !is_thinking_fence {
                literal_fence = Some(fence);
                response_lines.push(line);
                continue;
            }
        }
        if let Some(kind) = classify_think_start(line) {
            in_think = true;
            think_open_kind = Some(kind);
            think_lines.clear();
            continue;
        }
        if think_open_kind.is_some() && is_think_end(line) {
            in_think = false;
            think_open_kind = None;
            continue;
        }
        if in_think {
            think_lines.push(line);
        } else {
            response_lines.push(line);
        }
    }

    if !think_lines.is_empty() {
        let dedented = strip_common_indent(&think_lines);
        let t = dedented
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    l.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        thinking = if t.is_empty() { None } else { Some(t) };
    }

    if !response_lines.is_empty() {
        let dedented = strip_common_indent(&response_lines);
        let r = dedented
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    l.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        response = if r.is_empty() { None } else { Some(r) };
    }

    (thinking, response)
}

/// Remove lines that are tool-call status markers emitted by the agent
/// loop during tool execution (e.g. "▶ execute_command", "✓ ...", "📝
/// ..."). These lines appear in the model's `accumulated_text` because
/// the model may stream text *while* a tool is executing, but the
/// actual tool-call UI already rendered these lines as separate
/// `OutputEvent::Line`/`RawAnsi` events. If we include them in the
/// markdown re-render produced by `finalize_turn_stream`, the lines
/// appear twice — once from the re-render, once from the original
/// event.
///
/// Recognised prefixes (any amount of leading whitespace is tolerated):
///
/// - `▶` — "running tool" indicator
/// - `✓` — "tool completed" indicator
/// - `📝` — edit-file status lines
/// - `⏱` — elapsed-time / context lines
/// - `⏳` — "waiting" / in-progress
/// - `⠋` — spinner character
/// - `[sned]` — internal status messages
#[must_use]
pub fn strip_tool_call_lines(input: &str) -> String {
    let mut literal_fence: Option<FenceDelimiter> = None;
    let mut kept: Vec<&str> = Vec::new();
    for line in input.lines() {
        let stripped = line.strip_suffix('\r').unwrap_or(line);
        if let Some(opener) = literal_fence {
            kept.push(line);
            if is_fence_closer(stripped, opener) {
                literal_fence = None;
            }
            continue;
        }
        if let Some((fence, _)) = parse_fence_start(stripped) {
            literal_fence = Some(fence);
            kept.push(line);
            continue;
        }
        if !is_tool_call_marker_line(line) {
            kept.push(line);
        }
    }
    kept.join("\n")
}

/// Return the cleaned assistant response that is safe to show through the
/// `/full` recovery path. Thinking blocks and internal tool status lines are
/// not part of the response the user asked the model to produce.
#[must_use]
pub fn extract_response_text(text: &str) -> Option<String> {
    let (_, response) = split_model_output(text);
    response
        .map(|response| strip_tool_call_lines(&response))
        .filter(|response| !response.trim().is_empty())
}

/// Check whether a response contains a fenced code block larger than the
/// streamed display limit. This keeps an oversized code response recoverable
/// even when its non-code text is short.
#[must_use]
pub fn contains_code_block_over_limit(text: &str, limit: usize) -> bool {
    let mut in_code_block = false;
    let mut code_lines = 0usize;

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code_block && code_lines > limit {
                return true;
            }
            in_code_block = !in_code_block;
            code_lines = 0;
        } else if in_code_block {
            code_lines += 1;
        }
    }

    in_code_block && code_lines > limit
}

/// Returns true if a single line is a tool-call marker emitted by the
/// agent loop's tool execution UI.  These lines are **not** part of the
/// user-visible model response — they are purely internal status
/// indicators that already have their own rendered event.
fn is_tool_call_marker_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return false;
    }

    // Internal status messages — [sned] prefix
    if trimmed.starts_with("[sned]") {
        return true;
    }

    // Tool-call / tool-result status markers (Unicode prefix after trim)
    // "▶" — execute_command / tool-running indicator
    // "✓" — tool-result success indicator
    // "📝" — edit-file status
    // "⏱" — elapsed time / context usage
    // "⏳" — pending / in-progress
    // "⠋" — spinner character
    let first_char = trimmed.chars().next().unwrap();
    matches!(first_char, '▶' | '✓' | '📝' | '⏱' | '⏳' | '⠋')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_think_end_explicit_markers_always_work() {
        assert!(is_think_end("</think>"));
        assert!(is_think_end(" </think> "));
        assert!(is_think_end("<!-- /think -->"));
        assert!(is_think_end(" <!-- /think --> "));
    }

    #[test]
    fn test_is_think_end_code_fence_markers() {
        assert!(is_think_end("```"));
        assert!(!is_think_end("````"));
        assert!(!is_think_end("```python"));
    }

    #[test]
    fn test_is_think_end_not_end_marker() {
        assert!(!is_think_end("some text"));
        assert!(!is_think_end("</think>ing"));
    }

    #[test]
    fn test_split_model_output_with_explicit_end_marker() {
        let input = "<think>\nThis is thinking\n</think>\nThis is response";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, Some("This is thinking".to_string()));
        assert_eq!(response, Some("This is response".to_string()));
    }

    #[test]
    fn test_split_model_output_with_comment_tags() {
        let input = "<!-- think -->\nThinking content\n<!-- /think -->\nResponse content";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, Some("Thinking content".to_string()));
        assert_eq!(response, Some("Response content".to_string()));
    }

    #[test]
    fn test_split_model_output_preserves_thinking_tags_in_source_fences() {
        let input = "before\n```html\n<think>literal</think>\n<!-- think -->literal<!-- /think -->\n```\nafter";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, None);
        assert_eq!(response.as_deref(), Some(input));
    }

    #[test]
    fn test_literal_fence_requires_matching_run_and_whitespace_suffix() {
        let input = concat!(
            "before\n",
            "````\n",
            "<think>literal</think>\n",
            "```python\n",
            "```text\n",
            "````\n",
            "after"
        );
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, None);
        assert_eq!(response.as_deref(), Some(input));

        let input = "before\n```\nliteral\n````\nafter";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, None);
        assert_eq!(response.as_deref(), Some(input));
    }

    #[test]
    fn test_fence_indentation_matches_streaming_contract() {
        let input = "    ```think\nhidden\n    ```\nafter";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, None);
        assert_eq!(response.as_deref(), Some(input));
    }

    #[test]
    fn test_split_model_output_think_then_explicit_end() {
        let input = "<think>\nFirst thought\n</think>\nResponse with marker";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, Some("First thought".to_string()));
        assert_eq!(response, Some("Response with marker".to_string()));
    }

    #[test]
    fn test_split_model_output_preserves_blank_line_boundaries() {
        let input = "<think>\n\nfirst\n\n</think>\n\nanswer\n";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, Some("\nfirst\n".to_string()));
        assert_eq!(response, Some("\nanswer\n".to_string()));
    }

    #[test]
    fn test_split_model_output_preserves_crlf_content_without_carriage_returns() {
        let input = "<think>\r\nfirst\r\n\r\nsecond\r\n</think>\r\nanswer\r\n";
        let (thinking, response) = split_model_output(input);
        assert_eq!(thinking, Some("first\n\nsecond".to_string()));
        assert_eq!(response, Some("answer\n".to_string()));
    }

    // --- strip_tool_call_lines tests ---

    #[test]
    fn test_strip_tool_call_lines_removes_execute_command_marker() {
        let input = "▶ execute_command\nsome response text";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "some response text");
    }

    #[test]
    fn test_strip_tool_call_lines_removes_result_marker() {
        let input = "  ✓ Command completed\nHere is the output";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "Here is the output");
    }

    #[test]
    fn test_strip_tool_call_lines_removes_edit_marker() {
        let input = "📝 Edited 2 files\nDone.";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "Done.");
    }

    #[test]
    fn test_strip_tool_call_lines_removes_sned_status() {
        let input = "[sned] Context: 50% left\nResponse text";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "Response text");
    }

    #[test]
    fn test_strip_tool_call_lines_removes_all_marker_types() {
        let input = "▶ tool 1\n✓ done\n📝 edited\n⏱ elapsed\n⏳ waiting\n⠋ spinning\nreal text";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "real text");
    }

    #[test]
    fn test_strip_tool_call_lines_preserves_normal_text() {
        let input = "Hello world\nThis is a normal response\nNo markers here";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_tool_call_lines_preserves_text_with_similar_prefix() {
        // "▶" inside text or at start of a normal sentence should not be treated as a
        // tool marker — but our filter only checks the *first* character after trim,
        // so a sentence starting with "▶" would be stripped. This is intentional:
        // the agent loop only emits tool markers at the start of a line with no
        // preceding text, so a model response starting with "▶" is almost certainly
        // a tool marker, not genuine content.
        let input = "▶ execute_command\n▶ ls /tmp\n▶ cat file.txt\nFinal answer: hello";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "Final answer: hello");
    }

    #[test]
    fn test_strip_tool_call_lines_empty_input() {
        let result = strip_tool_call_lines("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_strip_tool_call_lines_only_markers() {
        let input = "▶ tool\n✓ done\n📝 edited";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "");
    }

    #[test]
    fn test_strip_tool_call_lines_leaves_empty_lines() {
        let input = "▶ tool\n\nsome text\n\n✓ done";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "\nsome text\n");
    }

    #[test]
    fn test_strip_tool_call_lines_whitespace_before_marker() {
        let input = "    ▶ execute_command\n    ✓ done\nresponse";
        let result = strip_tool_call_lines(input);
        assert_eq!(result, "response");
    }

    #[test]
    fn test_strip_tool_call_lines_intentional_stripping_of_tool_markers() {
        // Regression: lines starting with ▶, ✓, 📝, etc. are intentionally
        // stripped from the TurnEnd payload. This is a documented trade-off:
        // the agent loop only emits these markers, so a model response starting
        // with ▶ is assumed to be a tool marker. This is intentional behavior,
        // not a bug.
        let input = "▶ execute_command\ndetails\n✓ done\n📝 notes\nresponse text";
        let result = strip_tool_call_lines(input);
        // Tool marker lines are stripped (removed entirely); normal text is preserved.
        assert_eq!(result, "details\nresponse text");
    }

    #[test]
    fn test_strip_tool_call_lines_normal_text_preserved() {
        // Regression: normal text that does NOT start with a tool marker
        // is preserved even if it contains similar characters.
        let input = "▶ tool\nsome normal text with ▶ in the middle\nend";
        let result = strip_tool_call_lines(input);
        // The first line "▶ tool" is stripped; remaining lines are joined with \n.
        assert_eq!(result, "some normal text with ▶ in the middle\nend");
    }

    #[test]
    fn test_extract_response_text_removes_thinking_and_tool_markers() {
        let input = "<think>\nreasoning\n</think>\n▶ execute_command\nHere is the answer.";
        assert_eq!(
            extract_response_text(input).as_deref(),
            Some("Here is the answer.")
        );
    }

    #[test]
    fn test_extract_response_text_rejects_tool_only_and_empty_input() {
        assert_eq!(extract_response_text("▶ execute_command\n✓ done"), None);
        assert_eq!(extract_response_text("<think>only reasoning</think>"), None);
        assert_eq!(extract_response_text(""), None);
    }

    #[test]
    fn test_contains_code_block_over_limit() {
        assert!(contains_code_block_over_limit("```\na\nb\n```", 1));
        assert!(!contains_code_block_over_limit("```\na\n```", 1));
    }

    #[test]
    fn test_dedent_never_cuts_short_indent_lines() {
        let lines = vec![
            "                alpha".to_string(),
            "                beta".to_string(),
            "12345678901234567890".to_string(),
        ];
        assert_eq!(
            strip_common_indent(&lines.iter().map(String::as_str).collect::<Vec<_>>()),
            vec![
                "alpha".to_string(),
                "beta".to_string(),
                "12345678901234567890".to_string(),
            ]
        );
    }

    #[test]
    fn test_dedent_never_splits_a_multibyte_character() {
        let lines = vec![
            "                alpha".to_string(),
            "                beta".to_string(),
            "123456789012345Étail".to_string(),
        ];
        assert_eq!(
            strip_common_indent(&lines.iter().map(String::as_str).collect::<Vec<_>>())[2],
            "123456789012345Étail".to_string()
        );
    }

    #[test]
    fn test_dedent_keeps_line_when_cut_lands_inside_a_character() {
        // Fifteen spaces plus a two-byte NBSP: the 16-byte dedent lands
        // between the NBSP bytes, which byte indexing cannot express.
        let nbsp_line = "               \u{a0}x".to_string();
        let lines = vec![
            "                alpha".to_string(),
            "                beta".to_string(),
            nbsp_line.clone(),
        ];
        assert_eq!(
            strip_common_indent(&lines.iter().map(String::as_str).collect::<Vec<_>>()),
            vec!["alpha".to_string(), "beta".to_string(), nbsp_line]
        );
    }

    #[test]
    fn test_strip_tool_call_lines_keeps_fenced_marker_lookalikes() {
        let input = "```\n✓ done\n[sned] note\n▶ run\n```\nafter";
        assert_eq!(strip_tool_call_lines(input), input);
    }

    #[test]
    fn test_strip_tool_call_lines_still_strips_outside_fences() {
        let input = "▶ execute_command\n```\n✓ kept\n```\n✓ done\nanswer";
        assert_eq!(strip_tool_call_lines(input), "```\n✓ kept\n```\nanswer");
    }

    #[test]
    fn thinking_filter_retains_only_a_partial_delimiter_between_chunks() {
        let mut filter = ThinkingTagStreamFilter::new();
        assert_eq!(filter.push(&"x".repeat(100_000)), "x".repeat(100_000));
        assert!(filter.pending.len() < "<!-- think -->".len());
        assert_eq!(filter.push("<thi"), "");
        assert_eq!(filter.pending, "<thi");
    }
}
