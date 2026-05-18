use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
use crate::services::tree_sitter::get_file_skeleton;
use crate::services::tree_sitter::load_required_language_parsers;
use futures::future::join_all;
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
            .map(|rel_path| resolve_sanitized_path(&ctx.workspace_root, rel_path))
            .collect();
        let abs_paths = abs_paths?;
        let language_parsers = Arc::new(
            load_required_language_parsers(
                &abs_paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            )
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to load language parsers: {}", e))
            })?,
        );

        let anchor_mgr = ctx.anchor_mgr.clone();
        let futures = paths
            .iter()
            .zip(abs_paths.iter())
            .map(|(rel_path, abs_path)| {
                let rel_path = rel_path.to_string();
                let abs_path = abs_path.clone();
                let anchor_mgr = anchor_mgr.clone();
                let language_parsers = Arc::clone(&language_parsers);
                async move {
                    let abs_path_str = abs_path.to_string_lossy().into_owned();
                    match tokio::fs::read_to_string(&abs_path).await {
                        Ok(content) => match get_file_skeleton(
                            &anchor_mgr,
                            abs_path_str.as_str(),
                            &content,
                            language_parsers.as_ref(),
                            None,
                            None,
                        ) {
                            Ok(Some(skeleton)) => {
                                format!(
                                    "--- {} ---\nStable Anchors are provided with each line.\n{}",
                                    rel_path, skeleton
                                )
                            }
                            Ok(None) => format!("No definitions found in {}", rel_path),
                            Err(e) => format!("Error parsing {}: {}", rel_path, e),
                        },
                        Err(e) => format!("Error reading file {}: {}", rel_path, e),
                    }
                }
            });

        let results = join_all(futures).await;
        Ok(results.join("\n\n"))
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[get_file_skeleton]".to_string()
    }
}

#[async_trait::async_trait]
impl ToolHandler for GetFileSkeletonHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        Self::run(self, ctx, params)
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        GetFileSkeletonHandler::description(self, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use crate::core::file_editor::AnchorStateManager;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock};

    static TEST_MUTEX: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct CwdGuard(PathBuf);

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    fn build_context(workspace_root: &Path, anchor_mgr: AnchorStateManager) -> ToolContext {
        ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
            None,
            workspace_root.to_path_buf(),
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
        )
    }

    #[tokio::test]
    async fn test_multi_file_reuses_context_workspace_root_and_anchors() {
        let _guard = TEST_MUTEX.lock().await;

        let workspace_root = tempfile::tempdir().unwrap();
        let root = workspace_root.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "fn beta() {}\n").unwrap();

        let wrong_cwd = std::env::temp_dir().join("sned_wrong_cwd_for_skeleton_test");
        std::fs::create_dir_all(&wrong_cwd).unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        let _cwd_guard = CwdGuard(original_cwd.clone());
        std::env::set_current_dir(&wrong_cwd).unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let ctx = build_context(root, anchor_mgr.clone());
        let handler = GetFileSkeletonHandler;
        let params = serde_json::json!({
            "paths": ["src/a.rs", "src/b.rs"]
        });

        let first = handler.run(&ctx, params.clone()).await.unwrap();
        let second = handler.run(&ctx, params).await.unwrap();

        assert!(first.contains("a.rs") || first.contains("alpha"));
        assert!(first.contains("b.rs") || first.contains("beta"));
        assert_eq!(
            first, second,
            "Repeated calls should reuse anchor state and stay stable"
        );
    }
}
