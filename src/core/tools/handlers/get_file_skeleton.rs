use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
use crate::services::tree_sitter::get_file_skeleton;
use crate::services::tree_sitter::load_required_language_parsers;
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
                ToolError::ExecutionFailed(format!("Failed to load language parsers: {e}"))
            })?,
        );

        let anchor_mgr = ctx.anchor_mgr.clone();
        let futures = paths
            .iter()
            .zip(abs_paths.iter())
            .map(|(rel_path, abs_path)| {
                let rel_path = rel_path.clone();
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
                        ) {
                            Ok(Some(skeleton)) => {
                                format!(
                                    "--- {rel_path} ---\nStable Anchors are provided with each line.\n{skeleton}"
                                )
                            }
                            Ok(None) => format!("No definitions found in {rel_path}"),
                            Err(e) => format!("Error parsing {rel_path}: {e}"),
                        },
                        Err(e) => format!("Error reading file {rel_path}: {e}"),
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
