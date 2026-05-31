//! Plan mode respond tool handler for sned CLI.
//!
//! Core behavior:
//! - Validate response parameter
//! - Print plan response to user
//! - Return result indicating plan was received
//! - Create PlanState when needs_more_exploration is false
//! - Skip PlanState creation when needs_more_exploration is true

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
        ctx: &ToolContext,
        params: serde_json::Value,
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
        let mut state = ctx.state.lock().await;
        state.consecutive_mistakes = 0;
        drop(state);

        if needs_more {
            return Ok(
                "[You have indicated that you need more exploration. Proceed with calling tools to continue the planning process.]"
                    .to_string(),
            );
        }

        // Parse plan text into step descriptions
        let steps = crate::core::plan_state::PlanState::parse_plan(response);
        let steps = steps.ok_or_else(|| {
            ToolError::InvalidInput(
                "Could not parse plan into numbered steps. Use format: 1. Step description".to_string(),
            )
        })?;

        if steps.len() < 2 {
            return Err(ToolError::InvalidInput(
                "Plan must have at least 2 steps".to_string(),
            ));
        }

        // Create PlanState with the parsed steps
        let plan = crate::core::plan_state::PlanState::create_plan(steps);

        // Store in TaskState
        {
            let mut state = ctx.state.lock().await;
            state.plan_state = Some(plan);
        }

        // Print plan response to user
        if ctx.json_output {
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "plan_response",
                    "response": response
                })
            );
        } else {
            use crate::cli::output::OutputEvent;
            use ratatui::style::{Color, Modifier, Style};
            ctx.output_writer.emit(OutputEvent::styled(
                format!("\n{} {}\n{}\n", "Plan", "Generated Plan", response),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
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
        Self::execute(self, ctx, params)
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
    use crate::core::tools::ToolContext;
    use std::sync::Arc;

    #[test]
    fn test_plan_mode_respond_creation() {
        let handler = PlanModeRespondHandler::new();
        assert_eq!(format!("{:?}", handler), "PlanModeRespondHandler");
    }

    #[tokio::test]
    async fn test_plan_mode_respond_missing_response() {
        let handler = PlanModeRespondHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(
            crate::core::agent_loop::TaskState::default(),
        ));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = handler.execute(&ctx, serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("response"));
    }

    #[tokio::test]
    async fn test_plan_mode_respond_success() {
        let handler = PlanModeRespondHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(
            crate::core::agent_loop::TaskState::default(),
        ));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = handler
            .execute(&ctx, serde_json::json!({"response": "1. do this\n2. do that"}))
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("do this"));
    }

    #[tokio::test]
    async fn test_plan_mode_respond_needs_more() {
        let handler = PlanModeRespondHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(
            crate::core::agent_loop::TaskState::default(),
        ));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = handler
            .execute(
                &ctx,
                serde_json::json!({"response": "I need to explore more", "needs_more_exploration": true}),
            )
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("need more exploration"));
    }

    #[tokio::test]
    async fn test_plan_mode_respond_rejects_single_step() {
        let handler = PlanModeRespondHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(
            crate::core::agent_loop::TaskState::default(),
        ));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            crate::core::file_editor::AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = handler
            .execute(&ctx, serde_json::json!({"response": "1. only one step"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least 2 steps"));
    }
}
