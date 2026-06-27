//! Condense tool handler for sned CLI.
//!
//!
//! User-triggered context compaction variant (e.g. `/compact`).

use crate::core::context::context_manager::{self, CompactedSummary, TruncationKeep};
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use std::future::Future;
use std::pin::Pin;
use crate::providers::{MessageRole, StorageMessage};

/// Condense tool handler.
#[derive(Debug, Clone, Default)]
pub struct CondenseHandler;

impl CondenseHandler {
    #[must_use] 
    pub fn new() -> Self {
        Self
    }

    /// Execute condense with PreCompact hook support and interactive user approval.
    ///
    /// 1. Show condensation summary to user
    /// 2. Wait for user response
    /// 3. Empty response = accept and compact
    /// 4. Non-empty response = user feedback, do NOT compact
    pub async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let context = params
            .get("context")
            .and_then(|c| c.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput("Missing required parameter: context".to_string())
            })?;

        // Validate summary is non-empty
        if context.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "Summary context cannot be empty".to_string(),
            ));
        }

        // Execute PreCompact hook BEFORE acquiring state lock to avoid holding lock during blocking I/O.
        let mut hook_context_modification: Option<String> = None;
        if let Some(ref hook_mgr) = ctx.hook_manager {
            let result = hook_mgr.pre_compact(&ctx.task_id, "<conversation_history>");
            if result.error.is_none() {
                if let Some(output) = result.output
                    && let Some(modification) = output.context_modification
                {
                    hook_context_modification = Some(modification);
                    tracing::info!(
                        "[PreCompact] Hook provided context modification for task {}",
                        ctx.task_id
                    );
                }
            } else {
                tracing::warn!(
                    "[PreCompact] Hook execution failed, continuing with compaction: {}",
                    result.error.unwrap_or_default()
                );
            }
        }

        // Augment summary with hook modification if present
        let final_summary = if let Some(modification) = hook_context_modification {
            format!(
                "{context}\n\n[Context Modification from PreCompact Hook]\n{modification}"
            )
        } else {
            context.to_string()
        };

        // INTERACTIVE APPROVAL: Show summary and wait for user response
        if !ctx.json_output {
            use crate::cli::output::OutputEvent;
            use ratatui::style::{Modifier, Style};
            let timeout_secs = crate::core::approval::followup_timeout().as_secs();
            use crate::cli::tui::theme::{ACCENT, WARNING_FG};
            ctx.output_writer.emit(OutputEvent::tool_output_line(
                "\n[Sned wants to condense the conversation]",
                Style::default().fg(WARNING_FG),
            ));
            ctx.output_writer.emit(OutputEvent::tool_output_line(
                format!("{final_summary}\n"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            ctx.output_writer.emit(OutputEvent::tool_output_line(
                "Press Enter to accept, or provide feedback: ",
                Style::default().fg(ACCENT),
            ));
            ctx.output_writer.emit(OutputEvent::tool_output_line(
                format!("(waiting up to {timeout_secs}s for your response)"),
                Style::default().add_modifier(Modifier::DIM),
            ));
            ctx.output_writer.flush();

            // Use channel-based input to avoid blocking tokio worker and fighting TUI stdin
            // Same pattern as ask_followup_question and prompt_for_approval
            let (sender, receiver) = std::sync::mpsc::channel();
            crate::core::approval::set_followup_question_active(ctx.task_id.as_str(), true);
            crate::core::approval::set_followup_sender(ctx.task_id.as_str(), sender);

            // Use recv_timeout to avoid blocking the TUI event loop indefinitely.
            // Same pattern as ask_followup_question and other followup prompts.
            let response_result = tokio::task::spawn_blocking(move || {
                receiver.recv_timeout(crate::core::approval::followup_timeout())
            })
            .await;

            // Clean up followup state regardless of outcome
            crate::core::approval::clear_followup_sender(ctx.task_id.as_str());
            crate::core::approval::set_followup_question_active(ctx.task_id.as_str(), false);

            let user_response = match response_result {
                Ok(Ok(r)) => r.trim().to_string(),
                Ok(Err(_)) | Err(_) => String::new(), // Timeout or channel closed = no response
            };

            // If user provided feedback, do NOT compact - return feedback as result
            if !user_response.is_empty() {
                tracing::info!("User provided feedback on condensation instead of accepting");
                return Ok(format!(
                    "User provided feedback on the condensed conversation summary:\n<feedback>\n{user_response}\n</feedback>"
                ));
            }
            // Empty response = user accepted, proceed with compaction
        } else {
            // JSON mode: cannot read stdin, auto-accept
            tracing::warn!("Condense tool auto-accepted in JSON mode (cannot read stdin)");
        }

        // Now acquire state lock for validation and storage
        let mut state = ctx.state.lock().await;
        state.consecutive_mistakes = 0;

        // Soft guard: allow re-compaction only if enough new messages exist since last compact.
        // This prevents summarizing a summary with no new content.
        if state.compacted_summary.is_some() {
            let active_messages =
                if let Some(history_array) = params.get("history").and_then(|h| h.as_array()) {
                    let range_end = state
                        .conversation_history_deleted_range
                        .map_or(0, |r| r.1 + 1);
                    history_array.len().saturating_sub(range_end)
                } else {
                    0
                };

            if active_messages <= 2 {
                return Err(ToolError::InvalidInput(format!(
                    "Not enough new messages to re-compact ({active_messages} new message(s) since last compaction). \
                         Continue the conversation or start a new task before compacting again."
                )));
            }
            // Enough new messages — proceed with re-compaction (will overwrite below)
        }

        // Capture whether this is a re-compaction before overwriting
        let is_recompaction = state.compacted_summary.is_some();

        // Count messages that will be compacted
        let messages_compacted = params
            .get("history")
            .and_then(|h| h.as_array())
            .map_or(0, std::vec::Vec::len);

        // Create and store compacted summary (overwrites previous summary on re-compaction)
        let summary = CompactedSummary::new(final_summary.clone(), messages_compacted);
        state.compacted_summary = Some(summary);

        // Trigger context truncation via ContextManager when history is provided.
        // The caller (agent loop) is expected to inject the conversation history
        // into params as a JSON array of StorageMessage objects.
        if let Some(history_array) = params.get("history").and_then(|h| h.as_array()) {
            let history: Vec<StorageMessage> = history_array
                .iter()
                .filter_map(|v| match serde_json::from_value::<StorageMessage>(v.clone()) {
                    Ok(msg) => Some(msg),
                    Err(e) => {
                        let preview = serde_json::to_string(&v)
                            .unwrap_or_else(|_| "<unserializable>".to_string());
                        let truncated: String = preview.chars().take(120).collect();
                        tracing::warn!(
                            "condense: dropped history entry that failed deserialization: {} | value: {}",
                            e, truncated
                        );
                        None
                    }
                })
                .collect();

            if !history.is_empty() {
                // If the last message is from the assistant, we keep the last two
                // messages (so the assistant's summary message isn't lost).
                let keep = if history
                    .last()
                    .is_some_and(|m| m.role == MessageRole::Assistant)
                {
                    TruncationKeep::LastTwo
                } else {
                    TruncationKeep::None
                };

                let range = context_manager::get_next_truncation_range(
                    &history,
                    state.conversation_history_deleted_range,
                    keep,
                );

                state.conversation_history_deleted_range = Some(range);
            }
        }

        if is_recompaction {
            Ok(format!(
                "Conversation re-compacted. Continuing from updated summary:\n\n{final_summary}"
            ))
        } else {
            Ok(format!(
                "Conversation condensed. Continuing from summary:\n\n{final_summary}"
            ))
        }
    }
}

impl ToolHandler for CondenseHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            Self::execute(&Self, &ctx, params)
                .await
                .map(serde_json::Value::String)
        })
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[condense]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;

    #[test]
    fn test_condense_handler_creation() {
        let handler = CondenseHandler::new();
        assert_eq!(format!("{:?}", handler), "CondenseHandler");
    }

    #[tokio::test]
    async fn test_condense_missing_context() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = handler.execute(&ctx, serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("context"));
    }

    #[tokio::test]
    async fn test_condense_success_without_history() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = handler
            .execute(&ctx, serde_json::json!({"context": "Condensed summary"}))
            .await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Condensed summary"));
        assert!(text.contains("Conversation condensed"));
        let state_guard = state.lock().await;
        assert!(state_guard.conversation_history_deleted_range.is_none());
    }

    #[tokio::test]
    async fn test_condense_triggers_context_manager() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let history: Vec<StorageMessage> = (0..10)
            .map(|i| StorageMessage {
                id: Some(format!("msg_{}", i)),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: crate::providers::MessageContent::Text(format!("message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            })
            .collect();

        assert_eq!(history.last().unwrap().role, MessageRole::Assistant);

        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "Condensed summary",
                    "history": history,
                }),
            )
            .await;

        assert!(result.is_ok());
        let state_guard = state.lock().await;
        assert!(state_guard.conversation_history_deleted_range.is_some());

        let (start, end) = state_guard.conversation_history_deleted_range.unwrap();
        assert_eq!(start, 2);
        assert_eq!(end, 7);
    }

    #[tokio::test]
    async fn test_condense_keep_strategy_none_when_last_is_user() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let history: Vec<StorageMessage> = (0..9)
            .map(|i| StorageMessage {
                id: Some(format!("msg_{}", i)),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: crate::providers::MessageContent::Text(format!("message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            })
            .collect();

        assert_eq!(history.last().unwrap().role, MessageRole::User);

        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "Condensed summary",
                    "history": history,
                }),
            )
            .await;

        assert!(result.is_ok());
        let state_guard = state.lock().await;
        assert!(state_guard.conversation_history_deleted_range.is_some());

        let (start, end) = state_guard.conversation_history_deleted_range.unwrap();
        assert_eq!(start, 2);
        assert_eq!(end, 7);
    }

    #[tokio::test]
    async fn test_condense_corrupt_history_entry_is_skipped() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let valid_msg = serde_json::json!({
            "id": "msg_0",
            "role": "user",
            "content": {"type": "text", "text": "hello"},
        });

        let corrupt_entry = serde_json::json!({"garbage": true, "missing_required_fields": null});
        let valid_msg2 = serde_json::json!({
            "id": "msg_1",
            "role": "assistant",
            "content": {"type": "text", "text": "world"},
        });

        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "Condensed summary",
                    "history": [valid_msg, corrupt_entry, valid_msg2],
                }),
            )
            .await;

        assert!(result.is_ok());
        let state_guard = state.lock().await;
        assert!(
            state_guard.conversation_history_deleted_range.is_none(),
            "history with <3 valid messages should not trigger truncation"
        );
    }

    #[tokio::test]
    async fn test_condense_validates_empty_summary() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = handler
            .execute(&ctx, serde_json::json!({"context": "   "}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn test_condense_stores_compacted_summary() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let history: Vec<StorageMessage> = (0..5)
            .map(|i| StorageMessage {
                id: Some(format!("msg_{}", i)),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: crate::providers::MessageContent::Text(format!("message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            })
            .collect();

        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "Test summary",
                    "history": history,
                }),
            )
            .await;

        assert!(result.is_ok());
        let state_guard = state.lock().await;
        assert!(state_guard.compacted_summary.is_some());
        let summary = state_guard.compacted_summary.clone().unwrap();
        assert_eq!(summary.summary_text, "Test summary");
        assert_eq!(summary.messages_compacted, 5);
    }

    #[tokio::test]
    async fn test_condense_soft_guard_blocks_with_few_new_messages() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        // Use json_output=true to bypass interactive approval in tests
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true, // json_output
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // First compaction should succeed
        let result1 = handler
            .execute(&ctx, serde_json::json!({"context": "First summary"}))
            .await;
        assert!(result1.is_ok());

        // Second compaction without history should fail (soft guard: 0 active messages)
        let result2 = handler
            .execute(&ctx, serde_json::json!({"context": "Second summary"}))
            .await;
        assert!(result2.is_err());
        let err_msg = result2.unwrap_err().to_string();
        assert!(
            err_msg.contains("new message(s) since last compaction"),
            "Expected soft guard error, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_condense_recompaction_succeeds_with_enough_new_messages() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        // Build history with 10 messages for first compaction
        let history: Vec<StorageMessage> = (0..10)
            .map(|i| StorageMessage {
                id: Some(format!("msg_{}", i)),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: crate::providers::MessageContent::Text(format!("message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            })
            .collect();

        // Use json_output=true to bypass interactive approval in tests
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true, // json_output
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // First compaction
        let result1 = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "First summary",
                    "history": history,
                }),
            )
            .await;
        assert!(result1.is_ok());
        let text1 = result1.unwrap();
        assert!(text1.contains("Conversation condensed"));
        assert!(!text1.contains("re-compacted"));

        // After first compaction, get the deleted range end
        let state_guard = state.lock().await;
        let (_, _range_end) = state_guard.conversation_history_deleted_range.unwrap();
        drop(state_guard);

        // Build extended history with additional messages beyond the deleted range
        let mut extended_history: Vec<StorageMessage> = (0..10)
            .map(|i| StorageMessage {
                id: Some(format!("msg_{}", i)),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: crate::providers::MessageContent::Text(format!("message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            })
            .collect();
        // Add 5 more messages beyond range_end (enough to pass soft guard)
        for i in 10..15 {
            extended_history.push(StorageMessage {
                id: Some(format!("msg_{}", i)),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: crate::providers::MessageContent::Text(format!("new message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            });
        }

        // Second compaction with enough new messages should succeed
        let result2 = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "Updated summary",
                    "history": extended_history,
                }),
            )
            .await;
        assert!(result2.is_ok());
        let text2 = result2.unwrap();
        assert!(
            text2.contains("re-compacted"),
            "Expected re-compaction message, got: {}",
            text2
        );

        // Verify compacted_summary was overwritten
        let state_guard = state.lock().await;
        let summary = state_guard.compacted_summary.as_ref().unwrap();
        assert_eq!(summary.summary_text, "Updated summary");
    }

    #[tokio::test]
    async fn test_condense_preserves_recent_messages() {
        use crate::core::context::context_manager;
        use crate::providers::MessageContent;

        // Build a synthetic conversation history with 10 messages
        let history: Vec<StorageMessage> = (0..10)
            .map(|i| StorageMessage {
                id: Some(format!("msg_{}", i)),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: MessageContent::Text(format!("message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            })
            .collect();

        // Simulate compaction by setting deleted range
        let range = context_manager::get_next_truncation_range(
            &history,
            None,
            context_manager::TruncationKeep::LastTwo,
        );

        // Get truncated messages (should keep first 2 + last 2)
        let truncated = context_manager::get_truncated_messages(&history, Some(range), None);

        // Should have 4 messages: first 2 + last 2
        assert_eq!(truncated.len(), 4);
        // First message should be msg_0
        if let MessageContent::Text(ref text) = truncated[0].content {
            assert!(text.contains("message 0"));
        } else {
            panic!("Expected Text content");
        }
        // Last message should be msg_9
        if let MessageContent::Text(ref text) = truncated.last().unwrap().content {
            assert!(text.contains("message 9"));
        } else {
            panic!("Expected Text content");
        }
    }

    #[tokio::test]
    async fn test_condense_json_mode_auto_accepts() {
        use crate::core::tools::ToolContext;
        use std::sync::Arc;

        let handler = CondenseHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        // JSON mode = true, should auto-accept without stdin
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            true, // json_output = true
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = handler
            .execute(&ctx, serde_json::json!({"context": "Test summary"}))
            .await;

        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Conversation condensed"));
        assert!(text.contains("Test summary"));

        // Verify compaction happened
        let state_guard = state.lock().await;
        assert!(state_guard.compacted_summary.is_some());
    }
}
