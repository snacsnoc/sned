//! Write to file tool handler for sned CLI.
//!

use crate::cli::actionable_errors;
use crate::core::tools::{
    ToolContext, ToolError, ToolFailureClass, ToolFailureMetadata, ToolHandler,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct WriteToFileHandler;

impl WriteToFileHandler {
    fn format_missing_content_error(path: &str, consecutive_failures: u32) -> String {
        let base = format!(
            "Failed to write '{}': the 'content' parameter was empty. This usually means the model ran out of output budget or tried to emit the file in one oversized response.",
            path
        );

        match consecutive_failures {
            0 | 1 => format!(
                "{} Try writing a smaller skeleton first, then use edit_file for the remaining sections.",
                base
            ),
            2 => format!(
                "{} This is the second failed attempt. Switch strategies: write a minimal skeleton first, then fill sections incrementally with edit_file.",
                base
            ),
            _ => format!(
                "{} This has failed {} times in a row. Stop retrying write_to_file for this file and create a skeleton or split the file into smaller pieces before continuing.",
                base, consecutive_failures
            ),
        }
    }

    fn resolve_path(workspace_root: &Path, path: &str) -> Result<PathBuf, ToolError> {
        crate::core::tools::resolve_sanitized_path(workspace_root, path)
    }

    /// Write content to a file.
    ///
    pub async fn write_file(&self, path: &str, content: &str) -> anyhow::Result<String> {
        use crate::core::file_editor::FileEditGuard;
        use tokio::fs;

        let path_obj = Path::new(path);

        // Acquire exclusive file lock to prevent concurrent writes
        let _guard = FileEditGuard::acquire(path).await;

        // Create parent directories if they don't exist
        if let Some(parent) = path_obj.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Write the file atomically using async I/O (avoids spawn_blocking overhead)
        crate::storage::disk::atomic_write_file_async(path_obj, content).await?;

        Ok(format!("Successfully wrote to {}", path))
    }

    async fn execute_without_state(&self, params: serde_json::Value) -> Result<String, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("Missing 'path' parameter".to_string()))?;
        let content = params["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("Missing 'content' parameter".to_string()))?;
        if content.is_empty() {
            return Err(ToolError::InvalidInput(
                "'content' parameter must not be empty".to_string(),
            ));
        }

        self.write_file(path, content).await.map_err(|e| {
            if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                match io_err.kind() {
                    std::io::ErrorKind::PermissionDenied => ToolError::ExecutionFailedWithMetadata(
                        actionable_errors::permission_denied(path, "write to").to_string(),
                        ToolFailureMetadata {
                            class: ToolFailureClass::PermissionDenied,
                            affected_paths: vec![path.to_string()],
                            required_next_step: None,
                        },
                    ),
                    _ => ToolError::ExecutionFailed(format!(
                        "Failed to write '{}': {}",
                        path, io_err
                    )),
                }
            } else {
                ToolError::ExecutionFailed(e.to_string())
            }
        })
    }
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolHandler for WriteToFileHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("Missing 'path' parameter".to_string()))?;
        let path = path.to_string();
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
            .ok_or_else(|| ToolError::InvalidInput("Missing 'content' parameter".to_string()))?
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

        let result = self.execute_without_state(resolved_params).await;
        match result {
            Ok(text) => {
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
                // Record file change for session summary
                let entry = state
                    .session_file_changes
                    .entry(resolved_path.to_string_lossy().to_string())
                    .or_insert_with(|| crate::core::agent_types::FileChangeStats {
                        lines_added: 0,
                        lines_removed: 0,
                        action: "created".to_string(),
                    });
                entry.lines_added = entry.lines_added.saturating_add(lines_added);
                Ok(serde_json::Value::String(text))
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
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let path = params["path"].as_str().unwrap_or("unknown file");
        format!("Writing to {}", path)
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
            .write_file(file_path.to_str().unwrap(), "hello world")
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
            .write_file(file_path.to_str().unwrap(), content)
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
            .write_file(file_path.to_str().unwrap(), "nested")
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
            handles.push(tokio::spawn(async move {
                handler.write_file(&path, &content).await.unwrap();
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
                .write_file(path.to_str().unwrap(), &content)
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

        assert!(result.as_str().unwrap().contains("Successfully wrote to"));
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
