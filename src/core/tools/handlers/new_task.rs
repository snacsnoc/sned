use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use std::future::Future;
use std::pin::Pin;
pub struct NewTaskHandler;

impl NewTaskHandler {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub fn execute(
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
            "User requested to create a new task with context: {context}\n\nTo create a new task, please restart Sned with the --continue flag and provide this context summary."
        ))
    }
}

impl Default for NewTaskHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolHandler for NewTaskHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self;
        let ctx = ctx.clone();
        Box::pin(async move {
            let mut state = ctx.state.lock().await;
            Self::execute(handler, &mut state, params).map(serde_json::Value::String)
        })
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

        let result = handler.execute(&mut state, serde_json::json!({}));
        assert!(result.is_err());
        assert!(state.consecutive_mistakes > 0);
    }

    #[tokio::test]
    async fn test_new_task_handler_with_context() {
        let handler = NewTaskHandler::new();
        let mut state = TaskState::default();

        let result = handler.execute(
            &mut state,
            serde_json::json!({
                "context": "Test context summary"
            }),
        );

        assert!(result.is_ok());
        assert!(state.consecutive_mistakes == 0);
        assert!(result.unwrap().contains("Test context summary"));
    }
}
