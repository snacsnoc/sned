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
use crate::cli::tui::theme::{error_fg, prompt_fg};
use crate::core::agent_stream::{
    StreamAccumulator, StreamEvent, StreamOutcome, StreamProviderInfo,
};
use crate::core::agent_types::code_block_display_limit;
pub use crate::core::agent_types::{AgentConfig, AgentError, AgentMode, TaskState, TurnResult};
use crate::core::context::{
    PromptBuilder, SystemPromptContext, context_manager, context_window,
};
use crate::core::file_editor::AnchorStateManager;
use crate::core::provider_retry::{
    DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES, RetryConfig, create_message_with_retry,
};
use crate::core::tools::SnedTool;
use crate::core::tools::{
    ToolContext, ToolFailureClass, ToolFailureMetadata, ToolPublicationOutcome, ToolRegistry,
    ToolRequiredNextStep, coerce_command_array, coerce_string_array, tool_result_to_text,
};
use crate::providers::{
    ApiStreamChunk, ApiStreamToolCall, AssistantContentBlock, MessageContent, MessageRole,
    Provider, ProviderRequest, RedactedThinkingBlock, SharedContentFields, StorageMessage,
    TextContentBlock, ThinkingBlock, ToolResultContent, ToolResultContentBlock, ToolUseBlock,
    UserContentBlock,
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
use crate::core::stream_parsing::{extract_response_text, split_model_output};
use crate::core::tool_output::{
    extract_edit_stats_detailed, format_heat_map, format_heat_map_plain, format_tool_call_lines,
    format_tool_call_lines_with_raw_arguments, format_tool_result, format_tool_result_digest,
    path_from_read_file_header, strip_tool_result_anchors, summarize_matching_sections,
};

const MAX_EDIT_RESULT_DISPLAY_LINES: usize = 10;
/// Cap for full failed-tool params/results in the debug log, so a
/// post-mortem stays possible without one call flooding the file.
const DEBUG_ERROR_CONTEXT_CAP: usize = 64 * 1024;

fn truncated_debug_text(text: &str) -> String {
    let end = text.floor_char_boundary(text.len().min(DEBUG_ERROR_CONTEXT_CAP));
    if end < text.len() {
        format!(
            "{}...[{} more chars truncated]",
            &text[..end],
            text.len() - end
        )
    } else {
        text.to_string()
    }
}
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
    publication_outcomes: Vec<ToolPublicationOutcome>,
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
            publication_outcomes: Vec::new(),
        }
    }

    fn success_with_hook_context(text: String, hook_context: Vec<String>) -> Self {
        Self {
            text,
            metadata: None,
            is_error: false,
            hook_context,
            publication_outcomes: Vec::new(),
        }
    }

    fn error_with_hook_context(
        text: String,
        metadata: Option<ToolFailureMetadata>,
        hook_context: Vec<String>,
        publication_outcomes: Vec<ToolPublicationOutcome>,
    ) -> Self {
        Self {
            text,
            metadata,
            is_error: true,
            hook_context,
            publication_outcomes,
        }
    }

    fn cancelled_before_start() -> Self {
        Self::error(
            "Tool execution skipped: task was cancelled before the tool started".to_string(),
            None,
        )
    }
}

/// Resolve a prepared but unstarted tool future. Cancellation observed here
/// must not begin new work; an already-started future runs to completion so
/// a file transaction is never dropped mid-application.
async fn run_tool_unless_cancelled(
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    task: futures::future::BoxFuture<'static, ToolExecutionOutput>,
) -> ToolExecutionOutput {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        ToolExecutionOutput::cancelled_before_start()
    } else {
        task.await
    }
}

fn append_tool_result_blocks(
    blocks: &mut Vec<UserContentBlock>,
    tool_id: String,
    result_output: ToolExecutionOutput,
) {
    // Keep hook text beside its own result so per-tool context cannot be
    // mistaken for instructions belonging to a later parallel tool.
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
pub(crate) fn tool_result_history_limit() -> usize {
    std::env::var(TOOL_RESULT_HISTORY_LIMIT_ENV)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOOL_RESULT_HISTORY_LIMIT)
}

pub(crate) fn truncate_tool_result(result: &str) -> String {
    let limit = tool_result_history_limit();

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
                    span.style.fg = Some(crate::cli::tui::theme::accent());
                }
            }
            return line;
        }
    }

    // Block constructs need full-turn context and remain raw until TurnEnd.
    Line::from(Span::styled(
        text,
        Style::default().fg(crate::cli::tui::theme::accent()),
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
        Style::default().fg(error_fg()),
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
    #[must_use]
    pub fn try_queued_message_count(&self) -> Option<usize> {
        self.queue.try_lock().ok().map(|q| q.len())
    }

    /// Synchronously read the queue count and text previews for the TUI.
    #[must_use]
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
    fn new_history_was_discarded(
        previous_deleted_range: Option<(usize, usize)>,
        current_deleted_range: Option<(usize, usize)>,
        original_len: usize,
        current_len: usize,
    ) -> bool {
        previous_deleted_range != current_deleted_range
            || (previous_deleted_range.is_none() && current_len < original_len)
    }

    fn clear_history_dependent_read_state(state: &mut TaskState) {
        state.consecutive_reads.clear();
        state.last_read_turn.clear();
        state.recent_read_windows.clear();
    }

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
        // Oversized arguments are never executable, no matter how valid the
        // JSON looks after repair. Fail the call; the stream assembly below
        // keeps draining so the protocol stays in sync.
        if raw.len() > MAX_TOOL_ARGUMENT_SIZE {
            let preview: String = raw.chars().take(200).collect();
            tracing::error!(
                tool_name = %tool_name,
                tool_id = %tool_id,
                args_len = raw.len(),
                args_preview = %preview,
                "tool call arguments exceed size limit; call will not execute"
            );
            return Err(format!(
                "Tool '{tool_name}' arguments exceed the {MAX_TOOL_ARGUMENT_SIZE}-byte limit ({} bytes, id: {tool_id}) and were not executed. Please retry with smaller arguments. Rejected argument prefix: {preview}",
                raw.len()
            ));
        }
        // Treat empty string as no arguments (some providers send empty string instead of "{}")
        if raw.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(mut parsed) => {
                if let Some(parse_error) = crate::providers::tool_arguments_error(&parsed) {
                    return Err(format!(
                        "Tool '{tool_name}' arguments could not be repaired as JSON (id: {tool_id}): {parse_error} Please retry the same tool call with valid JSON arguments."
                    ));
                }
                let normalized = match tool_name {
                    "execute_command" => {
                        if let Some(raw) = parsed
                            .get("commands")
                            .and_then(serde_json::Value::as_str)
                            .filter(|raw| raw.trim().starts_with("[\""))
                        {
                            let commands = crate::core::tools::parse_unambiguous_stringified_string_array(raw)
                                .or_else(|| crate::core::tools::parse_relaxed_stringified_string_array(raw))
                                .ok_or_else(|| format!(
                                    "Tool '{tool_name}' arguments contain an ambiguous stringified 'commands' array (id: {tool_id}). Re-issue the tool call with a literal JSON array of command strings."
                                ))?;
                            parsed["commands"] = serde_json::json!(commands);
                            true
                        } else {
                            false
                        }
                    }
                    "edit_file" => crate::core::tools::handlers::edit_file::EditFileHandler::normalize_stringified_files_param(&mut parsed)
                        .map_err(|error| format!("Tool '{tool_name}' arguments could not be normalized (id: {tool_id}): {error}"))?,
                    _ => false,
                };
                if normalized {
                    tracing::debug!(tool_name = %tool_name, tool_id = %tool_id, "normalized compatibility tool arguments");
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
            Err(_) => {
                let raw = prepared
                    .tool_call
                    .function
                    .arguments
                    .as_deref()
                    .unwrap_or("");
                // Oversized payloads keep only a prefix in history; the full
                // bytes already went to the error above.
                let raw = if raw.len() > MAX_TOOL_ARGUMENT_SIZE {
                    let preview: String = raw.chars().take(200).collect();
                    format!(
                        "{preview}…[truncated: showing {} of {} bytes]",
                        preview.len(),
                        raw.len()
                    )
                } else {
                    raw.to_string()
                };
                serde_json::json!({
                    "_raw_arguments": raw
                })
            }
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
        state.provider_stream_completed_time = None;
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

    async fn emit_turn_end(&self, markdown_text: &str) {
        if self.config.json_output {
            return;
        }

        let accumulated_text = crate::core::stream_parsing::strip_tool_call_lines(markdown_text);
        if accumulated_text.is_empty() {
            return;
        }

        let timing = self.capture_turn_end_timing().await;
        self.config.output_writer.emit(OutputEvent::TurnEnd {
            accumulated_text,
            timing,
        });
    }

    async fn capture_turn_end_timing(&self) -> Option<crate::cli::output::TurnEndTiming> {
        if !crate::cli::output::timing_enabled() {
            return None;
        }
        let emitted_at = std::time::Instant::now();
        let state = self.state.lock().await;
        Some(crate::cli::output::TurnEndTiming {
            provider_completed_at: state.provider_stream_completed_time,
            first_output_at: state.first_output_emit_time,
            emitted_at,
        })
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
        let strict_plan_mode_enabled = config.strict_plan_mode_enabled;
        let state = TaskState {
            is_subagent_execution: is_subagent,
            strict_plan_mode_enabled,
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

    /// Snapshot the agent config so a fresh task can inherit it.
    pub fn config_snapshot(&self) -> crate::core::agent_types::AgentConfig {
        self.config.clone()
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

    async fn invalidate_changed_read_state(state: &Arc<Mutex<TaskState>>) {
        let snapshots = {
            let guard = state.lock().await;
            guard.read_file_snapshots.clone()
        };
        if snapshots.is_empty() {
            return;
        }

        let mut changed_paths = Vec::new();
        for (path, snapshot) in snapshots {
            let current = tokio::fs::metadata(&path).await.ok().map(|metadata| {
                (metadata.len(), metadata.modified().ok())
            });
            if current.as_ref() != Some(&snapshot) {
                changed_paths.push(path);
            }
        }
        if changed_paths.is_empty() {
            return;
        }

        let mut guard = state.lock().await;
        for path in changed_paths {
            guard.consecutive_reads.remove(&path);
            guard.last_read_turn.remove(&path);
            guard.recent_read_windows.remove(&path);
            guard.read_file_snapshots.remove(&path);
        }
    }

    /// Tools whose execution keeps the read-loop state alive. The model
    /// alternates between slicing a file and probing its surroundings
    /// (compile errors, shell greps, file listings, symbol lookups) — that
    /// is the SAME investigation phase, not a fresh task. Wiping on every
    /// non-read_file tool was the root cause of the live-log failure mode
    /// where the model did 9 narrow slices of `MetalWaterfallView.swift`
    /// (with intervening execute_command Python brace-depth scripts and
    /// `xcodebuild` greps) and the read-loop circuit breaker never fired,
    /// because each non-read tool reset `consecutive_reads` back to 1.
    /// Inspection tools include anything that reads from disk or the
    /// workspace and does not mutate state.
    const READ_LOOP_INSPECTION_TOOLS: &'static [&'static str] = &[
        "read_file",
        "search_files",
        "list_files",
        "get_function",
        "get_file_skeleton",
        "find_symbol_references",
        "diagnostics_scan",
        "execute_command",
        "web_fetch",
        "condense",
    ];

    /// Mutating tools whose own Phase 4b commit handles per-file cleanup
    /// on success. Wiping the read-loop counters here is the right
    /// semantic: a successful write resets the file's content and the
    /// read-tracking for it must start fresh.
    ///
    /// The wipe runs unconditionally for these tools; success/failure
    /// does not gate it because both states reset the model's mental
    /// model of the file — success because the bytes changed, failure
    /// because the model must plan a new approach.
    const READ_LOOP_MUTATING_TOOLS: &'static [&'static str] = &[
        "edit_file",
        "write_to_file",
        "replace_symbol",
        "rename_symbol",
    ];

    /// Read-loop decay applied between tool calls. Inspection tools
    /// keep state alive (the model is still investigating). Mutating
    /// tools reset the warning counters; their handlers invalidate only
    /// affected file coverage. Unknown tools apply a 2-turn cooldown via
    /// `last_read_turn` (which is updated on every read with the
    /// current `turns_completed`) so the detector eventually forgets
    /// files the model abandoned. The `READ_LOOP_INSPECTION_TOOLS`
    /// allowlist preserves state across read-only shell commands; mutating
    /// shell commands are invalidated after execution separately.
    /// Whether a read-warning key tracks a path a mutation changed.
    /// Handler keys may be canonicalized while params stay relative, so
    /// exact matches and relative/absolute suffix pairs both invalidate.
    fn read_state_key_matches_tracked(key: &str, affected: &str) -> bool {
        if key == affected {
            return true;
        }
        let key = key.replace('\\', "/");
        let affected = affected.replace('\\', "/");
        if key == affected {
            return true;
        }
        key.ends_with(&format!("/{affected}")) || affected.ends_with(&format!("/{key}"))
    }

    /// Invalidate read-warning state for paths a mutation actually changed.
    /// Runs only after a mutating tool executes successfully; denials,
    /// parse failures and deferrals never reach this point, so warning
    /// history for untouched files survives them.
    async fn invalidate_read_state_for_mutation(
        state: &Arc<Mutex<TaskState>>,
        tool: SnedTool,
        params: &serde_json::Value,
    ) {
        let affected = Self::extract_action_path(tool, params);
        let mut guard = state.lock().await;
        if affected.is_empty() {
            guard.consecutive_reads.clear();
            guard.last_read_turn.clear();
            guard.recent_read_windows.clear();
            return;
        }
        guard.consecutive_reads.retain(|key, _| {
            !affected
                .iter()
                .any(|path| Self::read_state_key_matches_tracked(key, path))
        });
        guard.last_read_turn.retain(|key, _| {
            !affected
                .iter()
                .any(|path| Self::read_state_key_matches_tracked(key, path))
        });
        guard.recent_read_windows.retain(|key, _| {
            !affected
                .iter()
                .any(|path| Self::read_state_key_matches_tracked(key, path))
        });
    }

    fn decay_read_loop_state(state: &mut TaskState, tool_name: &str) {
        if Self::READ_LOOP_INSPECTION_TOOLS.contains(&tool_name) {
            return;
        }
        if Self::READ_LOOP_MUTATING_TOOLS.contains(&tool_name) {
            state.consecutive_reads.clear();
            state.last_read_turn.clear();
            state.recent_read_windows.clear();
            return;
        }
        // Unknown tool: apply a 2-turn cooldown independently per path. A
        // read of an active file must not keep an abandoned file's warning
        // counter alive indefinitely.
        let current_turn = state.turns_completed;
        let stale_paths: Vec<String> = state
            .last_read_turn
            .iter()
            .filter(|(_, last)| current_turn.saturating_sub(**last) > 1)
            .map(|(path, _)| path.clone())
            .collect();
        for path in stale_paths {
            state.consecutive_reads.remove(&path);
            state.last_read_turn.remove(&path);
            state.recent_read_windows.remove(&path);
        }
    }

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
            state.strict_plan_mode_enabled = self.config.strict_plan_mode_enabled;
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
        if let Some(ref tracker) = self.env_tracker
            && let Err(e) = tracker.record_environment()
        {
            warn!(error = %e, "Failed to record environment snapshot");
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
                // A cancelled run must reach the normal cancellation handling
                // below even while the plan stays paused.
                let pause_can_wait = plan_is_paused && !state.is_cancelled;
                if pause_can_wait {
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
                                "Processing queued message ({queue_remaining} more queued)"
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
                                        "Processing queued message ({queue_remaining} more queued)"
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

    /// Measurement stays a loop method because it reads turn state and
    /// resolves the model id.
    async fn record_stream_timing(
        &self,
        chunk: &ApiStreamChunk,
        provider: &std::sync::Arc<Providers>,
        decoded_chunks: u64,
        text_chunks: u64,
        reasoning_chunks: u64,
        max_gap: std::time::Duration,
        attempt: usize,
    ) {
        let ApiStreamChunk::Timing(provider_timing) = chunk else {
            return;
        };
        if !crate::cli::output::timing_enabled() {
            return;
        }
        let mut state = self.state.lock().await;
        state.provider_stream_completed_time = provider_timing
            .completed_at
            .or_else(|| Some(std::time::Instant::now()));
        let request_to_first_chunk_us = state.request_sent_time.and_then(|request| {
            state
                .first_provider_chunk_time
                .map(|chunk| chunk.duration_since(request).as_micros() as u64)
        });
        let first_chunk_to_displayable_text_us =
            state.first_provider_chunk_time.and_then(|chunk| {
                state.first_displayable_text_time.map(|displayable| {
                    displayable.duration_since(chunk).as_micros() as u64
                })
            });
        let displayable_text_to_output_us =
            state.first_displayable_text_time.and_then(|displayable| {
                state
                    .first_output_emit_time
                    .map(|output| output.duration_since(displayable).as_micros() as u64)
            });
        crate::cli::output::emit_timing_record(&crate::cli::output::ProviderTimingRecord {
            record_type: "sned_timing_provider_attempt",
            run_id: crate::cli::output::timing_run_id(),
            session_id: self.config.task_id.clone(),
            turn: state.turns_completed.saturating_add(1),
            attempt: attempt + 1,
            provider: provider.name().to_string(),
            model: self.resolve_active_model_id(),
            stream: true,
            request_to_headers_us: provider_timing.request_to_headers_us,
            headers_to_first_byte_us: provider_timing.headers_to_first_byte_us,
            request_to_first_chunk_us,
            first_chunk_to_displayable_text_us,
            displayable_text_to_output_us,
            stream_total_us: provider_timing.stream_total_us,
            raw_sse_frames: provider_timing.raw_sse_frames,
            decoded_chunks,
            text_chunks,
            reasoning_chunks,
            empty_sse_frames: provider_timing.empty_sse_frames,
            max_inter_raw_byte_gap_us: provider_timing.max_inter_raw_byte_gap_us,
            max_inter_decoded_chunk_gap_us: max_gap.as_micros() as u64,
        });
    }

    /// Executes a single turn of the agent loop.
    async fn execute_turn(&mut self) -> TurnResult {
        // Reset per-turn counters so cumulative token totals stay correct but
        // per-turn deltas (like turn_tool_calls) restart from zero each turn.
        {
            let mut state = self.state.lock().await;
            state.turn_tool_calls = 0;
        }

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
            let mut conversation_guard = self.conversation_history.lock().await;
            compact_old_tool_results(&mut conversation_guard);
            let original_history_len = conversation_guard.len();
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

            // Update state if deleted range changed, then persist the
            // range without holding the state guard across disk IO.
            if result.updated_conversation_history_deleted_range {
                let (deleted_range, history_item) = {
                    let mut state = self.state.lock().await;
                    let deleted_range = result.conversation_history_deleted_range;
                    state.conversation_history_deleted_range = deleted_range;
                    let history_item = deleted_range.and_then(|_| {
                        self.state_manager.as_ref().and_then(|state_manager| {
                            state_manager.find_task_in_history(&self.config.task_id)
                        })
                    });
                    (deleted_range, history_item)
                };

                // Persist deleted_range to HistoryItem for cross-session restoration (C1 fix part 1)
                // Convert from (usize, usize) tuple to Vec<i32> for HistoryItem storage
                if let Some((start, end)) = deleted_range
                    && let Some(mut history_item) = history_item
                    && let Some(ref state_manager) = self.state_manager
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

            let history_reduced = Self::new_history_was_discarded(
                deleted_range,
                result.conversation_history_deleted_range,
                original_history_len,
                result.truncated_conversation_history.len(),
            );
            (result.truncated_conversation_history, history_reduced)
        };

        // 2. Apply context pruning if enabled
        let (truncated_history, context_reduced) = truncated_history;
        let truncated_len = truncated_history.len();
        let pruned_history = self.prune_conversation_history(truncated_history);
        if self.current_turn_retry_candidate.is_none() {
            self.current_turn_retry_candidate = Self::current_turn_retry_candidate(&pruned_history);
        }
        {
            let mut state = self.state.lock().await;
            state.retryable_failed_request = None;
            if context_reduced || pruned_history.len() < truncated_len {
                Self::clear_history_dependent_read_state(&mut state);
            }
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
                self.deps.yolo,
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
            filtered_text,
            leaked_thinking,
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

            // 4. Process stream chunks. Interpretation lives in the
            // accumulator; only presentation, measurement, timing, and
            // cancellation state remain here.
            let stream_provider_info = {
                let guard = self.config.provider.lock().expect("provider poisoned");
                StreamProviderInfo {
                    provider_name: guard.name().to_string(),
                    context_window: crate::core::context::get_context_window_info(guard.as_ref())
                        .context_window,
                }
            };
            let mut accumulator =
                StreamAccumulator::new(self.config.json_output, stream_provider_info);
            let mut first_chunk_received = false;
            let mut tool_call_detected = false;
            let mut display_buffer = String::new();
            let mut in_code_block = false;
            let mut code_block_lang = String::new();
            let mut code_block_buffer: Vec<String> = Vec::new();
            let mut code_block_lines: usize = 0;
            let mut code_block_snipped = false;
            let code_block_display_limit = code_block_display_limit(self.config.interactive_mode);

            let mut partial_line_displayed = false;
            let mut last_partial_flush_at: Option<std::time::Instant> = None;
            let mut preoutput_deadline_exceeded = false;
            let mut decoded_chunks = 0u64;
            let mut text_chunks = 0u64;
            let mut reasoning_chunks = 0u64;
            let mut max_inter_decoded_chunk_gap = std::time::Duration::ZERO;
            let mut last_decoded_chunk_at: Option<std::time::Instant> = None;

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
                        accumulator.note_preoutput_failure(format!(
                            "provider {output_kind} produced no output within {}s",
                            preoutput_budget.as_secs()
                        ));
                        break;
                    };
                    if let Ok(chunk) = tokio::time::timeout(remaining, rx.recv()).await {
                        chunk
                    } else {
                        preoutput_deadline_exceeded = true;
                        accumulator.note_preoutput_failure(format!(
                            "provider {output_kind} produced no output within {}s",
                            preoutput_budget.as_secs()
                        ));
                        break;
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

                if !matches!(&chunk, ApiStreamChunk::Timing(_) | ApiStreamChunk::Error(_)) {
                    let now = std::time::Instant::now();
                    if let Some(last_decoded_chunk_at) = last_decoded_chunk_at {
                        max_inter_decoded_chunk_gap = max_inter_decoded_chunk_gap
                            .max(now.duration_since(last_decoded_chunk_at));
                    }
                    last_decoded_chunk_at = Some(now);
                    decoded_chunks = decoded_chunks.saturating_add(1);
                }

                if !first_chunk_received
                    && !matches!(&chunk, ApiStreamChunk::Timing(_) | ApiStreamChunk::Error(_))
                {
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

                if matches!(&chunk, ApiStreamChunk::Timing(_)) {
                    self.record_stream_timing(
                        &chunk,
                        &provider,
                        decoded_chunks,
                        text_chunks,
                        reasoning_chunks,
                        max_inter_decoded_chunk_gap,
                        stream_retry_attempt,
                    )
                    .await;
                }
                for event in accumulator.push(&chunk) {
                    match event {
                        StreamEvent::VisibleText(processed) => {
                            text_chunks = text_chunks.saturating_add(1);
                            if !self.config.json_output && !processed.is_empty() {
                                self.record_first_displayable_text_time().await;
                                display_buffer.push_str(&processed);
                                while let Some(nl_pos) = display_buffer.find('\n') {
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
                                        let code_line = line.trim_end().to_string();
                                        if code_block_lines > code_block_display_limit {
                                            code_block_snipped = true;
                                            continue;
                                        }

                                        code_block_buffer.push(code_line);
                                        continue;
                                    }

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
                        StreamEvent::ReasoningText(reasoning) => {
                            reasoning_chunks = reasoning_chunks.saturating_add(1);
                            self.record_first_reasoning_chunk_time().await;
                            if !self.config.json_output && !reasoning.is_empty() {
                                self.config
                                    .output_writer
                                    .emit(OutputEvent::ReasoningChunk(reasoning));
                            }
                        }
                        StreamEvent::PrepareToolCall { name, .. } => {
                            if !self.config.json_output {
                                if !tool_call_detected {
                                    self.config.output_writer.flush();
                                    tool_call_detected = true;
                                }
                                self.config
                                    .output_writer
                                    .emit(OutputEvent::tool_call(format!("Preparing {name}…")));
                            }
                        }
                        StreamEvent::ToolCallReceived => {
                            if !tool_call_detected && !self.config.json_output {
                                self.config.output_writer.flush();
                                tool_call_detected = true;
                            }
                        }
                        StreamEvent::UsageUpdated { usage, deltas } => {
                            let mut state = self.state.lock().await;
                            state.last_api_req_info = Some(usage);
                            state.cumulative_tokens_in =
                                state.cumulative_tokens_in.saturating_add(deltas.tokens_in);
                            state.cumulative_tokens_out = state
                                .cumulative_tokens_out
                                .saturating_add(deltas.tokens_out);
                            state.cumulative_cache_writes = state
                                .cumulative_cache_writes
                                .saturating_add(deltas.cache_writes);
                            state.cumulative_cache_reads = state
                                .cumulative_cache_reads
                                .saturating_add(deltas.cache_reads);
                            state.cumulative_reasoning_tokens = state
                                .cumulative_reasoning_tokens
                                .saturating_add(deltas.reasoning_tokens);
                            state.cumulative_cost += deltas.cost;
                        }
                        StreamEvent::StreamError {
                            error: err,
                            retryable,
                            substantive_output,
                        } => {
                            if retryable && substantive_output && !self.config.json_output {
                                self.config.output_writer.emit(OutputEvent::error(format!(
                                    "Provider stream error: {err}"
                                )));
                            }
                        }
                    }
                }
            }

            let outcome = accumulator.finish();
            if !self.config.json_output {
                display_buffer.push_str(&outcome.filtered_tail);
            }
            let StreamOutcome {
                text: accumulated_text,
                filtered_text,
                filtered_tail: _,
                leaked_thinking,
                reasoning: accumulated_reasoning,
                signature: accumulated_signature,
                text_signature: accumulated_text_signature,
                redacted_data: accumulated_redacted_data,
                tool_call_order,
                tool_calls: tool_calls_map,
                usage: _,
                substantive_output: _,
                errored: stream_errored,
                retryable_error_before_output: retryable_stream_error_before_output,
                non_retryable_error: non_retryable_stream_error,
            } = outcome;

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
                            Style::default().fg(crate::cli::tui::theme::warning_fg()),
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
                filtered_text,
                leaked_thinking,
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
        let (fenced_thinking, _) = split_model_output(&filtered_text);
        let response_text = extract_response_text(&filtered_text);
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
            let extracted_thinking = match (leaked_thinking, fenced_thinking) {
                (Some(leaked), Some(fenced)) => Some(format!("{leaked}\n{fenced}")),
                (Some(leaked), None) => Some(leaked),
                (None, fenced) => fenced,
            };
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

            // Pre-request compaction already collapsed aged results this turn.
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
            // Mutation bookkeeping derives from executed outcomes below,
            // independent of presentation mode.
            let mut files_created: Vec<String> = Vec::new();
            let mut symbol_edited_paths: Vec<String> = Vec::new();
            // Commands that actually started, as opposed to calls that were
            // denied, malformed, or otherwise rejected before dispatch.
            let mut commands_executed: usize = 0;

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

                // Read-warning decay runs after execution, not here: a
                // denied or malformed call must not erase warning history
                // for files it never touched.
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
                                            let is_subagent = state.is_subagent_execution;
                                            state.record_denied_tool_action(
                                                crate::core::agent_types::DeniedToolAction {
                                                    tool_name: tool_name.clone(),
                                                    action_paths: action_paths.clone(),
                                                    params_fingerprint: params_fingerprint.clone(),
                                                },
                                            );
                                            Some(ToolExecutionOutput::error(
                                                crate::core::approval::format_denial_message_with_context(
                                                    &tool_name,
                                                    is_subagent,
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
                                            let is_subagent = state.is_subagent_execution;
                                            state.record_denied_tool_action(
                                                crate::core::agent_types::DeniedToolAction {
                                                    tool_name: tool_name.clone(),
                                                    action_paths: action_paths.clone(),
                                                    params_fingerprint: params_fingerprint.clone(),
                                                },
                                            );
                                            Some(ToolExecutionOutput::error(
                                                crate::core::approval::format_denial_message_with_context(
                                                    &tool_name,
                                                    is_subagent,
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
                                                    let is_subagent = state.is_subagent_execution;
                                                    state.record_denied_tool_action(
                                                        crate::core::agent_types::DeniedToolAction {
                                                            tool_name: tool_name.clone(),
                                                            action_paths: action_paths.clone(),
                                                            params_fingerprint: params_fingerprint
                                                                .clone(),
                                                        },
                                                    );
                                                    Some(ToolExecutionOutput::error(
                                                        crate::core::approval::format_denial_message_with_context(
                                                            &tool_name,
                                                            is_subagent,
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
                                            let params_text = tool_params.to_string();
                                            tracing::debug!(
                                                tool = %tool_name,
                                                params_len = params_text.len(),
                                                params_preview = %&params_text[..params_text.floor_char_boundary(params_text.len().min(1024))],
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
            {
                let mut state = self.state.lock().await;
                let batch = tool_tasks.len() as u32;
                state.turn_tool_calls = state.turn_tool_calls.saturating_add(batch);
                state.cumulative_tool_calls = state.cumulative_tool_calls.saturating_add(batch);
            }
            let mut result_map: std::collections::HashMap<usize, ToolExecutionOutput> =
                std::collections::HashMap::with_capacity(tool_tasks.len());
            let task_cancelled = self.state.lock().await.is_cancelled_atomic.clone();
            if !parallel_enabled {
                for (i, (_, _, _, task, _, _)) in tool_tasks.iter_mut().enumerate() {
                    if let Some(future) = task.take() {
                        result_map.insert(
                            i,
                            run_tool_unless_cancelled(task_cancelled.clone(), future).await,
                        );
                    }
                }
            }

            // Mutating and unknown-effect tools run as barriers in provider
            // order; read-only tools between two barriers form one bounded
            // batch. Path-overlap grouping cannot order a write against a
            // later validation command or symbol edit, so every barrier
            // drains the pending reads before it starts.
            if parallel_enabled {
                use futures::{FutureExt, StreamExt};
                type IndexedToolFuture =
                    futures::future::BoxFuture<'static, (usize, ToolExecutionOutput)>;
                let mut read_batch: Vec<IndexedToolFuture> = Vec::new();
                for (i, (_, tool_name, _, task, _, _)) in tool_tasks.iter_mut().enumerate() {
                    let Some(future) = task.take() else {
                        continue;
                    };
                    if Self::tool_is_schedulable_read(tool_name) {
                        let gated =
                            run_tool_unless_cancelled(task_cancelled.clone(), future).boxed();
                        read_batch.push(
                            async move {
                                let result = gated.await;
                                (i, result)
                            }
                            .boxed(),
                        );
                        continue;
                    }
                    if !read_batch.is_empty() {
                        let batch = std::mem::take(&mut read_batch);
                        for (j, result) in futures::stream::iter(batch)
                            .buffered(DEFAULT_TOOL_CONCURRENCY)
                            .collect::<Vec<_>>()
                            .await
                        {
                            result_map.insert(j, result);
                        }
                    }
                    result_map.insert(
                        i,
                        run_tool_unless_cancelled(task_cancelled.clone(), future).await,
                    );
                }
                if !read_batch.is_empty() {
                    for (j, result) in futures::stream::iter(read_batch)
                        .buffered(DEFAULT_TOOL_CONCURRENCY)
                        .collect::<Vec<_>>()
                        .await
                    {
                        result_map.insert(j, result);
                    }
                }
            }

            // One outcome per prepared call, immediate or executed: preflight
            // failures (denied, malformed, unknown, deferred) participate
            // before any statistics, completion, or plan decision. This must
            // run before result_map is drained below.
            let mut unified_failure_count = 0usize;
            let mut unified_called = false;
            for (index, (_, _, immediate, _, _, _)) in tool_tasks.iter().enumerate() {
                if let Some(output) = immediate {
                    unified_called = true;
                    unified_failure_count += usize::from(output.is_error);
                } else if let Some(output) = result_map.get(&index) {
                    unified_called = true;
                    unified_failure_count += usize::from(output.is_error);
                } else {
                    // A prepared call with no outcome at all: fail closed,
                    // matching the fabricated execution error Phase 3 pairs.
                    unified_called = true;
                    unified_failure_count += 1;
                }
            }
            // Track tool execution statistics for consecutive_mistakes tracking
            let tools_called = unified_called;
            tool_failure_count = unified_failure_count;

            let execution_results: Vec<ToolExecutionOutput> = (0..tool_tasks.len())
                .filter_map(|i| result_map.remove(&i))
                .collect();

            // Phase 3: Collect results in order, then push as ONE StorageMessage
            let mut execution_results_iter = execution_results.into_iter();
            let mut tool_result_blocks: Vec<UserContentBlock> = Vec::new();
            for (tool_id, tool_name, immediate_result_text, _task, edit_file_path, tool_params) in
                tool_tasks
            {
                let executed = immediate_result_text.is_none();
                let mut result_output = if let Some(result_text) = immediate_result_text {
                    result_text
                } else {
                    execution_results_iter.next().unwrap_or_else(|| {
                        ToolExecutionOutput::error("Tool execution failed".to_string(), None)
                    })
                };

                if tool_name == "execute_command" {
                    Self::invalidate_changed_read_state(&self.state).await;
                    if executed {
                        commands_executed += 1;
                    }
                }

                // Warning state follows actual mutation: a successfully
                // executed mutating tool invalidates its own paths, while
                // denied, malformed or deferred calls retain history for
                // files they never touched.
                if Self::READ_LOOP_MUTATING_TOOLS.contains(&tool_name.as_str()) {
                    if executed
                        && !result_output.is_error
                        && let Some(tool) = SnedTool::from_name(&tool_name)
                    {
                        Self::invalidate_read_state_for_mutation(&self.state, tool, &tool_params)
                            .await;
                    }
                } else if !Self::READ_LOOP_INSPECTION_TOOLS.contains(&tool_name.as_str()) {
                    let mut state = self.state.lock().await;
                    Self::decay_read_loop_state(&mut state, &tool_name);
                }

                // Workspace effects derive from actual per-path outcomes, not
                // from batch-summary text, the requested call count, or the
                // presentation mode. A failed write is never counted as
                // created; content applied without published anchors still
                // changed the workspace and counts.
                let content_changed = !result_output.is_error
                    || result_output
                        .publication_outcomes
                        .iter()
                        .any(|outcome| outcome.content_applied);
                if content_changed && let Some(tool) = SnedTool::from_name(&tool_name) {
                    match tool {
                        SnedTool::WriteToFile => {
                            files_created.extend(Self::extract_action_path(tool, &tool_params));
                        }
                        SnedTool::EditFile => {
                            let (_, added, removed) =
                                extract_edit_stats_detailed(&result_output.text);
                            if added > 0 || removed > 0 {
                                for path in &edit_file_path {
                                    edit_files.push((path.display.clone(), added, removed));
                                }
                            }
                        }
                        SnedTool::ReplaceSymbol | SnedTool::RenameSymbol => {
                            symbol_edited_paths
                                .extend(Self::extract_action_path(tool, &tool_params));
                        }
                        _ => {}
                    }
                }

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
                        let (stats, _, _) = extract_edit_stats_detailed(&result_output.text);
                        let status = if is_error { "✗" } else { "✓" };
                        self.config
                            .output_writer
                            .emit(OutputEvent::tool_output_line(
                                format!("  {status} {stats}"),
                                Style::default().fg(if is_error { error_fg() } else { prompt_fg() }),
                            ));
                        if is_error {
                            for detail in
                                crate::core::tool_output::edit_failure_details(&result_output.text)
                            {
                                self.config
                                    .output_writer
                                    .emit(OutputEvent::tool_output_line(
                                        format!("    {detail}"),
                                        Style::default().fg(error_fg()),
                                    ));
                            }
                        } else {
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
                            if is_error { error_fg() } else { prompt_fg() },
                            prompt_fg(),
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
                        let style = crate::cli::tui::theme::emphasize_for(
                            crate::cli::tui::theme::high_contrast(),
                            Style::default().fg(if is_error { error_fg() } else { prompt_fg() }),
                        );
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
                if result_output.is_error {
                    tracing::debug!(
                        tool_id = %tool_id,
                        tool_name = %tool_name,
                        params = %truncated_debug_text(&tool_params.to_string()),
                        result = %truncated_debug_text(&result_output.text),
                        "tool error full context"
                    );
                }

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
            if !edit_files.is_empty()
                || !files_created.is_empty()
                || !symbol_edited_paths.is_empty()
            {
                let mut edited_paths: Vec<String> =
                    edit_files.iter().map(|(p, _, _)| p.clone()).collect();
                edited_paths.extend(files_created.iter().cloned());
                edited_paths.extend(symbol_edited_paths.iter().cloned());
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
            }

            // Auto-commit to shadow git after file-modifying turns in any
            // output mode. Only commit when files were actually modified
            // (not just attempted, failed, or denied).
            if self.config.track_changes
                && let Some(message) =
                    Self::shadow_commit_message(&edit_files, &files_created, &symbol_edited_paths)
                && let Ok(workspace_root) = std::env::current_dir()
            {
                // Run synchronous git operations in spawn_blocking to avoid blocking runtime
                let result = tokio::task::spawn_blocking(move || {
                    crate::core::shadow_git::commit_turn(&workspace_root, &message)
                })
                .await;
                report_shadow_commit_result(&self.config.output_writer, result);
            }

            // Print action digest summarizing what happened in this turn
            if !self.config.json_output && !prepared_tool_calls.is_empty() {
                let files_created = files_created.len();
                let files_edited = edit_files
                    .iter()
                    .filter(|(_, added, removed)| *added > 0 || *removed > 0)
                    .count();
                let commands_run = commands_executed;

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
                            Style::default().fg(crate::cli::tui::theme::info_fg()),
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
        // Completion derives from an accepted completion outcome, never a
        // tool name alone: a rejected attempt_completion leaves
        // completion_result unset, and a failure elsewhere in the batch
        // must not be hidden by a completion request.
        let completion_candidate = completion_result.is_some() || text_only_completes_task;
        let plan_mode_responded = prepared_tool_calls.iter().any(|prepared| {
            matches!(
                SnedTool::from_name(&prepared.tool_name),
                Some(SnedTool::PlanModeRespond)
            )
        });
        let is_completion = tool_failure_count == 0
            && (completion_candidate || plan_mode_responded)
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
                let timing = self.capture_turn_end_timing().await;
                self.config.output_writer.emit(OutputEvent::TurnEnd {
                    accumulated_text: String::new(),
                    timing,
                });
            } else {
                self.emit_turn_end(markdown_text).await;
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
            if self.config.json_output {
                self.emit_turn_result_event(true).await;
            }
            TurnResult::Complete
        } else {
            // Same turn-end signal for the "more turns coming" branch.
            let markdown_text = response_text.as_deref().unwrap_or("");
            self.emit_turn_end(markdown_text).await;
            if self.config.json_output {
                // Intermediate turn: emit metrics under a different type so the
                // JsonOutputLayer doesn't overwrite the final result_file payload.
                self.emit_turn_result_event(false).await;
            }
            TurnResult::Continue
        }
    }

    async fn emit_turn_result_event(&self, is_final: bool) {
        let (
            tool_calls,
            input_tokens,
            output_tokens,
            cache_write_tokens,
            cache_read_tokens,
            total_cost,
            context_tokens,
            context_window,
            context_usage_pct,
        ) = {
            let state = self.state.lock().await;
            let tool_calls = state.turn_tool_calls;
            let input_tokens = state.cumulative_tokens_in;
            let output_tokens = state.cumulative_tokens_out;
            let cache_write_tokens = state.cumulative_cache_writes;
            let cache_read_tokens = state.cumulative_cache_reads;
            let total_cost = state.cumulative_cost;
            let context_tokens = state
                .last_api_req_info
                .as_ref()
                .and_then(|r| r.context_tokens)
                .unwrap_or(0);
            let context_window = state
                .last_api_req_info
                .as_ref()
                .and_then(|r| r.context_window)
                .unwrap_or(0);
            let context_usage_pct = state
                .last_api_req_info
                .as_ref()
                .and_then(|r| r.context_usage_percentage)
                .unwrap_or(0.0);
            (
                tool_calls,
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_read_tokens,
                total_cost,
                context_tokens,
                context_window,
                context_usage_pct,
            )
        };

        let event_type = if is_final { "result" } else { "turn_end" };
        tracing::info!(
            target: "json_output",
            "{}",
            serde_json::json!({
                "type": event_type,
                "tool_calls": tool_calls,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_write_tokens": cache_write_tokens,
                "cache_read_tokens": cache_read_tokens,
                "total_cost": total_cost,
                "context_tokens": context_tokens,
                "context_window": context_window,
                "context_usage_pct": context_usage_pct,
            })
            .to_string()
        );
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
        let id = guard.get_model().id;
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

    /// Shadow-commit message for a turn's actual workspace mutations, or
    /// `None` when nothing committable happened. Takes only executed
    /// outcomes, never the presentation mode: JSON versus TTY changes
    /// rendering, not whether a mutation is recorded.
    fn shadow_commit_message(
        edit_files: &[(String, i32, i32)],
        files_created: &[String],
        symbol_edited_paths: &[String],
    ) -> Option<String> {
        let mut parts = Vec::new();
        if edit_files
            .iter()
            .any(|(_, added, removed)| *added > 0 || *removed > 0)
        {
            parts.push(format_heat_map_plain(edit_files));
        }
        if !files_created.is_empty() {
            parts.push(format!("created {}", files_created.join(", ")));
        }
        if !symbol_edited_paths.is_empty() {
            parts.push(format!("symbols {}", symbol_edited_paths.join(", ")));
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!("[sned] turn: {}", parts.join("; ")))
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

    /// Whether a parallel-scheduled call may run inside a read-only batch.
    /// Only tools categorized as read-only qualify; unknown names fail
    /// closed to barrier so an unrecognized mutation cannot slip into a
    /// concurrent batch.
    fn tool_is_schedulable_read(tool_name: &str) -> bool {
        SnedTool::from_name(tool_name).is_some_and(|tool| {
            matches!(
                tool.category(),
                crate::core::tools::ToolCategory::ReadOnly
                    | crate::core::tools::ToolCategory::ReadFiles
            )
        })
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
                        Vec::new(),
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
                e.publication_outcomes().unwrap_or(&[]).to_vec(),
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
            // Snapshot counters and usage under the guard, then release it:
            // the metadata write takes a blocking file lock and performs
            // disk IO that must never stall state readers or the UI.
            let (persisted_usage, full_save, compacted_summary) = {
                let mut state = self.state.lock().await;
                state.turns_since_save += 1;
                let full_save = state.turns_since_save >= 5;
                if full_save {
                    state.turns_since_save = 0;
                }
                (
                    state
                        .last_api_req_info
                        .as_ref()
                        .map(crate::core::context::context_manager::PersistedApiReqInfo::from),
                    full_save,
                    full_save.then(|| state.compacted_summary.clone()).flatten(),
                )
            };

            // Keep the latest usage available after short sessions too; only
            // conversation history remains debounced below.
            if let Err(e) = storage.update_metadata(|metadata| {
                metadata.last_api_req_info.clone_from(&persisted_usage);
            }) {
                error!("Failed to save last API request info: {}", e);
            }

            // Debounce: only save every 5 turns to reduce I/O overhead
            if full_save {
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

            request.messages.clone_from(&history);

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
            Self::clear_history_dependent_read_state(&mut state);
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
            let mut recovered = storage.read_api_conversation_history_with_recovery();
            let compacted_summary: Option<crate::core::context::context_manager::CompactedSummary> =
                storage.read_compacted_summary();

            let mut loaded = false;

            // Resolve the saved deleted range BEFORE installing the recovered
            // messages below: remap needs the pre-install lineage and length.
            // The saved coordinates predate recovery: remap them against
            // the dropped lineage, or invalidate when recovery removed
            // records inside the range. Never apply old coordinates to the
            // shortened vector unquestioningly.
            let remapped_deleted_range = if let Some(ref state_manager) = self.state_manager
                && let Some(history_item) = state_manager.find_task_in_history(&self.config.task_id)
                && let Some(deleted_range_vec) = history_item.conversation_history_deleted_range
                && deleted_range_vec.len() >= 2
            {
                // Convert from Vec<i32> to (usize, usize) tuple for TaskState
                let remapped = recovered
                    .remap_file_range(deleted_range_vec[0] as usize, deleted_range_vec[1] as usize);
                if remapped.is_none() {
                    tracing::debug!(
                        dropped = ?recovered.dropped,
                        saved_range = ?deleted_range_vec,
                        "Discarding saved deleted range: recovery changed the history it was recorded against"
                    );
                }
                Some(remapped)
            } else {
                None
            };

            if !recovered.messages.is_empty() {
                let mut current = self.conversation_history.lock().await;
                std::mem::swap(&mut *current, &mut recovered.messages);
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
            if let Some(remapped) = remapped_deleted_range {
                let mut state = self.state.lock().await;
                state.conversation_history_deleted_range = remapped;
                loaded = true;
            }

            loaded
        } else {
            false
        }
    }

    /// Clear compacted summary to allow re-compaction.
    /// Returns true if a summary was cleared, false if none existed.
    pub async fn clear_compacted_summary(&self) -> bool {
        // Snapshot under the guard, then run the blocking file removal
        // without holding state.
        let (had_summary, file_path) = {
            let mut state = self.state.lock().await;
            if state.compacted_summary.is_some() {
                state.compacted_summary = None;
                let file_path = self.deps.task_storage.as_ref().map(|storage| {
                    storage
                        .task_dir()
                        .join(crate::storage::disk::GlobalFileNames::COMPACTED_SUMMARY)
                });
                (true, file_path)
            } else {
                (false, None)
            }
        };
        if had_summary {
            // Also delete the file if task storage is configured
            if let Some(file_path) = file_path {
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
                truncate_thinking_text(&mut thinking_block.thinking, limit);
            }
        }
    }
}

fn truncate_thinking_text(thinking: &mut String, token_limit: usize) {
    // Token limits are approximate, but truncation must use a valid byte
    // boundary because provider reasoning can contain multibyte text.
    let byte_limit = token_limit.saturating_mul(4);
    if thinking.len() > byte_limit {
        let safe_limit = thinking.floor_char_boundary(byte_limit);
        thinking.truncate(safe_limit);
        thinking.push_str("\n\n[truncated]");
    }
}

/// Compacts older tool results in the conversation history, preserving
/// the most recent `RECENT_TO_PRESERVE` reads and shell outputs in full.
/// Stale bulk output costs tokens on every later turn without adding new
/// information, so earlier large results collapse to a placeholder.
fn compact_old_tool_results(history: &mut [StorageMessage]) {
    const RECENT_TO_PRESERVE: usize = 2;
    const MIN_BYTES_TO_COMPACT: usize = 1000;

    let bytes_before = tool_result_text_bytes(history);

    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for message in history.iter() {
        if message.role != MessageRole::Assistant {
            continue;
        }
        let MessageContent::AssistantBlocks(blocks) = &message.content else {
            continue;
        };
        for block in blocks {
            if let AssistantContentBlock::ToolUse(tool_use) = block {
                tool_names.insert(tool_use.id.clone(), tool_use.name.clone());
            }
        }
    }

    let mut read_result_count = 0;
    let mut shell_result_count = 0;
    let mut search_result_count = 0;
    let mut edit_result_count = 0;

    // The newest tool-result batch has never appeared in a provider request:
    // compaction runs before the request is built. Everything after the
    // newest assistant tool-use message belongs to that undelivered batch
    // and is exempt from age-based collapse.
    let mut newest_tool_use_index: Option<usize> = None;
    for (index, message) in history.iter().enumerate() {
        if message.role != MessageRole::Assistant {
            continue;
        }
        let MessageContent::AssistantBlocks(blocks) = &message.content else {
            continue;
        };
        if blocks
            .iter()
            .any(|block| matches!(block, AssistantContentBlock::ToolUse(_)))
        {
            newest_tool_use_index = Some(index);
        }
    }

    for (index, message) in history.iter_mut().enumerate().rev() {
        if newest_tool_use_index.is_some_and(|cutoff| index > cutoff) {
            continue;
        }
        if message.role != MessageRole::User {
            continue;
        }
        let MessageContent::UserBlocks(blocks) = &mut message.content else {
            continue;
        };

        for block in blocks.iter_mut().rev() {
            let UserContentBlock::ToolResult(tr) = block else {
                continue;
            };

            let is_read_result = match &tr.content {
                ToolResultContent::Text(text) => is_read_result_text(text),
                ToolResultContent::Blocks(b) => b.iter().any(|cb| match cb {
                    ToolResultContentBlock::Text { text } => is_read_result_text(text),
                    _ => false,
                }),
            };
            let tool_name = tool_names.get(tr.tool_use_id.as_str());
            let is_shell_result =
                !is_read_result && tool_name.is_some_and(|name| *name == "execute_command");
            let is_search_result = !is_read_result
                && !is_shell_result
                && tool_name.is_some_and(|name| {
                    *name == "search_files"
                        || *name == "list_files"
                        || *name == "get_file_skeleton"
                });

            let is_edit_result = !is_read_result
                && !is_shell_result
                && !is_search_result
                && tool_name.is_some_and(|name| *name == "edit_file");

            // Other tools are never collapsed here.
            let counter = if is_read_result {
                &mut read_result_count
            } else if is_shell_result {
                &mut shell_result_count
            } else if is_search_result {
                &mut search_result_count
            } else if is_edit_result {
                &mut edit_result_count
            } else {
                continue;
            };

            *counter += 1;
            if *counter <= RECENT_TO_PRESERVE {
                continue;
            }

            match &mut tr.content {
                ToolResultContent::Text(text) => {
                    if is_read_result {
                        compact_single_read_text(text, MIN_BYTES_TO_COMPACT);
                    } else if is_search_result {
                        compact_single_search_text(text, MIN_BYTES_TO_COMPACT);
                    } else if is_edit_result {
                        compact_single_edit_text(text, MIN_BYTES_TO_COMPACT);
                    } else {
                        compact_single_shell_text(text, MIN_BYTES_TO_COMPACT);
                    }
                }
                ToolResultContent::Blocks(b) => {
                    for cb in b.iter_mut() {
                        if let ToolResultContentBlock::Text { text } = cb {
                            if is_read_result {
                                compact_single_read_text(text, MIN_BYTES_TO_COMPACT);
                            } else if is_search_result {
                                compact_single_search_text(text, MIN_BYTES_TO_COMPACT);
                            } else if is_edit_result {
                                compact_single_edit_text(text, MIN_BYTES_TO_COMPACT);
                            } else {
                                compact_single_shell_text(text, MIN_BYTES_TO_COMPACT);
                            }
                        }
                    }
                }
            }
        }
    }

    // One line per turn keeps every debug log a running record of what
    // compaction actually saved, so future sessions measure themselves.
    let bytes_after = tool_result_text_bytes(history);
    tracing::debug!(
        bytes_before,
        bytes_after,
        saved = bytes_before.saturating_sub(bytes_after),
        "history compaction collapsed stale tool results"
    );
}

fn tool_result_text_bytes(history: &[StorageMessage]) -> usize {
    history
        .iter()
        .filter(|msg| msg.role == MessageRole::User)
        .filter_map(|msg| match &msg.content {
            MessageContent::UserBlocks(blocks) => Some(blocks),
            _ => None,
        })
        .flat_map(|blocks| blocks.iter())
        .filter_map(|block| match block {
            UserContentBlock::ToolResult(tr) => Some(&tr.content),
            _ => None,
        })
        .map(|content| match content {
            ToolResultContent::Text(text) => text.len(),
            ToolResultContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|cb| match cb {
                    ToolResultContentBlock::Text { text } => Some(text.len()),
                    _ => None,
                })
                .sum(),
        })
        .sum()
}

fn compact_single_shell_text(text: &mut String, min_bytes: usize) {
    if text.len() <= min_bytes {
        return;
    }

    let header_end = text.find('\n').unwrap_or(text.len());
    let header = &text[..header_end.min(text.len())];

    let total_bytes = text.len();
    let total_lines = text.lines().count();

    *text = format!(
        "{}\n[... Earlier shell output ({} lines, {} bytes) collapsed to save context space. Re-run the command for fresh output.]",
        header.trim_end(),
        total_lines,
        total_bytes
    );
}

fn is_read_result_text(text: &str) -> bool {
    // Re-reads of edited files come back section-summarized without a
    // file header; without this they would never collapse.
    text.starts_with("[File: ")
        || text.contains("\n[File: ")
        || text.starts_with("[Context pruned:")
}

fn compact_single_read_text(text: &mut String, min_bytes: usize) {
    if text.len() <= min_bytes {
        return;
    }

    let header_end = text
        .find("\n[Anchors:")
        .or_else(|| text.find("\n\n"))
        .or_else(|| text.find('\n'))
        .unwrap_or(text.len());
    let header = &text[..header_end.min(text.len())];

    let total_bytes = text.len();
    let total_lines = text.lines().count();

    // Retaining anchored lines kept most reads at full size on every later
    // turn, while one fresh read_file restores citable anchors on demand.
    *text = format!(
        "{}\n[... Earlier read content ({} lines, {} bytes) collapsed to save context space. Call read_file again if fresh anchors are needed.]",
        header.trim_end(),
        total_lines,
        total_bytes
    );
}

fn compact_single_edit_text(text: &mut String, min_bytes: usize) {
    if text.len() <= min_bytes {
        return;
    }

    let header_end = text.find('\n').unwrap_or(text.len());
    let header = &text[..header_end.min(text.len())];

    let total_bytes = text.len();
    let total_lines = text.lines().count();

    // Aged edit anchors are unusable once later edits land, and the newest
    // results stay intact for follow-up edits; the header keeps the outcome.
    *text = format!(
        "{}\n[... Earlier edit result ({} lines, {} bytes) collapsed to save context space. Re-read the file for fresh anchors.]",
        header.trim_end(),
        total_lines,
        total_bytes
    );
}

fn compact_single_search_text(text: &mut String, min_bytes: usize) {
    if text.len() <= min_bytes {
        return;
    }

    let header_end = text.find('\n').unwrap_or(text.len());
    let header = &text[..header_end.min(text.len())];

    let total_bytes = text.len();
    let total_lines = text.lines().count();

    *text = format!(
        "{}\n[... Earlier search results ({} lines, {} bytes) collapsed to save context space. Re-run the discovery tool for fresh results.]",
        header.trim_end(),
        total_lines,
        total_bytes
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::stream_parsing::ThinkingTagStreamFilter;

    #[test]
    fn truncated_debug_text_passes_short_text_through() {
        assert_eq!(truncated_debug_text("short"), "short");
        let capped = "x".repeat(DEBUG_ERROR_CONTEXT_CAP + 10);
        let out = truncated_debug_text(&capped);
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(out.ends_with("more chars truncated]"));
        assert!(out.len() < DEBUG_ERROR_CONTEXT_CAP + 100);
    }

    fn filter_thinking_chunks(chunks: &[&str]) -> String {
        let mut filter = ThinkingTagStreamFilter::new();
        let mut visible = String::new();
        for chunk in chunks {
            visible.push_str(&filter.push(chunk));
        }
        visible.push_str(&filter.finish());
        visible
    }

    #[test]
    fn thinking_tags_are_filtered_at_every_stream_split_boundary() {
        for (open, close) in [
            ("<think>", "</think>"),
            ("<!-- think -->", "<!-- /think -->"),
        ] {
            let input = format!("before{open}hidden{close}after");
            let expected = "beforeafter";
            for split in 0..=input.len() {
                if !input.is_char_boundary(split) {
                    continue;
                }
                assert_eq!(
                    filter_thinking_chunks(&[&input[..split], &input[split..]]),
                    expected,
                    "failed at byte split {split} for {open}"
                );
            }

            for open_split in 0..=open.len() {
                for close_split in 0..=close.len() {
                    assert_eq!(
                        filter_thinking_chunks(&[
                            "before",
                            &open[..open_split],
                            &open[open_split..],
                            "hidden",
                            &close[..close_split],
                            &close[close_split..],
                            "after",
                        ]),
                        expected,
                        "failed at delimiter splits {open_split}/{close_split} for {open}"
                    );
                }
            }
        }
    }

    #[test]
    fn thinking_filter_handles_multiple_tags_and_unfinished_eof() {
        assert_eq!(
            filter_thinking_chunks(&[
                "a<th",
                "ink>one</think>b<!-- thi",
                "nk -->two<!-- /think -->c",
            ]),
            "abc"
        );
        assert_eq!(filter_thinking_chunks(&["visible<thi"]), "visible<thi");
        assert_eq!(
            filter_thinking_chunks(&["visible<think>unfinished reasoning"]),
            "visible"
        );
        assert_eq!(
            filter_thinking_chunks(&["visible<think>hidden</thi"]),
            "visible"
        );
    }

    #[test]
    fn thinking_tags_inside_streamed_fences_remain_visible() {
        let expected = concat!(
            "before\n",
            "```html\n",
            "<think>literal</think>\n",
            "<!-- think -->literal<!-- /think -->\n",
            "```\n",
            "after"
        );
        let chunks = [
            "before\n``",
            "`html\n<th",
            "ink>literal</think>\n<!-- thi",
            "nk -->literal<!-- /think -->\n`",
            "``\nafter",
        ];
        assert_eq!(filter_thinking_chunks(&chunks), expected);

        assert_eq!(
            filter_thinking_chunks(&["~~~text\n<thi", "nk>literal</think>\n~~~\n"]),
            "~~~text\n<think>literal</think>\n~~~\n"
        );
    }

    #[test]
    fn fenced_thinking_is_hidden_at_every_stream_split_boundary() {
        let input = "before\n```think\nhidden\n```\nafter";
        for split in 0..=input.len() {
            assert_eq!(
                filter_thinking_chunks(&[&input[..split], &input[split..]]),
                "before\nafter",
                "failed at byte split {split}"
            );
        }
        assert_eq!(
            split_model_output(input),
            (
                Some("hidden".to_string()),
                Some("before\nafter".to_string())
            )
        );
    }

    #[test]
    fn literal_fence_closing_matches_final_parser() {
        let input = concat!(
            "before\n",
            "````\n",
            "```think\n",
            "literal thinking marker\n",
            "```python\n",
            "still literal\n",
            "````\n",
            "after"
        );
        for split in 0..=input.len() {
            assert_eq!(
                filter_thinking_chunks(&[&input[..split], &input[split..]]),
                input,
                "failed at byte split {split}"
            );
        }

        let indented = "    ```think\nhidden\n    ```\nafter";
        assert_eq!(filter_thinking_chunks(&[indented]), indented);
    }

    #[tokio::test]
    async fn fenced_thinking_matches_displayed_and_persisted_output() {
        let responses = vec![vec![
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "before\n``".to_string(),
                id: None,
                signature: None,
            }),
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "`thi".to_string(),
                id: None,
                signature: None,
            }),
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "nk\nhidden\n`".to_string(),
                id: None,
                signature: None,
            }),
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "``\nafter".to_string(),
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
        let mut config = test_agent_config(provider, "test-fenced-thinking");
        config.output_writer = writer;
        let mut agent = AgentLoop::new(config);

        let _ = agent.execute_turn().await;

        let events = drain_output_events(&mut priority_rx, &mut rx);
        let rendered = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::Line(line) | OutputEvent::ModelUpdateLine(line) => {
                    Some(line.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("before"), "{rendered}");
        assert!(rendered.contains("after"), "{rendered}");
        assert!(!rendered.contains("hidden"), "{rendered}");
        assert!(!rendered.contains("```think"), "{rendered}");

        let history = agent.conversation_history.lock().await;
        let blocks = history
            .iter()
            .rev()
            .find_map(|message| match &message.content {
                MessageContent::AssistantBlocks(blocks) => Some(blocks),
                _ => None,
            })
            .expect("expected assistant block history");
        assert!(blocks.iter().any(|block| matches!(
            block,
            AssistantContentBlock::Text(text) if text.text == "before\nafter"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            AssistantContentBlock::Thinking(thinking) if thinking.thinking == "hidden\n"
        )));
    }

    #[tokio::test]
    async fn structured_reasoning_interleaves_with_split_thinking_tags() {
        let responses = vec![vec![
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "before<th".to_string(),
                id: None,
                signature: None,
            }),
            ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                reasoning: "structured reasoning".to_string(),
                details: None,
                signature: None,
                redacted_data: None,
                id: Some("reasoning-1".to_string()),
            }),
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "ink>hidden</thi".to_string(),
                id: None,
                signature: None,
            }),
            ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                reasoning: " continues".to_string(),
                details: None,
                signature: None,
                redacted_data: None,
                id: Some("reasoning-2".to_string()),
            }),
            ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "nk>after\n".to_string(),
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
        let mut config = test_agent_config(provider, "test-split-thinking-with-reasoning");
        config.output_writer = writer;
        let mut agent = AgentLoop::new(config);

        let _ = agent.execute_turn().await;

        let events = drain_output_events(&mut priority_rx, &mut rx);
        let rendered = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::Line(line) | OutputEvent::ModelUpdateLine(line) => {
                    Some(line.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let reasoning = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::ReasoningChunk(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect::<String>();

        assert!(rendered.contains("beforeafter"), "{rendered}");
        assert!(!rendered.contains("hidden"), "{rendered}");
        assert_eq!(reasoning, "structured reasoning continues");

        let history = agent.conversation_history.lock().await;
        let blocks = history
            .iter()
            .rev()
            .find_map(|message| match &message.content {
                MessageContent::AssistantBlocks(blocks) => Some(blocks),
                _ => None,
            })
            .expect("expected assistant block history");
        assert!(blocks.iter().any(|block| matches!(
            block,
            AssistantContentBlock::Text(text) if text.text == "beforeafter"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            AssistantContentBlock::Thinking(thinking)
                if thinking.thinking == "hidden\nstructured reasoning continues"
        )));
    }
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
                        // Models copy the full numbered line (`NNN: Word§…`);
                        // split_anchor strips the gutter downstream.
                        let line = line.trim_start_matches(|c: char| {
                            c.is_ascii_digit() || c == ':' || c == ' '
                        });
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
                    false,
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

    /// Records per-call start/finish events so scheduling tests assert
    /// barrier order from the log instead of elapsed time.
    struct OrderProbeHandler {
        log: Arc<std::sync::Mutex<Vec<String>>>,
        delay_ms: u64,
        rendezvous: Option<(Arc<std::sync::atomic::AtomicUsize>, usize)>,
    }

    impl OrderProbeHandler {
        fn label(params: &serde_json::Value) -> String {
            if let Some(path) = params.get("path").and_then(|v| v.as_str()) {
                return path.to_string();
            }
            if let Some(paths) = params.get("paths").and_then(|v| v.as_array()) {
                return paths
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
            }
            if let Some(files) = params.get("files").and_then(|v| v.as_array()) {
                return files
                    .iter()
                    .filter_map(|f| f.get("path"))
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
            }
            if let Some(commands) = params.get("commands").and_then(|v| v.as_array()) {
                return commands
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
            }
            if let Some(pattern) = params.get("pattern").and_then(|v| v.as_str()) {
                return pattern.to_string();
            }
            "call".to_string()
        }
    }

    impl crate::core::tools::ToolHandler for OrderProbeHandler {
        fn execute(
            &self,
            _ctx: &ToolContext,
            params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, crate::core::tools::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            let log = self.log.clone();
            let delay_ms = self.delay_ms;
            let rendezvous = self.rendezvous.clone();
            let label = Self::label(&params);
            Box::pin(async move {
                log.lock().unwrap().push(format!("start {label}"));
                if let Some((arrived, expected)) = rendezvous {
                    arrived.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    for _ in 0..5000 {
                        if arrived.load(std::sync::atomic::Ordering::SeqCst) >= expected {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    assert!(
                        arrived.load(std::sync::atomic::Ordering::SeqCst) >= expected,
                        "read rendezvous timed out: independent reads did not overlap"
                    );
                } else if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                log.lock().unwrap().push(format!("finish {label}"));
                Ok(serde_json::json!("ok"))
            })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "order probe".to_string()
        }
    }

    fn tool_call_chunks<S: AsRef<str>>(calls: &[(S, S, serde_json::Value)]) -> Vec<ApiStreamChunk> {
        calls
            .iter()
            .map(|(id, name, args)| {
                ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                    tool_call: ApiStreamToolCall {
                        call_id: Some(id.as_ref().to_string()),
                        function: ApiStreamToolCallFunction {
                            id: None,
                            name: Some(name.as_ref().to_string()),
                            arguments: Some(args.to_string()),
                        },
                        signature: None,
                    },
                    id: None,
                    signature: None,
                })
            })
            .collect()
    }

    fn assert_log_order(log: &[String], first: &str, second: &str) {
        let a = log
            .iter()
            .position(|e| e == first)
            .unwrap_or_else(|| panic!("missing log event {first:?} in {log:?}"));
        let b = log
            .iter()
            .position(|e| e == second)
            .unwrap_or_else(|| panic!("missing log event {second:?} in {log:?}"));
        assert!(
            a < b,
            "{first:?} must precede {second:?} in barrier order, got {log:?}"
        );
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

    struct PatternEchoHandler;

    impl crate::core::tools::ToolHandler for PatternEchoHandler {
        fn execute(
            &self,
            _ctx: &ToolContext,
            params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, crate::core::tools::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                let pattern = params
                    .get("pattern")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                Ok(serde_json::Value::String(format!(
                    "RESULT-{pattern}\n{}",
                    "y".repeat(1200)
                )))
            })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "pattern echo".to_string()
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
            false,
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
    fn edit_diff_preview_accepts_precise_identity_guidance() {
        let result = "Edited 1 file(s): 1 edit(s) applied.\n\nApplied 1 edit(s) successfully (+1, -1 lines). Untouched lines retain their anchors. Inserted and changed lines have new anchors shown below.\n\n- Old§old\n+ New§new";
        let previews = edit_result_diff_previews(result);
        assert_eq!(previews.len(), 1);
        assert!(previews[0].contains("Old§old"));
        assert!(previews[0].contains("New§new"));
    }

    #[test]
    fn publication_failure_does_not_render_a_normal_edit_diff() {
        let result = "Edited 1 file(s): 1 edit(s) applied.\n\nContent was applied, but anchor state could not be published.\nNo returned anchors are reusable. Call read_file before editing this file again.";
        assert!(edit_result_diff_previews(result).is_empty());
        let retained = truncate_tool_result(result);
        assert!(retained.contains("Content was applied"));
        assert!(retained.contains("Call read_file"));
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

    fn parallel_probe_agent(
        responses: Vec<Vec<ApiStreamChunk>>,
        log: &Arc<std::sync::Mutex<Vec<String>>>,
        rendezvous: Option<(Arc<std::sync::atomic::AtomicUsize>, usize)>,
    ) -> AgentLoop {
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        for tool in [
            SnedTool::WriteToFile,
            SnedTool::EditFile,
            SnedTool::ReadFile,
            SnedTool::ExecuteCommand,
            SnedTool::ReplaceSymbol,
        ] {
            registry.register(
                tool,
                Arc::new(OrderProbeHandler {
                    log: log.clone(),
                    delay_ms: 50,
                    rendezvous: rendezvous.clone(),
                }),
            );
        }
        AgentLoop::new(test_agent_config(provider, "parallel-barrier"))
            .with_tools(Arc::new(registry))
            .with_system_prompt_context(SystemPromptContext {
                enable_parallel_tool_calling: true,
                ..Default::default()
            })
    }

    #[tokio::test]
    async fn test_overlapping_write_groups_stay_in_provider_order() {
        use serde_json::json;
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let edit = |id: String, paths: &[&str]| {
            (
                id,
                "edit_file".to_string(),
                json!({"files": paths.iter().map(|p| json!({"path": p, "edits": []})).collect::<Vec<_>>()}),
            )
        };
        let turn1 = vec![
            edit("c1".to_string(), &["overlap-a.txt"]),
            edit("c2".to_string(), &["overlap-a.txt", "overlap-b.txt"]),
            edit("c3".to_string(), &["overlap-b.txt"]),
        ];
        let turn2 = vec![
            edit("d1".to_string(), &["chain-a.txt"]),
            edit("d2".to_string(), &["chain-b.txt"]),
            edit("d3".to_string(), &["chain-a.txt", "chain-b.txt"]),
        ];
        let responses = vec![tool_call_chunks(&turn1), tool_call_chunks(&turn2)];
        let mut agent = parallel_probe_agent(responses, &log, None);
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let log = log.lock().unwrap();
        assert_log_order(
            &log,
            "finish overlap-a.txt",
            "start overlap-a.txt,overlap-b.txt",
        );
        assert_log_order(
            &log,
            "finish overlap-a.txt,overlap-b.txt",
            "start overlap-b.txt",
        );
        assert_log_order(&log, "finish chain-a.txt", "start chain-b.txt");
        assert_log_order(&log, "finish chain-b.txt", "start chain-a.txt,chain-b.txt");
    }

    #[tokio::test]
    async fn test_write_completes_before_dependent_read() {
        use serde_json::json;
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = vec![tool_call_chunks(&[
            (
                "w1",
                "write_to_file",
                json!({"path": "dep.txt", "content": "new"}),
            ),
            (
                "r1",
                "read_file",
                json!({"paths": ["dep.txt", "other.txt"]}),
            ),
        ])];
        let mut agent = parallel_probe_agent(responses, &log, None);
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let log = log.lock().unwrap();
        assert_log_order(&log, "finish dep.txt", "start dep.txt,other.txt");
        assert_eq!(log.iter().filter(|e| e.starts_with("start")).count(), 2);
    }

    #[tokio::test]
    async fn test_write_completes_before_validation_command() {
        use serde_json::json;
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = vec![tool_call_chunks(&[
            (
                "w1",
                "write_to_file",
                json!({"path": "built.txt", "content": "new"}),
            ),
            ("v1", "execute_command", json!({"commands": ["cargo test"]})),
        ])];
        let mut agent = parallel_probe_agent(responses, &log, None);
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let log = log.lock().unwrap();
        assert_log_order(&log, "finish built.txt", "start cargo test");
    }

    #[tokio::test]
    async fn test_anchor_edit_completes_before_symbol_edit() {
        use serde_json::json;
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = vec![tool_call_chunks(&[
            (
                "e1",
                "edit_file",
                json!({"files": [{"path": "anchor.rs", "edits": []}]}),
            ),
            ("s1", "replace_symbol", json!({"path": "sym.rs"})),
        ])];
        let mut agent = parallel_probe_agent(responses, &log, None);
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let log = log.lock().unwrap();
        assert_log_order(&log, "finish anchor.rs", "start sym.rs");
    }

    #[tokio::test]
    async fn test_independent_reads_stay_concurrent() {
        use serde_json::json;
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let arrived = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let responses = vec![tool_call_chunks(&[
            ("r1", "read_file", json!({"paths": ["one.txt"]})),
            ("r2", "read_file", json!({"paths": ["two.txt"]})),
        ])];
        let mut agent = parallel_probe_agent(responses, &log, Some((arrived.clone(), 2)));
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        assert_eq!(arrived.load(std::sync::atomic::Ordering::SeqCst), 2);
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 4);
    }

    #[tokio::test]
    async fn test_read_batches_respect_concurrency_cap() {
        use serde_json::json;
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls: Vec<(String, String, serde_json::Value)> = (0..20)
            .map(|i| {
                (
                    format!("r{i}"),
                    "read_file".to_string(),
                    json!({"paths": [format!("file-{i}.txt")]}),
                )
            })
            .collect();
        let borrowed: Vec<(&str, &str, serde_json::Value)> = calls
            .iter()
            .map(|(id, name, args)| (id.as_str(), name.as_str(), args.clone()))
            .collect();
        let responses = vec![tool_call_chunks(&borrowed)];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::ReadFile,
            Arc::new(ConcurrencyProbeHandler {
                active,
                max_active: max_active.clone(),
            }),
        );
        let mut agent = AgentLoop::new(test_agent_config(provider, "read-cap"))
            .with_tools(Arc::new(registry))
            .with_system_prompt_context(SystemPromptContext {
                enable_parallel_tool_calling: true,
                ..Default::default()
            });
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let history = agent.get_conversation_history().await;
        let results = history
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::UserBlocks(blocks) => Some(blocks.len()),
                _ => None,
            })
            .sum::<usize>();
        assert_eq!(results, 20);
        assert!(
            max_active.load(std::sync::atomic::Ordering::SeqCst) <= DEFAULT_TOOL_CONCURRENCY,
            "read batch must stay within the concurrency cap"
        );
    }

    struct FailOnPathHandler {
        fail_marker: &'static str,
    }

    impl crate::core::tools::ToolHandler for FailOnPathHandler {
        fn execute(
            &self,
            _ctx: &ToolContext,
            params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, crate::core::tools::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            let fail_marker = self.fail_marker;
            Box::pin(async move {
                let path = params
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if path.contains(fail_marker) {
                    Err(crate::core::tools::ToolError::ExecutionFailed(format!(
                        "write rejected for {path}"
                    )))
                } else {
                    Ok(serde_json::Value::String(format!("created {path}")))
                }
            })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "fail on path".to_string()
        }
    }

    struct EditStatsHandler;

    impl crate::core::tools::ToolHandler for EditStatsHandler {
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
                Ok(serde_json::Value::String(
                    "Applied 1 edit(s) successfully (+3, -1 lines)".to_string(),
                ))
            })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "edit stats".to_string()
        }
    }

    struct PublicationFailureHandler {
        content_applied: bool,
    }

    impl crate::core::tools::ToolHandler for PublicationFailureHandler {
        fn execute(
            &self,
            _ctx: &ToolContext,
            params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, crate::core::tools::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            let content_applied = self.content_applied;
            Box::pin(async move {
                let path = params
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("partial.txt")
                    .to_string();
                Err(
                    crate::core::tools::ToolError::ExecutionFailedWithPublicationMetadata(
                        "anchor publication failed".to_string(),
                        crate::core::tools::ToolFailureMetadata {
                            class: crate::core::tools::ToolFailureClass::StorageFailure,
                            affected_paths: vec![path.clone()],
                            required_next_step: None,
                        },
                        vec![crate::core::tools::ToolPublicationOutcome {
                            path,
                            content_applied,
                            anchors_published: false,
                        }],
                    ),
                )
            })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "publication failure".to_string()
        }
    }

    fn turn_kind(result: &TurnResult) -> &'static str {
        match result {
            TurnResult::Continue => "continue",
            TurnResult::Complete => "complete",
            TurnResult::Cancelled => "cancelled",
            TurnResult::Error(_) => "error",
        }
    }

    fn tool_result_texts(history: &[StorageMessage]) -> Vec<String> {
        let mut texts = Vec::new();
        for message in history {
            if let MessageContent::UserBlocks(blocks) = &message.content {
                for block in blocks {
                    if let UserContentBlock::ToolResult(result) = block
                        && let ToolResultContent::Text(text) = &result.content
                    {
                        texts.push(text.clone());
                    }
                }
            }
        }
        texts
    }

    fn seed_read_result(history: &mut Vec<StorageMessage>, path: &str, body: &str) {
        use crate::providers::{SharedContentFields, ToolResultBlock};
        history.push(StorageMessage {
            id: Some("seed-read".to_string()),
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                ToolResultBlock {
                    tool_use_id: "seed-call".to_string(),
                    content: ToolResultContent::Text(format!(
                        "[File: {path}, Hash: seedhash] (2)\n{body}"
                    )),
                    shared: SharedContentFields {
                        call_id: Some("seed-call".to_string()),
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: Some(1),
        });
    }

    fn bookkeeping_agent(
        responses: Vec<Vec<ApiStreamChunk>>,
        json_output: bool,
        writer: Option<crate::cli::output::OutputWriterArc>,
    ) -> AgentLoop {
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::WriteToFile,
            Arc::new(FailOnPathHandler {
                fail_marker: "fail",
            }),
        );
        registry.register(SnedTool::EditFile, Arc::new(EditStatsHandler));
        let mut config = test_agent_config(provider, "bookkeeping");
        config.json_output = json_output;
        if let Some(writer) = writer {
            config.output_writer = writer;
        }
        AgentLoop::new(config).with_tools(Arc::new(registry))
    }

    #[tokio::test]
    async fn test_failed_write_not_counted_as_created() {
        use serde_json::json;
        let (tx, mut rx) = mpsc::channel(64);
        let responses = vec![tool_call_chunks(&[
            (
                "w1",
                "write_to_file",
                json!({"path": "good.txt", "content": "ok"}),
            ),
            (
                "w2",
                "write_to_file",
                json!({"path": "fail.txt", "content": "bad"}),
            ),
        ])];
        let mut agent = bookkeeping_agent(
            responses,
            false,
            Some(Arc::new(crate::cli::output::ChannelOutputWriter::new(tx))),
        );
        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));
        let rendered = drain_rendered_output(&mut rx);
        let digest = rendered
            .iter()
            .find(|line| line.contains("file created"))
            .unwrap_or_else(|| panic!("expected a created-files digest, got {rendered:?}"));
        assert!(
            digest.contains("1 file created"),
            "failed write must not count as created, got {rendered:?}"
        );
    }

    #[test]
    fn test_shadow_commit_message_covers_all_mutation_kinds() {
        assert_eq!(AgentLoop::shadow_commit_message(&[], &[], &[]), None);
        assert_eq!(
            AgentLoop::shadow_commit_message(&[], &["new.txt".to_string()], &[]),
            Some("[sned] turn: created new.txt".to_string())
        );
        assert_eq!(
            AgentLoop::shadow_commit_message(&[], &[], &["a.rs".to_string()]),
            Some("[sned] turn: symbols a.rs".to_string())
        );
        // Zero-stat edits commit nothing.
        assert_eq!(
            AgentLoop::shadow_commit_message(&[("e.txt".to_string(), 0, 0)], &[], &[]),
            None
        );
        // Edited files keep the established heat-map summary first.
        let edited = &[("e.txt".to_string(), 2, 1)];
        assert_eq!(
            AgentLoop::shadow_commit_message(edited, &[], &[]),
            Some(format!("[sned] turn: {}", format_heat_map_plain(edited)))
        );
        let mixed =
            AgentLoop::shadow_commit_message(edited, &["n.txt".to_string()], &["s.rs".to_string()])
                .expect("mutations must produce a message");
        assert!(mixed.contains("created n.txt"), "got {mixed}");
        assert!(mixed.contains("symbols s.rs"), "got {mixed}");
    }

    #[tokio::test]
    async fn test_digest_counts_executed_commands() {
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;
        use serde_json::json;
        let (tx, mut rx) = mpsc::channel(64);
        let responses = vec![tool_call_chunks(&[(
            "c1",
            "execute_command",
            json!({"commands": ["echo digest-probe"]}),
        )])];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new()),
        );
        let mut config = test_agent_config(provider, "digest-executed");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let rendered = drain_rendered_output(&mut rx);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("📝") && line.contains("1 command run")),
            "executed command must appear in digest, got {rendered:?}"
        );
    }

    #[tokio::test]
    async fn test_digest_omits_denied_command() {
        use crate::core::approval::ApprovalManager;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;
        use crate::test_support::env_lock;
        use serde_json::json;
        // SAFETY: env mutation is serialized by env_lock; restored below.
        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe { std::env::set_var("SNED_APPROVAL_DENY", "1") };

        let (tx, mut rx) = mpsc::channel(64);
        let responses = vec![tool_call_chunks(&[(
            "c1",
            "execute_command",
            json!({"commands": ["rm -rf /tmp/sned-digest-denied"]}),
        )])];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new()),
        );
        let mut config = test_agent_config(provider, "digest-denied");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let approval_manager = Arc::new(tokio::sync::Mutex::new(ApprovalManager::new()));
        let mut agent = AgentLoop::new(config)
            .with_tools(Arc::new(registry))
            .with_approval_manager(approval_manager);
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let history = agent.get_conversation_history().await;
        assert!(
            tool_result_texts(&history)
                .iter()
                .any(|text| text.contains("was denied")),
            "setup must actually deny the command"
        );
        let rendered = drain_rendered_output(&mut rx);
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("📝") && line.contains("command")),
            "denied command never ran and must stay out of the digest, got {rendered:?}"
        );

        // SAFETY: restoring env after test.
        unsafe { std::env::remove_var("SNED_APPROVAL_DENY") };
    }

    #[test]
    fn test_unknown_tools_fail_closed_to_barrier() {
        // Read-only tools may run inside concurrent read batches...
        for tool in [
            "read_file",
            "search_files",
            "list_files",
            "get_file_skeleton",
            "ask_followup_question",
        ] {
            assert!(
                AgentLoop::tool_is_schedulable_read(tool),
                "{tool} must stay schedulable"
            );
        }
        // ...while mutating, unknown-effect, and unknown tools must not:
        // an unrecognized mutation must never slip into a concurrent batch.
        for tool in [
            "edit_file",
            "write_to_file",
            "replace_symbol",
            "rename_symbol",
            "execute_command",
            "definitely_not_a_tool",
            "",
        ] {
            assert!(
                !AgentLoop::tool_is_schedulable_read(tool),
                "{tool} must fail closed to barrier"
            );
        }
    }

    #[tokio::test]
    async fn test_json_and_tty_modes_share_mutation_bookkeeping() {
        use serde_json::json;
        let batch = || {
            vec![tool_call_chunks(&[
                (
                    "e1",
                    "edit_file",
                    json!({"files": [{"path": "edited.txt", "edits": []}]}),
                ),
                (
                    "w1",
                    "write_to_file",
                    json!({"path": "fail.txt", "content": "bad"}),
                ),
            ])]
        };
        let mut outcomes = Vec::new();
        for json_output in [false, true] {
            let mut agent = bookkeeping_agent(batch(), json_output, None);
            {
                let mut history = agent.conversation_history.lock().await;
                let seeded: Vec<StorageMessage> = Vec::new();
                *history = seeded;
                seed_read_result(&mut history, "edited.txt", "stale body");
            }
            let result = agent.execute_turn().await;
            let history = agent.get_conversation_history().await;
            let mistakes = agent.state.lock().await.consecutive_mistakes;
            outcomes.push((
                turn_kind(&result).to_string(),
                tool_result_texts(&history),
                mistakes,
            ));
        }
        assert_eq!(
            outcomes[0], outcomes[1],
            "JSON and TTY modes must share history and accounting effects"
        );
        assert_eq!(outcomes[0].2, 1);
    }

    #[tokio::test]
    async fn test_publication_failure_without_content_change_not_counted() {
        use serde_json::json;
        let (tx, mut rx) = mpsc::channel(64);
        let responses = vec![tool_call_chunks(&[(
            "w1",
            "write_to_file",
            json!({"path": "partial.txt", "content": "new"}),
        )])];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::WriteToFile,
            Arc::new(PublicationFailureHandler {
                content_applied: false,
            }),
        );
        let mut config = test_agent_config(provider, "publication-no-change");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let rendered = drain_rendered_output(&mut rx);
        assert!(
            !rendered.iter().any(|line| line.contains("file created")),
            "unchanged content must not count as created, got {rendered:?}"
        );
    }

    #[tokio::test]
    async fn test_content_applied_publication_failure_counts_as_created() {
        use serde_json::json;
        let (tx, mut rx) = mpsc::channel(64);
        let responses = vec![tool_call_chunks(&[(
            "w1",
            "write_to_file",
            json!({"path": "partial.txt", "content": "new"}),
        )])];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::WriteToFile,
            Arc::new(PublicationFailureHandler {
                content_applied: true,
            }),
        );
        let mut config = test_agent_config(provider, "publication-applied");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        assert_eq!(agent.state.lock().await.consecutive_mistakes, 1);
        let rendered = drain_rendered_output(&mut rx);
        assert!(
            rendered.iter().any(|line| line.contains("1 file created")),
            "applied content must count as created despite publication failure, got {rendered:?}"
        );
    }

    // The blocking metadata lock must not stall the runtime: a
    // current-thread runtime would freeze its own timer behind the
    // blocked worker, so this test needs multiple workers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_delayed_metadata_keeps_state_available() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let sned_dir = temp_dir.path().join(".sned");
        let task_id = "delayed-metadata";
        let task_storage =
            crate::storage::task_storage::TaskStorage::new_with_dir(task_id, &sned_dir).unwrap();
        let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
            vec![],
        )));
        let agent =
            AgentLoop::new(test_agent_config(provider, task_id)).with_task_storage(task_storage);

        // Hold the task-directory lock from another thread so the metadata
        // write inside the save blocks like slow disk access would.
        let locker =
            crate::storage::task_storage::TaskStorage::new_with_dir(task_id, &sned_dir).unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let lock_thread = std::thread::spawn(move || {
            let _ = locker.with_lock(|| {
                let _ = held_tx.send(());
                std::thread::sleep(std::time::Duration::from_secs(3));
                Ok::<(), std::io::Error>(())
            });
        });
        held_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("metadata lock must be held before saving");

        // Prove the holder really owns the task lock the save will take.
        let lock_path = sned_dir
            .join("data")
            .join("tasks")
            .join(task_id)
            .join(".lock");
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("task lock file must exist");
        assert!(
            lock_file.try_lock().is_err(),
            "task lock must be held by the background thread"
        );

        let state = agent.state.clone();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let save = tokio::spawn(async move {
            agent.save_conversation_history().await;
            let _ = done_tx.send(());
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        match done_rx.try_recv() {
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                panic!("save task ended without completing")
            }
            Ok(()) => panic!("save completed instead of blocking on the held lock"),
        }
        // State readers and the UI must not stall behind the blocked write.
        // This try_lock fails while the state guard is held across the
        // blocking metadata update.
        assert!(
            state.try_lock().is_ok(),
            "task state must stay available while metadata persistence is delayed"
        );
        save.await
            .expect("save must finish once the metadata lock is released");
        lock_thread.join().expect("lock holder must exit");
    }

    #[tokio::test]
    async fn test_denied_malformed_edit_preserves_unrelated_read_warnings() {
        use serde_json::json;
        let responses = vec![tool_call_chunks(&[
            (
                "bad-args",
                "edit_file",
                serde_json::Value::String("{oops".to_string()),
            ),
            (
                "denied",
                "edit_file",
                json!({"files": [{"path": "b.txt", "edits": []}]}),
            ),
        ])];
        // Malformed arguments never parse, so build that call directly.
        let mut first = responses.into_iter().next().unwrap();
        first[0] = ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("bad-args".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("edit_file".to_string()),
                    arguments: Some("{oops".to_string()),
                },
                signature: None,
            },
            id: None,
            signature: None,
        });
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                vec![first],
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(SnedTool::EditFile, Arc::new(EditStatsHandler));
        let mut config = test_agent_config(provider, "read-warning-retention");
        config.mode = AgentMode::Plan;
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));
        {
            let mut state = agent.state.lock().await;
            state.turns_completed = 5;
            state.consecutive_reads.insert("a.txt".to_string(), 3);
            state.last_read_turn.insert("a.txt".to_string(), 4);
            let mut ring = std::collections::VecDeque::new();
            ring.push_back((10, 20));
            state.recent_read_windows.insert("a.txt".to_string(), ring);
        }
        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));
        // Both calls fail before execution, so the turn counts a mistake.
        assert_eq!(agent.state.lock().await.consecutive_mistakes, 1);
        let state = agent.state.lock().await;
        assert_eq!(state.consecutive_reads.get("a.txt"), Some(&3));
        assert_eq!(state.last_read_turn.get("a.txt"), Some(&4));
        assert_eq!(
            state.recent_read_windows.get("a.txt").map(|r| r.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn test_successful_edit_invalidates_only_edited_paths() {
        use serde_json::json;
        let responses = vec![tool_call_chunks(&[(
            "e1",
            "edit_file",
            json!({"files": [{"path": "b.txt", "edits": []}]}),
        )])];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(SnedTool::EditFile, Arc::new(EditStatsHandler));
        let mut agent = AgentLoop::new(test_agent_config(provider, "read-warning-invalidate"))
            .with_tools(Arc::new(registry));
        {
            let mut state = agent.state.lock().await;
            state.turns_completed = 5;
            state.consecutive_reads.insert("a.txt".to_string(), 3);
            state.consecutive_reads.insert("b.txt".to_string(), 2);
            state.last_read_turn.insert("a.txt".to_string(), 4);
            state.last_read_turn.insert("b.txt".to_string(), 4);
        }
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let state = agent.state.lock().await;
        assert_eq!(state.consecutive_reads.get("a.txt"), Some(&3));
        assert!(!state.consecutive_reads.contains_key("b.txt"));
        assert!(state.last_read_turn.contains_key("a.txt"));
        assert!(!state.last_read_turn.contains_key("b.txt"));
    }

    #[tokio::test]
    async fn test_strict_plan_mode_flows_from_config_into_state() {
        for enabled in [true, false] {
            let provider = Arc::new(Providers::Mock(crate::providers::mock::MockProvider::new(
                vec![],
            )));
            let mut config = test_agent_config(provider, "strict-plan-new");
            config.strict_plan_mode_enabled = enabled;
            let agent = AgentLoop::new(config);
            assert_eq!(
                agent.state.lock().await.strict_plan_mode_enabled,
                enabled,
                "constructor must copy strict-plan mode into state"
            );
        }
    }

    #[tokio::test]
    async fn test_run_startup_applies_configured_strict_plan_mode() {
        use crate::test_support::env_lock;

        for enabled in [true, false] {
            let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
            let temp_dir = tempfile::tempdir().unwrap();
            let data_dir = temp_dir.path().join("data");
            std::fs::create_dir_all(data_dir.join("state")).unwrap();
            std::fs::create_dir_all(data_dir.join("settings")).unwrap();
            let old_sned_dir = std::env::var_os("SNED_DIR");
            // SAFETY: env_lock serializes process-environment mutation.
            unsafe {
                std::env::set_var("SNED_DIR", temp_dir.path());
            }
            let provider = Arc::new(Providers::Mock(
                crate::providers::mock::MockProvider::single_text_response("should not run"),
            ));
            let mut config = test_agent_config(provider, "strict-plan-startup");
            config.strict_plan_mode_enabled = enabled;
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
            assert!(result.is_ok());
            assert_eq!(
                agent.state.lock().await.strict_plan_mode_enabled,
                enabled,
                "startup must apply the configured strict-plan mode"
            );
            // SAFETY: restore the process environment for later tests.
            unsafe {
                match old_sned_dir {
                    Some(ref value) => std::env::set_var("SNED_DIR", value),
                    None => std::env::remove_var("SNED_DIR"),
                }
            }
        }
    }

    #[tokio::test]
    async fn test_execution_wrapper_preserves_publication_outcomes() {
        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_text_response("unused"),
        ));
        let config = test_agent_config(provider, "publication-boundary");
        let state = Arc::new(Mutex::new(TaskState::default()));
        let context = Arc::new(ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            false,
            "publication-boundary".to_string(),
            None,
            true,
            Arc::new(crate::cli::output::StderrOutputWriter),
            false,
        ));

        let output = AgentLoop::execute_tool_with_hooks_internal(
            &config,
            None,
            context,
            "write_to_file",
            &serde_json::json!({"path": "partial.txt", "content": "new"}),
            Arc::new(PublicationFailureHandler {
                content_applied: true,
            }),
            None,
            Arc::new(Mutex::new(Vec::new())),
        )
        .await;

        assert!(output.is_error);
        assert_eq!(
            output.publication_outcomes,
            vec![crate::core::tools::ToolPublicationOutcome {
                path: "partial.txt".to_string(),
                content_applied: true,
                anchors_published: false,
            }]
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
            if let OutputEvent::ApprovalRequested(request) = event.event {
                break request;
            }
        };

        let second_request = loop {
            let event = timeout(Duration::from_secs(2), approval_rx.recv())
                .await
                .expect("second approval prompt should arrive after timeout")
                .expect("priority output should stay open");
            if let OutputEvent::ApprovalRequested(request) = event.event {
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
        assert_eq!(
            agent.state.lock().await.consecutive_mistakes,
            1,
            "the timed-out approval must count as a tool failure"
        );
    }

    fn drain_rendered_output(
        rx: &mut tokio::sync::mpsc::Receiver<crate::cli::output::SequencedOutputEvent>,
    ) -> Vec<String> {
        let mut rendered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event.event {
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
        priority_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
            crate::cli::output::SequencedOutputEvent,
        >,
        rx: &mut tokio::sync::mpsc::Receiver<crate::cli::output::SequencedOutputEvent>,
    ) -> Vec<OutputEvent> {
        let mut events = Vec::new();
        while let Ok(event) = priority_rx.try_recv() {
            events.push(event.event);
        }
        while let Ok(event) = rx.try_recv() {
            events.push(event.event);
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

    /// Read-loop decay: inspection tools MUST keep state alive. The
    /// live-log regression on SDRSkeleton was caused by the blanket
    /// `tool_name != "read_file"` wipe silently resetting
    /// `consecutive_reads` on every `execute_command` call — so the
    /// detector never tripped even after 9 narrow reads interleaved
    /// with shell commands.
    #[test]
    fn decay_read_loop_state_keeps_state_for_inspection_tools() {
        let mut state = TaskState::default();
        state.turns_completed = 5;
        state
            .consecutive_reads
            .insert("/tmp/foo.c".to_string(), 3);
        state
            .last_read_turn
            .insert("/tmp/foo.c".to_string(), 4);
        let mut ring = std::collections::VecDeque::new();
        ring.push_back((370, 410));
        ring.push_back((380, 403));
        ring.push_back((340, 400));
        state
            .recent_read_windows
            .insert("/tmp/foo.c".to_string(), ring);
        for tool in &[
            "execute_command",
            "search_files",
            "list_files",
            "get_function",
            "get_file_skeleton",
            "find_symbol_references",
            "diagnostics_scan",
            "web_fetch",
            "condense",
            "read_file",
        ] {
            AgentLoop::decay_read_loop_state(&mut state, tool);
            assert_eq!(
                state.consecutive_reads.get("/tmp/foo.c").copied(),
                Some(3),
                "{tool} must preserve consecutive_reads, got: {:?}",
                state.consecutive_reads
            );
            assert_eq!(
                state.last_read_turn.get("/tmp/foo.c").copied(),
                Some(4),
                "{tool} must preserve last_read_turn, got: {:?}",
                state.last_read_turn
            );
            assert_eq!(
                state
                    .recent_read_windows
                    .get("/tmp/foo.c")
                    .map(|r| r.len()),
                Some(3),
                "{tool} must preserve recent_read_windows, got: {:?}",
                state.recent_read_windows
            );
        }
    }

    /// Read-loop decay: mutating tools MUST reset state because the
    /// file's content changes (or the model's plan must pivot on
    /// failure). A successful edit resets the file; a failed edit
    /// resets the model's mental model.
    #[test]
    fn decay_read_loop_state_clears_state_for_mutating_tools() {
        for tool in &["edit_file", "write_to_file", "replace_symbol", "rename_symbol"] {
            let mut state = TaskState::default();
            state.turns_completed = 5;
            state
                .consecutive_reads
                .insert("/tmp/foo.c".to_string(), 3);
            state
                .last_read_turn
                .insert("/tmp/foo.c".to_string(), 4);
            let mut ring = std::collections::VecDeque::new();
            ring.push_back((370, 410));
            state
                .recent_read_windows
                .insert("/tmp/foo.c".to_string(), ring);
            AgentLoop::decay_read_loop_state(&mut state, tool);
            assert!(
                state.consecutive_reads.is_empty(),
                "{tool} must clear consecutive_reads, got: {:?}",
                state.consecutive_reads
            );
            assert!(
                state.last_read_turn.is_empty(),
                "{tool} must clear last_read_turn, got: {:?}",
                state.last_read_turn
            );
            assert!(
                state.recent_read_windows.is_empty(),
                "{tool} must clear recent_read_windows, got: {:?}",
                state.recent_read_windows
            );
        }
    }

    /// Read-loop decay: unknown tools apply a 2-turn cooldown. If the
    /// model hasn't touched any tracked file for 2 turns (the gap
    /// between `current_turn` and the latest `last_read_turn`), the
    /// detector forgets the file. Without this, the detector would
    /// resurrect stale counts from a previous investigation phase when
    /// the model finally returns to the same file in a new task.
    #[test]
    fn decay_read_loop_state_applies_cooldown_for_unknown_tools() {
        let mut state = TaskState::default();
        state.turns_completed = 5;
        state
            .consecutive_reads
            .insert("/tmp/foo.c".to_string(), 3);
        state
            .last_read_turn
            .insert("/tmp/foo.c".to_string(), 4);
        // 1-turn gap (current=5, last=4) — must NOT clear.
        AgentLoop::decay_read_loop_state(&mut state, "ask_followup_question");
        assert_eq!(
            state.consecutive_reads.get("/tmp/foo.c").copied(),
            Some(3),
            "1-turn gap must preserve state, got: {:?}",
            state.consecutive_reads
        );
        // Push past the 2-turn window — must clear.
        state
            .last_read_turn
            .insert("/tmp/foo.c".to_string(), 0);
        AgentLoop::decay_read_loop_state(&mut state, "ask_followup_question");
        assert!(
            state.consecutive_reads.is_empty(),
            "5+ turn gap must clear, got: {:?}",
            state.consecutive_reads
        );

        state.turns_completed = 20;
        state
            .consecutive_reads
            .insert("/tmp/active.c".to_string(), 4);
        state.last_read_turn.insert("/tmp/active.c".to_string(), 19);
        state
            .consecutive_reads
            .insert("/tmp/abandoned.c".to_string(), 4);
        state.last_read_turn.insert("/tmp/abandoned.c".to_string(), 1);
        AgentLoop::decay_read_loop_state(&mut state, "ask_followup_question");
        assert!(state.consecutive_reads.contains_key("/tmp/active.c"));
        assert!(!state.consecutive_reads.contains_key("/tmp/abandoned.c"));
    }

    /// End-to-end shape: alternating read_file + execute_command (the
    /// SDRSkeleton failure mode) now accumulates count=3 across 3
    /// turns and reaches the read-loop warning threshold. Without the
    /// inspection-tool allowlist this would still be 1.
    #[test]
    fn decay_read_loop_state_alternating_read_execute_accumulates() {
        let mut state = TaskState::default();
        state.turns_completed = 1;
        for turn in 1..=3u32 {
            // Simulate: read_file increments count, then a shell grep
            // follows without wiping.
            state.turns_completed = turn;
            state
                .consecutive_reads
                .insert("/tmp/foo.c".to_string(), turn);
            state
                .last_read_turn
                .insert("/tmp/foo.c".to_string(), turn);
            AgentLoop::decay_read_loop_state(&mut state, "execute_command");
        }
        assert_eq!(
            state.consecutive_reads.get("/tmp/foo.c").copied(),
            Some(3),
            "interleaved execute_command must not wipe, got: {:?}",
            state.consecutive_reads
        );
    }

    #[tokio::test]
    async fn invalidate_changed_read_state_preserves_unchanged_paths() {
        let directory = tempfile::tempdir().unwrap();
        let unchanged = directory.path().join("unchanged.rs");
        let changed = directory.path().join("changed.rs");
        std::fs::write(&unchanged, "same\n").unwrap();
        std::fs::write(&changed, "before\n").unwrap();
        let unchanged_key = unchanged.to_string_lossy().into_owned();
        let changed_key = changed.to_string_lossy().into_owned();
        let unchanged_metadata = std::fs::metadata(&unchanged).unwrap();
        let changed_metadata = std::fs::metadata(&changed).unwrap();
        let state = Arc::new(Mutex::new(TaskState::default()));
        {
            let mut guard = state.lock().await;
            for key in [&unchanged_key, &changed_key] {
                guard.consecutive_reads.insert(key.clone(), 3);
                guard.last_read_turn.insert(key.clone(), 2);
                guard
                    .recent_read_windows
                    .insert(key.clone(), std::collections::VecDeque::from([(1, 10)]));
            }
            guard.read_file_snapshots.insert(
                unchanged_key.clone(),
                (unchanged_metadata.len(), unchanged_metadata.modified().ok()),
            );
            guard.read_file_snapshots.insert(
                changed_key.clone(),
                (changed_metadata.len(), changed_metadata.modified().ok()),
            );
        }
        std::fs::write(&changed, "after external mutation\n").unwrap();

        AgentLoop::invalidate_changed_read_state(&state).await;

        let guard = state.lock().await;
        assert!(guard.consecutive_reads.contains_key(&unchanged_key));
        assert!(!guard.consecutive_reads.contains_key(&changed_key));
        assert!(guard.read_file_snapshots.contains_key(&unchanged_key));
        assert!(!guard.read_file_snapshots.contains_key(&changed_key));
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
            match event.event {
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
            Ok(event) if matches!(event.event, OutputEvent::Line(_)) => {
                let OutputEvent::Line(line) = event.event else {
                    unreachable!()
                };
                line.to_string()
            }
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
            span.content == "`code`" && span.style.fg == Some(crate::cli::tui::theme::prompt_fg())
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
            Some(crate::cli::tui::theme::accent())
        );

        let block = streaming_model_line("---".to_string(), true);
        assert_eq!(block.to_string(), "---");
        assert_eq!(
            block.spans[0].style.fg,
            Some(crate::cli::tui::theme::accent())
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
            Ok(event) if matches!(event.event, OutputEvent::ModelUpdateLine(_)) => {
                let OutputEvent::ModelUpdateLine(line) = event.event else {
                    unreachable!()
                };
                line
            }
            other => panic!("expected raw partial update, got {other:?}"),
        };
        assert_eq!(partial.to_string(), "**bol");

        let completed = match rx.try_recv() {
            Ok(event) if matches!(event.event, OutputEvent::ModelUpdateLine(_)) => {
                let OutputEvent::ModelUpdateLine(line) = event.event else {
                    unreachable!()
                };
                line
            }
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
        assert_eq!(agent.state.lock().await.consecutive_mistakes, 0);
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
            |event| matches!(event.event, OutputEvent::Line(ref line) if line.to_string().contains("partial output"))
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event.event, OutputEvent::Line(ref line) if line.to_string().contains("[sned] ERROR: Provider stream error: OpenAI SSE stream error: error decoding response body (retryable)")))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, OutputEvent::Line(ref line) if line.to_string().contains("should not be used")))
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
            Ok(event) if matches!(event.event, OutputEvent::Line(ref line) if line.to_string() == "[sned] Cancelled. Type /retry to resend.")
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
    async fn test_run_observes_cancellation_while_plan_stays_paused() {
        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_text_response("SENTINEL_NOT_CONSUMED"),
        ));
        let (tx, mut rx) = mpsc::channel(8);
        let mut config = test_agent_config(provider, "test-run-paused-plan-cancel");
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

        {
            let mut state = state_handle.lock().await;
            assert!(state.plan_state.as_ref().is_some_and(|plan| plan.paused));
            state.is_cancelled = true;
            state
                .is_cancelled_atomic
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), run)
            .await
            .expect("cancelled run must exit while the plan stays paused")
            .expect("paused plan task should not panic");
        assert!(result.is_ok());
        let rendered = drain_rendered_output(&mut rx);
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("SENTINEL_NOT_CONSUMED")),
            "no provider turn may run after cancellation"
        );
    }

    struct CancelProbeHandler {
        started: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::core::tools::ToolHandler for CancelProbeHandler {
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
            let started = self.started.clone();
            Box::pin(async move {
                started.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(serde_json::json!("handler ran"))
            })
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "cancel probe".to_string()
        }
    }

    #[tokio::test]
    async fn test_cancel_during_pending_approval_starts_no_tool() {
        use crate::core::approval::{ApprovalManager, ApprovalResult};
        use crate::core::tools::ToolRegistry;
        use crate::test_support::env_lock;
        use tokio::time::{Duration, timeout};

        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let _approval_guard = crate::core::approval::approval_test_guard();
        let _input_override = crate::core::approval::override_approval_input_for_test();

        let responses = vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("call_1".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("write_to_file".to_string()),
                    arguments: Some(
                        serde_json::json!({"path": "cancelled.txt", "content": "must not run"})
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
        let (tx, _rx) = mpsc::channel(32);
        let writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut approval_rx = writer
            .take_approval_rx()
            .expect("approval output receiver should be available");
        let mut config = test_agent_config(provider, "test-cancel-during-approval");
        config.output_writer = writer;

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(
            SnedTool::WriteToFile,
            Arc::new(CancelProbeHandler {
                started: Arc::clone(&started),
            }),
        );
        let approval_manager = Arc::new(tokio::sync::Mutex::new(ApprovalManager::new()));
        let mut agent = AgentLoop::new(config)
            .with_tools(Arc::new(registry))
            .with_approval_manager(approval_manager);
        let state_handle = Arc::clone(&agent.state);

        let turn = tokio::spawn(async move {
            let result = agent.execute_turn().await;
            (agent, result)
        });

        let request = loop {
            let event = timeout(Duration::from_secs(2), approval_rx.recv())
                .await
                .expect("approval prompt should arrive")
                .expect("priority output should stay open");
            if let OutputEvent::ApprovalRequested(request) = event.event {
                break request;
            }
        };

        {
            let mut state = state_handle.lock().await;
            state.is_cancelled = true;
            state
                .is_cancelled_atomic
                .store(true, std::sync::atomic::Ordering::Release);
        }
        assert!(request.respond(ApprovalResult::Approved));

        let (agent, result) = timeout(Duration::from_secs(5), turn)
            .await
            .expect("tool batch should finish")
            .expect("agent task should not panic");
        assert!(matches!(result, TurnResult::Continue));
        assert!(
            !started.load(std::sync::atomic::Ordering::SeqCst),
            "no unstarted tool may begin after cancellation"
        );
        let history = agent.conversation_history.lock().await;
        let skipped = history
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
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].contains("cancelled"),
            "the skipped tool must record cancellation, not success"
        );
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
            matches!(event.event, OutputEvent::Line(ref line) if line.to_string().contains("[snipped from streamed display; use /full]"))
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event.event,
                OutputEvent::TurnEnd {
                    ref accumulated_text,
                    ..
                }
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

    #[test]
    fn existing_deleted_range_does_not_discard_new_read_coverage() {
        use crate::core::context::context_manager;
        use crate::providers::{MessageContent, MessageRole, StorageMessage};

        let history = (0..12)
            .map(|index| StorageMessage {
                id: None,
                role: if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: MessageContent::Text(format!("Message {index}")),
                model_info: None,
                metrics: None,
                ts: None,
            })
            .collect::<Vec<_>>();
        let deleted_range = Some((2, 5));
        let result = context_manager::get_new_context_messages_and_metadata(
            &history,
            None,
            deleted_range,
            false,
            None,
            "openai",
        );

        assert!(result.truncated_conversation_history.len() < history.len());
        assert_eq!(result.conversation_history_deleted_range, deleted_range);
        assert!(!AgentLoop::new_history_was_discarded(
            deleted_range,
            result.conversation_history_deleted_range,
            history.len(),
            result.truncated_conversation_history.len(),
        ));
        assert!(AgentLoop::new_history_was_discarded(
            deleted_range,
            Some((2, 7)),
            history.len(),
            result.truncated_conversation_history.len(),
        ));
    }

    #[test]
    fn context_reduction_clears_read_warnings_with_coverage() {
        let mut state = TaskState::default();
        let path = "/tmp/covered.swift".to_string();
        state.consecutive_reads.insert(path.clone(), 3);
        state.last_read_turn.insert(path.clone(), 4);
        state
            .recent_read_windows
            .insert(path.clone(), std::collections::VecDeque::from([(1, 100)]));
        state.read_file_snapshots.insert(path.clone(), (100, None));

        AgentLoop::clear_history_dependent_read_state(&mut state);

        assert!(state.consecutive_reads.is_empty());
        assert!(state.last_read_turn.is_empty());
        assert!(state.recent_read_windows.is_empty());
        assert!(state.read_file_snapshots.contains_key(&path));
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

    #[tokio::test]
    async fn test_resume_remaps_deleted_range_across_recovery_drop() {
        use crate::test_support::env_lock;
        use std::env;
        use tempfile::TempDir;

        let task_id = "resume-remap-range";
        // Build storage handles under the env lock, then restore the
        // environment before any await: nothing below re-reads SNED_DIR,
        // so no guard is held across await points.
        let (_temp_dir, task_storage) = {
            let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
            let previous_sned_dir = env::var_os("SNED_DIR");
            let temp_dir = TempDir::new().unwrap();
            // SAFETY: exclusive access via env_lock; restored below.
            unsafe {
                env::set_var("SNED_DIR", temp_dir.path().join(".sned"));
            }
            let task_storage = TaskStorage::new(task_id).unwrap();
            // SAFETY: exclusive access via env_lock; restoring prior value.
            unsafe {
                match previous_sned_dir {
                    Some(value) => env::set_var("SNED_DIR", value),
                    None => env::remove_var("SNED_DIR"),
                }
            }
            (temp_dir, task_storage)
        };

        // File index 1 holds a dangling tool use with no matching result,
        // so repair drops it: survivors shift down by one.
        let messages = vec![
            StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("first instruction".to_string()),
                model_info: None,
                metrics: None,
                ts: Some(1000),
            },
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                    ToolUseBlock {
                        id: "orphan".to_string(),
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
                ts: Some(1001),
            },
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text("assistant reply".to_string()),
                model_info: None,
                metrics: None,
                ts: Some(1002),
            },
            StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("Do not modify any files".to_string()),
                model_info: None,
                metrics: None,
                ts: Some(1003),
            },
        ];
        task_storage
            .write_api_conversation_history(&messages)
            .unwrap();

        // Saved pre-recovery coordinates covering file indices 2..4.
        let state_manager = Arc::new(StateManager::new().unwrap());
        state_manager.add_task_to_history(HistoryItem {
            id: task_id.to_string(),
            ulid: Some(task_id.to_string()),
            number: 0,
            ts: 0,
            task: "resume".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cache_writes: None,
            cache_reads: None,
            total_cost: 0.0,
            size: None,
            shadow_git_config_work_tree: None,
            cwd_on_task_initialization: None,
            conversation_history_deleted_range: Some(vec![2, 4]),
            is_favorited: None,
            workspace_root_path: None,
            checkpoint_manager_error_message: None,
            model_id: None,
        });

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
        let mut agent = AgentLoop::new(config).with_task_storage(task_storage);
        agent.state_manager = Some(state_manager);

        assert!(agent.load_conversation_history().await);

        let history = agent.get_conversation_history().await;
        assert_eq!(history.len(), 3, "dangling tool use is repaired away");
        let MessageContent::Text(retained) = &history[2].content else {
            panic!("later user instruction must survive recovery");
        };
        assert_eq!(retained, "Do not modify any files");

        // File range (2, 4) must follow the survivors down by the one
        // dropped record instead of pointing past the shortened vector.
        assert_eq!(
            agent.state.lock().await.conversation_history_deleted_range,
            Some((1, 3)),
            "saved range must be remapped against recovery lineage"
        );
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
    fn test_parse_tool_arguments_rejects_oversized_arguments() {
        use crate::providers::MAX_TOOL_ARGUMENT_SIZE;
        let oversized = format!(
            "{{\"content\": \"{}\"}}",
            "x".repeat(MAX_TOOL_ARGUMENT_SIZE)
        );
        assert!(oversized.len() > MAX_TOOL_ARGUMENT_SIZE);
        let error = AgentLoop::parse_tool_arguments("write_to_file", "big-1", Some(&oversized))
            .expect_err("oversized arguments must never reach a tool handler");
        assert!(error.contains("exceed"));
        assert!(error.contains("big-1"));
    }

    #[test]
    fn test_parse_tool_arguments_accepts_arguments_at_size_limit() {
        use crate::providers::MAX_TOOL_ARGUMENT_SIZE;
        let at_limit = format!("{{\"a\":\"{}\"}}", "x".repeat(MAX_TOOL_ARGUMENT_SIZE - 8));
        assert_eq!(at_limit.len(), MAX_TOOL_ARGUMENT_SIZE);
        AgentLoop::parse_tool_arguments("write_to_file", "edge-1", Some(&at_limit))
            .expect("arguments exactly at the limit must still dispatch");
    }

    #[test]
    fn test_assistant_tool_input_bounds_oversized_raw_arguments() {
        use crate::providers::MAX_TOOL_ARGUMENT_SIZE;
        let oversized = format!(
            "{{\"content\": \"{}\"}}",
            "x".repeat(MAX_TOOL_ARGUMENT_SIZE)
        );
        let prepared = PreparedToolCall {
            tool_call: ApiStreamToolCall {
                call_id: Some("call-big".to_string()),
                function: crate::providers::ApiStreamToolCallFunction {
                    id: Some("tool-big".to_string()),
                    name: Some("write_to_file".to_string()),
                    arguments: Some(oversized),
                },
                signature: None,
            },
            tool_id: "tool-big".to_string(),
            tool_name: "write_to_file".to_string(),
            parsed_args: Err("oversized".to_string()),
        };
        let input = AgentLoop::assistant_tool_input(&prepared);
        let raw = input
            .get("_raw_arguments")
            .and_then(serde_json::Value::as_str)
            .expect("failure input keeps a raw prefix for diagnostics");
        assert!(
            raw.len() < 1000,
            "raw prefix must stay bounded, got {} bytes",
            raw.len()
        );
        assert!(raw.contains("truncated"));
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

    #[test]
    fn test_prepared_tool_call_normalizes_stringified_command_array() {
        let raw = serde_json::json!({
            "commands": r#"["awk 'NR>=115 && NR<=125 {print NR": "}' file.swift"]"#
        })
        .to_string();
        let parsed = AgentLoop::parse_tool_arguments("execute_command", "command-1", Some(&raw))
            .expect("recoverable commands should normalize before dispatch");
        assert_eq!(
            parsed,
            serde_json::json!({
                "commands": [r#"awk 'NR>=115 && NR<=125 {print NR": "}' file.swift"#]
            })
        );
    }

    #[test]
    fn test_prepared_tool_call_normalizes_stringified_edit_files() {
        let raw = serde_json::json!({
            "files": r#"[{"edits":[{"anchor":"one§old","text":"new"},"path":"src/main.rs"}]"#
        })
        .to_string();
        let parsed = AgentLoop::parse_tool_arguments("edit_file", "edit-1", Some(&raw))
            .expect("recoverable edit files should normalize before dispatch");
        assert_eq!(parsed["files"][0]["path"], "src/main.rs");
        assert_eq!(parsed["files"][0]["edits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_prepared_tool_call_normalizes_stringified_edit_paths_alias() {
        let raw = serde_json::json!({
            "paths": r#"[{"path":"src/main.rs","edits":[{"anchor":"Word§old","text":"new"}]}]"#
        })
        .to_string();
        let parsed = AgentLoop::parse_tool_arguments("edit_file", "edit-2", Some(&raw))
            .expect("unambiguous edit paths should normalize before dispatch");
        assert_eq!(parsed["files"][0]["path"], "src/main.rs");
        assert!(parsed.get("paths").is_none());
    }

    #[tokio::test]
    async fn test_multiline_edit_error_emits_actionable_tool_output() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::edit_file::EditFileHandler;

        let provider = Arc::new(Providers::Mock(
            crate::providers::mock::MockProvider::single_tool_call(
                "call_bad_edit",
                "edit_file",
                serde_json::json!({"files": [{"path": "src/core/tool_output.rs", "edits": [{
                    "anchor": "First§\nLast§last", "text": ""
                }]}]}),
            ),
        ));
        let (tx, mut rx) = mpsc::channel(64);
        let writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));
        let mut priority_rx = writer.take_priority_rx().unwrap();
        let mut config = test_agent_config(provider, "test-multiline-edit-output");
        config.output_writer = writer;
        let mut registry = ToolRegistry::new();
        registry.register(SnedTool::EditFile, Arc::new(EditFileHandler::new()));
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));
        assert!(matches!(agent.execute_turn().await, TurnResult::Continue));
        let rendered = drain_output_events(&mut priority_rx, &mut rx)
            .iter()
            .filter_map(|event| match event {
                OutputEvent::ToolOutputLine(line) => Some(line.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("does not match consecutive current Word§source lines"),
            "{rendered}"
        );
        assert!(rendered.contains("Read the changed range again"), "{rendered}");
        assert!(!agent.state.lock().await.must_reread_before_edit.is_empty());
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

        let _openai_env_lock = crate::providers::openai::OPENAI_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
            if let crate::cli::output::OutputEvent::TurnEnd {
                accumulated_text, ..
            } = event.event
            {
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
    fn test_truncate_old_thinking_blocks_preserves_utf8_boundaries() {
        let mut thinking = "界".repeat(4);
        truncate_thinking_text(&mut thinking, 2);

        assert_eq!(thinking.chars().take_while(|ch| *ch == '界').count(), 2);
        assert!(thinking.ends_with("\n\n[truncated]"));

        let mut short = "界".to_string();
        truncate_thinking_text(&mut short, usize::MAX);
        assert_eq!(short, "界");
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

    #[test]
    fn test_compact_old_tool_results_preserves_recent_reads() {
        let make_read_msg = |file: &str, size: usize| StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                crate::providers::ToolResultBlock {
                    tool_use_id: format!("call_{file}"),
                    content: crate::providers::ToolResultContent::Text(format!(
                        "[File: {file}, Hash: abc12345] (100 lines total)\n[Anchors: ...]\n{}",
                        "x".repeat(size)
                    )),
                    shared: crate::providers::SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: None,
        };

        let mut history = vec![
            make_read_msg("file1.rs", 5000),
            make_read_msg("file2.rs", 5000),
            make_read_msg("file3.rs", 5000),
        ];

        compact_old_tool_results(&mut history);

        // Most recent two reads (file2 and file3) must be preserved in full
        let get_text = |msg: &StorageMessage| match &msg.content {
            MessageContent::UserBlocks(blocks) => match &blocks[0] {
                UserContentBlock::ToolResult(tr) => match &tr.content {
                    ToolResultContent::Text(t) => t.clone(),
                    _ => panic!("Expected text"),
                },
                _ => panic!("Expected ToolResult"),
            },
            _ => panic!("Expected UserBlocks"),
        };

        assert!(
            get_text(&history[2]).contains(&"x".repeat(5000)),
            "Most recent read must not be compacted"
        );
        assert!(
            get_text(&history[1]).contains(&"x".repeat(5000)),
            "Second most recent read must not be compacted"
        );

        let file1_text = get_text(&history[0]);
        assert!(
            !file1_text.contains(&"x".repeat(5000)),
            "Oldest read should be compacted"
        );
        assert!(file1_text.starts_with("[File: file1.rs, Hash: abc12345] (100 lines total)"));
        assert!(file1_text.contains("Earlier read content"));
    }

    #[test]
    fn test_compact_old_tool_results_drops_aged_anchors() {
        let anchored_body: String = (1..=80)
            .map(|n| format!("{n}: ABC{n:04}X§line content number {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let make_old_read = |file: &str| StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                crate::providers::ToolResultBlock {
                    tool_use_id: format!("call_{file}"),
                    content: crate::providers::ToolResultContent::Text(format!(
                        "[File: {file}, Hash: abc12345] (85 lines total)\n[Anchors: ...]\n{anchored_body}"
                    )),
                    shared: crate::providers::SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: None,
        };

        let mut history = vec![
            make_old_read("old1.rs"),
            make_old_read("old2.rs"),
            make_old_read("old3.rs"),
        ];

        compact_old_tool_results(&mut history);

        let get_text = |msg: &StorageMessage| match &msg.content {
            MessageContent::UserBlocks(blocks) => match &blocks[0] {
                UserContentBlock::ToolResult(tr) => match &tr.content {
                    ToolResultContent::Text(t) => t.clone(),
                    _ => panic!("Expected text"),
                },
                _ => panic!("Expected ToolResult"),
            },
            _ => panic!("Expected UserBlocks"),
        };

        let aged = get_text(&history[0]);
        assert!(aged.contains("Earlier read content"));
        assert!(
            aged.len() < 1000,
            "aged read must collapse near header size, got {} bytes",
            aged.len()
        );
        assert!(
            get_text(&history[2]).contains("line content number 80"),
            "most recent read must keep full content"
        );
    }

    #[test]
    fn test_compact_old_tool_results_collapses_pruned_rereads() {
        let anchored_body: String = (1..=80)
            .map(|n| format!("{n}: ABC{n:04}X§line content number {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let make_pruned_read = |hash: &str| StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                crate::providers::ToolResultBlock {
                    tool_use_id: format!("call_{hash}"),
                    content: crate::providers::ToolResultContent::Text(format!(
                        "[Context pruned: 83 lines, ~5KB. Hash: {hash}]\nPreserved anchors (copy EXACTLY):\n{anchored_body}"
                    )),
                    shared: crate::providers::SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: None,
        };

        let mut history = vec![
            make_pruned_read("aaaa1111"),
            make_pruned_read("bbbb2222"),
            make_pruned_read("cccc3333"),
        ];

        compact_old_tool_results(&mut history);

        let get_text = |msg: &StorageMessage| match &msg.content {
            MessageContent::UserBlocks(blocks) => match &blocks[0] {
                UserContentBlock::ToolResult(tr) => match &tr.content {
                    ToolResultContent::Text(t) => t.clone(),
                    _ => panic!("Expected text"),
                },
                _ => panic!("Expected ToolResult"),
            },
            _ => panic!("Expected UserBlocks"),
        };

        let aged = get_text(&history[0]);
        assert!(aged.starts_with("[Context pruned: 83 lines"));
        assert!(aged.contains("Earlier read content"));
        assert!(
            aged.len() < 1000,
            "aged pruned re-read must collapse near header size, got {} bytes",
            aged.len()
        );
        assert!(
            get_text(&history[2]).contains("line content number 80"),
            "most recent pruned re-read must keep full content"
        );
    }

    #[test]
    fn test_compact_old_tool_results_collapses_stale_search_results() {
        let make_search_pair = |n: usize, size: usize| -> Vec<StorageMessage> {
            let id = format!("call_search_{n}");
            vec![
                StorageMessage {
                    id: None,
                    role: MessageRole::Assistant,
                    content: MessageContent::AssistantBlocks(vec![
                        AssistantContentBlock::ToolUse(ToolUseBlock {
                            id: id.clone(),
                            name: "search_files".to_string(),
                            input: serde_json::json!({}),
                            shared: SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                            reasoning_details: None,
                        }),
                    ]),
                    model_info: None,
                    metrics: None,
                    ts: None,
                },
                StorageMessage {
                    id: None,
                    role: MessageRole::User,
                    content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                        crate::providers::ToolResultBlock {
                            tool_use_id: id,
                            content: ToolResultContent::Text(format!(
                                "matches\n{}",
                                "z".repeat(size)
                            )),
                            shared: SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                        },
                    )]),
                    model_info: None,
                    metrics: None,
                    ts: None,
                },
            ]
        };

        let mut history = Vec::new();
        for n in 1..=4 {
            history.extend(make_search_pair(n, 5000));
        }

        compact_old_tool_results(&mut history);

        let get_result_text = |idx: usize| match &history[idx].content {
            MessageContent::UserBlocks(blocks) => match &blocks[0] {
                UserContentBlock::ToolResult(tr) => match &tr.content {
                    ToolResultContent::Text(t) => t.clone(),
                    _ => panic!("Expected text"),
                },
                _ => panic!("Expected ToolResult"),
            },
            _ => panic!("Expected UserBlocks"),
        };

        let stale = get_result_text(1);
        assert!(stale.contains("Earlier search results"));
        assert!(
            !stale.contains(&"z".repeat(5000)),
            "stale search results should be compacted"
        );
        assert!(
            get_result_text(5).contains(&"z".repeat(5000)),
            "delivered search results within retention must not be compacted"
        );
        assert!(
            get_result_text(7).contains(&"z".repeat(5000)),
            "undelivered newest batch must not be compacted"
        );
    }

    #[test]
    fn test_compact_old_tool_results_collapses_stale_skeletons() {
        let make_skeleton_pair = |n: usize, size: usize| -> Vec<StorageMessage> {
            let id = format!("call_skeleton_{n}");
            vec![
                StorageMessage {
                    id: None,
                    role: MessageRole::Assistant,
                    content: MessageContent::AssistantBlocks(vec![
                        AssistantContentBlock::ToolUse(ToolUseBlock {
                            id: id.clone(),
                            name: "get_file_skeleton".to_string(),
                            input: serde_json::json!({}),
                            shared: SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                            reasoning_details: None,
                        }),
                    ]),
                    model_info: None,
                    metrics: None,
                    ts: None,
                },
                StorageMessage {
                    id: None,
                    role: MessageRole::User,
                    content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                        crate::providers::ToolResultBlock {
                            tool_use_id: id,
                            content: ToolResultContent::Text(format!(
                                "symbols\n{}",
                                "y".repeat(size)
                            )),
                            shared: SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                        },
                    )]),
                    model_info: None,
                    metrics: None,
                    ts: None,
                },
            ]
        };

        let mut history = Vec::new();
        for n in 1..=4 {
            history.extend(make_skeleton_pair(n, 5000));
        }

        compact_old_tool_results(&mut history);

        let get_result_text = |idx: usize| match &history[idx].content {
            MessageContent::UserBlocks(blocks) => match &blocks[0] {
                UserContentBlock::ToolResult(tr) => match &tr.content {
                    ToolResultContent::Text(t) => t.clone(),
                    _ => panic!("Expected text"),
                },
                _ => panic!("Expected ToolResult"),
            },
            _ => panic!("Expected UserBlocks"),
        };

        let stale = get_result_text(1);
        assert!(stale.contains("Earlier search results"));
        assert!(
            !stale.contains(&"y".repeat(5000)),
            "stale skeleton results should be compacted"
        );
        assert!(
            get_result_text(5).contains(&"y".repeat(5000)),
            "delivered skeleton results within retention must not be compacted"
        );
        assert!(
            get_result_text(7).contains(&"y".repeat(5000)),
            "undelivered newest batch must not be compacted"
        );
    }

    #[test]
    fn test_compact_old_tool_results_collapses_stale_edits() {
        let make_edit_pair = |n: usize, size: usize| -> Vec<StorageMessage> {
            let id = format!("call_edit_{n}");
            vec![
                StorageMessage {
                    id: None,
                    role: MessageRole::Assistant,
                    content: MessageContent::AssistantBlocks(vec![
                        AssistantContentBlock::ToolUse(ToolUseBlock {
                            id: id.clone(),
                            name: "edit_file".to_string(),
                            input: serde_json::json!({}),
                            shared: SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                            reasoning_details: None,
                        }),
                    ]),
                    model_info: None,
                    metrics: None,
                    ts: None,
                },
                StorageMessage {
                    id: None,
                    role: MessageRole::User,
                    content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                        crate::providers::ToolResultBlock {
                            tool_use_id: id,
                            content: ToolResultContent::Text(format!(
                                "Edited 1 file(s): 1 edit(s) applied.\n\nApplied 1 edit(s) successfully (+1, -1 lines). New anchors shown below.\n{}",
                                "w".repeat(size)
                            )),
                            shared: SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                        },
                    )]),
                    model_info: None,
                    metrics: None,
                    ts: None,
                },
            ]
        };

        let mut history = Vec::new();
        for n in 1..=4 {
            history.extend(make_edit_pair(n, 5000));
        }

        compact_old_tool_results(&mut history);

        let get_result_text = |idx: usize| match &history[idx].content {
            MessageContent::UserBlocks(blocks) => match &blocks[0] {
                UserContentBlock::ToolResult(tr) => match &tr.content {
                    ToolResultContent::Text(t) => t.clone(),
                    _ => panic!("Expected text"),
                },
                _ => panic!("Expected ToolResult"),
            },
            _ => panic!("Expected UserBlocks"),
        };

        let stale = get_result_text(1);
        assert!(stale.starts_with("Edited 1 file(s): 1 edit(s) applied."));
        assert!(stale.contains("Earlier edit result"));
        assert!(
            !stale.contains(&"w".repeat(5000)),
            "stale edit results should be compacted"
        );
        assert!(
            get_result_text(5).contains(&"w".repeat(5000)),
            "delivered edit results within retention must not be compacted"
        );
        assert!(
            get_result_text(7).contains(&"w".repeat(5000)),
            "undelivered newest batch must not be compacted"
        );
    }

    #[test]
    fn test_compact_old_tool_results_collapses_stale_shell_output() {
        let make_shell_pair = |n: usize, tool: &str, size: usize| -> Vec<StorageMessage> {
            let id = format!("call_{tool}_{n}");
            vec![
                StorageMessage {
                    id: None,
                    role: MessageRole::Assistant,
                    content: MessageContent::AssistantBlocks(vec![
                        AssistantContentBlock::ToolUse(ToolUseBlock {
                            id: id.clone(),
                            name: tool.to_string(),
                            input: serde_json::json!({}),
                            shared: SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                            reasoning_details: None,
                        }),
                    ]),
                    model_info: None,
                    metrics: None,
                    ts: None,
                },
                StorageMessage {
                    id: None,
                    role: MessageRole::User,
                    content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                        crate::providers::ToolResultBlock {
                            tool_use_id: id,
                            content: ToolResultContent::Text(format!(
                                "first line\n{}",
                                "y".repeat(size)
                            )),
                            shared: SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                        },
                    )]),
                    model_info: None,
                    metrics: None,
                    ts: None,
                },
            ]
        };

        let mut history = Vec::new();
        for pair in make_shell_pair(1, "execute_command", 5000) {
            history.push(pair);
        }
        for pair in make_shell_pair(2, "execute_command", 5000) {
            history.push(pair);
        }
        for pair in make_shell_pair(3, "execute_command", 5000) {
            history.push(pair);
        }
        for pair in make_shell_pair(0, "execute_command", 100) {
            history.insert(0, pair);
        }
        for pair in make_shell_pair(9, "edit_file", 5000) {
            history.push(pair);
        }

        let bytes_before = tool_result_text_bytes(&history);
        compact_old_tool_results(&mut history);
        let bytes_after = tool_result_text_bytes(&history);
        assert!(
            bytes_before - bytes_after >= 4000,
            "collapsing one 5KB shell output must save bulk bytes, before={bytes_before} after={bytes_after}"
        );

        let texts: Vec<String> = history
            .iter()
            .filter(|msg| msg.role == MessageRole::User)
            .map(|msg| match &msg.content {
                MessageContent::UserBlocks(blocks) => match &blocks[0] {
                    UserContentBlock::ToolResult(tr) => match &tr.content {
                        ToolResultContent::Text(t) => t.clone(),
                        _ => panic!("Expected text"),
                    },
                    _ => panic!("Expected ToolResult"),
                },
                _ => panic!("Expected UserBlocks"),
            })
            .collect();
        assert_eq!(texts.len(), 5);
        assert!(
            texts[2].contains(&"y".repeat(5000)),
            "Second most recent shell output must not be compacted"
        );
        assert!(
            texts[3].contains(&"y".repeat(5000)),
            "Most recent shell output must not be compacted"
        );
        assert!(
            !texts[1].contains(&"y".repeat(5000))
                && texts[1].contains("Earlier shell output"),
            "Oldest large shell output must be collapsed, got: {}",
            &texts[1][..texts[1].len().min(200)]
        );
        assert!(
            texts[0].contains(&"y".repeat(100)),
            "Small shell output must not be compacted"
        );
        assert!(
            texts[4].contains(&"y".repeat(5000)),
            "Edit results must never be collapsed here"
        );
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

        let result = agent.execute_turn().await;
        assert!(
            matches!(result, TurnResult::Continue),
            "a rejected completion with no plan must continue, never complete"
        );

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

    fn assistant_tool_use_ids(history: &[crate::providers::StorageMessage]) -> Vec<String> {
        use crate::providers::{AssistantContentBlock, MessageContent};
        history
            .iter()
            .rev()
            .find_map(|message| match &message.content {
                MessageContent::AssistantBlocks(blocks) => Some(blocks),
                _ => None,
            })
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContentBlock::ToolUse(tool_use) => {
                            Some(tool_use.id.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn stored_tool_results(
        history: &[crate::providers::StorageMessage],
    ) -> Vec<(String, String)> {
        use crate::providers::{MessageContent, ToolResultContent, UserContentBlock};
        history
            .iter()
            .rev()
            .find_map(|message| match &message.content {
                MessageContent::UserBlocks(blocks) => Some(blocks),
                _ => None,
            })
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| match block {
                        UserContentBlock::ToolResult(result) => match &result.content {
                            ToolResultContent::Text(text) => {
                                Some((result.tool_use_id.clone(), text.clone()))
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn test_all_denied_batch_counts_mistakes_and_continues() {
        use crate::core::approval::ApprovalManager;
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;
        use crate::test_support::env_lock;

        let _approval_guard = crate::core::approval::approval_test_guard();
        // Force the non-interactive denial path. With a TTY stdin,
        // is_terminal() returns true and the prompt would fail closed with
        // Unavailable instead of returning Denied.
        // SAFETY: env mutation is serialized by env_lock; restored below.
        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe { std::env::set_var("SNED_APPROVAL_DENY", "1") };

        let responses = vec![vec![
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_d1".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("execute_command".to_string()),
                        arguments: Some(
                            serde_json::json!({"commands": ["rm -rf /tmp/sned-f02-d1"]})
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
                    call_id: Some("call_d2".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("execute_command".to_string()),
                        arguments: Some(
                            serde_json::json!({"commands": ["rm -rf /tmp/sned-f02-d2"]})
                                .to_string(),
                        ),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
        ]];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let (tx, _rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-all-denied-batch");
        config.interactive_mode = false;
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new()),
        );
        let approval_manager = Arc::new(tokio::sync::Mutex::new(ApprovalManager::new()));
        let mut agent = AgentLoop::new(config)
            .with_tools(Arc::new(registry))
            .with_approval_manager(approval_manager);

        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));
        assert_eq!(
            agent.state.lock().await.consecutive_mistakes,
            1,
            "denied calls are tool outcomes, not a text-only turn"
        );
        let history = agent.conversation_history.lock().await;
        let stored = stored_tool_results(&history);
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            assistant_tool_use_ids(&history),
            "every stored result must carry its prepared call ID"
        );
        assert!(stored.iter().all(|(_, text)| text.contains("was denied")));
        // SAFETY: restoring env after test.
        unsafe { std::env::remove_var("SNED_APPROVAL_DENY") };
    }

    #[tokio::test]
    async fn test_all_malformed_batch_counts_mistakes_and_continues() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;
        use crate::core::tools::handlers::read_file::ReadFileHandler;

        let responses = vec![vec![
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_m1".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("execute_command".to_string()),
                        arguments: Some("{oops".to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_m2".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("read_file".to_string()),
                        arguments: Some("[oops".to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
        ]];
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(
                responses,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        ));
        let (tx, _rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-all-malformed-batch");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new().with_yolo(true)),
        );
        registry.register(
            crate::core::tools::SnedTool::ReadFile,
            Arc::new(ReadFileHandler::new()),
        );
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));
        assert_eq!(
            agent.state.lock().await.consecutive_mistakes,
            1,
            "malformed calls are tool outcomes, not a text-only turn"
        );
        let history = agent.conversation_history.lock().await;
        let stored = stored_tool_results(&history);
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            assistant_tool_use_ids(&history),
            "every stored result must carry its prepared call ID"
        );
    }

    #[tokio::test]
    async fn test_unknown_tool_batch_counts_mistakes_and_continues() {
        use crate::core::tools::ToolRegistry;

        let responses = vec![vec![ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some("call_u1".to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: Some("definitely_not_a_tool_xyz".to_string()),
                    arguments: Some(serde_json::json!({}).to_string()),
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
        let (tx, _rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-unknown-tool-batch");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let registry = ToolRegistry::new();
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));
        assert_eq!(
            agent.state.lock().await.consecutive_mistakes,
            1,
            "unknown tools are tool outcomes, not a text-only turn"
        );
        let history = agent.conversation_history.lock().await;
        let stored = stored_tool_results(&history);
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            assistant_tool_use_ids(&history),
            "every stored result must carry its prepared call ID"
        );
        assert!(stored[0].1.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn test_mixed_success_and_denial_fails_plan_step_without_advancing() {
        use crate::core::approval::ApprovalManager;
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;
        use crate::core::tools::handlers::read_file::ReadFileHandler;
        use crate::test_support::env_lock;

        let _approval_guard = crate::core::approval::approval_test_guard();
        // Force the non-interactive denial path. With a TTY stdin,
        // is_terminal() returns true and the prompt would fail closed with
        // Unavailable instead of returning Denied.
        // SAFETY: env mutation is serialized by env_lock; restored below.
        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe { std::env::set_var("SNED_APPROVAL_DENY", "1") };

        let responses = vec![vec![
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_ok".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("read_file".to_string()),
                        arguments: Some(
                            serde_json::json!({"path": "shown.txt"}).to_string(),
                        ),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_denied".to_string()),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("execute_command".to_string()),
                        arguments: Some(
                            serde_json::json!({"commands": ["rm -rf /tmp/sned-f02-d3"]})
                                .to_string(),
                        ),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }),
        ]];
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
        let mut config = test_agent_config(provider, "test-mixed-success-denial");
        config.interactive_mode = false;
        config.output_writer = writer;

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ReadFile,
            Arc::new(ReadFileHandler::new()),
        );
        registry.register(
            crate::core::tools::SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new()),
        );
        let approval_manager = Arc::new(tokio::sync::Mutex::new(ApprovalManager::new()));
        let mut agent = AgentLoop::new(config)
            .with_tools(Arc::new(registry))
            .with_approval_manager(approval_manager);
        {
            let mut state = agent.state.lock().await;
            let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
                "Read then clean".to_string(),
            ]);
            plan.approved = true;
            plan.steps[0].status = crate::core::plan_state::PlanStepStatus::Running;
            state.plan_state = Some(plan);
        }

        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));
        assert_eq!(
            agent.state.lock().await.consecutive_mistakes,
            1,
            "the denied call must fail the batch"
        );
        {
            let state = agent.state.lock().await;
            let plan = state
                .plan_state
                .as_ref()
                .expect("plan should remain present");
            assert_eq!(
                plan.steps[0].status,
                crate::core::plan_state::PlanStepStatus::Failed,
                "the omitted denial must not read as a finished step"
            );
            assert!(plan.paused);
            assert!(!plan.complete);
        }
        let events = drain_output_events(&mut priority_rx, &mut rx);
        assert!(events.iter().any(|event| {
            matches!(event, OutputEvent::ErrorBox(message) if message.contains("Plan step 1/1 failed"))
        }));
        let history = agent.conversation_history.lock().await;
        let stored = stored_tool_results(&history);
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            assistant_tool_use_ids(&history),
            "every stored result must carry its prepared call ID"
        );
        assert!(stored[1].1.contains("was denied"));
        // SAFETY: restoring env after test.
        unsafe { std::env::remove_var("SNED_APPROVAL_DENY") };
    }

    #[tokio::test]
    async fn test_failed_command_with_completion_in_one_batch_continues() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::attempt_completion::AttemptCompletionHandler;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;

        let responses = vec![vec![
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
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
            }),
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some("call_early_completion".to_string()),
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
            }),
        ]];
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
        let mut config = test_agent_config(provider, "test-failed-command-same-batch-completion");
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

        let result = agent.execute_turn().await;
        assert!(
            matches!(result, TurnResult::Continue),
            "a failure elsewhere in the batch must not be hidden by a completion request"
        );
        assert_eq!(agent.state.lock().await.consecutive_mistakes, 1);
        let events = drain_output_events(&mut priority_rx, &mut rx);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, OutputEvent::Completion(_))),
            "no completion payload before the failure is addressed"
        );
    }

    fn next_request_text(requests: &[crate::providers::ProviderRequest], index: usize) -> String {
        use crate::providers::{MessageContent, ToolResultContent, UserContentBlock};
        requests
            .get(index)
            .expect("next provider request must exist")
            .messages
            .iter()
            .flat_map(|message| match &message.content {
                MessageContent::Text(text) => vec![text.clone()],
                MessageContent::UserBlocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        UserContentBlock::ToolResult(result) => match &result.content {
                            ToolResultContent::Text(text) => Some(text.clone()),
                            ToolResultContent::Blocks(inner) => Some(
                                inner
                                    .iter()
                                    .filter_map(|content| match content {
                                        crate::providers::ToolResultContentBlock::Text { text } => {
                                            Some(text.clone())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            ),
                        },
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn test_fresh_read_batch_survives_into_next_request() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::read_file::ReadFileHandler;

        // Scratch files must live under the workspace: the read handler
        // rejects absolute paths outside it. Cargo's target dir is
        // gitignored and the tempdir self-cleans on drop.
        let dir = tempfile::tempdir_in("target").unwrap();
        let mut markers = Vec::new();
        let mut paths = Vec::new();
        for (marker, fill) in [("FRESH-A", "x"), ("FRESH-B", "y"), ("FRESH-C", "z")] {
            let body = format!("{marker}-\n{}\n", fill.repeat(700));
            let path = dir.path().join(format!("{marker}.txt"));
            std::fs::write(&path, &body).unwrap();
            markers.push((marker, fill.repeat(700)));
            paths.push(path.to_string_lossy().into_owned());
        }

        let mut first_turn = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            first_turn.push(ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some(format!("call_r{}", index)),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("read_file".to_string()),
                        arguments: Some(serde_json::json!({"path": path}).to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }));
        }
        let responses = vec![
            first_turn,
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "done".to_string(),
                id: None,
                signature: None,
            })],
        ];
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, recorded.clone()),
        ));
        let (tx, _rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-fresh-read-batch");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ReadFile,
            Arc::new(ReadFileHandler::new()),
        );
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        agent.execute_turn().await;
        agent.execute_turn().await;

        let requests = recorded.lock().unwrap();
        let second = next_request_text(&requests, 1);
        for (marker, run) in &markers {
            assert!(
                second.contains(&format!("{marker}-")),
                "fresh batch body must reach the next request before any compaction"
            );
            assert!(
                second.contains(run),
                "fresh batch body must reach the next request before any compaction"
            );
        }
    }

    #[tokio::test]
    async fn test_fresh_search_batch_survives_into_next_request() {
        use crate::core::tools::ToolRegistry;

        let markers = ["SEARCH-A", "SEARCH-B", "SEARCH-C"];
        let mut first_turn = Vec::new();
        for (index, marker) in markers.iter().enumerate() {
            first_turn.push(ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some(format!("call_s{}", index)),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("search_files".to_string()),
                        arguments: Some(serde_json::json!({"pattern": marker}).to_string()),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }));
        }
        let responses = vec![
            first_turn,
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "done".to_string(),
                id: None,
                signature: None,
            })],
        ];
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, recorded.clone()),
        ));
        let (tx, _rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-fresh-search-batch");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::SearchFiles,
            Arc::new(PatternEchoHandler),
        );
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        agent.execute_turn().await;
        agent.execute_turn().await;

        let requests = recorded.lock().unwrap();
        let second = next_request_text(&requests, 1);
        for marker in markers {
            let body = format!("RESULT-{marker}\n{}", "y".repeat(1200));
            assert!(
                second.contains(&body),
                "fresh batch body must reach the next request before any compaction"
            );
        }
    }

    #[tokio::test]
    async fn test_fresh_command_batch_survives_into_next_request() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;

        let markers = ["CMD-A", "CMD-B", "CMD-C"];
        let mut first_turn = Vec::new();
        for (index, marker) in markers.iter().enumerate() {
            first_turn.push(ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some(format!("call_c{}", index)),
                    function: ApiStreamToolCallFunction {
                        id: None,
                        name: Some("execute_command".to_string()),
                        arguments: Some(
                            serde_json::json!({"commands": [format!("awk 'BEGIN {{ for (i=0;i<30;i++) printf \"{marker}-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n\", i }}'")]}).to_string(),
                        ),
                    },
                    signature: None,
                },
                id: None,
                signature: None,
            }));
        }
        let responses = vec![
            first_turn,
            vec![ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "done".to_string(),
                id: None,
                signature: None,
            })],
        ];
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Providers::RecordingChunk(
            crate::providers::RecordingChunkProvider::new(responses, recorded.clone()),
        ));
        let (tx, _rx) = mpsc::channel(32);
        let mut config = test_agent_config(provider, "test-fresh-command-batch");
        config.output_writer = Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let mut registry = ToolRegistry::new();
        registry.register(
            crate::core::tools::SnedTool::ExecuteCommand,
            Arc::new(ExecuteCommandHandler::new().with_yolo(true)),
        );
        let mut agent = AgentLoop::new(config).with_tools(Arc::new(registry));

        agent.execute_turn().await;
        agent.execute_turn().await;

        let requests = recorded.lock().unwrap();
        let second = next_request_text(&requests, 1);
        for marker in markers {
            let mid_batch_line = format!("{marker}-0020-{}", "x".repeat(30));
            assert!(
                second.contains(&mid_batch_line),
                "fresh batch body must reach the next request before any compaction"
            );
        }
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
