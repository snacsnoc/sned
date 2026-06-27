use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
use std::future::Future;
use std::pin::Pin;
use crate::services::tree_sitter::get_functions;
use crate::services::tree_sitter::load_required_language_parsers;

/// Handler for get_function tool.
pub struct GetFunctionHandler;

impl GetFunctionHandler {
    pub async fn run(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let path = params.get("path").and_then(|p| p.as_str()).unwrap_or("");

        // Schema declares "name" as string, but support "names" array for backwards compatibility
        let names = if let Some(name) = params.get("name").and_then(|n| n.as_str()) {
            vec![name.to_string()]
        } else if let Some(names_arr) = params.get("names").and_then(|n| n.as_array()) {
            names_arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        if path.is_empty() {
            return Err(ToolError::InvalidInput(
                "Missing required parameter: path".to_string(),
            ));
        }

        if names.is_empty() {
            return Err(ToolError::InvalidInput(
                "Missing required parameter: name".to_string(),
            ));
        }

        let anchor_mgr = ctx.anchor_mgr.clone();
        let abs_path = resolve_sanitized_path(&ctx.workspace_root, path)?;
        let abs_path_str = abs_path.to_string_lossy();
        let language_parsers =
            load_required_language_parsers(&[abs_path_str.as_ref()]).map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to load language parsers: {e}"))
            })?;

        match tokio::fs::read_to_string(&abs_path).await {
            Ok(content) => {
                match get_functions(
                    &anchor_mgr,
                    abs_path_str.as_ref(),
                    path,
                    &names,
                    &content,
                    &language_parsers,
                    None,
                ) {
                    Ok(Some(result)) => Ok(result.formatted_content),
                    Ok(None) => Ok(format!("No functions found in {path}")),
                    Err(e) => Err(ToolError::ExecutionFailed(format!(
                        "Error getting functions: {e}"
                    ))),
                }
            }
            Err(e) => Err(ToolError::ExecutionFailed(format!(
                "Error reading file {path}: {e}"
            ))),
        }
    }
}

impl ToolHandler for GetFunctionHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self;
        let ctx = ctx.clone();
        Box::pin(async move {
            Self::run(handler, &ctx, params)
                .await
                .map(serde_json::Value::String)
        })
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[get_function]".to_string()
    }
}
