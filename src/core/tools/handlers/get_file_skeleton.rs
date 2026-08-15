use crate::core::tools::handlers::read_file::record_complete_file_read;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use crate::services::tree_sitter::{
    MAX_STRUCTURAL_FILE_READ_SIZE, get_file_skeleton, load_required_language_parsers,
};
use futures::future::join_all;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Handler for get_file_skeleton tool.
pub struct GetFileSkeletonHandler;

impl GetFileSkeletonHandler {
    pub async fn run(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let paths = crate::core::tools::coerce_string_array(&params, "paths", "path");

        if paths.is_empty() {
            return Err(ToolError::InvalidInput(
                "Missing required parameter: paths".to_string(),
            ));
        }

        let abs_paths: Result<Vec<_>, _> = paths
            .iter()
            .map(|rel_path| ctx.resolve_path(rel_path))
            .collect();
        let abs_paths = abs_paths?;
        let mut results = Vec::with_capacity(paths.len());
        let mut readable_paths = Vec::with_capacity(paths.len());
        for (index, (rel_path, abs_path)) in paths.iter().zip(abs_paths).enumerate() {
            let canonical_path = match tokio::fs::canonicalize(&abs_path).await {
                Ok(path) => path,
                Err(error) => {
                    results.push((index, format!("Error reading file {rel_path}: {error}")));
                    continue;
                }
            };
            match tokio::fs::metadata(&canonical_path).await {
                Ok(metadata) if metadata.len() > MAX_STRUCTURAL_FILE_READ_SIZE => results.push((
                    index,
                    format!(
                        "File too large for structural analysis ({}KB > {}KB). Use search_files, or ask the user to restart Sned with a higher SNED_MAX_FILE_READ_SIZE for a full read.",
                        metadata.len().div_ceil(1024),
                        MAX_STRUCTURAL_FILE_READ_SIZE / 1024,
                    ),
                )),
                Ok(_) => readable_paths.push((index, rel_path.clone(), canonical_path)),
                Err(error) => {
                    results.push((index, format!("Error reading file {rel_path}: {error}")));
                }
            }
        }

        if readable_paths.is_empty() {
            results.sort_by_key(|(index, _)| *index);
            return Ok(results
                .into_iter()
                .map(|(_, result)| result)
                .collect::<Vec<_>>()
                .join("\n\n"));
        }
        let language_parsers = Arc::new(
            load_required_language_parsers(
                &readable_paths
                    .iter()
                    .map(|(_, _, path)| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            )
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to load language parsers: {e}"))
            })?,
        );

        let anchor_mgr = ctx.anchor_mgr.clone();
        let state = ctx.state.clone();
        let task_id = ctx.task_id.clone();
        let futures = readable_paths
            .into_iter()
            .map(|(index, rel_path, abs_path)| {
                let anchor_mgr = anchor_mgr.clone();
                let language_parsers = Arc::clone(&language_parsers);
                let state = state.clone();
                let task_id = task_id.clone();
                async move {
                    let abs_path_str = abs_path.to_string_lossy().into_owned();
                    match tokio::fs::read_to_string(&abs_path).await {
                        Ok(content) => match get_file_skeleton(
                            &anchor_mgr,
                            abs_path_str.as_str(),
                            &content,
                            language_parsers.as_ref(),
                            Some(task_id.as_str()),
                        ) {
                            Ok(Some(skeleton)) => {
                                let mut state = state.lock().await;
                                record_complete_file_read(&mut state, &abs_path);
                                state.consecutive_reads.remove(&abs_path_str);
                                (
                                    index,
                                    format!(
                                        "--- {rel_path} ---\nStable Anchors are provided with each line.\n{skeleton}"
                                    ),
                                )
                            }
                            Ok(None) => (index, format!("No definitions found in {rel_path}")),
                            Err(e) => (index, format!("Error parsing {rel_path}: {e}")),
                        },
                        Err(e) => (index, format!("Error reading file {rel_path}: {e}")),
                    }
                }
            });

        results.extend(join_all(futures).await);
        results.sort_by_key(|(index, _)| *index);
        Ok(results
            .into_iter()
            .map(|(_, result)| result)
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[get_file_skeleton]".to_string()
    }
}

impl ToolHandler for GetFileSkeletonHandler {
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

    fn description(&self, params: &serde_json::Value) -> String {
        Self::description(self, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use crate::core::file_editor::AnchorStateManager;

    #[tokio::test]
    async fn test_get_file_skeleton_clears_reread_requirement_after_returning_anchors() {
        let workspace = tempfile::tempdir().unwrap();
        let file_path = workspace.path().join("example.rs");
        std::fs::write(&file_path, "struct Widget;\n").unwrap();
        let canonical_path = std::fs::canonicalize(&file_path).unwrap();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        state
            .lock()
            .await
            .must_reread_before_edit
            .insert(canonical_path.to_string_lossy().into_owned());
        let ctx = ToolContext::new(
            state.clone(),
            None,
            workspace.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let output = GetFileSkeletonHandler
            .run(&ctx, serde_json::json!({"path": "example.rs"}))
            .await
            .unwrap();

        assert!(output.contains("Stable Anchors"));
        assert!(
            !state
                .lock()
                .await
                .must_reread_before_edit
                .contains(&canonical_path.to_string_lossy().into_owned())
        );
    }
}
