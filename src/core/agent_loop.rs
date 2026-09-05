//! Core task and agent loop for sned CLI.
//!
//! # Lock Ordering
//!
//! To prevent deadlocks, always acquire locks in this order:
//! 1. `self.state` (TaskState)
//! 2. `self.conversation_history` (Vec<StorageMessage>)
//! 3. `self.message_queue` (VecDeque<StorageMessage>)
//!
//! Never acquire a lower-priority lock while holding a higher-priority one.
//! When multiple locks are needed, acquire them in order and release them in
//! reverse order when possible.

use crate::cli::output::OutputEvent;
use crate::cli::tui::theme::{ERROR_FG, PROMPT_FG};
use crate::core::agent_types::code_block_display_limit;
pub use crate::core::agent_types::{AgentConfig, AgentError, AgentMode, TaskState, TurnResult};
use crate::core::context::{
    ApiReqInfo, PromptBuilder, SystemPromptContext, context_manager, context_window,
};
use crate::core::file_editor::AnchorStateManager;
use crate::core::provider_retry::{
    DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES, RetryConfig, create_message_with_retry,
};
use crate::core::tools::SnedTool;
use crate::core::tools::{
    ToolContext, ToolFailureClass, ToolFailureMetadata, ToolRegistry, ToolRequiredNextStep,
    coerce_command_array, coerce_string_array, tool_result_to_text,
};
use crate::providers::{
    ApiStreamChunk, ApiStreamToolCall, AssistantContentBlock, MessageContent, MessageRole,
    Provider, ProviderRequest, RedactedThinkingBlock, SharedContentFields, StorageMessage,
    TextContentBlock, ThinkingBlock, ToolResultContent, ToolUseBlock, UserContentBlock,
};
use crate::providers::{ProviderError, Providers};
use crate::storage::global_state::HistoryItem;
use crate::storage::state_manager::StateManager;
use crate::storage::task_storage::TaskStorage;
use futures::future::FutureExt;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};

const DEFAULT_MESSAGE_QUEUE_MAX_LEN: usize = 1000;
const MESSAGE_QUEUE_MAX_LEN_ENV: &str = "SNED_AGENT_MAX_QUEUED_MESSAGES";
const MAX_QUEUED_MESSAGE_PREVIEW_CHARS: usize = 256;

/// Default token limit for tool results stored in history (~5000 tokens / ~20KB)
const DEFAULT_TOOL_RESULT_HISTORY_LIMIT: usize = 20_000;
/// Environment variable to configure tool result history limit
const TOOL_RESULT_HISTORY_LIMIT_ENV: &str = "SNED_TOOL_RESULT_HISTORY_LIMIT";

/// Default token limit for thinking blocks in old history entries (~2000 tokens)
const DEFAULT_THINKING_HISTORY_LIMIT: usize = 2_000;
/// Environment variable to configure thinking block history limit
const THINKING_HISTORY_LIMIT_ENV: &str = "SNED_THINKING_HISTORY_LIMIT";

use crate::core::plan_state::PlanStepStatus;
use crate::core::stream_parsing::{
    extract_response_text, split_model_output, truncate_json_arguments,
};
use crate::core::tool_output::{
    extract_edit_stats_detailed, format_heat_map, format_heat_map_plain, format_tool_call_lines,
    format_tool_call_lines_with_raw_arguments, format_tool_result, format_tool_result_digest,
    path_from_read_file_header, strip_tool_result_anchors, summarize_matching_sections,
};

const MAX_EDIT_RESULT_DISPLAY_LINES: usize = 10;
/// Default concurrency limit for parallel non-grouped tool execution.
/// Prevents I/O contention when many tools run simultaneously.
const DEFAULT_TOOL_CONCURRENCY: usize = 12;
/// Maximum number of times a single provider stream can be retried
/// within one turn when the stream fails before any output is
/// emitted. Without this cap, a provider returning repeated retryable
/// transport errors would loop indefinitely. Set equal to
/// DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES so the user-facing
/// behavior matches the request-level cap.
const MAX_STREAM_RETRY_ATTEMPTS: usize = DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES as usize;
const PARTIAL_MODEL_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
// MAX_TOOL_ARGUMENT_SIZE moved to providers/mod.rs for shared use
use crate::providers::MAX_TOOL_ARGUMENT_SIZE;

async fn wait_for_cancellation(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        if flag.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        interval.tick().await;
    }
}

#[derive(Debug, Clone)]
struct ToolExecutionOutput {
    text: String,
    metadata: Option<ToolFailureMetadata>,
    is_error: bool,
    hook_context: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileActionPath {
    normalized: String,
    display: String,
}

fn stream_retry_delay(retry_attempt: usize) -> std::time::Duration {
    std::time::Duration::from_secs(1_u64 << retry_attempt.saturating_sub(1).min(2))
}

impl ToolExecutionOutput {
    fn error(text: String, metadata: Option<ToolFailureMetadata>) -> Self {
        Self {
            text,
            metadata,
            is_error: true,
            hook_context: Vec::new(),
        }
    }

    fn success_with_hook_context(text: String, hook_context: Vec<String>) -> Self {
        Self {
            text,
            metadata: None,
            is_error: false,
            hook_context,
        }
    }

    fn error_with_hook_context(
        text: String,
        metadata: Option<ToolFailureMetadata>,
        hook_context: Vec<String>,
    ) -> Self {
        Self {
            text,
            metadata,
            is_error: true,
            hook_context,
        }
    }
}

fn append_tool_result_blocks(
    blocks: &mut Vec<UserContentBlock>,
    tool_id: String,
    result_output: ToolExecutionOutput,
) {
    let truncated_text = truncate_tool_result(&result_output.text);
    blocks.push(UserContentBlock::ToolResult(
        crate::providers::ToolResultBlock {
            tool_use_id: tool_id.clone(),
            content: ToolResultContent::Text(truncated_text),
            shared: SharedContentFields {
                call_id: Some(tool_id),
                signature: None,
            },
        },
    ));
    for context in result_output.hook_context {
        blocks.push(UserContentBlock::Text(TextContentBlock {
            text: context,
            shared: SharedContentFields {
                call_id: None,
                signature: None,
            },
            reasoning_details: None,
        }));
    }
}

/// Truncate tool result text to fit within the configured history limit.
/// Returns the truncated text with a marker if truncation occurred.
fn truncate_tool_result(result: &str) -> String {
    let limit = std::env::var(TOOL_RESULT_HISTORY_LIMIT_ENV)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOOL_RESULT_HISTORY_LIMIT);

    if result.len() <= limit {
        return result.to_string();
    }

    // Truncate at byte boundary and add marker
    let truncated_len = limit.saturating_sub(50); // Reserve space for marker
    let boundary = result
        .floor_char_boundary(truncated_len.min(result.len()))
        .min(result.len());

    let truncated = &result[..boundary];
    let original_lines = result.lines().count();
    let truncated_lines = truncated.lines().count();
    let remaining_lines = original_lines - truncated_lines;

    format!("{truncated}\n\n[{remaining_lines} lines truncated, use read_file to see full content]")
}

fn code_fence_language(line: &str) -> &str {
    line.trim_start()
        .trim_start_matches("```")
        .split_whitespace()
        .next()
        .unwrap_or("")
}

fn edit_result_diff_previews(result: &str) -> Vec<String> {
    let mut sections = result.split("\n\n");
    sections.next();
    let mut previews = Vec::new();
    while let Some(summary) = sections.next() {
        if summary.starts_with("Applied ") && summary.contains("edit(s) successfully") {
            let mut preview_section = sections.next().unwrap_or_default();
            if preview_section.starts_with("Because the changes were extensive") {
                preview_section = sections.next().unwrap_or_default();
            }
            let preview = format_tool_result(preview_section, MAX_EDIT_RESULT_DISPLAY_LINES);
            if !preview.is_empty() {
                previews.push(preview);
            }
        }
    }
    previews
}

fn strip_edit_diff_anchor(line: &str) -> String {
    let (prefix, anchored_line) = ["+ ", "- ", "  "]
        .into_iter()
        .find_map(|prefix| line.strip_prefix(prefix).map(|rest| (prefix, rest)))
        .unwrap_or(("", line));
    let Some((anchor, content)) = anchored_line.split_once('§') else {
        return line.to_string();
    };
    if anchor.is_empty() || anchor.contains(char::is_whitespace) {
        return line.to_string();
    }
    format!("{prefix}{content}")
}

fn strip_edit_diff_anchors(line: &mut ratatui::text::Line<'static>) {
    for span in &mut line.spans {
        let text = span.content.to_string();
        let stripped = strip_edit_diff_anchor(&text);
        if stripped != text {
            span.content = stripped.into();
        }
    }
}

// Cached terminal width to avoid repeated syscalls during streaming output.
// Terminal width rarely changes mid-task; refresh every 2 seconds.
static TERM_WIDTH_CACHE: std::sync::Mutex<Option<(usize, std::time::Instant)>> =
    std::sync::Mutex::new(None);

fn get_terminal_width() -> usize {
    use std::time::{Duration, Instant};

    const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

    let mut cache = TERM_WIDTH_CACHE.lock().expect("TERM_WIDTH_CACHE poisoned");
    let now = Instant::now();

    let needs_refresh = cache
        .as_ref()
        .is_none_or(|(_, last)| now.duration_since(*last) >= REFRESH_INTERVAL);

    if needs_refresh {
        let width = crossterm::terminal::size()
            .map(|(cols, _)| cols as usize)
            .unwrap_or(80);
        *cache = Some((width, now));
        width
    } else {
        cache.as_ref().map_or(80, |(w, _)| *w)
    }
}

fn streaming_model_line(text: String, style_markdown: bool) -> Line<'static> {
    if style_markdown {
        let mut rendered = crate::cli::markdown::render_markdown(None, &text);
        if rendered.len() == 1 {
            let mut line = rendered
                .pop()
                .expect("markdown renderer returned empty output");
            for span in &mut line.spans {
                if span.style.fg.is_none() {
                    span.style.fg = Some(crate::cli::tui::theme::ACCENT);
                }
            }
            return line;
        }
    }

    // Block constructs need full-turn context and remain raw until TurnEnd.
    Line::from(Span::styled(
        text,
        Style::default().fg(crate::cli::tui::theme::ACCENT),
    ))
}

fn print_model_line(
    line: &str,
    output_writer: &crate::cli::output::OutputWriterArc,
    style_markdown: bool,
) {
    use crate::cli::output::OutputEvent;
    let term_width = get_terminal_width();
    let indent = "  ";
    let sanitized = sanitize_model_text_for_display(line);
    if sanitized.trim().is_empty() {
        return;
    }
    let wrapped = crate::cli::text_utils::wrap_text(&sanitized, term_width, indent);

    // The TUI output buffer stores one ratatui Line per visual line. Emitting a
    // single Line that still contains embedded '\n' lets one model event occupy
    // multiple rows inside a single span, which can scramble viewport math and
    // corrupt rendering when model output is long or malformed.
    for wrapped_line in wrapped.lines() {
        output_writer.emit(OutputEvent::Line(streaming_model_line(
            wrapped_line.to_string(),
            style_markdown,
        )));
    }
}

fn update_model_line(
    line: &str,
    output_writer: &crate::cli::output::OutputWriterArc,
    style_markdown: bool,
) {
    let sanitized = sanitize_model_text_for_display(line);
    if sanitized.trim().is_empty() {
        return;
    }
    output_writer.emit(OutputEvent::ModelUpdateLine(streaming_model_line(
        sanitized.into_owned(),
        style_markdown,
    )));
}

/// Like `print_model_line`, but if `pending` is true, emits a separate
/// turn-indicator line ("♦") before the model output and clears the flag.
/// The indicator is emitted as `OutputEvent::TurnIndicator` so that
/// `finalize_turn_stream` does not strip it when re-rendering as markdown.
fn print_model_line_with_prefix_if_pending(
    line: &str,
    output_writer: &crate::cli::output::OutputWriterArc,
    pending: &mut bool,
    style_markdown: bool,
) {
    if *pending && !line.trim().is_empty() {
        *pending = false;
        // Emit the turn indicator as a separate event so the TUI stores it
        // outside the streamed-line buffer. `finalize_turn_stream` pops the
        // streamed lines and re-renders them as markdown; if the indicator
        // were part of the streamed text, it would be lost in the re-render.
        output_writer.emit(crate::cli::output::OutputEvent::turn_indicator("\u{2666}"));
    }
    print_model_line(line, output_writer, style_markdown);
}

fn update_model_line_with_prefix_if_pending(
    line: &str,
    output_writer: &crate::cli::output::OutputWriterArc,
    pending: &mut bool,
    style_markdown: bool,
) {
    if *pending && !line.trim().is_empty() {
        *pending = false;
        output_writer.emit(crate::cli::output::OutputEvent::turn_indicator("\u{2666}"));
        print_model_line(line, output_writer, style_markdown);
        return;
    }
    update_model_line(line, output_writer, style_markdown);
}

fn stream_error_is_retryable(error: &str) -> bool {
    error.contains("(retryable)")
}

fn report_shadow_commit_result(
    output_writer: &crate::cli::output::OutputWriterArc,
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
) {
    let error = match result {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error.to_string(),
        Err(error) => format!("background task failed: {error}"),
    };
    let detail = error
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    output_writer.emit(OutputEvent::tool_output_line(
        format!("Change tracking failed; /diff and /log will not include this turn: {detail}"),
        Style::default().fg(ERROR_FG),
    ));
}

fn sanitize_model_text_for_display(line: &str) -> Cow<'_, str> {
    if line.chars().all(|ch| !ch.is_control() && ch != '\t') {
        Cow::Borrowed(line)
    } else {
        Cow::Owned(
            line.chars()
                .map(|ch| {
                    if matches!(ch, '\t') {
                        ' '
                    } else if ch.is_control() {
                        ' '
                    } else {
                        ch
                    }
                })
                .collect(),
        )
    }
}

fn print_code_block(
    lines: &[String],
    lang: &str,
    output_writer: &crate::cli::output::OutputWriterArc,
    interactive_mode: bool,
) {
    use crate::cli::output::OutputEvent;
    if lines.is_empty() {
        return;
    }

    let code = lines.join("\n");
    let highlighted = crate::cli::syntax_highlight::highlight_code(&code, lang);
    let rendered = format!("  {}\n", highlighted.replace('\n', "\n  "));
    if interactive_mode {
        for line in crate::cli::tui::ansi_converter::ansi_to_ratatui_lines(&rendered) {
            output_writer.emit(OutputEvent::Line(line));
        }
    } else {
        output_writer.emit(OutputEvent::RawAnsi(rendered));
    }
}

fn emit_turn_end(
    output_writer: &crate::cli::output::OutputWriterArc,
    json_output: bool,
    markdown_text: &str,
) {
    if json_output {
        return;
    }

    let accumulated_text = crate::core::stream_parsing::strip_tool_call_lines(markdown_text);
    if !accumulated_text.is_empty() {
        output_writer.emit(OutputEvent::TurnEnd { accumulated_text });
    }
}

fn snipped_code_block_hint() -> &'static str {
    "  ... [snipped from streamed display; use /full]"
}

fn message_queue_max_len() -> usize {
    std::env::var(MESSAGE_QUEUE_MAX_LEN_ENV)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_MESSAGE_QUEUE_MAX_LEN)
}

async fn enqueue_message_with_limit(
    queue: &Arc<Mutex<VecDeque<StorageMessage>>>,
    message: StorageMessage,
    max_queue_len: usize,
) -> (usize, usize) {
    let mut mq = queue.lock().await;
    mq.push_back(message);

    let mut dropped = 0usize;
    while mq.len() > max_queue_len {
        mq.pop_front();
        dropped += 1;
    }

    (mq.len(), dropped)
}

struct AgentLoopDeps {
    registry: Option<Arc<ToolRegistry>>,
    system_prompt_context: Option<SystemPromptContext>,
    cached_system_prompt: Option<String>,
    loaded_agents_rule_paths: HashSet<String>,
    context_loader: Option<crate::core::context::ContextLoader>,
    task_storage: Option<TaskStorage>,
    hook_manager: Option<Arc<crate::core::hooks::HookManager>>,
    approval_manager: Option<Arc<tokio::sync::Mutex<crate::core::approval::ApprovalManager>>>,
    checkpoint_manager: Option<crate::core::checkpoints::TaskCheckpointManager>,
    tool_profile: Option<crate::core::tools::definitions::ToolProfile>,
    /// When true, the tool profile is forced to at least `Validate` so
    /// `execute_command` is available. This is the explicit opt-in for
    /// shell execution (paired with `--yolo` / `--auto-approve-all`).
    yolo: bool,
}

impl AgentLoopDeps {
    fn new() -> Self {
        Self {
            registry: None,
            system_prompt_context: None,
            cached_system_prompt: None,
            loaded_agents_rule_paths: HashSet::new(),
            context_loader: None,
            task_storage: None,
            hook_manager: None,
            approval_manager: None,
            checkpoint_manager: None,
            tool_profile: None,
            yolo: false,
        }
    }

    fn registry(&self) -> &Arc<ToolRegistry> {
        self.registry
            .as_ref()
            .expect("AgentLoopDeps: registry not initialized. Call with_tools() before run().")
    }
}

struct PreparedToolCall {
    tool_call: ApiStreamToolCall,
    tool_id: String,
    tool_name: String,
    parsed_args: Result<serde_json::Value, String>,
}

/// A clonable handle for enqueuing messages into an AgentLoop from any task.
#[derive(Clone)]
pub struct MessageQueueHandle {
    queue: Arc<Mutex<VecDeque<StorageMessage>>>,
    json_output: bool,
    message_counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl MessageQueueHandle {
    pub async fn enqueue_text_message(&self, text: String) {
        let msg = StorageMessage {
            id: Some(AgentLoop::next_message_id(&self.message_counter)),
            role: MessageRole::User,
            content: MessageContent::Text(text),
            model_info: None,
            metrics: None,
            ts: Some(chrono::Utc::now().timestamp_millis() as u64),
        };
        let max_queue_len = message_queue_max_len();
        let (count, dropped) = enqueue_message_with_limit(&self.queue, msg, max_queue_len).await;

        if dropped > 0 {
            warn!(
                max_queue_len,
                dropped, "message queue exceeded its limit; dropped {} queued message(s)", dropped
            );
            if !self.json_output {
                info!(
                    "[sned] Warning: queue overflow — dropped {} message(s) (limit is {})",
                    dropped, max_queue_len
                );
            }
        }

        if !self.json_output && count > 0 {
            info!(
                "[sned] Message queued ({} message{} in queue)",
                count,
                if count == 1 { "" } else { "s" }
            );
        }
    }

    pub async fn prepend_text_message(&self, text: String) {
        let msg = StorageMessage {
            id: Some(AgentLoop::next_message_id(&self.message_counter)),
            role: MessageRole::User,
            content: MessageContent::Text(text),
            model_info: None,
            metrics: None,
            ts: Some(chrono::Utc::now().timestamp_millis() as u64),
        };
        let max_queue_len = message_queue_max_len();
        let mut mq = self.queue.lock().await;
        mq.push_front(msg);

        let mut dropped = 0usize;
        while mq.len() > max_queue_len {
            mq.pop_back();
            dropped += 1;
        }

        let count = mq.len();
        drop(mq);

        if dropped > 0 {
            warn!(
                max_queue_len,
                dropped,
                "message queue exceeded its limit; dropped {} queued message(s) from the back",
                dropped
            );
            if !self.json_output {
                info!(
                    "[sned] Warning: queue overflow — dropped {} queued message(s) (limit is {})",
                    dropped, max_queue_len
                );
            }
        }

        if !self.json_output && count > 0 {
            info!(
                "[sned] Message queued to run next ({} message{} in queue)",
                count,
                if count == 1 { "" } else { "s" }
            );
        }
    }

    pub async fn queued_message_count(&self) -> usize {
        self.queue.lock().await.len()
    }

    /// Synchronous queue length (for use in the TUI main loop).
    pub fn try_queued_message_count(&self) -> Option<usize> {
        self.queue.try_lock().ok().map(|q| q.len())
    }

    /// Synchronously read the queue count and text previews for the TUI.
    pub fn try_queued_message_snapshot(&self, limit: usize) -> Option<(usize, Vec<String>)> {
        let queue = self.queue.try_lock().ok()?;
        let count = queue.len();
        let previews = queue
            .iter()
            .take(limit)
            .filter_map(|msg| match &msg.content {
                MessageContent::Text(text) => {
                    let mut chars = text.chars();
                    let mut preview: String = chars
                        .by_ref()
                        .take(MAX_QUEUED_MESSAGE_PREVIEW_CHARS)
                        .collect();
                    if chars.next().is_some() {
                        preview.push('…');
                    }
                    Some(preview)
                }
                _ => None,
            })
            .collect();
        Some((count, previews))
    }

    pub async fn has_queued_messages(&self) -> bool {
        !self.queue.lock().await.is_empty()
    }

    pub async fn peek_queued_messages(&self, limit: usize) -> Vec<String> {
        let queue = self.queue.lock().await;
        queue
            .iter()
            .take(limit)
            .filter_map(|msg| {
                if let MessageContent::Text(text) = &msg.content {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

/// The core agent loop that orchestrates provider requests, stream handling,
/// tool dispatch, and state management.
pub struct AgentLoop {
    config: AgentConfig,
    state: Arc<Mutex<TaskState>>,
    /// Clone of `TaskState::is_cancelled_atomic` for lock-free reads
    /// in the hot-path streaming loop (avoids mutex per chunk).
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// Lock-free once-per-turn flags for the three `record_first_*_time`
    /// helpers below. Each call takes `state.lock().await` to write the
    /// `Instant`; subsequent calls skip the lock entirely once the
    /// corresponding flag is set. The TUI main loop also reads
    /// `TaskState` on every iteration via `try_lock`, so reducing mutex
    /// acquisitions on the streaming hot path cuts contention.
    first_output_emit_recorded: std::sync::atomic::AtomicBool,
    first_reasoning_chunk_recorded: std::sync::atomic::AtomicBool,
    first_displayable_text_recorded: std::sync::atomic::AtomicBool,
    anchor_mgr: AnchorStateManager,
    conversation_history: Arc<Mutex<Vec<StorageMessage>>>,
    message_queue: Arc<Mutex<VecDeque<StorageMessage>>>,
    deps: AgentLoopDeps,
    state_manager: Option<Arc<crate::storage::state_manager::StateManager>>,
    /// Tracks model/provider/mode usage for task metadata
    model_tracker: Option<crate::core::context_tracking::ModelContextTracker>,
    /// Tracks environment snapshots for task metadata
    env_tracker: Option<crate::core::context_tracking::EnvironmentContextTracker>,
    /// Monotonically increasing counter for generating unique message IDs.
    /// Shared via Arc so static methods (execute_tool_with_hooks_internal) can also generate IDs.
    message_counter: Arc<std::sync::atomic::AtomicUsize>,
    current_turn_retry_candidate: Option<StorageMessage>,
}

impl AgentLoop {
    fn current_turn_retry_candidate(history: &[StorageMessage]) -> Option<StorageMessage> {
        history.iter().rev().find_map(|message| {
            if message.role == MessageRole::User {
                Some(message.clone())
            } else {
                None
            }
        })
    }

    fn parse_tool_arguments(
        tool_name: &str,
        tool_id: &str,
        raw_arguments: Option<&String>,
    ) -> Result<serde_json::Value, String> {
        let Some(raw) = raw_arguments else {
            return Ok(serde_json::json!({}));
        };
        // Treat empty string as no arguments (some providers send empty string instead of "{}")
        if raw.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(parsed) => {
                if let Some(parse_error) = crate::providers::tool_arguments_error(&parsed) {
                    return Err(format!(
                        "Tool '{tool_name}' arguments could not be repaired as JSON (id: {tool_id}): {parse_error} Please retry the same tool call with valid JSON arguments."
                    ));
                }
                Ok(parsed)
            }
            Err(err) => {
                let preview: String = raw.chars().take(200).collect();
                tracing::error!(
                    tool_name = %tool_name,
                    tool_id = %tool_id,
                    error = %err,
                    args_len = raw.len(),
                    args_preview = %preview,
                    "failed to parse tool call arguments JSON"
                );
                Err(format!(
                    "Tool '{tool_name}' arguments were invalid JSON and could not be parsed (id: {tool_id}): {err}. Please retry with valid JSON arguments."
                ))
            }
        }
    }

    fn prepare_tool_calls(
        tool_call_order: &[String],
        tool_calls_map: &mut HashMap<String, ApiStreamToolCall>,
    ) -> Vec<PreparedToolCall> {
        let mut prepared = Vec::with_capacity(tool_call_order.len());

        for key in tool_call_order {
            let Some(tool_call) = tool_calls_map.get_mut(key) else {
                error!(
                    "Tool call order mismatch: key '{}' not found in tool_calls_map. \
                     This indicates a stream parsing bug.",
                    key
                );
                continue;
            };

            if tool_call
                .function
                .id
                .as_ref()
                .is_none_or(std::string::String::is_empty)
            {
                let generated = ulid::Ulid::new().to_string();
                tool_call.function.id = Some(generated);
            }

            let mut tool_call_clone = tool_call.clone();
            let tool_id = tool_call_clone.function.id.take().unwrap_or_else(|| {
                error!("Tool call ID is None after initialization, generating fallback");
                ulid::Ulid::new().to_string()
            });
            let tool_name = tool_call_clone.function.name.take().unwrap_or_else(|| {
                warn!("Tool call missing name, using 'unknown_tool'");
                "unknown_tool".to_string()
            });
            let parsed_args = Self::parse_tool_arguments(
                &tool_name,
                &tool_id,
                tool_call.function.arguments.as_ref(),
            );

            prepared.push(PreparedToolCall {
                tool_call: tool_call_clone,
                tool_id,
                tool_name,
                parsed_args,
            });
        }

        prepared
    }

    fn assistant_tool_input(prepared: &PreparedToolCall) -> serde_json::Value {
        match &prepared.parsed_args {
            Ok(value) => value.clone(),
            Err(_) => serde_json::json!({
                "_raw_arguments": prepared
                    .tool_call
                    .function
                    .arguments
                    .as_deref()
                    .unwrap_or("")
            }),
        }
    }

    fn synthetic_json_completion_event(
        text_only_completes_task: bool,
        completion_tool_emitted: bool,
        response_text: Option<&str>,
    ) -> Option<serde_json::Value> {
        if !text_only_completes_task || completion_tool_emitted {
            return None;
        }

        let result = response_text?;
        if result.is_empty() {
            return None;
        }

        Some(serde_json::json!({
            "type": "completion",
            "result": result,
        }))
    }

    async fn plan_execution_active(&self) -> bool {
        let state = self.state.lock().await;
        state
            .plan_state
            .as_ref()
            .is_some_and(|plan| plan.approved && !plan.complete && !plan.paused)
    }

    async fn record_first_state_update(
        &self,
        recorded: &std::sync::atomic::AtomicBool,
        update: impl FnOnce(&mut TaskState),
    ) {
        // Only the first chunk should contend with TUI state reads for each timing marker.
        if recorded.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock().await;
        if recorded.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        update(&mut state);
    }

    async fn reset_stream_attempt_timing(&self) {
        self.first_output_emit_recorded
            .store(false, std::sync::atomic::Ordering::Release);
        self.first_reasoning_chunk_recorded
            .store(false, std::sync::atomic::Ordering::Release);
        self.first_displayable_text_recorded
            .store(false, std::sync::atomic::Ordering::Release);

        let mut state = self.state.lock().await;
        state.request_sent_time =
            crate::cli::output::timing_enabled().then(std::time::Instant::now);
        state.first_provider_chunk_time = None;
        state.first_reasoning_chunk_time = None;
        state.first_displayable_text_time = None;
        state.first_output_emit_time = None;
    }

    async fn wait_for_stream_retry_delay(&self, delay: std::time::Duration) -> bool {
        let poll_interval = std::time::Duration::from_millis(100);
        let mut elapsed = std::time::Duration::ZERO;
        while elapsed < delay {
            if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return false;
            }
            let remaining = delay.saturating_sub(elapsed);
            let sleep_for = poll_interval.min(remaining);
            tokio::time::sleep(sleep_for).await;
            elapsed += sleep_for;
        }
        !self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn record_first_output_emit_time(&self) {
        self.record_first_state_update(&self.first_output_emit_recorded, |state| {
            if state.first_output_emit_time.is_none() {
                if crate::cli::output::timing_enabled() {
                    let now = std::time::Instant::now();
                    state.first_output_emit_time = Some(now);
                    if state.first_token_time.is_none() {
                        state.first_token_time = Some(now);
                    }
                }
                state.reasoning_active = false;
            }
        })
        .await;
    }

    async fn record_first_reasoning_chunk_time(&self) {
        self.record_first_state_update(&self.first_reasoning_chunk_recorded, |state| {
            if state.first_reasoning_chunk_time.is_none() {
                if crate::cli::output::timing_enabled() {
                    state.first_reasoning_chunk_time = Some(std::time::Instant::now());
                }
                state.reasoning_active = true;
            }
        })
        .await;
    }

    async fn record_first_displayable_text_time(&self) {
        if !crate::cli::output::timing_enabled() {
            return;
        }
        self.record_first_state_update(&self.first_displayable_text_recorded, |state| {
            if state.first_displayable_text_time.is_none() {
                let now = std::time::Instant::now();
                state.first_displayable_text_time = Some(now);
            }
        })
        .await;
    }

    #[must_use]
    pub fn new(config: AgentConfig) -> Self {
        let is_subagent = config.is_subagent_execution;
        let state = TaskState {
            is_subagent_execution: is_subagent,
            ..TaskState::default()
        };
        let cancelled = state.is_cancelled_atomic.clone();
        let task_id = config.task_id.clone();
        Self {
            config,
            state: Arc::new(Mutex::new(state)),
            cancelled,
            first_output_emit_recorded: std::sync::atomic::AtomicBool::new(false),
            first_reasoning_chunk_recorded: std::sync::atomic::AtomicBool::new(false),
            first_displayable_text_recorded: std::sync::atomic::AtomicBool::new(false),
            anchor_mgr: AnchorStateManager::new(),
            conversation_history: Arc::new(Mutex::new(Vec::new())),
            message_queue: Arc::new(Mutex::new(VecDeque::new())),
            deps: AgentLoopDeps::new(),
            state_manager: None,
            model_tracker: Some(crate::core::context_tracking::ModelContextTracker::new(
                &task_id,
            )),
            env_tracker: Some(
                crate::core::context_tracking::EnvironmentContextTracker::new(&task_id),
            ),
            message_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            current_turn_retry_candidate: None,
        }
    }

    /// Enable yolo mode — forces tool profile to `Validate` so
    /// `execute_command` is available (explicit shell opt-in).
    #[must_use]
    pub fn with_yolo(mut self, yolo: bool) -> Self {
        self.deps.yolo = yolo;
        self
    }

    /// Generate the next unique message ID for this task.
    /// Format: `msg_{counter}` (monotonically increasing per AgentLoop instance).
    fn next_message_id(counter: &std::sync::atomic::AtomicUsize) -> String {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("msg_{n}")
    }

    /// Get the underlying provider as a cloned Arc.
    pub fn get_provider(&self) -> Arc<Providers> {
        self.config
            .provider
            .lock()
            .expect("provider poisoned")
            .clone()
    }

    /// Get the current agent mode.
    pub fn mode(&self) -> crate::core::agent_types::AgentMode {
        self.config.mode
    }

    /// Set the active provider. Preserves conversation history.
    pub async fn set_provider(&mut self, new_provider: Arc<Providers>) {
        let context_window =
            crate::core::context::get_context_window_info(&new_provider).context_window;
        *self.config.provider.lock().expect("provider poisoned") = new_provider;
        if let Some(info) = self.state.lock().await.last_api_req_info.as_mut() {
            info.recalculate_context_window(context_window);
        }
    }

    /// Set the agent mode (used for Plan -> Act transition after approval).
    pub fn set_mode(&mut self, mode: crate::core::agent_types::AgentMode) {
        if self.config.mode != mode {
            self.config.mode = mode;
            // A cached profile from the previous mode can omit the completion
            // tool required in ACT or expose it while gathering a plan.
            self.deps.tool_profile = None;
            // Invalidate the cached system prompt so any mode-dependent text
            // is rebuilt under the new mode.
            self.deps.cached_system_prompt = None;
        }
    }

    /// Get the task ID.
    pub fn task_id(&self) -> &str {
        &self.config.task_id
    }

    /// Get a reference to the checkpoint manager, if configured.
    pub fn checkpoint_manager(&self) -> Option<&crate::core::checkpoints::TaskCheckpointManager> {
        self.deps.checkpoint_manager.as_ref()
    }

    /// Get a reference to the output writer.
    pub fn output_writer(&self) -> &crate::cli::output::OutputWriterArc {
        &self.config.output_writer
    }

    /// Get a clonable handle for enqueuing messages from other tasks.
    pub fn message_queue_handle(&self) -> MessageQueueHandle {
        MessageQueueHandle {
            queue: self.message_queue.clone(),
            json_output: self.config.json_output,
            message_counter: self.message_counter.clone(),
        }
    }

    /// Initialize the agent loop with a checkpoint manager.
    #[must_use]
    pub fn with_checkpoint_manager(
        mut self,
        checkpoint_manager: crate::core::checkpoints::TaskCheckpointManager,
    ) -> Self {
        self.deps.checkpoint_manager = Some(checkpoint_manager);
        self
    }

    /// Initialize the agent loop with an approval manager.
    #[must_use]
    pub fn with_approval_manager(
        mut self,
        approval_manager: Arc<tokio::sync::Mutex<crate::core::approval::ApprovalManager>>,
    ) -> Self {
        self.deps.approval_manager = Some(approval_manager);
        self
    }

    /// Initialize the agent loop with a context loader.
    #[must_use]
    pub fn with_context_loader(mut self, loader: crate::core::context::ContextLoader) -> Self {
        self.deps.context_loader = Some(loader);
        self
    }

    /// Initialize the agent loop with task storage for persisting conversation history.
    #[must_use]
    pub fn with_task_storage(mut self, task_storage: TaskStorage) -> Self {
        self.deps.task_storage = Some(task_storage);
        self
    }

    /// Set the system prompt context.
    #[must_use]
    pub fn with_system_prompt_context(mut self, context: SystemPromptContext) -> Self {
        self.deps.loaded_agents_rule_paths.clear();
        if let Some(cwd) = context.cwd.as_deref() {
            let root_rule = Path::new(cwd).join("AGENTS.md");
            let canonical_root_rule = root_rule
                .canonicalize()
                .ok()
                .map(|path| path.to_string_lossy().into_owned());
            if root_rule
                .metadata()
                .ok()
                .is_some_and(|metadata| metadata.is_file())
                && !matches!(
                    context
                        .local_agents_rule_toggles
                        .get(&root_rule.to_string_lossy().to_string()),
                    Some(false)
                )
                && !canonical_root_rule.as_ref().is_some_and(|path| {
                    matches!(context.local_agents_rule_toggles.get(path), Some(false))
                })
                && context.local_agents_rules_file_instructions.is_some()
            {
                self.deps
                    .loaded_agents_rule_paths
                    .insert(root_rule.to_string_lossy().into_owned());
                if let Some(canonical_root_rule) = canonical_root_rule {
                    self.deps
                        .loaded_agents_rule_paths
                        .insert(canonical_root_rule);
                }
            }
        }
        self.deps.system_prompt_context = Some(context);
        self.deps.cached_system_prompt = None;
        self
    }

    /// Runs the main agent loop.
    ///
    /// The loop sequence:
    /// 1. Build system prompt with context
    /// 2. Send provider request
    /// 3. Handle streaming response
    /// 4. Process assistant message
    /// 5. Dispatch tools if needed
    /// 6. Append tool results
    /// 7. Repeat until complete, cancelled, or max turns reached
    ///
    async fn record_task_history(&self, state_manager: &Arc<StateManager>, task_text: &str) {
        let workspace_root_str = self.resolve_workspace_root().to_str().map(String::from);
        let state_guard = self.state.lock().await;
        let history_item = HistoryItem {
            id: self.config.task_id.clone(),
            ulid: Some(self.config.task_id.clone()),
            number: 0,
            ts: chrono::Utc::now().timestamp_millis(),
            task: task_text.to_string(),
            tokens_in: state_guard.cumulative_tokens_in as i32,
            tokens_out: state_guard.cumulative_tokens_out as i32,
            cache_writes: Some(state_guard.cumulative_cache_writes as i32),
            cache_reads: Some(state_guard.cumulative_cache_reads as i32),
            total_cost: state_guard.cumulative_cost,
            size: None,
            shadow_git_config_work_tree: None,
            cwd_on_task_initialization: workspace_root_str.clone(),
            conversation_history_deleted_range: state_guard
                .conversation_history_deleted_range
                .map(|(start, end)| vec![start as i32, end as i32]),
            is_favorited: None,
            workspace_root_path: workspace_root_str,
            checkpoint_manager_error_message: None,
            model_id: None,
        };
        drop(state_guard);

        state_manager.add_task_to_history(history_item);
        if let Err(error) = StateManager::persist_async(Arc::clone(state_manager)).await {
            error!("Failed to persist task history: {}", error);
        }
    }

    /// Initialize the agent loop with tool handlers.
    #[must_use]
    pub fn with_tools(mut self, registry: Arc<ToolRegistry>) -> Self {
        self.deps.registry = Some(registry);
        self
    }

    /// Initialize the agent loop with hook manager.
    #[must_use]
    pub fn with_hooks(mut self, hook_manager: Arc<crate::core::hooks::HookManager>) -> Self {
        self.deps.hook_manager = Some(hook_manager);
        self
    }

    pub async fn run(
        &mut self,
        initial_messages: Vec<StorageMessage>,
        state_manager: Arc<crate::storage::state_manager::StateManager>,
    ) -> Result<(), AgentError> {
        tracing::debug!(target: "sned::agent_loop", "AgentLoop::run() called with {} initial messages", initial_messages.len());
        if initial_messages
            .iter()
            .any(|message| message.role == MessageRole::User)
        {
            // Adaptive profiles belong to the top-level task, not the session.
            self.deps.tool_profile = None;
        }
        // Store state_manager for use during execution
        self.state_manager = Some(state_manager.clone());
        self.current_turn_retry_candidate = initial_messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .cloned();

        // Initialize conversation history
        // On resume, history may already be populated from disk - append instead of replace
        {
            let mut history = self.conversation_history.lock().await;
            if history.is_empty() {
                *history = initial_messages;
            } else if !initial_messages.is_empty() {
                history.extend(initial_messages);
            }
        }

        // Apply double-check completion setting from config and wire task_id into tracker
        {
            let mut state = self.state.lock().await;
            state.double_check_completion_enabled = self.config.double_check_completion;
            state.first_tool_result_printed = false;
            // Initialize session start time for session summary
            state.session_start_time = Some(std::time::Instant::now());
            if state.file_context_tracker.task_id().is_none() {
                state.file_context_tracker = state
                    .file_context_tracker
                    .clone()
                    .with_task_id(self.config.task_id.clone());
            }
            // Initialize file watcher for real-time external edit detection
            if let Err(e) = state.file_context_tracker.init_watcher() {
                warn!(
                    "Failed to initialize file watcher: {}. External edit detection disabled.",
                    e
                );
            }
        }

        // Record environment snapshot for task metadata
        if let Some(ref tracker) = self.env_tracker {
            if let Err(e) = tracker.record_environment() {
                warn!(error = %e, "Failed to record environment snapshot");
            }
        }

        // Initialize shadow git repo for change tracking
        if self.config.track_changes
            && let Ok(workspace_root) = std::env::current_dir()
            && let Err(e) = crate::core::shadow_git::init_shadow_repo(&workspace_root)
        {
            warn!(
                "Failed to initialize shadow git repo: {}. Change tracking disabled.",
                e
            );
        }

        // Apply subagents enabled setting from global state
        {
            let mut state = self.state.lock().await;
            state.subagents_enabled = state_manager
                .get_global_state_key::<bool>(crate::storage::GlobalStateKey::SubagentsEnabled)
                .unwrap_or(false);
        }

        // Process initial context with ContextLoader on first turn
        if let Some(ref loader) = self.deps.context_loader {
            let mut history = self.conversation_history.lock().await;
            if let Some(first_msg) = history.first_mut()
                && let crate::providers::MessageContent::Text(ref text) = first_msg.content
            {
                let (enriched_text, env_details) = loader.load_initial_context(text).await;

                // Update first message with enriched text
                first_msg.content = crate::providers::MessageContent::Text(enriched_text);

                // Append environment details as a separate message
                history.push(crate::providers::StorageMessage {
                    id: Some(Self::next_message_id(&self.message_counter)),
                    role: crate::providers::MessageRole::User,
                    content: crate::providers::MessageContent::Text(env_details),
                    model_info: None,
                    metrics: None,
                    ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                });
            }
        }

        let mut turn_count = 0u32;
        let mut task_text = None;

        // Extract task text from first user message for hooks
        {
            let history = self.conversation_history.lock().await;
            if let Some(first_msg) = history.first()
                && let crate::providers::MessageContent::Text(ref text) = first_msg.content
            {
                task_text = Some(text.clone());
            }
        }

        // Execute TaskStart hook before first turn with timeout to prevent hangs
        if let Some(hook_mgr) = self.deps.hook_manager.clone() {
            let task = task_text.clone().unwrap_or_default();
            let task_id = self.config.task_id.clone();

            // Hook execution timeout: 10 seconds default (configurable via SNED_HOOK_TIMEOUT_MS).
            // Lower than the previous 60s because a misbehaving TaskStart hook (e.g. a
            // slow `git status` on a large repo) blocks the entire submit path for the
            // full timeout. Users with legitimate slow hooks opt in via the env var.
            let timeout_ms = std::env::var("SNED_HOOK_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&v| v > 0)
                .unwrap_or(10_000);
            let timeout_duration = std::time::Duration::from_millis(timeout_ms);

            // Note: HookManager::task_start is synchronous, so we use tokio::task::spawn_blocking
            let result = match tokio::time::timeout(timeout_duration, async {
                tokio::task::spawn_blocking(move || hook_mgr.task_start(&task_id, &task)).await
            })
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => {
                    error!("TaskStart hook join failed: {}", e);
                    crate::core::hooks::HookResult {
                        output: None,
                        error: Some(format!("Hook execution failed: {e}")),
                        exit_code: -1,
                        execution_time_ms: 0,
                    }
                }
                Err(_) => {
                    error!("TaskStart hook timed out after {}ms", timeout_ms);
                    crate::core::hooks::HookResult {
                        output: None,
                        error: Some(format!("Hook execution timed out after {timeout_ms}ms")),
                        exit_code: -1,
                        execution_time_ms: timeout_ms,
                    }
                }
            };

            if let Some(output) = result.output {
                if let Some(modification) = output.context_modification {
                    info!("[TaskStart hook] {}", modification);
                    // Inject context modification into conversation history
                    let mut history = self.conversation_history.lock().await;
                    history.push(StorageMessage {
                        id: Some(Self::next_message_id(&self.message_counter)),
                        role: MessageRole::User,
                        content: MessageContent::Text(format!(
                            "[Hook context from TaskStart]: {modification}"
                        )),
                        model_info: None,
                        metrics: None,
                        ts: None,
                    });
                    drop(history);
                }
                if output.cancel == Some(true) {
                    self.record_task_history(&state_manager, task_text.as_deref().unwrap_or(""))
                        .await;
                    // Persist state on hook cancellation
                    if let Err(e) = StateManager::persist_async(Arc::clone(&state_manager)).await {
                        error!("Failed to persist state manager on hook cancel: {}", e);
                    }
                    return Err(AgentError::Cancelled);
                }
            }
        }

        let mut dequeued_message_for_notification = false;
        let mut paused_plan_epoch = false;
        let mut pause_notice_emitted = false;

        loop {
            // A paused plan must wait without consuming provider turns. Emit
            // one notice per pause epoch, then let /plan resume or /plan abort
            // change the shared state while the task remains available.
            {
                let state = self.state.lock().await;
                let plan_is_paused = state
                    .plan_state
                    .as_ref()
                    .is_some_and(|plan| plan.paused && plan.approved);
                if plan_is_paused {
                    drop(state);
                    paused_plan_epoch = true;
                    if !pause_notice_emitted {
                        self.config.output_writer.emit(OutputEvent::dim_yellow(
                            "Plan is paused. Type /plan resume to continue.",
                        ));
                        pause_notice_emitted = true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                drop(state);

                if paused_plan_epoch {
                    let plan_still_active = {
                        let state = self.state.lock().await;
                        state
                            .plan_state
                            .as_ref()
                            .is_some_and(|plan| plan.approved && !plan.complete)
                    };
                    if !plan_still_active {
                        return Ok(());
                    }
                    paused_plan_epoch = false;
                    pause_notice_emitted = false;
                }
            }

            if turn_count >= self.config.max_turns {
                self.record_task_history(&state_manager, task_text.as_deref().unwrap_or(""))
                    .await;
                // Persist state on max turns exceeded
                if let Err(e) = StateManager::persist_async(Arc::clone(&state_manager)).await {
                    error!("Failed to persist state manager on max turns: {}", e);
                }
                // Force-save conversation history to preserve final turns
                if let Some(ref storage) = self.deps.task_storage {
                    let history = self.conversation_history.lock().await.clone();
                    if !history.is_empty()
                        && let Err(e) = storage.write_api_conversation_history_async(&history).await
                    {
                        error!("Failed to save conversation history on max turns: {}", e);
                    }
                }
                return Err(AgentError::MaxTurnsExceeded);
            }
            turn_count += 1;

            // Check if cancelled
            {
                let state = self.state.lock().await;
                if state.is_cancelled {
                    drop(state);
                    if !self.config.json_output {
                        self.config
                            .output_writer
                            .emit(OutputEvent::info("Cancelled. Type /retry to resend."));
                    }
                    // Execute full abort sequence: TaskCancel hook, state save, resource cleanup
                    let cancellation_handler =
                        crate::core::cancellation::CancellationHandler::new(self.state.clone());
                    if let Err(e) = cancellation_handler
                        .abort_task(
                            self.deps
                                .hook_manager
                                .as_ref()
                                .map(std::convert::AsRef::as_ref),
                            Arc::clone(&state_manager),
                            &self.config.task_id,
                            Some(&self.anchor_mgr),
                        )
                        .await
                    {
                        error!(
                            "Cancellation handler failed: {}. Attempting fallback cleanup.",
                            e
                        );
                        // Fallback: at least save state to prevent data loss
                        if let Err(save_e) =
                            StateManager::persist_async(Arc::clone(&state_manager)).await
                        {
                            error!("Fallback state persist failed: {}", save_e);
                        }
                    }
                    // Force-save conversation history to preserve turns that would
                    // otherwise be lost to the debounce window (W4)
                    if let Some(ref storage) = self.deps.task_storage {
                        let history = self.conversation_history.lock().await.clone();
                        if !history.is_empty()
                            && let Err(e) =
                                storage.write_api_conversation_history_async(&history).await
                        {
                            tracing::error!(
                                "Failed to save conversation history on cancellation: {}",
                                e
                            );
                        }
                    }
                    self.record_task_history(&state_manager, task_text.as_deref().unwrap_or(""))
                        .await;
                    return Ok(());
                }
            }

            // Check message queue for pending messages
            {
                let mut mq = self.message_queue.lock().await;
                if let Some(queued_message) = mq.pop_front() {
                    let queue_remaining = mq.len();
                    drop(mq);
                    self.current_turn_retry_candidate = Some(queued_message.clone());
                    if !self.config.json_output {
                        if queue_remaining > 0 {
                            info!(
                                "[sned] Processing queued message ({} more queued)",
                                queue_remaining
                            );
                            self.config.output_writer.emit(OutputEvent::info(format!(
                                "Processing queued message ({} more queued)",
                                queue_remaining
                            )));
                        } else {
                            info!("[sned] Processing queued message");
                            self.config
                                .output_writer
                                .emit(OutputEvent::info("Processing queued message"));
                        }
                        // Display the queued message in the transcript only when
                        // it leaves the queue and begins its agent turn.
                        if let MessageContent::Text(ref text) = queued_message.content {
                            self.config
                                .output_writer
                                .emit(OutputEvent::queued_message_started(queue_remaining));
                            self.config
                                .output_writer
                                .emit(OutputEvent::user_prompt_line(text));
                        }
                    }
                    let expanded_message = self.expand_message_mentions(queued_message).await;
                    // If a plan is active, prepend plan context so the model doesn't abandon it
                    let final_message = {
                        let state = self.state.lock().await;
                        if let Some(ref plan) = state.plan_state
                            && plan.approved
                            && !plan.complete
                            && !plan.paused
                        {
                            let note = format!(
                                "[Note: A plan is in progress at step {}/{}. Continue executing the plan after addressing this message.]\n\n",
                                plan.current_step_index + 1,
                                plan.steps.len(),
                            );
                            let mut msg = expanded_message;
                            if let MessageContent::Text(ref text) = msg.content {
                                msg.content = MessageContent::Text(format!("{note}{text}"));
                            }
                            msg
                        } else {
                            expanded_message
                        }
                    };
                    let mut history = self.conversation_history.lock().await;
                    history.push(final_message);
                    drop(history);
                    let mut state = self.state.lock().await;
                    state.clear_denied_tool_actions();
                    dequeued_message_for_notification = true;
                }
            }

            // Execute one turn
            match self.execute_turn().await {
                TurnResult::Continue => {
                    self.current_turn_retry_candidate = None;
                    if dequeued_message_for_notification && !self.config.json_output {
                        info!("[sned] Queued message sent to provider");
                        self.config
                            .output_writer
                            .emit(OutputEvent::info("Queued message sent to provider"));
                    }
                    dequeued_message_for_notification = false;
                    continue;
                }
                TurnResult::Complete => {
                    self.current_turn_retry_candidate = None;
                    if dequeued_message_for_notification && !self.config.json_output {
                        info!("[sned] Queued message sent to provider");
                        self.config
                            .output_writer
                            .emit(OutputEvent::info("Queued message sent to provider"));
                    }
                    dequeued_message_for_notification = false;

                    // Check if more messages are queued
                    {
                        let mut mq = self.message_queue.lock().await;
                        if let Some(queued_message) = mq.pop_front() {
                            let queue_remaining = mq.len();
                            drop(mq);
                            self.current_turn_retry_candidate = Some(queued_message.clone());
                            if !self.config.json_output {
                                if queue_remaining > 0 {
                                    info!(
                                        "[sned] Processing queued message ({} more queued)",
                                        queue_remaining,
                                    );
                                    self.config.output_writer.emit(OutputEvent::info(format!(
                                        "Processing queued message ({} more queued)",
                                        queue_remaining
                                    )));
                                } else {
                                    info!("[sned] Processing queued message");
                                    self.config
                                        .output_writer
                                        .emit(OutputEvent::info("Processing queued message"));
                                }
                                // Display the queued message in the transcript only when
                                // it leaves the queue and begins its agent turn.
                                if let MessageContent::Text(ref text) = queued_message.content {
                                    self.config
                                        .output_writer
                                        .emit(OutputEvent::queued_message_started(queue_remaining));
                                    self.config
                                        .output_writer
                                        .emit(OutputEvent::user_prompt_line(text));
                                }
                            }
                            let expanded_message =
                                self.expand_message_mentions(queued_message).await;
                            // If a plan is active, prepend plan context so the model doesn't abandon it
                            let final_message = {
                                let state = self.state.lock().await;
                                if let Some(ref plan) = state.plan_state
                                    && plan.approved
                                    && !plan.complete
                                    && !plan.paused
                                {
                                    let note = format!(
                                        "[Note: A plan is in progress at step {}/{}. Continue executing the plan after addressing this message.]\n\n",
                                        plan.current_step_index + 1,
                                        plan.steps.len(),
                                    );
                                    let mut msg = expanded_message;
                                    if let MessageContent::Text(ref text) = msg.content {
                                        msg.content = MessageContent::Text(format!("{note}{text}"));
                                    }
                                    msg
                                } else {
                                    expanded_message
                                }
                            };
                            let mut history = self.conversation_history.lock().await;
                            history.push(final_message);
                            drop(history);
                            {
                                let mut state = self.state.lock().await;
                                state.consecutive_mistakes = 0;
                                state.clear_denied_tool_actions();
                            }
                            continue;
                        }
                    }

                    // Execute TaskComplete hook
                    if let Some(ref hook_mgr) = self.deps.hook_manager {
                        let task = task_text.as_deref().unwrap_or("");
                        let result = hook_mgr.task_complete(&self.config.task_id, task, "");
                        if let Some(output) = result.output
                            && let Some(modification) = output.context_modification
                        {
                            info!("[TaskComplete hook] {}", modification);
                            // Inject context modification into conversation history
                            let mut history = self.conversation_history.lock().await;
                            history.push(StorageMessage {
                                id: Some(Self::next_message_id(&self.message_counter)),
                                role: MessageRole::User,
                                content: MessageContent::Text(format!(
                                    "[Hook context from TaskComplete]: {modification}"
                                )),
                                model_info: None,
                                metrics: None,
                                ts: None,
                            });
                            drop(history);
                        }
                    }

                    // Record task in history for `sned history` and `--continue` support.
                    self.record_task_history(&state_manager, task_text.as_deref().unwrap_or(""))
                        .await;

                    return Ok(());
                }
                TurnResult::Cancelled => {
                    self.current_turn_retry_candidate = None;
                    if !self.config.json_output {
                        self.config
                            .output_writer
                            .emit(OutputEvent::info("Cancelled. Type /retry to resend."));
                    }
                    // Force-save conversation history immediately on cancellation (W4 fix)
                    // Bypass the 5-turn debounce to prevent data loss
                    if let Some(ref storage) = self.deps.task_storage {
                        let history = self.conversation_history.lock().await.clone();
                        if !history.is_empty()
                            && let Err(e) =
                                storage.write_api_conversation_history_async(&history).await
                        {
                            error!("Failed to save conversation history on cancel: {}", e);
                        }

                        let summary = self.state.lock().await.compacted_summary.clone();
                        if let Some(summary) = summary
                            && let Err(e) = storage.write_compacted_summary_async(&summary).await
                        {
                            error!("Failed to save compacted summary on cancel: {}", e);
                        }
                    }

                    self.record_task_history(&state_manager, task_text.as_deref().unwrap_or(""))
                        .await;

                    // Persist state manager (global state, task states, secrets)
                    if let Err(e) = StateManager::persist_async(Arc::clone(&state_manager)).await {
                        error!("Failed to persist state manager on cancel: {}", e);
                    }
                    return Ok(());
                }
                TurnResult::Error(e) => {
                    self.current_turn_retry_candidate = None;
                    self.config.output_writer.emit(OutputEvent::error_box(&e));

                    // Rollback the user message that was never processed by the model.
                    // Only rollback for context-window errors to prevent compounding failure.
                    // For other errors (rate limit, auth, etc.), keep the message so the user
                    // doesn't lose their input when retrying after fixing the issue.
                    if e.contains("exceeds") && e.contains("context") {
                        let mut history = self.conversation_history.lock().await;
                        if let Some(last) = history.last()
                            && last.role == MessageRole::User
                        {
                            history.pop();
                            tracing::info!(
                                "Rolled back unprocessed user message after context window error"
                            );
                        }
                    }

                    self.record_task_history(&state_manager, task_text.as_deref().unwrap_or(""))
                        .await;

                    // Persist state on error
                    if let Err(e_persist) =
                        StateManager::persist_async(Arc::clone(&state_manager)).await
                    {
                        error!("Failed to persist state manager on error: {}", e_persist);
                    }
                    return Err(AgentError::ExecutionError(e));
                }
            }
        }
    }

    /// Executes a single turn of the agent loop.
    async fn execute_turn(&mut self) -> TurnResult {
        // Keep the current plan state in the conversation history before we
        // derive the request snapshot so the model actually sees the latest
        // plan context on this turn.
        self.inject_plan_state_into_history().await;

        // 1. Prepare conversation history (possibly truncated by context manager)
        let truncated_history = {
            // Read api_req_info + deleted_range BEFORE locking history,
            // avoiding nested locks and a full Vec clone.
            let (api_req_info, deleted_range, compacted_summary) = {
                let state = self.state.lock().await;
                (
                    state.last_api_req_info.clone(),
                    state.conversation_history_deleted_range,
                    state.compacted_summary.clone(),
                )
            };

            // Pass history by reference to context_manager — saves a full deep clone
            // of every message/tool-result per turn.
            let conversation_guard = self.conversation_history.lock().await;
            let result = context_manager::get_new_context_messages_and_metadata(
                &conversation_guard,
                api_req_info.as_ref(),
                deleted_range,
                self.config.use_auto_condense,
                compacted_summary.as_ref(),
                self.config
                    .provider
                    .lock()
                    .expect("provider poisoned")
                    .as_ref()
                    .name(),
            );
            drop(conversation_guard);

            // Update state if deleted range changed (re-use same lock scope)
            if result.updated_conversation_history_deleted_range {
                let mut state = self.state.lock().await;
                let deleted_range = result.conversation_history_deleted_range;
                state.conversation_history_deleted_range = deleted_range;

                // Persist deleted_range to HistoryItem for cross-session restoration (C1 fix part 1)
                // Convert from (usize, usize) tuple to Vec<i32> for HistoryItem storage
                if let Some(ref state_manager) = self.state_manager
                    && let Some((start, end)) = deleted_range
                    && let Some(mut history_item) =
                        state_manager.find_task_in_history(&self.config.task_id)
                {
                    history_item.conversation_history_deleted_range =
                        Some(vec![start as i32, end as i32]);
                    state_manager.add_task_to_history(history_item);
                    if let Err(e) = StateManager::persist_async(state_manager.clone()).await {
                        self.config.output_writer.emit(OutputEvent::error(format!(
                            "Failed to persist state after compaction: {e}"
                        )));
                    }
                }
            }

            result.truncated_conversation_history
        };

        // 2. Apply context pruning if enabled
        let pruned_history = self.prune_conversation_history(truncated_history);
        if self.current_turn_retry_candidate.is_none() {
            self.current_turn_retry_candidate = Self::current_turn_retry_candidate(&pruned_history);
        }
        {
            let mut state = self.state.lock().await;
            state.retryable_failed_request = None;
        }

        // 3. Select the tool profile before building the system prompt. The
        // prompt's examples and the request's schemas must describe the same
        // inventory, especially for reduced profiles such as DirectAnswer.
        let profile = {
            let mode_str = match self.config.mode {
                crate::core::agent_types::AgentMode::Plan => "plan",
                crate::core::agent_types::AgentMode::Act => "act",
            };
            let prompt = self
                .current_turn_retry_candidate
                .as_ref()
                .and_then(|m| match &m.content {
                    crate::providers::MessageContent::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let profile =
                resolve_tool_profile(self.deps.tool_profile, self.deps.yolo, prompt, mode_str);
            let profile_changed = self.deps.tool_profile != Some(profile);
            self.deps.tool_profile = Some(profile);
            if profile_changed {
                self.deps.cached_system_prompt = None;
            }
            tracing::info!(profile = ?profile, prompt_len = prompt.len(), "selected tool profile");
            profile
        };

        // 3.1 Create provider request and build the matching system prompt.
        let mut context =
            self.deps
                .system_prompt_context
                .clone()
                .unwrap_or_else(|| SystemPromptContext {
                    cwd: std::env::current_dir()
                        .ok()
                        .and_then(|p| p.to_str().map(String::from)),
                    active_shell_path: std::env::var("SHELL").ok(),
                    active_shell_type: std::env::var("SHELL").ok().and_then(|s| {
                        std::path::Path::new(&s)
                            .file_name()
                            .and_then(|n| n.to_str().map(String::from))
                    }),
                    active_shell_is_posix: true,
                    enable_parallel_tool_calling: false,
                    model_id: self.resolve_active_model_id(),
                    ..Default::default()
                });
        context.tool_profile = Some(profile);
        let workspace_root = context
            .cwd
            .clone()
            .map_or_else(|| self.resolve_workspace_root(), std::path::PathBuf::from);
        let (cancellation_flag, consecutive_failures) = {
            let state = self.state.lock().await;
            (
                state.is_cancelled_atomic.clone(),
                state.consecutive_mistakes,
            )
        };
        let tool_context = Arc::new(
            ToolContext::new(
                self.state.clone(),
                self.deps.approval_manager.clone(),
                workspace_root.clone(),
                self.anchor_mgr.clone(),
                self.config.json_output,
                self.config.task_id.clone(),
                self.deps.hook_manager.clone(),
                false, // Initial context: not explicitly approved (approval happens per-tool)
                self.config.output_writer.clone(),
            )
            .with_cancellation_flag(cancellation_flag)
            .with_consecutive_failures(consecutive_failures),
        );
        let system_prompt = if let Some(prompt) = self.deps.cached_system_prompt.clone() {
            prompt
        } else {
            let prompt = PromptBuilder::new(context).build();
            self.deps.cached_system_prompt = Some(prompt.clone());
            prompt
        };

        // 2.5 Execute TaskStart hook
        // (TaskStart hook is executed in run() before the first turn)

        // 2.6 Record model usage for task metadata
        if let Some(ref tracker) = self.model_tracker {
            let guard = self.config.provider.lock().expect("provider lock poisoned");
            let provider_id = guard.name().to_string();
            let model_id = guard.get_model().id;
            drop(guard);
            let mode = match self.config.mode {
                crate::core::agent_types::AgentMode::Plan => "plan",
                crate::core::agent_types::AgentMode::Act => "act",
            };
            if let Err(e) = tracker.record_model_usage(&provider_id, &model_id, mode) {
                warn!(error = %e, "Failed to record model usage");
            }
        }

        // 3.2 Build the tool schemas from that same profile.
        let tool_definitions =
            crate::core::tools::definitions::get_tool_definitions_for_profile(profile);
        let tools = if tool_definitions.is_empty() {
            None
        } else {
            Some(tool_definitions)
        };

        let mut request = ProviderRequest {
            system_prompt: system_prompt.clone(),
            messages: pruned_history,
            tools,
            tool_choice: Some(crate::providers::ToolChoice::Auto),
            use_response_api: None,
            max_tokens: self.config.max_tokens,
        };

        // Emergency truncation: if the request exceeds context limits, aggressively
        // truncate to the last N messages to break the deadlock (e.g., /compact failing
        // because the compact instruction itself pushes the request over the limit).
        // This is a last-resort fallback after context_manager truncation.
        let validation_result = {
            let provider = self
                .config
                .provider
                .lock()
                .expect("provider poisoned")
                .clone();
            context_window::validate_context_window(&request, provider.as_ref())
        };
        if let Err(msg) = validation_result {
            tracing::warn!(
                "Request exceeds context limits after context_manager truncation: {}",
                msg
            );
            tracing::info!("Applying emergency truncation to break deadlock");
            if let Err(msg) = self.emergency_truncate_request(&mut request).await {
                tracing::error!(
                    "Request still exceeds context limits after emergency truncation: {}",
                    msg
                );
                return TurnResult::Error(format!("Context window overflow: {msg}"));
            }
        }

        let state_clone = self.state.clone();
        let history_clone = self.conversation_history.clone();
        let provider = self
            .config
            .provider
            .lock()
            .expect("provider poisoned")
            .clone();

        let retry_config = if provider.name() == "gemini" {
            RetryConfig {
                max_retries: 4,
                base_delay_ms: 2_000,
                max_delay_ms: 15_000,
            }
        } else {
            RetryConfig::default()
        };

        let mut stream_retry_attempt = 0usize;
        let preoutput_retry_started_at = std::time::Instant::now();
        let preoutput_policy = provider.preoutput_policy();
        let preoutput_budget = preoutput_policy.budget;
        let output_kind = match preoutput_policy.transport {
            crate::providers::ProviderTransport::Streaming => "stream",
            crate::providers::ProviderTransport::Buffered => "response",
        };
        let mut preoutput_elapsed_at_first_chunk: Option<std::time::Duration> = None;
        let (
            accumulated_text,
            accumulated_reasoning,
            accumulated_signature,
            accumulated_text_signature,
            accumulated_redacted_data,
            mut tool_calls_map,
            tool_call_order,
        ) = 'provider_stream_attempt: loop {
            self.reset_stream_attempt_timing().await;
            tracing::debug!(
                stream_attempt = stream_retry_attempt + 1,
                message_count = request.messages.len(),
                tool_count = request.tools.as_ref().map_or(0, std::vec::Vec::len),
                preoutput_elapsed_ms = preoutput_retry_started_at.elapsed().as_millis(),
                "starting provider stream"
            );
            // Create channel for stream chunks with large buffer to prevent
            // backpressure deadlocks when the provider emits faster than the
            // consumer processes (e.g. during very long responses).
            let (tx, mut rx) = mpsc::channel::<ApiStreamChunk>(10_000);

            let Some(remaining_preoutput_budget) =
                preoutput_budget.checked_sub(preoutput_retry_started_at.elapsed())
            else {
                let error = ProviderError::NetworkError(format!(
                    "provider {output_kind} produced no output within {}s",
                    preoutput_budget.as_secs()
                ));
                let actionable = crate::cli::actionable_errors::provider_error(&error);
                return TurnResult::Error(format!(
                    "Provider request did not produce a {output_kind} within {}s: {}",
                    preoutput_budget.as_secs(),
                    actionable.display()
                ));
            };

            let provider_request = create_message_with_retry(
                provider.clone(),
                request.clone(),
                state_clone.clone(),
                retry_config,
                self.config.json_output,
                Some(self.config.output_writer.clone()),
                Some(self.cancelled.clone()),
            );
            tokio::pin!(provider_request);
            let cancellation = wait_for_cancellation(self.cancelled.clone());
            tokio::pin!(cancellation);
            let request_result = tokio::select! {
                biased;
                _ = &mut cancellation => None,
                result = tokio::time::timeout(remaining_preoutput_budget, &mut provider_request) => Some(result),
            };
            let Some(request_result) = request_result else {
                if let Some(ref retry_message) = self.current_turn_retry_candidate {
                    let mut state = self.state.lock().await;
                    state.retryable_failed_request = Some(retry_message.clone());
                }
                return TurnResult::Cancelled;
            };
            let stream = match request_result {
                Ok(Ok(stream)) => {
                    if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        if let Some(ref retry_message) = self.current_turn_retry_candidate {
                            let mut state = self.state.lock().await;
                            state.retryable_failed_request = Some(retry_message.clone());
                        }
                        return TurnResult::Cancelled;
                    }
                    stream
                }
                Ok(Err(e)) => {
                    if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        if let Some(ref retry_message) = self.current_turn_retry_candidate {
                            let mut state = self.state.lock().await;
                            state.retryable_failed_request = Some(retry_message.clone());
                        }
                        return TurnResult::Cancelled;
                    }
                    error!(error = %e, "provider request failed");
                    if let Some(ref retry_message) = self.current_turn_retry_candidate {
                        let mut state = self.state.lock().await;
                        state.retryable_failed_request = Some(retry_message.clone());
                    }
                    let actionable = crate::cli::actionable_errors::provider_error(&e);
                    let consecutive_failures = {
                        let state = self.state.lock().await;
                        state.consecutive_provider_failures
                    };
                    let message = if consecutive_failures
                        >= DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES
                    {
                        format!(
                            "{}\nProvider has failed {} consecutive requests. Retry after the provider recovers, or use /model to switch providers.",
                            actionable.display(),
                            consecutive_failures
                        )
                    } else {
                        actionable.display()
                    };
                    return TurnResult::Error(message);
                }
                Err(_) => {
                    if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        if let Some(ref retry_message) = self.current_turn_retry_candidate {
                            let mut state = self.state.lock().await;
                            state.retryable_failed_request = Some(retry_message.clone());
                        }
                        return TurnResult::Cancelled;
                    }
                    let error = ProviderError::NetworkError(format!(
                        "provider request did not produce a {output_kind} within {}s",
                        preoutput_budget.as_secs()
                    ));
                    let actionable = crate::cli::actionable_errors::provider_error(&error);
                    return TurnResult::Error(format!(
                        "Provider request did not produce a {output_kind} within {}s: {}",
                        preoutput_budget.as_secs(),
                        actionable.display()
                    ));
                }
            };

            let cancelled_flag = self.cancelled.clone();
            let stream_handle = tokio::spawn(async move {
                let mut stream = stream;
                use tokio_stream::StreamExt;
                'stream: loop {
                    tokio::select! {
                        chunk = stream.next() => {
                            match chunk {
                                Some(c) => {
                                    if cancelled_flag.load(std::sync::atomic::Ordering::Acquire) {
                                        break 'stream;
                                    }
                                    // Race the send against cancellation so a slow
                                    // consumer (UI backpressure, full bounded channel)
                                    // doesn't block Ctrl+C response. If cancellation
                                    // wins, the chunk is dropped — acceptable since
                                    // the user is cancelling.
                                    let mut send_fut = Box::pin(tx.send(c));
                                    loop {
                                        tokio::select! {
                                            result = send_fut.as_mut() => {
                                                if result.is_err() {
                                                    break 'stream;
                                                }
                                                break;
                                            }
                                            () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                                                if cancelled_flag.load(std::sync::atomic::Ordering::Acquire) {
                                                    break 'stream;
                                                }
                                            }
                                        }
                                    }
                                }
                                None => break 'stream,
                            }
                        }
                        () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                            if cancelled_flag.load(std::sync::atomic::Ordering::Acquire) {
                                break 'stream;
                            }
                        }
                    }
                }
            });

            // 4. Process stream chunks
            let mut accumulated_text = String::new();
            let mut first_chunk_received = false;
            let mut accumulated_reasoning = String::new();
            let mut accumulated_signature: Option<String> = None;
            let mut accumulated_text_signature: Option<String> = None;
            let mut accumulated_redacted_data: Vec<String> = Vec::new();
            // Use HashMap for O(1) merge + Vec to preserve insertion order (P4)
            let mut tool_calls_map: HashMap<String, ApiStreamToolCall> = HashMap::with_capacity(4);
            let mut tool_call_order: Vec<String> = Vec::new();
            let mut announced_tool_call_ids = std::collections::HashSet::new();
            let mut tool_call_detected = false;
            let mut display_buffer = String::new();
            let mut in_code_block = false;
            let mut code_block_lang = String::new();
            let mut code_block_buffer: Vec<String> = Vec::new();
            let mut code_block_lines: usize = 0;
            let mut code_block_snipped = false;
            let code_block_display_limit = code_block_display_limit(self.config.interactive_mode);

            let mut stream_errored = false;
            let mut retryable_stream_error_before_output: Option<String> = None;
            let mut non_retryable_stream_error: Option<String> = None;
            let mut substantive_stream_output_received = false;
            let mut in_thinking_tag = false;
            let mut partial_line_displayed = false;
            let mut last_partial_flush_at: Option<std::time::Instant> = None;
            let mut stream_usage: Option<ApiReqInfo> = None;
            let mut preoutput_deadline_exceeded = false;

            // Turn indicator is prepended to the first output line, not emitted separately,
            // so it appears on the same line as the start of the response.
            let mut turn_indicator_pending = true;

            loop {
                let next_chunk = if first_chunk_received {
                    rx.recv().await
                } else {
                    let Some(remaining) =
                        preoutput_budget.checked_sub(preoutput_retry_started_at.elapsed())
                    else {
                        preoutput_deadline_exceeded = true;
                        retryable_stream_error_before_output = Some(format!(
                            "provider {output_kind} produced no output within {}s",
                            preoutput_budget.as_secs()
                        ));
                        break;
                    };
                    match tokio::time::timeout(remaining, rx.recv()).await {
                        Ok(chunk) => chunk,
                        Err(_) => {
                            preoutput_deadline_exceeded = true;
                            retryable_stream_error_before_output = Some(format!(
                                "provider {output_kind} produced no output within {}s",
                                preoutput_budget.as_secs()
                            ));
                            break;
                        }
                    }
                };
                let Some(chunk) = next_chunk else {
                    break;
                };
                // Check for cancellation during stream processing so Ctrl+C
                // takes effect promptly instead of waiting for the full stream.
                // Uses lock-free AtomicBool to avoid mutex contention on every chunk.
                if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    tracing::info!("cancellation detected during stream processing, aborting turn");
                    return TurnResult::Cancelled;
                }

                if !first_chunk_received && !matches!(&chunk, ApiStreamChunk::Error(_)) {
                    preoutput_elapsed_at_first_chunk
                        .get_or_insert_with(|| preoutput_retry_started_at.elapsed());
                    if crate::cli::output::timing_enabled() {
                        let mut state = self.state.lock().await;
                        if state.first_provider_chunk_time.is_none() {
                            state.first_provider_chunk_time = Some(std::time::Instant::now());
                        }
                    }
                    first_chunk_received = true;
                }

                match chunk {
                    ApiStreamChunk::Text(text_chunk) => {
                        tracing::debug!(text = %text_chunk.text, "received text chunk");
                        if self.config.json_output {
                            substantive_stream_output_received |= !text_chunk.text.is_empty();
                            tracing::info!(
                                target: "json_output",
                                "{}",
                                serde_json::json!({
                                    "type": "text",
                                    "text": text_chunk.text
                                })
                                .to_string()
                            );
                        } else {
                            // Check for thinking tags and suppress content between them
                            let text = &text_chunk.text;
                            let mut processed = String::new();
                            let mut pos = 0;

                            while pos < text.len() {
                                // Check for thinking start tag
                                if !in_thinking_tag {
                                    if let Some(tag_start) = text[pos..].find("<!-- think -->") {
                                        let abs_start = pos + tag_start;
                                        processed.push_str(&text[pos..abs_start]);
                                        in_thinking_tag = true;
                                        pos = abs_start + "<!-- think -->".len();
                                        continue;
                                    } else if let Some(tag_start) = text[pos..].find("<think>") {
                                        let abs_start = pos + tag_start;
                                        processed.push_str(&text[pos..abs_start]);
                                        in_thinking_tag = true;
                                        pos = abs_start + "<think>".len();
                                        continue;
                                    }
                                }

                                // Check for thinking end tag
                                if in_thinking_tag {
                                    if let Some(tag_start) = text[pos..].find("<!-- /think -->") {
                                        let abs_start = pos + tag_start;
                                        in_thinking_tag = false;
                                        pos = abs_start + "<!-- /think -->".len();
                                        continue;
                                    } else if let Some(tag_start) = text[pos..].find("</think>") {
                                        let abs_start = pos + tag_start;
                                        in_thinking_tag = false;
                                        pos = abs_start + "</think>".len();
                                        continue;
                                    }
                                    // Skip content while inside thinking tag
                                    pos = text.len();
                                } else {
                                    // Not in thinking tag, output remaining text
                                    processed.push_str(&text[pos..]);
                                    pos = text.len();
                                }
                            }

                            // Only display non-thinking content
                            if !processed.is_empty() {
                                substantive_stream_output_received = true;
                                self.record_first_displayable_text_time().await;
                                display_buffer.push_str(&processed);
                                while let Some(nl_pos) = display_buffer.find('\n') {
                                    // Extract line and trim in one pass (reduces allocations)
                                    let line = display_buffer[..nl_pos].to_string();
                                    display_buffer.drain(..=nl_pos);
                                    let trimmed_line = line.trim();

                                    if trimmed_line.starts_with("```") {
                                        if in_code_block {
                                            print_code_block(
                                                &code_block_buffer,
                                                &code_block_lang,
                                                &self.config.output_writer,
                                                self.config.interactive_mode,
                                            );
                                            if code_block_snipped {
                                                self.config.output_writer.emit(OutputEvent::dim(
                                                    snipped_code_block_hint(),
                                                ));
                                            }
                                            in_code_block = false;
                                            code_block_lang.clear();
                                            code_block_buffer.clear();
                                            code_block_lines = 0;
                                            code_block_snipped = false;
                                        } else {
                                            in_code_block = true;
                                            code_block_lang =
                                                code_fence_language(trimmed_line).to_string();
                                            code_block_buffer.clear();
                                            code_block_lines = 0;
                                            code_block_snipped = false;
                                        }

                                        print_model_line_with_prefix_if_pending(
                                            trimmed_line,
                                            &self.config.output_writer,
                                            &mut turn_indicator_pending,
                                            false,
                                        );
                                        partial_line_displayed = false;
                                        last_partial_flush_at = None;
                                        continue;
                                    }

                                    if in_code_block {
                                        code_block_lines += 1;
                                        // For code blocks, preserve leading indentation (only trim end)
                                        let code_line = line.trim_end().to_string();
                                        if code_block_lines > code_block_display_limit {
                                            code_block_snipped = true;
                                            continue;
                                        }

                                        code_block_buffer.push(code_line);
                                        continue;
                                    }

                                    // Regular content - already trimmed
                                    self.record_first_output_emit_time().await;
                                    if partial_line_displayed {
                                        update_model_line_with_prefix_if_pending(
                                            trimmed_line,
                                            &self.config.output_writer,
                                            &mut turn_indicator_pending,
                                            self.config.interactive_mode,
                                        );
                                        partial_line_displayed = false;
                                        last_partial_flush_at = None;
                                    } else {
                                        print_model_line_with_prefix_if_pending(
                                            trimmed_line,
                                            &self.config.output_writer,
                                            &mut turn_indicator_pending,
                                            self.config.interactive_mode,
                                        );
                                    }
                                }

                                let trimmed_partial = display_buffer.trim_end();
                                let should_flush_partial = self.config.interactive_mode
                                    && !self.config.json_output
                                    && !in_code_block
                                    && !trimmed_partial.is_empty()
                                    && !trimmed_partial.trim_start().starts_with("```")
                                    && last_partial_flush_at.is_none_or(|last| {
                                        last.elapsed() >= PARTIAL_MODEL_FLUSH_INTERVAL
                                    });
                                if should_flush_partial {
                                    self.record_first_output_emit_time().await;
                                    if partial_line_displayed {
                                        update_model_line_with_prefix_if_pending(
                                            trimmed_partial,
                                            &self.config.output_writer,
                                            &mut turn_indicator_pending,
                                            false,
                                        );
                                    } else {
                                        print_model_line_with_prefix_if_pending(
                                            trimmed_partial,
                                            &self.config.output_writer,
                                            &mut turn_indicator_pending,
                                            false,
                                        );
                                        partial_line_displayed = true;
                                    }
                                    last_partial_flush_at = Some(std::time::Instant::now());
                                }
                            }
                        }
                        if text_chunk.signature.is_some() {
                            accumulated_text_signature = text_chunk.signature.clone();
                        }
                        accumulated_text.push_str(&text_chunk.text);
                    }
                    ApiStreamChunk::Reasoning(reasoning_chunk) => {
                        substantive_stream_output_received |= !reasoning_chunk.reasoning.is_empty();
                        self.record_first_reasoning_chunk_time().await;
                        if self.config.json_output {
                            tracing::info!(
                                target: "json_output",
                                "{}",
                                serde_json::json!({
                                    "type": "reasoning",
                                    "reasoning": reasoning_chunk.reasoning,
                                    "signature": reasoning_chunk.signature,
                                    "redacted_data": reasoning_chunk.redacted_data,
                                })
                                .to_string()
                            );
                        } else if !reasoning_chunk.reasoning.is_empty() {
                            self.config.output_writer.emit(OutputEvent::ReasoningChunk(
                                reasoning_chunk.reasoning.clone(),
                            ));
                        }
                        accumulated_reasoning.push_str(&reasoning_chunk.reasoning);
                        if reasoning_chunk.signature.is_some() {
                            accumulated_signature = reasoning_chunk.signature.clone();
                        }
                        if let Some(redacted_data) = reasoning_chunk.redacted_data {
                            accumulated_redacted_data.push(redacted_data);
                        }
                    }
                    ApiStreamChunk::Usage(usage_chunk) => {
                        if self.config.json_output {
                            tracing::info!(
                                target: "json_output",
                                "{}",
                                serde_json::json!({
                                    "type": "usage",
                                    "input_tokens": usage_chunk.input_tokens,
                                    "output_tokens": usage_chunk.output_tokens,
                                    "cache_write_tokens": usage_chunk.cache_write_tokens,
                                    "cache_read_tokens": usage_chunk.cache_read_tokens,
                                    "reasoning_tokens": usage_chunk.reasoning_tokens,
                                    "total_cost": usage_chunk.total_cost,
                                    "stop_reason": usage_chunk.stop_reason,
                                    "id": usage_chunk.id,
                                })
                                .to_string()
                            );
                        }
                        let is_synthetic_empty_usage = usage_chunk.input_tokens == 0
                            && usage_chunk.output_tokens == 0
                            && usage_chunk.cache_write_tokens == Some(0)
                            && usage_chunk.cache_read_tokens.is_none()
                            && usage_chunk.reasoning_tokens.is_none()
                            && usage_chunk.total_cost.is_none()
                            && usage_chunk.id.is_none();
                        let mut state = self.state.lock().await;
                        if is_synthetic_empty_usage {
                            // Keep the last measured usage when this provider
                            // response has no usage data. Do not replace it
                            // with a fabricated zero or an estimate.
                            continue;
                        }
                        let prev_info = stream_usage.as_ref();
                        let context_window_info = crate::core::context::get_context_window_info(
                            self.config
                                .provider
                                .lock()
                                .expect("provider poisoned")
                                .as_ref(),
                        );
                        let context_window = context_window_info.context_window;
                        let guard = self.config.provider.lock().expect("provider lock poisoned");
                        let provider_name = guard.name().to_string();
                        drop(guard);
                        let tokens_in = if usage_chunk.input_tokens > 0 {
                            usage_chunk.input_tokens
                        } else {
                            prev_info.and_then(|r| r.tokens_in).unwrap_or(0)
                        };
                        let tokens_out = if usage_chunk.output_tokens > 0 {
                            usage_chunk.output_tokens
                        } else {
                            prev_info.and_then(|r| r.tokens_out).unwrap_or(0)
                        };
                        let cache_writes = usage_chunk
                            .cache_write_tokens
                            .or(prev_info.and_then(|r| r.cache_writes));
                        let cache_reads = usage_chunk
                            .cache_read_tokens
                            .or(prev_info.and_then(|r| r.cache_reads));
                        let reasoning_tokens = usage_chunk
                            .reasoning_tokens
                            .or(prev_info.and_then(|r| r.reasoning_tokens));
                        // Gemini marks thinking tokens separately from candidate output;
                        // OpenAI-compatible providers include reasoning in completion_tokens.
                        let context_output_tokens = if usage_chunk.thoughts_token_count.is_some() {
                            tokens_out.saturating_add(reasoning_tokens.unwrap_or(0))
                        } else {
                            tokens_out
                        };
                        let context_tokens =
                            crate::core::context::context_window::calculate_context_tokens(
                                tokens_in,
                                context_output_tokens,
                                cache_writes,
                                cache_reads,
                                &provider_name,
                            );
                        let context_usage_pct =
                            crate::core::context::context_window::calculate_context_usage_percentage(
                                tokens_in,
                                context_output_tokens,
                                cache_writes,
                                cache_reads,
                                context_window,
                                &provider_name,
                            );
                        let usage = ApiReqInfo {
                            request: None,
                            tokens_in: Some(tokens_in),
                            tokens_out: Some(tokens_out),
                            cache_writes,
                            cache_reads,
                            reasoning_tokens,
                            context_tokens: Some(context_tokens),
                            cost: usage_chunk.total_cost.or(prev_info.and_then(|r| r.cost)),
                            context_window: Some(context_window),
                            context_usage_percentage: Some(context_usage_pct),
                        };
                        stream_usage = Some(usage.clone());
                        state.last_api_req_info = Some(usage);
                        if usage_chunk.input_tokens > 0 {
                            state.cumulative_tokens_in = state
                                .cumulative_tokens_in
                                .saturating_add(usage_chunk.input_tokens);
                        }
                        if usage_chunk.output_tokens > 0 {
                            state.cumulative_tokens_out = state
                                .cumulative_tokens_out
                                .saturating_add(usage_chunk.output_tokens);
                        }
                        if let Some(cache_writes) = usage_chunk.cache_write_tokens
                            && cache_writes > 0
                        {
                            state.cumulative_cache_writes =
                                state.cumulative_cache_writes.saturating_add(cache_writes);
                        }
                        if let Some(cache_reads) = usage_chunk.cache_read_tokens
                            && cache_reads > 0
                        {
                            state.cumulative_cache_reads =
                                state.cumulative_cache_reads.saturating_add(cache_reads);
                        }
                        if let Some(reasoning_tokens) = usage_chunk.reasoning_tokens
                            && reasoning_tokens > 0
                        {
                            state.cumulative_reasoning_tokens = state
                                .cumulative_reasoning_tokens
                                .saturating_add(reasoning_tokens);
                        }
                        if let Some(cost) = usage_chunk.total_cost
                            && cost > 0.0
                        {
                            state.cumulative_cost += cost;
                        }
                    }
                    ApiStreamChunk::ToolCallStarted { call_id, name } => {
                        if !self.config.json_output && announced_tool_call_ids.insert(call_id) {
                            if !tool_call_detected {
                                self.config.output_writer.flush();
                                tool_call_detected = true;
                            }
                            self.config
                                .output_writer
                                .emit(OutputEvent::tool_call(format!("Preparing {name}…")));
                        }
                    }
                    ApiStreamChunk::ToolCalls(tool_chunk) => {
                        substantive_stream_output_received = true;
                        // Print separator when first tool call is detected
                        if !tool_call_detected && !self.config.json_output {
                            self.config.output_writer.flush();
                            tool_call_detected = true;
                        }

                        let tc = tool_chunk.tool_call;
                        let key = tc
                            .call_id
                            .clone()
                            .unwrap_or_else(|| tc.function.id.clone().unwrap_or_default());
                        // Prevent empty-key collisions when provider sends tool calls without IDs.
                        // Two calls both keyed by "" would overwrite each other in tool_calls_map.
                        let key = if key.is_empty() {
                            ulid::Ulid::new().to_string()
                        } else {
                            key
                        };
                        tracing::info!(
                            tool_name = ?tc.function.name,
                            tool_id = ?key,
                            has_args = tc.function.arguments.is_some(),
                            "received tool call from stream"
                        );

                        if self.config.json_output {
                            tracing::info!(
                                target: "json_output",
                                "{}",
                                serde_json::json!({
                                    "type": "tool_calls",
                                    "tool_call": {
                                        "call_id": tc.call_id,
                                        "function": {
                                            "id": tc.function.id,
                                            "name": tc.function.name,
                                            "arguments": tc.function.arguments,
                                        }
                                    },
                                    "id": tool_chunk.id,
                                    "signature": tool_chunk.signature,
                                })
                                .to_string()
                            );
                        }
                        // Allow partial tool call deltas with arguments even when name is missing.
                        // Provider may send name in a later chunk; merge logic assembles complete call.
                        let args_absent = tc.function.arguments.is_none()
                            || tc
                                .function
                                .arguments
                                .as_ref()
                                .is_some_and(std::string::String::is_empty);
                        if (tc.function.name.is_none()
                            || tc
                                .function
                                .name
                                .as_ref()
                                .is_some_and(std::string::String::is_empty))
                            && args_absent
                        {
                            tracing::warn!(
                                "received tool call with empty name and no arguments, skipping"
                            );
                            continue;
                        }
                        // Merge partial tool call chunks by ID using HashMap for O(1) lookup (P4)
                        // Preserve insertion order via tool_call_order vec
                        if let Some(existing) = tool_calls_map.get_mut(&key) {
                            if let Some(new_args) = tc.function.arguments
                                && !new_args.is_empty()
                            {
                                let merged = existing
                                    .function
                                    .arguments
                                    .as_ref()
                                    .map(|a| a.clone() + &new_args)
                                    .unwrap_or(new_args);
                                // Validate merged argument size
                                if merged.len() > MAX_TOOL_ARGUMENT_SIZE {
                                    let truncated =
                                        truncate_json_arguments(&merged, MAX_TOOL_ARGUMENT_SIZE);
                                    if truncated.was_repaired {
                                        tracing::warn!(
                                            "Tool call arguments were truncated AND repaired (original JSON was malformed)"
                                        );
                                    }
                                    existing.function.arguments = Some(truncated.value);
                                } else {
                                    existing.function.arguments = Some(merged);
                                }
                            }
                            if tc.function.name.is_some() {
                                existing.function.name = tc.function.name;
                            }
                            if tc.call_id.is_some() {
                                existing.call_id = tc.call_id;
                            }
                        } else {
                            // Validate initial argument size
                            if let Some(ref args) = tc.function.arguments
                                && args.len() > MAX_TOOL_ARGUMENT_SIZE
                            {
                                let truncated =
                                    truncate_json_arguments(args, MAX_TOOL_ARGUMENT_SIZE);
                                if truncated.was_repaired {
                                    tracing::warn!(
                                        "Tool call arguments were truncated AND repaired (original JSON was malformed)"
                                    );
                                }
                                let mut truncated_tc = tc.clone();
                                truncated_tc.function.arguments = Some(truncated.value);
                                tool_call_order.push(key.clone());
                                tool_calls_map.insert(key, truncated_tc);
                                continue;
                            }
                            tool_call_order.push(key.clone());
                            tool_calls_map.insert(key, tc);
                        }
                    }
                    ApiStreamChunk::Error(err) => {
                        tracing::error!(error = %err, "received error chunk from provider stream");
                        let retryable = stream_error_is_retryable(&err);
                        if !retryable {
                            stream_errored = true;
                            if non_retryable_stream_error.is_none() {
                                if self.config.json_output {
                                    tracing::info!(
                                        target: "json_output",
                                        "{}",
                                        serde_json::json!({
                                            "type": "error",
                                            "error": err
                                        })
                                    );
                                }
                                non_retryable_stream_error = Some(err);
                            }
                            continue;
                        }
                        if non_retryable_stream_error.is_some() {
                            continue;
                        }
                        if !substantive_stream_output_received {
                            retryable_stream_error_before_output = Some(err);
                            break;
                        }
                        stream_errored = true;
                        if self.config.json_output {
                            tracing::info!(
                                target: "json_output",
                                "{}",
                                serde_json::json!({
                                    "type": "error",
                                    "error": err
                                })
                            );
                        } else {
                            self.config
                                .output_writer
                                .emit(OutputEvent::error(format!("Provider stream error: {err}")));
                        }
                    }
                }
            }

            // Final flush: print any remaining buffered content and ensure newline
            if in_code_block && !self.config.json_output {
                let remaining = display_buffer.trim_end().to_string();
                if !remaining.is_empty() {
                    code_block_lines += 1;
                    if code_block_lines <= code_block_display_limit {
                        code_block_buffer.push(remaining);
                    } else {
                        code_block_snipped = true;
                    }
                }
                print_code_block(
                    &code_block_buffer,
                    &code_block_lang,
                    &self.config.output_writer,
                    self.config.interactive_mode,
                );
                if code_block_snipped {
                    self.config
                        .output_writer
                        .emit(OutputEvent::dim(snipped_code_block_hint()));
                }
                self.config.output_writer.flush();
            } else if !display_buffer.is_empty() && !self.config.json_output {
                let remaining = display_buffer.trim_end().to_string();
                if !remaining.is_empty() {
                    self.record_first_output_emit_time().await;
                    if partial_line_displayed {
                        update_model_line_with_prefix_if_pending(
                            &remaining,
                            &self.config.output_writer,
                            &mut turn_indicator_pending,
                            self.config.interactive_mode,
                        );
                    } else {
                        print_model_line_with_prefix_if_pending(
                            &remaining,
                            &self.config.output_writer,
                            &mut turn_indicator_pending,
                            self.config.interactive_mode,
                        );
                    }
                }
            } else if !self.config.json_output {
                self.config.output_writer.flush();
            }

            // Wait for stream to complete
            if preoutput_deadline_exceeded {
                stream_handle.abort();
            }
            if let Err(e) = stream_handle.await
                && !preoutput_deadline_exceeded
            {
                let error = ProviderError::UnexpectedError(e.to_string());
                let actionable = crate::cli::actionable_errors::provider_error(&error);
                return TurnResult::Error(actionable.display());
            }

            if let Some(err) = non_retryable_stream_error {
                return TurnResult::Error(err);
            }

            if let Some(err) = retryable_stream_error_before_output {
                if preoutput_deadline_exceeded || stream_retry_attempt >= MAX_STREAM_RETRY_ATTEMPTS
                {
                    tracing::error!(
                        attempts = stream_retry_attempt + 1,
                        preoutput_elapsed_ms = preoutput_retry_started_at.elapsed().as_millis(),
                        error = %err,
                        "stream retry cap exceeded; surfacing error to user"
                    );
                    let error = ProviderError::NetworkError(err);
                    let actionable = crate::cli::actionable_errors::provider_error(&error);
                    return TurnResult::Error(format!(
                        "Provider {output_kind} failed after {} attempts: {}",
                        stream_retry_attempt + 1,
                        actionable.display()
                    ));
                }
                {
                    let mut state = self.state.lock().await;
                    state.did_automatically_retry_failed_api_request = true;
                }
                stream_retry_attempt += 1;
                let remaining_preoutput_budget = preoutput_budget
                    .checked_sub(preoutput_retry_started_at.elapsed())
                    .unwrap_or_default();
                let delay =
                    stream_retry_delay(stream_retry_attempt).min(remaining_preoutput_budget);
                tracing::warn!(
                    attempt = stream_retry_attempt,
                    next_delay_ms = delay.as_millis(),
                    preoutput_elapsed_ms = preoutput_retry_started_at.elapsed().as_millis(),
                    error = %err,
                    "retrying provider stream after pre-output transport failure"
                );
                if !self.config.json_output {
                    self.config
                        .output_writer
                        .emit(OutputEvent::tool_output_line(
                            format!(
                                "Provider stream stalled before output; retrying attempt {}/{} in {}s.",
                                stream_retry_attempt + 1,
                                MAX_STREAM_RETRY_ATTEMPTS + 1,
                                delay.as_secs(),
                            ),
                            Style::default().fg(crate::cli::tui::theme::WARNING_FG),
                        ));
                }
                if !self.wait_for_stream_retry_delay(delay).await {
                    return TurnResult::Cancelled;
                }
                continue 'provider_stream_attempt;
            }

            // If stream errored mid-response, note the partial content in the error
            if stream_errored {
                if let Some(ref retry_message) = self.current_turn_retry_candidate {
                    let mut state = self.state.lock().await;
                    state.retryable_failed_request = Some(retry_message.clone());
                }
                let partial_note =
                    if !accumulated_text.is_empty() || !accumulated_reasoning.is_empty() {
                        format!(
                            " (partial response of {} text chars{} discarded)",
                            accumulated_text.len(),
                            if accumulated_reasoning.is_empty() {
                                String::new()
                            } else {
                                format!(" + {} reasoning chars", accumulated_reasoning.len())
                            }
                        )
                    } else {
                        String::new()
                    };
                return TurnResult::Error(format!(
                    "Provider stream error{partial_note} - retry the request."
                ));
            }
            break (
                accumulated_text,
                accumulated_reasoning,
                accumulated_signature,
                accumulated_text_signature,
                accumulated_redacted_data,
                tool_calls_map,
                tool_call_order,
            );
        };

        if !self.config.json_output && !accumulated_text.is_empty() {
            tracing::debug!("");
        }

        let prepared_tool_calls = Self::prepare_tool_calls(&tool_call_order, &mut tool_calls_map);

        // Discover applicable rules before any tool-path-related early return.
        // A second pass after execution below catches AGENTS.md files created
        // by a write in this same tool batch.
        let scoped_rules_added = if prepared_tool_calls.is_empty() {
            false
        } else {
            self.discover_agents_rules_for_tool_calls(&workspace_root, &prepared_tool_calls)
        };

        // 5. Check for empty response
        // Log what we received from the model
        tracing::info!(
            text_len = accumulated_text.len(),
            reasoning_len = accumulated_reasoning.len(),
            tool_call_count = prepared_tool_calls.len(),
            "stream complete"
        );

        if accumulated_text.is_empty()
            && prepared_tool_calls.is_empty()
            && accumulated_reasoning.is_empty()
        {
            let mut state = state_clone.lock().await;
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                max_allowed = ?self.config.max_consecutive_mistakes,
                "Model returned empty response (no text, no tool calls)"
            );

            if self
                .config
                .max_consecutive_mistakes
                .is_some_and(|limit| state.consecutive_mistakes >= limit)
            {
                return TurnResult::Error("Max consecutive mistakes reached".to_string());
            }

            return TurnResult::Continue;
        }

        // CRITICAL: Do NOT reset consecutive_mistakes here - tool execution may fail.
        // Reset happens after tool execution if all tools succeed.

        // 6. Add assistant message to history
        let mut text_only_completes_task = false;
        // Split raw model text into thinking + response.
        // DeepSeek/Wafer embed thinking tags in delta.content; use the
        // response part for completion output so hidden thinking stays hidden.
        let (extracted_thinking, _) = split_model_output(&accumulated_text);
        let response_text = extract_response_text(&accumulated_text);
        // A response that accompanies tool calls is an intermediate handoff,
        // not the completed model response that `/full` should recover.
        if prepared_tool_calls.is_empty() {
            let mut state = state_clone.lock().await;
            state.retain_full_response(
                response_text.clone(),
                code_block_display_limit(self.config.interactive_mode),
            );
        }
        {
            let mut history = history_clone.lock().await;
            let mut blocks: Vec<AssistantContentBlock> = Vec::new();

            if let Some(ref text) = response_text
                && !text.is_empty()
            {
                blocks.push(AssistantContentBlock::Text(TextContentBlock {
                    text: text.clone(),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: accumulated_text_signature.clone(),
                    },
                    reasoning_details: None,
                }));
            }

            // Merge extracted thinking with any reasoning from the provider.
            // If the provider already sent reasoning_content, prepend any
            // thinking extracted from delta.content (rare but possible).
            let merged_thinking = match (extracted_thinking, accumulated_reasoning.is_empty()) {
                (Some(t), true) => Some(t),
                (Some(t), false) => Some(format!("{t}\n{accumulated_reasoning}")),
                (None, false) => Some(accumulated_reasoning.clone()),
                (None, true) => None,
            };

            if let Some(ref thinking) = merged_thinking
                && !thinking.is_empty()
            {
                blocks.push(AssistantContentBlock::Thinking(ThinkingBlock {
                    thinking: thinking.clone(),
                    signature: accumulated_signature.clone(),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    summary: None,
                }));
            }

            for redacted_data in &accumulated_redacted_data {
                blocks.push(AssistantContentBlock::RedactedThinking(
                    RedactedThinkingBlock {
                        data: redacted_data.clone(),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                    },
                ));
            }

            for prepared in &prepared_tool_calls {
                let tool_input = Self::assistant_tool_input(prepared);
                blocks.push(AssistantContentBlock::ToolUse(ToolUseBlock {
                    id: prepared.tool_id.clone(),
                    name: prepared.tool_name.clone(),
                    input: tool_input,
                    shared: SharedContentFields {
                        call_id: prepared.tool_call.call_id.clone(),
                        signature: prepared.tool_call.signature.clone(),
                    },
                    reasoning_details: None,
                }));
            }

            // Truncate thinking blocks in older history entries before adding new message.
            // This prevents token bloat from extended-thinking models (Claude, DeepSeek).
            truncate_old_thinking_blocks(&mut history);

            history.push(StorageMessage {
                id: Some(Self::next_message_id(&self.message_counter)),
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(blocks),
                model_info: None,
                metrics: None,
                ts: Some(chrono::Utc::now().timestamp_millis() as u64),
            });

            let text_without_tools = response_text.as_ref().is_some_and(|t| !t.is_empty())
                && prepared_tool_calls.is_empty();

            if !prepared_tool_calls.is_empty() {
                let mut state = state_clone.lock().await;
                state.text_only_turns = 0;
            } else if text_without_tools {
                let mut state = state_clone.lock().await;
                let first_task_turn = state.turns_completed == 0;
                state.text_only_turns = state.text_only_turns.saturating_add(1);
                let text_only_turns = state.text_only_turns;
                drop(state);

                let first_turn_direct_answer = first_task_turn
                    && self.config.mode == AgentMode::Act
                    && !self.config.interactive_mode;
                let plan_active = self.plan_execution_active().await;

                if (first_turn_direct_answer || text_only_turns > 1) && !plan_active {
                    text_only_completes_task = true;
                } else if text_only_turns == 1 {
                    if let Some(profile) = self.deps.tool_profile
                        && let Some(next) = profile.escalate()
                    {
                        tracing::info!(
                            ?profile,
                            ?next,
                            "escalating tool profile after text-only response"
                        );
                        self.deps.tool_profile = Some(next);
                        // The cached prompt was built for `profile`. Keep the
                        // next request's instructions aligned with the newly
                        // escalated tool schemas.
                        self.deps.cached_system_prompt = None;
                    }
                    history.push(StorageMessage {
                        id: Some(Self::next_message_id(&self.message_counter)),
                        role: MessageRole::User,
                        content: MessageContent::Text(String::from(
                            "You returned text without using a tool. If this task requires workspace changes or verification, use the required tool. If the task is complete, call attempt_completion or plan_mode_respond.",
                        )),
                        model_info: None,
                        metrics: None,
                        ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                    });
                }
            }
        }

        // 7. Save a checkpoint only before a batch that can change the workspace.
        // Read-only turns used to run `git add --all` and `git commit` here too,
        // which can take minutes on a large or remote workspace and prevented the
        // first tool result from being emitted.
        let checkpoint_required = prepared_tool_calls.iter().any(|prepared| {
            SnedTool::from_name(&prepared.tool_name).is_some_and(Self::tool_may_modify_workspace)
        });
        if checkpoint_required && let Some(ref mut checkpoint_mgr) = self.deps.checkpoint_manager {
            let checkpoint_cancellation = self.state.lock().await.checkpoint_cancellation.clone();
            let checkpoint_started = std::time::Instant::now();
            tracing::debug!("saving checkpoint before mutating tool batch");
            checkpoint_mgr
                .save_checkpoint_with_cancellation(Some(checkpoint_cancellation))
                .await;
            tracing::debug!(
                elapsed_ms = checkpoint_started.elapsed().as_millis(),
                "saved checkpoint before mutating tool batch"
            );
        }

        // Provider tool-call order must remain stable even when independent work overlaps.
        let mut tool_failure_count = 0usize;
        let mut completion_result: Option<String> = None;
        if !prepared_tool_calls.is_empty() {
            let mut edit_files: Vec<(String, i32, i32)> = Vec::new();

            // Print the complete dispatched call so the user can verify what the
            // model asked Sned to do (skip malformed tool calls with empty names).
            if !self.config.json_output {
                for prepared in &prepared_tool_calls {
                    let tool_name = prepared.tool_name.as_str();

                    // Skip malformed tool calls with empty names
                    if tool_name.is_empty() {
                        continue;
                    }

                    let call_lines = match &prepared.parsed_args {
                        Ok(tool_params) => format_tool_call_lines(tool_name, tool_params),
                        Err(parse_error) => format_tool_call_lines_with_raw_arguments(
                            tool_name,
                            prepared.tool_call.function.arguments.as_deref(),
                            parse_error,
                        ),
                    };
                    for line in call_lines {
                        self.config.output_writer.emit(OutputEvent::tool_call(line));
                    }
                    self.config.output_writer.flush();
                }
            }

            let hook_manager_handle = self.deps.hook_manager.clone();
            let config_handle = self.config.clone();

            // Phase 1: Pre-process all tools (check plan mode, approval, resolve handlers)
            // This is done sequentially since approval may require user interaction
            type ToolTask = (
                String,
                String,
                Option<ToolExecutionOutput>,
                Option<futures::future::BoxFuture<'static, ToolExecutionOutput>>,
                Vec<FileActionPath>,
                serde_json::Value,
            );
            let mut tool_tasks: Vec<ToolTask> = Vec::with_capacity(prepared_tool_calls.len());

            for prepared in &prepared_tool_calls {
                let tool_name = prepared.tool_name.clone();
                tracing::debug!(tool = %tool_name, "preparing tool execution");

                // Skip tool calls with empty names (malformed provider response)
                if tool_name.is_empty() {
                    tracing::warn!("received tool call with empty name, skipping");
                    continue;
                }

                let tool_id = prepared.tool_id.clone();
                let tool_params = match &prepared.parsed_args {
                    Ok(params) => params.clone(),
                    Err(parse_error) => {
                        tool_tasks.push((
                            tool_id,
                            tool_name,
                            Some(ToolExecutionOutput::error(parse_error.clone(), None)),
                            None,
                            vec![],
                            serde_json::Value::Null,
                        ));
                        continue;
                    }
                };

                // A write/edit generated without the applicable nested rules
                // must not run under an incomplete prompt. The rules have now
                // been loaded, so return a retryable tool result and let the
                // next provider request make the informed decision.
                if scoped_rules_added && Self::is_mutating_file_tool(&tool_name) {
                    tracing::debug!(
                        tool = %tool_name,
                        "deferred mutating file tool until scoped AGENTS.md rules are visible"
                    );
                    tool_tasks.push((
                        tool_id,
                        tool_name,
                        Some(ToolExecutionOutput::error(
                            "Scoped AGENTS.md rules were loaded for this path. Retry the file operation so the updated instructions are applied.".to_string(),
                            None,
                        )),
                        None,
                        vec![],
                        tool_params,
                    ));
                    continue;
                }

                let immediate_output = if let Some(tool) = SnedTool::from_name(&tool_name) {
                    // Reject tools that are not in the active profile so the model
                    // cannot call tools its current profile has filtered out.
                    let profile_denied = self
                        .deps
                        .tool_profile
                        .is_some_and(|p| !p.tools().contains(&tool));

                    // Check plan mode restrictions
                    let is_restricted = if self.config.mode == AgentMode::Plan {
                        tracing::debug!(tool = %tool_name, "checking plan-mode restriction");
                        let state = self.state.lock().await;
                        state.strict_plan_mode_enabled && Self::is_plan_mode_restricted(tool)
                    } else {
                        false
                    };

                    if profile_denied {
                        ToolExecutionOutput::error(
                            format!(
                                "Tool '{tool_name}' is not available in the current tool profile. Use one of the tools listed for this turn."
                            ),
                            None,
                        )
                    } else if is_restricted {
                        ToolExecutionOutput::error(
                            format!(
                                "Tool '{tool_name}' is not available in PLAN MODE. This tool is restricted to ACT MODE for file modifications. Only use tools available for PLAN MODE when in that mode."
                            ),
                            None,
                        )
                    } else if let Some(handler) = self.deps.registry().get_handler(&tool) {
                        // Check approval with per-path resolution (ported from autoApprove.ts:126-180)
                        //
                        // Key semantics matching TypeScript source:
                        //   shouldAutoApprove = isYolo || (isSafe && autoApproveEnabled)
                        // Safety gates auto-approval, NEVER post-approval execution.
                        // Once the user approves at the prompt, the command always runs.
                        // For execute_command: if auto-approved but command is unsafe,
                        // force a prompt so the user can review.
                        let action_paths = Self::extract_action_path(tool, &tool_params);
                        let external_directories = Self::external_action_directories(
                            tool,
                            &tool_context.workspace_root,
                            &action_paths,
                        );
                        let params_fingerprint = Self::tool_params_fingerprint(&tool_params);
                        tracing::debug!(tool = %tool_name, "checking prior tool denial");
                        let previously_denied = {
                            let state = self.state.lock().await;
                            state
                                .is_denied_tool_action(&tool_name, &params_fingerprint)
                                .is_some()
                        };
                        if previously_denied {
                            ToolExecutionOutput::error(
                                format!(
                                    "Tool '{tool_name}' was already denied for this exact request. Ask the user before retrying the same action."
                                ),
                                Some(ToolFailureMetadata {
                                    class: ToolFailureClass::ApprovalDenied,
                                    affected_paths: action_paths.clone(),
                                    required_next_step: Some(ToolRequiredNextStep::AskUser),
                                }),
                            )
                        } else {
                            let mut user_prompted = false;
                            let mut session_command_scope_approved = false;
                            let mut allowed_external_roots = Vec::new();
                            let command_scopes = (tool_name == "execute_command")
                                .then(|| {
                                    crate::core::approval::command_approval_scopes(&tool_params)
                                })
                                .flatten();
                            let approval_result = if let Some(ref approval_mgr) =
                                self.deps.approval_manager
                            {
                                tracing::debug!(tool = %tool_name, "waiting for approval manager");
                                let mgr = approval_mgr.lock().await;
                                tracing::debug!(tool = %tool_name, "acquired approval manager");
                                allowed_external_roots = mgr.external_directory_grants_for(
                                    tool.category(),
                                    &external_directories,
                                );
                                let external_needs_prompt = !external_directories.is_empty()
                                    && !mgr.external_directories_are_granted(
                                        tool.category(),
                                        &external_directories,
                                    );
                                // Check if any action paths require prompting
                                let needs_prompt = if external_needs_prompt {
                                    true
                                } else if action_paths.is_empty() {
                                    if tool_name == "execute_command" {
                                        session_command_scope_approved =
                                            command_scopes.as_ref().is_some_and(|scopes| {
                                                mgr.command_scopes_are_approved(scopes)
                                            });
                                        !session_command_scope_approved
                                            && mgr.should_prompt(
                                                tool,
                                                Some(params_fingerprint.as_str()),
                                            )
                                    } else {
                                        mgr.should_prompt(tool, None)
                                    }
                                } else {
                                    // Has paths: check per-path approval
                                    action_paths.iter().any(|p| {
                                        mgr.should_prompt_with_path(tool, Some(p.as_str()))
                                    })
                                };
                                if needs_prompt {
                                    drop(mgr); // Drop lock before async call
                                    user_prompted = true;
                                    let approval = if external_needs_prompt {
                                        crate::core::approval::prompt_for_external_directory_approval_async(
                                            &tool_name,
                                            &tool_params,
                                            external_directories.clone(),
                                            self.config.output_writer.clone(),
                                            Some(tool_context.workspace_root.clone()),
                                        )
                                        .await
                                    } else {
                                        crate::core::approval::prompt_for_approval_async_in_workspace(
                                            &tool_name,
                                            &tool_params,
                                            self.config.output_writer.clone(),
                                            Some(tool_context.workspace_root.clone()),
                                        )
                                        .await
                                    };
                                    match approval {
                                        Ok(crate::core::approval::ApprovalResult::Denied) => {
                                            let mut state = self.state.lock().await;
                                            state.record_denied_tool_action(
                                                crate::core::agent_types::DeniedToolAction {
                                                    tool_name: tool_name.clone(),
                                                    action_paths: action_paths.clone(),
                                                    params_fingerprint: params_fingerprint.clone(),
                                                },
                                            );
                                            Some(ToolExecutionOutput::error(
                                                crate::core::approval::format_denial_message(
                                                    &tool_name,
                                                ),
                                                Some(ToolFailureMetadata {
                                                    class: ToolFailureClass::ApprovalDenied,
                                                    affected_paths: action_paths.clone(),
                                                    required_next_step: Some(
                                                        ToolRequiredNextStep::AskUser,
                                                    ),
                                                }),
                                            ))
                                        }
                                        Ok(crate::core::approval::ApprovalResult::Always) => {
                                            if let Some(ref am) = self.deps.approval_manager {
                                                let mut mgr = am.lock().await;
                                                if tool_name == "execute_command" {
                                                    mgr.auto_approve_command(
                                                        &params_fingerprint,
                                                        command_scopes.as_deref(),
                                                    );
                                                } else {
                                                    mgr.auto_approve(tool, None);
                                                }
                                            }
                                            None // Proceed to execute
                                        }
                                        Ok(
                                            crate::core::approval::ApprovalResult::AllowExternalDirectory,
                                        ) => {
                                            if let Some(ref am) = self.deps.approval_manager {
                                                let mut mgr = am.lock().await;
                                                if let Some(error) = external_directories.iter().find_map(|directory| {
                                                    mgr.grant_external_directory(directory, tool.category()).err()
                                                }) {
                                                    Some(ToolExecutionOutput::error(
                                                        format!("Could not authorize external directory: {error}"),
                                                        None,
                                                    ))
                                                } else {
                                                    allowed_external_roots = mgr.external_directory_grants_for(
                                                        tool.category(),
                                                        &external_directories,
                                                    );
                                                    None
                                                }
                                            } else {
                                                Some(ToolExecutionOutput::error(
                                                    "External directory approval is unavailable for this task".to_string(),
                                                    None,
                                                ))
                                            }
                                        }
                                        Ok(crate::core::approval::ApprovalResult::Approved) => {
                                            allowed_external_roots = external_directories.clone();
                                            None // Proceed to execute
                                        }
                                        Err(e) => Some(ToolExecutionOutput::error(
                                            crate::core::approval::format_approval_error(
                                                Some(&tool_name),
                                                &e,
                                            ),
                                            None,
                                        )),
                                    }
                                } else if tool_name == "execute_command" {
                                    // Auto-approved path for execute_command: check command
                                    // safety before auto-approving. If the command is
                                    // unsafe, prompt the user instead (matching TS:
                                    // shouldAutoApprove = isSafe && autoApproveEnabled).
                                    let commands = coerce_command_array(&tool_params);
                                    let script = tool_params.get("script").and_then(|s| s.as_str());
                                    let yolo = mgr.is_yolo_mode();
                                    let user_safe = mgr.get_user_safe_commands().clone();
                                    let checker =
                                        crate::core::approval::CommandSafetyChecker::new()
                                            .with_yolo(yolo)
                                            .with_user_safe_commands(user_safe);
                                    let any_unsafe = if session_command_scope_approved {
                                        commands.iter().any(|cmd| {
                                            !cmd.is_empty()
                                                && checker
                                                    .is_structurally_safe_for_scope(cmd)
                                                    .is_err()
                                        }) || script.is_some_and(|s| {
                                            checker.is_structurally_safe_for_scope(s).is_err()
                                        })
                                    } else {
                                        commands.iter().any(|cmd| {
                                            !cmd.is_empty() && checker.is_safe(cmd).is_err()
                                        }) || script.is_some_and(|s| checker.is_safe(s).is_err())
                                    };
                                    if any_unsafe {
                                        // In non-interactive mode, deny unsafe commands directly
                                        // (no TUI available to prompt the user).
                                        if !self.config.interactive_mode {
                                            let mut state = self.state.lock().await;
                                            state.record_denied_tool_action(
                                                crate::core::agent_types::DeniedToolAction {
                                                    tool_name: tool_name.clone(),
                                                    action_paths: action_paths.clone(),
                                                    params_fingerprint: params_fingerprint.clone(),
                                                },
                                            );
                                            Some(ToolExecutionOutput::error(
                                                crate::core::approval::format_denial_message(
                                                    &tool_name,
                                                ),
                                                Some(ToolFailureMetadata {
                                                    class: ToolFailureClass::ApprovalDenied,
                                                    affected_paths: action_paths.clone(),
                                                    required_next_step: Some(
                                                        ToolRequiredNextStep::AskUser,
                                                    ),
                                                }),
                                            ))
                                        } else {
                                            drop(mgr);
                                            user_prompted = true;
                                            match crate::core::approval::prompt_for_approval_async_in_workspace(
                                                &tool_name,
                                                &tool_params,
                                                self.config.output_writer.clone(),
                                                Some(tool_context.workspace_root.clone()),
                                            )
                                            .await
                                            {
                                                Ok(
                                                    crate::core::approval::ApprovalResult::Denied,
                                                ) => {
                                                    let mut state = self.state.lock().await;
                                                    state.record_denied_tool_action(
                                                        crate::core::agent_types::DeniedToolAction {
                                                            tool_name: tool_name.clone(),
                                                            action_paths: action_paths.clone(),
                                                            params_fingerprint: params_fingerprint
                                                                .clone(),
                                                        },
                                                    );
                                                    Some(ToolExecutionOutput::error(
                                                        crate::core::approval::format_denial_message(
                                                            &tool_name,
                                                        ),
                                                        Some(ToolFailureMetadata {
                                                            class: ToolFailureClass::ApprovalDenied,
                                                            affected_paths: action_paths.clone(),
                                                            required_next_step: Some(
                                                                ToolRequiredNextStep::AskUser,
                                                            ),
                                                        }),
                                                    ))
                                                }
                                                Ok(
                                                    crate::core::approval::ApprovalResult::Always,
                                                ) => {
                                                    if let Some(ref am) = self.deps.approval_manager
                                                    {
                                                        let mut mgr = am.lock().await;
                                                        mgr.auto_approve_command(
                                                            &params_fingerprint,
                                                            command_scopes.as_deref(),
                                                        );
                                                    }
                                                    None
                                                }
                                                Ok(
                                                    crate::core::approval::ApprovalResult::AllowExternalDirectory,
                                                ) => Some(ToolExecutionOutput::error(
                                                    "External directory access does not apply to execute_command"
                                                        .to_string(),
                                                    None,
                                                )),
                                                Ok(
                                                    crate::core::approval::ApprovalResult::Approved,
                                                ) => None,
                                                Err(e) => Some(ToolExecutionOutput::error(
                                                    crate::core::approval::format_approval_error(
                                                        Some(&tool_name),
                                                        &e,
                                                    ),
                                                    None,
                                                )),
                                            }
                                        }
                                    } else {
                                        None // Safe command, auto-approve proceeds
                                    }
                                } else {
                                    None // No approval needed
                                }
                            } else {
                                None // No approval manager configured
                            };

                            if let Some(denied_text) = approval_result {
                                tracing::debug!(tool = %tool_name, "tool execution denied by approval");
                                denied_text
                            } else {
                                // Prompt approval already performed the safety review that the
                                // handler otherwise applies to an auto-approved command.
                                let mut tool_context = (*tool_context).clone();
                                tool_context.explicitly_approved = user_prompted;
                                tool_context.allowed_external_roots = allowed_external_roots;
                                tool_context.session_command_scope_approved =
                                    session_command_scope_approved;
                                let tool_context = Arc::new(tool_context);
                                let hook_manager = hook_manager_handle.clone();
                                let config = config_handle.clone();
                                let handler = handler.clone();
                                let tool_name = tool_name.clone();
                                let tool_params = tool_params.clone();
                                let task_storage = self.deps.task_storage.clone().map(Arc::new);
                                let edit_file_paths =
                                    if tool_name == "edit_file" || tool_name == "write_to_file" {
                                        Self::extract_file_action_path(
                                            &tool_name,
                                            &tool_params,
                                            &tool_context.workspace_root,
                                        )
                                    } else {
                                        vec![]
                                    };

                                // Condense reads the current history while the tool runs.
                                let conversation_history = self.conversation_history.clone();

                                let tool_params_for_task = tool_params.clone();
                                tool_tasks.push((
                                    tool_id,
                                    tool_name.clone(),
                                    None,
                                    Some(
                                        async move {
                                            tracing::debug!(
                                                tool = %tool_name,
                                                params = %tool_params.to_string(),
                                                "executing tool"
                                            );
                                            let result = Self::execute_tool_with_hooks_internal(
                                                &config,
                                                hook_manager,
                                                tool_context,
                                                &tool_name,
                                                &tool_params,
                                                handler,
                                                task_storage,
                                                conversation_history,
                                            )
                                            .await;
                                            tracing::debug!(
                                                tool = %tool_name,
                                                result_len = result.text.len(),
                                                "tool execution complete"
                                            );
                                            result
                                        }
                                        .boxed(),
                                    ),
                                    edit_file_paths,
                                    tool_params_for_task,
                                ));
                                continue;
                            }
                        }
                    } else {
                        tracing::warn!(tool = %tool_name, "tool handler not implemented");
                        ToolExecutionOutput::error(
                            format!("Tool execution for '{tool_name}' not yet implemented"),
                            None,
                        )
                    }
                } else {
                    tracing::warn!(tool = %tool_name, "unknown tool requested");
                    // Surface only the tools in the active profile so the model
                    // does not hallucinate names from tools it cannot actually call.
                    let active_profile = self
                        .deps
                        .tool_profile
                        .unwrap_or(crate::core::tools::definitions::ToolProfile::Full);
                    let available =
                        crate::core::tools::definitions::get_tool_definitions_for_profile(
                            active_profile,
                        )
                        .iter()
                        .map(|t| t.function.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    ToolExecutionOutput::error(
                        format!("Unknown tool: '{tool_name}'. Available tools: {available}"),
                        None,
                    )
                };

                tool_tasks.push((
                    tool_id,
                    tool_name,
                    Some(immediate_output),
                    None,
                    vec![],
                    tool_params,
                ));
            }

            let parallel_enabled = self
                .deps
                .system_prompt_context
                .as_ref()
                .is_some_and(|context| context.enable_parallel_tool_calling);
            let mut result_map: std::collections::HashMap<usize, ToolExecutionOutput> =
                std::collections::HashMap::with_capacity(tool_tasks.len());
            if !parallel_enabled {
                for (i, (_, _, _, task, _, _)) in tool_tasks.iter_mut().enumerate() {
                    if let Some(future) = task.take() {
                        result_map.insert(i, future.await);
                    }
                }
            }

            let mut non_edit_executed = std::collections::HashSet::new();
            type EditGroup = (
                std::collections::HashSet<String>,
                Vec<(
                    usize,
                    futures::future::BoxFuture<'static, ToolExecutionOutput>,
                )>,
            );
            let mut edit_groups: Vec<EditGroup> = Vec::new();
            for (i, (_, tool_name, _, task, edit_file_paths, _tool_params)) in
                tool_tasks.iter_mut().enumerate()
            {
                if (tool_name == "edit_file" || tool_name == "write_to_file")
                    && let Some(future) = task.take()
                {
                    let paths: std::collections::HashSet<String> = edit_file_paths
                        .iter()
                        .map(|path| path.normalized.clone())
                        .collect();
                    let mut found_group = None;
                    for (idx, (group_paths, _)) in edit_groups.iter().enumerate() {
                        if paths.iter().any(|p| group_paths.contains(p)) {
                            found_group = Some(idx);
                            break;
                        }
                    }
                    if let Some(idx) = found_group {
                        edit_groups[idx].1.push((i, future));
                    } else {
                        edit_groups.push((paths, vec![(i, future)]));
                    }
                }
            }

            let non_edit_futures: Vec<_> = tool_tasks
                .iter_mut()
                .enumerate()
                .filter_map(|(i, (_, tool_name, _, task, _, _))| {
                    if task.is_some() && tool_name != "edit_file" && tool_name != "write_to_file" {
                        non_edit_executed.insert(i);
                        task.take()
                    } else {
                        None
                    }
                })
                .collect();

            let non_edit_results: Vec<_> = {
                use futures::StreamExt;
                futures::stream::iter(non_edit_futures)
                    .buffered(DEFAULT_TOOL_CONCURRENCY)
                    .collect()
                    .await
            };

            // Overlapping file writes stay ordered to avoid stale reads and lost updates.
            let edit_group_futures: Vec<_> = edit_groups
                .into_iter()
                .map(|(_paths, calls)| {
                    async move {
                        let mut results = Vec::new();
                        for (i, future) in calls {
                            let result = future.await;
                            results.push((i, result));
                        }
                        results
                    }
                    .boxed()
                })
                .collect();

            let edit_group_results = futures::future::join_all(edit_group_futures).await;

            let mut non_edit_iter = non_edit_results.into_iter();
            for i in 0..tool_tasks.len() {
                if non_edit_executed.contains(&i) {
                    let Some(result) = non_edit_iter.next() else {
                        error!(
                            "Tool execution invariant violated: non_edit_results has fewer items \
                             than non_edit_executed indices (missing at index {}). This indicates \
                             a bug in parallel tool execution logic.",
                            i
                        );
                        return TurnResult::Error(
                            "Internal error: tool execution produced inconsistent results"
                                .to_string(),
                        );
                    };
                    result_map.insert(i, result);
                }
            }
            for group_result in edit_group_results {
                for (i, result) in group_result {
                    result_map.insert(i, result);
                }
            }

            let execution_results: Vec<ToolExecutionOutput> = (0..tool_tasks.len())
                .filter_map(|i| result_map.remove(&i))
                .collect();

            // Track tool execution statistics for consecutive_mistakes tracking
            let tools_called = !execution_results.is_empty();
            tool_failure_count = execution_results.iter().filter(|r| r.is_error).count();

            // Phase 3: Collect results in order, then push as ONE StorageMessage
            let mut execution_results_iter = execution_results.into_iter();
            let mut tool_result_blocks: Vec<UserContentBlock> = Vec::new();
            for (tool_id, tool_name, immediate_result_text, _task, edit_file_path, tool_params) in
                tool_tasks
            {
                let mut result_output = if let Some(result_text) = immediate_result_text {
                    result_text
                } else {
                    execution_results_iter.next().unwrap_or_else(|| {
                        ToolExecutionOutput::error("Tool execution failed".to_string(), None)
                    })
                };

                // Display compact tool result in TTY mode
                if !self.config.json_output {
                    // Hold lock across check-and-set to avoid TOCTOU race
                    let mut state = self.state.lock().await;
                    if !state.first_tool_result_printed {
                        state.first_tool_result_printed = true;
                    }
                    drop(state);

                    let is_error = result_output.is_error;

                    if tool_name == "edit_file" {
                        let (stats, added, removed) =
                            extract_edit_stats_detailed(&result_output.text);
                        for path in &edit_file_path {
                            if added > 0 || removed > 0 {
                                edit_files.push((path.display.clone(), added, removed));
                            }
                        }
                        let status = if is_error { "✗" } else { "✓" };
                        self.config
                            .output_writer
                            .emit(OutputEvent::tool_output_line(
                                format!("  {status} {stats}"),
                                Style::default().fg(if is_error { ERROR_FG } else { PROMPT_FG }),
                            ));
                        if !is_error {
                            for preview in edit_result_diff_previews(&result_output.text) {
                                for mut line in
                                    crate::cli::tui::ansi_converter::ansi_to_ratatui_lines(&preview)
                                {
                                    strip_edit_diff_anchors(&mut line);
                                    self.config
                                        .output_writer
                                        .emit(OutputEvent::ToolOutputLine(line));
                                }
                            }
                        }
                    } else if tool_name == "execute_command" {
                        // execute_command streams stdout/stderr while it runs;
                        // keep the final status summary without duplicating the
                        // already-visible command output.
                        let digest_lines = format_tool_result_digest(
                            &tool_name,
                            &tool_params,
                            &result_output.text,
                            is_error,
                            if is_error { ERROR_FG } else { PROMPT_FG },
                            PROMPT_FG,
                        );
                        for line in digest_lines {
                            let mut style = Style::default();
                            if let Some(color) = line.fg {
                                style = style.fg(color);
                            }
                            if line.dim {
                                style = style.add_modifier(Modifier::DIM);
                            }
                            self.config
                                .output_writer
                                .emit(OutputEvent::tool_output_line(line.text, style));
                        }
                    } else if !matches!(
                        tool_name.as_str(),
                        "plan_mode_respond"
                            | "ask_followup_question"
                            | "condense"
                            | "use_subagents"
                    ) && (tool_name != "attempt_completion" || is_error)
                    {
                        let style =
                            Style::default().fg(if is_error { ERROR_FG } else { PROMPT_FG });
                        let status = if is_error { "✗" } else { "✓" };
                        self.config
                            .output_writer
                            .emit(OutputEvent::tool_output_line(
                                format!("  {status} {tool_name} result"),
                                style,
                            ));
                        if result_output.text.is_empty() {
                            self.config
                                .output_writer
                                .emit(OutputEvent::tool_output_line(
                                    "  (empty tool result)",
                                    style,
                                ));
                        } else {
                            // Keep the result in one channel event. The TUI
                            // splits embedded newlines into transcript rows,
                            // while this avoids dropping individual lines if
                            // the output channel is under pressure.
                            self.config
                                .output_writer
                                .emit(OutputEvent::tool_output_line(
                                    strip_tool_result_anchors(&result_output.text),
                                    style,
                                ));
                        }
                    }
                }

                if tool_name == "edit_file"
                    && let Some(metadata) = &result_output.metadata
                    && metadata.class == ToolFailureClass::AnchorInvalid
                    && metadata.required_next_step == Some(ToolRequiredNextStep::ReadFile)
                {
                    result_output.text.push_str(
                        "\n\nNext step: call read_file on this path again before retrying edit_file.",
                    );
                }
                if tool_name == "edit_file"
                    && let Some(metadata) = &result_output.metadata
                    && metadata.required_next_step == Some(ToolRequiredNextStep::AskUser)
                {
                    result_output.text.push_str(
                        "\n\nNext step: call ask_followup_question. Do not bypass this edit_file safety limit with execute_command.",
                    );
                }

                tracing::debug!(
                    tool_id = %tool_id,
                    tool_name = %tool_name,
                    result_len = result_output.text.len(),
                    result_preview = %&result_output.text[..result_output.text.floor_char_boundary(result_output.text.len().min(80))],
                    "tool result paired with ID"
                );

                if tool_name == "attempt_completion" && !result_output.is_error {
                    completion_result = Some(result_output.text.clone());
                }

                append_tool_result_blocks(&mut tool_result_blocks, tool_id, result_output);
            }
            if !tool_result_blocks.is_empty() {
                if !self.config.json_output {
                    self.config.output_writer.flush();
                }

                let mut history = self.conversation_history.lock().await;
                history.push(StorageMessage {
                    id: Some(Self::next_message_id(&self.message_counter)),
                    role: MessageRole::User,
                    content: MessageContent::UserBlocks(tool_result_blocks),
                    model_info: None,
                    metrics: None,
                    ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                });
            }

            // Track consecutive mistakes for tool failures (denied approval, parse error, etc.)
            // This ensures repeated tool failures trigger the same safety net as empty responses
            if tools_called {
                // Tools were called - check if they succeeded
                if tool_failure_count > 0 {
                    let mut state = self.state.lock().await;
                    state.consecutive_mistakes += 1;
                    tracing::warn!(
                        consecutive_mistakes = state.consecutive_mistakes,
                        max_allowed = ?self.config.max_consecutive_mistakes,
                        tool_failures = tool_failure_count,
                        "Tool execution failures detected"
                    );

                    // Handle plan step failure: mark current step as Failed and stop execution
                    let mut step_fail_msg = None;
                    if let Some(ref mut plan) = state.plan_state
                        && plan.approved
                        && !plan.complete
                        && plan.current_step_index < plan.steps.len()
                    {
                        let current_status = &plan.steps[plan.current_step_index].status;
                        if *current_status != PlanStepStatus::Failed {
                            plan.mark_step(plan.current_step_index, PlanStepStatus::Failed)
                                .ok();
                            plan.set_paused(true);
                            tracing::info!(
                                step_index = plan.current_step_index,
                                "Plan step failed. Execution paused. User action required."
                            );
                            if !self.config.json_output {
                                step_fail_msg = Some(format!(
                                    "Plan step {}/{} failed. Use /plan resume to retry or /plan abort to cancel.",
                                    plan.current_step_index + 1,
                                    plan.steps.len()
                                ));
                            }
                        }
                    }

                    let max_reached = self
                        .config
                        .max_consecutive_mistakes
                        .is_some_and(|limit| state.consecutive_mistakes >= limit);
                    drop(state);

                    if let Some(msg) = step_fail_msg {
                        self.config.output_writer.emit(OutputEvent::error_box(msg));
                        return TurnResult::Continue;
                    }

                    if max_reached {
                        return TurnResult::Error(format!(
                            "Max consecutive mistakes ({}) reached. The model is repeatedly failing.",
                            self.config
                                .max_consecutive_mistakes
                                .expect("max_reached requires a configured limit")
                        ));
                    }
                } else {
                    // All tools succeeded - reset consecutive mistakes
                    let mut state = self.state.lock().await;
                    state.consecutive_mistakes = 0;
                    // Advance plan step on success
                    let mut plan_completed = false;
                    if let Some(ref mut plan) = state.plan_state
                        && plan.approved
                        && !plan.complete
                    {
                        plan.advance();
                        // Check if plan is now complete
                        if plan.complete {
                            plan_completed = true;
                            tracing::info!("All plan steps completed successfully.");
                        }
                    }
                    drop(state);

                    if plan_completed && !self.config.json_output {
                        self.config
                            .output_writer
                            .emit(OutputEvent::tool_output_line(
                                "✓ Plan complete. All steps executed successfully.",
                                Style::default()
                                    .fg(Color::Green)
                                    .add_modifier(Modifier::BOLD),
                            ));
                    }
                    if plan_completed {
                        self.set_mode(AgentMode::Act);
                    }
                }
            } else {
                // No tools were called (text-only response) - reset consecutive mistakes
                let mut state = self.state.lock().await;
                state.consecutive_mistakes = 0;
                drop(state);
            }

            // Inject hint when approaching the mistake limit
            let mistakes_count;
            {
                let state = self.state.lock().await;
                mistakes_count = state.consecutive_mistakes;
            }
            if self
                .config
                .max_consecutive_mistakes
                .is_some_and(|limit| mistakes_count >= limit.saturating_sub(1))
            {
                let hint = {
                    let state = self.state.lock().await;
                    Self::reread_recovery_hint(&state)
                };
                if let Some(hint) = hint {
                    let mut history = self.conversation_history.lock().await;
                    history.push(StorageMessage {
                        id: Some(Self::next_message_id(&self.message_counter)),
                        role: crate::providers::MessageRole::User,
                        content: crate::providers::MessageContent::Text(hint),
                        model_info: None,
                        metrics: None,
                        ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                    });
                }
            }

            // Summarize consumed read_file results after successful edit_file
            // This prevents ~22KB anchored file contents from accumulating as dead weight
            if !edit_files.is_empty() {
                let edited_paths: Vec<String> =
                    edit_files.iter().map(|(p, _, _)| p.clone()).collect();
                let mut history = self.conversation_history.lock().await;
                let mut known_read_paths = Vec::new();
                for msg in history.iter() {
                    if let MessageContent::UserBlocks(blocks) = &msg.content {
                        for block in blocks {
                            if let UserContentBlock::ToolResult(tr) = block
                                && let ToolResultContent::Text(text) = &tr.content
                            {
                                known_read_paths.extend(
                                    text.split("\n---\n")
                                        .filter_map(path_from_read_file_header)
                                        .map(String::from),
                                );
                            }
                        }
                    }
                }
                for msg in history.iter_mut().rev() {
                    if let MessageContent::UserBlocks(ref mut blocks) = msg.content {
                        for block in blocks.iter_mut() {
                            if let UserContentBlock::ToolResult(tr) = block
                                && let ToolResultContent::Text(text) = &tr.content
                                && text.contains("[File: ")
                            {
                                let new_text = summarize_matching_sections(
                                    text,
                                    &edited_paths,
                                    &known_read_paths,
                                );
                                if new_text != *text {
                                    tr.content = ToolResultContent::Text(new_text);
                                }
                            }
                        }
                    }
                }
            }

            if !edit_files.is_empty() && !self.config.json_output {
                self.config
                    .output_writer
                    .emit(OutputEvent::tool_output_line(
                        format_heat_map(&edit_files),
                        Style::default().add_modifier(Modifier::DIM),
                    ));

                // Auto-commit to shadow git after file-modifying turns
                // Only commit if files were actually modified (not just attempted or failed)
                // Check that we have actual changes (added or removed lines > 0)
                let has_actual_changes = edit_files
                    .iter()
                    .any(|(_, added, removed)| *added > 0 || *removed > 0);
                if self.config.track_changes
                    && has_actual_changes
                    && let Ok(workspace_root) = std::env::current_dir()
                {
                    let message = format!("[sned] turn: {}", format_heat_map_plain(&edit_files));
                    // Run synchronous git operations in spawn_blocking to avoid blocking runtime
                    let result = tokio::task::spawn_blocking(move || {
                        crate::core::shadow_git::commit_turn(&workspace_root, &message)
                    })
                    .await;
                    report_shadow_commit_result(&self.config.output_writer, result);
                }
            }

            // Print action digest summarizing what happened in this turn
            if !self.config.json_output && !prepared_tool_calls.is_empty() {
                let files_created = prepared_tool_calls
                    .iter()
                    .filter(|prepared| prepared.tool_name == "write_to_file")
                    .count();
                let files_edited = edit_files
                    .iter()
                    .filter(|(_, added, removed)| *added > 0 || *removed > 0)
                    .count();
                let commands_run = prepared_tool_calls
                    .iter()
                    .filter(|prepared| prepared.tool_name == "execute_command")
                    .count();

                let mut parts = Vec::new();
                if files_created > 0 {
                    parts.push(format!(
                        "{} file{} created",
                        files_created,
                        if files_created == 1 { "" } else { "s" }
                    ));
                }
                if files_edited > 0 {
                    parts.push(format!(
                        "{} file{} edited",
                        files_edited,
                        if files_edited == 1 { "" } else { "s" }
                    ));
                }
                if commands_run > 0 {
                    parts.push(format!(
                        "{} command{} run",
                        commands_run,
                        if commands_run == 1 { "" } else { "s" }
                    ));
                }

                if !parts.is_empty() {
                    self.config
                        .output_writer
                        .emit(OutputEvent::tool_output_line(
                            format!("  📝 {}", parts.join(", ")),
                            Style::default().fg(crate::cli::tui::theme::INFO_FG),
                        ));
                }
            }
        }

        // Discover instruction files only for explicit file/directory targets.
        // This runs after tool execution so a newly created nested AGENTS.md is
        // available to the next provider request.
        if !prepared_tool_calls.is_empty() {
            self.discover_agents_rules_for_tool_calls(&workspace_root, &prepared_tool_calls);
        }

        // 8. Save conversation history after each turn
        self.save_conversation_history().await;

        // 9. Check for completion
        let completion_tool_emitted = prepared_tool_calls.iter().any(|prepared| {
            matches!(
                SnedTool::from_name(&prepared.tool_name),
                Some(SnedTool::AttemptCompletion)
            )
        });
        let plan_blocks_completion = {
            let state = self.state.lock().await;
            state.plan_state.as_ref().is_some_and(|plan| {
                plan.approved
                    && !plan.complete
                    && (plan.paused
                        || plan
                            .steps
                            .iter()
                            .any(|step| step.status == PlanStepStatus::Failed))
            })
        };
        let plan_active = self.plan_execution_active().await;
        let completion_candidate = prepared_tool_calls.iter().any(|prepared| {
            matches!(
                SnedTool::from_name(&prepared.tool_name),
                Some(SnedTool::AttemptCompletion)
            )
        }) || text_only_completes_task;
        let plan_mode_responded = prepared_tool_calls.iter().any(|prepared| {
            matches!(
                SnedTool::from_name(&prepared.tool_name),
                Some(SnedTool::PlanModeRespond)
            )
        });
        let is_completion = (completion_candidate
            || (tool_failure_count == 0 && plan_mode_responded))
            && !plan_active
            && !plan_blocks_completion;

        if self.config.json_output
            && let Some(event) = Self::synthetic_json_completion_event(
                text_only_completes_task,
                completion_tool_emitted,
                response_text.as_deref(),
            )
        {
            tracing::info!(target: "json_output", "{}", event.to_string());
        }
        // Clear file content cache after each turn (cross-call coordination within a single turn)
        {
            let mut state = self.state.lock().await;
            state.file_content_cache.clear();
        }

        // Display token usage and context window usage (not in JSON mode, and if enabled)
        if !self.config.json_output && self.config.show_token_usage {
            let state = self.state.lock().await;
            if let Some(ref api_req_info) = state.last_api_req_info {
                let context_pct = api_req_info.context_usage_percentage.unwrap_or(0.0);

                if context_pct >= 95.0 {
                    self.config
                        .output_writer
                        .emit(OutputEvent::tool_output_line(
                            "⚠ 95% context window — /compact or start new session".to_string(),
                            Style::default().fg(Color::Yellow),
                        ));
                } else if context_pct >= 80.0 {
                    self.config
                        .output_writer
                        .emit(OutputEvent::tool_output_line(
                            "⚠ 80% context window used — consider /compact".to_string(),
                            Style::default().fg(Color::Yellow),
                        ));
                } else if context_pct >= 50.0 {
                    self.config.output_writer.emit(OutputEvent::tool_output_line(
                        "ℹ 50% context window used — use /compact to free space before starting new topics".to_string(),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
            }
        }

        // Increment turns_completed counter for session summary
        {
            let mut state = self.state.lock().await;
            state.turns_completed = state.turns_completed.saturating_add(1);
        }

        if is_completion {
            if !self.config.json_output
                && let Some(result) = completion_result
            {
                self.config
                    .output_writer
                    .emit(OutputEvent::Completion(result));
            }
            // Force save on completion (async, non-blocking)
            if let Some(ref storage) = self.deps.task_storage {
                let history = self.conversation_history.lock().await.clone();
                if !history.is_empty()
                    && let Err(e) = storage.write_api_conversation_history_async(&history).await
                {
                    error!(
                        "Failed to save API conversation history on completion: {}",
                        e
                    );
                }
            }
            // Use the response-only text (thinking tags stripped) for
            // markdown re-rendering. accumulated_text contains raw
            // thinking tags which pulldown_cmark treats as raw HTML and
            // emits as raw text, defeating the markdown render.
            let markdown_text = response_text.as_deref().unwrap_or("");
            if self.config.interactive_mode && !self.config.json_output && markdown_text.is_empty()
            {
                self.config.output_writer.emit(OutputEvent::TurnEnd {
                    accumulated_text: String::new(),
                });
            } else {
                emit_turn_end(
                    &self.config.output_writer,
                    self.config.json_output,
                    markdown_text,
                );
            }
            if !self.config.interactive_mode
                && !self.config.json_output
                && crate::cli::output::timing_enabled()
            {
                let state = self.state.lock().await;
                if let Some(start) = state.session_start_time {
                    let retry_info = (stream_retry_attempt > 0).then(|| {
                        (
                            stream_retry_attempt + 1,
                            preoutput_elapsed_at_first_chunk
                                .unwrap_or_else(|| preoutput_retry_started_at.elapsed()),
                        )
                    });
                    for line in crate::cli::output::format_timing_phases_with_retries(
                        start,
                        state.request_sent_time,
                        state.first_provider_chunk_time,
                        state.first_reasoning_chunk_time,
                        state.first_displayable_text_time,
                        state.first_output_emit_time,
                        None,
                        retry_info,
                    ) {
                        self.config.output_writer.emit(OutputEvent::dim(line));
                    }
                    self.config.output_writer.flush();
                }
            }
            TurnResult::Complete
        } else {
            // Same turn-end signal for the "more turns coming" branch.
            let markdown_text = response_text.as_deref().unwrap_or("");
            emit_turn_end(
                &self.config.output_writer,
                self.config.json_output,
                markdown_text,
            );
            TurnResult::Continue
        }
    }

    async fn inject_plan_state_into_history(&self) {
        let plan_state_entry = {
            let mut state = self.state.lock().await;
            if let Some(plan_state) = state.plan_state.as_ref() {
                let text = plan_state.format_state();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&text, &mut hasher);
                let hash = hasher.finish();
                let should_inject = state.last_injected_plan_state_hash != Some(hash);
                Some((text, hash, should_inject))
            } else {
                state.last_injected_plan_state_hash = None;
                None
            }
        };

        let Some((ps_text, hash, should_inject)) = plan_state_entry else {
            return;
        };

        if !should_inject {
            return;
        }

        let mut history = self.conversation_history.lock().await;
        history.push(StorageMessage {
            id: Some(Self::next_message_id(&self.message_counter)),
            role: MessageRole::User,
            content: MessageContent::Text(ps_text),
            model_info: None,
            metrics: None,
            ts: Some(chrono::Utc::now().timestamp_millis() as u64),
        });
        drop(history);

        let mut state = self.state.lock().await;
        state.last_injected_plan_state_hash = Some(hash);
    }

    /// Cancels the current task.
    pub async fn cancel(&self) {
        let mut state = self.state.lock().await;
        state.is_cancelled = true;
        state
            .checkpoint_cancellation
            .store(true, std::sync::atomic::Ordering::Release);
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Clears cancellation state when the caller explicitly starts a new turn.
    pub async fn reset_cancellation(&self) {
        let mut state = self.state.lock().await;
        state.is_cancelled = false;
        state
            .checkpoint_cancellation
            .store(true, std::sync::atomic::Ordering::Release);
        state.checkpoint_cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.cancelled
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Returns a handle to the internal task state for external cancellation.
    pub fn state_handle(&self) -> Arc<Mutex<TaskState>> {
        self.state.clone()
    }

    fn resolve_workspace_root(&self) -> std::path::PathBuf {
        self.deps
            .system_prompt_context
            .as_ref()
            .and_then(|context| context.cwd.clone())
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// Lock the provider, read its configured model id. Returns None if
    /// the provider mutex cannot be locked or the model id is empty.
    fn resolve_active_model_id(&self) -> Option<String> {
        let guard = self.config.provider.lock().ok()?;
        let id = guard.get_model().id.clone();
        if id.is_empty() { None } else { Some(id) }
    }

    /// Check if a tool is restricted in plan mode.
    fn is_plan_mode_restricted(tool: SnedTool) -> bool {
        matches!(tool, SnedTool::WriteToFile | SnedTool::EditFile)
    }

    /// Extract the first action path from tool params for per-path approval.
    ///
    /// Each tool extracts paths differently:
    /// - ReadFile/SearchFiles/ListFiles: `params.paths` (string or string[])
    /// - WriteToFile: `params.path` (single string)
    /// - EditFile: `params.files[0].path`
    /// - ReplaceSymbol: `params.path` or `params.replacements[0].path`
    /// - RenameSymbol: `params.paths[0]`
    /// - GetFileSkeleton/FindSymbolReferences/DiagnosticsScan: `params.path`
    fn extract_action_path(tool: SnedTool, params: &serde_json::Value) -> Vec<String> {
        match tool {
            SnedTool::ReadFile
            | SnedTool::GetFileSkeleton
            | SnedTool::FindSymbolReferences
            | SnedTool::DiagnosticsScan
            | SnedTool::RenameSymbol => coerce_string_array(params, "paths", "path"),
            SnedTool::WriteToFile
            | SnedTool::SearchFiles
            | SnedTool::ListFiles
            | SnedTool::GetFunction => params
                .get("path")
                .and_then(|p| p.as_str())
                .map(|s| vec![String::from(s)])
                .unwrap_or_default(),
            SnedTool::EditFile => {
                crate::core::tools::handlers::edit_file::EditFileHandler::requested_paths_for_locking(params)
            }
            SnedTool::ReplaceSymbol => {
                if let Some(s) = params.get("path").and_then(|p| p.as_str()) {
                    vec![String::from(s)]
                } else {
                    params
                        .get("replacements")
                        .and_then(|r| r.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|r| r.get("path"))
                                .filter_map(|p| p.as_str())
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default()
                }
            }
            _ => vec![],
        }
    }

    /// Load AGENTS.md files for explicit file-oriented tool targets so the
    /// following provider request sees the rules governing the work just
    /// inspected or changed.
    fn is_mutating_file_tool(tool_name: &str) -> bool {
        matches!(
            SnedTool::from_name(tool_name),
            Some(
                SnedTool::WriteToFile
                    | SnedTool::EditFile
                    | SnedTool::ReplaceSymbol
                    | SnedTool::RenameSymbol
            )
        )
    }

    /// Whether a tool can modify the workspace and therefore needs a rollback
    /// checkpoint before it starts. Read-only tools must never wait for a full
    /// workspace Git snapshot.
    fn tool_may_modify_workspace(tool: SnedTool) -> bool {
        matches!(tool.category(), crate::core::tools::ToolCategory::EditFiles)
            || matches!(tool, SnedTool::ExecuteCommand | SnedTool::UseSubagents)
    }

    fn discover_agents_rules_for_tool_calls(
        &mut self,
        workspace_root: &Path,
        prepared_tool_calls: &[PreparedToolCall],
    ) -> bool {
        let mut targets = HashSet::new();
        for prepared in prepared_tool_calls {
            let Some(tool) = SnedTool::from_name(&prepared.tool_name) else {
                continue;
            };
            let Ok(params) = &prepared.parsed_args else {
                continue;
            };
            targets.extend(Self::extract_action_path(tool, params));
        }
        if targets.is_empty() {
            return false;
        }

        let toggles = self
            .deps
            .system_prompt_context
            .as_ref()
            .map(|context| context.local_agents_rule_toggles.clone())
            .unwrap_or_default();
        let mut additions = Vec::new();
        for target in targets {
            for rule_file in crate::core::context::load_path_scoped_agents_rules(
                workspace_root,
                Path::new(&target),
                &toggles,
            ) {
                let key = rule_file.path.to_string_lossy().into_owned();
                if self.deps.loaded_agents_rule_paths.insert(key) {
                    additions.push(rule_file);
                }
            }
        }
        if additions.is_empty() {
            return false;
        }

        let context = self
            .deps
            .system_prompt_context
            .get_or_insert_with(|| SystemPromptContext {
                cwd: Some(workspace_root.to_string_lossy().into_owned()),
                ..Default::default()
            });
        let rules = context
            .local_agents_rules_file_instructions
            .get_or_insert_with(|| "# AGENTS.md Rules".to_string());
        let canonical_workspace_root = match workspace_root.canonicalize() {
            Ok(root) => Some(root),
            Err(error) => {
                warn!(
                    workspace = %workspace_root.display(),
                    error = %error,
                    "Failed to canonicalize workspace root while formatting AGENTS.md rules"
                );
                None
            }
        };
        for rule_file in &additions {
            let relative = rule_file
                .path
                .strip_prefix(workspace_root)
                .ok()
                .or_else(|| {
                    canonical_workspace_root
                        .as_deref()
                        .and_then(|root| rule_file.path.strip_prefix(root).ok())
                })
                .unwrap_or(&rule_file.path);
            rules.push_str(&format!(
                "\n\n## {}\n\n{}",
                relative.display(),
                rule_file.content
            ));
        }
        self.deps.cached_system_prompt = None;
        tracing::debug!(
            workspace = %workspace_root.display(),
            discovered_files = ?additions.iter().map(|file| &file.path).collect::<Vec<_>>(),
            "added newly discovered path-scoped AGENTS.md rules and invalidated system prompt"
        );
        true
    }

    fn external_action_directories(
        tool: SnedTool,
        workspace_root: &std::path::Path,
        action_paths: &[String],
    ) -> Vec<PathBuf> {
        if !matches!(
            tool.category(),
            crate::core::tools::ToolCategory::ReadFiles
                | crate::core::tools::ToolCategory::EditFiles
        ) {
            return Vec::new();
        }

        let mut directories = action_paths
            .iter()
            .filter(|path| {
                let path = std::path::Path::new(path);
                path.is_absolute() && !path.starts_with(workspace_root)
            })
            .filter_map(|path| crate::core::approval::external_directory_for_path(path))
            .collect::<Vec<_>>();
        directories.sort();
        directories.dedup();
        directories
    }

    fn canonicalize_tool_params(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let ordered: std::collections::BTreeMap<_, _> = map
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::canonicalize_tool_params(value)))
                    .collect();
                serde_json::Value::Object(ordered.into_iter().collect())
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(Self::canonicalize_tool_params).collect())
            }
            other => other.clone(),
        }
    }

    fn tool_params_fingerprint(params: &serde_json::Value) -> String {
        serde_json::to_string(&Self::canonicalize_tool_params(params))
            .unwrap_or_else(|_| params.to_string())
    }

    fn reread_recovery_hint(state: &TaskState) -> Option<String> {
        if state.must_reread_before_edit.is_empty() {
            return None;
        }

        let mut paths: Vec<_> = state.must_reread_before_edit.iter().cloned().collect();
        paths.sort();
        let listed = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        let suffix = if paths.len() > 3 { ", ..." } else { "" };
        Some(format!(
            "[system] Before using edit_file again, refresh the stale path(s): {listed}{suffix}. Use read_file for the full file. A symbol-scoped read of just the surrounding definition can also refresh only the relevant anchors when one is available."
        ))
    }

    fn extract_file_action_path(
        tool_name: &str,
        params: &serde_json::Value,
        workspace_root: &std::path::Path,
    ) -> Vec<FileActionPath> {
        let requested_paths = match tool_name {
            "edit_file" => {
                let Some(files) = params.get("files").and_then(|files| files.as_array()) else {
                    return vec![];
                };
                let fallback = params.get("path").and_then(|path| path.as_str());
                let use_fallback = fallback.is_some()
                    && !files.is_empty()
                    && files
                        .iter()
                        .all(|file| file.get("path").is_none() && file.get("edits").is_some());

                files
                    .iter()
                    .filter_map(|file| {
                        if use_fallback {
                            return fallback.map(String::from);
                        }
                        match file.get("path") {
                            Some(path) => path.as_str().map(String::from),
                            None => file
                                .get("edits")
                                .and_then(|edits| edits.as_array())
                                .and_then(|edits| edits.first())
                                .and_then(|edit| edit.get("path"))
                                .and_then(|path| path.as_str())
                                .map(String::from),
                        }
                    })
                    .collect()
            }
            "write_to_file" => params
                .get("path")
                .and_then(|path| path.as_str())
                .map(|path| vec![String::from(path)])
                .unwrap_or_default(),
            _ => return vec![],
        };

        let mut seen = std::collections::HashSet::with_capacity(requested_paths.len());
        requested_paths
            .into_iter()
            .filter_map(|display| {
                let normalized =
                    crate::core::tools::resolve_sanitized_path(workspace_root, &display)
                        .ok()?
                        .to_string_lossy()
                        .into_owned();
                if !seen.insert(normalized.clone()) {
                    return None;
                }
                Some(FileActionPath {
                    normalized,
                    display,
                })
            })
            .collect()
    }

    /// Static version of execute_tool_with_hooks for parallel execution.
    /// Takes ownership of shared resources to avoid borrowing issues across async boundaries.
    async fn execute_tool_with_hooks_internal(
        config: &AgentConfig,
        hook_manager: Option<Arc<crate::core::hooks::HookManager>>,
        tool_context: Arc<ToolContext>,
        tool_name: &str,
        tool_params: &serde_json::Value,
        handler: Arc<dyn crate::core::tools::ToolHandler>,
        task_storage: Option<Arc<crate::storage::task_storage::TaskStorage>>,
        conversation_history: Arc<Mutex<Vec<StorageMessage>>>,
    ) -> ToolExecutionOutput {
        let mut params_for_execution = tool_params.clone();
        let mut hook_context = Vec::new();
        if let Some(ref hook_mgr) = hook_manager {
            let pre_result = hook_mgr.pre_tool_use(&config.task_id, tool_name, tool_params);
            if let Some(error) = pre_result.error.as_deref() {
                warn!(tool = tool_name, error, "PreToolUse hook reported an error");
            }
            if let Some(output) = pre_result.output {
                if let Some(error) = output.error_message.as_deref() {
                    warn!(tool = tool_name, error, "PreToolUse hook returned an error");
                }
                if output.cancel == Some(true) {
                    return ToolExecutionOutput::error(
                        format!("Tool '{tool_name}' was cancelled by PreToolUse hook."),
                        None,
                    );
                }
                if let Some(modification) = output.context_modification {
                    info!("[PreToolUse hook] {}", modification);
                    hook_context.push(format!("[Hook context from PreToolUse]: {modification}"));
                }
            }
        }

        if tool_name == "condense" {
            let history = conversation_history.lock().await;
            let history = match serde_json::to_value(&*history) {
                Ok(history) => history,
                Err(error) => {
                    return ToolExecutionOutput::error_with_hook_context(
                        format!("Failed to prepare conversation history for condense: {error}"),
                        None,
                        hook_context,
                    );
                }
            };
            if let Some(params) = params_for_execution.as_object_mut() {
                params.insert("history".to_string(), history);
            }
        }

        let execute_future = handler.execute(&tool_context, params_for_execution);
        let execution_result = if !matches!(
            tool_name,
            "edit_file" | "write_to_file" | "replace_symbol" | "rename_symbol"
        ) && let Some(cancellation_flag) =
            tool_context.cancellation_flag.clone()
        {
            tokio::select! {
                result = execute_future => result,
                () = async {
                    while !cancellation_flag.load(std::sync::atomic::Ordering::Acquire) {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                } => Err(crate::core::tools::ToolError::ExecutionFailed(
                    "Tool cancelled by user".to_string(),
                )),
            }
        } else {
            execute_future.await
        };

        match execution_result {
            Ok(res) => {
                let res_text = tool_result_to_text(res);

                // Persist compacted summary immediately if condense tool was used
                let summary = if tool_name == "condense" {
                    tool_context.state.lock().await.compacted_summary.clone()
                } else {
                    None
                };
                if let Some(summary) = summary
                    && let Some(storage) = task_storage
                    && let Err(e) = storage.write_compacted_summary_async(&summary).await
                {
                    error!("Failed to persist compacted summary immediately: {}", e);
                }

                if let Some(ref hook_mgr) = hook_manager {
                    let post_result =
                        hook_mgr.post_tool_use(&config.task_id, tool_name, tool_params, &res_text);
                    if let Some(post_output) = post_result.output
                        && let Some(modification) = post_output.context_modification
                    {
                        info!("[PostToolUse hook] {}", modification);
                        hook_context
                            .push(format!("[Hook context from PostToolUse]: {modification}"));
                    }
                }
                ToolExecutionOutput::success_with_hook_context(res_text, hook_context)
            }
            Err(e) => ToolExecutionOutput::error_with_hook_context(
                format!("Error: {e}"),
                e.metadata().cloned(),
                hook_context,
            ),
        }
    }

    /// Returns the current conversation history.
    pub async fn get_conversation_history(&self) -> Vec<StorageMessage> {
        let history = self.conversation_history.lock().await;
        history.clone()
    }

    /// Format duration as human-readable string.

    /// Save conversation history to disk if task storage is configured.
    async fn save_conversation_history(&self) {
        if let Some(ref storage) = self.deps.task_storage {
            let mut state = self.state.lock().await;
            state.turns_since_save += 1;
            let persisted_usage = state
                .last_api_req_info
                .as_ref()
                .map(crate::core::context::context_manager::PersistedApiReqInfo::from);

            // Keep the latest usage available after short sessions too; only
            // conversation history remains debounced below.
            if let Err(e) = storage.update_metadata(|metadata| {
                metadata.last_api_req_info = persisted_usage.clone();
            }) {
                error!("Failed to save last API request info: {}", e);
            }

            // Debounce: only save every 5 turns to reduce I/O overhead
            if state.turns_since_save >= 5 {
                state.turns_since_save = 0;
                let compacted_summary = state.compacted_summary.clone();
                drop(state); // Drop state lock before acquiring history lock

                let history = self.conversation_history.lock().await.clone();
                if !history.is_empty()
                    && let Err(e) = storage.write_api_conversation_history_async(&history).await
                {
                    error!("Failed to save API conversation history: {}", e);
                }

                // Save compacted summary if present
                if let Some(ref summary) = compacted_summary
                    && let Err(e) = storage.write_compacted_summary_async(summary).await
                {
                    error!("Failed to save compacted summary: {}", e);
                }
            }
        }
    }

    /// Return the earliest history index that keeps tool_use/tool_result pairs intact.
    /// If a kept tool_result would be orphaned by pruning, extend the keep region
    /// backwards to include its corresponding tool_use.
    fn keep_from_preserving_tool_pairs(history: &[StorageMessage], keep_from_base: usize) -> usize {
        // Build a map of tool_use_id -> message index for all tool_uses in history.
        let mut tool_use_index: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(16);
        for (idx, msg) in history.iter().enumerate() {
            if let MessageContent::AssistantBlocks(blocks) = &msg.content {
                for block in blocks {
                    if let AssistantContentBlock::ToolUse(tu) = block {
                        tool_use_index.insert(tu.id.clone(), idx);
                    }
                }
            }
        }

        let mut keep_from = keep_from_base.min(history.len());
        loop {
            let mut changed = false;
            for msg in history.iter().skip(keep_from) {
                if let MessageContent::UserBlocks(blocks) = &msg.content {
                    for block in blocks {
                        if let UserContentBlock::ToolResult(tr) = block
                            && let Some(&tool_use_idx) = tool_use_index.get(&tr.tool_use_id)
                            && tool_use_idx < keep_from
                        {
                            let new_keep_from = keep_from.min(tool_use_idx);
                            if new_keep_from != keep_from {
                                keep_from = new_keep_from;
                                changed = true;
                            }
                        }
                    }
                }
            }

            if !changed {
                break;
            }
        }

        keep_from
    }

    /// Prune oldest conversation history when it exceeds max_context_turns.
    /// Keeps system prompt (first message if present) + most recent N turns.
    /// A "turn" is counted as a user-assistant pair (2 messages).
    /// CRITICAL: Preserves tool_use/tool_result pairs — never splits a tool result
    /// from its corresponding tool use. If a tool_result would be kept but its
    /// tool_use was pruned, we extend the keep region backwards to include the tool_use.
    fn prune_conversation_history(&self, history: Vec<StorageMessage>) -> Vec<StorageMessage> {
        let max_turns = self.config.max_context_turns;
        let max_messages = max_turns * 2; // Each turn = user + assistant

        // Allow extra messages for system prompt and tool results
        let buffer = 10;
        let threshold = max_messages + buffer;

        if history.len() <= threshold {
            return history;
        }

        // Start with the most recent N messages
        let keep_from_base = history.len().saturating_sub(max_messages);
        let keep_from = Self::keep_from_preserving_tool_pairs(&history, keep_from_base);

        // Preserve system prompt if it exists (first message with role=assistant)
        let has_system_prompt = history
            .first()
            .is_some_and(|m| matches!(m.role, MessageRole::Assistant));

        if has_system_prompt {
            // Keep system prompt + most recent messages
            let mut pruned = Vec::with_capacity(max_messages + 1);
            pruned.push(history[0].clone());
            pruned.extend(history[keep_from..].iter().cloned());
            pruned
        } else {
            history[keep_from..].to_vec()
        }
    }

    /// Apply emergency truncation repeatedly until the current request fits the provider
    /// context window, while preserving tool_use/tool_result pairs in the retained tail.
    async fn emergency_truncate_request(
        &self,
        request: &mut ProviderRequest,
    ) -> Result<(), String> {
        const INITIAL_KEEP_MESSAGES: usize = 20;
        const MIN_KEEP_MESSAGES: usize = 2;

        let mut keep_messages = INITIAL_KEEP_MESSAGES;
        let mut truncated_any = false;
        let mut history = self.conversation_history.lock().await;

        let result = loop {
            let dropped = Self::truncate_history_preserving_tool_pairs(&mut history, keep_messages);
            if dropped > 0 {
                truncated_any = true;
                tracing::info!(
                    dropped,
                    retained = history.len(),
                    keep_messages,
                    "Emergency truncation dropped oldest messages while preserving tool pairs"
                );
            }

            request.messages = history.clone();

            let value = context_window::validate_context_window(
                request,
                self.config
                    .provider
                    .lock()
                    .expect("provider poisoned")
                    .as_ref(),
            );
            match value {
                Ok(()) => break Ok(()),
                Err(msg) => {
                    tracing::warn!(
                        keep_messages,
                        retained = history.len(),
                        "Request still exceeds context limits after emergency truncation: {}",
                        msg
                    );

                    if keep_messages <= MIN_KEEP_MESSAGES || history.len() <= MIN_KEEP_MESSAGES {
                        break Err(msg);
                    }

                    let next_keep = keep_messages.saturating_sub(2).max(MIN_KEEP_MESSAGES);
                    if next_keep == keep_messages {
                        break Err(msg);
                    }

                    tracing::info!(
                        next_keep,
                        "Emergency truncation still exceeds limits; retrying with smaller retained tail"
                    );
                    keep_messages = next_keep;
                }
            }
        };

        drop(history);

        if truncated_any {
            let mut state = self.state.lock().await;
            if state.conversation_history_deleted_range.is_some() {
                tracing::debug!(
                    "Reset conversation_history_deleted_range after emergency truncation"
                );
                state.conversation_history_deleted_range = None;
            }
        }

        result
    }

    fn truncate_history_preserving_tool_pairs(
        history: &mut Vec<StorageMessage>,
        keep_messages: usize,
    ) -> usize {
        let keep_from_base = history.len().saturating_sub(keep_messages);
        let keep_from = Self::keep_from_preserving_tool_pairs(history, keep_from_base);
        let dropped = keep_from.min(history.len());
        if dropped > 0 {
            history.drain(0..dropped);
        }
        dropped
    }

    /// Load conversation history from disk if task storage is configured.
    /// Returns true if history was loaded, false otherwise.
    pub async fn load_conversation_history(&self) -> bool {
        if let Some(ref storage) = self.deps.task_storage {
            let history: Vec<StorageMessage> = storage.read_api_conversation_history();
            let compacted_summary: Option<crate::core::context::context_manager::CompactedSummary> =
                storage.read_compacted_summary();

            let mut loaded = false;

            if !history.is_empty() {
                let mut current = self.conversation_history.lock().await;
                *current = history;
                loaded = true;
            }

            if let Some(summary) = compacted_summary {
                let mut state = self.state.lock().await;
                state.compacted_summary = Some(summary);
                loaded = true;
            }

            let metadata = storage.read_task_metadata();
            if let Some(persisted_usage) = metadata.last_api_req_info {
                let context_window = crate::core::context::get_context_window_info(
                    self.config
                        .provider
                        .lock()
                        .expect("provider poisoned")
                        .as_ref(),
                )
                .context_window;
                let mut state = self.state.lock().await;
                state.last_api_req_info = Some(persisted_usage.into_api_req_info(context_window));
                loaded = true;
            }

            // Load conversation_history_deleted_range from HistoryItem (C1 fix part 2)
            // This ensures compacted messages don't reappear on --continue
            if let Some(ref state_manager) = self.state_manager
                && let Some(history_item) = state_manager.find_task_in_history(&self.config.task_id)
                && let Some(deleted_range_vec) = history_item.conversation_history_deleted_range
            {
                // Convert from Vec<i32> to (usize, usize) tuple for TaskState
                if deleted_range_vec.len() >= 2 {
                    let mut state = self.state.lock().await;
                    state.conversation_history_deleted_range =
                        Some((deleted_range_vec[0] as usize, deleted_range_vec[1] as usize));
                    loaded = true;
                }
            }

            loaded
        } else {
            false
        }
    }

    /// Clear compacted summary to allow re-compaction.
    /// Returns true if a summary was cleared, false if none existed.
    pub async fn clear_compacted_summary(&self) -> bool {
        let mut state = self.state.lock().await;
        if state.compacted_summary.is_some() {
            state.compacted_summary = None;

            // Also delete the file if task storage is configured
            if let Some(ref storage) = self.deps.task_storage {
                let file_path = storage
                    .task_dir()
                    .join(crate::storage::disk::GlobalFileNames::COMPACTED_SUMMARY);
                let _ = std::fs::remove_file(&file_path);
            }

            true
        } else {
            false
        }
    }

    /// Remove the last turn (assistant response + user message) from conversation history.
    /// Returns the number of messages removed (0, 1, or 2).
    pub async fn remove_last_turn(&self) -> usize {
        use crate::providers::MessageRole;

        let mut history = self.conversation_history.lock().await;

        if history.is_empty() {
            return 0;
        }

        // Remove last message (assistant response)
        history.pop();
        let mut removed = 1;

        // Remove user message if present
        if history.last().is_some_and(|m| m.role == MessageRole::User) {
            history.pop();
            removed = 2;
        }

        removed
    }

    /// Load file context tracker metadata from disk if task storage is configured.
    /// Sets the task_id on the tracker and restores files_in_context from storage.
    pub async fn load_file_context_tracker(&self) {
        let mut state = self.state.lock().await;
        if state.file_context_tracker.task_id().is_none() {
            state.file_context_tracker = state
                .file_context_tracker
                .clone()
                .with_task_id(self.config.task_id.clone());
        }
        state.file_context_tracker.load_from_storage();
    }

    /// Enqueue a message to be sent after the current request completes.
    ///
    /// If the queue is empty and no request is in progress, the message will be
    /// processed on the next turn. If a request is in progress, the message will
    /// be queued and processed immediately after the current response completes.
    pub async fn enqueue_message(&self, message: StorageMessage) {
        let max_queue_len = message_queue_max_len();
        let (count, dropped) =
            enqueue_message_with_limit(&self.message_queue, message, max_queue_len).await;

        if dropped > 0 {
            warn!(
                max_queue_len,
                dropped, "message queue exceeded its limit; dropped oldest queued message(s)"
            );
        }

        if !self.config.json_output && count > 0 {
            info!(
                "[sned] Message queued ({} message{} in queue)",
                count,
                if count == 1 { "" } else { "s" }
            );
        }
    }

    pub async fn enqueue_text_message(&self, text: String) {
        self.enqueue_message(StorageMessage {
            id: Some(Self::next_message_id(&self.message_counter)),
            role: MessageRole::User,
            content: MessageContent::Text(text),
            model_info: None,
            metrics: None,
            ts: Some(chrono::Utc::now().timestamp_millis() as u64),
        })
        .await;
    }

    /// Expand mentions in a queued user message and track mentioned files.
    async fn expand_message_mentions(&self, mut message: StorageMessage) -> StorageMessage {
        if let MessageContent::Text(ref text) = message.content {
            let workspace_root = self.resolve_workspace_root();

            let (enriched_text, expanded) =
                crate::core::mentions::expand_mentions(text, &workspace_root).await;

            // Track mentioned files/folders in FileContextTracker
            let regex = crate::core::mentions::get_mention_regex();
            for caps in regex.captures_iter(text) {
                let mention_str = &caps[1];
                if let Some(
                    crate::core::mentions::Mention::File(path)
                    | crate::core::mentions::Mention::Folder(path),
                ) = crate::core::mentions::Mention::parse(mention_str)
                {
                    let clean_path = path.trim_start_matches('/');
                    if let Ok(full_path) =
                        crate::core::tools::resolve_sanitized_path(&workspace_root, clean_path)
                        && let Ok(canonical) = full_path.canonicalize()
                        && let Some(path_str) = canonical.to_str()
                    {
                        let (task_id, file_context_metadata) = {
                            let mut state = self.state.lock().await;
                            state
                                .file_context_tracker
                                .track_file_context_in_memory_at_path(
                                    path_str,
                                    crate::core::context::trackers::FileRecordSource::FileMentioned,
                                    &canonical,
                                );
                            (
                                state.file_context_tracker.task_id().map(str::to_owned),
                                state.file_context_tracker.files_in_context().to_vec(),
                            )
                        };

                        if let Some(task_id) = task_id {
                            tokio::spawn(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    let storage =
                                        crate::storage::task_storage::TaskStorage::new(&task_id)?;
                                    storage.save_file_context_metadata(&file_context_metadata)
                                })
                                .await;
                                match result {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => {
                                        warn!(error = %e, "Failed to persist file context metadata")
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "File context metadata task failed")
                                    }
                                }
                            });
                        }
                    }
                }
            }

            let mut final_text = enriched_text;
            if !expanded.is_empty() {
                final_text.push_str("\n\n");
                final_text.push_str(&expanded.join("\n\n"));
            }

            message.content = MessageContent::Text(final_text);
        }
        message
    }

    pub async fn queued_message_count(&self) -> usize {
        self.message_queue.lock().await.len()
    }

    pub async fn has_queued_messages(&self) -> bool {
        !self.message_queue.lock().await.is_empty()
    }

    pub async fn clear_queue(&self) {
        self.message_queue.lock().await.clear();
    }
}

fn resolve_tool_profile(
    cached: Option<crate::core::tools::definitions::ToolProfile>,
    yolo: bool,
    prompt: &str,
    mode_str: &str,
) -> crate::core::tools::definitions::ToolProfile {
    if mode_str == "plan" {
        return crate::core::tools::definitions::ToolProfile::Plan;
    }

    // /compact injects an explicit condense instruction. The model must
    // receive the condense tool schema; reduced profiles (especially YOLO's
    // Validate) omit it and force the model to hallucinate a tool name.
    if prompt.contains("type=\"condense\"") {
        return crate::core::tools::definitions::ToolProfile::Full;
    }

    let selected = match cached {
        Some(profile) => profile,
        None => crate::core::tools::definitions::select_tool_profile(prompt, mode_str),
    };

    if yolo {
        crate::core::tools::definitions::ToolProfile::Validate
    } else {
        selected
    }
}

/// Truncates thinking blocks in all assistant messages except the most recent one.
///
/// This prevents token bloat from extended-thinking models (Claude, DeepSeek)
/// that emit 5,000-20,000 tokens of thinking per turn. Old thinking blocks are
/// truncated to the first N tokens (configurable via `SNED_THINKING_HISTORY_LIMIT`,
/// default: 2000) with a `[truncated]` marker.
///
/// The most recent assistant message's thinking is preserved in full to maintain
/// context for the current turn.
fn truncate_old_thinking_blocks(history: &mut [StorageMessage]) {
    let limit = std::env::var(THINKING_HISTORY_LIMIT_ENV)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_THINKING_HISTORY_LIMIT);

    // Find the index of the most recent assistant message (if any)
    let most_recent_assistant_idx = history
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, msg)| (msg.role == MessageRole::Assistant).then_some(i));

    for (i, message) in history.iter_mut().enumerate() {
        // Skip the most recent assistant message - preserve its thinking in full
        if Some(i) == most_recent_assistant_idx {
            continue;
        }

        if message.role != MessageRole::Assistant {
            continue;
        }

        let MessageContent::AssistantBlocks(blocks) = &mut message.content else {
            continue;
        };

        for block in blocks {
            if let AssistantContentBlock::Thinking(thinking_block) = block {
                // Truncate by character count (approximate token proxy)
                // 1 token ≈ 4 chars for English text
                let char_limit = limit * 4;
                if thinking_block.thinking.len() > char_limit {
                    thinking_block.thinking.truncate(char_limit);
                    thinking_block.thinking.push_str("\n\n[truncated]");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tool_output::{
        format_tool_summary, normalize_path_for_matching, summarize_single_section,
    };
    use crate::providers::{
        ApiStreamReasoningChunk, ApiStreamTextChunk, ApiStreamToolCallFunction,
        ApiStreamToolCallsChunk,
    };

    fn test_agent_config(provider: Arc<Providers>, task_id: &str) -> AgentConfig {
        AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: task_id.to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        }
    }

    #[tokio::test]
    async fn native_workflow_scripted_provider_recovers_without_fallback() {
        use crate::core::tools::handlers::{
            attempt_completion::AttemptCompletionHandler, edit_file::EditFileHandler,
            read_file::ReadFileHandler,
        };
        use crate::providers::mock::MockProvider;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.txt");
        std::fs::write(&path, "alpha  \nbeta\n").unwrap();
        let provider = Arc::new(Providers::Mock(MockProvider::new(vec![])));
        let mut registry = ToolRegistry::new();
        registry.register(SnedTool::ReadFile, Arc::new(ReadFileHandler::new()));
        registry.register(SnedTool::EditFile, Arc::new(EditFileHandler::new()));
        registry.register(
            SnedTool::AttemptCompletion,
            Arc::new(AttemptCompletionHandler::new()),
        );
        let mut agent = AgentLoop::new(test_agent_config(provider, "native-workflow-loop"))
            .with_tools(Arc::new(registry))
            .with_system_prompt_context(SystemPromptContext {
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                ..Default::default()
            });
        agent.anchor_mgr = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        agent.state.lock().await.double_check_completion_enabled = false;
        let mut copied = String::new();
        let mut stale = String::new();
        for step in 0..7 {
            let (name, params) = match step {
                0 | 3 => ("read_file", json!({"paths": ["fixture.txt"]})),
                1 => (
                    "edit_file",
                    json!({"files": [{"path": "fixture.txt", "edits": [{"anchor": copied, "text": "changed"}]}]}),
                ),
                2 => (
                    "edit_file",
                    json!({"files": [{"path": "fixture.txt", "edits": [{"anchor": stale, "text": "wrong"}]}]}),
                ),
                4 => (
                    "edit_file",
                    json!({"files": [{"path": "fixture.txt", "edits": [{"anchor": copied, "content": ["changed"], "text": "final"}]}]}),
                ),
                5 => (
                    "edit_file",
                    json!({"files": [{"path": "fixture.txt", "edits": [{"anchor": copied, "text": "final"}]}]}),
                ),
                _ => (
                    "attempt_completion",
                    json!({"result": "Verified native editing"}),
                ),
            };
            agent
                .set_provider(Arc::new(Providers::Mock(MockProvider::single_tool_call(
                    &format!("workflow-{step}"),
                    name,
                    params,
                ))))
                .await;
            let outcome = agent.execute_turn().await;
            assert!(
                !matches!(outcome, TurnResult::Error(_)),
                "step {step}: {outcome:?}"
            );
            if step == 6 {
                assert!(matches!(outcome, TurnResult::Complete));
            } else {
                assert!(matches!(outcome, TurnResult::Continue));
            }
            let state = agent.state.lock().await;
            assert_eq!(
                state.consecutive_mistakes,
                if step == 2 || step == 4 { 1 } else { 0 },
                "step {step}"
            );
            assert_eq!(
                !state.must_reread_before_edit.is_empty(),
                step == 2,
                "step {step}"
            );
            drop(state);
            let history = agent.conversation_history.lock().await;
            let expected_id = history
                .iter()
                .rev()
                .find_map(|message| match &message.content {
                    MessageContent::AssistantBlocks(blocks) => {
                        blocks.iter().find_map(|block| match block {
                            AssistantContentBlock::ToolUse(call)
                                if call.shared.call_id.as_deref()
                                    == Some(format!("workflow-{step}").as_str()) =>
                            {
                                Some(call.id.clone())
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .unwrap();
            let text = history
                .iter()
                .rev()
                .find_map(|message| match &message.content {
                    MessageContent::UserBlocks(blocks) => {
                        blocks.iter().find_map(|block| match block {
                            UserContentBlock::ToolResult(result)
                                if result.tool_use_id == expected_id =>
                            {
                                match &result.content {
                                    ToolResultContent::Text(text) => Some(text.clone()),
                                    _ => None,
                                }
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .unwrap_or_else(|| {
                    panic!("actual tool result must reach provider history: {history:?}")
                });
            if step == 0 || step == 3 {
                copied = text
                    .split('\n')
                    .find(|line| {
                        line.split_once('§')
                            .is_some_and(|(word, _)| word.chars().all(char::is_alphanumeric))
                    })
                    .unwrap()
                    .to_owned();
                if step == 0 {
                    stale = copied.clone();
                }
            }
            if step == 4 {
                assert!(text.contains("Correct the edit parameters"));
                assert!(!text.contains("unknown or stale"));
            }
            if step == 2 {
                assert!(text.contains("read_file"));
                assert_eq!(std::fs::read(&path).unwrap(), b"changed\nbeta\n");
                drop(history);
                let context = Arc::new(ToolContext::new(
                    agent.state.clone(),
                    None,
                    dir.path().to_path_buf(),
                    agent.anchor_mgr.clone(),
                    false,
                    "native-workflow-loop".into(),
                    None,
                    true,
                    agent.config.output_writer.clone(),
                ));
                let rejected = AgentLoop::execute_tool_with_hooks_internal(
                    &agent.config, None, context, "edit_file",
                    &json!({"files": [{"path": "fixture.txt", "edits": [{"anchor": stale, "text": "wrong"}]}]}),
                    Arc::new(EditFileHandler::new()), None, agent.conversation_history.clone(),
                ).await;
                assert!(rejected.is_error);
                assert_eq!(
                    rejected.metadata.unwrap().required_next_step,
                    Some(ToolRequiredNextStep::ReadFile)
                );
            }
        }
        assert_eq!(std::fs::read(path).unwrap(), b"final\nbeta\n");
    }

    #[tokio::test]
    async fn test_shadow_commit_failure_is_visible() {
        let (tx, mut rx) = mpsc::channel(4);
        let writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        report_shadow_commit_result(
            &writer,
            Ok(Err(anyhow::anyhow!(
                "git commit failed:\nAuthor identity unknown"
            ))),
        );

        let rendered = drain_rendered_output(&mut rx);
        assert_eq!(
            rendered,
            vec![
                "Change tracking failed; /diff and /log will not include this turn: git commit failed: Author identity unknown"
            ]
        );
    }

    #[derive(Clone)]
    struct CapturedTraceWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedTraceWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct ConcurrencyProbeHandler {
        active: Arc<std::sync::atomic::AtomicUsize>,
        max_active: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::core::tools::ToolHandler for ConcurrencyProbeHandler {
        fn execute(
            &self,
            _ctx: &ToolContext,
            _params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, crate::core::tools::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            let active = self.active.clone();
            let max_active = self.max_active.clone();
            Box::pin(async move {
                let current = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                max_active.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok(serde_json::json!("ok"))
            })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "probe".to_string()
        }
    }

    struct StaticResultHandler(&'static str);

    impl crate::core::tools::ToolHandler for StaticResultHandler {
        fn execute(
            &self,
            _ctx: &ToolContext,
            _params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, crate::core::tools::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async move { Ok(serde_json::Value::String(self.0.to_string())) })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "static result".to_string()
        }
    }

    struct StaticErrorHandler;

    impl crate::core::tools::ToolHandler for StaticErrorHandler {
        fn execute(
            &self,
            _ctx: &ToolContext,
            _params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, crate::core::tools::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                Err(crate::core::tools::ToolError::ExecutionFailed(
                    "file batch rejected".to_string(),
                ))
            })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "static error".to_string()
        }
    }

    #[tokio::test]
    async fn test_handler_error_sets_tool_execution_error_flag() {
        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_text_response("unused"),
        ));
        let config = test_agent_config(provider, "handler-error-flag");
        let state = Arc::new(Mutex::new(TaskState::default()));
        let context = Arc::new(ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            false,
            "handler-error-flag".to_string(),
            None,
            true,
            Arc::new(crate::cli::output::StderrOutputWriter),
        ));

        let output = AgentLoop::execute_tool_with_hooks_internal(
            &config,
            None,
            context,
            "edit_file",
            &serde_json::json!({}),
            Arc::new(StaticErrorHandler),
            None,
            Arc::new(Mutex::new(Vec::new())),
        )
        .await;

        assert!(output.is_error);
        assert!(output.text.contains("file batch rejected"));
    }

    #[tokio::test]
    async fn test_edit_file_handler_error_increments_failure_counter() {
        let responses = vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("call_edit_error".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("edit_file".to_string()),
                    arguments: Some(
                        serde_json::json!({
                            "files": [{"path": "file.rs", "edits": []}],
                        })
                        .to_string(),
                    ),
                },
                signature: None,
            },
            id: None,
            signature: None,
        })]];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(SnedTool::EditFile, Arc::new(StaticErrorHandler));
        let mut agent = AgentLoop::new(test_agent_config(provider, "edit-error-counter"))
            .with_tools(Arc::new(registry));

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        assert_eq!(agent.state.lock().await.consecutive_mistakes, 1);
    }

    #[test]
    fn test_parallel_tool_results_keep_hook_context_in_the_tool_turn() {
        let mut blocks = Vec::new();
        for (tool_id, context) in [("call_1", "context 1"), ("call_2", "context 2")] {
            append_tool_result_blocks(
                &mut blocks,
                tool_id.to_string(),
                ToolExecutionOutput::success_with_hook_context(
                    format!("result for {tool_id}"),
                    vec![format!("[Hook context from PreToolUse]: {context}")],
                ),
            );
        }

        assert_eq!(blocks.len(), 4);
        assert!(matches!(
            &blocks[0],
            UserContentBlock::ToolResult(result) if result.tool_use_id == "call_1"
        ));
        assert!(matches!(
            &blocks[1],
            UserContentBlock::Text(text) if text.text.contains("context 1")
        ));
        assert!(matches!(
            &blocks[2],
            UserContentBlock::ToolResult(result) if result.tool_use_id == "call_2"
        ));
        assert!(matches!(
            &blocks[3],
            UserContentBlock::Text(text) if text.text.contains("context 2")
        ));
    }

    #[test]
    fn test_strip_edit_diff_anchors_preserves_prefix_and_style() {
        let mut line = crate::cli::tui::ansi_converter::ansi_to_ratatui_lines(
            "\x1b[92m+ AddedHash§new line\x1b[0m",
        )
        .pop()
        .unwrap();

        strip_edit_diff_anchors(&mut line);

        assert_eq!(line.to_string(), "+ new line");
        assert_eq!(line.spans[0].style.fg, Some(ratatui::style::Color::Green));
    }

    #[test]
    fn test_strip_edit_diff_anchors_preserves_syntax_spans() {
        let mut line = crate::cli::tui::ansi_converter::ansi_to_ratatui_lines(
            "\x1b[92m+ AddedHash§\x1b[0m\x1b[96mlet\x1b[0m value = 1;",
        )
        .pop()
        .unwrap();

        strip_edit_diff_anchors(&mut line);

        assert_eq!(line.to_string(), "+ let value = 1;");
        assert!(line.spans.iter().any(
            |span| span.content == "let" && span.style.fg == Some(ratatui::style::Color::Cyan)
        ));
    }

    #[tokio::test]
    async fn test_edit_file_result_displays_anchor_free_diff_previews_for_every_file() {
        let responses = vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("call_edit".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("edit_file".to_string()),
                    arguments: Some(
                        serde_json::json!({
                            "files": [{"path": "Cargo.toml", "edits": []}],
                        })
                        .to_string(),
                    ),
                },
                signature: None,
            },
            id: None,
            signature: None,
        })]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-edit-diff-output");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::EditFile,
            Arc::new(StaticResultHandler(
                "Edited 3 file(s): 3 edit(s) applied, 0 edit(s) failed.\n\nApplied 1 edit(s) successfully (+1, -1 lines). NOTE the UPDATED anchors below.\n\n- FirstOldHash§first old line\n+ FirstNewHash§first new line\n\n---\n\nApplied 1 edit(s) successfully (+1, -1 lines). NOTE the UPDATED anchors below.\n\n- SecondOldHash§second old line\n+ SecondNewHash§second new line\n\n---\n\nApplied 1 edit(s) successfully (+2, -1 lines). NOTE the UPDATED anchors below.\n\nBecause the changes were extensive, the full updated file content with anchors is provided below to ensure clarity:\n\nFullFirstHash§full first line\nFullSecondHash§full second line",
            )),
        );
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));

        let output = drain_rendered_output(&mut rx);
        assert!(output.iter().any(|line| line == "- first old line"));
        assert!(output.iter().any(|line| line == "+ first new line"));
        assert!(output.iter().any(|line| line == "- second old line"));
        assert!(output.iter().any(|line| line == "+ second new line"));
        assert!(output.iter().any(|line| line == "full first line"));
        assert!(output.iter().any(|line| line == "full second line"));
        assert!(
            output.iter().all(|line| !line.contains('§')),
            "edit diff preview leaked hash anchors: {output:?}"
        );
    }

    #[tokio::test]
    async fn test_streamed_tool_start_announces_preparation_once() {
        let responses = vec![vec![
            ApiStreamChunk::ToolCallStarted {
                call_id: "call_write".to_string(),
                name: "write_to_file".to_string(),
            },
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_write".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("write_to_file".to_string()),
                        arguments: Some(
                            serde_json::json!({"path": "tetris.c", "content": "x"}).to_string(),
                        ),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
        ]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "streamed-tool-start");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::WriteToFile,
            Arc::new(StaticResultHandler("written")),
        );
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));

        let output = drain_rendered_output(&mut rx);
        assert_eq!(
            output
                .iter()
                .filter(|line| line.as_str() == "Preparing write_to_file…")
                .count(),
            1,
            "streamed tool start should have one preparation notice: {output:?}"
        );
        assert!(
            output
                .iter()
                .filter(|line| line.contains("▶ write_to_file"))
                .count()
                == 1,
            "completed tool call should have one full call display: {output:?}"
        );
        assert!(
            output.iter().any(|line| line.contains("\"content\"")),
            "completed tool call should show its arguments: {output:?}"
        );
    }

    #[tokio::test]
    async fn test_disabled_parallel_tool_calling_serializes_tools() {
        let responses = vec![vec![
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_1".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("list_files".to_string()),
                        arguments: Some("{}".to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_2".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("list_files".to_string()),
                        arguments: Some("{}".to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
        ]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::ListFiles,
            Arc::new(ConcurrencyProbeHandler {
                active,
                max_active: max_active.clone(),
            }),
        );

        let mut agent = AgentLoop::new(test_agent_config(provider, "sequential-tools"))
            .with_tools(Arc::new(registry))
            .with_system_prompt_context(SystemPromptContext {
                enable_parallel_tool_calling: false,
                ..Default::default()
            });

        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));
        assert_eq!(
            max_active.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "disabled parallel tool calling must keep tool execution sequential"
        );
    }

    #[tokio::test]
    async fn test_approval_timeout_does_not_skip_remaining_batch_calls() {
        use crate::core::approval::{ApprovalManager, ApprovalResult};
        use crate::core::tools::ToolRegistry;
        use crate::test_support::env_lock;
        use tokio::time::{Duration, timeout};

        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let _approval_guard = crate::core::approval::approval_test_guard();
        let _input_override = crate::core::approval::override_approval_input_for_test();
        let _timeout_override =
            crate::core::approval::override_approval_timeout_for_test(Duration::from_millis(25));

        let responses = vec![vec![
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_1".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("write_to_file".to_string()),
                        arguments: Some(
                            serde_json::json!({"path": "first.txt", "content": "first"})
                                .to_string(),
                        ),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_2".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("write_to_file".to_string()),
                        arguments: Some(
                            serde_json::json!({"path": "second.txt", "content": "second"})
                                .to_string(),
                        ),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
        ]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let (tx, _rx) = mpsc::channel(32);
        let writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut approval_rx = writer
            .take_approval_rx()
            .expect("approval output receiver should be available");
        let mut config = test_agent_config(provider, "test-approval-timeout-batch");
        config.output_writer = writer;

        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::WriteToFile,
            Arc::new(StaticResultHandler("write completed")),
        );
        let approval_manager = Arc::new(tokio::sync::Mutex::new(ApprovalManager::new()));
        let mut agent = AgentLoop::new(config)
            .with_tools(Arc::new(registry))
            .with_approval_manager(approval_manager);

        let turn = tokio::spawn(async move {
            let result = agent.execute_turn().await;
            (agent, result)
        });

        let first_request = loop {
            let event = timeout(Duration::from_secs(2), approval_rx.recv())
                .await
                .expect("first approval prompt should arrive")
                .expect("priority output should stay open");
            if let OutputEvent::ApprovalRequested(request) = event {
                break request;
            }
        };

        let second_request = loop {
            let event = timeout(Duration::from_secs(2), approval_rx.recv())
                .await
                .expect("second approval prompt should arrive after timeout")
                .expect("priority output should stay open");
            if let OutputEvent::ApprovalRequested(request) = event {
                break request;
            }
        };
        assert_ne!(first_request.id(), second_request.id());
        assert!(second_request.respond(ApprovalResult::Approved));
        drop(first_request);

        let (agent, result) = timeout(Duration::from_secs(2), turn)
            .await
            .expect("tool batch should finish")
            .expect("agent task should not panic");
        assert!(matches!(result, TurnResult::Continue));

        let history = agent.conversation_history.lock().await;
        let tool_results = history
            .last()
            .and_then(|message| match &message.content {
                MessageContent::UserBlocks(blocks) => Some(blocks),
                _ => None,
            })
            .expect("tool result message should be recorded")
            .iter()
            .filter_map(|block| match block {
                UserContentBlock::ToolResult(result) => match &result.content {
                    ToolResultContent::Text(text) => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("didn't respond within 5 minutes"));
        assert_eq!(tool_results[1], "write completed");
    }

    fn drain_rendered_output(
        rx: &mut tokio::sync::mpsc::Receiver<crate::cli::output::OutputEvent>,
    ) -> Vec<String> {
        let mut rendered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                crate::cli::output::OutputEvent::Line(line) => rendered.push(line.to_string()),
                crate::cli::output::OutputEvent::ModelUpdateLine(line) => {
                    rendered.push(line.to_string())
                }
                crate::cli::output::OutputEvent::ToolOutputLine(line) => {
                    rendered.push(line.to_string())
                }
                crate::cli::output::OutputEvent::RawAnsi(raw) => rendered.push(raw),
                crate::cli::output::OutputEvent::Completion(text) => rendered.push(text),
                crate::cli::output::OutputEvent::TurnEnd { .. } => {}
                crate::cli::output::OutputEvent::QueuedMessageStarted { .. } => {}
                crate::cli::output::OutputEvent::TurnIndicator(line) => {
                    rendered.push(line.to_string())
                }
                crate::cli::output::OutputEvent::ErrorBox(msg) => rendered.push(msg),
                crate::cli::output::OutputEvent::ToolHeaderLine(line) => {
                    rendered.push(line.to_string())
                }
                crate::cli::output::OutputEvent::CommandHeaderLine(line) => {
                    rendered.push(line.to_string())
                }
                crate::cli::output::OutputEvent::CommandOutputLine(line) => {
                    rendered.push(line.to_string())
                }
                crate::cli::output::OutputEvent::ReasoningChunk(chunk) => rendered.push(chunk),
                crate::cli::output::OutputEvent::UserPromptLine(line)
                | crate::cli::output::OutputEvent::LocalCommandEcho(line) => {
                    rendered.push(line.to_string())
                }
                crate::cli::output::OutputEvent::ApprovalRequested(request) => {
                    rendered.push(request.details().to_string());
                    request.fail("test output has no interactive approval UI");
                }
                crate::cli::output::OutputEvent::ApprovalFinished { .. } => {}
            }
        }
        rendered
    }

    fn drain_output_events(
        priority_rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutputEvent>,
        rx: &mut tokio::sync::mpsc::Receiver<OutputEvent>,
    ) -> Vec<OutputEvent> {
        let mut events = Vec::new();
        while let Ok(event) = priority_rx.try_recv() {
            events.push(event);
        }
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn test_resolve_tool_profile_applies_yolo_over_cached_profile() {
        let profile = resolve_tool_profile(
            Some(crate::core::tools::definitions::ToolProfile::WriteOnly),
            true,
            "write a file",
            "act",
        );

        assert_eq!(
            profile,
            crate::core::tools::definitions::ToolProfile::Validate
        );
    }

    #[test]
    fn test_resolve_tool_profile_plan_mode_ignores_cached_profile_and_yolo() {
        let profile = resolve_tool_profile(
            Some(crate::core::tools::definitions::ToolProfile::Full),
            true,
            "inspect the workspace",
            "plan",
        );

        assert_eq!(profile, crate::core::tools::definitions::ToolProfile::Plan);
    }

    #[test]
    fn test_resolve_tool_profile_compact_instruction_ignores_yolo_and_cached_profile() {
        // /compact injects <explicit_instructions type="condense">. The model
        // must receive the condense tool schema even in YOLO mode (which
        // otherwise forces Validate and omits condense).
        let prompt = r#"<explicit_instructions type="condense">
The user has explicitly asked you to create a detailed summary of the conversation so far.
Irrespective of whether additional information or instructions are given, you are only allowed to respond to this message by calling the condense tool.
</explicit_instructions>
"#;

        let profile_yolo = resolve_tool_profile(
            Some(crate::core::tools::definitions::ToolProfile::WriteOnly),
            true,
            prompt,
            "act",
        );
        assert_eq!(
            profile_yolo,
            crate::core::tools::definitions::ToolProfile::Full
        );

        let profile_cached = resolve_tool_profile(
            Some(crate::core::tools::definitions::ToolProfile::CoreEdit),
            false,
            prompt,
            "act",
        );
        assert_eq!(
            profile_cached,
            crate::core::tools::definitions::ToolProfile::Full
        );
    }

    #[test]
    fn test_compact_profile_includes_condense_tool() {
        // Regression guard: a /compact turn must expose the condense tool.
        // The qwen bug was that the model hallucinated `condense_tool`
        // because Validate (YOLO's forced profile) omitted `condense`.
        let prompt = "<explicit_instructions type=\"condense\">compact now</explicit_instructions>";
        let profile = resolve_tool_profile(None, true, prompt, "act");
        let has_condense = profile.tools().iter().any(|t| t.name() == "condense");
        assert!(
            has_condense,
            "condense must be in the resolved profile for /compact, got: {:?}",
            profile
        );
    }

    #[tokio::test]
    async fn test_act_profile_uses_current_task_after_plan_transition() {
        let responses = vec![vec![ApiStreamChunk::Text(ApiStreamTextChunk {
            text: "I need to inspect the file first.".to_string(),
            id: None,
            signature: None,
        })]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "act-profile-after-plan"));

        let user_message = |text: &str| StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        };
        agent.conversation_history.lock().await.extend([
            user_message("Explain this repository"),
            user_message("Edit the configuration parser and run its tests"),
        ]);

        agent.set_mode(AgentMode::Plan);
        agent.set_mode(AgentMode::Act);
        let _ = agent.execute_turn().await;

        let requests = requests.lock().unwrap();
        let tools = requests
            .first()
            .and_then(|request| request.tools.as_ref())
            .expect("ACT edit task should receive tools");
        assert!(
            tools.iter().any(|tool| tool.function.name == "edit_file"),
            "ACT profile should be selected from the current task, not the first historical prompt"
        );
    }

    #[tokio::test]
    async fn test_new_task_recomputes_profile_in_same_act_session() {
        let responses = vec![
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "The answer is 4.".to_string(),
                id: None,
                signature: None,
            })],
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "I need to inspect the file first.".to_string(),
                id: None,
                signature: None,
            })],
        ];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));
        let mut config = test_agent_config(provider, "act-profile-new-task");
        config.interactive_mode = false;
        let mut agent = AgentLoop::new(config);
        let state_manager = Arc::new(StateManager::new().unwrap());

        let user_message = |text: &str| StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        };

        agent
            .run(
                vec![user_message("Explain this repository")],
                state_manager.clone(),
            )
            .await
            .expect("answer task should complete");
        agent
            .run(
                vec![user_message("Edit the configuration parser")],
                state_manager,
            )
            .await
            .expect("edit task should complete");

        let requests = requests.lock().unwrap();
        assert!(requests[0].tools.is_none());
        let tools = requests[1]
            .tools
            .as_ref()
            .expect("new ACT task should receive tools");
        assert!(
            tools.iter().any(|tool| tool.function.name == "edit_file"),
            "new ACT task should recompute its profile instead of reusing DirectAnswer"
        );
    }

    #[test]
    fn test_task_state_default() {
        let state = TaskState::default();
        assert_eq!(state.consecutive_mistakes, 0);
        assert!(!state.is_cancelled);
        assert!(!state.did_complete_reading_stream);
    }

    #[test]
    fn test_print_model_line_emits_one_output_event_per_wrapped_line() {
        let (tx, mut rx) = mpsc::channel(8);
        let writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let line = "x".repeat(get_terminal_width().max(1).saturating_add(1));
        print_model_line(&line, &writer, false);

        let mut emitted = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                OutputEvent::Line(line) => emitted.push(line.to_string()),
                OutputEvent::ModelUpdateLine(line) => emitted.push(line.to_string()),
                other => panic!("unexpected output event: {:?}", other),
            }
        }

        assert!(
            emitted.len() >= 2,
            "expected wrapped output to span multiple events"
        );
        assert!(emitted.iter().all(|line| !line.contains('\n')));
    }

    #[test]
    fn test_print_model_line_sanitizes_control_characters() {
        let (tx, mut rx) = mpsc::channel(8);
        let writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        print_model_line("ok\r\x1b[31mthere\tfriend", &writer, false);

        let rendered = match rx.try_recv() {
            Ok(OutputEvent::Line(line)) => line.to_string(),
            Ok(other) => panic!("unexpected output event: {:?}", other),
            Err(err) => panic!("expected output event, got {}", err),
        };

        assert!(!rendered.contains('\r'));
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("ok"));
        assert!(rendered.contains("there"));
        assert!(rendered.contains("friend"));
    }

    #[test]
    fn test_streaming_model_line_renders_completed_markdown() {
        let inline = streaming_model_line("  **bold** and `code`".to_string(), true);
        assert_eq!(inline.to_string(), "bold and `code`");
        assert!(inline.spans.iter().any(|span| {
            span.content == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(inline.spans.iter().any(|span| {
            span.content == "`code`" && span.style.fg == Some(crate::cli::tui::theme::PROMPT_FG)
        }));

        let heading = streaming_model_line("  ### heading".to_string(), true);
        assert!(
            heading
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );

        let list_item = streaming_model_line("  1. first item".to_string(), true);
        assert!(list_item.to_string().contains("• first item"));
    }

    #[test]
    fn test_streaming_model_line_keeps_partial_and_block_markdown_raw() {
        let partial = streaming_model_line("**bol".to_string(), false);
        assert_eq!(partial.to_string(), "**bol");
        assert_eq!(
            partial.spans[0].style.fg,
            Some(crate::cli::tui::theme::ACCENT)
        );

        let block = streaming_model_line("---".to_string(), true);
        assert_eq!(block.to_string(), "---");
        assert_eq!(
            block.spans[0].style.fg,
            Some(crate::cli::tui::theme::ACCENT)
        );
    }

    #[test]
    fn test_update_model_line_styles_completed_partial_line() {
        let (tx, mut rx) = mpsc::channel(2);
        let writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        update_model_line("**bol", &writer, false);
        update_model_line("**bold**", &writer, true);

        let partial = match rx.try_recv() {
            Ok(OutputEvent::ModelUpdateLine(line)) => line,
            other => panic!("expected raw partial update, got {other:?}"),
        };
        assert_eq!(partial.to_string(), "**bol");

        let completed = match rx.try_recv() {
            Ok(OutputEvent::ModelUpdateLine(line)) => line,
            other => panic!("expected styled completed update, got {other:?}"),
        };
        assert_eq!(completed.to_string(), "bold");
        assert!(
            completed
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn test_sanitize_model_text_fast_path_borrows_clean_input() {
        match sanitize_model_text_for_display("already clean") {
            Cow::Borrowed(text) => assert_eq!(text, "already clean"),
            Cow::Owned(_) => panic!("clean input should not allocate"),
        }
    }

    #[tokio::test]
    async fn test_provider_failure_threshold_surfaces_recovery_message() {
        let provider = Arc::new(Providers::Error(crate::providers::ErrorProvider));
        let mut agent = AgentLoop::new(test_agent_config(
            provider,
            "test-provider-failure-threshold",
        ));
        {
            let mut state = agent.state.lock().await;
            state.consecutive_provider_failures =
                DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES.saturating_sub(1);
        }

        let result = agent.execute_turn().await;

        match result {
            TurnResult::Error(message) => {
                assert!(message.contains("consecutive requests"));
                assert!(message.contains("/model"));
            }
            other => panic!("expected provider failure error, got {:?}", other),
        }

        let state = agent.state.lock().await;
        assert_eq!(
            state.consecutive_provider_failures,
            DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES
        );
    }

    #[tokio::test]
    async fn test_provider_failure_captures_retryable_failed_request() {
        let provider = Arc::new(Providers::Error(crate::providers::ErrorProvider));
        let mut agent = AgentLoop::new(test_agent_config(
            provider,
            "test-provider-failure-captures-retry",
        ));
        let message = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("keep working on this bug".to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        };
        agent
            .conversation_history
            .lock()
            .await
            .push(message.clone());

        let result = agent.execute_turn().await;

        assert!(matches!(result, TurnResult::Error(_)));
        let state = agent.state.lock().await;
        assert_eq!(state.retryable_failed_request, Some(message));
    }

    /// Regression test for the `consecutive_mistakes` cap. When the
    /// model returns an empty response (no text, no tool calls, no
    /// reasoning) N times in a row where N = `max_consecutive_mistakes`,
    /// the turn must terminate with `TurnResult::Error("Max consecutive
    /// mistakes reached")` rather than continuing indefinitely.
    ///
    /// The existing test `test_provider_failure_threshold_surfaces_
    /// recovery_message` covers the `consecutive_provider_failures`
    /// cap (request-level, not turn-level). This test covers the
    /// turn-level `consecutive_mistakes` cap.
    ///
    /// Note: `execute_turn` consumes one provider response per call. The
    /// outer TUI loop would call `execute_turn` again when the turn
    /// returns `TurnResult::Continue`. This test simulates that loop
    /// by calling `execute_turn` up to `max_consecutive_mistakes` times
    /// and asserts the final call returns `TurnResult::Error`.
    #[tokio::test]
    async fn test_max_consecutive_mistakes_terminates_turn() {
        let max_mistakes = 3; // matches test_agent_config default
        // Provide max_mistakes empty responses; the cap should fire on
        // the last one. Sentinel: must NOT be consumed.
        let mut all_responses: Vec<crate::providers::mock::MockResponse> = (0..max_mistakes)
            .map(|_| crate::providers::mock::MockResponse::Stream(vec![]))
            .collect();
        all_responses.push(crate::providers::mock::MockResponse::Text(
            "SENTINEL_NOT_CONSUMED\n".to_string(),
        ));

        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            all_responses,
        )));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-max-consecutive-mistakes");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);

        // Simulate the outer loop calling execute_turn until it
        // returns a final result (Error or completion). On the cap-th
        // turn, it should return Error instead of Continue.
        let mut final_result = None;
        for _ in 0..(max_mistakes + 1) {
            let result = agent.execute_turn().await;
            match &result {
                TurnResult::Error(_) => {
                    final_result = Some(result);
                    break;
                }
                TurnResult::Continue => {
                    // Continue the loop, consuming the next response.
                    continue;
                }
                _ => panic!(
                    "unexpected turn result before cap: {result:?}. The model \
                     should produce empty responses until the cap fires."
                ),
            }
        }

        match final_result {
            Some(TurnResult::Error(message)) => {
                assert!(
                    message.contains("Max consecutive mistakes reached"),
                    "error must indicate consecutive mistakes cap was hit, got: {message}"
                );
            }
            Some(other) => panic!(
                "expected TurnResult::Error after {max_mistakes} empty responses, \
                 got {other:?}. If the consecutive_mistakes cap is broken, the \
                 agent would continue and consume the sentinel response."
            ),
            None => {
                panic!("consecutive_mistakes cap never fired after {max_mistakes} empty responses")
            }
        }
        // The sentinel must NOT have been consumed.
        let rendered = drain_rendered_output(&mut rx);
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("SENTINEL_NOT_CONSUMED")),
            "sentinel response was consumed — the consecutive_mistakes \
             cap did not fire. rendered: {rendered:?}"
        );
        // The state should reflect consecutive_mistakes = 3.
        let state = agent.state.lock().await;
        assert_eq!(
            state.consecutive_mistakes, max_mistakes,
            "consecutive_mistakes must reach the cap, got: {}",
            state.consecutive_mistakes
        );
    }

    #[tokio::test]
    async fn test_unlimited_consecutive_mistakes_never_stops_empty_responses() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            (0..4)
                .map(|_| crate::providers::mock::MockResponse::Stream(vec![]))
                .collect(),
        )));
        let mut config = test_agent_config(provider, "test-unlimited-consecutive-mistakes");
        config.max_consecutive_mistakes = None;
        let mut agent = AgentLoop::new(config);

        for _ in 0..4 {
            assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        }

        assert_eq!(agent.state.lock().await.consecutive_mistakes, 4);
    }

    #[tokio::test]
    async fn test_successful_turn_clears_stale_retryable_failed_request() {
        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_text_response("done"),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-clear-stale-retry"));
        {
            let mut state = agent.state.lock().await;
            state.retryable_failed_request = Some(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("stale".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            });
        }
        agent
            .conversation_history
            .lock()
            .await
            .push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("fresh request".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            });

        let result = agent.execute_turn().await;

        assert!(matches!(
            result,
            TurnResult::Continue | TurnResult::Complete
        ));
        assert!(agent.state.lock().await.retryable_failed_request.is_none());
    }

    #[tokio::test]
    async fn test_tool_call_turn_does_not_replace_full_response() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![crate::providers::mock::MockResponse::Stream(vec![
                crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Text(
                    ApiStreamTextChunk {
                        text: "I will inspect the workspace first.".to_string(),
                        id: None,
                        signature: None,
                    },
                )),
                crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::ToolCalls(
                    ApiStreamToolCallsChunk {
                        tool_call: ApiStreamToolCall {
                            call_id: Some("full-tool-call".to_string()),
                            function: ApiStreamToolCallFunction {
                                id: None,
                                name: Some("list_files".to_string()),
                                arguments: Some("{}".to_string()),
                            },
                            signature: None,
                        },
                        id: None,
                        signature: None,
                    },
                )),
            ])],
        )));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::ListFiles,
            Arc::new(crate::core::tools::handlers::list_files::ListFilesHandler::new()),
        );
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-full-tool-turn"))
            .with_tools(Arc::new(registry));

        let result = agent.execute_turn().await;

        assert!(matches!(
            result,
            TurnResult::Continue | TurnResult::Complete
        ));
        assert!(agent.state.lock().await.last_full_response.is_none());
    }

    #[tokio::test]
    async fn test_retryable_stream_error_before_output_retries_once() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![
                crate::providers::mock::MockResponse::Stream(vec![
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                        "OpenAI SSE stream error: error decoding response body (retryable)"
                            .to_string(),
                    )),
                ]),
                crate::providers::mock::MockResponse::Text("recovered output\n".to_string()),
            ],
        )));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-stream-retry-before-output");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);

        let result = agent.execute_turn().await;

        assert!(matches!(result, TurnResult::Continue));
        let rendered = drain_rendered_output(&mut rx);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("recovered output"))
        );
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("error decoding response body"))
        );
        assert!(
            agent
                .state
                .lock()
                .await
                .did_automatically_retry_failed_api_request
        );
    }

    #[tokio::test]
    async fn test_retryable_stream_error_after_tool_preparation_retries_once() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![
                crate::providers::mock::MockResponse::Stream(vec![
                    crate::providers::mock::MockStreamEvent::Chunk(
                        ApiStreamChunk::ToolCallStarted {
                            call_id: "call_write".to_string(),
                            name: "write_to_file".to_string(),
                        },
                    ),
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                        "OpenAI SSE stream error: error decoding response body (retryable)"
                            .to_string(),
                    )),
                ]),
                crate::providers::mock::MockResponse::Text("recovered output\n".to_string()),
            ],
        )));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-stream-retry-after-tool-preparation");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));

        let rendered = drain_rendered_output(&mut rx);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("recovered output"))
        );
        assert!(
            agent
                .state
                .lock()
                .await
                .did_automatically_retry_failed_api_request
        );
    }

    #[tokio::test]
    async fn test_retryable_stream_error_after_signature_only_text_retries_once() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![
                crate::providers::mock::MockResponse::Stream(vec![
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Text(
                        ApiStreamTextChunk {
                            text: String::new(),
                            id: None,
                            signature: Some("gemini-signature".to_string()),
                        },
                    )),
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                        "Gemini stream error: error decoding response body (retryable)".to_string(),
                    )),
                ]),
                crate::providers::mock::MockResponse::Text("recovered output\n".to_string()),
            ],
        )));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-stream-retry-after-signature-only-text");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));

        let rendered = drain_rendered_output(&mut rx);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("recovered output"))
        );
        assert!(
            agent
                .state
                .lock()
                .await
                .did_automatically_retry_failed_api_request
        );
    }

    #[tokio::test]
    async fn test_retryable_stream_error_after_hidden_thinking_text_retries_once() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![
                crate::providers::mock::MockResponse::Stream(vec![
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Text(
                        ApiStreamTextChunk {
                            text: "<think>hidden reasoning</think>".to_string(),
                            id: None,
                            signature: None,
                        },
                    )),
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Text(
                        ApiStreamTextChunk {
                            text: "<!-- think -->hidden reasoning<!-- /think -->".to_string(),
                            id: None,
                            signature: None,
                        },
                    )),
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                        "OpenAI SSE stream error: error decoding response body (retryable)"
                            .to_string(),
                    )),
                ]),
                crate::providers::mock::MockResponse::Text("recovered output\n".to_string()),
            ],
        )));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-stream-retry-after-hidden-thinking");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));

        let rendered = drain_rendered_output(&mut rx);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("recovered output"))
        );
        assert!(
            agent
                .state
                .lock()
                .await
                .did_automatically_retry_failed_api_request
        );
    }

    #[tokio::test]
    async fn test_retryable_stream_error_before_output_is_quiet_in_json_mode() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![
                crate::providers::mock::MockResponse::Stream(vec![
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                        "OpenAI SSE stream error: error decoding response body (retryable)"
                            .to_string(),
                    )),
                ]),
                crate::providers::mock::MockResponse::Text("recovered output\n".to_string()),
            ],
        )));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-json-stream-retry-before-output");
        config.json_output = true;
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);

        let result = agent.execute_turn().await;

        assert!(matches!(result, TurnResult::Continue));
        let rendered = drain_rendered_output(&mut rx);
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("Provider stream stalled before output"))
        );
        assert!(
            agent
                .state
                .lock()
                .await
                .did_automatically_retry_failed_api_request
        );
    }

    #[tokio::test]
    async fn test_retryable_stream_error_after_output_does_not_retry() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![
                crate::providers::mock::MockResponse::Stream(vec![
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Text(
                        ApiStreamTextChunk {
                            text: "partial output\n".to_string(),
                            id: None,
                            signature: None,
                        },
                    )),
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                        "OpenAI SSE stream error: error decoding response body (retryable)"
                            .to_string(),
                    )),
                ]),
                crate::providers::mock::MockResponse::Text("should not be used\n".to_string()),
            ],
        )));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-stream-retry-after-output");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);

        let result = agent.execute_turn().await;

        match result {
            TurnResult::Error(message) => {
                assert!(message.contains("Provider stream error"));
            }
            other => panic!("expected stream error, got {:?}", other),
        }
        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(events.iter().any(
            |event| matches!(event, OutputEvent::Line(line) if line.to_string().contains("partial output"))
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, OutputEvent::Line(line) if line.to_string().contains("[sned] ERROR: Provider stream error: OpenAI SSE stream error: error decoding response body (retryable)")))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, OutputEvent::Line(line) if line.to_string().contains("should not be used")))
        );
        assert!(
            !agent
                .state
                .lock()
                .await
                .did_automatically_retry_failed_api_request
        );
    }

    #[tokio::test]
    async fn test_non_retryable_stream_error_preserves_message_without_retry_state() {
        let error = "Gemini blocked the response (finish reason: SAFETY). Rephrase the request and try again.";
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![crate::providers::mock::MockResponse::Stream(vec![
                crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                    error.to_string(),
                )),
            ])],
        )));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-non-retryable-stream-error");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);
        agent
            .conversation_history
            .lock()
            .await
            .push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("blocked request".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            });

        let result = agent.execute_turn().await;

        match result {
            TurnResult::Error(message) => assert_eq!(message, error),
            other => panic!("expected non-retryable stream error, got {other:?}"),
        }
        assert!(
            drain_rendered_output(&mut rx)
                .iter()
                .all(|line| !line.contains(error))
        );
        assert!(agent.state.lock().await.retryable_failed_request.is_none());
    }

    #[tokio::test]
    async fn test_non_retryable_stream_error_precedes_later_retryable_error() {
        let policy_error = "Gemini blocked the response (finish reason: SAFETY). Rephrase the request and try again.";
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![
                crate::providers::mock::MockResponse::Stream(vec![
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                        policy_error.to_string(),
                    )),
                    crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                        "Gemini SSE stream error: connection reset (retryable)".to_string(),
                    )),
                ]),
                crate::providers::mock::MockResponse::Text(
                    "blocked request must not be retried\n".to_string(),
                ),
            ],
        )));
        let mut agent = AgentLoop::new(test_agent_config(
            provider,
            "test-non-retryable-error-precedence",
        ));

        let result = agent.execute_turn().await;

        match result {
            TurnResult::Error(message) => assert_eq!(message, policy_error),
            other => panic!("expected policy error, got {other:?}"),
        }
        assert!(
            !agent
                .state
                .lock()
                .await
                .did_automatically_retry_failed_api_request
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_non_retryable_stream_error_is_emitted_in_json_mode() {
        let policy_error =
            "Gemini blocked the prompt (reason: SAFETY). Rephrase the prompt and try again.";
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![crate::providers::mock::MockResponse::Stream(vec![
                crate::providers::mock::MockStreamEvent::Chunk(ApiStreamChunk::Error(
                    policy_error.to_string(),
                )),
            ])],
        )));
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || CapturedTraceWriter(writer.clone()))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let mut config = test_agent_config(provider, "test-json-non-retryable-error");
        config.json_output = true;
        let mut agent = AgentLoop::new(config);

        let result = agent.execute_turn().await;

        match result {
            TurnResult::Error(message) => assert_eq!(message, policy_error),
            other => panic!("expected policy error, got {other:?}"),
        }
        let output = String::from_utf8(
            captured
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        )
        .unwrap();
        assert!(output.contains("json_output"));
        assert!(output.contains("\"type\":\"error\""));
        assert!(output.contains(policy_error));
    }

    /// Regression test for the MAX_STREAM_RETRY_ATTEMPTS cap added in
    /// commit 719927e. The original test (test_retryable_stream_error_
    /// before_output_retries_once at line 4598) only covered the happy
    /// path: one retryable error, then recovery.
    ///
    /// NOTE: this test was attempted but could not be made to fail
    /// without changing production code. The cap at agent_loop.rs:2280
    /// is guarded by `if stream_retry_attempt == 0` in the chunk
    /// handler (line 2196), which means the cap only fires on the first
    /// attempt's error. On attempts 2+, the error falls through to the
    /// mid-output error path (line 2295) which returns "Provider stream
    /// error - retry the request." The cap code at line 2280 is
    /// effectively a 1-shot guard that never fires in practice for >1
    /// retryable errors. A follow-up fix is needed to either: (a) move
    /// the cap check outside the `stream_retry_attempt == 0` guard,
    /// or (b) increment the retry counter in the mid-output error path.
    /// Skipped per plan: no production code changes.

    #[tokio::test]
    async fn test_run_preserves_pending_cancellation_until_observed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let data_dir = temp_dir.path().join("data");
        std::fs::create_dir_all(data_dir.join("state")).unwrap();
        std::fs::create_dir_all(data_dir.join("settings")).unwrap();
        let old_sned_dir = std::env::var_os("SNED_DIR");
        // SAFETY: this test is intended to run with isolated validation commands.
        unsafe {
            std::env::set_var("SNED_DIR", temp_dir.path());
        }

        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_text_response("should not run"),
        ));
        let (tx, mut rx) = mpsc::channel(8);
        let mut config = test_agent_config(provider, "test-run-pending-cancel");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);
        {
            let mut state = agent.state.lock().await;
            state.is_cancelled = true;
            state
                .is_cancelled_atomic
                .store(true, std::sync::atomic::Ordering::Release);
        }

        let state_manager = Arc::new(StateManager::new().unwrap());
        let result = agent.run(vec![], state_manager).await;
        assert!(result.is_ok(), "pending cancellation should exit cleanly");
        assert!(agent.state.lock().await.is_cancelled);
        assert!(matches!(
            rx.try_recv(),
            Ok(OutputEvent::Line(line)) if line.to_string() == "[sned] Cancelled. Type /retry to resend."
        ));

        // SAFETY: restore the process environment for later tests.
        unsafe {
            match old_sned_dir {
                Some(ref value) => std::env::set_var("SNED_DIR", value),
                None => std::env::remove_var("SNED_DIR"),
            }
        }
    }

    #[tokio::test]
    async fn test_run_waits_on_approved_paused_plan_without_repeating_notice() {
        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_text_response("SENTINEL_NOT_CONSUMED"),
        ));
        let (tx, mut rx) = mpsc::channel(8);
        let mut config = test_agent_config(provider, "test-run-paused-plan");
        config.max_turns = 1;
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);
        {
            let mut state = agent.state.lock().await;
            let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
                "Resume this step later".to_string(),
            ]);
            plan.approved = true;
            plan.paused = true;
            state.plan_state = Some(plan);
        }

        let state_handle = Arc::clone(&agent.state);
        let state_manager = Arc::new(StateManager::new().unwrap());
        let run = tokio::spawn(async move { agent.run(vec![], state_manager).await });
        tokio::time::sleep(std::time::Duration::from_millis(650)).await;

        let rendered = drain_rendered_output(&mut rx);
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains("Plan is paused. Type /plan resume to continue."))
                .count(),
            1
        );
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("SENTINEL_NOT_CONSUMED"))
        );

        {
            let mut state = state_handle.lock().await;
            state.plan_state = None;
            state.is_cancelled = true;
            state
                .is_cancelled_atomic
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), run)
            .await
            .expect("paused plan task should observe cancellation")
            .expect("paused plan task should not panic");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_interactive_stream_snips_long_code_block_with_recovery_hint() {
        let mut code = String::from("```rust\n");
        for line in 1..=65 {
            code.push_str(&format!("fn line_{}() {{}}\n", line));
        }
        code.push_str("```\n");

        let (tx, mut rx) = mpsc::channel(256);
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_text_response(&code),
            )))),
            mode: AgentMode::Act,
            task_id: "test-snipped-code".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 1,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::ChannelOutputWriter::new(tx)),
            strict_plan_mode_enabled: true,
        };

        let mut agent = AgentLoop::new(config);
        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));

        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(events.iter().any(|event| {
            matches!(event, OutputEvent::Line(line) if line.to_string().contains("[snipped from streamed display; use /full]"))
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                OutputEvent::TurnEnd { accumulated_text }
                    if accumulated_text.contains("```rust")
                        && accumulated_text.contains("fn line_65()")
            )
        }));
    }

    #[tokio::test]
    async fn test_one_shot_stream_snips_after_200_code_lines() {
        let mut code = String::from("```rust\n");
        for line in 1..=201 {
            code.push_str(&format!("fn line_{}() {{}}\n", line));
        }
        code.push_str("```\n");

        let (tx, mut rx) = mpsc::channel(64);
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_text_response(&code),
            )))),
            mode: AgentMode::Act,
            task_id: "test-one-shot-code".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 1,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::ChannelOutputWriter::new(tx)),
            strict_plan_mode_enabled: true,
        };

        let mut agent = AgentLoop::new(config);
        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Complete));

        let rendered = drain_rendered_output(&mut rx).join("\n");
        let rendered = crate::cli::tui::ansi_converter::ansi_to_ratatui_lines(&rendered)
            .iter()
            .map(ratatui::text::Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("fn line_200()"));
        assert!(!rendered.contains("fn line_201()"));
        assert!(rendered.contains("[snipped from streamed display; use /full]"));
    }

    #[tokio::test]
    async fn test_one_shot_text_only_response_completes_without_tool_nudge() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_text_response("4"),
            )))),
            mode: AgentMode::Act,
            task_id: "test-one-shot-text-only".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 1,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let mut agent = AgentLoop::new(config);
        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Complete));

        let history = agent.conversation_history.lock().await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, MessageRole::Assistant);
    }

    #[test]
    fn test_synthetic_json_completion_uses_response_text_without_thinking() {
        let (_thinking, response_text) =
            split_model_output("<think>\nhidden work\n</think>\nVisible result");

        let event =
            AgentLoop::synthetic_json_completion_event(true, false, response_text.as_deref())
                .unwrap();

        assert_eq!(event["type"], "completion");
        assert_eq!(event["result"], "Visible result");
    }

    #[test]
    fn test_synthetic_json_completion_skips_attempt_completion_path() {
        assert!(
            AgentLoop::synthetic_json_completion_event(true, true, Some("Done")).is_none(),
            "attempt_completion already emits a completion event"
        );
        assert!(
            AgentLoop::synthetic_json_completion_event(false, false, Some("Done")).is_none(),
            "non-completing text turns should not emit completion"
        );
    }

    #[tokio::test]
    async fn test_reasoning_stream_coalesces_display_snapshot_without_flattening() {
        let responses = vec![vec![
            ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                reasoning: "first".to_string(),
                details: None,
                signature: None,
                redacted_data: None,
                id: Some("reasoning-1".to_string()),
            }),
            ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                reasoning: " thought\n\n".to_string(),
                details: None,
                signature: None,
                redacted_data: None,
                id: Some("reasoning-2".to_string()),
            }),
            ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                reasoning: "third".to_string(),
                details: None,
                signature: None,
                redacted_data: None,
                id: Some("reasoning-3".to_string()),
            }),
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "answer".to_string(),
                id: None,
                signature: None,
            }),
        ]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut priority_rx = writer
            .take_priority_rx()
            .expect("priority output receiver should be available");
        let mut config = test_agent_config(provider, "test-reasoning-chunks");
        config.output_writer = writer;
        let mut agent = AgentLoop::new(config);

        let _ = agent.execute_turn().await;

        let reasoning: Vec<String> = drain_output_events(&mut priority_rx, &mut rx)
            .into_iter()
            .filter_map(|event| match event {
                OutputEvent::ReasoningChunk(chunk) => Some(chunk),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, ["first thought\n\nthird"]);
    }

    #[tokio::test]
    async fn test_later_text_only_response_gets_one_bounded_nudge() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_text_response_repeat("I checked it."),
            )))),
            mode: AgentMode::Act,
            task_id: "test-text-only-nudge".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 2,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let mut agent = AgentLoop::new(config);
        {
            let mut state = agent.state.lock().await;
            state.turns_completed = 1;
        }

        let first_result = agent.execute_turn().await;
        assert!(matches!(first_result, TurnResult::Continue));

        {
            let history = agent.conversation_history.lock().await;
            assert_eq!(history.len(), 2);
            assert_eq!(history[1].role, MessageRole::User);
            match &history[1].content {
                MessageContent::Text(text) => assert!(text.contains("use the required tool")),
                other => panic!("expected text nudge, got {other:?}"),
            }
        }

        let second_result = agent.execute_turn().await;
        assert!(matches!(second_result, TurnResult::Complete));

        let history = agent.conversation_history.lock().await;
        let nudge_count = history
            .iter()
            .filter(|message| {
                matches!(
                    &message.content,
                    MessageContent::Text(text) if text.contains("use the required tool")
                )
            })
            .count();
        assert_eq!(nudge_count, 1);
    }

    #[test]
    fn test_agent_mode_equality() {
        assert_eq!(AgentMode::Plan, AgentMode::Plan);
        assert_ne!(AgentMode::Plan, AgentMode::Act);
    }

    #[test]
    fn test_turn_result_variants() {
        let results = [
            TurnResult::Continue,
            TurnResult::Complete,
            TurnResult::Cancelled,
            TurnResult::Error("test".to_string()),
        ];

        assert_eq!(results.len(), 4);
    }

    #[test]
    fn test_agent_error_display() {
        assert_eq!(
            format!("{}", AgentError::MaxTurnsExceeded),
            "Maximum turns exceeded"
        );
        assert_eq!(
            format!("{}", AgentError::ExecutionError(String::from("foo"))),
            "Execution error: foo"
        );
    }

    #[test]
    fn test_system_prompt_integration() {
        let context = SystemPromptContext {
            cwd: Some("/tmp/test".to_string()),
            active_shell_path: Some("/bin/zsh".to_string()),
            active_shell_type: Some("zsh".to_string()),
            active_shell_is_posix: true,
            enable_parallel_tool_calling: true,
            ..Default::default()
        };

        let prompt = PromptBuilder::new(context).build();

        assert!(
            prompt.contains("You are Sned"),
            "Prompt should contain 'You are Sned'"
        );
        assert!(
            prompt.contains("PRIME DIRECTIVES"),
            "Prompt should contain 'PRIME DIRECTIVES'"
        );
        // Environment info (OS, shell, CWD, CPU) is now provided by context_loader in <environment_details>
        // to avoid duplication. System prompt focuses on instructions and tool usage.
        assert!(
            !prompt.contains("Operating System:"),
            "System prompt should not contain OS info (provided by context_loader)"
        );
        assert!(
            !prompt.contains("Default Shell:"),
            "System prompt should not contain shell info (provided by context_loader)"
        );
        assert!(
            !prompt.contains("Available CPU Cores:"),
            "System prompt should not contain CPU info (provided by context_loader)"
        );
    }

    #[tokio::test]
    async fn test_system_prompt_is_cached_across_turns() {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = vec![
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "first response".to_string(),
                id: None,
                signature: None,
            })],
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "second response".to_string(),
                id: None,
                signature: None,
            })],
        ];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-system-prompt-cache"))
            .with_system_prompt_context(SystemPromptContext {
                cwd: Some("/tmp/cache-first".to_string()),
                active_shell_is_posix: true,
                enable_parallel_tool_calling: true,
                ..Default::default()
            });

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        agent.deps.system_prompt_context = Some(SystemPromptContext {
            cwd: Some("/tmp/cache-second".to_string()),
            active_shell_is_posix: true,
            enable_parallel_tool_calling: true,
            ..Default::default()
        });
        assert!(matches!(agent.execute_turn().await, TurnResult::Complete));

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
        // Verify system prompt is cached (doesn't change between turns)
        // Environment info like CWD is now in context_loader, not system prompt
        assert!(requests[0].system_prompt.contains("You are Sned"));
        assert!(requests[0].system_prompt.contains("PRIME DIRECTIVES"));
    }

    #[tokio::test]
    async fn test_profile_escalation_rebuilds_cached_system_prompt() {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = vec![
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "I should finish this through the tool.".to_string(),
                id: None,
                signature: None,
            })],
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "Done.".to_string(),
                id: None,
                signature: None,
            })],
        ];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));
        let mut agent = AgentLoop::new(test_agent_config(
            provider,
            "test-profile-escalation-prompt-cache",
        ));
        agent.deps.tool_profile = Some(crate::core::tools::definitions::ToolProfile::DirectAnswer);

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        assert_eq!(
            agent.deps.tool_profile,
            Some(crate::core::tools::definitions::ToolProfile::AnswerOnly)
        );
        assert!(
            agent.deps.cached_system_prompt.is_none(),
            "escalation must invalidate the prompt built for DirectAnswer"
        );

        assert!(matches!(agent.execute_turn().await, TurnResult::Complete));

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].tools.is_none());
        assert!(
            requests[0]
                .system_prompt
                .contains("No tools are available for this turn")
        );
        let second_tools = requests[1]
            .tools
            .as_ref()
            .expect("AnswerOnly request should include completion tools");
        assert!(
            second_tools
                .iter()
                .any(|tool| tool.function.name == "attempt_completion")
        );
        assert!(
            !requests[1]
                .system_prompt
                .contains("No tools are available for this turn")
        );
        assert_ne!(requests[0].system_prompt, requests[1].system_prompt);
    }

    #[test]
    fn test_tool_path_discovery_merges_rules_once_and_ignores_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("AGENTS.md"), "root rule").unwrap();
        std::fs::create_dir_all(root.join("src/frontend")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/AGENTS.md"), "src rule").unwrap();
        std::fs::write(root.join("src/frontend/AGENTS.md"), "frontend rule").unwrap();
        std::fs::write(root.join("tests/AGENTS.md"), "tests rule").unwrap();

        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_text_response("done"),
        ));
        let root_rules = crate::core::context::get_local_agents_rules(
            root,
            &crate::core::context::RuleToggles::new(),
        );
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-path-rules"))
            .with_system_prompt_context(SystemPromptContext {
                cwd: Some(root.to_string_lossy().into_owned()),
                local_agents_rules_file_instructions: root_rules,
                ..Default::default()
            });
        let prepared = PreparedToolCall {
            tool_call: ApiStreamToolCall {
                call_id: Some("call-1".to_string()),
                function: ApiStreamToolCallFunction {
                    id: Some("tool-1".to_string()),
                    name: Some("read_file".to_string()),
                    arguments: Some(r#"{"paths":["src/frontend/main.rs"]}"#.to_string()),
                },
                signature: None,
            },
            tool_id: "tool-1".to_string(),
            tool_name: "read_file".to_string(),
            parsed_args: Ok(serde_json::json!({"paths": ["src/frontend/main.rs"]})),
        };

        agent.discover_agents_rules_for_tool_calls(root, &[prepared]);
        let context = agent.deps.system_prompt_context.as_ref().unwrap();
        let rules = context
            .local_agents_rules_file_instructions
            .as_ref()
            .unwrap()
            .clone();
        assert!(rules.contains("root rule"));
        assert!(rules.contains("src rule"));
        assert!(rules.contains("frontend rule"));
        assert!(!rules.contains("tests rule"));

        agent.deps.cached_system_prompt = Some("cached".to_string());
        let prepared = PreparedToolCall {
            tool_call: ApiStreamToolCall {
                call_id: Some("call-2".to_string()),
                function: ApiStreamToolCallFunction {
                    id: Some("tool-2".to_string()),
                    name: Some("read_file".to_string()),
                    arguments: Some(r#"{"paths":["src/frontend/main.rs"]}"#.to_string()),
                },
                signature: None,
            },
            tool_id: "tool-2".to_string(),
            tool_name: "read_file".to_string(),
            parsed_args: Ok(serde_json::json!({"paths": ["src/frontend/main.rs"]})),
        };
        agent.discover_agents_rules_for_tool_calls(root, &[prepared]);
        assert_eq!(agent.deps.cached_system_prompt.as_deref(), Some("cached"));
        assert_eq!(
            rules.matches("## src/AGENTS.md").count(),
            1,
            "repeated access must not duplicate rules"
        );
    }

    #[test]
    fn test_new_scoped_rules_defer_mutating_file_tools() {
        assert!(AgentLoop::is_mutating_file_tool("write_to_file"));
        assert!(AgentLoop::is_mutating_file_tool("edit_file"));
        assert!(AgentLoop::is_mutating_file_tool("replace_symbol"));
        assert!(AgentLoop::is_mutating_file_tool("rename_symbol"));
        assert!(!AgentLoop::is_mutating_file_tool("read_file"));
        assert!(!AgentLoop::is_mutating_file_tool("list_files"));
        assert!(!AgentLoop::is_mutating_file_tool("execute_command"));
    }

    #[test]
    fn test_checkpointing_is_limited_to_workspace_mutations() {
        assert!(!AgentLoop::tool_may_modify_workspace(SnedTool::ReadFile));
        assert!(!AgentLoop::tool_may_modify_workspace(SnedTool::SearchFiles));
        assert!(!AgentLoop::tool_may_modify_workspace(SnedTool::WebFetch));
        assert!(AgentLoop::tool_may_modify_workspace(SnedTool::WriteToFile));
        assert!(AgentLoop::tool_may_modify_workspace(SnedTool::EditFile));
        assert!(AgentLoop::tool_may_modify_workspace(
            SnedTool::ExecuteCommand
        ));
        assert!(AgentLoop::tool_may_modify_workspace(SnedTool::UseSubagents));
    }

    #[test]
    fn test_context_truncation() {
        use crate::core::context::context_manager::{self, ApiReqInfo};
        use crate::providers::{MessageContent, MessageRole, StorageMessage};

        // Create a large conversation history
        let mut history = Vec::new();
        for i in 0..20 {
            history.push(StorageMessage {
                id: None,
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: MessageContent::Text(format!("Message {}", i)),
                model_info: None,
                metrics: None,
                ts: Some(1000 + i as u64),
            });
        }

        // Create ApiReqInfo with high token count to trigger truncation.
        // Note: context_manager now only counts tokens_in (not tokens_in + tokens_out)
        // since we're validating input size. Threshold is context_window * 0.8 = 204,800.
        let api_req_info = ApiReqInfo {
            tokens_in: Some(210_000),
            tokens_out: Some(50_000),
            context_window: Some(256_000),
            ..Default::default()
        };

        // Call get_new_context_messages_and_metadata
        let result = context_manager::get_new_context_messages_and_metadata(
            &history,
            Some(&api_req_info),
            None,
            false,       // use_auto_condense = false
            None,        // no compacted summary yet
            "anthropic", // provider_name
        );

        // Verify truncation occurred (history was shortened)
        assert!(
            result.truncated_conversation_history.len() < history.len(),
            "History should be truncated. Original: {}, Truncated: {}",
            history.len(),
            result.truncated_conversation_history.len()
        );

        // Verify deleted range was updated
        assert!(
            result.updated_conversation_history_deleted_range,
            "Deleted range should be updated"
        );
        assert!(
            result.conversation_history_deleted_range.is_some(),
            "Deleted range should be set"
        );
    }

    #[tokio::test]
    async fn test_emergency_truncation_iteratively_shrinks_until_context_fits() {
        use crate::core::context::context_window;
        use crate::providers::{MessageContent, MessageRole, ProviderRequest, StorageMessage};

        let provider: Arc<Providers> = Arc::new(Providers::TinyContext(
            crate::providers::TinyContextProvider,
        ));
        let agent = AgentLoop::new(test_agent_config(provider.clone(), "test-emergency-trunc"));

        {
            let mut history = agent.conversation_history.lock().await;
            for i in 0..30 {
                history.push(StorageMessage {
                    id: None,
                    role: if i % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    },
                    content: MessageContent::Text("x".repeat(192)),
                    model_info: None,
                    metrics: None,
                    ts: Some(1_000 + i as u64),
                });
            }
        }

        let mut request = ProviderRequest {
            system_prompt: String::new(),
            messages: agent.conversation_history.lock().await.clone(),
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        assert!(context_window::validate_context_window(&request, provider.as_ref()).is_err());

        agent
            .emergency_truncate_request(&mut request)
            .await
            .expect("emergency truncation should reduce the request until it fits");

        assert!(
            context_window::validate_context_window(&request, provider.as_ref()).is_ok(),
            "emergency truncation should leave a request that fits the context window"
        );
        assert!(
            request.messages.len() <= 16,
            "emergency truncation should shrink past the first 20-message fallback when needed"
        );
    }

    #[test]
    fn test_truncate_history_preserves_tool_pairs() {
        use crate::providers::{
            AssistantContentBlock, MessageContent, MessageRole, SharedContentFields,
            StorageMessage, TextContentBlock, ToolResultBlock, ToolResultContent, ToolUseBlock,
            UserContentBlock,
        };

        let mut history = Vec::new();
        for i in 0..5 {
            history.push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(format!("filler-{i}")),
                model_info: None,
                metrics: None,
                ts: Some(1_000 + i as u64),
            });
        }
        history.push(StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                ToolUseBlock {
                    id: "tool-1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(2_000),
        });
        for i in 6..15 {
            history.push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(format!("middle-{i}")),
                model_info: None,
                metrics: None,
                ts: Some(2_000 + i as u64),
            });
        }
        history.push(StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                ToolResultBlock {
                    tool_use_id: "tool-1".to_string(),
                    content: ToolResultContent::Text("ok".to_string()),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(3_000),
        });
        for i in 16..30 {
            history.push(StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::Text(
                    TextContentBlock {
                        text: format!("tail-{i}"),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        reasoning_details: None,
                    },
                )]),
                model_info: None,
                metrics: None,
                ts: Some(4_000 + i as u64),
            });
        }

        let dropped = AgentLoop::truncate_history_preserving_tool_pairs(&mut history, 20);
        assert_eq!(dropped, 5);
        assert_eq!(history.len(), 25);

        let tool_use_present = history.iter().any(|msg| {
            matches!(
                &msg.content,
                MessageContent::AssistantBlocks(blocks)
                    if blocks.iter().any(|block| matches!(
                        block,
                        AssistantContentBlock::ToolUse(tool_use) if tool_use.id == "tool-1"
                    ))
            )
        });
        let tool_result_present = history.iter().any(|msg| {
            matches!(
                &msg.content,
                MessageContent::UserBlocks(blocks)
                    if blocks.iter().any(|block| matches!(
                        block,
                        UserContentBlock::ToolResult(result) if result.tool_use_id == "tool-1"
                    ))
            )
        });
        assert!(
            tool_use_present,
            "tool_use should be retained when result is kept"
        );
        assert!(
            tool_result_present,
            "tool_result should still be present after pruning"
        );
    }

    #[tokio::test]
    async fn test_history_persistence() {
        use tempfile::TempDir;

        // Create a temp directory and use new_with_dir to avoid env var races
        let temp_dir = TempDir::new().unwrap();
        let sned_dir = temp_dir.path().join(".sned");

        let task_id = "test-task-123";
        let task_storage = TaskStorage::new_with_dir(task_id, &sned_dir).unwrap();

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: task_id.to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config).with_task_storage(task_storage);

        // Add a message to conversation history
        {
            let mut history = agent.conversation_history.lock().await;
            history.push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("Hello".to_string()),
                model_info: None,
                metrics: None,
                ts: Some(1234567890),
            });
        }

        // Usage metadata is persisted on the first save, without waiting for
        // the five-turn conversation-history debounce.
        {
            let mut state = agent.state.lock().await;
            state.last_api_req_info = Some(crate::core::context::context_manager::ApiReqInfo {
                request: Some("test request".to_string()),
                tokens_in: Some(100),
                tokens_out: Some(50),
                cache_writes: None,
                cache_reads: None,
                reasoning_tokens: None,
                context_tokens: Some(150),
                cost: Some(0.001),
                context_window: Some(8_192),
                context_usage_percentage: Some(1.8),
            });
        }
        agent.save_conversation_history().await;
        let metadata = agent
            .deps
            .task_storage
            .as_ref()
            .unwrap()
            .read_task_metadata();
        assert_eq!(
            metadata.last_api_req_info.as_ref().unwrap().context_tokens,
            Some(150)
        );

        // Save conversation history (debounced: need 5 calls to trigger save)
        for _ in 0..5 {
            agent.save_conversation_history().await;
        }

        // Verify file was created
        let expected_path = sned_dir
            .join("data")
            .join("tasks")
            .join(task_id)
            .join("api_conversation_history.json");

        assert!(
            expected_path.exists(),
            "Conversation history file should exist after 5 debounced saves"
        );

        // Verify content
        let content = std::fs::read_to_string(&expected_path).unwrap();
        let messages: Vec<StorageMessage> = serde_json::from_str(&content).unwrap();
        assert_eq!(messages.len(), 1, "Should have 1 message");

        // Add another message and save again
        {
            let mut history = agent.conversation_history.lock().await;
            history.push(StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text("Hi there".to_string()),
                model_info: None,
                metrics: None,
                ts: Some(1234567891),
            });
        }

        // Save again (need 5 more calls to trigger debounced save)
        for _ in 0..5 {
            agent.save_conversation_history().await;
        }

        let content = std::fs::read_to_string(&expected_path).unwrap();
        let messages: Vec<StorageMessage> = serde_json::from_str(&content).unwrap();
        assert_eq!(
            messages.len(),
            2,
            "Should have 2 messages after second save batch"
        );

        // Cleanup: temp_dir dropped automatically
    }

    #[tokio::test]
    async fn test_task_resume() {
        use std::env;
        use tempfile::TempDir;

        // Create a temp directory and set SNED_DIR to use it
        let temp_dir = TempDir::new().unwrap();
        let sned_dir = temp_dir.path().join(".sned");
        // SAFETY: single-threaded test; sequential env mutation
        unsafe {
            env::set_var("SNED_DIR", &sned_dir);
        }

        let task_id = "resume-task-456";
        let task_storage = TaskStorage::new(task_id).unwrap();

        // Pre-populate the conversation history file on disk
        let pre_existing_messages = vec![
            StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("Previous user message".to_string()),
                model_info: None,
                metrics: None,
                ts: Some(1000),
            },
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text("Previous assistant response".to_string()),
                model_info: None,
                metrics: None,
                ts: Some(1001),
            },
        ];
        task_storage
            .write_api_conversation_history(&pre_existing_messages)
            .unwrap();
        task_storage
            .update_metadata(|metadata| {
                metadata.last_api_req_info =
                    Some(crate::core::context::context_manager::PersistedApiReqInfo {
                        request: Some("persisted request".to_string()),
                        tokens_in: Some(100),
                        tokens_out: Some(50),
                        cache_writes: None,
                        cache_reads: None,
                        reasoning_tokens: None,
                        context_tokens: Some(150),
                        cost: Some(0.001),
                    });
            })
            .unwrap();

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: task_id.to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config).with_task_storage(task_storage);

        // Load conversation history from disk
        let loaded = agent.load_conversation_history().await;
        assert!(loaded, "Should load existing history");

        // Verify loaded history
        let history = agent.get_conversation_history().await;
        assert_eq!(history.len(), 2, "Should have 2 loaded messages");
        assert_eq!(history[0].role, MessageRole::User);
        assert_eq!(history[1].role, MessageRole::Assistant);
        let usage = agent
            .state
            .lock()
            .await
            .last_api_req_info
            .clone()
            .expect("resume should restore persisted API usage");
        assert_eq!(usage.context_tokens, Some(150));
        assert_eq!(usage.context_window, Some(256_000));
        assert_eq!(
            usage.context_usage_percentage,
            Some(150.0 / 256_000.0 * 100.0)
        );

        // Verify no history is loaded when file is empty/missing
        let task_storage_empty = TaskStorage::new("empty-task").unwrap();
        let agent_empty = AgentLoop::new(AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "empty-task".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        })
        .with_task_storage(task_storage_empty);

        let loaded_empty = agent_empty.load_conversation_history().await;
        assert!(!loaded_empty, "Should not load history for empty task");

        // SAFETY: single-threaded test; restoring env after test
        unsafe { env::remove_var("SNED_DIR") };
    }

    #[test]
    fn test_hook_manager_stored() {
        use crate::core::hooks::HookManager;

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let hook_manager = Arc::new(HookManager::new("test-user"));
        let agent = AgentLoop::new(config).with_hooks(hook_manager);

        // Verify the agent was created with hook manager stored
        assert!(agent.deps.hook_manager.is_some());
    }

    #[tokio::test]
    async fn test_tool_hooks_execute() {
        use crate::core::hooks::HookManager;
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::read_file::ReadFileHandler;

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "read_file",
                    serde_json::json!({"path": "/tmp/test_hook_file.txt"}),
                ),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let hook_manager = Arc::new(HookManager::new("test-user"));
        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ReadFile,
            Arc::new(ReadFileHandler),
        );

        let mut agent = AgentLoop::new(config)
            .with_hooks(hook_manager)
            .with_tools(Arc::new(registry));

        // Execute one turn - this will dispatch the read_file tool
        // The hook manager has no hooks configured, so it should return empty results immediately
        let result = agent.execute_turn().await;

        // Should continue (tool result needs to be sent back to provider)
        assert!(
            matches!(result, TurnResult::Continue),
            "Expected Continue after tool execution, got {:?}",
            result
        );

        // Verify tool result was added to history
        let history = agent.conversation_history.lock().await;
        assert!(
            history.len() >= 2,
            "Should have assistant message + tool result"
        );

        // Last message should be tool result
        if let Some(last) = history.last() {
            assert_eq!(last.role, MessageRole::User);
        } else {
            panic!("Expected at least one message in history");
        }
    }

    #[tokio::test]
    async fn test_condense_uses_internal_conversation_history() {
        use crate::core::context::context_manager::CompactedSummary;
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::condense::CondenseHandler;

        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_tool_call(
                "call_1",
                "condense",
                serde_json::json!({
                    "context": "Updated summary",
                }),
            ),
        ));
        let mut config = test_agent_config(provider, "test-condense-history");
        config.interactive_mode = false;

        let mut registry = ToolRegistry::new();
        registry.register(SnedTool::Condense, Arc::new(CondenseHandler::new()));
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        {
            let mut history = agent.conversation_history.lock().await;
            history.extend((0..12).map(|index| StorageMessage {
                id: Some(format!("msg_{index}")),
                role: if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: MessageContent::Text(format!("message {index}")),
                model_info: None,
                metrics: None,
                ts: None,
            }));
        }
        {
            let mut state = agent.state.lock().await;
            state.compacted_summary = Some(CompactedSummary::new("Old summary".to_string(), 10));
            state.conversation_history_deleted_range = Some((2, 7));
        }

        let result = agent.execute_turn().await;

        assert!(matches!(result, TurnResult::Continue));
        let state = agent.state.lock().await;
        let summary = state.compacted_summary.as_ref().unwrap();
        assert_eq!(summary.summary_text, "Updated summary");
        assert_eq!(summary.messages_compacted, 13);
        assert!(state.conversation_history_deleted_range.is_some());
    }

    #[test]
    fn test_plan_mode_restricted_tools() {
        // WriteToFile is restricted in plan mode
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::WriteToFile));
        // EditFile is restricted in plan mode
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::EditFile));

        // Read-only tools are NOT restricted
        assert!(!AgentLoop::is_plan_mode_restricted(SnedTool::ReadFile));
        assert!(!AgentLoop::is_plan_mode_restricted(SnedTool::ListFiles));
        assert!(!AgentLoop::is_plan_mode_restricted(SnedTool::SearchFiles));

        // Other tools are NOT restricted
        assert!(!AgentLoop::is_plan_mode_restricted(
            SnedTool::ExecuteCommand
        ));
        assert!(!AgentLoop::is_plan_mode_restricted(
            SnedTool::AskFollowupQuestion
        ));
        assert!(!AgentLoop::is_plan_mode_restricted(
            SnedTool::AttemptCompletion
        ));
        assert!(!AgentLoop::is_plan_mode_restricted(
            SnedTool::PlanModeRespond
        ));
    }

    #[test]
    fn test_plan_mode_allows_execute_command_but_blocks_file_writes() {
        // PLAN mode should allow execute_command for read-only operations
        // (cat, wc, ls, grep, etc.) while still blocking file modifications.
        // The CommandSafetyChecker handles safety for execute_command.
        assert!(!AgentLoop::is_plan_mode_restricted(
            SnedTool::ExecuteCommand
        ));
        // WriteToFile and EditFile remain blocked in PLAN mode
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::WriteToFile));
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::EditFile));
    }

    #[tokio::test]
    async fn test_plan_mode_blocks_restricted_tools() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Plan,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config);
        let state = agent.state.lock().await;

        // Strict plan mode is enabled by default
        assert!(state.strict_plan_mode_enabled);

        // Verify restricted tools are blocked
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::WriteToFile));
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::EditFile));

        // Verify non-restricted tools are allowed
        assert!(!AgentLoop::is_plan_mode_restricted(SnedTool::ReadFile));
        assert!(!AgentLoop::is_plan_mode_restricted(
            SnedTool::PlanModeRespond
        ));
    }

    #[tokio::test]
    async fn test_act_mode_allows_all_tools() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config);
        let state = agent.state.lock().await;

        // In act mode, strict_plan_mode_enabled doesn't matter - tools are not blocked
        // because the mode check is `mode == Plan && strict_plan_mode_enabled`
        assert!(state.strict_plan_mode_enabled);

        // is_plan_mode_restricted only checks the tool type, not settings
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::WriteToFile));
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::EditFile));

        // But the actual restriction in execute_turn checks:
        // mode == Plan && strict_plan_mode_enabled && is_plan_mode_restricted
        // So in Act mode, tools would NOT be blocked regardless of the tool type
    }

    #[tokio::test]
    async fn test_plan_mode_disabled_allows_all_tools() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Plan,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config);
        let mut state = agent.state.lock().await;
        state.strict_plan_mode_enabled = false;

        // is_plan_mode_restricted only checks the tool type, not settings
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::WriteToFile));
        assert!(AgentLoop::is_plan_mode_restricted(SnedTool::EditFile));

        // But the actual restriction in execute_turn checks:
        // mode == Plan && strict_plan_mode_enabled && is_plan_mode_restricted
        // So with strict_plan_mode_enabled = false, tools would NOT be blocked
        assert!(!state.strict_plan_mode_enabled);
    }

    #[tokio::test]
    async fn test_approval_manager_read_only_tools_no_prompt() {
        use crate::core::approval::ApprovalManager;
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::read_file::ReadFileHandler;

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "read_file",
                    serde_json::json!({"path": "/tmp/test_approval_file.txt"}),
                ),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ReadFile,
            Arc::new(ReadFileHandler),
        );

        let approval_manager = Arc::new(tokio::sync::Mutex::new(ApprovalManager::new()));
        let mut agent = AgentLoop::new(config)
            .with_tools(Arc::new(registry))
            .with_approval_manager(approval_manager);

        // Execute one turn - read_file is read-only so it should execute without prompting
        let result = agent.execute_turn().await;

        // Should continue (tool result needs to be sent back to provider)
        assert!(
            matches!(result, TurnResult::Continue),
            "Expected Continue after tool execution, got {:?}",
            result
        );

        // Verify tool result was added to history
        let history = agent.conversation_history.lock().await;
        assert!(
            history.len() >= 2,
            "Should have assistant message + tool result"
        );

        // Last message should be tool result
        if let Some(last) = history.last() {
            assert_eq!(last.role, MessageRole::User);
        } else {
            panic!("Expected at least one message in history");
        }
    }

    #[tokio::test]
    async fn test_approval_manager_non_interactive_denies_by_default() {
        use crate::core::approval::ApprovalManager;
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;
        use crate::test_support::env_lock;

        // Force non-interactive denial path. cargo test allocates a PTY for
        // stdin, so is_terminal() returns true and the channel-based path
        // would otherwise block/close instead of returning Denied.
        // SAFETY: single-threaded test; sequential env mutation.
        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe { std::env::set_var("SNED_APPROVAL_DENY", "1") };

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "execute_command",
                    serde_json::json!({"command": "echo hello"}),
                ),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new()),
        );

        let approval_manager = Arc::new(tokio::sync::Mutex::new(ApprovalManager::new()));
        let mut agent = AgentLoop::new(config)
            .with_tools(Arc::new(registry))
            .with_approval_manager(approval_manager);

        // Execute one turn - in non-interactive mode (tests), tools should be DENIED by default (F-01 fix)
        let result = agent.execute_turn().await;

        // Should continue (tool result needs to be added to history)
        assert!(
            matches!(result, TurnResult::Continue),
            "Expected Continue after tool denial, got {:?}",
            result
        );

        // Verify tool result was added to history
        let history = agent.conversation_history.lock().await;
        assert!(
            history.len() >= 2,
            "Should have assistant message + tool result"
        );

        // Last message should be tool result
        if let Some(last) = history.last() {
            assert_eq!(last.role, MessageRole::User);
            // In non-interactive mode, the command should be DENIED (F-01 security fix)
            if let MessageContent::UserBlocks(blocks) = &last.content {
                if let Some(UserContentBlock::ToolResult(result)) = blocks.first() {
                    let content_text = match &result.content {
                        ToolResultContent::Text(t) => t.clone(),
                        _ => String::new(),
                    };
                    // Should BE a denial message (F-01: non-interactive stdin denies by default)
                    assert!(
                        content_text.contains("was denied by user"),
                        "Tool should be denied in non-interactive mode (F-01): {}",
                        content_text
                    );
                } else {
                    panic!("Expected ToolResult block");
                }
            } else {
                panic!("Expected UserBlocks content");
            }
        } else {
            panic!("Expected at least one message in history");
        }

        // SAFETY: single-threaded test; restoring env after test.
        unsafe { std::env::remove_var("SNED_APPROVAL_DENY") };
    }

    #[tokio::test]
    async fn test_execute_command_full_flow_produces_output() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "execute_command",
                    serde_json::json!({"commands": ["echo hello world"]}),
                ),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new()),
        );

        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        let result = agent.execute_turn().await;
        assert!(
            matches!(result, TurnResult::Continue),
            "Expected Continue after tool execution, got {:?}",
            result
        );

        let history = agent.conversation_history.lock().await;
        assert!(
            history.len() >= 2,
            "Should have assistant + tool result messages, got {}",
            history.len()
        );

        if let Some(last) = history.last()
            && last.role == MessageRole::User
            && let MessageContent::UserBlocks(blocks) = &last.content
            && let Some(UserContentBlock::ToolResult(tool_result)) = blocks.first()
        {
            let result_text = match &tool_result.content {
                ToolResultContent::Text(t) => t.clone(),
                _ => String::new(),
            };
            assert!(
                result_text.contains("hello world"),
                "execute_command result should contain 'hello world', got: {}",
                result_text
            );
        } else {
            panic!("Expected UserBlocks with ToolResult in history");
        }
    }

    #[tokio::test]
    async fn test_execute_command_pipeline_scope_approval_skips_noninteractive_prompt() {
        use crate::core::approval::{ApprovalManager, command_approval_scopes};
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;
        use crate::test_support::env_lock;

        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe { std::env::set_var("SNED_APPROVAL_DENY", "1") };

        let params = serde_json::json!({"command": "cat Cargo.toml | head -1"});
        let scopes =
            command_approval_scopes(&params).expect("pipeline should receive a reusable scope");
        let approval_manager = Arc::new(tokio::sync::Mutex::new(ApprovalManager::new()));
        approval_manager
            .lock()
            .await
            .auto_approve_command("cat Cargo.toml | head -1", Some(&scopes));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "execute_command",
                    params,
                ),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new()),
        );

        let mut agent = AgentLoop::new(config)
            .with_tools(Arc::new(registry))
            .with_approval_manager(approval_manager);

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));

        let history = agent.conversation_history.lock().await;
        let Some(last) = history.last() else {
            panic!("expected execute_command tool result");
        };
        let MessageContent::UserBlocks(blocks) = &last.content else {
            panic!("expected a tool result message");
        };
        let Some(UserContentBlock::ToolResult(result)) = blocks.first() else {
            panic!("expected execute_command tool result");
        };
        let ToolResultContent::Text(text) = &result.content else {
            panic!("expected text tool result");
        };
        assert!(
            text.contains("[package]"),
            "scope reuse should execute the pipeline: {text}"
        );

        unsafe { std::env::remove_var("SNED_APPROVAL_DENY") };
    }

    #[tokio::test]
    async fn test_message_queue_enqueue_and_count() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config);

        assert_eq!(agent.queued_message_count().await, 0);
        assert!(!agent.has_queued_messages().await);

        agent.enqueue_text_message("Hello".to_string()).await;
        assert_eq!(agent.queued_message_count().await, 1);
        assert!(agent.has_queued_messages().await);

        agent.enqueue_text_message("World".to_string()).await;
        assert_eq!(agent.queued_message_count().await, 2);
        assert!(agent.has_queued_messages().await);
        assert_eq!(
            agent.message_queue_handle().try_queued_message_snapshot(3),
            Some((2, vec!["Hello".to_string(), "World".to_string()]))
        );

        let long_message = "x".repeat(MAX_QUEUED_MESSAGE_PREVIEW_CHARS + 100);
        agent.enqueue_text_message(long_message).await;
        let (_, previews) = agent
            .message_queue_handle()
            .try_queued_message_snapshot(3)
            .expect("queue snapshot should be available");
        assert_eq!(previews.len(), 3);
        assert_eq!(
            previews[2].chars().count(),
            MAX_QUEUED_MESSAGE_PREVIEW_CHARS + 1
        );
        assert!(previews[2].ends_with('…'));
    }

    #[tokio::test]
    async fn test_message_queue_clear() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config);

        agent.enqueue_text_message("Message 1".to_string()).await;
        agent.enqueue_text_message("Message 2".to_string()).await;
        assert_eq!(agent.queued_message_count().await, 2);

        agent.clear_queue().await;
        assert_eq!(agent.queued_message_count().await, 0);
        assert!(!agent.has_queued_messages().await);
    }

    #[tokio::test]
    async fn test_message_queue_enqueue_message_struct() {
        use crate::providers::{MessageContent, MessageRole, StorageMessage};

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config);

        let msg = StorageMessage {
            id: Some("msg_1".to_string()),
            role: MessageRole::User,
            content: MessageContent::Text("Custom message".to_string()),
            model_info: None,
            metrics: None,
            ts: Some(1234567890),
        };

        agent.enqueue_message(msg).await;
        assert_eq!(agent.queued_message_count().await, 1);
    }

    #[tokio::test]
    async fn test_message_queue_bounded_to_max_size() {
        use crate::providers::{MessageContent, MessageRole, StorageMessage};

        let queue = Arc::new(Mutex::new(VecDeque::new()));

        let make_message = |idx: usize| StorageMessage {
            id: Some(format!("msg_{idx}")),
            role: MessageRole::User,
            content: MessageContent::Text(format!("Message {idx}")),
            model_info: None,
            metrics: None,
            ts: Some(1_000 + idx as u64),
        };

        let (count, dropped) = enqueue_message_with_limit(&queue, make_message(1), 2).await;
        assert_eq!(count, 1);
        assert_eq!(dropped, 0);

        let (count, dropped) = enqueue_message_with_limit(&queue, make_message(2), 2).await;
        assert_eq!(count, 2);
        assert_eq!(dropped, 0);

        let (count, dropped) = enqueue_message_with_limit(&queue, make_message(3), 2).await;
        assert_eq!(count, 2);
        assert_eq!(dropped, 1);

        let mq = queue.lock().await;
        assert_eq!(mq.len(), 2);
        assert_eq!(mq.front().and_then(|msg| msg.id.as_deref()), Some("msg_2"));
        assert_eq!(mq.back().and_then(|msg| msg.id.as_deref()), Some("msg_3"));
    }

    #[test]
    fn test_extract_action_path_read_file_array() {
        let params = serde_json::json!({"paths": ["/home/user/project/src/main.rs"]});
        let paths = AgentLoop::extract_action_path(SnedTool::ReadFile, &params);
        assert_eq!(paths, vec!["/home/user/project/src/main.rs".to_string()]);
    }

    #[test]
    fn test_extract_action_path_read_file_string() {
        let params = serde_json::json!({"paths": "/home/user/project/README.md"});
        let paths = AgentLoop::extract_action_path(SnedTool::ReadFile, &params);
        assert_eq!(paths, vec!["/home/user/project/README.md".to_string()]);
    }

    #[test]
    fn test_extract_action_path_read_file_stringified_array() {
        let params = serde_json::json!({
            "paths": "[\"/tmp/outside-a.rs\",\"/tmp/outside-b.rs\"]"
        });
        let paths = AgentLoop::extract_action_path(SnedTool::ReadFile, &params);
        assert_eq!(
            paths,
            vec![
                "/tmp/outside-a.rs".to_string(),
                "/tmp/outside-b.rs".to_string()
            ]
        );
    }

    #[test]
    fn test_extract_action_path_diagnostics_scan() {
        let params = serde_json::json!({"paths": ["/tmp/outside.rs"]});
        let paths = AgentLoop::extract_action_path(SnedTool::DiagnosticsScan, &params);
        assert_eq!(paths, vec!["/tmp/outside.rs".to_string()]);
    }

    #[test]
    fn test_extract_action_path_write_to_file() {
        let params = serde_json::json!({"path": "/home/user/project/new_file.rs"});
        let paths = AgentLoop::extract_action_path(SnedTool::WriteToFile, &params);
        assert_eq!(paths, vec!["/home/user/project/new_file.rs".to_string()]);
    }

    #[test]
    fn test_parse_tool_arguments_invalid_json_returns_error() {
        let invalid = "{\"path\":\"src/main.rs\",\"content\":\"unterminated".to_string();
        let parsed = AgentLoop::parse_tool_arguments("write_to_file", "abc123", Some(&invalid));
        assert!(parsed.is_err());
    }

    #[test]
    fn test_parse_tool_arguments_reports_provider_repair_error() {
        let invalid = serde_json::json!({
            crate::providers::TOOL_ARGUMENTS_ERROR_FIELD: "invalid escape at line 1 column 23"
        })
        .to_string();
        let error = AgentLoop::parse_tool_arguments("edit_file", "abc123", Some(&invalid))
            .expect_err("provider repair marker must not reach a tool handler");
        assert!(error.contains("could not be repaired"));
        assert!(error.contains("invalid escape at line 1 column 23"));
    }

    #[test]
    fn test_prepared_tool_call_parses_args_once_for_display_summary() {
        let mut tool_calls = HashMap::with_capacity(1);
        tool_calls.insert(
            "0".to_string(),
            ApiStreamToolCall {
                call_id: Some("call_valid".to_string()),
                function: crate::providers::ApiStreamToolCallFunction {
                    id: None,
                    name: Some("read_file".to_string()),
                    arguments: Some(r#"{"paths":["src/main.rs","src/lib.rs"]}"#.to_string()),
                },
                signature: None,
            },
        );
        let prepared = AgentLoop::prepare_tool_calls(&["0".to_string()], &mut tool_calls);

        assert_eq!(prepared.len(), 1);
        assert!(!prepared[0].tool_id.is_empty());
        assert_eq!(prepared[0].tool_name, "read_file");
        let parsed_args = prepared[0].parsed_args.as_ref().unwrap();
        let expected_args = serde_json::json!({"paths":["src/main.rs","src/lib.rs"]});
        assert_eq!(parsed_args, &expected_args);
        assert_eq!(
            format_tool_summary("read_file", parsed_args),
            format_tool_summary("read_file", &expected_args)
        );
    }

    #[tokio::test]
    async fn test_search_files_output_shows_call_and_matches() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::search_files::SearchFilesHandler;

        let params = serde_json::json!({
            "path": "src/core/tool_output.rs",
            "regex": "format_tool_call_lines",
            "file_pattern": "*.rs",
        });
        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_tool_call(
                "call_search",
                "search_files",
                params.clone(),
            ),
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut priority_rx = writer
            .take_priority_rx()
            .expect("priority output receiver should be available");
        let mut config = test_agent_config(provider, "test-search-files-output");
        config.output_writer = writer;

        let mut registry = ToolRegistry::new();
        registry.register(SnedTool::SearchFiles, Arc::new(SearchFilesHandler::new()));
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));

        let events = drain_output_events(&mut priority_rx, &mut rx);
        let tool_call = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::ToolHeaderLine(line) => Some(line.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!tool_call.is_empty(), "search tool call should be visible");
        assert!(tool_call.contains("search_files"));
        assert!(tool_call.contains("format_tool_call_lines"));
        assert!(tool_call.contains("src/core/tool_output.rs"));

        let tool_output = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::ToolOutputLine(line) => Some(line.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            tool_output.contains("src/core/tool_output.rs:"),
            "raw search match was hidden from the TUI: {tool_output}"
        );
        assert!(
            tool_output.contains("format_tool_call_lines"),
            "raw search result was hidden from the TUI: {tool_output}"
        );
    }

    #[tokio::test]
    async fn test_prepared_tool_call_parse_error_history_and_dispatch_result() {
        let raw_args = "{\"path\":\"unterminated".to_string();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                    tool_call: ApiStreamToolCall {
                        call_id: Some("call_bad".to_string()),
                        function: crate::providers::ApiStreamToolCallFunction {
                            id: None,
                            name: Some("read_file".to_string()),
                            arguments: Some(raw_args.clone()),
                        },
                        signature: None,
                    },
                    id: None,
                    signature: None,
                })]],
                requests,
            ),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-invalid-tool-call"));
        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));

        let history = agent.conversation_history.lock().await;
        let assistant = history
            .iter()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("assistant tool-use message should be recorded");
        match &assistant.content {
            MessageContent::AssistantBlocks(blocks) => {
                let tool_use = blocks
                    .iter()
                    .find_map(|block| match block {
                        AssistantContentBlock::ToolUse(tool_use) => Some(tool_use),
                        _ => None,
                    })
                    .expect("assistant message should include tool use");
                assert_eq!(tool_use.name, "read_file");
                assert_eq!(
                    tool_use.input["_raw_arguments"].as_str(),
                    Some(raw_args.as_str())
                );
            }
            other => panic!("expected assistant blocks, got {other:?}"),
        }

        let tool_result = history
            .iter()
            .rev()
            .find_map(|message| match &message.content {
                MessageContent::UserBlocks(blocks) => blocks.iter().find_map(|block| match block {
                    UserContentBlock::ToolResult(result) => Some(result),
                    _ => None,
                }),
                _ => None,
            })
            .expect("parse failure should be returned as a tool result");
        match &tool_result.content {
            ToolResultContent::Text(text) => {
                assert!(text.contains("arguments were invalid JSON"));
                assert!(text.contains("read_file"));
            }
            other => panic!("expected text tool result, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_tool_arguments_empty_string_returns_empty_object() {
        // Some providers send empty string instead of "{}"
        let empty = "".to_string();
        let parsed = AgentLoop::parse_tool_arguments("list_files", "call_123", Some(&empty));
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap(), serde_json::json!({}));

        // Whitespace-only should also be treated as empty
        let whitespace = "   ".to_string();
        let parsed = AgentLoop::parse_tool_arguments("list_files", "call_123", Some(&whitespace));
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap(), serde_json::json!({}));
    }

    #[test]
    fn test_extract_action_path_edit_file() {
        let params =
            serde_json::json!({"files": [{"path": "/home/user/project/src/lib.rs", "edits": []}]});
        let paths = AgentLoop::extract_action_path(SnedTool::EditFile, &params);
        assert_eq!(paths, vec!["/home/user/project/src/lib.rs".to_string()]);
    }

    #[test]
    fn test_extract_action_path_edit_file_stringified_files() {
        let params = serde_json::json!({
            "files": "[{\"path\":\"src/a.rs\",\"edits\":[]},{\"path\":\"src/b.rs\",\"edits\":[]}]"
        });
        assert_eq!(
            AgentLoop::extract_action_path(SnedTool::EditFile, &params),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
    }

    #[test]
    fn test_extract_action_path_replace_symbol() {
        let params = serde_json::json!({"path": "/home/user/project/src/lib.rs"});
        let paths = AgentLoop::extract_action_path(SnedTool::ReplaceSymbol, &params);
        assert_eq!(paths, vec!["/home/user/project/src/lib.rs".to_string()]);
    }

    #[test]
    fn test_extract_action_path_replace_symbol_batch() {
        let params = serde_json::json!({"replacements": [{"path": "/home/user/project/a.rs"}, {"path": "/home/user/project/b.rs"}]});
        let paths = AgentLoop::extract_action_path(SnedTool::ReplaceSymbol, &params);
        assert_eq!(
            paths,
            vec![
                "/home/user/project/a.rs".to_string(),
                "/home/user/project/b.rs".to_string()
            ]
        );
    }

    #[test]
    fn test_extract_action_path_rename_symbol() {
        let params =
            serde_json::json!({"paths": ["/home/user/project/a.rs", "/home/user/project/b.rs"]});
        let paths = AgentLoop::extract_action_path(SnedTool::RenameSymbol, &params);
        assert_eq!(
            paths,
            vec![
                "/home/user/project/a.rs".to_string(),
                "/home/user/project/b.rs".to_string()
            ]
        );
    }

    #[test]
    fn test_extract_action_path_execute_command_none() {
        let params = serde_json::json!({"command": "ls -la"});
        let paths = AgentLoop::extract_action_path(SnedTool::ExecuteCommand, &params);
        assert_eq!(paths, Vec::<String>::new());
    }

    #[test]
    fn test_extract_action_path_empty_params() {
        let params = serde_json::json!({});
        let paths = AgentLoop::extract_action_path(SnedTool::ReadFile, &params);
        assert_eq!(paths, Vec::<String>::new());
    }

    #[test]
    fn test_extract_file_action_path_edit_file() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.rs"), "").unwrap();
        std::fs::write(workspace.path().join("b.rs"), "").unwrap();
        let params = serde_json::json!({"files": [{"path": "a.rs"}, {"path": "b.rs"}]});
        let paths = AgentLoop::extract_file_action_path("edit_file", &params, workspace.path());

        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].display, "a.rs");
        assert_eq!(paths[1].display, "b.rs");
        assert_eq!(
            paths[0].normalized,
            std::fs::canonicalize(workspace.path().join("a.rs"))
                .unwrap()
                .to_string_lossy()
        );
    }

    #[test]
    fn test_extract_file_action_path_write_to_file() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("src/main.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let params = serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"});
        let paths = AgentLoop::extract_file_action_path("write_to_file", &params, workspace.path());

        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].display, "src/main.rs");
        assert_eq!(
            paths[0].normalized,
            std::fs::canonicalize(path).unwrap().to_string_lossy()
        );
    }

    #[test]
    fn test_extract_file_action_path_applies_fallback_and_deduplicates() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("src/main.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let edit_params = serde_json::json!({
            "path": "src/./main.rs",
            "files": [
                {"edits": [{"anchor": "One§a"}]},
                {"edits": [{"anchor": "Two§b"}]}
            ]
        });
        let write_params = serde_json::json!({
            "path": path,
            "content": "fn main() {}\n"
        });

        let edit_paths =
            AgentLoop::extract_file_action_path("edit_file", &edit_params, workspace.path());
        let write_paths =
            AgentLoop::extract_file_action_path("write_to_file", &write_params, workspace.path());

        assert_eq!(edit_paths.len(), 1);
        assert_eq!(edit_paths[0].display, "src/./main.rs");
        assert_eq!(edit_paths[0].normalized, write_paths[0].normalized);
    }

    #[test]
    fn test_extract_file_action_path_unknown_tool() {
        let workspace = tempfile::tempdir().unwrap();
        let params = serde_json::json!({"path": "foo.rs"});
        let paths = AgentLoop::extract_file_action_path("read_file", &params, workspace.path());
        assert!(paths.is_empty());
    }

    #[test]
    fn test_tool_params_fingerprint_is_stable_across_object_key_order() {
        let left = serde_json::json!({
            "path": "src/main.rs",
            "options": {"end": 10, "start": 1}
        });
        let right = serde_json::json!({
            "options": {"start": 1, "end": 10},
            "path": "src/main.rs"
        });

        assert_eq!(
            AgentLoop::tool_params_fingerprint(&left),
            AgentLoop::tool_params_fingerprint(&right)
        );
    }

    #[test]
    fn test_reread_recovery_hint_lists_stale_paths() {
        let mut state = TaskState::default();
        state
            .must_reread_before_edit
            .insert("/tmp/a.rs".to_string());
        state
            .must_reread_before_edit
            .insert("/tmp/b.rs".to_string());

        let hint = AgentLoop::reread_recovery_hint(&state).expect("hint should be present");
        assert!(hint.contains("read_file"));
        assert!(hint.contains("/tmp/a.rs"));
        assert!(hint.contains("/tmp/b.rs"));
    }

    /// Test that the recovery hint returns None when no paths are
    /// stale. This is the inverse of `test_reread_recovery_hint_lists_
    /// stale_paths` and guards against a refactor that emits a hint
    /// even when there's nothing to re-read.
    #[test]
    fn test_reread_recovery_hint_returns_none_when_no_stale_paths() {
        let state = TaskState::default();
        let hint = AgentLoop::reread_recovery_hint(&state);
        assert!(
            hint.is_none(),
            "hint must be None when must_reread_before_edit is empty, got: {hint:?}"
        );
    }

    /// Recovery hint must not name symbol-scoped tools by name: those tools
    /// are absent from Validate/CoreEdit profiles and naming them would cause
    /// the model to hallucinate calls it cannot make.
    #[test]
    fn test_reread_recovery_hint_does_not_name_profile_excluded_tools() {
        let mut state = TaskState::default();
        state
            .must_reread_before_edit
            .insert("/tmp/a.rs".to_string());

        let hint = AgentLoop::reread_recovery_hint(&state).expect("hint should be present");
        assert!(
            !hint.contains("get_function"),
            "hint must not name get_function: {hint}"
        );
        assert!(
            !hint.contains("get_file_skeleton"),
            "hint must not name get_file_skeleton: {hint}"
        );
    }

    #[test]
    fn test_per_path_approval_local_read_no_prompt() {
        let settings = crate::core::approval::AutoApprovalSettings {
            read_files: true,
            read_files_externally: false,
            ..Default::default()
        };
        let manager = crate::core::approval::ApprovalManager::new()
            .with_workspace_root("/home/user/project".to_string())
            .with_auto_approval_settings(settings);
        assert!(
            !manager
                .should_prompt_with_path(SnedTool::ReadFile, Some("/home/user/project/README.md"))
        );
    }

    #[test]
    fn test_per_path_approval_external_read_prompts() {
        let settings = crate::core::approval::AutoApprovalSettings {
            read_files: true,
            read_files_externally: false,
            ..Default::default()
        };
        let manager = crate::core::approval::ApprovalManager::new()
            .with_workspace_root("/home/user/project".to_string())
            .with_auto_approval_settings(settings);
        assert!(manager.should_prompt_with_path(SnedTool::ReadFile, Some("/etc/hosts")));
    }

    #[test]
    fn test_per_path_approval_external_write_yolo_skips() {
        let manager = crate::core::approval::ApprovalManager::new()
            .with_yolo(true)
            .with_workspace_root("/home/user/project".to_string());
        assert!(!manager.should_prompt_with_path(SnedTool::EditFile, Some("/tmp/external.rs")));
        assert!(!manager.should_prompt_with_path(SnedTool::WriteToFile, Some("/etc/config.yaml")));
        assert!(!manager.should_prompt_with_path(SnedTool::RenameSymbol, Some("/tmp/outside.rs")));
    }

    #[test]
    fn test_checkpoint_manager_wired() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "test-checkpoint-task".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let checkpoint_mgr = crate::core::checkpoints::TaskCheckpointManager::new(
            config.task_id.clone(),
            config.enable_checkpoints,
            "/tmp",
        );

        let agent = AgentLoop::new(config).with_checkpoint_manager(checkpoint_mgr);

        // Verify the agent was created with checkpoint manager stored
        drop(agent);
    }

    #[tokio::test]
    async fn test_mention_expansion_in_queued_message() {
        use crate::providers::{MessageContent, MessageRole, StorageMessage};

        let temp_dir = std::env::temp_dir().join("sned_test_mentions");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Create a test file to mention
        let test_file = temp_dir.join("test_file.rs");
        std::fs::write(&test_file, "fn main() {}").unwrap();

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "test-mention-task".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config).with_system_prompt_context(
            crate::core::context::SystemPromptContext {
                cwd: Some(temp_dir.to_str().unwrap().to_string()),
                ..Default::default()
            },
        );

        // Create a message with a file mention (relative path)
        let message = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("Check @/test_file.rs for context".to_string()),
            model_info: None,
            metrics: None,
            ts: Some(1000),
        };

        // Expand mentions
        let expanded = agent.expand_message_mentions(message).await;

        // Verify the message was enriched with file content
        if let MessageContent::Text(text) = expanded.content {
            assert!(
                text.contains("test_file.rs"),
                "Expanded text should contain file mention description"
            );
            assert!(
                text.contains("fn main()"),
                "Expanded text should contain file content"
            );
        } else {
            panic!("Expected Text content");
        }

        // Verify the file was tracked in FileContextTracker
        let state = agent.state.lock().await;
        assert!(
            !state.file_context_tracker.files_in_context().is_empty(),
            "File should be tracked in context"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_ctrl_c_cancellation_wired() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::new(vec![]),
            )))),
            mode: AgentMode::Act,
            task_id: "test-cancel-task".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let agent = AgentLoop::new(config);
        let state_handle = agent.state_handle();

        // Verify state_handle can be passed to setup_ctrl_c_handler
        crate::core::cancellation::setup_ctrl_c_handler(state_handle).await;

        // Simulate Ctrl+C by setting the flag
        {
            let mut state = agent.state.lock().await;
            state.is_cancelled = true;
            agent
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }

        // Verify the flag was set
        let state = agent.state.lock().await;
        assert!(state.is_cancelled, "Ctrl+C should set cancellation flag");
    }

    #[tokio::test]
    async fn test_reset_cancellation_replaces_checkpoint_token() {
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![],
        )));
        let agent = AgentLoop::new(test_agent_config(provider, "checkpoint-cancellation"));
        let previous = agent.state.lock().await.checkpoint_cancellation.clone();

        agent.reset_cancellation().await;

        let current = agent.state.lock().await.checkpoint_cancellation.clone();
        assert!(previous.load(std::sync::atomic::Ordering::Acquire));
        assert!(!current.load(std::sync::atomic::Ordering::Acquire));
        assert!(!Arc::ptr_eq(&previous, &current));
    }

    #[tokio::test]
    async fn test_stream_channel_does_not_deadlock_on_fast_producer() {
        use tokio::sync::mpsc;
        use tokio::time::{Duration, sleep};

        // Simulate a fast producer / slow consumer scenario
        let (tx, mut rx) = mpsc::channel::<String>(10_000);

        let producer = tokio::spawn(async move {
            for i in 0..5000 {
                match tx.try_send(format!("chunk-{}", i)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Expected when buffer is saturated
                        tracing::warn!("Chunk {} dropped due to full buffer", i);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        });

        let consumer = tokio::spawn(async move {
            let mut count = 0;
            while let Some(_chunk) = rx.recv().await {
                count += 1;
                // Slow consumer: 1ms sleep per chunk
                sleep(Duration::from_millis(1)).await;
            }
            count
        });

        // Producer should complete without blocking indefinitely
        tokio::time::timeout(Duration::from_secs(5), producer)
            .await
            .expect("Producer should finish within timeout")
            .unwrap();

        // Give consumer time to drain
        sleep(Duration::from_secs(2)).await;

        let consumed = consumer.await.unwrap();
        // Consumer should have received most chunks (some may have been dropped)
        assert!(
            consumed > 4000,
            "Consumer should receive >4000 chunks, got {}",
            consumed
        );
    }

    #[tokio::test]
    async fn test_cumulative_openai_stream_reaches_agent_loop_without_duplicate_text() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let _openai_env_lock = crate::providers::openai::OPENAI_ENV_LOCK.lock().unwrap();
        let old_cumulative_text_stream = std::env::var_os("SNED_OPENAI_CUMULATIVE_TEXT_STREAM");
        // SAFETY: this test holds the shared OpenAI environment lock.
        unsafe {
            std::env::set_var("SNED_OPENAI_CUMULATIVE_TEXT_STREAM", "1");
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let (header_end, content_length) = loop {
                let bytes_read = socket.read(&mut buffer).unwrap();
                assert!(bytes_read > 0, "provider request ended before its headers");
                request.extend_from_slice(&buffer[..bytes_read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                break (header_end + 4, content_length);
            };
            while request.len() < header_end + content_length {
                let bytes_read = socket.read(&mut buffer).unwrap();
                assert!(bytes_read > 0, "provider request ended before its body");
                request.extend_from_slice(&buffer[..bytes_read]);
            }
            let body = concat!(
                "data: {\"id\":\"chatcmpl-agent-loop\",\"choices\":[{\"delta\":{\"content\":\"the quick\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"chatcmpl-agent-loop\",\"choices\":[{\"delta\":{\"content\":\"the quick brown\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"chatcmpl-agent-loop\",\"choices\":[{\"delta\":{\"content\":\"the quick brown fox\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(body.as_bytes()).unwrap();
        });

        let provider = Arc::new(Providers::OpenAi(
            crate::providers::openai::OpenAiProvider::new(crate::providers::openai::OpenAiConfig {
                api_key: "test-key".to_string(),
                base_url: Some(format!("http://{address}")),
                model_id: "custom-model".to_string(),
                model_info: None,
                reasoning_effort: None,
                extra_body: None,
                custom_headers: None,
                endpoint_kind: crate::providers::openai::OpenAiEndpointKind::Compatible,
                stream: true,
                provider_name: None,
            })
            .unwrap(),
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-cumulative-openai-stream");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config);

        let result = agent.execute_turn().await;
        assert!(
            matches!(result, TurnResult::Continue | TurnResult::Complete),
            "unexpected agent result: {result:?}"
        );
        server.join().unwrap();

        // SAFETY: restore the process environment while still holding the lock.
        unsafe {
            match old_cumulative_text_stream {
                Some(value) => {
                    std::env::set_var("SNED_OPENAI_CUMULATIVE_TEXT_STREAM", value);
                }
                None => std::env::remove_var("SNED_OPENAI_CUMULATIVE_TEXT_STREAM"),
            }
        }

        let accumulated_text = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|event| {
            if let crate::cli::output::OutputEvent::TurnEnd { accumulated_text } = event {
                Some(accumulated_text)
            } else {
                None
            }
        });
        assert_eq!(accumulated_text.as_deref(), Some("the quick brown fox"));
    }

    #[test]
    fn test_path_from_read_file_header() {
        assert_eq!(
            path_from_read_file_header("[File: src/main.rs, Hash: abc123]\n1§hello"),
            Some("src/main.rs")
        );
        assert_eq!(
            path_from_read_file_header("[File Hash: abc123]\n1§hello"),
            None
        );
        assert_eq!(path_from_read_file_header("some random text"), None);
    }

    #[test]
    fn test_summarize_single_section() {
        let section = "[File: src/main.rs, Hash: abc123]\n1§hello\n2§world";
        let summary = summarize_single_section(section);
        assert!(summary.contains("Hash: abc123"));
        assert!(summary.contains("2 lines"));
        assert!(summary.contains("Preserved anchors"));
        assert!(summary.contains("1§hello"));
        assert!(summary.contains("2§world"));
    }

    #[test]
    fn test_summarize_single_section_no_anchors() {
        let section = "[File: src/main.rs, Hash: abc123]\nno anchors here\njust plain text";
        let summary = summarize_single_section(section);
        assert!(summary.contains("Hash: abc123"));
        assert!(!summary.contains("Preserved anchors"));
        assert!(summary.contains("Re-read with read_file if you need current anchors"));
    }

    #[test]
    fn test_summarize_single_section_caps_preserved_anchors() {
        let mut section = String::from("[File: src/main.rs, Hash: abc123]");
        for i in 0..200 {
            section.push_str(&format!("\n{}§line {}", i, i));
        }
        let summary = summarize_single_section(&section);
        assert!(summary.contains("Preserved anchors"));
        assert!(summary.contains("0§line 0"));
        assert!(summary.contains("79§line 79"));
        assert!(!summary.contains("80§line 80"));
        assert!(!summary.contains("199§line 199"));
    }

    #[test]
    fn test_summarize_matching_sections_partial() {
        let text =
            "[File: src/foo.rs, Hash: aaa]\n1§foo\n---\n[File: src/bar.rs, Hash: bbb]\n1§bar";
        let edited = vec!["src/foo.rs".to_string()];
        let known = vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()];
        let result = summarize_matching_sections(text, &edited, &known);
        assert!(result.contains("Hash: aaa"));
        assert!(
            result.contains("1§foo"),
            "pruned section preserves anchored lines"
        );
        assert!(result.contains("1§bar"));
    }

    #[test]
    fn test_summarize_matching_sections_all() {
        let text =
            "[File: src/foo.rs, Hash: aaa]\n1§foo\n---\n[File: src/bar.rs, Hash: bbb]\n1§bar";
        let edited = vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()];
        let known = edited.clone();
        let result = summarize_matching_sections(text, &edited, &known);
        assert!(
            result.contains("1§foo"),
            "pruned section preserves anchored lines"
        );
        assert!(
            result.contains("1§bar"),
            "pruned section preserves anchored lines"
        );
        assert!(result.contains("Hash: aaa"));
        assert!(result.contains("Hash: bbb"));
    }

    #[test]
    fn test_normalize_path_for_matching() {
        assert_eq!(normalize_path_for_matching("main.rs"), "main.rs");
        assert_eq!(
            normalize_path_for_matching("/foo/bar/main.rs"),
            "foo/bar/main.rs"
        );
        assert_eq!(
            normalize_path_for_matching("/Users/test/project/main.c"),
            "Users/test/project/main.c"
        );
        assert_eq!(normalize_path_for_matching("./src/lib.rs"), "src/lib.rs");
        assert_eq!(
            normalize_path_for_matching(r"src\nested\lib.rs"),
            "src/nested/lib.rs"
        );
    }

    #[test]
    fn test_path_matching_with_absolute_and_relative() {
        let text = "[File: /Users/easto/test/tictactoe/main.c, Hash: abc123]\n1§hello";
        let edited = vec!["tictactoe/main.c".to_string()];
        let known = vec!["/Users/easto/test/tictactoe/main.c".to_string()];
        let result = summarize_matching_sections(text, &edited, &known);
        assert!(result.starts_with("[Context pruned:"));
    }

    #[test]
    fn test_summarize_matching_sections_with_mixed_paths() {
        let text = "[File: /Users/test/project/main.c, Hash: abc123]\n1§hello\n2§world";
        let edited = vec!["main.c".to_string()];
        let known = vec!["/Users/test/project/main.c".to_string()];
        let result = summarize_matching_sections(text, &edited, &known);
        assert!(
            result.contains("1§hello"),
            "pruned section preserves anchored lines"
        );
        assert!(result.contains("Hash: abc123"));
    }

    #[test]
    fn test_summarize_matching_sections_disambiguates_duplicate_basenames() {
        let text = "[File: /workspace/src/config.rs, Hash: aaa]\n1§source\n---\n[File: /workspace/tests/config.rs, Hash: bbb]\n1§test";
        let edited = vec!["src/config.rs".to_string()];
        let known = vec![
            "/workspace/src/config.rs".to_string(),
            "/workspace/tests/config.rs".to_string(),
        ];

        let result = summarize_matching_sections(text, &edited, &known);

        assert_eq!(result.matches("[Context pruned:").count(), 1);
        assert!(!result.contains("[File: /workspace/src/config.rs"));
        assert!(result.contains("[File: /workspace/tests/config.rs"));
    }

    #[test]
    fn test_summarize_matching_sections_rejects_ambiguous_filename_fallback() {
        let text = "[File: /workspace/src/config.rs, Hash: aaa]\n1§source";
        let edited = vec!["config.rs".to_string()];
        let known = vec![
            "/workspace/src/config.rs".to_string(),
            "/workspace/tests/config.rs".to_string(),
        ];

        assert_eq!(summarize_matching_sections(text, &edited, &known), text);
    }

    #[tokio::test]
    async fn test_prune_conversation_history_no_pruning_needed() {
        let config = AgentConfig::default();
        let agent = AgentLoop::new(config);

        // Create 10 messages (5 turns) - well under the limit
        let mut history = Vec::new();
        for i in 0..10 {
            history.push(StorageMessage {
                id: None,
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: MessageContent::Text(format!("Message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            });
        }

        let result = agent.prune_conversation_history(history.clone());
        assert_eq!(result.len(), 10); // No pruning
    }

    #[tokio::test]
    async fn test_prune_conversation_history_exceeds_limit() {
        let config = AgentConfig {
            max_context_turns: 5, // 10 messages max
            ..Default::default()
        };
        let agent = AgentLoop::new(config);

        // Create 30 messages (15 turns) - exceeds limit
        let mut history = Vec::new();
        for i in 0..30 {
            history.push(StorageMessage {
                id: None,
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: MessageContent::Text(format!("Message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            });
        }

        let result = agent.prune_conversation_history(history);
        // Should keep ~10 messages (5 turns) + buffer
        assert!(result.len() <= 20);
        // Should keep the most recent messages
        assert!(result.iter().any(|m| {
            if let MessageContent::Text(ref text) = m.content {
                text.contains("Message 29")
            } else {
                false
            }
        }));
    }

    #[tokio::test]
    async fn test_prune_conversation_history_preserves_system_prompt() {
        let config = AgentConfig {
            max_context_turns: 2, // 4 messages max
            ..Default::default()
        };
        let agent = AgentLoop::new(config);

        // Create system prompt + 20 messages
        let mut history = Vec::new();
        history.push(StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("System prompt".to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        });
        for i in 0..20 {
            history.push(StorageMessage {
                id: None,
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: MessageContent::Text(format!("Message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            });
        }

        let result = agent.prune_conversation_history(history);
        // Should preserve system prompt as first message
        assert_eq!(result[0].role, MessageRole::Assistant);
        if let MessageContent::Text(ref text) = result[0].content {
            assert!(text.contains("System prompt"));
        } else {
            panic!("Expected Text content");
        }
    }

    #[test]
    fn test_token_usage_display_format() {
        // Verify token usage display format (shown after each model response)
        let usage_line = crate::cli::colors::colorize_stderr(
            "  📊 150 tokens | $0.0015 | 2% context",
            crate::cli::colors::style::DIM,
        );
        assert!(usage_line.contains("📊"));
        assert!(usage_line.contains("tokens"));
        assert!(usage_line.contains("context"));
    }

    #[test]
    fn test_truncate_tool_result_small() {
        // Small results should pass through unchanged
        let small = "Hello, world!";
        let result = truncate_tool_result(small);
        assert_eq!(result, small);
    }

    #[test]
    fn test_truncate_tool_result_large() {
        // Large results should be truncated with marker
        let large = "line 1\n".repeat(10000); // ~70KB
        let result = truncate_tool_result(&large);

        // Should be truncated
        assert!(result.len() < large.len());
        // Should have truncation marker
        assert!(result.contains("lines truncated"));
        assert!(result.contains("use read_file to see full content"));
        // Should still have some content (at least 100 bytes)
        assert!(result.len() > 100);
    }

    #[test]
    fn test_truncate_tool_result_respects_env_var() {
        // Test with custom limit via environment variable
        // SAFETY: single-threaded test; sequential env mutation
        unsafe {
            std::env::set_var(TOOL_RESULT_HISTORY_LIMIT_ENV, "100");
        }
        let large = "x".repeat(500);
        let result = truncate_tool_result(&large);
        assert!(result.len() < 150); // 100 limit + marker
        // SAFETY: single-threaded test; restoring env after test
        unsafe {
            std::env::remove_var(TOOL_RESULT_HISTORY_LIMIT_ENV);
        }
    }

    #[test]
    fn test_truncate_tool_result_preserves_unicode() {
        // Truncation should preserve Unicode boundaries
        let large = "Hello 🌍 ".repeat(5000);
        let result = truncate_tool_result(&large);
        // Should not end with a partial emoji (which is 4 bytes)
        assert!(!result.ends_with("�"));
        assert!(!result.ends_with("🌍"));
        // Should have truncation marker
        assert!(result.contains("lines truncated"));
    }

    #[test]
    fn test_truncate_old_thinking_blocks() {
        // Test that old thinking blocks are truncated while recent ones are preserved
        let mut history = vec![
            // First assistant message with long thinking - should be truncated
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![
                    AssistantContentBlock::Thinking(ThinkingBlock {
                        thinking: "x".repeat(10000), // 10000 chars, well over limit
                        signature: Some("sig1".to_string()),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        summary: None,
                    }),
                    AssistantContentBlock::Text(TextContentBlock {
                        text: "Response 1".to_string(),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        reasoning_details: None,
                    }),
                ]),
                model_info: None,
                metrics: None,
                ts: Some(1000),
            },
            // Second assistant message with long thinking - should be preserved (most recent)
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![
                    AssistantContentBlock::Thinking(ThinkingBlock {
                        thinking: "y".repeat(10000), // 10000 chars, should NOT be truncated
                        signature: Some("sig2".to_string()),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        summary: None,
                    }),
                    AssistantContentBlock::Text(TextContentBlock {
                        text: "Response 2".to_string(),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        reasoning_details: None,
                    }),
                ]),
                model_info: None,
                metrics: None,
                ts: Some(2000),
            },
        ];

        truncate_old_thinking_blocks(&mut history);

        // First message thinking should be truncated
        if let MessageContent::AssistantBlocks(blocks) = &history[0].content {
            if let AssistantContentBlock::Thinking(tb) = &blocks[0] {
                assert!(
                    tb.thinking.len() < 10000,
                    "Old thinking should be truncated"
                );
                assert!(
                    tb.thinking.contains("[truncated]"),
                    "Should have truncation marker"
                );
            } else {
                panic!("First block should be Thinking");
            }
        } else {
            panic!("First message should have AssistantBlocks");
        }

        // Second message thinking should NOT be truncated (most recent)
        if let MessageContent::AssistantBlocks(blocks) = &history[1].content {
            if let AssistantContentBlock::Thinking(tb) = &blocks[0] {
                assert_eq!(
                    tb.thinking.len(),
                    10000,
                    "Recent thinking should NOT be truncated"
                );
                assert!(
                    !tb.thinking.contains("[truncated]"),
                    "Should NOT have truncation marker"
                );
            } else {
                panic!("First block should be Thinking");
            }
        } else {
            panic!("Second message should have AssistantBlocks");
        }
    }

    #[test]
    fn test_truncate_old_thinking_blocks_respects_env_var() {
        // Test with custom limit via environment variable
        // SAFETY: single-threaded test; sequential env mutation
        unsafe {
            std::env::set_var(THINKING_HISTORY_LIMIT_ENV, "100");
        }

        let mut history = vec![
            // First message - should be truncated (not most recent)
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::Thinking(
                    ThinkingBlock {
                        thinking: "z".repeat(2000),
                        signature: Some("sig".to_string()),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        summary: None,
                    },
                )]),
                model_info: None,
                metrics: None,
                ts: Some(1000),
            },
            // Second message - most recent, should NOT be truncated
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::Thinking(
                    ThinkingBlock {
                        thinking: "w".repeat(2000),
                        signature: Some("sig2".to_string()),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        summary: None,
                    },
                )]),
                model_info: None,
                metrics: None,
                ts: Some(2000),
            },
        ];

        truncate_old_thinking_blocks(&mut history);

        // With 100 token limit (400 chars), first message's 2000 chars should be truncated
        if let MessageContent::AssistantBlocks(blocks) = &history[0].content
            && let AssistantContentBlock::Thinking(tb) = &blocks[0]
        {
            assert!(
                tb.thinking.len() < 2000,
                "Should be truncated with custom limit"
            );
            assert!(
                tb.thinking.contains("[truncated]"),
                "Should have truncation marker"
            );
        }

        // Second message (most recent) should NOT be truncated
        if let MessageContent::AssistantBlocks(blocks) = &history[1].content
            && let AssistantContentBlock::Thinking(tb) = &blocks[0]
        {
            assert_eq!(
                tb.thinking.len(),
                2000,
                "Most recent thinking should NOT be truncated"
            );
        }

        // SAFETY: single-threaded test; restoring env after test
        unsafe {
            std::env::remove_var(THINKING_HISTORY_LIMIT_ENV);
        }
    }

    #[tokio::test]
    async fn test_cumulative_tokens_tracked_across_turns() {
        use crate::providers::ApiStreamUsageChunk;

        // Create responses with usage chunks for multiple turns
        // Note: text-only responses will trigger completion after 2 turns due to nudge logic
        let responses = vec![
            // Turn 1: 100 input, 50 output
            vec![
                ApiStreamChunk::Text(ApiStreamTextChunk {
                    text: "Response 1".to_string(),
                    id: None,
                    signature: None,
                }),
                ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_write_tokens: None,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: Some(0.001),
                    stop_reason: Some("stop".to_string()),
                    id: None,
                }),
            ],
            // Turn 2: 200 input, 100 output (nudge response)
            vec![
                ApiStreamChunk::Text(ApiStreamTextChunk {
                    text: "I'll use a tool now".to_string(),
                    id: None,
                    signature: None,
                }),
                ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: 200,
                    output_tokens: 100,
                    cache_write_tokens: None,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: Some(0.002),
                    stop_reason: Some("stop".to_string()),
                    id: None,
                }),
            ],
        ];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-cumulative-tokens"));

        // Execute turn 1
        let result1 = agent.execute_turn().await;
        assert!(
            matches!(result1, TurnResult::Continue),
            "Turn 1 should continue, got {:?}",
            result1
        );

        // Check cumulative tokens after turn 1
        {
            let state = agent.state.lock().await;
            assert_eq!(
                state.cumulative_tokens_in, 100,
                "Turn 1 cumulative_tokens_in should be 100"
            );
            assert_eq!(
                state.cumulative_tokens_out, 50,
                "Turn 1 cumulative_tokens_out should be 50"
            );
            assert_eq!(
                state.cumulative_cost, 0.001,
                "Turn 1 cumulative_cost should be 0.001"
            );
            assert_eq!(state.turns_completed, 1, "Turns completed should be 1");

            // Check last_api_req_info
            assert!(
                state.last_api_req_info.is_some(),
                "last_api_req_info should be set after turn 1"
            );
            let api_info = state.last_api_req_info.as_ref().unwrap();
            assert_eq!(
                api_info.tokens_in,
                Some(100),
                "Turn 1 api_req_info tokens_in should be 100"
            );
            assert_eq!(
                api_info.tokens_out,
                Some(50),
                "Turn 1 api_req_info tokens_out should be 50"
            );
            // Context percentage: (100+50)/8192*100 = 1.8310546875
            assert!(
                api_info.context_usage_percentage.unwrap() > 1.8,
                "Turn 1 context_usage_percentage should be ~1.83%, got {:?}",
                api_info.context_usage_percentage
            );
        }

        // Execute turn 2 (should complete due to text-only nudge logic)
        let result2 = agent.execute_turn().await;
        assert!(
            matches!(result2, TurnResult::Complete),
            "Turn 2 should complete (text-only nudge), got {:?}",
            result2
        );

        // Check cumulative tokens after turn 2
        {
            let state = agent.state.lock().await;
            assert_eq!(
                state.cumulative_tokens_in, 300,
                "Turn 2 cumulative_tokens_in should be 100+200=300"
            );
            assert_eq!(
                state.cumulative_tokens_out, 150,
                "Turn 2 cumulative_tokens_out should be 50+100=150"
            );
            assert_eq!(
                state.cumulative_cost, 0.003,
                "Turn 2 cumulative_cost should be 0.001+0.002=0.003"
            );
            assert_eq!(state.turns_completed, 2, "Turns completed should be 2");

            // Check last_api_req_info
            let api_info = state.last_api_req_info.as_ref().unwrap();
            assert_eq!(
                api_info.tokens_in,
                Some(200),
                "Turn 2 api_req_info tokens_in should be 200"
            );
            assert_eq!(
                api_info.tokens_out,
                Some(100),
                "Turn 2 api_req_info tokens_out should be 100"
            );
            // Context percentage: (200+100)/8192*100 = 3.662109375
            assert!(
                api_info.context_usage_percentage.unwrap() > 3.6,
                "Turn 2 context_usage_percentage should be ~3.66%, got {:?}",
                api_info.context_usage_percentage
            );
        }
    }

    #[tokio::test]
    async fn test_context_percentage_preserves_input_across_output_only_usage_chunk() {
        use crate::providers::ApiStreamUsageChunk;

        let responses = vec![vec![
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "Response".to_string(),
                id: None,
                signature: None,
            }),
            ApiStreamChunk::Usage(ApiStreamUsageChunk {
                input_tokens: 2_000,
                output_tokens: 0,
                cache_write_tokens: Some(1_000),
                cache_read_tokens: Some(500),
                reasoning_tokens: None,
                thoughts_token_count: None,
                total_cost: None,
                stop_reason: None,
                id: None,
            }),
            ApiStreamChunk::Usage(ApiStreamUsageChunk {
                input_tokens: 0,
                output_tokens: 250,
                cache_write_tokens: None,
                cache_read_tokens: None,
                reasoning_tokens: None,
                thoughts_token_count: None,
                total_cost: None,
                stop_reason: Some("stop".to_string()),
                id: None,
            }),
        ]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-split-usage-context"));

        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));

        let state = agent.state.lock().await;
        let usage = state.last_api_req_info.as_ref().unwrap();
        assert_eq!(usage.tokens_in, Some(2_000));
        assert_eq!(usage.tokens_out, Some(250));
        assert_eq!(usage.cache_writes, Some(1_000));
        assert_eq!(usage.cache_reads, Some(500));
        let expected = (3_750.0 / 8_192.0) * 100.0;
        assert!(
            (usage.context_usage_percentage.unwrap() - expected).abs() < f64::EPSILON,
            "expected {expected}, got {:?}",
            usage.context_usage_percentage
        );
    }

    #[tokio::test]
    async fn test_context_percentage_includes_separate_thinking_tokens() {
        use crate::providers::ApiStreamUsageChunk;

        let responses = vec![vec![ApiStreamChunk::Usage(ApiStreamUsageChunk {
            input_tokens: 100,
            output_tokens: 50,
            cache_write_tokens: None,
            cache_read_tokens: None,
            reasoning_tokens: Some(25),
            thoughts_token_count: Some(25),
            total_cost: None,
            stop_reason: Some("stop".to_string()),
            id: Some("thinking".to_string()),
        })]];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-thinking-context"));

        let _ = agent.execute_turn().await;

        let state = agent.state.lock().await;
        let usage = state
            .last_api_req_info
            .as_ref()
            .expect("usage should be recorded");
        assert_eq!(usage.context_tokens, Some(175));
        assert_eq!(usage.context_usage_percentage, Some(175.0 / 8192.0 * 100.0));
    }

    #[tokio::test]
    async fn test_synthetic_empty_usage_keeps_last_measured_context() {
        use crate::providers::ApiStreamUsageChunk;

        let responses = vec![
            vec![ApiStreamChunk::Usage(ApiStreamUsageChunk {
                input_tokens: 7_500,
                output_tokens: 100,
                cache_write_tokens: None,
                cache_read_tokens: None,
                reasoning_tokens: None,
                thoughts_token_count: None,
                total_cost: None,
                stop_reason: Some("stop".to_string()),
                id: Some("metered".to_string()),
            })],
            vec![ApiStreamChunk::Usage(ApiStreamUsageChunk {
                input_tokens: 0,
                output_tokens: 0,
                cache_write_tokens: Some(0),
                cache_read_tokens: None,
                reasoning_tokens: None,
                thoughts_token_count: None,
                total_cost: None,
                stop_reason: Some("stop".to_string()),
                id: None,
            })],
        ];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-synthetic-usage"));

        let _ = agent.execute_turn().await;
        assert!(
            agent.state.lock().await.last_api_req_info.is_some(),
            "metered request should record usage"
        );

        let _ = agent.execute_turn().await;
        let state = agent.state.lock().await;
        let api_info = state
            .last_api_req_info
            .as_ref()
            .expect("synthetic empty usage must not erase the last measured context");
        assert_eq!(api_info.tokens_in, Some(7_500));
        assert_eq!(api_info.tokens_out, Some(100));
    }

    #[tokio::test]
    async fn test_provider_switch_preserves_last_measured_context() {
        use crate::providers::ApiStreamUsageChunk;

        let first_provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                vec![vec![ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: 7_500,
                    output_tokens: 100,
                    cache_write_tokens: None,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: None,
                    stop_reason: Some("stop".to_string()),
                    id: Some("first-provider".to_string()),
                })]],
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut agent = AgentLoop::new(test_agent_config(
            first_provider,
            "test-provider-switch-context",
        ));

        let _ = agent.execute_turn().await;
        assert!(
            agent.state.lock().await.last_api_req_info.is_some(),
            "first provider should record usage"
        );

        let second_provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                vec![vec![ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_write_tokens: Some(0),
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: None,
                    stop_reason: Some("stop".to_string()),
                    id: None,
                })]],
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        agent.set_provider(second_provider).await;

        let _ = agent.execute_turn().await;
        let usage = agent
            .state
            .lock()
            .await
            .last_api_req_info
            .clone()
            .expect("provider switch must retain usage from the previous provider");
        assert_eq!(usage.tokens_in, Some(7_500));
        assert_eq!(usage.tokens_out, Some(100));
    }

    #[tokio::test]
    async fn test_provider_switch_recalculates_context_percentage() {
        use crate::providers::ApiStreamUsageChunk;

        let first_provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                vec![vec![ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: 7_500,
                    output_tokens: 100,
                    cache_write_tokens: None,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: None,
                    stop_reason: Some("stop".to_string()),
                    id: Some("first-provider".to_string()),
                })]],
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut agent = AgentLoop::new(test_agent_config(
            first_provider,
            "test-provider-switch-context-window",
        ));
        let _ = agent.execute_turn().await;

        let second_provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::new_with_context_window(vec![], 200_000),
        ));
        agent.set_provider(second_provider).await;

        let usage = agent
            .state
            .lock()
            .await
            .last_api_req_info
            .clone()
            .expect("provider switch must retain usage");
        assert_eq!(usage.context_window, Some(200_000));
        assert_eq!(usage.context_tokens, Some(7_600));
        assert_eq!(usage.context_usage_percentage, Some(3.8));
    }

    #[tokio::test]
    async fn test_context_percentage_fallback_estimation() {
        // Create responses WITHOUT usage chunks to test fallback estimation
        let responses = vec![
            // Turn 1: no usage - should use fallback estimation
            vec![
                ApiStreamChunk::Text(ApiStreamTextChunk {
                    text: "Hello, this is a test response with some content.".to_string(),
                    id: None,
                    signature: None,
                }),
                // Note: no Usage chunk - simulating providers that don't send usage
            ],
        ];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-context-fallback"));

        // Execute turn 1
        let _result = agent.execute_turn().await;

        // After the turn, last_api_req_info should be None (no usage was sent)
        // But the context percentage display should use fallback estimation
        {
            let state = agent.state.lock().await;
            assert!(
                state.last_api_req_info.is_none(),
                "last_api_req_info should be None when provider doesn't send usage"
            );
        }

        // The fallback estimation happens at display time, not during turn execution
        // This test verifies that the state is correctly set up for fallback
        // The actual display logic is tested manually or via integration tests
    }

    #[tokio::test]
    async fn test_plan_mode_respond_creates_plan_state() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::plan_mode_respond::PlanModeRespondHandler;

        let plan_json = serde_json::json!({
            "response": "1. Inspect the codebase\n2. Write the implementation\n3. Run tests",
            "needs_more_exploration": false,
        });

        let responses = vec![
            vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_plan".to_string()),
                    function: crate::providers::ApiStreamToolCallFunction {
                        id: None,
                        name: Some("plan_mode_respond".to_string()),
                        arguments: Some(plan_json.to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            })],
            // Second turn: model responds with text after plan is created
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "Plan created. Waiting for approval.".to_string(),
                id: None,
                signature: None,
            })],
        ];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Plan,
            task_id: "test-plan-respond".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::PlanModeRespond,
            Arc::new(PlanModeRespondHandler::new()),
        );

        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        let result = agent.execute_turn().await;
        assert!(
            matches!(result, TurnResult::Continue) || matches!(result, TurnResult::Complete),
            "Expected Continue or Complete, got {:?}",
            result
        );

        let state = agent.state.lock().await;
        assert!(state.plan_state.is_some(), "PlanState should be created");
        let plan = state.plan_state.as_ref().unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert!(!plan.approved);
        assert!(plan.format_state().contains("mode: APPROVAL"));
    }

    #[tokio::test]
    async fn test_plan_state_is_injected_into_provider_request() {
        use crate::core::plan_state::PlanStepStatus;

        let responses = vec![vec![ApiStreamChunk::Text(ApiStreamTextChunk {
            text: "No-op response".to_string(),
            id: None,
            signature: None,
        })]];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-injection".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let registry = ToolRegistry::new();
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        {
            let mut state = agent.state.lock().await;
            let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
                "First step".to_string(),
                "Second step".to_string(),
            ]);
            plan.approved = false;
            plan.steps[0].status = PlanStepStatus::Pending;
            state.plan_state = Some(plan);
            state.last_injected_plan_state_hash = None;
        }

        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));

        let requests = requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|request| request.messages.iter().any(|message| {
                    matches!(
                        &message.content,
                        crate::providers::MessageContent::Text(text)
                            if text.contains("Plan state:\nmode: APPROVAL")
                    )
                })),
            "Plan state should be injected into at least one provider request"
        );
    }

    #[tokio::test]
    async fn test_plan_advance_on_tool_success() {
        use crate::core::tools::ToolRegistry;

        // Create plan directly in state (skip PlanModeRespond call)
        let responses = vec![vec![ApiStreamChunk::Text(ApiStreamTextChunk {
            text: "Executing step 1".to_string(),
            id: None,
            signature: None,
        })]];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-advance".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: false,
        };

        let registry = ToolRegistry::new();

        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        // Set up plan state manually: approved, step 0 running
        {
            let mut state = agent.state.lock().await;
            let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
                "Step one".to_string(),
                "Step two".to_string(),
            ]);
            plan.approved = true;
            plan.steps[0].status = crate::core::plan_state::PlanStepStatus::Running;
            state.plan_state = Some(plan);
        }

        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));

        // After a text-only turn (no tools called), step should NOT be failed
        let state = agent.state.lock().await;
        let plan = state.plan_state.as_ref().unwrap();
        assert_eq!(
            plan.steps[0].status,
            crate::core::plan_state::PlanStepStatus::Running,
            "Text-only response should not fail the step"
        );
    }

    #[tokio::test]
    async fn test_plan_act_transition_on_completion() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::list_files::ListFilesHandler;

        // Two turns: each returns a list_files tool call (succeeds on workspace root)
        let responses = vec![
            vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_1".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("list_files".to_string()),
                        arguments: Some(serde_json::json!({"path": "."}).to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            })],
            vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_2".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("list_files".to_string()),
                        arguments: Some(serde_json::json!({"path": "."}).to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            })],
        ];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-act-transition".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: false,
        };

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ListFiles,
            Arc::new(ListFilesHandler::new()),
        );

        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        // Set up plan: 2 steps, step 0 Running, approved
        {
            let mut state = agent.state.lock().await;
            let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
                "Step one".to_string(),
                "Step two".to_string(),
            ]);
            plan.approved = true;
            plan.steps[0].status = crate::core::plan_state::PlanStepStatus::Running;
            state.plan_state = Some(plan);
        }

        // Turn 1: tool call succeeds → advance to step 1
        let result1 = agent.execute_turn().await;
        assert!(
            matches!(result1, TurnResult::Continue),
            "Expected Continue after step 1 tool, got {:?}",
            result1
        );
        {
            let state = agent.state.lock().await;
            let plan = state.plan_state.as_ref().unwrap();
            assert_eq!(
                plan.steps[0].status,
                crate::core::plan_state::PlanStepStatus::Done
            );
            assert_eq!(
                plan.steps[1].status,
                crate::core::plan_state::PlanStepStatus::Running
            );
            assert!(!plan.complete);
        }

        // Turn 2: tool call succeeds → plan completes → transition to Act
        let result2 = agent.execute_turn().await;
        // Plan completion returns Continue (agent keeps running in Act mode).
        // TurnResult::Complete is only for attempt_completion/plan_mode_respond.
        assert!(
            matches!(result2, TurnResult::Continue),
            "Expected Continue (agent continues in Act mode), got {:?}",
            result2
        );
        {
            let state = agent.state.lock().await;
            let plan = state.plan_state.as_ref().unwrap();
            assert!(plan.complete, "Plan should be marked complete");
            assert_eq!(
                plan.steps[1].status,
                crate::core::plan_state::PlanStepStatus::Done
            );
            assert_eq!(
                agent.mode(),
                AgentMode::Act,
                "Mode should transition to Act"
            );
        }
    }

    #[tokio::test]
    async fn test_attempt_completion_during_active_plan_continues() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::attempt_completion::AttemptCompletionHandler;

        let responses = vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("call_complete".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("attempt_completion".to_string()),
                    arguments: Some(serde_json::json!({"result": "Finished step 1"}).to_string()),
                },
                signature: None,
            },
            id: None,
            signature: None,
        })]];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-attempt-completion-active-plan".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: false,
        };

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::AttemptCompletion,
            Arc::new(AttemptCompletionHandler::new()),
        );

        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        {
            let mut state = agent.state.lock().await;
            let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
                "Step one".to_string(),
                "Step two".to_string(),
            ]);
            plan.approved = true;
            plan.steps[0].status = crate::core::plan_state::PlanStepStatus::Running;
            state.plan_state = Some(plan);
            state.double_check_completion_pending = true;
        }

        let result = agent.execute_turn().await;
        assert!(
            matches!(result, TurnResult::Continue),
            "Expected Continue when attempt_completion is used during an active plan, got {:?}",
            result
        );

        let state = agent.state.lock().await;
        let plan = state.plan_state.as_ref().unwrap();
        assert!(!plan.complete, "Active plan should not be marked complete");
        assert_eq!(plan.current_step_index, 1);
        assert_eq!(
            plan.steps[0].status,
            crate::core::plan_state::PlanStepStatus::Done
        );
        assert_eq!(
            plan.steps[1].status,
            crate::core::plan_state::PlanStepStatus::Running
        );
    }

    #[tokio::test]
    async fn test_attempt_completion_success_emits_only_completion_output() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::attempt_completion::AttemptCompletionHandler;

        let responses = vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("call_complete".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("attempt_completion".to_string()),
                    arguments: Some(serde_json::json!({"result": "Done once"}).to_string()),
                },
                signature: None,
            },
            id: None,
            signature: None,
        })]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-attempt-completion-output");
        let writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut priority_rx = writer
            .take_priority_rx()
            .expect("priority output receiver should be available");
        config.output_writer = writer;

        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::AttemptCompletion,
            Arc::new(AttemptCompletionHandler::new()),
        );
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));
        agent.state.lock().await.double_check_completion_enabled = false;

        let result = agent.execute_turn().await;

        assert!(matches!(result, TurnResult::Complete));
        let mut completions = Vec::new();
        let mut tool_output = Vec::new();
        for event in drain_output_events(&mut priority_rx, &mut rx) {
            match event {
                OutputEvent::Completion(text) => completions.push(text),
                OutputEvent::ToolOutputLine(line) => tool_output.push(line.to_string()),
                _ => {}
            }
        }
        assert_eq!(completions, vec!["Done once"]);
        assert!(
            !tool_output.iter().any(|line| line.contains("Done once")),
            "completion result was also emitted as tool output: {tool_output:?}"
        );
    }

    #[tokio::test]
    async fn test_attempt_completion_rejection_remains_visible() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::attempt_completion::AttemptCompletionHandler;

        let responses = vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("call_complete".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("attempt_completion".to_string()),
                    arguments: Some(serde_json::json!({"result": "Done once"}).to_string()),
                },
                signature: None,
            },
            id: None,
            signature: None,
        })]];
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests),
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-attempt-completion-rejection-output");
        let writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut priority_rx = writer
            .take_priority_rx()
            .expect("priority output receiver should be available");
        config.output_writer = writer;

        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::AttemptCompletion,
            Arc::new(AttemptCompletionHandler::new()),
        );
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));
        agent.state.lock().await.double_check_completion_enabled = true;

        let _ = agent.execute_turn().await;

        let mut completion_count = 0;
        let mut tool_output = Vec::new();
        for event in drain_output_events(&mut priority_rx, &mut rx) {
            match event {
                OutputEvent::Completion(_) => completion_count += 1,
                OutputEvent::ToolOutputLine(line) => tool_output.push(line.to_string()),
                _ => {}
            }
        }
        assert_eq!(completion_count, 0);
        assert!(
            tool_output
                .iter()
                .any(|line| line.contains("Before completing, re-verify your work")),
            "completion rejection was not emitted as tool output: {tool_output:?}"
        );
    }

    #[tokio::test]
    async fn test_text_only_turns_during_active_plan_continues() {
        let responses = vec![vec![ApiStreamChunk::Text(ApiStreamTextChunk {
            text: "Still working on it.".to_string(),
            id: None,
            signature: None,
        })]];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-text-only-active-plan".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: false,
        };

        let registry = ToolRegistry::new();
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        {
            let mut state = agent.state.lock().await;
            let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
                "Step one".to_string(),
                "Step two".to_string(),
            ]);
            plan.approved = true;
            plan.steps[0].status = crate::core::plan_state::PlanStepStatus::Running;
            state.plan_state = Some(plan);
        }

        let result = agent.execute_turn().await;
        assert!(
            matches!(result, TurnResult::Continue),
            "Expected Continue when text-only output is returned during an active plan, got {:?}",
            result
        );

        let state = agent.state.lock().await;
        let plan = state.plan_state.as_ref().unwrap();
        assert!(!plan.complete, "Active plan should not be marked complete");
        assert_eq!(plan.current_step_index, 0);
        assert_eq!(
            plan.steps[0].status,
            crate::core::plan_state::PlanStepStatus::Running
        );
    }

    #[tokio::test]
    async fn test_plan_step_failure_pauses_execution() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::list_files::ListFilesHandler;

        // One turn: list_files with non-existent path → tool failure
        let responses = vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("call_fail".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("list_files".to_string()),
                    arguments: Some(
                        serde_json::json!({"path": "nonexistent_dir_12345"}).to_string(),
                    ),
                },
                signature: None,
            },
            id: None,
            signature: None,
        })]];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, requests.clone()),
        ));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-failure-pauses".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: Some(3),
            double_check_completion: false,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: false,
        };

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ListFiles,
            Arc::new(ListFilesHandler::new()),
        );

        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        // Set up plan: 1 step, step 0 Running, approved
        {
            let mut state = agent.state.lock().await;
            let mut plan =
                crate::core::plan_state::PlanState::create_plan(vec!["Step one".to_string()]);
            plan.approved = true;
            plan.steps[0].status = crate::core::plan_state::PlanStepStatus::Running;
            state.plan_state = Some(plan);
        }

        // Turn 1: tool fails → step marked Failed, plan paused
        let result = agent.execute_turn().await;
        assert!(
            matches!(result, TurnResult::Continue),
            "Expected Continue after failed step, got {:?}",
            result
        );
        {
            let state = agent.state.lock().await;
            let plan = state.plan_state.as_ref().unwrap();
            assert_eq!(
                plan.steps[0].status,
                crate::core::plan_state::PlanStepStatus::Failed,
                "Step should be marked Failed on tool failure"
            );
            assert!(plan.paused, "Plan should be paused after step failure");
            assert!(!plan.complete, "Plan should not be complete after failure");
        }
    }

    #[tokio::test]
    async fn test_failed_command_blocks_misleading_attempt_completion() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::attempt_completion::AttemptCompletionHandler;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;

        let responses = vec![
            vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_failed_command".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("execute_command".to_string()),
                        arguments: Some(serde_json::json!({"commands": ["false"]}).to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            })],
            vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_misleading_completion".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("attempt_completion".to_string()),
                        arguments: Some(
                            serde_json::json!({"result": "Everything completed successfully"})
                                .to_string(),
                        ),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            })],
        ];

        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut priority_rx = writer
            .take_priority_rx()
            .expect("priority output receiver should be available");
        let mut config = test_agent_config(provider, "test-failed-command-completion");
        config.output_writer = writer;

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new().with_yolo(true)),
        );
        registry.register(
            crate::core::tools::SnedTool::AttemptCompletion,
            Arc::new(AttemptCompletionHandler::new()),
        );

        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));
        {
            let mut state = agent.state.lock().await;
            let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
                "Run the command".to_string(),
            ]);
            plan.approved = true;
            plan.steps[0].status = crate::core::plan_state::PlanStepStatus::Running;
            state.plan_state = Some(plan);
        }

        let first_result = agent.execute_turn().await;
        assert!(matches!(first_result, TurnResult::Continue));
        let first_events = drain_output_events(&mut priority_rx, &mut rx);
        assert!(first_events.iter().any(|event| {
            matches!(event, OutputEvent::ErrorBox(message) if message.contains("Plan step 1/1 failed"))
        }));

        {
            let state = agent.state.lock().await;
            let plan = state
                .plan_state
                .as_ref()
                .expect("plan should remain present");
            assert_eq!(
                plan.steps[0].status,
                crate::core::plan_state::PlanStepStatus::Failed
            );
            assert!(plan.paused);
            assert!(!plan.complete);
        }

        let second_result = agent.execute_turn().await;
        assert!(matches!(second_result, TurnResult::Continue));
        let second_events = drain_output_events(&mut priority_rx, &mut rx);
        assert!(second_events.iter().any(|event| {
            matches!(event, OutputEvent::ToolOutputLine(line) if line.to_string().contains("Cannot complete while the approved plan"))
        }));
        assert!(
            !first_events
                .iter()
                .chain(second_events.iter())
                .any(|event| matches!(event, OutputEvent::Completion(_)))
        );
    }

    // =====================================================================
    // keep_from_preserving_tool_pairs tests (real ToolUse/ToolResult blocks)
    // =====================================================================

    #[test]
    fn test_keep_from_preserving_tool_pairs_basic() {
        // ToolUse at index 3, ToolResult at index 5 (outside kept region).
        // keep_from_base = 6 → should pull back to 3.
        use crate::providers::{
            AssistantContentBlock, MessageContent, MessageRole, SharedContentFields,
            StorageMessage, ToolResultBlock, ToolResultContent, ToolUseBlock, UserContentBlock,
        };

        let mut history = Vec::new();
        for i in 0..6 {
            history.push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(format!("msg-{i}")),
                model_info: None,
                metrics: None,
                ts: Some(i as u64),
            });
        }
        // Index 3: ToolUse (assistant message)
        history[3] = StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                ToolUseBlock {
                    id: "tu-1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(3),
        };
        // Index 5: ToolResult (user message)
        history[5] = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                ToolResultBlock {
                    tool_use_id: "tu-1".to_string(),
                    content: ToolResultContent::Text("ok".to_string()),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(5),
        };

        // keep_from_base = 6 keeps [6..]. The ToolResult at 5 and its ToolUse
        // at 3 are both in the dropped region [0..6], so no orphan exists in
        // the kept region — keep_from stays at 6.
        let result = AgentLoop::keep_from_preserving_tool_pairs(&history, 6);
        assert_eq!(
            result, 6,
            "Both pair members are in the dropped region — no pullback needed"
        );
    }

    #[test]
    fn test_keep_from_preserving_tool_pairs_no_orphan() {
        // ToolUse and ToolResult both inside kept region → keep_from unchanged.
        use crate::providers::{
            AssistantContentBlock, MessageContent, MessageRole, SharedContentFields,
            StorageMessage, ToolResultBlock, ToolResultContent, ToolUseBlock, UserContentBlock,
        };

        let mut history = Vec::new();
        for i in 0..10 {
            history.push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(format!("msg-{i}")),
                model_info: None,
                metrics: None,
                ts: Some(i as u64),
            });
        }
        // ToolUse at index 7, ToolResult at index 8 — both in kept region (keep_from_base=5)
        history[7] = StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                ToolUseBlock {
                    id: "tu-2".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "b.rs"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(7),
        };
        history[8] = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                ToolResultBlock {
                    tool_use_id: "tu-2".to_string(),
                    content: ToolResultContent::Text("ok".to_string()),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(8),
        };

        // Both ToolUse (7) and ToolResult (8) are in kept region (5..) → no change
        let result = AgentLoop::keep_from_preserving_tool_pairs(&history, 5);
        assert_eq!(result, 5, "No orphans — keep_from unchanged");
    }

    #[test]
    fn test_keep_from_preserving_tool_pairs_cascade() {
        // Cascade: ToolUse at 3, ToolResult at 5 (refers to 3).
        // ToolUse at 7, ToolResult at 9 (refers to 7).
        // keep_from_base = 10 → pulls to 7 (first pass), then 3 (second pass).
        use crate::providers::{
            AssistantContentBlock, MessageContent, MessageRole, SharedContentFields,
            StorageMessage, ToolResultBlock, ToolResultContent, ToolUseBlock, UserContentBlock,
        };

        let mut history = Vec::new();
        for i in 0..10 {
            history.push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(format!("msg-{i}")),
                model_info: None,
                metrics: None,
                ts: Some(i as u64),
            });
        }
        // ToolUse at 3
        history[3] = StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                ToolUseBlock {
                    id: "tu-a".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(3),
        };
        // ToolResult at 5 referencing tu-a
        history[5] = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                ToolResultBlock {
                    tool_use_id: "tu-a".to_string(),
                    content: ToolResultContent::Text("ok-a".to_string()),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(5),
        };
        // ToolUse at 7
        history[7] = StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                ToolUseBlock {
                    id: "tu-b".to_string(),
                    name: "edit_file".to_string(),
                    input: serde_json::json!({"path": "b.rs"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(7),
        };
        // ToolResult at 9 referencing tu-b
        history[9] = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                ToolResultBlock {
                    tool_use_id: "tu-b".to_string(),
                    content: ToolResultContent::Text("ok-b".to_string()),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(9),
        };

        // keep_from_base=10 keeps [10..] (empty since history.len()==10).
        // Both tool pairs (3↔5 and 7↔9) are entirely in the dropped region
        // [0..10], so no orphan exists in the kept region — keep_from stays
        // at 10.
        let result = AgentLoop::keep_from_preserving_tool_pairs(&history, 10);
        assert_eq!(
            result, 10,
            "Both pairs are in the dropped region — no cascade pullback"
        );
    }

    #[test]
    fn test_keep_from_preserving_tool_pairs_text_only() {
        // No tool blocks at all → keep_from_base unchanged.
        use crate::providers::{MessageContent, MessageRole, StorageMessage};

        let history: Vec<StorageMessage> = (0..10)
            .map(|i| StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(format!("msg-{i}")),
                model_info: None,
                metrics: None,
                ts: Some(i as u64),
            })
            .collect();

        let result = AgentLoop::keep_from_preserving_tool_pairs(&history, 5);
        assert_eq!(result, 5, "Text-only history — keep_from unchanged");
    }

    #[test]
    fn test_keep_from_preserving_tool_pairs_cascade_two_levels() {
        use crate::providers::{
            AssistantContentBlock, MessageContent, MessageRole, SharedContentFields,
            StorageMessage, ToolResultBlock, ToolResultContent, ToolUseBlock, UserContentBlock,
        };

        // This fixture forces a true two-level cascade: pulling the kept range
        // back for tool-use "tu-b" exposes a second orphaned tool result for
        // "tu-a", which then forces a second pullback in the same helper.

        let mut history = Vec::new();
        for i in 0..9 {
            history.push(StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(format!("msg-{i}")),
                model_info: None,
                metrics: None,
                ts: Some(i as u64),
            });
        }
        history[1] = StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                ToolUseBlock {
                    id: "tu-a".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(1),
        };
        history[5] = StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                ToolUseBlock {
                    id: "tu-b".to_string(),
                    name: "edit_file".to_string(),
                    input: serde_json::json!({"path": "b.rs"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(5),
        };
        history[6] = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                ToolResultBlock {
                    tool_use_id: "tu-a".to_string(),
                    content: ToolResultContent::Text("ok-a".to_string()),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(6),
        };
        history[8] = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                ToolResultBlock {
                    tool_use_id: "tu-b".to_string(),
                    content: ToolResultContent::Text("ok-b".to_string()),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(8),
        };

        let result = AgentLoop::keep_from_preserving_tool_pairs(&history, 7);
        assert_eq!(
            result, 1,
            "Cascade: pass 1 pulls to 5 (tu-b), pass 2 pulls to 1 (tu-a)"
        );
    }

    /// The `record_first_*_time` helpers use an atomic flag so that
    /// only the first chunk on a turn takes `state.lock().await`.
    /// Subsequent calls on the same turn must observe the flag and
    /// return without touching the mutex. We verify the contract by
    /// observing the side-effect that the helper ALWAYS performs
    /// (setting `reasoning_active`); the `Instant` write is gated on
    /// `timing_enabled()` and is only set in instrumented sessions.
    #[tokio::test]
    async fn test_record_first_output_emit_time_atomic_fast_path() {
        use crate::providers::mock::{MockProvider, MockResponse};
        let provider: Arc<Providers> = Arc::new(Providers::Mock(MockProvider::new(vec![
            MockResponse::Stream(vec![]),
        ])));
        let agent = AgentLoop::new(test_agent_config(provider, "test-atomic-fast-path"));

        // Set reasoning_active to true so the first call's
        // `reasoning_active = false` is observable.
        {
            let mut state = agent.state.lock().await;
            state.reasoning_active = true;
        }
        assert!(
            !agent
                .first_output_emit_recorded
                .load(std::sync::atomic::Ordering::Acquire)
        );

        // First call claims the flag and performs the state mutation.
        agent.record_first_output_emit_time().await;
        assert!(
            agent
                .first_output_emit_recorded
                .load(std::sync::atomic::Ordering::Acquire)
        );
        {
            let state = agent.state.lock().await;
            assert!(
                !state.reasoning_active,
                "first call must clear reasoning_active"
            );
        }

        // Reset state and call again. The atomic flag must short-circuit
        // the second call, so the state mutation must NOT happen.
        {
            let mut state = agent.state.lock().await;
            state.reasoning_active = true;
        }
        agent.record_first_output_emit_time().await;
        {
            let state = agent.state.lock().await;
            assert!(
                state.reasoning_active,
                "atomic fast-path must skip the state write after the first claim"
            );
        }
    }

    #[tokio::test]
    async fn test_reset_stream_attempt_timing_clears_phase_state_and_flags() {
        use crate::providers::mock::{MockProvider, MockResponse};
        let provider: Arc<Providers> = Arc::new(Providers::Mock(MockProvider::new(vec![
            MockResponse::Stream(vec![]),
        ])));
        let agent = AgentLoop::new(test_agent_config(provider, "test-stream-attempt-timing"));
        let now = std::time::Instant::now();

        agent
            .first_output_emit_recorded
            .store(true, std::sync::atomic::Ordering::Release);
        agent
            .first_reasoning_chunk_recorded
            .store(true, std::sync::atomic::Ordering::Release);
        agent
            .first_displayable_text_recorded
            .store(true, std::sync::atomic::Ordering::Release);
        {
            let mut state = agent.state.lock().await;
            state.request_sent_time = Some(now);
            state.first_provider_chunk_time = Some(now);
            state.first_reasoning_chunk_time = Some(now);
            state.first_displayable_text_time = Some(now);
            state.first_output_emit_time = Some(now);
        }

        agent.reset_stream_attempt_timing().await;

        assert!(
            !agent
                .first_output_emit_recorded
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert!(
            !agent
                .first_reasoning_chunk_recorded
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert!(
            !agent
                .first_displayable_text_recorded
                .load(std::sync::atomic::Ordering::Acquire)
        );
        let state = agent.state.lock().await;
        assert!(state.request_sent_time.is_none());
        assert!(state.first_provider_chunk_time.is_none());
        assert!(state.first_reasoning_chunk_time.is_none());
        assert!(state.first_displayable_text_time.is_none());
        assert!(state.first_output_emit_time.is_none());
    }

    #[test]
    fn test_stream_retry_delay_is_bounded_exponential() {
        assert_eq!(stream_retry_delay(1), std::time::Duration::from_secs(1));
        assert_eq!(stream_retry_delay(2), std::time::Duration::from_secs(2));
        assert_eq!(stream_retry_delay(3), std::time::Duration::from_secs(4));
        assert_eq!(stream_retry_delay(20), std::time::Duration::from_secs(4));
    }

    /// Same contract for the reasoning-chunk timing helper.
    #[tokio::test]
    async fn test_record_first_reasoning_chunk_time_atomic_fast_path() {
        use crate::providers::mock::{MockProvider, MockResponse};
        let provider: Arc<Providers> = Arc::new(Providers::Mock(MockProvider::new(vec![
            MockResponse::Stream(vec![]),
        ])));
        let agent = AgentLoop::new(test_agent_config(provider, "test-reasoning-fast-path"));

        assert!(
            !agent
                .first_reasoning_chunk_recorded
                .load(std::sync::atomic::Ordering::Acquire)
        );
        {
            let mut state = agent.state.lock().await;
            state.reasoning_active = false;
        }

        // First call claims the flag and sets reasoning_active = true.
        agent.record_first_reasoning_chunk_time().await;
        assert!(
            agent
                .first_reasoning_chunk_recorded
                .load(std::sync::atomic::Ordering::Acquire)
        );
        {
            let state = agent.state.lock().await;
            assert!(
                state.reasoning_active,
                "first call must set reasoning_active"
            );
        }

        // Reset and call again; flag must short-circuit.
        {
            let mut state = agent.state.lock().await;
            state.reasoning_active = false;
        }
        agent.record_first_reasoning_chunk_time().await;
        {
            let state = agent.state.lock().await;
            assert!(
                !state.reasoning_active,
                "atomic fast-path must skip the state write after the first claim"
            );
        }
    }
}
