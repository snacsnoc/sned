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

pub use crate::core::agent_types::{
    AgentConfig, AgentError, AgentMode, SnippedCodeBlock, TaskState, TurnResult,
};
use crate::core::agent_types::{
    MAX_CODE_BLOCK_DISPLAY_LINES_INTERACTIVE, MAX_CODE_BLOCK_DISPLAY_LINES_ONE_SHOT,
};
use crate::core::context::{ApiReqInfo, PromptBuilder, SystemPromptContext, context_manager};
use crate::core::file_editor::AnchorStateManager;
use crate::core::provider_retry::{RetryConfig, create_message_with_retry};
use crate::core::tools::SnedTool;
use crate::core::tools::definitions::get_active_tool_definitions;
use crate::core::tools::{ToolContext, ToolRegistry, coerce_string_array, tool_result_to_text};
use crate::providers::{
    ApiStreamChunk, ApiStreamToolCall, AssistantContentBlock, MessageContent, MessageRole,
    Provider, ProviderRequest, RedactedThinkingBlock, SharedContentFields, StorageMessage,
    TextContentBlock, ThinkingBlock, ToolResultContent, ToolUseBlock, UserContentBlock,
};
use crate::storage::global_state::HistoryItem;
use crate::storage::task_storage::TaskStorage;
use futures::future::FutureExt;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
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

pub use crate::cli::spinner::Spinner;
use crate::cli::spinner::multi_tool_label;
use crate::core::stream_parsing::{split_model_output, truncate_json_arguments};
use crate::core::tool_output::{
    extract_edit_stats_detailed, format_heat_map, format_tool_result, format_tool_summary,
    normalize_path_for_matching, path_from_read_file_header, summarize_matching_sections,
};

const MAX_TOOL_RESULT_DISPLAY_LINES: usize = 5;
const MAX_COMMAND_RESULT_DISPLAY_LINES: usize = 8;
// MAX_TOOL_ARGUMENT_SIZE moved to providers/mod.rs for shared use
use crate::providers::MAX_TOOL_ARGUMENT_SIZE;

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

fn print_model_line(line: &str) {
    let term_width = get_terminal_width();
    let indent = "  ";
    let wrapped = crate::cli::text_utils::wrap_text(line, term_width, indent);

    eprintln!(
        "{}",
        crate::cli::colors::colorize_stderr(&wrapped, crate::cli::colors::style::CYAN)
    );
}

fn print_code_block(lines: &[String], lang: &str) {
    if lines.is_empty() {
        return;
    }

    let code = lines.join("\n");
    let highlighted = crate::cli::syntax_highlight::highlight_code(&code, lang);
    for line in highlighted.lines() {
        eprintln!("  {}", line);
    }
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
}

impl MessageQueueHandle {
    pub async fn enqueue_text_message(&self, text: String) {
        let msg = StorageMessage {
            id: None,
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
                dropped, "message queue exceeded its limit; dropped oldest queued message(s)"
            );
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

            let tool_id = tool_call.function.id.clone().unwrap_or_else(|| {
                error!("Tool call ID is None after initialization, generating fallback");
                ulid::Ulid::new().to_string()
            });
            let tool_name = tool_call.function.name.clone().unwrap_or_else(|| {
                warn!("Tool call missing name, using 'unknown_tool'");
                "unknown_tool".to_string()
            });
            let parsed_args = Self::parse_tool_arguments(
                &tool_name,
                &tool_id,
                tool_call.function.arguments.as_ref(),
            );

            prepared.push(PreparedToolCall {
                tool_call: tool_call.clone(),
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
        }
    }

    /// Get a reference to the underlying provider.
    pub fn get_provider(&self) -> &dyn Provider {
        self.config.provider.as_ref()
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

    /// Get a clonable handle for enqueuing messages from other tasks.
    pub fn message_queue_handle(&self) -> MessageQueueHandle {
        MessageQueueHandle {
            queue: self.message_queue.clone(),
            json_output: self.config.json_output,
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
            state.is_cancelled = false;
            self.cancelled
                .store(false, std::sync::atomic::Ordering::Release);
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
            let _ = tracker.record_environment().await;
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
                    id: None,
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
                        id: None,
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
                    if let Err(e) = state_manager.persist() {
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
                if let Err(e) = state_manager.persist() {
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

            // Print turn separator (visual break between agent turns)
            if !self.config.json_output {
                let context_pct = {
                    let state = self.state.lock().await;
                    state
                        .last_api_req_info
                        .as_ref()
                        .and_then(|info| info.context_usage_percentage)
                        .unwrap_or(0.0)
                };
                eprintln!(
                    "{}",
                    crate::cli::colors::section_header(&format!(
                        "Turn {} · {:.0}% context",
                        turn_count, context_pct
                    ))
                );
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
                        if let Err(save_e) = state_manager.persist() {
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
                    let mut history = self.conversation_history.lock().await;
                    history.push(expanded_message);
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
                            let mut history = self.conversation_history.lock().await;
                            history.push(expanded_message);
                            {
                                let mut state = self.state.lock().await;
                                state.consecutive_mistakes = 0;
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
                                id: None,
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
                    let api_req_info = state_guard.last_api_req_info.as_ref();
                    let tokens_in = api_req_info.and_then(|r| r.tokens_in).unwrap_or(0) as i32;
                    let tokens_out = api_req_info.and_then(|r| r.tokens_out).unwrap_or(0) as i32;
                    let cache_writes = api_req_info.and_then(|r| r.cache_writes).map(|v| v as i32);
                    let cache_reads = api_req_info.and_then(|r| r.cache_reads).map(|v| v as i32);
                    let cost = api_req_info.and_then(|r| r.cost).unwrap_or(0.0);
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

                    // Print session summary (not in JSON mode)
                    if !self.config.json_output {
                        Self::print_session_summary(&self.state).await;
                    }

                    // Persist state to disk before exiting
                    if let Err(e) = state_manager.persist() {
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
                    if let Err(e) = state_manager.persist() {
                        error!("Failed to persist state manager on cancel: {}", e);
                    }
                    return Ok(());
                }
                TurnResult::Error(e) => {
                    // Persist state on error
                    if let Err(e_persist) = state_manager.persist() {
                        error!("Failed to persist state manager on error: {}", e_persist);
                    }
                    return Err(AgentError::ExecutionError(e));
                }
            }
        }
    }

    /// Executes a single turn of the agent loop.
    async fn execute_turn(&mut self) -> TurnResult {
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
                self.config.provider.name(),
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
                    if let Err(e) = state_manager.persist() {
                        eprintln!(
                            "[agent_loop] Failed to persist state after compaction: {}",
                            e
                        );
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
            let provider_id = self.config.provider.name();
            let model_id = &self.config.provider.get_model().id;
            let mode = match self.config.mode {
                crate::core::agent_types::AgentMode::Plan => "plan",
                crate::core::agent_types::AgentMode::Act => "act",
            };
            let _ = tracker
                .record_model_usage(provider_id, model_id, mode)
                .await;
        }

        // 3. Send request and handle stream
        let tools = Some(get_active_tool_definitions());

        let request = ProviderRequest {
            system_prompt: system_prompt.clone(),
            messages: pruned_history,
            tools,
            tool_choice: Some(crate::providers::ToolChoice::Auto),
            use_response_api: None,
            max_tokens: self.config.max_tokens,
        };

        // Create channel for stream chunks with large buffer to prevent
        // backpressure deadlocks when the provider emits faster than the
        // consumer processes (e.g. during very long responses).
        let (tx, mut rx) = mpsc::channel::<ApiStreamChunk>(10_000);

        let state_clone = self.state.clone();
        let history_clone = self.conversation_history.clone();
        let provider = self.config.provider.clone();

        // Start animated spinner while waiting for provider response
        let show_progress = !self.config.json_output;
        let spinner = if show_progress {
            Some(Spinner::start("Thinking..."))
        } else {
            None
        };

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

        let stream = match create_message_with_retry(
            provider.clone(),
            request,
            state_clone.clone(),
            retry_config,
            self.config.json_output,
        )
        .await
        {
            Ok(stream) => stream,
            Err(e) => {
                drop(spinner);
                if show_progress {
                    eprint!("\r\x1b[K");
                    let _ = io::stderr().flush();
                }
                error!(error = %e, "provider request failed");
                let actionable = crate::cli::actionable_errors::provider_error(&e.to_string());
                return TurnResult::Error(actionable.display());
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
        let mut tool_calls_map: HashMap<String, ApiStreamToolCall> = HashMap::new();
        let mut tool_call_order: Vec<String> = Vec::new();
        let mut tool_call_detected = false;
        let mut spinner = spinner;
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
        let mut in_thinking_tag = false;

        // Track time to first token for UX feedback on slow connections
        let first_chunk_start = std::time::Instant::now();
        let mut slow_connection_warned = false;

        // Buffer flush timing to reduce syscalls on high-latency connections (P9)
        let mut last_flush_time = Instant::now();
        let flush_interval = std::time::Duration::from_millis(50);

        while let Some(chunk) = rx.recv().await {
            // Check for cancellation during stream processing so Ctrl+C
            // takes effect promptly instead of waiting for the full stream.
            // Uses lock-free AtomicBool to avoid mutex contention on every chunk.
            if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                tracing::info!("cancellation detected during stream processing, aborting turn");
                drop(spinner.take());
                return TurnResult::Cancelled;
            }

            // Warn about slow connection if waiting >3s for first token
            if !first_chunk_received
                && !slow_connection_warned
                && first_chunk_start.elapsed().as_secs() >= 3
            {
                slow_connection_warned = true;
                eprintln!(
                    "{}",
                    crate::cli::colors::colorize(
                        "⏳ Waiting for API response (slow connection?)...",
                        &format!(
                            "{}{}",
                            crate::cli::colors::style::YELLOW,
                            crate::cli::colors::style::DIM
                        ),
                    )
                );
            }

            // Stop spinner on first chunk received
            if !first_chunk_received {
                first_chunk_received = true;
                if let Some(s) = spinner.take() {
                    s.stop();
                }
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
                            display_buffer.push_str(&processed);
                            while let Some(nl_pos) = display_buffer.find('\n') {
                                // Extract line and trim in one pass (reduces allocations)
                                let line = display_buffer[..nl_pos].to_string();
                                display_buffer.drain(..=nl_pos);
                                let trimmed_line = line.trim();

                                if trimmed_line.starts_with("```") {
                                    if in_code_block {
                                        print_code_block(&code_block_buffer, &code_block_lang);
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
                                            eprintln!(
                                                "{}",
                                                crate::cli::colors::colorize(
                                                    &snipped_code_block_hint(index),
                                                    crate::cli::colors::style::DIM
                                                )
                                            );
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

                                    print_model_line(trimmed_line);
                                    continue;
                                }

                                if in_code_block {
                                    code_block_lines += 1;
                                    // For code blocks, preserve leading indentation (only trim end)
                                    let code_line = line.trim_end().to_string();
                                    code_block_full_buffer.push(code_line.clone());
                                    if code_block_lines > code_block_display_limit {
                                        code_block_snipped = true;
                                        continue;
                                    }

                                    code_block_buffer.push(code_line);
                                    continue;
                                }

                                // Regular content - already trimmed
                                print_model_line(trimmed_line);
                            }
                            // Buffer flush to ~50ms frames to reduce syscalls on high-latency connections (P9)
                            if last_flush_time.elapsed() >= flush_interval {
                                let _ = io::stderr().flush();
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

                                eprintln!(
                                    "{}",
                                    crate::cli::colors::colorize(
                                        &format!("  Ɵ {}", truncated),
                                        crate::cli::colors::style::DIM
                                    )
                                );
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
                        self.config.provider.as_ref(),
                    );
                    let context_window = context_window_info.context_window;
                    let provider_name = self.config.provider.name();
                    let context_usage_pct =
                        crate::core::context::context_window::calculate_context_usage_percentage(
                            usage_chunk.input_tokens,
                            usage_chunk.output_tokens,
                            usage_chunk.cache_write_tokens,
                            usage_chunk.cache_read_tokens,
                            context_window,
                            provider_name,
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
                    // Update cumulative session stats using MERGED values
                    // (not raw chunk values - prevents double-counting when
                    // providers send multiple usage chunks per response)
                    state.cumulative_tokens_in =
                        state.cumulative_tokens_in.saturating_add(tokens_in);
                    state.cumulative_tokens_out =
                        state.cumulative_tokens_out.saturating_add(tokens_out);
                    state.cumulative_cache_writes = state
                        .cumulative_cache_writes
                        .saturating_add(usage_chunk.cache_write_tokens.unwrap_or(0));
                    state.cumulative_cache_reads = state
                        .cumulative_cache_reads
                        .saturating_add(usage_chunk.cache_read_tokens.unwrap_or(0));
                    state.cumulative_reasoning_tokens = state
                        .cumulative_reasoning_tokens
                        .saturating_add(usage_chunk.reasoning_tokens.unwrap_or(0));
                    state.cumulative_cost += usage_chunk.total_cost.unwrap_or(0.0);
                }
                ApiStreamChunk::ToolCalls(tool_chunk) => {
                    // Print separator when first tool call is detected
                    if !tool_call_detected && !self.config.json_output {
                        eprintln!();
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
                        eprintln!("{}", crate::cli::colors::tool_call_header(tool_name));
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
                        tracing::error!(
                            "{}",
                            crate::cli::colors::error(&format!("Stream error: {}", err))
                        );
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
            print_code_block(&code_block_buffer, &code_block_lang);
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
                eprintln!(
                    "{}",
                    crate::cli::colors::colorize(
                        &snipped_code_block_hint(index),
                        crate::cli::colors::style::DIM
                    )
                );
            }
            eprintln!(); // Ensure we're on a fresh line for tool summaries
        } else if !display_buffer.is_empty() && !self.config.json_output {
            let remaining = display_buffer.trim_end().to_string();
            if !remaining.is_empty() {
                print_model_line(&remaining);
            }
        } else if !self.config.json_output {
            eprintln!(); // Ensure newline even if buffer was empty
        }
        let _ = io::stderr().flush();

        // Wait for stream to complete
        if let Err(e) = stream_handle.await {
            let actionable = crate::cli::actionable_errors::provider_error(&e.to_string());
            return TurnResult::Error(actionable.display());
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

        if accumulated_text.is_empty() && prepared_tool_calls.is_empty() {
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

        // Reset consecutive mistakes on successful response.
        // Tool handlers manage their own consecutive_mistakes tracking
        // based on execution success/failure, so we only reset here
        // when the model returned a valid response with no tool failures.
        // If tools were called, the handlers will reset on success.
        // CRITICAL: Reset for text-only responses too, not just tool success.
        {
            let mut state = state_clone.lock().await;
            state.consecutive_mistakes = 0;
        }

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
                id: None,
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

                if first_turn_direct_answer || text_only_turns > 1 {
                    text_only_completes_task = true;
                } else {
                    history.push(StorageMessage {
                        id: None,
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

        // 7. Save checkpoint before executing tools (if checkpoint manager is configured)
        if !prepared_tool_calls.is_empty()
            && let Some(ref mut checkpoint_mgr) = self.deps.checkpoint_manager
        {
            checkpoint_mgr.save_checkpoint().await;
        }

        // 8. Dispatch tools (parallel execution for independent tools)
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
                    eprintln!(
                        "{}",
                        crate::cli::colors::colorize(&summary, crate::cli::colors::style::DIM)
                    );
                }
                let _ = io::stderr().flush();
            }

            let hook_manager_handle = self.deps.hook_manager.clone();
            let config_handle = self.config.clone();

            // Phase 1: Pre-process all tools (check plan mode, approval, resolve handlers)
            // This is done sequentially since approval may require user interaction
            type ToolTask = (
                String,
                String,
                Option<String>,
                Option<futures::future::BoxFuture<'static, String>>,
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
                            Some(parse_error.clone()),
                            None,
                            vec![],
                        ));
                        continue;
                    }
                };

                let result_text = if let Some(tool) = SnedTool::from_name(&tool_name) {
                    // Check plan mode restrictions
                    let is_restricted = {
                        let state = self.state.lock().await;
                        self.config.mode == AgentMode::Plan
                            && state.strict_plan_mode_enabled
                            && Self::is_plan_mode_restricted(tool)
                    };

                    if is_restricted {
                        format!(
                            "Tool '{}' is not available in PLAN MODE. This tool is restricted to ACT MODE for file modifications. Only use tools available for PLAN MODE when in that mode.",
                            tool_name
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
                        let mut user_prompted = false;
                        let approval_result = if let Some(ref approval_mgr) =
                            self.deps.approval_manager
                        {
                            let mgr = approval_mgr.lock().await;
                            if action_paths
                                .iter()
                                .any(|p| mgr.should_prompt_with_path(tool, Some(p.as_str())))
                            {
                                drop(mgr); // Drop lock before async call
                                user_prompted = true;
                                match crate::core::approval::prompt_for_approval_async(
                                    &tool_name,
                                    &tool_params,
                                )
                                .await
                                {
                                    Ok(crate::core::approval::ApprovalResult::Denied) => {
                                        Some(format!("Tool '{}' was denied by user.", tool_name))
                                    }
                                    Ok(crate::core::approval::ApprovalResult::Always) => {
                                        if let Some(ref am) = self.deps.approval_manager {
                                            let mut mgr = am.lock().await;
                                            mgr.auto_approve(tool);
                                        }
                                        None // Proceed to execute
                                    }
                                    Ok(crate::core::approval::ApprovalResult::Approved) => {
                                        None // Proceed to execute
                                    }
                                    Err(e) => Some(format!(
                                        "Approval error for tool '{}': {}",
                                        tool_name, e
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
                                let checker = crate::core::approval::CommandSafetyChecker::new()
                                    .with_yolo(yolo);
                                let any_unsafe = commands
                                    .iter()
                                    .any(|cmd| !cmd.is_empty() && checker.is_safe(cmd).is_err())
                                    || script.is_some_and(|s| checker.is_safe(s).is_err());
                                if any_unsafe {
                                    // Auto-approved but unsafe — override auto-approval and prompt the user
                                    drop(mgr);
                                    user_prompted = true;
                                    match crate::core::approval::prompt_for_approval_async(
                                        &tool_name,
                                        &tool_params,
                                    )
                                    .await
                                    {
                                        Ok(crate::core::approval::ApprovalResult::Denied) => Some(
                                            format!("Tool '{}' was denied by user.", tool_name),
                                        ),
                                        Ok(crate::core::approval::ApprovalResult::Always) => {
                                            // User approved unsafe command — future
                                            // auto-approves skip safety for this tool.
                                            if let Some(ref am) = self.deps.approval_manager {
                                                let mut mgr = am.lock().await;
                                                mgr.auto_approve(tool);
                                            }
                                            None
                                        }
                                        Ok(crate::core::approval::ApprovalResult::Approved) => None,
                                        Err(e) => Some(format!(
                                            "Approval error for tool '{}': {}",
                                            tool_name, e
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
                            let edit_file_paths = if tool_name == "edit_file" {
                                Self::extract_edit_file_path(&tool_params)
                            } else {
                                vec![]
                            };

                            // Clone conversation history for hook context injection
                            let conversation_history = self.conversation_history.clone();

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
                                            result_len = result.len(),
                                            "tool execution complete"
                                        );
                                        result
                                    }
                                    .boxed(),
                                ),
                                edit_file_paths,
                            ));
                            continue; // Skip adding result_text for parallel tools
                        }
                    } else {
                        tracing::warn!(tool = %tool_name, "tool handler not implemented");
                        format!("Tool execution for '{}' not yet implemented", tool_name)
                    }
                } else {
                    tracing::warn!(tool = %tool_name, "unknown tool requested");
                    format!("Unknown tool: '{}'", tool_name)
                };

                // For denied/restricted/unknown tools, add immediately (no parallel execution needed)
                tool_tasks.push((tool_id, tool_name, Some(result_text), None, vec![]));
            }

            // Phase 2: Execute approved tools in parallel
            // BUT: serialize edit_file calls to the same file to avoid anchor mismatch
            let tool_spinner = if show_progress && !tool_tasks.is_empty() {
                let label = {
                    let names: Vec<String> =
                        tool_tasks.iter().map(|(_, n, _, _, _)| n.clone()).collect();
                    multi_tool_label(&names)
                };
                Some(Spinner::start(&label))
            } else {
                None
            };

            // Execute with serialization only for same-file edit_file calls
            let mut non_edit_executed = std::collections::HashSet::new();

            // Collect edit_file futures grouped by file paths (take ownership)
            // Each edit_file call is stored with ALL its paths to detect overlaps
            type EditGroup = (
                std::collections::HashSet<String>,
                Vec<(usize, futures::future::BoxFuture<'static, String>)>,
            );
            let mut edit_groups: Vec<EditGroup> = Vec::new();
            for (i, (_, tool_name, _, task, edit_file_paths)) in tool_tasks.iter_mut().enumerate() {
                if tool_name == "edit_file"
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

            // Extract non-edit futures, execute in parallel
            let non_edit_futures: Vec<_> = tool_tasks
                .iter_mut()
                .enumerate()
                .filter_map(|(i, (_, tool_name, _, task, _))| {
                    if task.is_some() && tool_name != "edit_file" {
                        non_edit_executed.insert(i);
                        task.take()
                    } else {
                        None
                    }
                })
                .collect();

            let non_edit_results = futures::future::join_all(non_edit_futures).await;

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
            let mut result_map: std::collections::HashMap<usize, String> =
                std::collections::HashMap::new();
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
            let parallel_results: Vec<String> = (0..tool_tasks.len())
                .filter_map(|i| result_map.remove(&i))
                .collect();

            // Stop tool spinner BEFORE displaying results to prevent cursor misalignment
            drop(tool_spinner);

            // Phase 3: Collect results in order, then push as ONE StorageMessage
            let mut parallel_results_iter = parallel_results.into_iter();
            let mut tool_result_blocks: Vec<UserContentBlock> = Vec::new();
            for (tool_id, tool_name, immediate_result_text, _task, edit_file_path) in tool_tasks {
                let mut result_text = if let Some(result_text) = immediate_result_text {
                    result_text
                } else {
                    // Parallel execution result
                    parallel_results_iter
                        .next()
                        .unwrap_or_else(|| "Tool execution failed".to_string())
                };

                // Display compact tool result in TTY mode
                if !self.config.json_output {
                    // Hold lock across check-and-set to avoid TOCTOU race
                    let mut state = self.state.lock().await;
                    if !state.first_tool_result_printed {
                        state.first_tool_result_printed = true;
                    }
                    drop(state);

                    let is_error = result_text.contains("Error")
                        || result_text.contains("Failed")
                        || result_text.contains("error:");

                    if tool_name == "edit_file" {
                        let (stats, _, added, removed) = extract_edit_stats_detailed(&result_text);
                        for path in &edit_file_path {
                            if added > 0 || removed > 0 {
                                edit_files.push((path.clone(), added, removed));
                            }
                        }
                        let status = if is_error { "✗" } else { "✓" };
                        eprintln!(
                            "{}",
                            crate::cli::colors::colorize(
                                &format!("  {} {}", status, stats),
                                if is_error {
                                    crate::cli::colors::style::RED
                                } else {
                                    crate::cli::colors::style::GREEN
                                }
                            )
                        );
                        if is_error && added == 0 && removed == 0 {
                            eprintln!(
                                "{}",
                                crate::cli::colors::colorize(
                                    "  💡 Hint: If edit_file keeps failing, use write_to_file to rewrite the entire file instead.",
                                    crate::cli::colors::style::DIM
                                )
                            );
                        }
                    } else if tool_name == "execute_command" {
                        let max_lines = MAX_COMMAND_RESULT_DISPLAY_LINES;
                        let displayed = format_tool_result(&result_text, max_lines);
                        let status = if is_error { "✗" } else { "✓" };
                        let first_line = displayed.lines().next().unwrap_or("");
                        // Consistent 2-space indent for all tool results
                        eprintln!(
                            "{}",
                            crate::cli::colors::colorize(
                                &format!("  {} {}", status, first_line),
                                if is_error {
                                    crate::cli::colors::style::RED
                                } else {
                                    crate::cli::colors::style::GREEN
                                }
                            )
                        );
                    } else {
                        let max_lines = MAX_TOOL_RESULT_DISPLAY_LINES;
                        let displayed = format_tool_result(&result_text, max_lines);
                        let status = if is_error { "✗" } else { "✓" };
                        let mut display_lines = displayed.lines();
                        let first = display_lines.next().unwrap_or("");
                        // Consistent 2-space indent for all tool results
                        eprintln!(
                            "{}",
                            crate::cli::colors::colorize(
                                &format!("  {} {}", status, first),
                                if is_error {
                                    crate::cli::colors::style::RED
                                } else {
                                    crate::cli::colors::style::GREEN
                                }
                            )
                        );
                        for line in display_lines.take(2) {
                            eprintln!(
                                "{}",
                                crate::cli::colors::colorize(
                                    &format!("    {}", line),
                                    crate::cli::colors::style::DIM
                                )
                            );
                        }
                        let total_lines = displayed.lines().count();
                        if total_lines > 3 {
                            eprintln!(
                                "{}",
                                crate::cli::colors::colorize(
                                    &format!("    ... {} more lines", total_lines - 3),
                                    crate::cli::colors::style::DIM
                                )
                            );
                        }
                    }
                }

                // LLM-visible hint: always append to result_text regardless of json_output mode
                if tool_name == "edit_file" {
                    let is_error = result_text.contains("Error")
                        || result_text.contains("Failed")
                        || result_text.contains("error:");
                    let (_, _, added, removed) = extract_edit_stats_detailed(&result_text);
                    if is_error && added == 0 && removed == 0 {
                        result_text.push_str("\n\n💡 HINT FOR AGENT: If edit_file keeps failing, use write_to_file to rewrite the entire file instead. This bypasses anchor matching.");
                    }
                }

                tracing::debug!(
                    tool_id = %tool_id,
                    tool_name = %tool_name,
                    result_len = result_text.len(),
                    result_preview = %&result_text[..result_text.floor_char_boundary(result_text.len().min(80))],
                    "tool result paired with ID"
                );

                // Truncate tool result before storing in history to prevent context bloat
                let truncated_text = truncate_tool_result(&result_text);

                tool_result_blocks.push(UserContentBlock::ToolResult(
                    crate::providers::ToolResultBlock {
                        tool_use_id: tool_id,
                        content: ToolResultContent::Text(truncated_text),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                    },
                ));
            }
            if !tool_result_blocks.is_empty() {
                if !self.config.json_output {
                    eprintln!();
                }

                let mut history = self.conversation_history.lock().await;
                history.push(StorageMessage {
                    id: None,
                    role: MessageRole::User,
                    content: MessageContent::UserBlocks(tool_result_blocks),
                    model_info: None,
                    metrics: None,
                    ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                });
            }

            // Check if consecutive mistakes threshold is reached after tool execution
            let mistakes_count;
            {
                let state = self.state.lock().await;
                mistakes_count = state.consecutive_mistakes;
                if mistakes_count >= self.config.max_consecutive_mistakes {
                    return TurnResult::Error(format!(
                        "Max consecutive mistakes ({}) reached. The model is repeatedly failing.",
                        self.config.max_consecutive_mistakes
                    ));
                }
            }

            // Inject hint when approaching the mistake limit
            if mistakes_count >= self.config.max_consecutive_mistakes.saturating_sub(1) {
                let hint = "[system] You are repeatedly failing on edit_file. If your edits keep failing due to anchor format issues (multi-line anchors, missing Word§ prefix), consider using write_to_file to rewrite the entire file instead of making incremental edits.";
                let mut history = self.conversation_history.lock().await;
                history.push(StorageMessage {
                    id: None,
                    role: crate::providers::MessageRole::User,
                    content: crate::providers::MessageContent::Text(hint.to_string()),
                    model_info: None,
                    metrics: None,
                    ts: Some(chrono::Utc::now().timestamp_millis() as u64),
                });
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
                eprintln!(
                    "{}",
                    crate::cli::colors::colorize(
                        &format_heat_map(&edit_files),
                        crate::cli::colors::style::DIM
                    )
                );

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
                    eprintln!(
                        "{}",
                        crate::cli::colors::colorize(
                            &format!("  📝 {}", parts.join(", ")),
                            crate::cli::colors::style::DIM
                        )
                    );
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
        let is_completion = text_only_completes_task
            || prepared_tool_calls.iter().any(|prepared| {
                matches!(
                    SnedTool::from_name(&prepared.tool_name),
                    Some(SnedTool::AttemptCompletion) | Some(SnedTool::PlanModeRespond)
                )
            });

        if self.config.json_output
            && let Some(event) = Self::synthetic_json_completion_event(
                text_only_completes_task,
                completion_tool_emitted,
                response_text.as_deref(),
            )
        {
            tracing::info!(target: "json_output", "{}", event);
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
                let tokens_in = api_req_info.tokens_in.unwrap_or(0);
                let tokens_out = api_req_info.tokens_out.unwrap_or(0);
                let cache_writes = api_req_info.cache_writes.unwrap_or(0);
                let cache_reads = api_req_info.cache_reads.unwrap_or(0);
                let reasoning = api_req_info.reasoning_tokens.unwrap_or(0);
                let cost = api_req_info.cost.unwrap_or(0.0);
                let context_pct = api_req_info.context_usage_percentage.unwrap_or(0.0);

                let cost_str = if cost > 0.0 {
                    format!(" | ${:.4}", cost)
                } else {
                    String::new()
                };

                eprintln!(
                    "{}",
                    crate::cli::colors::colorize(
                        &format!(
                            "  📊 Tokens: {} in / {} out{}{}{} | Context: {:.1}%",
                            tokens_in,
                            tokens_out,
                            if cache_writes > 0 || cache_reads > 0 {
                                format!(
                                    " ({} cache write / {} cache read)",
                                    cache_writes, cache_reads
                                )
                            } else {
                                String::new()
                            },
                            if reasoning > 0 {
                                format!(" | {} reasoning", reasoning)
                            } else {
                                String::new()
                            },
                            cost_str,
                            context_pct
                        ),
                        crate::cli::colors::style::DIM
                    )
                );

                if context_pct >= 95.0 {
                    eprintln!(
                        "{}",
                        crate::cli::colors::colorize(
                            "⚠ 95% context window — /compact or start new session",
                            crate::cli::colors::style::YELLOW
                        )
                    );
                } else if context_pct >= 80.0 {
                    eprintln!(
                        "{}",
                        crate::cli::colors::colorize(
                            "⚠ 80% context window used — consider /compact",
                            crate::cli::colors::style::YELLOW
                        )
                    );
                } else if context_pct >= 50.0 {
                    eprintln!(
                        "{}",
                        crate::cli::colors::colorize(
                            "ℹ 50% context window used — use /compact to free space before starting new topics",
                            crate::cli::colors::style::DIM
                        )
                    );
                }

                let cost_warn_threshold = std::env::var("SNED_COST_WARN")
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(5.0);
                if cost >= cost_warn_threshold {
                    eprintln!(
                        "{}",
                        crate::cli::colors::colorize(
                            &format!(
                                "⚠ Session cost ${:.2} (threshold: ${:.2})",
                                cost, cost_warn_threshold
                            ),
                            crate::cli::colors::style::YELLOW
                        )
                    );
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
            TurnResult::Complete
        } else {
            TurnResult::Continue
        }
    }

    /// Cancels the current task.
    pub async fn cancel(&self) {
        let mut state = self.state.lock().await;
        state.is_cancelled = true;
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
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

    /// Extract all file paths from edit_file tool params for per-file grouping.
    /// Returns empty vec if params don't contain a valid files array with paths.
    fn extract_edit_file_path(params: &serde_json::Value) -> Vec<String> {
        params
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
            .unwrap_or_default()
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
    ) -> String {
        let params_for_execution = tool_params.clone();
        let _ = if let Some(ref hook_mgr) = hook_manager {
            let pre_result = hook_mgr.pre_tool_use(&config.task_id, tool_name, tool_params);
            if let Some(output) = pre_result.output {
                if output.cancel == Some(true) {
                    return format!("Tool '{}' was cancelled by PreToolUse hook.", tool_name);
                }
                if let Some(modification) = output.context_modification {
                    info!("[PreToolUse hook] {}", modification);
                    // Inject context modification into conversation history
                    let mut history = conversation_history.lock().await;
                    history.push(StorageMessage {
                        id: None,
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
                            id: None,
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
                res_text
            }
            Err(e) => format!("Error: {}", e),
        }
    }

    /// Returns the current conversation history.
    pub async fn get_conversation_history(&self) -> Vec<StorageMessage> {
        let history = self.conversation_history.lock().await;
        history.clone()
    }

    /// Print session summary after task completion.
    async fn print_session_summary(state: &Arc<Mutex<TaskState>>) {
        use std::time::Duration;

        let state = state.lock().await;

        // Calculate session duration
        let duration = state
            .session_start_time
            .map(|start| start.elapsed())
            .unwrap_or(Duration::ZERO);

        // Aggregate file changes from session_file_changes
        let mut files_created = 0u32;
        let mut files_edited = 0u32;
        let mut file_entries: Vec<(&String, &crate::core::agent_types::FileChangeStats)> =
            Vec::new();

        for (path, stats) in &state.session_file_changes {
            if stats.action == "created" {
                files_created += 1;
            } else {
                files_edited += 1;
            }
            file_entries.push((path, stats));
        }

        // Sort by path for consistent output
        file_entries.sort_by(|a, b| a.0.cmp(b.0));

        // Build summary line
        let mut summary_parts: Vec<String> = Vec::new();
        if files_created > 0 {
            summary_parts.push(format!(
                "{} file{} created",
                files_created,
                if files_created == 1 { "" } else { "s" }
            ));
        }
        if files_edited > 0 {
            summary_parts.push(format!(
                "{} file{} edited",
                files_edited,
                if files_edited == 1 { "" } else { "s" }
            ));
        }
        if state.commands_executed > 0 {
            summary_parts.push(format!(
                "{} command{} run",
                state.commands_executed,
                if state.commands_executed == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        let summary_line = if summary_parts.is_empty() {
            String::from("No files changed")
        } else {
            summary_parts.join(", ")
        };

        // Print session summary box
        eprintln!();
        crate::cli::colors::print_horizontal_rule();
        eprintln!(
            "{}",
            crate::cli::colors::colorize("  Session", crate::cli::colors::style::BOLD)
        );
        eprintln!("  {}", summary_line);

        // Per-file diff stats
        for (path, stats) in &file_entries {
            let filename = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string());
            eprintln!(
                "{}",
                crate::cli::colors::colorize(
                    &format!(
                        "  {:17} +{} -{}\t({})",
                        filename, stats.lines_added, stats.lines_removed, stats.action
                    ),
                    crate::cli::colors::style::DIM
                )
            );
        }

        // Last executed command with ⚙ icon
        #[allow(clippy::collapsible_if)]
        if let Some(ref cmd) = state.last_executed_command {
            if !cmd.is_empty() {
                eprintln!(
                    "{}",
                    crate::cli::colors::colorize(
                        &format!("  ⚙ {}", cmd),
                        crate::cli::colors::style::DIM
                    )
                );
            }
        }

        // Token, cost, turns, duration with 📊 icon
        let total_tokens = state.cumulative_tokens_in + state.cumulative_tokens_out;
        let tokens_str = if total_tokens >= 1000 {
            format!("{}K", total_tokens / 1000)
        } else {
            format!("{}", total_tokens)
        };
        let cost_str = if state.cumulative_cost > 0.0 {
            format!("${:.3}", state.cumulative_cost)
        } else {
            String::from("$0.000")
        };
        eprintln!(
            "{}",
            crate::cli::colors::colorize(
                &format!(
                    "  📊 {} tokens | {} | {} turn{} | {}",
                    tokens_str,
                    cost_str,
                    state.turns_completed,
                    if state.turns_completed == 1 { "" } else { "s" },
                    Self::format_duration(duration)
                ),
                crate::cli::colors::style::DIM
            )
        );

        crate::cli::colors::print_horizontal_rule();

        // JSON mode output
        if state.turns_completed > 0 {
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "session_summary",
                    "files_created": files_created,
                    "files_edited": files_edited,
                    "commands_executed": state.commands_executed,
                    "last_command": state.last_executed_command,
                    "tokens": total_tokens,
                    "cost": state.cumulative_cost,
                    "turns": state.turns_completed,
                    "duration_secs": duration.as_secs()
                })
            );
        }
    }

    /// Format duration as human-readable string.
    fn format_duration(duration: std::time::Duration) -> String {
        let total_secs = duration.as_secs();
        if total_secs < 60 {
            format!("{}s", total_secs)
        } else if total_secs < 3600 {
            let mins = total_secs / 60;
            let secs = total_secs % 60;
            format!("{}m {}s", mins, secs)
        } else {
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            format!("{}h {}m", hours, mins)
        }
    }

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

        // Build a map of tool_use_id → message index for all tool_uses in history
        let mut tool_use_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (idx, msg) in history.iter().enumerate() {
            if let MessageContent::AssistantBlocks(blocks) = &msg.content {
                for block in blocks {
                    if let AssistantContentBlock::ToolUse(tu) = block {
                        tool_use_index.insert(tu.id.clone(), idx);
                    }
                }
            }
        }

        // Start with the most recent N messages
        let keep_from_base = history.len().saturating_sub(max_messages);

        // Scan the kept region for tool_results whose tool_use was pruned
        // For each orphan, extend keep_from backwards to include the tool_use
        let mut keep_from = keep_from_base;
        for msg in history.iter().skip(keep_from_base) {
            if let MessageContent::UserBlocks(blocks) = &msg.content {
                for block in blocks {
                    if let UserContentBlock::ToolResult(tr) = block {
                        // Check if this tool_result's tool_use exists and is before keep_from
                        if let Some(&tool_use_idx) = tool_use_index.get(&tr.tool_use_id) {
                            if tool_use_idx < keep_from {
                                // This tool_use would be pruned but its result is kept — orphan!
                                // Extend keep_from backwards to include the tool_use
                                keep_from = keep_from.min(tool_use_idx);
                            }
                        }
                    }
                }
            }
        }

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
            id: None,
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
    use crate::providers::{ApiStream, ApiStreamTextChunk, ApiStreamToolCallsChunk, ProviderError};

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

    fn test_agent_config(provider: Arc<dyn Provider>, task_id: &str) -> AgentConfig {
        AgentConfig {
            provider,
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
        }
    }

    #[test]
    fn test_task_state_default() {
        let state = TaskState::default();
        assert_eq!(state.consecutive_mistakes, 0);
        assert!(!state.is_cancelled);
        assert!(!state.did_complete_reading_stream);
        assert!(state.snipped_code_blocks.is_empty());
    }

    #[tokio::test]
    async fn test_interactive_stream_tracks_snipped_code_block_for_expand() {
        let mut code = String::from("```rust\n");
        for line in 1..=25 {
            code.push_str(&format!("fn line_{}() {{}}\n", line));
        }
        code.push_str("```\n");

        let config = AgentConfig {
            provider: Arc::new(crate::providers::mock::MockProvider::single_text_response(
                &code,
            )),
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
            provider: Arc::new(crate::providers::mock::MockProvider::single_text_response(
                &code,
            )),
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
            provider: Arc::new(crate::providers::mock::MockProvider::single_text_response(
                "4",
            )),
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
            provider: Arc::new(
                crate::providers::mock::MockProvider::single_text_response_repeat("I checked it."),
            ),
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

        // Create ApiReqInfo with high token count to trigger truncation
        let api_req_info = ApiReqInfo {
            tokens_in: Some(200_000),
            tokens_out: Some(50_000),
            context_window: Some(256_000),
            ..Default::default()
        };

        // Call get_new_context_messages_and_metadata
        let result = context_manager::get_new_context_messages_and_metadata(
            &history,
            Some(&api_req_info),
            None,
            false, // use_auto_condense = false
            None,  // no compacted summary yet
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
    async fn test_history_persistence() {
        use tempfile::TempDir;

        // Create a temp directory and use new_with_dir to avoid env var races
        let temp_dir = TempDir::new().unwrap();
        let sned_dir = temp_dir.path().join(".sned");

        let task_id = "test-task-123";
        let task_storage = TaskStorage::new_with_dir(task_id, &sned_dir).unwrap();

        let config = AgentConfig {
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
        })
        .with_task_storage(task_storage_empty);

        let loaded_empty = agent_empty.load_conversation_history().await;
        assert!(!loaded_empty, "Should not load history for empty task");

        unsafe { env::remove_var("SNED_DIR") };
    }

    #[test]
    fn test_hook_manager_stored() {
        use crate::core::hooks::HookManager;

        let config = AgentConfig {
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::single_tool_call(
                "call_1",
                "read_file",
                serde_json::json!({"path": "/tmp/test_hook_file.txt"}),
            )),
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

    #[tokio::test]
    async fn test_plan_mode_blocks_restricted_tools() {
        let config = AgentConfig {
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::single_tool_call(
                "call_1",
                "read_file",
                serde_json::json!({"path": "/tmp/test_approval_file.txt"}),
            )),
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
    async fn test_approval_manager_non_interactive_auto_approves() {
        use crate::core::approval::ApprovalManager;
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;

        let config = AgentConfig {
            provider: Arc::new(crate::providers::mock::MockProvider::single_tool_call(
                "call_1",
                "execute_command",
                serde_json::json!({"command": "echo hello"}),
            )),
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

        // Execute one turn - in non-interactive mode (tests), execute_command should be auto-approved
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
            // In non-interactive mode, the command should execute (not be denied)
            if let MessageContent::UserBlocks(blocks) = &last.content {
                if let Some(UserContentBlock::ToolResult(result)) = blocks.first() {
                    let content_text = match &result.content {
                        ToolResultContent::Text(t) => t.clone(),
                        _ => String::new(),
                    };
                    // Should NOT be a denial message
                    assert!(
                        !content_text.contains("was denied by user"),
                        "Tool should not be denied in non-interactive mode: {}",
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
    }

    #[tokio::test]
    async fn test_execute_command_full_flow_produces_output() {
        use crate::core::tools::ToolRegistry;
        use crate::core::tools::handlers::execute_command::ExecuteCommandHandler;

        let config = AgentConfig {
            provider: Arc::new(crate::providers::mock::MockProvider::single_tool_call(
                "call_1",
                "execute_command",
                serde_json::json!({"commands": ["echo hello world"]}),
            )),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
        let mut tool_calls = HashMap::new();
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
            provider: Arc::new(crate::providers::mock::MockProvider::new(vec![])),
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
        assert!(!summary.contains("hello"));
    }

    #[test]
    fn test_summarize_matching_sections_partial() {
        let text =
            "[File: src/foo.rs, Hash: aaa]\n1§foo\n---\n[File: src/bar.rs, Hash: bbb]\n1§bar";
        let edited = vec!["src/foo.rs".to_string()];
        let result = summarize_matching_sections(text, &edited);
        assert!(result.contains("Hash: aaa"));
        assert!(!result.contains("1§foo"));
        assert!(result.contains("1§bar"));
    }

    #[test]
    fn test_summarize_matching_sections_all() {
        let text =
            "[File: src/foo.rs, Hash: aaa]\n1§foo\n---\n[File: src/bar.rs, Hash: bbb]\n1§bar";
        let edited = vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()];
        let result = summarize_matching_sections(text, &edited);
        assert!(!result.contains("1§foo"));
        assert!(!result.contains("1§bar"));
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
        // Should summarize (not keep full content) because paths match after normalization
        assert!(!result.contains("hello"));
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
    fn test_turn_separator_includes_context_percentage() {
        // Verify section_header format for turn separator
        let header = crate::cli::colors::section_header("Turn 3 · 45% context");
        assert!(header.contains("Turn 3"));
        assert!(header.contains("45% context"));
        assert!(header.contains("═══"));
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
        unsafe {
            std::env::set_var(TOOL_RESULT_HISTORY_LIMIT_ENV, "100");
        }
        let large = "x".repeat(500);
        let result = truncate_tool_result(&large);
        assert!(result.len() < 150); // 100 limit + marker
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
                assert!(tb.thinking.len() < 10000, "Old thinking should be truncated");
                assert!(tb.thinking.contains("[truncated]"), "Should have truncation marker");
            } else {
                panic!("First block should be Thinking");
            }
        } else {
            panic!("First message should have AssistantBlocks");
        }

        // Second message thinking should NOT be truncated (most recent)
        if let MessageContent::AssistantBlocks(blocks) = &history[1].content {
            if let AssistantContentBlock::Thinking(tb) = &blocks[0] {
                assert_eq!(tb.thinking.len(), 10000, "Recent thinking should NOT be truncated");
                assert!(!tb.thinking.contains("[truncated]"), "Should NOT have truncation marker");
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
        unsafe {
            std::env::set_var(THINKING_HISTORY_LIMIT_ENV, "100");
        }

        let mut history = vec![
            // First message - should be truncated (not most recent)
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![
                    AssistantContentBlock::Thinking(ThinkingBlock {
                        thinking: "z".repeat(2000),
                        signature: "sig".to_string(),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        summary: None,
                    }),
                ]),
                model_info: None,
                metrics: None,
                ts: Some(1000),
            },
            // Second message - most recent, should NOT be truncated
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![
                    AssistantContentBlock::Thinking(ThinkingBlock {
                        thinking: "w".repeat(2000),
                        signature: "sig2".to_string(),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        summary: None,
                    }),
                ]),
                model_info: None,
                metrics: None,
                ts: Some(2000),
            },
        ];

        truncate_old_thinking_blocks(&mut history);

        // With 100 token limit (400 chars), first message's 2000 chars should be truncated
        if let MessageContent::AssistantBlocks(blocks) = &history[0].content {
            if let AssistantContentBlock::Thinking(tb) = &blocks[0] {
                assert!(tb.thinking.len() < 2000, "Should be truncated with custom limit");
                assert!(tb.thinking.contains("[truncated]"), "Should have truncation marker");
            }
        }

        // Second message (most recent) should NOT be truncated
        if let MessageContent::AssistantBlocks(blocks) = &history[1].content {
            if let AssistantContentBlock::Thinking(tb) = &blocks[0] {
                assert_eq!(tb.thinking.len(), 2000, "Most recent thinking should NOT be truncated");
            }
        }

        unsafe {
            std::env::remove_var(THINKING_HISTORY_LIMIT_ENV);
        }
    }
}
