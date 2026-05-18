//! Summarize task tool handler for sned CLI.
//!
//!
//! Triggers context compaction on demand by providing a summary.

use crate::core::context::context_manager::{self, TruncationKeep};
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use crate::providers::StorageMessage;
use async_trait::async_trait;

/// Summarize task tool handler.
#[derive(Debug, Clone, Default)]
pub struct SummarizeTaskHandler;

impl SummarizeTaskHandler {
    pub fn new() -> Self {
        Self
    }

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

        // Parse optional required_files parameter.
        let required_files: Vec<String> = params
            .get("required_files")
            .and_then(|f| f.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let mut state = ctx.state.lock().await;
        state.consecutive_mistakes = 0;

        // Execute PreCompact hook before truncation if hooks are available.
        let mut hook_context_modification: Option<String> = None;
        if let Some(ref hook_mgr) = ctx.hook_manager {
            let result = hook_mgr.pre_compact(&ctx.task_id, "<conversation_history>");

            // Check for hook cancellation (hook requested to abort the operation)
            if let Some(output) = &result.output
                && output.cancel == Some(true)
            {
                tracing::warn!(
                    "[PreCompact] Hook requested cancellation for task {}, aborting summarization",
                    ctx.task_id
                );
                return Err(ToolError::ExecutionFailed(
                    "Context compaction was cancelled by PreCompact hook. Task has been aborted."
                        .to_string(),
                ));
            }

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

        // Parse "Required Files" section from context if required_files not provided.
        let mut file_paths = required_files;
        if file_paths.is_empty() {
            let file_path_regex =
                regex::Regex::new(r"9\.\s*(?:Optional\s+)?Required Files:\s*((?:\n\s*-\s*.+)+)")
                    .unwrap();
            let line_regex = regex::Regex::new(r"^\s*-\s*(.+)$").unwrap();
            if let Some(caps) = file_path_regex.captures(context)
                && let Some(file_list_text) = caps.get(1)
            {
                for line in file_list_text.as_str().lines() {
                    if let Some(path_match) = line_regex.captures(line)
                        && let Some(path) = path_match.get(1)
                    {
                        file_paths.push(path.as_str().trim().to_string());
                    }
                }
            }
        }

        // Read required files with limits.
        let mut file_contents = String::new();
        let mut loaded_file_paths = Vec::new();
        let mut total_chars = 0;
        const MAX_FILES_LOADED: usize = 8;
        const MAX_FILES_PROCESSED: usize = 10;
        const MAX_CHARS: usize = 100_000;

        let mut loaded_files = std::collections::HashSet::new();
        let mut files_loaded = 0;

        for rel_path in file_paths {
            let normalized_path = rel_path.to_lowercase();
            if loaded_files.contains(&normalized_path) {
                continue;
            }
            loaded_files.insert(normalized_path);

            if loaded_files.len() > MAX_FILES_PROCESSED {
                break;
            }

            let absolute_path = ctx.workspace_root.join(&rel_path);
            if !absolute_path.exists() {
                continue;
            }

            match tokio::fs::read_to_string(&absolute_path).await {
                Ok(content) => {
                    if total_chars + content.len() > MAX_CHARS {
                        break;
                    }

                    file_contents.push_str(&format!(
                        "\n\n<file_content path=\"{}\">\n{}\n</file_content>",
                        rel_path, content
                    ));
                    loaded_file_paths.push(rel_path.clone());
                    total_chars += content.len();
                    files_loaded += 1;

                    if files_loaded >= MAX_FILES_LOADED {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read {} during summarization: {}", rel_path, e);
                }
            }
        }

        // Trigger context truncation via ContextManager when history is provided.
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
                            "summarize_task: dropped history entry that failed deserialization: {} | value: {}",
                            e, truncated
                        );
                        None
                    }
                })
                .collect();

            if !history.is_empty() {
                let range = context_manager::get_next_truncation_range(
                    &history,
                    state.conversation_history_deleted_range,
                    TruncationKeep::LastTwo,
                );

                state.conversation_history_deleted_range = Some(range);
            }
        }

        drop(state);

        // Build the result with file contents and hook modification.
        let mut result = format!(
            "Task summarized. The following summary will be used to continue the conversation:\n\n{}",
            context
        );

        if !loaded_file_paths.is_empty() {
            let file_mention_string = loaded_file_paths
                .iter()
                .map(|p| format!("'{}'", p))
                .collect::<Vec<_>>()
                .join(", ");
            result.push_str(&format!(
                "\n\nThe following files were automatically read based on the files listed in the Required Files section: {} (see below for file content). These are the latest versions of these files - you should reference them directly and not re-read them:{}",
                file_mention_string, file_contents
            ));
        }

        if let Some(modification) = hook_context_modification {
            result.push_str(&format!(
                "\n\n[Context Modification from PreCompact Hook]\n{}",
                modification
            ));
        }

        Ok(result)
    }
}

#[async_trait]
impl ToolHandler for SummarizeTaskHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        Self::execute(self, ctx, params)
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[summarize_task]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use std::sync::Arc;

    fn create_test_ctx() -> ToolContext {
        ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
        )
    }

    #[test]
    fn test_summarize_task_handler_creation() {
        let handler = SummarizeTaskHandler::new();
        assert_eq!(format!("{:?}", handler), "SummarizeTaskHandler");
    }

    #[tokio::test]
    async fn test_summarize_task_missing_context() {
        let handler = SummarizeTaskHandler::new();
        let ctx = create_test_ctx();
        let result = handler.execute(&ctx, serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("context"));
    }

    #[tokio::test]
    async fn test_summarize_task_success_without_history() {
        let handler = SummarizeTaskHandler::new();
        let ctx = create_test_ctx();
        let result = handler
            .execute(&ctx, serde_json::json!({"context": "Summary of work done"}))
            .await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Summary of work done"));
        assert!(text.contains("Task summarized"));
    }

    #[tokio::test]
    async fn test_summarize_task_triggers_context_manager() {
        let handler = SummarizeTaskHandler::new();
        let ctx = create_test_ctx();

        let history: Vec<StorageMessage> = (0..10)
            .map(|i| StorageMessage {
                id: Some(format!("msg_{}", i)),
                role: if i % 2 == 0 {
                    crate::providers::MessageRole::User
                } else {
                    crate::providers::MessageRole::Assistant
                },
                content: crate::providers::MessageContent::Text(format!("message {}", i)),
                model_info: None,
                metrics: None,
                ts: None,
            })
            .collect();

        let result = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "Summary of work done",
                    "history": history,
                }),
            )
            .await;

        assert!(result.is_ok());
        let state = ctx.state.lock().await;
        assert!(state.conversation_history_deleted_range.is_some());

        let (start, end) = state.conversation_history_deleted_range.unwrap();
        assert_eq!(start, 2);
        assert_eq!(end, 7);
    }

    #[tokio::test]
    async fn test_summarize_task_with_required_files() {
        let handler = SummarizeTaskHandler::new();
        let ctx = create_test_ctx();
        let result = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "Summary of work done",
                    "required_files": ["src/main.rs", "Cargo.toml"],
                }),
            )
            .await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Summary of work done"));
    }

    #[tokio::test]
    async fn test_summarize_task_corrupt_history_entry_is_skipped() {
        let handler = SummarizeTaskHandler::new();
        let ctx = create_test_ctx();

        let valid_msg = serde_json::json!({
            "id": "msg_0",
            "role": "user",
            "content": {"type": "text", "text": "hello"},
        });

        let corrupt_entry = serde_json::json!({"bogus": 42, "not_a_message": true});
        let valid_msg2 = serde_json::json!({
            "id": "msg_1",
            "role": "assistant",
            "content": {"type": "text", "text": "world"},
        });

        let result = handler
            .execute(
                &ctx,
                serde_json::json!({
                    "context": "Summary",
                    "history": [valid_msg, corrupt_entry, valid_msg2],
                }),
            )
            .await;

        assert!(result.is_ok());
        let state = ctx.state.lock().await;
        assert!(
            state.conversation_history_deleted_range.is_none(),
            "history with <3 valid messages should not trigger truncation"
        );
    }

    #[tokio::test]
    async fn test_summarize_task_hook_cancellation_path_exists() {
        use crate::core::hooks::HookManager as ConcreteHookManager;
        use std::sync::Arc;

        let handler = SummarizeTaskHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        // Create a hook manager (no hooks discovered = no cancellation)
        let hook_mgr = ConcreteHookManager::new("test-user");

        let ctx = crate::core::tools::ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            Some(Arc::new(hook_mgr)),
            false,
        );

        // Without an actual hook script, the hook won't run
        // This test verifies the handler doesn't crash when hook manager is present
        let result = handler
            .execute(&ctx, serde_json::json!({"context": "Summary"}))
            .await;

        // Should succeed (no hook script = no cancellation)
        assert!(result.is_ok());
    }
}
