//! Ask followup question tool handler for sned CLI.
//!
//! Uses channel-based input (same pattern as approval prompts) to avoid
//! fighting the interactive input loop for stdin.

use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;

/// Ask followup question tool handler.
#[derive(Debug, Clone, Default)]
pub struct AskFollowupQuestionHandler;

impl AskFollowupQuestionHandler {
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
            ctx.output_writer.emit(OutputEvent::tool_output_line(
                format!("\n{} {}\n", "[Sned Question]", question),
                Style::default()
                    .fg(WARNING_FG)
                    .add_modifier(Modifier::BOLD),
            ));
            ctx.output_writer.emit(OutputEvent::tool_output_line(
                "Your answer: ",
                Style::default().fg(ACCENT),
            ));
            ctx.output_writer.emit(OutputEvent::tool_output_line(
                format!("(waiting up to {}s for your response)", timeout_secs),
                Style::default().add_modifier(Modifier::DIM),
            ));
            ctx.output_writer.flush();

            // Capture task_id for use after spawn_blocking
            let task_id = ctx.task_id.clone();

            // Use channel-based input to avoid fighting the interactive loop
            let (sender, receiver) = std::sync::mpsc::channel();
            crate::core::approval::set_followup_question_active(&task_id, true);
            crate::core::approval::set_followup_sender(&task_id, sender);

            // Use recv_timeout to avoid blocking the TUI event loop indefinitely.
            // Same pattern as /undo, /commit, /checkpoint-restore followup prompts.
            let response_result = tokio::task::spawn_blocking(move || {
                receiver.recv_timeout(crate::core::approval::followup_timeout())
            })
            .await;

            // Clean up followup state regardless of outcome
            crate::core::approval::clear_followup_sender(&task_id);
            crate::core::approval::set_followup_question_active(&task_id, false);

            let response = match response_result {
                Ok(Ok(r)) => r,
                Ok(Err(_)) | Err(_) => {
                    return Ok("User provided no response.".to_string());
                }
            };

            let response = response.trim().to_string();

            if response.is_empty() {
                Ok("User provided no response.".to_string())
            } else {
                Ok(format!("User response: {}", response))
            }
        } else {
            Err(ToolError::ExecutionFailed(
                "Cannot read stdin in JSON mode".to_string(),
            ))
        }
    }
}

#[async_trait]
impl ToolHandler for AskFollowupQuestionHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        // Don't acquire state lock - ask_followup_question doesn't use state
        // and holding the lock across user input delays Ctrl+C cancellation
        Self::execute(self, ctx, params)
            .await
            .map(serde_json::Value::String)
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

    #[test]
    fn test_ask_handler_creation() {
        let handler = AskFollowupQuestionHandler::new();
        assert_eq!(format!("{:?}", handler), "AskFollowupQuestionHandler");
    }
}
