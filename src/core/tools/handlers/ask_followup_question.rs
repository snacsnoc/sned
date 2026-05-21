//! Ask followup question tool handler for sned CLI.
//!
//! Uses channel-based input (same pattern as approval prompts) to avoid
//! fighting the interactive input loop for stdin.

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;
use std::io::{self, Write};

/// Ask followup question tool handler.
#[derive(Debug, Clone, Default)]
pub struct AskFollowupQuestionHandler;

impl AskFollowupQuestionHandler {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(
        &self,
        _state: &mut TaskState,
        params: serde_json::Value,
        json_output: bool,
    ) -> Result<String, ToolError> {
        let question = params
            .get("question")
            .and_then(|q| q.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput("Missing required parameter: question".to_string())
            })?;

        if !json_output {
            eprintln!(
                "\n{} {}\n",
                crate::cli::colors::colorize("[Sned Question]", crate::cli::colors::style::YELLOW),
                crate::cli::colors::colorize(question, crate::cli::colors::style::BOLD)
            );
            eprint!(
                "{}",
                crate::cli::colors::colorize("Your answer: ", crate::cli::colors::style::CYAN)
            );
            io::stderr().flush().map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to flush stderr: {}", e))
            })?;

            // Use channel-based input to avoid fighting the interactive loop
            let (sender, receiver) = std::sync::mpsc::channel();
            crate::core::approval::set_followup_question_active(true);
            crate::core::approval::set_followup_sender(sender);

            // Wrap blocking recv() in spawn_blocking to avoid blocking tokio worker thread
            let response_result = tokio::task::spawn_blocking(move || receiver.recv())
                .await;
            
            // Clean up regardless of result
            crate::core::approval::clear_followup_sender();
            crate::core::approval::set_followup_question_active(false);

            let response = match response_result {
                Ok(Ok(r)) => r,
                Ok(Err(_)) | Err(_) => {
                    return Ok("User provided no response.".to_string());
                }
            };

            crate::core::approval::clear_followup_sender();
            crate::core::approval::set_followup_question_active(false);

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
        let mut state = ctx.state.lock().await;
        Self::execute(self, &mut state, params, ctx.json_output)
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
