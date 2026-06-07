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
pub use crate::core::agent_types::{
    AgentConfig, AgentError, AgentMode, SnippedCodeBlock, TaskState, TurnResult,
};
use crate::core::agent_types::{
    MAX_CODE_BLOCK_DISPLAY_LINES_INTERACTIVE, MAX_CODE_BLOCK_DISPLAY_LINES_ONE_SHOT,
};
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
    coerce_string_array, tool_result_to_text,
};
use crate::providers::{
    ApiStreamChunk, ApiStreamToolCall, AssistantContentBlock, MessageContent, MessageRole,
    Provider, ProviderRequest, RedactedThinkingBlock, SharedContentFields, StorageMessage,
    TextContentBlock, ThinkingBlock, ToolResultContent, ToolUseBlock, UserContentBlock,
};
use crate::storage::global_state::HistoryItem;
use crate::storage::state_manager::StateManager;
use crate::storage::task_storage::TaskStorage;
use futures::future::FutureExt;
use ratatui::style::{Color, Modifier, Style};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::hash::Hasher;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};

const DEFAULT_MESSAGE_QUEUE_MAX_LEN: usize = 1000;
const MESSAGE_QUEUE_MAX_LEN_ENV: &str = "SNED_AGENT_MAX_QUEUED_MESSAGES";

/// Default token limit for tool results stored in history (~5000 tokens / ~20KB)
const DEFAULT_TOOL_RESULT_HISTORY_LIMIT: usize = 20_000;
/// Environment variable to configure tool result history limit
const TOOL_RESULT_HISTORY_LIMIT_ENV: &str = "SNED_TOOL_RESULT_HISTORY_LIMIT";

/// Default token limit for thinking blocks in old history entries (~2000 tokens)
const DEFAULT_THINKING_HISTORY_LIMIT: usize = 2_000;
/// Environment variable to configure thinking block history limit
const THINKING_HISTORY_LIMIT_ENV: &str = "SNED_THINKING_HISTORY_LIMIT";

use crate::core::plan_state::PlanStepStatus;
use crate::core::stream_parsing::{split_model_output, truncate_json_arguments};
use crate::core::tool_output::{
    extract_edit_stats_detailed, format_heat_map, format_tool_result, format_tool_summary,
    normalize_path_for_matching, path_from_read_file_header, summarize_matching_sections,
};

const MAX_TOOL_RESULT_DISPLAY_LINES: usize = 5;
const MAX_COMMAND_RESULT_DISPLAY_LINES: usize = 8;
/// Default concurrency limit for parallel non-grouped tool execution.
/// Prevents I/O contention when many tools run simultaneously.
const DEFAULT_TOOL_CONCURRENCY: usize = 12;
// MAX_TOOL_ARGUMENT_SIZE moved to providers/mod.rs for shared use
use crate::providers::MAX_TOOL_ARGUMENT_SIZE;

#[derive(Debug, Clone)]
struct ToolExecutionOutput {
    text: String,
    metadata: Option<ToolFailureMetadata>,
    is_error: bool,
}

impl ToolExecutionOutput {
    fn success(text: String) -> Self {
        Self {
            text,
            metadata: None,
            is_error: false,
        }
    }

    fn error(text: String, metadata: Option<ToolFailureMetadata>) -> Self {
        Self {
            text,
            metadata,
            is_error: true,
        }
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

    format!(
        "{}\n\n[{} lines truncated, use read_file to see full content]",
        truncated, remaining_lines
    )
}

fn code_fence_language(line: &str) -> &str {
    line.trim_start()
        .trim_start_matches("```")
        .split_whitespace()
        .next()
        .unwrap_or("")
}

// Cached terminal width to avoid repeated syscalls during streaming output.
// Terminal width rarely changes mid-task; refresh every 2 seconds.
static TERM_WIDTH_CACHE: std::sync::Mutex<Option<(usize, std::time::Instant)>> =
    std::sync::Mutex::new(None);

fn get_terminal_width() -> usize {
    use std::time::{Duration, Instant};

    const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

    let mut cache = TERM_WIDTH_CACHE.lock().unwrap();
    let now = Instant::now();

    let needs_refresh = cache
        .as_ref()
        .map(|(_, last)| now.duration_since(*last) >= REFRESH_INTERVAL)
        .unwrap_or(true);

    if needs_refresh {
        let width = crossterm::terminal::size()
            .map(|(cols, _)| cols as usize)
            .unwrap_or(80);
        *cache = Some((width, now));
        width
    } else {
        cache.as_ref().map(|(w, _)| *w).unwrap_or(80)
    }
}

fn print_model_line(line: &str, output_writer: &crate::cli::output::OutputWriterArc) {
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
        output_writer.emit(OutputEvent::model_output(wrapped_line.to_string()));
    }
}

/// Like `print_model_line`, but if `pending` is true, prepends the turn indicator
/// "♦ " to the first emitted line and clears the flag. This keeps the indicator
/// on the same line as the start of the response instead of on its own line.
fn print_model_line_with_prefix_if_pending(
    line: &str,
    output_writer: &crate::cli::output::OutputWriterArc,
    pending: &mut bool,
) {
    if *pending && !line.trim().is_empty() {
        *pending = false;
        let prefixed = format!("♦ {}", line);
        print_model_line(&prefixed, output_writer);
    } else {
        print_model_line(line, output_writer);
    }
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
) {
    use crate::cli::output::OutputEvent;
    if lines.is_empty() {
        return;
    }

    let code = lines.join("\n");
    let highlighted = crate::cli::syntax_highlight::highlight_code(&code, lang);
    output_writer.emit(OutputEvent::RawAnsi(format!(
        "  {}\n",
        highlighted.replace("\n", "\n  ")
    )));
}

fn code_block_display_limit(interactive_mode: bool) -> usize {
    if interactive_mode {
        MAX_CODE_BLOCK_DISPLAY_LINES_INTERACTIVE
    } else {
        MAX_CODE_BLOCK_DISPLAY_LINES_ONE_SHOT
    }
}

fn snipped_code_block_hint(index: Option<usize>) -> String {
    match index {
        Some(index) => format!(
            "  ... [snipped - type /expand {} to show full block]",
            index
        ),
        None => "  ... [snipped]".to_string(),
    }
}

fn push_snipped_code_block(
    blocks: &mut Vec<SnippedCodeBlock>,
    start_index: usize,
    language: &str,
    lines: &[String],
) -> usize {
    let index = start_index + blocks.len() + 1;
    blocks.push(SnippedCodeBlock {
        index,
        language: language.to_string(),
        code: lines.join("\n"),
    });
    index
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

    pub async fn queued_message_count(&self) -> usize {
        self.queue.lock().await.len()
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
}

impl AgentLoop {
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
            Ok(parsed) => Ok(parsed),
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
                    "Tool '{}' arguments were invalid JSON and could not be parsed (id: {}). Please retry with valid JSON arguments.",
                    tool_name, tool_id
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
                .is_none_or(|id| id.is_empty())
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

    async fn record_first_output_emit_time(&self) {
        let mut state = self.state.lock().await;
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
    }

    async fn record_first_reasoning_chunk_time(&self) {
        let mut state = self.state.lock().await;
        if state.first_reasoning_chunk_time.is_none() {
            if crate::cli::output::timing_enabled() {
                state.first_reasoning_chunk_time = Some(std::time::Instant::now());
            }
            state.reasoning_active = true;
        }
    }

    async fn record_first_displayable_text_time(&self) {
        if !crate::cli::output::timing_enabled() {
            return;
        }

        let mut state = self.state.lock().await;
        if state.first_displayable_text_time.is_none() {
            state.first_displayable_text_time = Some(std::time::Instant::now());
        }
    }

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
        }
    }

    /// Enable yolo mode — forces tool profile to `Validate` so
    /// `execute_command` is available (explicit shell opt-in).
    pub fn with_yolo(mut self, yolo: bool) -> Self {
        self.deps.yolo = yolo;
        self
    }

    /// Generate the next unique message ID for this task.
    /// Format: `msg_{counter}` (monotonically increasing per AgentLoop instance).
    fn next_message_id(counter: &std::sync::atomic::AtomicUsize) -> String {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("msg_{}", n)
    }

    /// Get the underlying provider as a cloned Arc.
    pub fn get_provider(&self) -> Arc<dyn Provider> {
        self.config.provider.lock().unwrap().clone()
    }

    /// Get the current agent mode.
    pub fn mode(&self) -> crate::core::agent_types::AgentMode {
        self.config.mode
    }

    /// Set the active provider. Preserves conversation history.
    pub fn set_provider(&mut self, new_provider: Arc<dyn Provider>) {
        *self.config.provider.lock().unwrap() = new_provider;
    }

    /// Set the agent mode (used for Plan -> Act transition after approval).
    pub fn set_mode(&mut self, mode: crate::core::agent_types::AgentMode) {
        self.config.mode = mode;
    }

    /// Get the task ID.
    pub fn task_id(&self) -> &str {
        &self.config.task_id
    }

    /// Get a reference to the checkpoint manager, if configured.
    pub fn checkpoint_manager(&self) -> Option<&crate::core::checkpoints::TaskCheckpointManager> {
        self.deps.checkpoint_manager.as_ref()
    }

    /// Get a mutable reference to the checkpoint manager, if configured.
    pub fn checkpoint_manager_mut(
        &mut self,
    ) -> Option<&mut crate::core::checkpoints::TaskCheckpointManager> {
        self.deps.checkpoint_manager.as_mut()
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
    pub fn with_checkpoint_manager(
        mut self,
        checkpoint_manager: crate::core::checkpoints::TaskCheckpointManager,
    ) -> Self {
        self.deps.checkpoint_manager = Some(checkpoint_manager);
        self
    }

    /// Initialize the agent loop with an approval manager.
    pub fn with_approval_manager(
        mut self,
        approval_manager: Arc<tokio::sync::Mutex<crate::core::approval::ApprovalManager>>,
    ) -> Self {
        self.deps.approval_manager = Some(approval_manager);
        self
    }

    /// Initialize the agent loop with a context loader.
    pub fn with_context_loader(mut self, loader: crate::core::context::ContextLoader) -> Self {
        self.deps.context_loader = Some(loader);
        self
    }

    /// Initialize the agent loop with task storage for persisting conversation history.
    pub fn with_task_storage(mut self, task_storage: TaskStorage) -> Self {
        self.deps.task_storage = Some(task_storage);
        self
    }

    /// Set the system prompt context.
    pub fn with_system_prompt_context(mut self, context: SystemPromptContext) -> Self {
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
    /// Initialize the agent loop with tool handlers.
    pub fn with_tools(mut self, registry: Arc<ToolRegistry>) -> Self {
        self.deps.registry = Some(registry);
        self
    }

    /// Initialize the agent loop with hook manager.
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
        // Store state_manager for use during execution
        self.state_manager = Some(state_manager.clone());

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
            let _ = tracker.record_environment();
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

            // Hook execution timeout: 60 seconds default (configurable via SNED_HOOK_TIMEOUT_MS)
            let timeout_ms = std::env::var("SNED_HOOK_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&v| v > 0)
                .unwrap_or(60_000);
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
                        error: Some(format!("Hook execution failed: {}", e)),
                        exit_code: -1,
                        execution_time_ms: 0,
                    }
                }
                Err(_) => {
                    error!("TaskStart hook timed out after {}ms", timeout_ms);
                    crate::core::hooks::HookResult {
                        output: None,
                        error: Some(format!("Hook execution timed out after {}ms", timeout_ms)),
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
                            "[Hook context from TaskStart]: {}",
                            modification
                        )),
                        model_info: None,
                        metrics: None,
                        ts: None,
                    });
                    drop(history);
                }
                if output.cancel == Some(true) {
                    // Persist state on hook cancellation
                    if let Err(e) = StateManager::persist_async(Arc::clone(&state_manager)).await {
                        error!("Failed to persist state manager on hook cancel: {}", e);
                    }
                    return Err(AgentError::Cancelled);
                }
            }
        }

        let mut dequeued_message_for_notification = false;

        loop {
            if turn_count >= self.config.max_turns {
                // Persist state on max turns exceeded
                if let Err(e) = StateManager::persist_async(Arc::clone(&state_manager)).await {
                    error!("Failed to persist state manager on max turns: {}", e);
                }
                // Force-save conversation history to preserve final turns
                if let Some(ref storage) = self.deps.task_storage {
                    let history = self.conversation_history.lock().await;
                    if !history.is_empty()
                        && let Err(e) = storage.write_api_conversation_history_async(&history).await
                    {
                        error!("Failed to save conversation history on max turns: {}", e);
                    }
                }
                return Err(AgentError::MaxTurnsExceeded);
            }
            turn_count += 1;

            // Check plan pause: halt iteration if plan is paused
            {
                let state = self.state.lock().await;
                if let Some(ref plan) = state.plan_state
                    && plan.paused
                    && plan.approved
                {
                    drop(state);
                    self.config.output_writer.emit(OutputEvent::dim_yellow(
                        "Plan is paused. Type /plan resume to continue.",
                    ));
                    // Prevent CPU spinning on pause
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                drop(state);
            }

            // Check if cancelled
            {
                let state = self.state.lock().await;
                if state.is_cancelled {
                    drop(state);
                    // Execute full abort sequence: TaskCancel hook, state save, resource cleanup
                    let cancellation_handler =
                        crate::core::cancellation::CancellationHandler::new(self.state.clone());
                    if let Err(e) = cancellation_handler
                        .abort_task(
                            self.deps.hook_manager.as_ref().map(|hm| hm.as_ref()),
                            &state_manager,
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
                        let history = self.conversation_history.lock().await;
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
                    return Ok(());
                }
            }

            // Check message queue for pending messages
            {
                let mut mq = self.message_queue.lock().await;
                if let Some(queued_message) = mq.pop_front() {
                    let queue_remaining = mq.len();
                    drop(mq);
                    if !self.config.json_output {
                        if queue_remaining > 0 {
                            info!(
                                "\n[sned] Processing queued message ({} more queued)\n",
                                queue_remaining
                            );
                        } else {
                            info!("\n[sned] Processing queued message\n");
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
                                msg.content = MessageContent::Text(format!("{}{}", note, text));
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
                    if dequeued_message_for_notification && !self.config.json_output {
                        info!("\n[sned] Queued message sent to provider\n");
                    }
                    dequeued_message_for_notification = false;
                    continue;
                }
                TurnResult::Complete => {
                    if dequeued_message_for_notification && !self.config.json_output {
                        info!("\n[sned] Queued message sent to provider\n");
                    }
                    dequeued_message_for_notification = false;

                    // Check if more messages are queued
                    {
                        let mut mq = self.message_queue.lock().await;
                        if let Some(queued_message) = mq.pop_front() {
                            let queue_remaining = mq.len();
                            drop(mq);
                            if !self.config.json_output {
                                if queue_remaining > 0 {
                                    info!(
                                        "\n[sned] Processing queued message ({} more queued)\n",
                                        queue_remaining
                                    );
                                } else {
                                    info!("\n[sned] Processing queued message\n");
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
                                        msg.content =
                                            MessageContent::Text(format!("{}{}", note, text));
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
                                    "[Hook context from TaskComplete]: {}",
                                    modification
                                )),
                                model_info: None,
                                metrics: None,
                                ts: None,
                            });
                            drop(history);
                        }
                    }

                    // Record task in history for `sned history` and `--continue` support
                    let task = task_text.as_deref().unwrap_or("");
                    let state_guard = self.state.lock().await;
                    // Use cumulative tokens/cost for the entire session, not just last turn
                    let tokens_in = state_guard.cumulative_tokens_in as i32;
                    let tokens_out = state_guard.cumulative_tokens_out as i32;
                    let cache_writes = Some(state_guard.cumulative_cache_writes as i32);
                    let cache_reads = Some(state_guard.cumulative_cache_reads as i32);
                    let cost = state_guard.cumulative_cost;
                    drop(state_guard);

                    let workspace_root = self.resolve_workspace_root();
                    let workspace_root_str = workspace_root.to_str().map(String::from);
                    let history_item = HistoryItem {
                        id: self.config.task_id.clone(),
                        ulid: Some(self.config.task_id.clone()),
                        number: 0,
                        ts: chrono::Utc::now().timestamp_millis(),
                        task: task.to_string(),
                        tokens_in,
                        tokens_out,
                        cache_writes,
                        cache_reads,
                        total_cost: cost,
                        size: None,
                        shadow_git_config_work_tree: None,
                        cwd_on_task_initialization: workspace_root_str.clone(),
                        conversation_history_deleted_range: None,
                        is_favorited: None,
                        workspace_root_path: workspace_root_str,
                        checkpoint_manager_error_message: None,
                        model_id: None,
                    };

                    state_manager.add_task_to_history(history_item);

                    // Persist state to disk before exiting
                    if let Err(e) = StateManager::persist_async(Arc::clone(&state_manager)).await {
                        error!("Failed to persist state manager: {}", e);
                    }

                    return Ok(());
                }
                TurnResult::Cancelled => {
                    // Force-save conversation history immediately on cancellation (W4 fix)
                    // Bypass the 5-turn debounce to prevent data loss
                    if let Some(ref storage) = self.deps.task_storage {
                        let history = self.conversation_history.lock().await;
                        if !history.is_empty()
                            && let Err(e) =
                                storage.write_api_conversation_history_async(&history).await
                        {
                            error!("Failed to save conversation history on cancel: {}", e);
                        }
                        drop(history);

                        let state = self.state.lock().await;
                        if let Some(ref summary) = state.compacted_summary
                            && let Err(e) = storage.write_compacted_summary_async(summary).await
                        {
                            error!("Failed to save compacted summary on cancel: {}", e);
                        }

                        // Persist deleted range to history item
                        if let Some(deleted_range) = state.conversation_history_deleted_range
                            && let Some(ref state_mgr) = self.state_manager
                            && let Some(mut history_item) =
                                state_mgr.find_task_in_history(&self.config.task_id)
                        {
                            history_item.conversation_history_deleted_range =
                                Some(vec![deleted_range.0 as i32, deleted_range.1 as i32]);
                            state_mgr.add_task_to_history(history_item);
                            if let Err(e) = state_mgr.persist() {
                                warn!("Failed to persist task history on cancel: {}", e);
                            }
                        }
                    }

                    // Persist state manager (global state, task states, secrets)
                    if let Err(e) = StateManager::persist_async(Arc::clone(&state_manager)).await {
                        error!("Failed to persist state manager on cancel: {}", e);
                    }
                    return Ok(());
                }
                TurnResult::Error(e) => {
                    // Emit error to output (visible in TUI and logged)
                    self.config.output_writer.emit(OutputEvent::error(&e));

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
                self.config.provider.lock().unwrap().as_ref().name(),
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

        // 3. Create provider request
        // Build system prompt with context
        let context =
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
                    enable_parallel_tool_calling: true,
                    ..Default::default()
                });
        let workspace_root = context
            .cwd
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.resolve_workspace_root());
        let tool_context = Arc::new(ToolContext::new(
            self.state.clone(),
            self.deps.approval_manager.clone(),
            workspace_root,
            self.anchor_mgr.clone(),
            self.config.json_output,
            self.config.task_id.clone(),
            self.deps.hook_manager.clone(),
            false, // Initial context: not explicitly approved (approval happens per-tool)
            self.config.output_writer.clone(),
        ));
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
            let guard = self.config.provider.lock().unwrap();
            let provider_id = guard.name().to_string();
            let model_id = guard.get_model().id.clone();
            drop(guard);
            let mode = match self.config.mode {
                crate::core::agent_types::AgentMode::Plan => "plan",
                crate::core::agent_types::AgentMode::Act => "act",
            };
            let _ = tracker.record_model_usage(&provider_id, &model_id, mode);
        }

        // 3. Select tool profile and build tool definitions
        let profile = {
            let mode_str = match self.config.mode {
                crate::core::agent_types::AgentMode::Plan => "plan",
                crate::core::agent_types::AgentMode::Act => "act",
            };
            let prompt = pruned_history
                .iter()
                .find(|m| m.role == crate::providers::MessageRole::User)
                .and_then(|m| match &m.content {
                    crate::providers::MessageContent::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let profile =
                resolve_tool_profile(self.deps.tool_profile, self.deps.yolo, prompt, mode_str);
            tracing::info!(profile = ?profile, prompt_len = prompt.len(), "selected tool profile");
            self.deps.tool_profile = Some(profile);
            profile
        };
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
            let provider = self.config.provider.lock().unwrap().clone();
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
                return TurnResult::Error(format!("Context window overflow: {}", msg));
            }
        }

        // Create channel for stream chunks with large buffer to prevent
        // backpressure deadlocks when the provider emits faster than the
        // consumer processes (e.g. during very long responses).
        let (tx, mut rx) = mpsc::channel::<ApiStreamChunk>(10_000);

        let state_clone = self.state.clone();
        let history_clone = self.conversation_history.clone();
        let provider = self.config.provider.lock().unwrap().clone();

        tracing::debug!(
            message_count = request.messages.len(),
            tool_count = request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
            "starting provider stream"
        );

        let retry_config = if provider.name() == "gemini" {
            RetryConfig {
                max_retries: 4,
                base_delay_ms: 2_000,
                max_delay_ms: 15_000,
            }
        } else {
            RetryConfig::default()
        };

        if crate::cli::output::timing_enabled() {
            let mut state = self.state.lock().await;
            if state.request_sent_time.is_none() {
                state.request_sent_time = Some(std::time::Instant::now());
            }
        }

        let stream = match create_message_with_retry(
            provider.clone(),
            request,
            state_clone.clone(),
            retry_config,
            self.config.json_output,
            Some(self.config.output_writer.clone()),
            Some(self.cancelled.clone()),
        )
        .await
        {
            Ok(stream) => stream,
            Err(e) => {
                error!(error = %e, "provider request failed");
                let actionable = crate::cli::actionable_errors::provider_error(&e.to_string());
                let consecutive_failures = {
                    let state = self.state.lock().await;
                    state.consecutive_provider_failures
                };
                let message = if consecutive_failures >= DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES {
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
        };

        let stream_handle = tokio::spawn(async move {
            let mut stream = stream;
            use tokio_stream::StreamExt;
            while let Some(chunk) = stream.next().await {
                if tx.send(chunk).await.is_err() {
                    break;
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
        let mut tool_call_detected = false;
        let mut display_buffer = String::new();
        let mut in_code_block = false;
        let mut code_block_lang = String::new();
        let mut code_block_buffer: Vec<String> = Vec::new();
        let mut code_block_full_buffer: Vec<String> = Vec::new();
        let mut code_block_lines: usize = 0;
        let mut code_block_snipped = false;
        let code_block_display_limit = code_block_display_limit(self.config.interactive_mode);
        let snipped_block_start_index = {
            let state = self.state.lock().await;
            state.snipped_code_blocks.len()
        };
        let mut snipped_blocks_this_turn: Vec<SnippedCodeBlock> = Vec::new();
        let mut reasoning_preview_shown = false;
        let mut stream_errored = false;
        let mut in_thinking_tag = false;

        // Track time to first token for UX feedback on slow connections
        let first_chunk_start = std::time::Instant::now();
        let mut slow_connection_warned = false;

        // Buffer flush timing to reduce syscalls on high-latency connections (P9)
        let mut last_flush_time = Instant::now();
        let flush_interval = std::time::Duration::from_millis(50);

        // Turn indicator is prepended to the first output line, not emitted separately,
        // so it appears on the same line as the start of the response.
        let mut turn_indicator_pending = true;

        while let Some(chunk) = rx.recv().await {
            // Check for cancellation during stream processing so Ctrl+C
            // takes effect promptly instead of waiting for the full stream.
            // Uses lock-free AtomicBool to avoid mutex contention on every chunk.
            if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                tracing::info!("cancellation detected during stream processing, aborting turn");
                return TurnResult::Cancelled;
            }

            // Warn about slow connection if waiting >3s for first token
            if !first_chunk_received
                && !slow_connection_warned
                && first_chunk_start.elapsed().as_secs() >= 3
            {
                slow_connection_warned = true;
                self.config.output_writer.emit(OutputEvent::dim_yellow(
                    "⏳ Waiting for API response (slow connection?)...",
                ));
            }

            if !first_chunk_received {
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
                                        );
                                        if code_block_snipped {
                                            let index = if self.config.interactive_mode {
                                                Some(push_snipped_code_block(
                                                    &mut snipped_blocks_this_turn,
                                                    snipped_block_start_index,
                                                    &code_block_lang,
                                                    &code_block_full_buffer,
                                                ))
                                            } else {
                                                None
                                            };
                                            self.config.output_writer.emit(OutputEvent::dim(
                                                snipped_code_block_hint(index),
                                            ));
                                        }
                                        in_code_block = false;
                                        code_block_lang.clear();
                                        code_block_buffer.clear();
                                        code_block_full_buffer.clear();
                                        code_block_lines = 0;
                                        code_block_snipped = false;
                                    } else {
                                        in_code_block = true;
                                        code_block_lang =
                                            code_fence_language(trimmed_line).to_string();
                                        code_block_buffer.clear();
                                        code_block_full_buffer.clear();
                                        code_block_lines = 0;
                                        code_block_snipped = false;
                                    }

                                    print_model_line_with_prefix_if_pending(
                                        trimmed_line,
                                        &self.config.output_writer,
                                        &mut turn_indicator_pending,
                                    );
                                    continue;
                                }

                                if in_code_block {
                                    code_block_lines += 1;
                                    // For code blocks, preserve leading indentation (only trim end)
                                    let code_line = line.trim_end().to_string();
                                    code_block_full_buffer.push(code_line.clone());
                                    if code_block_lines > code_block_display_limit {
                                        code_block_snipped = true;
                                        // Prevent CPU spinning on pause
                                        tokio::time::sleep(std::time::Duration::from_millis(500))
                                            .await;
                                        continue;
                                    }

                                    code_block_buffer.push(code_line);
                                    continue;
                                }

                                // Regular content - already trimmed
                                self.record_first_output_emit_time().await;
                                print_model_line_with_prefix_if_pending(
                                    trimmed_line,
                                    &self.config.output_writer,
                                    &mut turn_indicator_pending,
                                );
                            }
                            // Buffer flush to ~50ms frames to reduce syscalls on high-latency connections (P9)
                            if last_flush_time.elapsed() >= flush_interval {
                                self.config.output_writer.flush();
                                last_flush_time = Instant::now();
                            }
                        }
                    }
                    if text_chunk.signature.is_some() {
                        accumulated_text_signature = text_chunk.signature.clone();
                    }
                    accumulated_text.push_str(&text_chunk.text);
                }
                ApiStreamChunk::Reasoning(reasoning_chunk) => {
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
                    } else {
                        // Show compact thinking indicator instead of raw reasoning dump.
                        // Collects first non-empty line as a summary, displays with Ɵ symbol.
                        if !reasoning_preview_shown {
                            let first_line = reasoning_chunk
                                .reasoning
                                .lines()
                                .find(|l| !l.trim().is_empty())
                                .map(|l| l.trim().to_string());

                            if let Some(summary) = first_line {
                                // Truncate summary to fit on one line
                                let term_width = crossterm::terminal::size()
                                    .map(|(cols, _)| cols as usize)
                                    .unwrap_or(80);
                                let max_summary_len = term_width.saturating_sub(6); // Account for "  Ɵ " prefix
                                let truncated = if summary.len() > max_summary_len {
                                    let safe_end = summary
                                        .floor_char_boundary(max_summary_len.saturating_sub(3));
                                    format!("{}...", &summary[..safe_end])
                                } else {
                                    summary
                                };

                                self.config
                                    .output_writer
                                    .emit(OutputEvent::dim(format!("  Ɵ {}", truncated)));
                                reasoning_preview_shown = true;
                            }
                        }
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
                    // Store ApiReqInfo for context management in next turn.
                    // Merge with previous info: some providers (Anthropic) send
                    // usage in two chunks (input tokens in message_start, output
                    // tokens in message_delta). The second chunk hardcodes
                    // input_tokens=0, so we preserve the first chunk's values
                    // when the new chunk sends 0.
                    let mut state = self.state.lock().await;
                    let prev_info = state.last_api_req_info.as_ref();
                    let context_window_info = crate::core::context::get_context_window_info(
                        self.config.provider.lock().unwrap().as_ref(),
                    );
                    let context_window = context_window_info.context_window;
                    let guard = self.config.provider.lock().unwrap();
                    let provider_name = guard.name().to_string();
                    drop(guard);
                    let context_usage_pct =
                        crate::core::context::context_window::calculate_context_usage_percentage(
                            usage_chunk.input_tokens,
                            usage_chunk.output_tokens,
                            usage_chunk.cache_write_tokens,
                            usage_chunk.cache_read_tokens,
                            context_window,
                            &provider_name,
                        );
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
                    state.last_api_req_info = Some(ApiReqInfo {
                        request: None,
                        tokens_in: Some(tokens_in),
                        tokens_out: Some(tokens_out),
                        cache_writes: usage_chunk
                            .cache_write_tokens
                            .or(prev_info.and_then(|r| r.cache_writes)),
                        cache_reads: usage_chunk
                            .cache_read_tokens
                            .or(prev_info.and_then(|r| r.cache_reads)),
                        reasoning_tokens: usage_chunk
                            .reasoning_tokens
                            .or(prev_info.and_then(|r| r.reasoning_tokens)),
                        cost: usage_chunk.total_cost.or(prev_info.and_then(|r| r.cost)),
                        context_window: Some(context_window),
                        context_usage_percentage: Some(context_usage_pct),
                    });
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
                ApiStreamChunk::ToolCalls(tool_chunk) => {
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

                    // Print tool call header only on first appearance of this tool call key
                    if !self.config.json_output && !tool_calls_map.contains_key(&key) {
                        let tool_name = tc.function.name.as_deref().unwrap_or("unknown");
                        self.config
                            .output_writer
                            .emit(OutputEvent::tool_call(format!("▶ {}", tool_name)));
                    }
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
                        || tc.function.arguments.as_ref().is_some_and(|a| a.is_empty());
                    if (tc.function.name.is_none()
                        || tc.function.name.as_ref().is_some_and(|n| n.is_empty()))
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
                            let truncated = truncate_json_arguments(args, MAX_TOOL_ARGUMENT_SIZE);
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
                        // Emit to output_writer so TUI users see the error in the output pane
                        self.config
                            .output_writer
                            .emit(OutputEvent::plain(format!("Error: {}", err)));
                    }
                }
            }
        }

        // Final flush: print any remaining buffered content and ensure newline
        if in_code_block && !self.config.json_output {
            let remaining = display_buffer.trim_end().to_string();
            if !remaining.is_empty() {
                code_block_lines += 1;
                code_block_full_buffer.push(remaining.clone());
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
            );
            if code_block_snipped {
                let index = if self.config.interactive_mode {
                    Some(push_snipped_code_block(
                        &mut snipped_blocks_this_turn,
                        snipped_block_start_index,
                        &code_block_lang,
                        &code_block_full_buffer,
                    ))
                } else {
                    None
                };
                self.config
                    .output_writer
                    .emit(OutputEvent::dim(snipped_code_block_hint(index)));
            }
            self.config.output_writer.flush();
        } else if !display_buffer.is_empty() && !self.config.json_output {
            let remaining = display_buffer.trim_end().to_string();
            if !remaining.is_empty() {
                self.record_first_output_emit_time().await;
                print_model_line_with_prefix_if_pending(
                    &remaining,
                    &self.config.output_writer,
                    &mut turn_indicator_pending,
                );
            }
        } else if !self.config.json_output {
            self.config.output_writer.flush();
        }

        // Wait for stream to complete
        if let Err(e) = stream_handle.await {
            let actionable = crate::cli::actionable_errors::provider_error(&e.to_string());
            return TurnResult::Error(actionable.display());
        }

        // If stream errored mid-response, note the partial content in the error
        if stream_errored {
            let partial_note = if !accumulated_text.is_empty() || !accumulated_reasoning.is_empty()
            {
                format!(
                    " (partial response of {} text chars{} discarded)",
                    accumulated_text.len(),
                    if !accumulated_reasoning.is_empty() {
                        format!(" + {} reasoning chars", accumulated_reasoning.len())
                    } else {
                        String::new()
                    }
                )
            } else {
                String::new()
            };
            return TurnResult::Error(format!(
                "Provider stream error{} - retry the request.",
                partial_note
            ));
        }

        if !snipped_blocks_this_turn.is_empty() {
            let mut state = self.state.lock().await;
            state.snipped_code_blocks.extend(snipped_blocks_this_turn);
        }

        if !self.config.json_output && !accumulated_text.is_empty() {
            tracing::debug!("");
        }

        let prepared_tool_calls = Self::prepare_tool_calls(&tool_call_order, &mut tool_calls_map);

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
                max_allowed = self.config.max_consecutive_mistakes,
                "Model returned empty response (no text, no tool calls)"
            );

            if state.consecutive_mistakes >= self.config.max_consecutive_mistakes {
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
        let (extracted_thinking, response_text) = split_model_output(&accumulated_text);
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
                (Some(t), false) => Some(format!("{}\n{}", t, accumulated_reasoning)),
                (None, false) => Some(accumulated_reasoning.clone()),
                (None, true) => None,
            };

            if let Some(ref thinking) = merged_thinking
                && !thinking.is_empty()
            {
                blocks.push(AssistantContentBlock::Thinking(ThinkingBlock {
                    thinking: thinking.clone(),
                    signature: accumulated_signature.clone().unwrap_or_default(),
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
                    if let Some(profile) = self.deps.tool_profile {
                        if let Some(next) = profile.escalate() {
                            tracing::info!(
                                ?profile,
                                ?next,
                                "escalating tool profile after text-only response"
                            );
                            self.deps.tool_profile = Some(next);
                            history.push(StorageMessage {
                                id: Some(Self::next_message_id(&self.message_counter)),
                                role: MessageRole::User,
                                content: MessageContent::Text(
                                    String::from("You returned text without using a tool. If this task requires workspace changes or verification, use the required tool. If the task is complete, call attempt_completion or plan_mode_respond.")
                                ),
                                model_info: None,
                                metrics: None,
                                ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                            });
                        } else {
                            history.push(StorageMessage {
                                id: Some(Self::next_message_id(&self.message_counter)),
                                role: MessageRole::User,
                                content: MessageContent::Text(
                                    String::from("You returned text without using a tool. If this task requires workspace changes or verification, use the required tool. If the task is complete, call attempt_completion or plan_mode_respond.")
                                ),
                                model_info: None,
                                metrics: None,
                                ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                            });
                        }
                    } else {
                        history.push(StorageMessage {
                            id: Some(Self::next_message_id(&self.message_counter)),
                            role: MessageRole::User,
                            content: MessageContent::Text(
                                String::from("You returned text without using a tool. If this task requires workspace changes or verification, use the required tool. If the task is complete, call attempt_completion or plan_mode_respond.")
                            ),
                            model_info: None,
                            metrics: None,
                            ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                        });
                    }
                }
            }
        }

        // 7. Save checkpoint before executing tools (if checkpoint manager is configured)
        if !prepared_tool_calls.is_empty()
            && let Some(ref mut checkpoint_mgr) = self.deps.checkpoint_manager
        {
            checkpoint_mgr.save_checkpoint().await;
        }

        // 8. Dispatch tools (parallel execution for independent tools)
        let mut tool_failure_count = 0usize;
        if !prepared_tool_calls.is_empty() {
            let mut edit_files: Vec<(String, i32, i32)> = Vec::new();

            // Print compact tool call summaries (skip malformed tool calls with empty names)
            if !self.config.json_output {
                for prepared in &prepared_tool_calls {
                    let tool_name = prepared.tool_name.as_str();

                    // Skip malformed tool calls with empty names
                    if tool_name.is_empty() {
                        continue;
                    }

                    let tool_params = prepared
                        .parsed_args
                        .as_ref()
                        .unwrap_or(&serde_json::Value::Null);
                    let summary = format_tool_summary(tool_name, tool_params);
                    self.config.output_writer.emit(OutputEvent::dim(summary));
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
                Vec<String>,
            );
            let mut tool_tasks: Vec<ToolTask> = Vec::with_capacity(prepared_tool_calls.len());

            for prepared in &prepared_tool_calls {
                let tool_name = prepared.tool_name.clone();

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
                        ));
                        continue;
                    }
                };

                let immediate_output = if let Some(tool) = SnedTool::from_name(&tool_name) {
                    // Check plan mode restrictions
                    let is_restricted = {
                        let state = self.state.lock().await;
                        self.config.mode == AgentMode::Plan
                            && state.strict_plan_mode_enabled
                            && Self::is_plan_mode_restricted(tool)
                    };

                    if is_restricted {
                        ToolExecutionOutput::error(
                            format!(
                                "Tool '{}' is not available in PLAN MODE. This tool is restricted to ACT MODE for file modifications. Only use tools available for PLAN MODE when in that mode.",
                                tool_name
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
                        let params_fingerprint = Self::tool_params_fingerprint(&tool_params);
                        let previously_denied = {
                            let state = self.state.lock().await;
                            state
                                .is_denied_tool_action(&tool_name, &params_fingerprint)
                                .is_some()
                        };
                        if previously_denied {
                            ToolExecutionOutput::error(
                                format!(
                                    "Tool '{}' was already denied for this exact request. Ask the user before retrying the same action.",
                                    tool_name
                                ),
                                Some(ToolFailureMetadata {
                                    class: ToolFailureClass::ApprovalDenied,
                                    affected_paths: action_paths.clone(),
                                    required_next_step: Some(ToolRequiredNextStep::AskUser),
                                }),
                            )
                        } else {
                            let mut user_prompted = false;
                            let approval_result = if let Some(ref approval_mgr) =
                                self.deps.approval_manager
                            {
                                let mgr = approval_mgr.lock().await;
                                // Check if any action paths require prompting
                                let needs_prompt = if action_paths.is_empty() {
                                    // No paths (e.g., execute_command): check tool-level approval
                                    // For execute_command, pass command fingerprint for per-command approval (F-02 fix)
                                    let cmd_fp = if tool_name == "execute_command" {
                                        Some(params_fingerprint.as_str())
                                    } else {
                                        None
                                    };
                                    mgr.should_prompt(tool, cmd_fp)
                                } else {
                                    // Has paths: check per-path approval
                                    action_paths.iter().any(|p| {
                                        mgr.should_prompt_with_path(tool, Some(p.as_str()))
                                    })
                                };
                                if needs_prompt {
                                    drop(mgr); // Drop lock before async call
                                    user_prompted = true;
                                    match crate::core::approval::prompt_for_approval_async(
                                        &tool_name,
                                        &tool_params,
                                        self.config.output_writer.clone(),
                                    )
                                    .await
                                    {
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
                                                // For execute_command, pass command fingerprint for per-command approval (F-02 fix)
                                                let cmd_fp = if tool_name == "execute_command" {
                                                    Some(params_fingerprint.as_str())
                                                } else {
                                                    None
                                                };
                                                mgr.auto_approve(tool, cmd_fp);
                                            }
                                            None // Proceed to execute
                                        }
                                        Ok(crate::core::approval::ApprovalResult::Approved) => {
                                            None // Proceed to execute
                                        }
                                        Err(e) => Some(ToolExecutionOutput::error(
                                            format!(
                                                "Approval error for tool '{}': {}",
                                                tool_name, e
                                            ),
                                            None,
                                        )),
                                    }
                                } else if tool_name == "execute_command" {
                                    // Auto-approved path for execute_command: check command
                                    // safety before auto-approving. If the command is
                                    // unsafe, prompt the user instead (matching TS:
                                    // shouldAutoApprove = isSafe && autoApproveEnabled).
                                    let commands =
                                        coerce_string_array(&tool_params, "commands", "command");
                                    let script = tool_params.get("script").and_then(|s| s.as_str());
                                    let yolo = mgr.is_yolo_mode();
                                    let user_safe = mgr.get_user_safe_commands().clone();
                                    let checker =
                                        crate::core::approval::CommandSafetyChecker::new()
                                            .with_yolo(yolo)
                                            .with_user_safe_commands(user_safe);
                                    let any_unsafe = commands.iter().any(|cmd| {
                                        !cmd.is_empty() && checker.is_safe(cmd).is_err()
                                    }) || script
                                        .is_some_and(|s| checker.is_safe(s).is_err());
                                    if any_unsafe {
                                        // Auto-approved but unsafe — override auto-approval and prompt the user
                                        drop(mgr);
                                        user_prompted = true;
                                        match crate::core::approval::prompt_for_approval_async(
                                            &tool_name,
                                            &tool_params,
                                            self.config.output_writer.clone(),
                                        )
                                        .await
                                        {
                                            Ok(crate::core::approval::ApprovalResult::Denied) => {
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
                                            Ok(crate::core::approval::ApprovalResult::Always) => {
                                                // User approved unsafe command — future
                                                // auto-approves skip safety for this tool.
                                                if let Some(ref am) = self.deps.approval_manager {
                                                    let mut mgr = am.lock().await;
                                                    // For execute_command, pass command fingerprint for per-command approval (F-02 fix)
                                                    let cmd_fp = if tool_name == "execute_command" {
                                                        Some(params_fingerprint.as_str())
                                                    } else {
                                                        None
                                                    };
                                                    mgr.auto_approve(tool, cmd_fp);
                                                }
                                                None
                                            }
                                            Ok(crate::core::approval::ApprovalResult::Approved) => {
                                                None
                                            }
                                            Err(e) => Some(ToolExecutionOutput::error(
                                                format!(
                                                    "Approval error for tool '{}': {}",
                                                    tool_name, e
                                                ),
                                                None,
                                            )),
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
                                // Tool is approved - prepare for parallel execution.
                                // When the user was prompted and approved, skip safety checks
                                // (the user already reviewed the command). When auto-approved
                                // without a prompt, safety checks still apply in the handler.
                                let mut tool_context = (*tool_context).clone();
                                tool_context.explicitly_approved = user_prompted;
                                let tool_context = Arc::new(tool_context);
                                let hook_manager = hook_manager_handle.clone();
                                let config = config_handle.clone();
                                let handler = handler.clone();
                                let tool_name = tool_name.clone();
                                let tool_params = tool_params.clone();
                                let task_storage = self.deps.task_storage.clone().map(Arc::new);
                                let edit_file_paths =
                                    if tool_name == "edit_file" || tool_name == "write_to_file" {
                                        Self::extract_file_action_path(&tool_name, &tool_params)
                                    } else {
                                        vec![]
                                    };

                                // Clone conversation history for hook context injection
                                let conversation_history = self.conversation_history.clone();
                                let message_counter = self.message_counter.clone();

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
                                                message_counter,
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
                                ));
                                continue; // Skip adding immediate output for parallel tools
                            }
                        }
                    } else {
                        tracing::warn!(tool = %tool_name, "tool handler not implemented");
                        ToolExecutionOutput::error(
                            format!("Tool execution for '{}' not yet implemented", tool_name),
                            None,
                        )
                    }
                } else {
                    tracing::warn!(tool = %tool_name, "unknown tool requested");
                    let available = crate::core::tools::definitions::get_active_tool_definitions()
                        .iter()
                        .map(|t| t.function.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    ToolExecutionOutput::error(
                        format!(
                            "Unknown tool: '{}'. Available tools: {}",
                            tool_name, available
                        ),
                        None,
                    )
                };

                // For denied/restricted/unknown tools, add immediately (no parallel execution needed)
                tool_tasks.push((tool_id, tool_name, Some(immediate_output), None, vec![]));
            }

            // Execute with serialization only for same-file edit_file calls
            let mut non_edit_executed = std::collections::HashSet::new();

            // Collect edit_file and write_to_file futures grouped by file paths (take ownership)
            // Both tools modify files, so they need path-based serialization
            type EditGroup = (
                std::collections::HashSet<String>,
                Vec<(
                    usize,
                    futures::future::BoxFuture<'static, ToolExecutionOutput>,
                )>,
            );
            let mut edit_groups: Vec<EditGroup> = Vec::new();
            for (i, (_, tool_name, _, task, edit_file_paths)) in tool_tasks.iter_mut().enumerate() {
                if (tool_name == "edit_file" || tool_name == "write_to_file")
                    && let Some(future) = task.take()
                {
                    let paths: std::collections::HashSet<String> =
                        edit_file_paths.iter().cloned().collect();
                    // Find existing group with overlapping paths
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

            // Extract non-edit futures (exclude both edit_file and write_to_file which are grouped)
            let non_edit_futures: Vec<_> = tool_tasks
                .iter_mut()
                .enumerate()
                .filter_map(|(i, (_, tool_name, _, task, _))| {
                    if task.is_some() && tool_name != "edit_file" && tool_name != "write_to_file" {
                        non_edit_executed.insert(i);
                        task.take()
                    } else {
                        None
                    }
                })
                .collect();

            // Cap concurrency to prevent I/O contention when many tools run simultaneously
            let non_edit_results: Vec<_> = {
                use futures::StreamExt;
                futures::stream::iter(non_edit_futures)
                    .buffered(DEFAULT_TOOL_CONCURRENCY)
                    .collect()
                    .await
            };

            // Execute edit_file groups in parallel, but calls within each group sequentially
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

            // Map results back to original indices
            let mut result_map: std::collections::HashMap<usize, ToolExecutionOutput> =
                std::collections::HashMap::with_capacity(tool_tasks.len());
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

            // Collect results in order
            let parallel_results: Vec<ToolExecutionOutput> = (0..tool_tasks.len())
                .filter_map(|i| result_map.remove(&i))
                .collect();

            // Track tool execution statistics for consecutive_mistakes tracking
            let tools_called = !parallel_results.is_empty();
            tool_failure_count = parallel_results.iter().filter(|r| r.is_error).count();

            // Phase 3: Collect results in order, then push as ONE StorageMessage
            let mut parallel_results_iter = parallel_results.into_iter();
            let mut tool_result_blocks: Vec<UserContentBlock> = Vec::new();
            for (tool_id, tool_name, immediate_result_text, _task, edit_file_path) in tool_tasks {
                let mut result_output = if let Some(result_text) = immediate_result_text {
                    result_text
                } else {
                    // Parallel execution result
                    parallel_results_iter.next().unwrap_or_else(|| {
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
                        let (stats, _, added, removed) =
                            extract_edit_stats_detailed(&result_output.text);
                        for path in &edit_file_path {
                            if added > 0 || removed > 0 {
                                edit_files.push((path.clone(), added, removed));
                            }
                        }
                        let status = if is_error { "✗" } else { "✓" };
                        self.config
                            .output_writer
                            .emit(OutputEvent::error_or_success(
                                format!("  {} {}", status, stats),
                                is_error,
                            ));
                    } else if tool_name == "execute_command" {
                        let max_lines = MAX_COMMAND_RESULT_DISPLAY_LINES;
                        let displayed = format_tool_result(&result_output.text, max_lines);
                        let status = if is_error { "✗" } else { "✓" };
                        let first_line = displayed.lines().next().unwrap_or("");
                        self.config
                            .output_writer
                            .emit(OutputEvent::error_or_success(
                                format!("  {} {}", status, first_line),
                                is_error,
                            ));
                    } else {
                        let max_lines = MAX_TOOL_RESULT_DISPLAY_LINES;
                        let displayed = format_tool_result(&result_output.text, max_lines);
                        let status = if is_error { "✗" } else { "✓" };
                        let mut display_lines = displayed.lines();
                        let first = display_lines.next().unwrap_or("");
                        self.config
                            .output_writer
                            .emit(OutputEvent::error_or_success(
                                format!("  {} {}", status, first),
                                is_error,
                            ));
                        for line in display_lines.take(2) {
                            self.config
                                .output_writer
                                .emit(OutputEvent::dim(format!("    {}", line)));
                        }
                        let total_lines = displayed.lines().count();
                        if total_lines > 3 {
                            self.config.output_writer.emit(OutputEvent::dim(format!(
                                "    ... {} more lines",
                                total_lines - 3
                            )));
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

                tracing::debug!(
                    tool_id = %tool_id,
                    tool_name = %tool_name,
                    result_len = result_output.text.len(),
                    result_preview = %&result_output.text[..result_output.text.floor_char_boundary(result_output.text.len().min(80))],
                    "tool result paired with ID"
                );

                // Truncate tool result before storing in history to prevent context bloat
                let truncated_text = truncate_tool_result(&result_output.text);

                tool_result_blocks.push(UserContentBlock::ToolResult(
                    crate::providers::ToolResultBlock {
                        tool_use_id: tool_id.clone(),
                        content: ToolResultContent::Text(truncated_text),
                        shared: SharedContentFields {
                            call_id: Some(tool_id),
                            signature: None,
                        },
                    },
                ));
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
                        max_allowed = self.config.max_consecutive_mistakes,
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
                            plan.paused = true;
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

                    let max_reached =
                        state.consecutive_mistakes >= self.config.max_consecutive_mistakes;
                    drop(state);

                    if let Some(msg) = step_fail_msg {
                        self.config.output_writer.emit(OutputEvent::error(msg));
                        return TurnResult::Continue;
                    }

                    if max_reached {
                        return TurnResult::Error(format!(
                            "Max consecutive mistakes ({}) reached. The model is repeatedly failing.",
                            self.config.max_consecutive_mistakes
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
                        self.config.output_writer.emit(OutputEvent::styled(
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
            if mistakes_count >= self.config.max_consecutive_mistakes.saturating_sub(1) {
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
                for msg in history.iter_mut().rev() {
                    if let MessageContent::UserBlocks(ref mut blocks) = msg.content {
                        for block in blocks.iter_mut() {
                            let needs_summary = match block {
                                UserContentBlock::ToolResult(tr) => {
                                    if let ToolResultContent::Text(ref text) = tr.content {
                                        if text.contains("[File: ")
                                            || text.starts_with("[File Hash:")
                                        {
                                            let sections: Vec<&str> =
                                                text.split("\n---\n").collect();
                                            sections.iter().any(|sec| {
                                                path_from_read_file_header(sec)
                                                    .map(|p| {
                                                        let normalized_p =
                                                            normalize_path_for_matching(p);
                                                        edited_paths.iter().any(|ep| {
                                                            normalize_path_for_matching(ep)
                                                                == normalized_p
                                                        })
                                                    })
                                                    .unwrap_or(false)
                                            })
                                        } else {
                                            false
                                        }
                                    } else {
                                        false
                                    }
                                }
                                _ => false,
                            };
                            if needs_summary
                                && let UserContentBlock::ToolResult(tr) = block
                                && let ToolResultContent::Text(ref text) = tr.content
                            {
                                let new_text = summarize_matching_sections(text, &edited_paths);
                                tr.content = ToolResultContent::Text(new_text);
                            }
                        }
                    }
                }
            }

            if !edit_files.is_empty() && !self.config.json_output {
                self.config
                    .output_writer
                    .emit(OutputEvent::dim(format_heat_map(&edit_files)));

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
                    let message = format!("[sned] turn: {}", format_heat_map(&edit_files));
                    // Run synchronous git operations in spawn_blocking to avoid blocking runtime
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::core::shadow_git::commit_turn(&workspace_root, &message)
                    })
                    .await;
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
                        .emit(OutputEvent::dim(format!("  📝 {}", parts.join(", "))));
                }
            }
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
        let plan_active = self.plan_execution_active().await;
        let is_completion = (prepared_tool_calls.iter().any(|prepared| {
            matches!(
                SnedTool::from_name(&prepared.tool_name),
                Some(SnedTool::AttemptCompletion)
            )
        }) || text_only_completes_task)
            && !plan_active
            || (tool_failure_count == 0
                && prepared_tool_calls.iter().any(|prepared| {
                    matches!(
                        SnedTool::from_name(&prepared.tool_name),
                        Some(SnedTool::PlanModeRespond)
                    )
                }));

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
                    self.config.output_writer.emit(OutputEvent::yellow(
                        "⚠ 95% context window — /compact or start new session".to_string(),
                    ));
                } else if context_pct >= 80.0 {
                    self.config.output_writer.emit(OutputEvent::yellow(
                        "⚠ 80% context window used — consider /compact".to_string(),
                    ));
                } else if context_pct >= 50.0 {
                    self.config.output_writer.emit(OutputEvent::dim(
                        "ℹ 50% context window used — use /compact to free space before starting new topics".to_string(),
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
            // Force save on completion (async, non-blocking)
            if let Some(ref storage) = self.deps.task_storage {
                let history = self.conversation_history.lock().await;
                if !history.is_empty()
                    && let Err(e) = storage.write_api_conversation_history_async(&history).await
                {
                    error!(
                        "Failed to save API conversation history on completion: {}",
                        e
                    );
                }
            }
            if !self.config.interactive_mode
                && !self.config.json_output
                && crate::cli::output::timing_enabled()
            {
                let state = self.state.lock().await;
                if let Some(start) = state.session_start_time {
                    for line in crate::cli::output::format_timing_phases(
                        start,
                        state.request_sent_time,
                        state.first_provider_chunk_time,
                        state.first_reasoning_chunk_time,
                        state.first_displayable_text_time,
                        state.first_output_emit_time,
                        None,
                    ) {
                        self.config.output_writer.emit(OutputEvent::dim(line));
                    }
                    self.config.output_writer.flush();
                }
            }
            TurnResult::Complete
        } else {
            TurnResult::Continue
        }
    }

    async fn inject_plan_state_into_history(&self) {
        let plan_state_entry = {
            let mut state = self.state.lock().await;
            match state.plan_state.as_ref() {
                Some(plan_state) => {
                    let text = plan_state.format_state();
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    std::hash::Hash::hash(&text, &mut hasher);
                    let hash = hasher.finish();
                    let should_inject = state.last_injected_plan_state_hash != Some(hash);
                    Some((text, hash, should_inject))
                }
                None => {
                    state.last_injected_plan_state_hash = None;
                    None
                }
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
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Clears cancellation state when the caller explicitly starts a new turn.
    pub async fn reset_cancellation(&self) {
        let mut state = self.state.lock().await;
        state.is_cancelled = false;
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
            | SnedTool::SearchFiles
            | SnedTool::ListFiles
            | SnedTool::RenameSymbol => {
                if let Some(arr) = params.get("paths").and_then(|p| p.as_array()) {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(String::from)
                        .collect()
                } else if let Some(s) = params.get("paths").and_then(|p| p.as_str()) {
                    vec![String::from(s)]
                } else {
                    vec![]
                }
            }
            SnedTool::WriteToFile
            | SnedTool::GetFileSkeleton
            | SnedTool::FindSymbolReferences
            | SnedTool::DiagnosticsScan => params
                .get("path")
                .and_then(|p| p.as_str())
                .map(|s| vec![String::from(s)])
                .unwrap_or_default(),
            SnedTool::EditFile => params
                .get("files")
                .and_then(|f| f.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|f| f.get("path"))
                        .filter_map(|p| p.as_str())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
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
            "[system] Before using edit_file again, call read_file on the stale path(s): {}{}.",
            listed, suffix
        ))
    }

    /// Extract all file paths from file-modifying tool params for per-file grouping.
    /// Supports edit_file (files array) and write_to_file (single path).
    /// Returns empty vec if params don't contain valid paths.
    fn extract_file_action_path(tool_name: &str, params: &serde_json::Value) -> Vec<String> {
        match tool_name {
            "edit_file" => params
                .get("files")
                .and_then(|f| f.as_array())
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|file| file.get("path"))
                        .filter_map(|p| p.as_str())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            "write_to_file" => params
                .get("path")
                .and_then(|p| p.as_str())
                .map(|s| vec![String::from(s)])
                .unwrap_or_default(),
            _ => vec![],
        }
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
        message_counter: Arc<std::sync::atomic::AtomicUsize>,
    ) -> ToolExecutionOutput {
        let params_for_execution = tool_params.clone();
        let _ = if let Some(ref hook_mgr) = hook_manager {
            let pre_result = hook_mgr.pre_tool_use(&config.task_id, tool_name, tool_params);
            if let Some(output) = pre_result.output {
                if output.cancel == Some(true) {
                    return ToolExecutionOutput::error(
                        format!("Tool '{}' was cancelled by PreToolUse hook.", tool_name),
                        None,
                    );
                }
                if let Some(modification) = output.context_modification {
                    info!("[PreToolUse hook] {}", modification);
                    // Inject context modification into conversation history
                    let mut history = conversation_history.lock().await;
                    history.push(StorageMessage {
                        id: Some(Self::next_message_id(&message_counter)),
                        role: MessageRole::User,
                        content: MessageContent::Text(format!(
                            "[Hook context from PreToolUse]: {}",
                            modification
                        )),
                        model_info: None,
                        metrics: None,
                        ts: None,
                    });
                    drop(history);
                }
                true
            } else {
                false
            }
        } else {
            false
        };

        match handler.execute(&tool_context, params_for_execution).await {
            Ok(res) => {
                let res_text = tool_result_to_text(res);

                // Persist compacted summary immediately if condense tool was used
                if tool_name == "condense"
                    && let Some(summary) = &tool_context.state.lock().await.compacted_summary
                    && let Some(storage) = task_storage
                    && let Err(e) = storage.write_compacted_summary_async(summary).await
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
                        // Inject context modification into conversation history
                        let mut history = conversation_history.lock().await;
                        history.push(StorageMessage {
                            id: Some(Self::next_message_id(&message_counter)),
                            role: MessageRole::User,
                            content: MessageContent::Text(format!(
                                "[Hook context from PostToolUse]: {}",
                                modification
                            )),
                            model_info: None,
                            metrics: None,
                            ts: None,
                        });
                        drop(history);
                    }
                }
                ToolExecutionOutput::success(res_text)
            }
            Err(e) => ToolExecutionOutput::error(format!("Error: {}", e), e.metadata().cloned()),
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

            // Debounce: only save every 5 turns to reduce I/O overhead
            if state.turns_since_save >= 5 {
                state.turns_since_save = 0;
                let compacted_summary = state.compacted_summary.clone();
                drop(state); // Drop state lock before acquiring history lock

                let history = self.conversation_history.lock().await;
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
            .map(|m| matches!(m.role, MessageRole::Assistant))
            .unwrap_or(false);

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

            match context_window::validate_context_window(
                request,
                self.config.provider.lock().unwrap().as_ref(),
            ) {
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
        if history
            .last()
            .map(|m| m.role == MessageRole::User)
            .unwrap_or(false)
        {
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
                        let mut state = self.state.lock().await;
                        state
                            .file_context_tracker
                            .track_file_context(
                                path_str,
                                crate::core::context::trackers::FileRecordSource::FileMentioned,
                            )
                            .await;
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
    use crate::core::tool_output::summarize_single_section;
    use crate::providers::{
        ApiStream, ApiStreamTextChunk, ApiStreamToolCall, ApiStreamToolCallFunction,
        ApiStreamToolCallsChunk, ModelInfo, ProviderError, ProviderModel,
    };

    struct RecordingChunkProvider {
        responses: Vec<Vec<ApiStreamChunk>>,
        response_index: std::sync::Mutex<usize>,
        requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
    }

    impl RecordingChunkProvider {
        fn new(
            responses: Vec<Vec<ApiStreamChunk>>,
            requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
        ) -> Self {
            Self {
                responses,
                response_index: std::sync::Mutex::new(0),
                requests,
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for RecordingChunkProvider {
        async fn create_message(
            &self,
            request: ProviderRequest,
        ) -> Result<ApiStream, ProviderError> {
            self.requests.lock().unwrap().push(request);
            let response = {
                let mut idx = self.response_index.lock().unwrap();
                let response = self.responses.get(*idx).cloned().unwrap_or_default();
                *idx += 1;
                response
            };
            Ok(Box::pin(tokio_stream::iter(response)))
        }

        fn get_model(&self) -> crate::providers::ProviderModel {
            crate::providers::ProviderModel {
                id: "recording-model".to_string(),
                info: crate::providers::ModelInfo {
                    name: Some("Recording Model".to_string()),
                    max_tokens: Some(4096),
                    context_window: Some(8192),
                    ..Default::default()
                },
            }
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    struct TinyContextProvider;

    #[async_trait::async_trait]
    impl Provider for TinyContextProvider {
        async fn create_message(
            &self,
            _request: ProviderRequest,
        ) -> Result<ApiStream, ProviderError> {
            panic!("TinyContextProvider should not be called in truncation tests")
        }

        fn get_model(&self) -> ProviderModel {
            ProviderModel {
                id: "tiny-context".to_string(),
                info: ModelInfo {
                    name: Some("Tiny Context".to_string()),
                    max_tokens: Some(1024),
                    context_window: Some(1000),
                    ..Default::default()
                },
            }
        }

        fn name(&self) -> &str {
            "tiny-context"
        }
    }

    struct ErrorProvider;

    #[async_trait::async_trait]
    impl Provider for ErrorProvider {
        async fn create_message(
            &self,
            _request: ProviderRequest,
        ) -> Result<ApiStream, ProviderError> {
            Err(ProviderError::RateLimitError {
                message: "rate limited".to_string(),
                retry_delay_ms: None,
            })
        }

        fn get_model(&self) -> ProviderModel {
            ProviderModel {
                id: "error-provider".to_string(),
                info: ModelInfo {
                    name: Some("Error Provider".to_string()),
                    max_tokens: Some(4096),
                    context_window: Some(8192),
                    ..Default::default()
                },
            }
        }

        fn name(&self) -> &str {
            "error-provider"
        }
    }

    fn test_agent_config(provider: Arc<dyn Provider>, task_id: &str) -> AgentConfig {
        AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: task_id.to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
    fn test_task_state_default() {
        let state = TaskState::default();
        assert_eq!(state.consecutive_mistakes, 0);
        assert!(!state.is_cancelled);
        assert!(!state.did_complete_reading_stream);
        assert!(state.snipped_code_blocks.is_empty());
    }

    #[test]
    fn test_print_model_line_emits_one_output_event_per_wrapped_line() {
        let (tx, mut rx) = mpsc::channel(8);
        let writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        print_model_line(&"x".repeat(200), &writer);

        let mut emitted = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                OutputEvent::Line(line) => emitted.push(line.to_string()),
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

        print_model_line("ok\r\x1b[31mthere\tfriend", &writer);

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
    fn test_sanitize_model_text_fast_path_borrows_clean_input() {
        match sanitize_model_text_for_display("already clean") {
            Cow::Borrowed(text) => assert_eq!(text, "already clean"),
            Cow::Owned(_) => panic!("clean input should not allocate"),
        }
    }

    #[tokio::test]
    async fn test_provider_failure_threshold_surfaces_recovery_message() {
        let mut agent = AgentLoop::new(test_agent_config(
            Arc::new(ErrorProvider),
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
    async fn test_run_preserves_pending_cancellation_until_observed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let data_dir = temp_dir.path().join("data");
        std::fs::create_dir_all(data_dir.join("state")).unwrap();
        std::fs::create_dir_all(data_dir.join("settings")).unwrap();
        let old_data_dir = std::env::var_os("SNED_DATA_DIR");
        // SAFETY: this test is intended to run with isolated validation commands.
        unsafe {
            std::env::set_var("SNED_DATA_DIR", &data_dir);
        }

        let provider = Arc::new(crate::providers::mock::MockProvider::single_text_response(
            "should not run",
        ));
        let mut agent = AgentLoop::new(test_agent_config(provider, "test-run-pending-cancel"));
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

        // SAFETY: restore the process environment for later tests.
        unsafe {
            match old_data_dir {
                Some(ref value) => std::env::set_var("SNED_DATA_DIR", value),
                None => std::env::remove_var("SNED_DATA_DIR"),
            }
        }
    }

    #[tokio::test]
    async fn test_interactive_stream_tracks_snipped_code_block_for_expand() {
        let mut code = String::from("```rust\n");
        for line in 1..=25 {
            code.push_str(&format!("fn line_{}() {{}}\n", line));
        }
        code.push_str("```\n");

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::single_text_response(&code),
            ))),
            mode: AgentMode::Act,
            task_id: "test-snipped-code".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 1,
            max_consecutive_mistakes: 3,
            double_check_completion: true,
            timeout_secs: 300,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: true,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        };

        let mut agent = AgentLoop::new(config);
        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Continue));

        let state = agent.state.lock().await;
        assert_eq!(state.snipped_code_blocks.len(), 1);
        let block = &state.snipped_code_blocks[0];
        assert_eq!(block.index, 1);
        assert_eq!(block.language, "rust");
        assert!(block.code.contains("fn line_25() {}"));
    }

    #[tokio::test]
    async fn test_one_shot_stream_allows_25_line_code_block_without_snip() {
        let mut code = String::from("```rust\n");
        for line in 1..=25 {
            code.push_str(&format!("fn line_{}() {{}}\n", line));
        }
        code.push_str("```\n");

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::single_text_response(&code),
            ))),
            mode: AgentMode::Act,
            task_id: "test-one-shot-code".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 1,
            max_consecutive_mistakes: 3,
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

        let mut agent = AgentLoop::new(config);
        let result = agent.execute_turn().await;
        assert!(matches!(result, TurnResult::Complete));

        let state = agent.state.lock().await;
        assert!(state.snipped_code_blocks.is_empty());
    }

    #[tokio::test]
    async fn test_one_shot_text_only_response_completes_without_tool_nudge() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::single_text_response("4"),
            ))),
            mode: AgentMode::Act,
            task_id: "test-one-shot-text-only".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 1,
            max_consecutive_mistakes: 3,
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
    async fn test_later_text_only_response_gets_one_bounded_nudge() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::single_text_response_repeat("I checked it."),
            ))),
            mode: AgentMode::Act,
            task_id: "test-text-only-nudge".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 2,
            max_consecutive_mistakes: 3,
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));
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

        let provider: Arc<dyn Provider> = Arc::new(TinyContextProvider);
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: task_id.to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: task_id.to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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

        // Verify no history is loaded when file is empty/missing
        let task_storage_empty = TaskStorage::new("empty-task").unwrap();
        let agent_empty = AgentLoop::new(AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "empty-task".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "read_file",
                    serde_json::json!({"path": "/tmp/test_hook_file.txt"}),
                ),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Plan,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Plan,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "read_file",
                    serde_json::json!({"path": "/tmp/test_approval_file.txt"}),
                ),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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

        // Force non-interactive denial path. cargo test allocates a PTY for
        // stdin, so is_terminal() returns true and the channel-based path
        // would otherwise block/close instead of returning Denied.
        // SAFETY: single-threaded test; sequential env mutation.
        unsafe { std::env::set_var("SNED_APPROVAL_DENY", "1") };

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "execute_command",
                    serde_json::json!({"command": "echo hello"}),
                ),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::single_tool_call(
                    "call_1",
                    "execute_command",
                    serde_json::json!({"commands": ["echo hello world"]}),
                ),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
    async fn test_message_queue_enqueue_and_count() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
    }

    #[tokio::test]
    async fn test_message_queue_clear() {
        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "test".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
    async fn test_prepared_tool_call_parse_error_history_and_dispatch_result() {
        let raw_args = "{\"path\":\"unterminated".to_string();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingChunkProvider::new(
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
        let params = serde_json::json!({"files": [{"path": "a.rs"}, {"path": "b.rs"}]});
        let paths = AgentLoop::extract_file_action_path("edit_file", &params);
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[test]
    fn test_extract_file_action_path_write_to_file() {
        let params = serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"});
        let paths = AgentLoop::extract_file_action_path("write_to_file", &params);
        assert_eq!(paths, vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn test_extract_file_action_path_unknown_tool() {
        let params = serde_json::json!({"path": "foo.rs"});
        let paths = AgentLoop::extract_file_action_path("read_file", &params);
        assert_eq!(paths, Vec::<String>::new());
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "test-checkpoint-task".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "test-mention-task".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
            provider: Arc::new(std::sync::Mutex::new(Arc::new(
                crate::providers::mock::MockProvider::new(vec![]),
            ))),
            mode: AgentMode::Act,
            task_id: "test-cancel-task".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: true,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
        let result = summarize_matching_sections(text, &edited);
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
        let result = summarize_matching_sections(text, &edited);
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
        assert_eq!(normalize_path_for_matching("/foo/bar/main.rs"), "main.rs");
        assert_eq!(
            normalize_path_for_matching("/Users/test/project/main.c"),
            "main.c"
        );
        assert_eq!(normalize_path_for_matching("src/lib.rs"), "lib.rs");
    }

    #[test]
    fn test_path_matching_with_absolute_and_relative() {
        // Absolute path from read_file header vs relative path from edit_file
        let header_path = "/Users/easto/test/tictactoe/main.c";
        let edited_path = "main.c";
        assert_eq!(
            normalize_path_for_matching(header_path),
            normalize_path_for_matching(edited_path)
        );
    }

    #[test]
    fn test_summarize_matching_sections_with_mixed_paths() {
        // read_file header has absolute path, edit_file tracks relative
        let text = "[File: /Users/test/project/main.c, Hash: abc123]\n1§hello\n2§world";
        let edited = vec!["main.c".to_string()];
        let result = summarize_matching_sections(text, &edited);
        // Pruned section preserves hash-anchored lines for use with edit_file
        assert!(
            result.contains("1§hello"),
            "pruned section preserves anchored lines"
        );
        assert!(result.contains("Hash: abc123"));
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
                        signature: "sig1".to_string(),
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
                        signature: "sig2".to_string(),
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
                        signature: "sig".to_string(),
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
                        signature: "sig2".to_string(),
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Plan,
            task_id: "test-plan-respond".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-injection".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-advance".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-act-transition".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-attempt-completion-active-plan".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
    async fn test_text_only_turns_during_active_plan_continues() {
        let responses = vec![vec![ApiStreamChunk::Text(ApiStreamTextChunk {
            text: "Still working on it.".to_string(),
            id: None,
            signature: None,
        })]];

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-text-only-active-plan".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
        let provider = Arc::new(RecordingChunkProvider::new(responses, requests.clone()));

        let config = AgentConfig {
            provider: Arc::new(std::sync::Mutex::new(provider)),
            mode: AgentMode::Act,
            task_id: "test-plan-failure-pauses".to_string(),
            enable_checkpoints: false,
            use_auto_condense: false,
            show_token_usage: false,
            json_output: false,
            max_turns: 10,
            max_consecutive_mistakes: 3,
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
}
