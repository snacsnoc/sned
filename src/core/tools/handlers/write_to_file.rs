//! Write to file tool handler for sned CLI.
//!

use crate::cli::actionable_errors;
use crate::core::tools::handlers::error_guidance;
use crate::core::tools::{
    ToolContext, ToolError, ToolFailureClass, ToolFailureMetadata, ToolHandler,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

#[derive(Debug, Clone, Default)]
pub struct WriteToFileHandler;

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

    fn resolve_path(workspace_root: &Path, path: &str) -> Result<PathBuf, ToolError> {
        crate::core::tools::resolve_sanitized_path(workspace_root, path)
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
        use crate::core::file_editor::FileEditGuard;
        use tokio::fs;

        // Acquire exclusive file lock to prevent concurrent writes
        let _guard = FileEditGuard::acquire(path).await;

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

            if !canonical_parent.starts_with(&canonical_workspace) {
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
            let parent = path_obj.parent().unwrap_or(Path::new("."));
            let canonical_parent = fs::canonicalize(parent)
                .await
                .unwrap_or_else(|_| PathBuf::from(parent));
            canonical_parent.join(path_obj.file_name().unwrap_or_default())
        };

        if !final_canonical.starts_with(&canonical_workspace) {
            anyhow::bail!(
                "Path {} resolved to {} which is outside workspace {} (symlink detected)",
                path,
                final_canonical.display(),
                canonical_workspace.display()
            );
        }

        // Write the file atomically using async I/O (avoids spawn_blocking overhead)
        crate::storage::disk::atomic_write_file_async(&final_canonical, content).await?;

        Ok(format!("Successfully wrote to {path}"))
    }

    async fn execute_with_workspace(
        &self,
        params: serde_json::Value,
        workspace_root: &Path,
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

        self.write_file(path, content, workspace_root)
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
        Self
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
            let resolved_path = Self::resolve_path(ctx.workspace_root.as_path(), &path)?;
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
                .execute_with_workspace(resolved_params, ctx.workspace_root.as_path())
                .await;
            match result {
                Ok(_) => {
                    let mut state = ctx.state.lock().await;
                    state.consecutive_mistakes = 0;
                    // Track newly created file to suppress "not read this session" warning on subsequent edits
                    state
                        .file_context_tracker
                        .track_file_context(
                            &resolved_path.to_string_lossy(),
                            crate::core::context::trackers::FileRecordSource::SnedEdited,
                        )
                        .await;
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

    struct CwdGuard {
        original: std::path::PathBuf,
    }

    impl CwdGuard {
        fn set_to(path: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

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
        let wrong_cwd = TempDir::new().unwrap();
        let _guard = CwdGuard::set_to(wrong_cwd.path());

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
        assert!(!wrong_cwd.path().join("nested/output.go").exists());
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
