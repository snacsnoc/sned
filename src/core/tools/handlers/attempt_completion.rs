//! Attempt completion tool handler for sned CLI.
//!
//!
//! Core behavior:
//! - Validate result parameter
//! - Print completion message
//! - Return success result (agent loop will exit on this tool)

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;

/// Attempt completion tool handler.
#[derive(Debug, Clone, Default)]
pub struct AttemptCompletionHandler;

impl AttemptCompletionHandler {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let result = params
            .get("result")
            .and_then(|r| r.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput("Missing required parameter: result".to_string())
            })?;

        // Reset consecutive mistakes on completion
        state.consecutive_mistakes = 0;

        // Double-check completion: reject first attempt_completion call if not yet re-verified
        if state.double_check_completion_enabled && !state.double_check_completion_pending {
            state.double_check_completion_pending = true;
            let rejection_message = "Before completing, re-verify your work against the original task requirements. Check that:\n\
                1. All requested changes have been made (verify using a script/execute_command when possible)\n\
                2. No steps were skipped or partially completed\n\
                3. Edge cases and error handling are addressed\n\
                4. The solution matches what was asked for, not just what was convenient\n\
                5. Output files contain exactly what was specified - no extra columns, fields, debug output, or commentary\n\
                6. If the task specifies numerical thresholds or accuracy targets, verify your result meets the criteria. If close but not passing, iterate rather than declaring completion\n\n\
                If everything checks out, call attempt_completion again with your final result.";
            return Err(ToolError::ExecutionFailed(rejection_message.to_string()));
        }

        // Reset pending flag so the next attempt_completion pair triggers double-check again
        state.double_check_completion_pending = false;

        // Print completion message (conditional based on json_output - caller must pass context)
        // This is handled in the ToolHandler::execute wrapper below

        // Return result text (agent loop detects attempt_completion and exits)
        Ok(result.to_string())
    }
}

#[async_trait]
impl ToolHandler for AttemptCompletionHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let mut state = ctx.state.lock().await;
        let result = Self::execute(self, &mut state, params).await?;

        if ctx.json_output {
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "completion",
                    "result": result
                })
            );
        } else {
            use crate::cli::output::OutputEvent;
            let completion_text = result.clone();
            ctx.output_writer.emit(OutputEvent::Completion(completion_text));
        }

        Ok(serde_json::Value::String(result))
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[attempt_completion]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attempt_completion_creation() {
        let handler = AttemptCompletionHandler::new();
        assert_eq!(format!("{:?}", handler), "AttemptCompletionHandler");
    }

    #[tokio::test]
    async fn test_attempt_completion_missing_result() {
        let handler = AttemptCompletionHandler::new();
        let mut state = TaskState::default();
        let result = handler.execute(&mut state, serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("result"));
    }

    #[tokio::test]
    async fn test_attempt_completion_success() {
        let handler = AttemptCompletionHandler::new();
        let mut state = TaskState {
            double_check_completion_pending: true,
            ..Default::default()
        };
        // Simulate second attempt (after double-check rejection)
        let result = handler
            .execute(&mut state, serde_json::json!({"result": "Done!"}))
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Done!");
    }

    #[tokio::test]
    async fn test_double_check_first_attempt_rejected() {
        let handler = AttemptCompletionHandler::new();
        let mut state = TaskState::default();
        // double_check_completion_enabled defaults to true
        assert!(state.double_check_completion_enabled);
        assert!(!state.double_check_completion_pending);

        let result = handler
            .execute(&mut state, serde_json::json!({"result": "Done!"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("re-verify your work"),
            "Expected rejection message, got: {}",
            err
        );
        assert!(
            state.double_check_completion_pending,
            "Pending flag should be set after first attempt"
        );
    }

    #[tokio::test]
    async fn test_double_check_second_attempt_succeeds() {
        let handler = AttemptCompletionHandler::new();
        let mut state = TaskState {
            double_check_completion_pending: true,
            ..Default::default()
        };

        let result = handler
            .execute(&mut state, serde_json::json!({"result": "Done!"}))
            .await;
        assert!(result.is_ok(), "Second attempt should succeed");
        assert_eq!(result.unwrap(), "Done!");
        assert!(
            !state.double_check_completion_pending,
            "Pending flag should be reset after successful completion"
        );
    }

    #[tokio::test]
    async fn test_double_check_disabled_first_attempt_succeeds() {
        let handler = AttemptCompletionHandler::new();
        let mut state = TaskState {
            double_check_completion_enabled: false,
            ..Default::default()
        };

        let result = handler
            .execute(&mut state, serde_json::json!({"result": "Done!"}))
            .await;
        assert!(
            result.is_ok(),
            "First attempt should succeed when double-check is disabled"
        );
        assert_eq!(result.unwrap(), "Done!");
        assert!(
            !state.double_check_completion_pending,
            "Pending flag should remain false"
        );
    }

    #[tokio::test]
    async fn test_double_check_resets_after_success() {
        let handler = AttemptCompletionHandler::new();
        let mut state = TaskState::default();

        // First attempt: rejected
        let result = handler
            .execute(&mut state, serde_json::json!({"result": "Done!"}))
            .await;
        assert!(result.is_err());
        assert!(state.double_check_completion_pending);

        // Second attempt: succeeds
        let result = handler
            .execute(&mut state, serde_json::json!({"result": "Done!"}))
            .await;
        assert!(result.is_ok());
        assert!(!state.double_check_completion_pending);

        // Third attempt (new pair): rejected again
        let result = handler
            .execute(&mut state, serde_json::json!({"result": "Done!"}))
            .await;
        assert!(
            result.is_err(),
            "Third attempt should be rejected again (new double-check cycle)"
        );
        assert!(state.double_check_completion_pending);
    }
}
