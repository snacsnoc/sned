use crate::core::tools::handlers::read_file::record_complete_file_read;
use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
use crate::services::tree_sitter::{
    MAX_STRUCTURAL_FILE_READ_SIZE, get_functions, load_required_language_parsers,
};
use std::future::Future;
use std::pin::Pin;

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
        let canonical_path = tokio::fs::canonicalize(&abs_path).await.map_err(|error| {
            ToolError::ExecutionFailed(format!("Error reading file {path}: {error}"))
        })?;
        let metadata = tokio::fs::metadata(&canonical_path)
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!("Error reading file {path}: {error}"))
            })?;
        if metadata.len() > MAX_STRUCTURAL_FILE_READ_SIZE {
            return Err(ToolError::ExecutionFailed(format!(
                "File too large for structural analysis ({}KB > {}KB). Use search_files, or ask the user to restart Sned with a higher SNED_MAX_FILE_READ_SIZE for a full read.",
                metadata.len().div_ceil(1024),
                MAX_STRUCTURAL_FILE_READ_SIZE / 1024,
            )));
        }

        let abs_path_str = canonical_path.to_string_lossy().into_owned();
        let language_parsers =
            load_required_language_parsers(&[abs_path_str.as_str()]).map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to load language parsers: {e}"))
            })?;

        match tokio::fs::read_to_string(&canonical_path).await {
            Ok(content) => {
                match get_functions(
                    &anchor_mgr,
                    &abs_path_str,
                    path,
                    &names,
                    &content,
                    &language_parsers,
                    Some(ctx.task_id.as_str()),
                ) {
                    Ok(Some(result)) => {
                        if !result.found_names.is_empty() {
                            let mut state = ctx.state.lock().await;
                            record_complete_file_read(&mut state, &canonical_path);
                            state
                                .consecutive_reads
                                .remove(&canonical_path.to_string_lossy().into_owned());
                        }
                        Ok(result.formatted_content)
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use crate::core::file_editor::AnchorStateManager;
    use crate::core::tools::handlers::edit_file::EditFileHandler;
    use std::sync::Arc;

    fn test_context(
        workspace_root: &std::path::Path,
        state: Arc<tokio::sync::Mutex<TaskState>>,
    ) -> ToolContext {
        ToolContext::new(
            state,
            None,
            workspace_root.to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        )
    }

    #[tokio::test]
    async fn test_get_function_clears_reread_requirement_after_returning_anchors() {
        let workspace = tempfile::tempdir().unwrap();
        let file_path = workspace.path().join("example.rs");
        std::fs::write(&file_path, "fn target() {\n    println!(\"ok\");\n}\n").unwrap();
        let canonical_path = std::fs::canonicalize(&file_path).unwrap();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        state
            .lock()
            .await
            .must_reread_before_edit
            .insert(canonical_path.to_string_lossy().into_owned());
        let ctx = test_context(workspace.path(), state.clone());

        let output = GetFunctionHandler
            .run(
                &ctx,
                serde_json::json!({"path": "example.rs", "name": "target"}),
            )
            .await
            .unwrap();

        assert!(output.contains("target"));
        let anchor = output
            .lines()
            .find(|line| line.contains("fn target()"))
            .expect("function output should contain an anchored definition")
            .to_string();
        assert!(
            !state
                .lock()
                .await
                .must_reread_before_edit
                .contains(&canonical_path.to_string_lossy().into_owned())
        );
        ToolHandler::execute(
            &EditFileHandler::new(),
            &ctx,
            serde_json::json!({
                "files": [{
                    "path": "example.rs",
                    "edits": [{
                        "anchor": anchor,
                        "edit_type": "replace",
                        "text": "fn target() { println!(\"updated\"); }"
                    }]
                }]
            }),
        )
        .await
        .expect("edit_file should accept an anchor returned by get_function");
        assert!(
            std::fs::read_to_string(&file_path)
                .unwrap()
                .contains("updated")
        );
    }

    #[tokio::test]
    async fn test_get_function_keeps_reread_requirement_when_structural_cap_rejects_file() {
        let workspace = tempfile::tempdir().unwrap();
        let file_path = workspace.path().join("large.rs");
        std::fs::write(
            &file_path,
            vec![b'x'; MAX_STRUCTURAL_FILE_READ_SIZE as usize + 1],
        )
        .unwrap();
        let canonical_path = std::fs::canonicalize(&file_path).unwrap();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        state
            .lock()
            .await
            .must_reread_before_edit
            .insert(canonical_path.to_string_lossy().into_owned());
        let ctx = test_context(workspace.path(), state.clone());

        let error = GetFunctionHandler
            .run(
                &ctx,
                serde_json::json!({"path": "large.rs", "name": "target"}),
            )
            .await
            .expect_err("oversized file must not be read");

        assert!(
            error
                .to_string()
                .contains("File too large for structural analysis")
        );
        assert!(
            state
                .lock()
                .await
                .must_reread_before_edit
                .contains(&canonical_path.to_string_lossy().into_owned())
        );
    }
}
