//! Write to file tool handler for sned CLI.
//!

use crate::cli::actionable_errors;
use crate::core::tools::handlers::error_guidance;
use crate::core::tools::{
    ToolContext, ToolError, ToolFailureClass, ToolFailureMetadata, ToolHandler,
};
use crate::services::symbol_index::SymbolIndexService;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct WriteToFileHandler {
    symbol_index_service: Option<Arc<std::sync::Mutex<SymbolIndexService>>>,
}

impl WriteToFileHandler {
    fn format_missing_content_error(path: &str, consecutive_failures: u32) -> String {
        let base = format!(
            "Failed to write '{path}': the 'content' parameter was empty. This usually means the model ran out of output budget or tried to emit the file in one oversized response."
        );

        match consecutive_failures {
            0 | 1 => format!(
                "{base} Try writing a smaller skeleton first, then use edit_file for the remaining sections."
            ),
            2 => format!(
                "{base} This is the second failed attempt. Switch strategies: write a minimal skeleton first, then fill sections incrementally with edit_file."
            ),
            _ => format!(
                "{base} This has failed {consecutive_failures} times in a row. Stop retrying write_to_file for this file and create a skeleton or split the file into smaller pieces before continuing."
            ),
        }
    }

    fn workspace_relative_display_path(workspace_root: &Path, requested_path: &str) -> String {
        let requested_path = Path::new(requested_path);
        requested_path
            .strip_prefix(workspace_root)
            .unwrap_or(requested_path)
            .to_string_lossy()
            .into_owned()
    }

    /// Write content to a file.
    ///
    pub async fn write_file(
        &self,
        path: &str,
        content: &str,
        workspace_root: &Path,
    ) -> anyhow::Result<String> {
        self.write_file_with_allowed_roots(path, content, workspace_root, &[])
            .await
    }

    async fn write_file_with_allowed_roots(
        &self,
        path: &str,
        content: &str,
        workspace_root: &Path,
        allowed_external_roots: &[PathBuf],
    ) -> anyhow::Result<String> {
        let resolved = crate::core::tools::resolve_authorized_path(
            workspace_root,
            allowed_external_roots,
            path,
        )?;
        let _guard =
            crate::core::file_editor::FileEditGuard::acquire(&resolved.to_string_lossy()).await;
        self.write_file_unlocked(path, content, workspace_root, allowed_external_roots)
            .await
    }

    async fn write_file_unlocked(
        &self,
        path: &str,
        content: &str,
        workspace_root: &Path,
        allowed_external_roots: &[PathBuf],
    ) -> anyhow::Result<String> {
        use tokio::fs;

        // Capture whether the file existed before the write so the
        // success message can explicitly flag overwrite operations.
        // Without this, the model feared write_to_file as a destructive
        // op; making the overwrite explicit reduces that hesitation.
        let file_existed_before = Path::new(path).exists();

        // Canonicalize workspace root once for consistent comparison
        let canonical_workspace = fs::canonicalize(workspace_root)
            .await
            .unwrap_or_else(|_| workspace_root.to_path_buf());

        let path_obj = Path::new(path);

        // Create parent directories if they don't exist
        if let Some(parent) = path_obj.parent() {
            fs::create_dir_all(parent).await?;

            // Re-verify parent directory after creation to catch symlink race
            let canonical_parent = fs::canonicalize(parent).await?;

            if !canonical_parent.starts_with(&canonical_workspace)
                && !allowed_external_roots
                    .iter()
                    .any(|root| canonical_parent.starts_with(root))
            {
                anyhow::bail!(
                    "Parent directory {} resolved to {} which is outside workspace {}",
                    parent.display(),
                    canonical_parent.display(),
                    canonical_workspace.display()
                );
            }
        }

        // Final canonicalization check immediately before write
        // Use parent + filename if file doesn't exist yet
        let final_canonical = if path_obj.exists() {
            fs::canonicalize(path)
                .await
                .unwrap_or_else(|_| PathBuf::from(path))
        } else {
            // File doesn't exist yet - canonicalize parent and append filename
            let parent = path_obj.parent().unwrap_or_else(|| Path::new("."));
            let canonical_parent = fs::canonicalize(parent)
                .await
                .unwrap_or_else(|_| PathBuf::from(parent));
            canonical_parent.join(path_obj.file_name().unwrap_or_default())
        };

        if !final_canonical.starts_with(&canonical_workspace)
            && !allowed_external_roots
                .iter()
                .any(|root| final_canonical.starts_with(root))
        {
            anyhow::bail!(
                "Path {} resolved to {} which is outside workspace {} (symlink detected)",
                path,
                final_canonical.display(),
                canonical_workspace.display()
            );
        }

        // Write the file atomically using async I/O (avoids spawn_blocking overhead)
        crate::storage::disk::atomic_write_file_async(&final_canonical, content).await?;

        if file_existed_before {
            Ok(format!(
                "File {path} existed and was overwritten.\nSuccessfully wrote to {path}."
            ))
        } else {
            Ok(format!("Successfully wrote to {path} (new file)."))
        }
    }

    #[must_use]
    pub fn with_symbol_index(mut self, service: Arc<std::sync::Mutex<SymbolIndexService>>) -> Self {
        self.symbol_index_service = Some(service);
        self
    }

    async fn execute_with_workspace(
        &self,
        params: serde_json::Value,
        workspace_root: &Path,
        allowed_external_roots: &[PathBuf],
    ) -> Result<String, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput(error_guidance::missing_parameter("path", 0)))?;
        let content = params["content"].as_str().ok_or_else(|| {
            ToolError::InvalidInput(error_guidance::missing_parameter("content", 0))
        })?;
        if content.is_empty() {
            return Err(ToolError::InvalidInput(error_guidance::empty_content(
                path, 0,
            )));
        }

        self.write_file_unlocked(path, content, workspace_root, allowed_external_roots)
            .await
            .map_err(|e| {
                if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                    match io_err.kind() {
                        std::io::ErrorKind::PermissionDenied => {
                            ToolError::ExecutionFailedWithMetadata(
                                actionable_errors::permission_denied(path, "write to").to_string(),
                                ToolFailureMetadata {
                                    class: ToolFailureClass::PermissionDenied,
                                    affected_paths: vec![path.to_string()],
                                    required_next_step: None,
                                },
                            )
                        }
                        _ => ToolError::ExecutionFailed(format!(
                            "Failed to write '{path}': {io_err}"
                        )),
                    }
                } else {
                    ToolError::ExecutionFailed(e.to_string())
                }
            })
    }
    #[must_use]
    pub fn new() -> Self {
        Self {
            symbol_index_service: None,
        }
    }
}

impl ToolHandler for WriteToFileHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let consecutive_mistakes = ctx.state.lock().await.consecutive_mistakes;
            let path = params["path"].as_str().ok_or_else(|| {
                ToolError::InvalidInput(error_guidance::missing_parameter(
                    "path",
                    consecutive_mistakes,
                ))
            })?;
            let path = path.to_string();
            let display_path =
                Self::workspace_relative_display_path(ctx.workspace_root.as_path(), &path);
            let resolved_path = ctx.resolve_path(&path)?;
            let mut resolved_params = params;
            if let Some(obj) = resolved_params.as_object_mut() {
                obj.insert(
                    "path".to_string(),
                    serde_json::Value::String(resolved_path.to_string_lossy().to_string()),
                );
            }

            let content = resolved_params["content"]
                .as_str()
                .ok_or_else(|| {
                    ToolError::InvalidInput(error_guidance::missing_parameter(
                        "content",
                        consecutive_mistakes,
                    ))
                })?
                .to_string();
            let lines_added = content.lines().count() as u32;

            // Keep reads and edits from observing this file while it is being
            // written. The guard remains held through the state update below.
            let _file_locks = ctx
                .lock_file_paths(std::slice::from_ref(&resolved_path))
                .await;

            if content.is_empty() {
                let mut state = ctx.state.lock().await;
                state.consecutive_mistakes += 1;
                tracing::warn!(
                    consecutive_mistakes = state.consecutive_mistakes,
                    path = %path,
                    "write_to_file: empty content provided"
                );
                let message = Self::format_missing_content_error(&path, state.consecutive_mistakes);
                return Err(ToolError::InvalidInput(message));
            }

            let result = handler
                .execute_with_workspace(
                    resolved_params,
                    ctx.workspace_root.as_path(),
                    &ctx.allowed_external_roots,
                )
                .await;
            match result {
                Ok(_) => {
                    ctx.invalidate_edit_context(&resolved_path).await;
                    let file_context_metadata = {
                        let mut state = ctx.state.lock().await;
                        state.consecutive_mistakes = 0;
                        // Update in memory while holding the state lock, but
                        // defer the synchronous metadata write until after it
                        // is released.
                        state.file_context_tracker.track_file_context_in_memory(
                            &resolved_path.to_string_lossy(),
                            crate::core::context::trackers::FileRecordSource::SnedEdited,
                        );
                        // Mark file as edited by Sned to suppress stale mtime detection
                        state
                            .file_context_tracker
                            .mark_file_as_edited_by_sned(&resolved_path);
                        let entry = state
                            .session_file_changes
                            .entry(resolved_path.to_string_lossy().to_string())
                            .or_insert_with(|| crate::core::agent_types::FileChangeStats {
                                lines_added: 0,
                                lines_removed: 0,
                                action: "created".to_string(),
                            });
                        entry.lines_added = entry.lines_added.saturating_add(lines_added);
                        state.file_context_tracker.files_in_context().to_vec()
                    };
                    let task_id = ctx.task_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(storage) =
                            crate::storage::task_storage::TaskStorage::new(&task_id)
                        {
                            let _ = storage.save_file_context_metadata(&file_context_metadata);
                        }
                    })
                    .await;
                    if let Some(symbol_index_service) = &handler.symbol_index_service {
                        crate::services::symbol_index::index_file_after_write(
                            Arc::clone(symbol_index_service),
                            ctx.workspace_root.as_path(),
                            &display_path,
                            &content,
                        )
                        .await;
                    }
                    Ok(serde_json::Value::String(format!(
                        "Successfully wrote to {display_path}"
                    )))
                }
                Err(err) => {
                    let mut state = ctx.state.lock().await;
                    state.consecutive_mistakes += 1;
                    tracing::warn!(
                        consecutive_mistakes = state.consecutive_mistakes,
                        path = %resolved_path.display(),
                        error = %err,
                        "write_to_file: write failed"
                    );
                    Err(err)
                }
            }
        })
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let path = params["path"].as_str().unwrap_or("unknown file");
        format!("Writing to {path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use crate::core::file_editor::AnchorStateManager;
    use crate::core::tools::{ToolContext, ToolHandler};
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_write_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let handler = WriteToFileHandler::new();

        let result = handler
            .write_file(file_path.to_str().unwrap(), "hello world", temp_dir.path())
            .await
            .unwrap();
        assert!(result.contains("Successfully wrote to"));
        assert_eq!(fs::read_to_string(file_path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_write_file_allows_authorized_external_directory() {
        let workspace = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let file_path = external.path().join("generated.sql");
        let handler = WriteToFileHandler::new();

        handler
            .write_file_with_allowed_roots(
                file_path.to_str().unwrap(),
                "select 1;\n",
                workspace.path(),
                &[external.path().canonicalize().unwrap()],
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(file_path).unwrap(), "select 1;\n");
    }

    #[tokio::test]
    async fn test_write_file_preserves_content() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_content.txt");
        let handler = WriteToFileHandler::new();

        let content = "a1b2c3d4: line 1\na5b6c7d8: line 2";
        handler
            .write_file(file_path.to_str().unwrap(), content, temp_dir.path())
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(file_path).unwrap(), content);
    }

    /// Field incident regression: when the model falls back to
    /// `write_to_file` after `edit_file` rejects (or after a duplicate
    /// line scenario), the success message must explicitly state
    /// whether the file existed and was overwritten, so the model
    /// cannot misread the tool as having silently created a new file
    /// when it actually replaced an existing one.
    #[tokio::test]
    async fn test_write_file_result_names_overwrite_vs_create() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("overwrite.txt");
        std::fs::write(&file_path, "old content").unwrap();

        let handler = WriteToFileHandler::new();
        let overwrite_msg = handler
            .write_file(file_path.to_str().unwrap(), "new content", temp_dir.path())
            .await
            .unwrap();
        assert!(
            overwrite_msg.contains("existed and was overwritten"),
            "overwrite path must be flagged explicitly: {overwrite_msg}"
        );
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "new content");

        let new_path = temp_dir.path().join("fresh.txt");
        let create_msg = handler
            .write_file(new_path.to_str().unwrap(), "fresh", temp_dir.path())
            .await
            .unwrap();
        assert!(
            create_msg.contains("new file"),
            "create path must be flagged as new: {create_msg}"
        );
    }

    #[tokio::test]
    async fn test_write_file_create_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("subdir/nested/test.txt");
        let handler = WriteToFileHandler::new();

        handler
            .write_file(file_path.to_str().unwrap(), "nested", temp_dir.path())
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(file_path).unwrap(), "nested");
    }

    #[tokio::test]
    async fn test_concurrent_writes_no_corruption() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("concurrent.txt");
        let handler = WriteToFileHandler::new();

        // Spawn multiple concurrent writes
        let mut handles = Vec::new();
        for i in 0..10 {
            let handler = handler.clone();
            let path = file_path.to_str().unwrap().to_string();
            let content = format!("content-{}", i);
            let workspace = temp_dir.path().to_path_buf();
            handles.push(tokio::spawn(async move {
                handler
                    .write_file(&path, &content, &workspace)
                    .await
                    .unwrap();
            }));
        }

        // Wait for all writes to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify file content is valid (should be one of the written values, not corrupted)
        let final_content = fs::read_to_string(&file_path).unwrap();
        let is_valid = (0..10).any(|i| final_content == format!("content-{}", i));
        assert!(
            is_valid,
            "File content should not be corrupted: got '{}'",
            final_content
        );
    }

    #[tokio::test]
    async fn test_write_file_large_payload_sizes() {
        let temp_dir = TempDir::new().unwrap();
        let handler = WriteToFileHandler::new();
        let cases = [
            ("1kb.txt", 1024usize),
            ("5kb.txt", 5 * 1024usize),
            ("10kb.txt", 10 * 1024usize),
            ("50kb.txt", 50 * 1024usize),
        ];

        for (name, size) in cases {
            let path = temp_dir.path().join(name);
            let content = "x".repeat(size);
            handler
                .write_file(path.to_str().unwrap(), &content, temp_dir.path())
                .await
                .unwrap();
            let written = fs::read_to_string(path).unwrap();
            assert_eq!(written.len(), size);
            assert_eq!(written, content);
        }
    }

    #[tokio::test]
    async fn test_execute_uses_workspace_root_not_process_cwd() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();

        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": "nested/output.go",
                "content": "package main\n"
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            serde_json::json!("Successfully wrote to nested/output.go")
        );
        assert!(workspace_root.path().join("nested/output.go").exists());
    }

    fn test_ctx(workspace_root: &Path) -> ToolContext {
        ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
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
    async fn test_execute_rejects_parent_traversal() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();
        let ctx = test_ctx(workspace_root.path());
        let outside_filename = format!(
            "escape-{}.txt",
            workspace_root
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace")
        );
        let outside_path = workspace_root
            .path()
            .parent()
            .unwrap()
            .join(&outside_filename);

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": format!("../{outside_filename}"),
                "content": "escaped"
            }),
        )
        .await;

        assert!(result.is_err(), "traversal above workspace must fail");
        assert!(
            !outside_path.exists(),
            "no file may be created outside the workspace"
        );
    }

    #[tokio::test]
    async fn test_execute_rejects_absolute_path_outside_workspace() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let ctx = test_ctx(workspace_root.path());
        let outside_path = outside.path().join("escape.txt");

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": outside_path.to_string_lossy(),
                "content": "escaped"
            }),
        )
        .await;

        assert!(result.is_err(), "absolute path outside workspace must fail");
        assert!(!outside_path.exists());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_execute_rejects_symlinked_parent_escaping_workspace() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let link = workspace_root.path().join("linked_dir");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let ctx = test_ctx(workspace_root.path());

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": "linked_dir/escape.txt",
                "content": "escaped"
            }),
        )
        .await;

        assert!(
            result.is_err(),
            "symlinked parent escaping workspace must fail"
        );
        assert!(
            !outside.path().join("escape.txt").exists(),
            "no file may be created via symlink escape"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_execute_rejects_symlink_file_pointing_outside_workspace() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("target.txt");
        std::fs::write(&outside_file, "original").unwrap();
        let link = workspace_root.path().join("link.txt");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        let ctx = test_ctx(workspace_root.path());

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": "link.txt",
                "content": "overwritten"
            }),
        )
        .await;

        assert!(
            result.is_err(),
            "writing through an escaping symlink must fail"
        );
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            "original",
            "external target must remain untouched"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_execute_rejects_dangling_symlink_escaping_workspace() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // Dangling: the symlink target does not exist.
        let link = workspace_root.path().join("dangling.txt");
        std::os::unix::fs::symlink(outside.path().join("missing.txt"), &link).unwrap();
        let ctx = test_ctx(workspace_root.path());

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": "dangling.txt",
                "content": "escaped"
            }),
        )
        .await;

        assert!(result.is_err(), "dangling symlink escape must fail");
        assert!(
            !outside.path().join("missing.txt").exists(),
            "no file may be created via dangling symlink"
        );
    }

    #[tokio::test]
    async fn test_execute_rejects_external_root_without_authorization() {
        // Defense-in-depth at the execute() layer: even though ToolContext
        // with no allowed_external_roots, an absolute external path must fail.
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let ctx = test_ctx(workspace_root.path());
        let external_path = external.path().join("unauthorized.txt");

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": external_path.to_string_lossy(),
                "content": "escaped"
            }),
        )
        .await;

        assert!(result.is_err());
        assert!(!external_path.exists());
    }

    #[tokio::test]
    async fn test_execute_refreshes_symbol_index() {
        let workspace_root = TempDir::new().unwrap();
        let index = Arc::new(std::sync::Mutex::new(SymbolIndexService::new(
            workspace_root.path().to_string_lossy().into_owned(),
        )));
        let handler = WriteToFileHandler::new().with_symbol_index(Arc::clone(&index));
        let ctx = ToolContext::new(
            Arc::new(Mutex::new(TaskState::default())),
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        handler
            .execute(
                &ctx,
                serde_json::json!({
                    "path": "indexed.rs",
                    "content": "fn write_indexed_symbol() {}\n",
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            index
                .lock()
                .unwrap()
                .get_definitions("write_indexed_symbol", None)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_execute_rejects_empty_content() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();

        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": "empty.txt",
                "content": ""
            }),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("content") && err.contains("edit_file"),
            "Error should mention empty content and suggest edit_file: {}",
            err
        );

        let state = ctx.state.lock().await;
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_execute_escalates_empty_content_guidance() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();

        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let first = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": "retry.txt",
                "content": ""
            }),
        )
        .await;
        assert!(first.is_err());
        let first_err = first.unwrap_err().to_string();
        assert!(first_err.contains("skeleton"));

        let second = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": "retry.txt",
                "content": ""
            }),
        )
        .await;
        assert!(second.is_err());
        let second_err = second.unwrap_err().to_string();
        assert!(second_err.contains("second failed attempt") || second_err.contains("retrying"));

        let state = state.lock().await;
        assert_eq!(state.consecutive_mistakes, 2);
    }

    #[tokio::test]
    async fn test_execute_resets_mistakes_on_success() {
        let handler = WriteToFileHandler::new();
        let workspace_root = TempDir::new().unwrap();

        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        {
            let mut guard = state.lock().await;
            guard.consecutive_mistakes = 2;
        }
        let ctx = ToolContext::new(
            state.clone(),
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({
                "path": "ok.txt",
                "content": "hello"
            }),
        )
        .await
        .unwrap();

        assert!(result.as_str().unwrap().contains("Successfully wrote to"));

        let state = state.lock().await;
        assert_eq!(state.consecutive_mistakes, 0);
    }
}
