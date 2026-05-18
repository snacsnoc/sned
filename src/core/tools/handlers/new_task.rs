use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
pub struct NewTaskHandler;

impl NewTaskHandler {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let context = params.get("context").and_then(|c| c.as_str()).unwrap_or("");

        if context.is_empty() {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "new_task: empty context provided"
            );
            return Err(ToolError::InvalidInput(
                "Missing required parameter: context".to_string(),
            ));
        }

        state.consecutive_mistakes = 0;

        Ok(format!(
            "User requested to create a new task with context: {}\n\nTo create a new task, please restart Sned with the --continue flag and provide this context summary.",
            context
        ))
    }
}

impl Default for NewTaskHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolHandler for NewTaskHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let mut state = ctx.state.lock().await;
        Self::execute(self, &mut state, params)
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[new_task]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_task_handler_empty_context() {
        let handler = NewTaskHandler::new();
        let mut state = TaskState::default();

        let result = handler.execute(&mut state, serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(state.consecutive_mistakes > 0);
    }

    #[tokio::test]
    async fn test_new_task_handler_with_context() {
        let handler = NewTaskHandler::new();
        let mut state = TaskState::default();

        let result = handler
            .execute(
                &mut state,
                serde_json::json!({
                    "context": "Test context summary"
                }),
            )
            .await;

        assert!(result.is_ok());
        assert!(state.consecutive_mistakes == 0);
        assert!(result.unwrap().contains("Test context summary"));
    }
}
