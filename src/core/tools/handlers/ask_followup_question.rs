//! Ask followup question tool handler for sned CLI.
//!
//! Uses channel-based input (same pattern as approval prompts) to avoid
//! fighting the interactive input loop for stdin.

use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use std::future::Future;
use std::pin::Pin;

struct FollowupStateGuard<'a> {
    task_id: &'a str,
    armed: bool,
}

impl<'a> FollowupStateGuard<'a> {
    fn new(task_id: &'a str, sender: std::sync::mpsc::Sender<String>) -> Self {
        crate::core::approval::set_followup_question_active(task_id, true);
        crate::core::approval::set_followup_sender(task_id, sender);
        Self {
            task_id,
            armed: true,
        }
    }
}

impl<'a> Drop for FollowupStateGuard<'a> {
    fn drop(&mut self) {
        if self.armed {
            crate::core::approval::clear_followup_sender(self.task_id);
            crate::core::approval::set_followup_question_active(self.task_id, false);
        }
    }
}

/// Ask followup question tool handler.
#[derive(Debug, Clone, Default)]
pub struct AskFollowupQuestionHandler;

impl AskFollowupQuestionHandler {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let question = params
            .get("question")
            .and_then(|q| q.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput("Missing required parameter: question".to_string())
            })?;

        if !ctx.json_output {
            use crate::cli::output::OutputEvent;
            use ratatui::style::{Modifier, Style};
            let timeout_secs = crate::core::approval::followup_timeout().as_secs();
            use crate::cli::tui::theme::{ACCENT, WARNING_FG};
            let task_id = ctx.task_id.clone();

            // Arm the prompt state before emitting any lines so a drain that
            // lands mid-emit still pins the viewport to the blocking question.
            let (sender, receiver) = std::sync::mpsc::channel::<String>();
            let _followup_guard = FollowupStateGuard::new(&task_id, sender);

            ctx.output_writer.emit(OutputEvent::tool_output_line(
                format!("\n{} {}\n", "[Sned Question]", question),
                Style::default().fg(WARNING_FG).add_modifier(Modifier::BOLD),
            ));

            // Render the question text as markdown so the TUI displays
            // formatted content instead of raw `**bold**` / `| tables |` text.
            let rendered = crate::cli::markdown::render_markdown(None, question);
            for line in rendered {
                ctx.output_writer.emit(OutputEvent::ToolOutputLine(line));
            }

            ctx.output_writer.emit(OutputEvent::tool_output_line(
                "Your answer: ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
            ctx.output_writer.emit(OutputEvent::tool_output_line(
                format!("(waiting up to {timeout_secs}s for your response)"),
                Style::default().add_modifier(Modifier::DIM),
            ));
            ctx.output_writer.flush();

            // Use recv_timeout to avoid blocking the TUI event loop indefinitely.
            // Same pattern as /undo, /commit, /checkpoint-restore followup prompts.
            let response_result = tokio::task::spawn_blocking(move || {
                receiver.recv_timeout(crate::core::approval::followup_timeout())
            })
            .await;

            let Ok(Ok(response)) = response_result else {
                return Ok("User provided no response.".to_string());
            };

            let response = response.trim().to_string();

            if response.is_empty() {
                Ok("User provided no response.".to_string())
            } else {
                Ok(format!("User response: {response}"))
            }
        } else {
            Err(ToolError::ExecutionFailed(
                "Cannot read stdin in JSON mode".to_string(),
            ))
        }
    }
}

impl ToolHandler for AskFollowupQuestionHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            // Don't acquire state lock - ask_followup_question doesn't use state
            // and holding the lock across user input delays Ctrl+C cancellation
            Self::execute(&Self, &ctx, params)
                .await
                .map(serde_json::Value::String)
        })
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let question = params
            .get("question")
            .and_then(|q| q.as_str())
            .unwrap_or("?");
        format!(
            "[ask_followup_question for '{}']",
            &question[..question.len().min(50)]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use crate::core::approval::approval_test_guard;
    use crate::core::file_editor::AnchorStateManager;
    use crate::core::tools::ToolContext;
    use crate::test_support::env_lock;
    use std::sync::Arc;

    #[test]
    fn test_ask_handler_creation() {
        let handler = AskFollowupQuestionHandler::new();
        assert_eq!(format!("{:?}", handler), "AskFollowupQuestionHandler");
    }

    fn test_ctx_for_task(
        task_id: &str,
        json_output: bool,
    ) -> (ToolContext, Arc<tokio::sync::Mutex<TaskState>>) {
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            json_output,
            task_id.to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        (ctx, state)
    }

    #[tokio::test]
    async fn test_missing_question_does_not_set_followup_state() {
        let _lock = approval_test_guard();
        let task_id = "ask_followup_question_missing_question";
        let (ctx, _state) = test_ctx_for_task(task_id, false);

        let handler = AskFollowupQuestionHandler::new();
        let result = handler.execute(&ctx, serde_json::json!({})).await;
        assert!(matches!(
            result,
            Err(crate::core::tools::ToolError::InvalidInput(msg))
                if msg == "Missing required parameter: question"
        ));

        assert!(!crate::core::approval::is_followup_question_active(task_id));
        assert!(crate::core::approval::take_followup_sender(task_id).is_none());
    }

    #[tokio::test]
    async fn test_json_output_rejects_without_arming_followup_state() {
        let _lock = approval_test_guard();
        let task_id = "ask_followup_question_json_output";
        let (ctx, _state) = test_ctx_for_task(task_id, true);

        let handler = AskFollowupQuestionHandler::new();
        let result = handler
            .execute(&ctx, serde_json::json!({"question": "What is your plan?"}))
            .await;
        assert!(matches!(
            result,
            Err(crate::core::tools::ToolError::ExecutionFailed(msg))
                if msg == "Cannot read stdin in JSON mode"
        ));

        assert!(!crate::core::approval::is_followup_question_active(task_id));
        assert!(crate::core::approval::take_followup_sender(task_id).is_none());
    }

    #[tokio::test]
    async fn test_followup_cleanup_after_timeout() {
        let _lock = approval_test_guard();
        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let timeout_env = "SNED_FOLLOWUP_TIMEOUT_SECS";
        let original_timeout = std::env::var_os(timeout_env);
        unsafe {
            std::env::set_var(timeout_env, "1");
        }

        let task_id = "ask_followup_question_timeout_cleanup";
        let (ctx, _state) = test_ctx_for_task(task_id, false);
        let handler = AskFollowupQuestionHandler::new();
        let result = handler
            .execute(
                &ctx,
                serde_json::json!({"question": "Still waiting for answer?"}),
            )
            .await;

        unsafe {
            match original_timeout {
                Some(value) => std::env::set_var(timeout_env, value),
                None => std::env::remove_var(timeout_env),
            }
        }

        assert_eq!(result.unwrap(), "User provided no response.");
        assert!(!crate::core::approval::is_followup_question_active(task_id));
        assert!(crate::core::approval::take_followup_sender(task_id).is_none());
    }
}
