//! Core types for the agent loop.
//!
//! Extracted from agent_loop.rs to keep file sizes manageable.

use crate::core::context::context_manager::ApiReqInfo;
use crate::providers::Provider;
use lru::LruCache;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

/// Maximum consecutive mistakes before asking user.
pub const MAX_CONSECUTIVE_MISTAKES: u32 = 3;

/// Maximum file content cache size (10MB) to prevent memory spikes.
pub const MAX_FILE_CONTENT_CACHE_SIZE: usize = 10 * 1024 * 1024;

/// Maximum number of entries in file content cache.
pub const MAX_FILE_CONTENT_CACHE_ENTRIES: usize = 50;

/// Default max lines to display for code blocks in interactive mode.
pub const MAX_CODE_BLOCK_DISPLAY_LINES_INTERACTIVE: usize = 15;

/// Default max lines to display for code blocks in one-shot mode.
pub const MAX_CODE_BLOCK_DISPLAY_LINES_ONE_SHOT: usize = 40;

/// A snipped code block tracked for the /expand command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippedCodeBlock {
    pub index: usize,
    pub language: String,
    pub code: String,
}

/// Exact denied tool action fingerprint for the current recovery context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeniedToolAction {
    pub tool_name: String,
    pub action_paths: Vec<String>,
    pub params_fingerprint: String,
}

/// The mode the agent is operating in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    Plan,
    Act,
}

/// Current state of the task execution.
///
/// # Concurrency
///
/// This struct is always accessed through `Arc<Mutex<TaskState>>` in the agent loop.
/// The Mutex provides necessary memory barriers for cross-thread visibility.
/// `is_cancelled_atomic` is an `Arc<AtomicBool>` that can be cloned and read
/// without the mutex for the hot-path cancellation check in the streaming loop.
#[derive(Debug)]
pub struct TaskState {
    /// Number of consecutive mistakes made by the assistant.
    pub consecutive_mistakes: u32,
    /// Whether the task has been cancelled.
    /// Always accessed under mutex lock for proper memory ordering.
    pub is_cancelled: bool,
    /// Lock-free cancellation flag for hot-path checks in the streaming loop.
    /// Mirrors `is_cancelled` — set by cancellation handlers alongside `is_cancelled`.
    /// Clone the Arc and call `load(Acquire)` to check without mutex.
    pub is_cancelled_atomic: Arc<std::sync::atomic::AtomicBool>,
    /// PIDs of running background commands (for cancellation).
    pub running_command_pids: Vec<i32>,
    /// Whether we're waiting for the first chunk.
    pub is_waiting_for_first_chunk: bool,
    /// Whether we've completed reading the stream.
    pub did_complete_reading_stream: bool,
    /// Whether the model is in a reasoning/thinking phase (no displayable output yet).
    pub reasoning_active: bool,
    /// Whether we automatically retried a failed API request.
    pub did_automatically_retry_failed_api_request: bool,
    /// Whether to use native tool calls.
    pub use_native_tool_calls: bool,
    /// Current conversation history deleted range.
    pub conversation_history_deleted_range: Option<(usize, usize)>,
    /// Whether double-check completion is enabled (default: true).
    pub double_check_completion_enabled: bool,
    /// Whether a double-check completion is currently pending (waiting for re-verification).
    pub double_check_completion_pending: bool,
    /// Consecutive assistant turns that returned text without tool calls.
    pub text_only_turns: u32,
    /// Whether strict plan mode is enabled (default: true).
    pub strict_plan_mode_enabled: bool,
    /// File context tracker for stale context warnings.
    pub file_context_tracker: crate::core::context::trackers::FileContextTracker,
    /// Last API request info from the previous turn's usage chunk.
    pub last_api_req_info: Option<ApiReqInfo>,
    /// Available skills discovered for this task (populated from system prompt context).
    pub available_skills: Vec<crate::core::context::instructions::SkillMetadata>,
    /// Whether subagents are enabled (checked from global settings).
    pub subagents_enabled: bool,
    /// Whether this task is running as a subagent (prevents recursion).
    pub is_subagent_execution: bool,
    /// Counter for debouncing conversation history saves (save every N turns).
    pub turns_since_save: u32,
    /// File content cache for cross-call coordination within a single turn.
    /// Maps absolute file paths to their latest content after edits in this turn.
    pub file_content_cache: LruCache<String, String>,
    pub first_tool_result_printed: bool,
    /// Current compacted summary (inserted into conversation history).
    pub compacted_summary: Option<crate::core::context::context_manager::CompactedSummary>,
    /// Session start time for calculating duration in session summary.
    pub session_start_time: Option<std::time::Instant>,
    /// First token emission timestamp (set when first model output reaches user).
    pub first_token_time: Option<std::time::Instant>,
    /// Timestamp when the provider request was handed off.
    pub request_sent_time: Option<std::time::Instant>,
    /// Timestamp when the first provider chunk was received.
    pub first_provider_chunk_time: Option<std::time::Instant>,
    /// Timestamp when the first reasoning chunk was received.
    pub first_reasoning_chunk_time: Option<std::time::Instant>,
    /// Timestamp when the first displayable text reached the line buffer.
    pub first_displayable_text_time: Option<std::time::Instant>,
    /// Timestamp when the first model output was emitted to the output writer.
    pub first_output_emit_time: Option<std::time::Instant>,
    /// Cumulative input tokens across all turns.
    pub cumulative_tokens_in: u32,
    /// Cumulative output tokens across all turns.
    pub cumulative_tokens_out: u32,
    /// Cumulative cache write tokens across all turns.
    pub cumulative_cache_writes: u32,
    /// Cumulative cache read tokens across all turns.
    pub cumulative_cache_reads: u32,
    /// Cumulative reasoning tokens across all turns.
    pub cumulative_reasoning_tokens: u32,
    /// Cumulative cost across all turns.
    pub cumulative_cost: f64,
    /// Consecutive API failures from provider errors (network/rate-limit/server errors).
    pub consecutive_provider_failures: u32,
    /// Number of commands executed in this session.
    pub commands_executed: u32,
    /// Number of turns in this session.
    pub turns_completed: u32,
    /// Full code blocks shortened in terminal display.
    pub snipped_code_blocks: Vec<SnippedCodeBlock>,
    /// File changes tracked across all turns for session summary.
    /// Maps absolute file path -> change stats (lines added/removed, action).
    pub session_file_changes:
        std::collections::HashMap<String, crate::core::agent_types::FileChangeStats>,
    /// The last command executed (for session summary display).
    pub last_executed_command: Option<String>,
    /// Exact file paths that must be re-read before the next edit attempt.
    pub must_reread_before_edit: HashSet<String>,
    /// Plan state for the interactive Plan -> Approve -> Act workflow.
    pub plan_state: Option<crate::core::plan_state::PlanState>,
    /// Last injected plan state hash to avoid duplicate injections.
    pub last_injected_plan_state_hash: Option<u64>,
    /// Exact denied tool calls for the current recovery context.
    pub denied_tool_actions: Vec<DeniedToolAction>,
}

/// Stats for a single file's changes in a session.
#[derive(Debug, Clone, Default)]
pub struct FileChangeStats {
    /// Lines added to the file.
    pub lines_added: u32,
    /// Lines removed from the file.
    pub lines_removed: u32,
    /// Action performed: "created" or "edited".
    pub action: String,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            consecutive_mistakes: 0,
            is_cancelled: false,
            is_cancelled_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            running_command_pids: Vec::new(),
            is_waiting_for_first_chunk: false,
            did_complete_reading_stream: false,
            reasoning_active: false,
            did_automatically_retry_failed_api_request: false,
            use_native_tool_calls: false,
            conversation_history_deleted_range: None,
            double_check_completion_enabled: true,
            double_check_completion_pending: false,
            text_only_turns: 0,
            strict_plan_mode_enabled: true,
            file_context_tracker: crate::core::context::trackers::FileContextTracker::new(),
            last_api_req_info: None,
            available_skills: Vec::new(),
            subagents_enabled: false,
            is_subagent_execution: false,
            turns_since_save: 0,
            file_content_cache: LruCache::new(
                NonZeroUsize::new(MAX_FILE_CONTENT_CACHE_ENTRIES)
                    .expect("file content cache entry limit must be non-zero"),
            ),
            first_tool_result_printed: false,
            compacted_summary: None,
            session_start_time: None,
            first_token_time: None,
            request_sent_time: None,
            first_provider_chunk_time: None,
            first_reasoning_chunk_time: None,
            first_displayable_text_time: None,
            first_output_emit_time: None,
            cumulative_tokens_in: 0,
            cumulative_tokens_out: 0,
            cumulative_cache_writes: 0,
            cumulative_cache_reads: 0,
            cumulative_reasoning_tokens: 0,
            cumulative_cost: 0.0,
            consecutive_provider_failures: 0,
            commands_executed: 0,
            turns_completed: 0,
            snipped_code_blocks: Vec::new(),
            session_file_changes: std::collections::HashMap::with_capacity(8),
            last_executed_command: None,
            must_reread_before_edit: HashSet::new(),
            plan_state: None,
            last_injected_plan_state_hash: None,
            denied_tool_actions: Vec::new(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            provider: Arc::new(Mutex::new(Arc::new(
                crate::providers::openai::OpenAiProvider::new(
                    crate::providers::openai::OpenAiConfig {
                        api_key: String::new(),
                        base_url: None,
                        model_id: String::new(),
                        model_info: None,
                        reasoning_effort: None,
                        custom_headers: None,
                        provider_name: None, // Use default "OpenAI"
                    }
                ).expect("OpenAiProvider::new() should never fail with empty config; reqwest::Client build failure indicates system resource exhaustion")
            ))),
            mode: AgentMode::Act,
            task_id: String::new(),
            enable_checkpoints: false,
            use_auto_condense: true,
            show_token_usage: false,
            json_output: false,
            max_turns: 100,
            max_consecutive_mistakes: MAX_CONSECUTIVE_MISTAKES,
            double_check_completion: true,
            timeout_secs: 30,
            track_changes: false,
            is_subagent_execution: false,
            max_context_turns: 50,
            max_tokens: None,
            interactive_mode: false,
            output_writer: Arc::new(crate::cli::output::StderrOutputWriter),
            strict_plan_mode_enabled: true,
        }
    }
}

/// Configuration for the agent loop.
#[derive(Clone)]
pub struct AgentConfig {
    /// The provider to use for API calls.
    pub provider: Arc<Mutex<Arc<dyn Provider>>>,
    /// The mode (plan or act).
    pub mode: AgentMode,
    /// Task ID (also used as ULID for telemetry).
    pub task_id: String,
    /// Whether to enable checkpoints.
    pub enable_checkpoints: bool,
    /// Whether to use auto-condense for context management.
    pub use_auto_condense: bool,
    /// Whether to show token usage after each turn.
    pub show_token_usage: bool,
    /// Whether to emit machine-readable JSON output.
    pub json_output: bool,
    /// Maximum number of turns.
    pub max_turns: u32,
    /// Maximum consecutive mistakes before escalation.
    pub max_consecutive_mistakes: u32,
    /// Whether to reject first completion attempt (double-check).
    pub double_check_completion: bool,
    /// Command timeout in seconds.
    pub timeout_secs: u64,
    /// Enable automatic change tracking (shadow git).
    pub track_changes: bool,
    /// Whether this task is running as a subagent.
    pub is_subagent_execution: bool,
    /// Maximum number of conversation turns to keep in context (default: 50).
    pub max_context_turns: usize,
    /// Optional per-request cap for provider output tokens.
    pub max_tokens: Option<u32>,
    /// Whether running in interactive shell mode (affects display behavior).
    pub interactive_mode: bool,
    /// Output writer for decoupled terminal output (ratatui migration).
    pub output_writer: crate::cli::output::OutputWriterArc,
    /// Whether strict plan mode is enabled (blocks write tools in Plan mode).
    pub strict_plan_mode_enabled: bool,
}

/// Result of a single turn in the agent loop.
#[derive(Debug)]
pub enum TurnResult {
    /// Continue to next turn.
    Continue,
    /// Task completed successfully.
    Complete,
    /// Task cancelled.
    Cancelled,
    /// Error occurred.
    Error(String),
}

impl TaskState {
    /// Insert a file into the content cache with LRU eviction.
    /// Evicts oldest entries when cache exceeds size or entry limits.
    pub fn insert_file_content(&mut self, path: String, content: String) {
        let content_size = content.len();
        let replaced_size = self
            .file_content_cache
            .peek(&path)
            .map_or(0, |existing| existing.len());
        let mut total_size = self.file_content_cache_size().saturating_sub(replaced_size);

        while total_size + content_size > MAX_FILE_CONTENT_CACHE_SIZE {
            if let Some((_key, evicted_content)) = self.file_content_cache.pop_lru() {
                total_size = total_size.saturating_sub(evicted_content.len());
            } else {
                break;
            }
        }

        self.file_content_cache.put(path, content);
    }

    fn file_content_cache_size(&self) -> usize {
        self.file_content_cache
            .iter()
            .map(|(_, content)| content.len())
            .sum()
    }

    pub fn record_denied_tool_action(&mut self, action: DeniedToolAction) {
        if !self
            .denied_tool_actions
            .iter()
            .any(|existing| existing == &action)
        {
            self.denied_tool_actions.push(action);
        }
    }

    pub fn is_denied_tool_action(
        &self,
        tool_name: &str,
        params_fingerprint: &str,
    ) -> Option<&DeniedToolAction> {
        self.denied_tool_actions.iter().find(|action| {
            action.tool_name == tool_name && action.params_fingerprint == params_fingerprint
        })
    }

    pub fn clear_denied_tool_actions(&mut self) {
        self.denied_tool_actions.clear();
    }
}

/// Errors that can occur during agent execution.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Maximum turns exceeded")]
    MaxTurnsExceeded,
    #[error("Execution error: {0}")]
    ExecutionError(String),
    #[error("Cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_file_content_preserves_recently_used_entries() {
        let mut state = TaskState::default();

        for idx in 0..MAX_FILE_CONTENT_CACHE_ENTRIES {
            state.insert_file_content(format!("file-{idx}"), format!("content-{idx}"));
        }

        assert!(
            state
                .file_content_cache
                .get(&"file-0".to_string())
                .is_some()
        );

        state.insert_file_content("file-new".to_string(), "content-new".to_string());

        assert!(
            state
                .file_content_cache
                .peek(&"file-0".to_string())
                .is_some()
        );
        assert!(
            state
                .file_content_cache
                .peek(&"file-1".to_string())
                .is_none()
        );
        assert!(
            state
                .file_content_cache
                .peek(&"file-new".to_string())
                .is_some()
        );
    }

    #[test]
    fn test_insert_file_content_respects_total_size_limit_with_lru_eviction() {
        let mut state = TaskState::default();
        let four_mb = "a".repeat(4 * 1024 * 1024);

        state.insert_file_content("a".to_string(), four_mb.clone());
        state.insert_file_content("b".to_string(), four_mb.clone());
        assert!(state.file_content_cache.get(&"a".to_string()).is_some());
        state.insert_file_content("c".to_string(), four_mb);

        assert!(state.file_content_cache.peek(&"a".to_string()).is_some());
        assert!(state.file_content_cache.peek(&"b".to_string()).is_none());
        assert!(state.file_content_cache.peek(&"c".to_string()).is_some());
        assert!(state.file_content_cache_size() <= MAX_FILE_CONTENT_CACHE_SIZE);
    }

    #[test]
    fn test_denied_tool_action_matches_exact_fingerprint_only() {
        let mut state = TaskState::default();
        state.record_denied_tool_action(DeniedToolAction {
            tool_name: "edit_file".to_string(),
            action_paths: vec!["/tmp/file.rs".to_string()],
            params_fingerprint: r#"{"files":[{"path":"file.rs"}]}"#.to_string(),
        });

        assert!(
            state
                .is_denied_tool_action("edit_file", r#"{"files":[{"path":"file.rs"}]}"#)
                .is_some()
        );
        assert!(
            state
                .is_denied_tool_action("edit_file", r#"{"files":[{"path":"other.rs"}]}"#)
                .is_none()
        );
    }

    #[test]
    fn test_clear_denied_tool_actions_resets_recovery_context() {
        let mut state = TaskState::default();
        state.record_denied_tool_action(DeniedToolAction {
            tool_name: "write_to_file".to_string(),
            action_paths: vec!["/tmp/file.rs".to_string()],
            params_fingerprint: r#"{"path":"file.rs","content":"x"}"#.to_string(),
        });
        assert_eq!(state.denied_tool_actions.len(), 1);

        state.clear_denied_tool_actions();
        assert!(state.denied_tool_actions.is_empty());
    }

    #[test]
    fn test_strict_plan_mode_enabled_default_and_restore() {
        let state = TaskState::default();
        assert!(state.strict_plan_mode_enabled);

        let mut state = TaskState {
            strict_plan_mode_enabled: false,
            ..Default::default()
        };
        assert!(!state.strict_plan_mode_enabled);

        state.strict_plan_mode_enabled = true;
        assert!(state.strict_plan_mode_enabled);
    }
}
