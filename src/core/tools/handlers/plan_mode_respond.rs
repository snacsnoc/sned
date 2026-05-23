//! Plan mode respond tool handler for sned CLI.
//!
//!
//! Core behavior:
//! - Validate response parameter
//! - Print plan response to user
//! - Return result indicating plan was received

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;

/// Plan mode respond tool handler.
#[derive(Debug, Clone, Default)]
pub struct PlanModeRespondHandler;

impl PlanModeRespondHandler {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        json_output: bool,
    ) -> Result<String, ToolError> {
        let response = params
            .get("response")
            .and_then(|r| r.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput("Missing required parameter: response".to_string())
            })?;

        // Check for needs_more_exploration escape hatch
        let needs_more = params
            .get("needs_more_exploration")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Reset consecutive mistakes
        state.consecutive_mistakes = 0;

        if needs_more {
            return Ok(
                "[You have indicated that you need more exploration. Proceed with calling tools to continue the planning process.]"
                    .to_string(),
            );
        }

        // Print plan response to user
        if json_output {
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "plan_response",
                    "response": response
                })
            );
        } else {
            eprintln!(
                "\n{} {}\n{}\n",
                crate::cli::colors::colorize_stderr("📋", crate::cli::colors::style::CYAN),
                crate::cli::colors::colorize_stderr("Plan", crate::cli::colors::style::BOLD),
                response
            );
        }

        Ok(format!("<user_message>\n{}\n</user_message>", response))
    }
}

#[async_trait]
impl ToolHandler for PlanModeRespondHandler {
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

    fn description(&self, _params: &serde_json::Value) -> String {
        "[plan_mode_respond]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_mode_respond_creation() {
        let handler = PlanModeRespondHandler::new();
        assert_eq!(format!("{:?}", handler), "PlanModeRespondHandler");
    }

    #[tokio::test]
    async fn test_plan_mode_respond_missing_response() {
        let handler = PlanModeRespondHandler::new();
        let mut state = TaskState::default();
        let result = handler
            .execute(&mut state, serde_json::json!({}), false)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("response"));
    }

    #[tokio::test]
    async fn test_plan_mode_respond_success() {
        let handler = PlanModeRespondHandler::new();
        let mut state = TaskState::default();
        let result = handler
            .execute(
                &mut state,
                serde_json::json!({"response": "Step 1: do this"}),
                false,
            )
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Step 1: do this"));
    }

    #[tokio::test]
    async fn test_plan_mode_respond_needs_more() {
        let handler = PlanModeRespondHandler::new();
        let mut state = TaskState::default();
        let result = handler
            .execute(
                &mut state,
                serde_json::json!({"response": "I need to explore more", "needs_more_exploration": true}),
                false,
            )
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("need more exploration"));
    }
}
