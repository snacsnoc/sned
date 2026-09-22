//! Write to file tool handler for sned CLI.
//!

use crate::cli::actionable_errors;
use crate::core::stream_parsing::{is_fence_closer, parse_fence_start};
use crate::core::tools::handlers::error_guidance;
use crate::core::tools::{
    ToolContext, ToolError, ToolFailureClass, ToolFailureMetadata, ToolHandler,
};
use crate::services::symbol_index::SymbolIndexService;
use std::borrow::Cow;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// Strip an outer CommonMark code fence pair from `content` only when doing so
/// is unambiguous. Returns the inner body on success, or the original content
/// unchanged when the body isn't a fence-wrapped block.
///
/// Reuses `parse_fence_start` / `is_fence_closer` from the streaming parser so
/// indentation, marker type, run length, and closer-vs-opener length are
/// handled in lockstep with the rest of Sned.
///
/// Extensions are checked to avoid corrupting markdown documents that
/// legitimately begin with a non-markdown code block (e.g. a README whose
/// only content is a shell snippet). Markdown files only lose their outer
/// fence when the opener's info string is `md` or `markdown`.
fn strip_outer_markdown_fences<'a>(path: &'a Path, content: &'a str) -> Cow<'a, str> {
    if content.is_empty() {
        return Cow::Borrowed(content);
    }

    let mut lines = content.split('\n');
    let Some(first_line) = lines.next() else {
        return Cow::Borrowed(content);
    };
    let Some((opener, info)) = parse_fence_start(first_line) else {
        return Cow::Borrowed(content);
    };

    let last_line = content
        .lines()
        .rev()
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if !is_fence_closer(last_line, opener) {
        return Cow::Borrowed(content);
    }

    let ext_lc = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    let info_lc = info.trim().to_ascii_lowercase();
    let is_markdown_extension = matches!(
        ext_lc.as_deref(),
        Some("md" | "markdown" | "mdx" | "mdown" | "mkdn")
    );
    let markdown_only = is_markdown_extension && !matches!(info_lc.as_str(), "md" | "markdown");
    if markdown_only {
        return Cow::Borrowed(content);
    }

    let opener_prefix_len = first_line.len();
    let after_opener = &content[opener_prefix_len..];
    // Skip the newline that terminated the opener line — it belongs to the
    // opener, not to the fence body.
    let body_start = after_opener.strip_prefix('\n').unwrap_or(after_opener);

    // CommonMark §4.5: the first fence line whose run length is ≥ the opener
    // closes the block. Iterate line-by-line using `is_fence_closer` so the
    // first matching closer terminates the block, rather than doing an
    // ambiguous substring search or picking the last closer via `rfind`.
    let mut offset = 0;
    let mut closer_found = false;
    let mut body_end = 0;
    let mut after_closer_start = body_start.len();

    for line in body_start.split_inclusive('\n') {
        let trimmed_line = line
            .strip_suffix("\r\n")
            .or_else(|| line.strip_suffix('\n'))
            .unwrap_or(line);
        if is_fence_closer(trimmed_line, opener) {
            body_end = offset;
            after_closer_start = offset + line.len();
            closer_found = true;
            break;
        }
        offset += line.len();
    }

    if closer_found {
        let body = &body_start[..body_end];
        let after_closer = &body_start[after_closer_start..];
        Cow::Owned(format!("{body}{after_closer}"))
    } else if body_start.trim() == last_line.trim() || body_start.trim().is_empty() {
        // Empty fenced body (e.g. ````\n````\n``); downstream `is_empty`
        // check on the cleaned content will surface the empty-content error.
        Cow::Owned(String::new())
    } else {
        Cow::Borrowed(content)
    }
}

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
            // LLMs frequently wrap file bodies in markdown code fences; strip
            // the outer pair so fence characters don't leak into source files.
            let content = strip_outer_markdown_fences(Path::new(&path), &content).into_owned();
            if let Some(obj) = resolved_params.as_object_mut() {
                obj.insert(
                    "content".to_string(),
                    serde_json::Value::String(content.clone()),
                );
            }
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
                    {
                        // Phase 4b cleanup: mirror edit_file's per-file state
                        // reset so the read-loop and edit-thrash detectors
                        // see a coherent file. Without this, write_to_file
                        // creates an asymmetry where full-rewrites leave
                        // stale read windows + edit counts pointing at
                        // pre-rewrite bytes. edit_file already does this in
                        // its own Phase 4b; write_to_file must do the same
                        // because it overwrites the file in one shot rather
                        // than applying incremental edits.
                        let mut state = ctx.state.lock().await;
                        let key = crate::core::tools::canonical_path_key(&resolved_path);
                        state.consecutive_reads.remove(&key);
                        state.last_read_turn.remove(&key);
                        state.recent_read_windows.remove(&key);
                        state.read_file_snapshots.remove(&key);
                        state.visible_read_coverage.remove(&key);
                        // Increment consecutive_edits so a subsequent build
                        // failure can surface the thrashing diagnostic.
                        // Cleared by agent_loop when build/test succeeds, or
                        // by invalidate_restored_file_context on checkpoint
                        // restore.
                        let count = state.consecutive_edits.entry(key).or_insert(0);
                        *count += 1;
                    }
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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

    #[test]
    fn strip_outer_fence_removes_backtick_pair_from_gitignore() {
        let stripped =
            strip_outer_markdown_fences(Path::new(".gitignore"), "```\nfoo\n```\n").into_owned();
        assert_eq!(stripped, "foo\n");
    }

    #[test]
    fn strip_outer_fence_removes_language_fence_from_rust_file() {
        let stripped =
            strip_outer_markdown_fences(Path::new("main.rs"), "```rust\nfn main(){}\n```\n")
                .into_owned();
        assert_eq!(stripped, "fn main(){}\n");
    }

    #[test]
    fn strip_outer_fence_accepts_longer_closer() {
        let stripped =
            strip_outer_markdown_fences(Path::new("foo.txt"), "```\nfoo\n````\n").into_owned();
        assert_eq!(stripped, "foo\n");
    }

    #[test]
    fn strip_outer_fence_uses_first_closer_when_body_contains_longer_fence() {
        // CommonMark §4.5: the FIRST fence whose run length ≥ opener's closes
        // the block. A 4-backtick fence inside a 3-backtick outer fence is a
        // valid closer for the outer (4 ≥ 3), so the outer fence ends at the
        // first 4-backtick row — leaving the body before it and everything
        // after it. Previously the implementation used `rfind`, which
        // incorrectly matched the LAST closer and left the first closer row
        // as stray content in the body.
        let stripped = strip_outer_markdown_fences(
            Path::new("foo.txt"),
            "```\nfirst\n````\nsecond\n````\n",
        )
        .into_owned();
        assert_eq!(
            stripped, "first\nsecond\n````\n",
            "first valid closer wins, not the last"
        );

        // When the first valid closer sits immediately after the opener line:
        let stripped_immediate = strip_outer_markdown_fences(
            Path::new("foo.txt"),
            "```\n````\nfoo\n````\n",
        )
        .into_owned();
        assert_eq!(
            stripped_immediate, "foo\n````\n",
            "immediate first closer leaves subsequent lines intact"
        );
    }

    #[test]
    fn strip_outer_fence_removes_tilde_fence_from_config() {
        let stripped =
            strip_outer_markdown_fences(Path::new("config.toml"), "~~~toml\nkey = \"v\"\n~~~\n")
                .into_owned();
        assert_eq!(stripped, "key = \"v\"\n");
    }

    #[test]
    fn strip_outer_fence_preserves_rust_fence_in_readme() {
        let original = "```rust\ncargo install sned\n```\n";
        let stripped = strip_outer_markdown_fences(Path::new("README.md"), original).into_owned();
        assert_eq!(
            stripped, original,
            "non-markdown info string must not be stripped from .md"
        );
    }

    #[test]
    fn strip_outer_fence_removes_markdown_info_string_from_markdown_file() {
        let stripped =
            strip_outer_markdown_fences(Path::new("README.md"), "```markdown\n# Title\n```\n")
                .into_owned();
        assert_eq!(stripped, "# Title\n");
    }

    #[test]
    fn strip_outer_fence_removes_md_info_string_from_markdown_file() {
        let stripped = strip_outer_markdown_fences(Path::new("README.md"), "```md\n# Title\n```\n")
            .into_owned();
        assert_eq!(stripped, "# Title\n");
    }

    #[test]
    fn strip_outer_fence_preserves_naked_outer_fence_in_markdown_file() {
        let original = "```\nsome snippet\n```\n";
        let stripped = strip_outer_markdown_fences(Path::new("README.md"), original).into_owned();
        assert_eq!(
            stripped, original,
            "naked fence on a markdown file must not be stripped"
        );
    }

    #[test]
    fn strip_outer_fence_extension_check_is_case_insensitive() {
        let stripped =
            strip_outer_markdown_fences(Path::new("README.MD"), "```markdown\n# Title\n```\n")
                .into_owned();
        assert_eq!(stripped, "# Title\n");
    }

    #[test]
    fn strip_outer_fence_preserves_inner_only_fence() {
        let original = "before\n```\nliteral\n```\nafter\n";
        let stripped = strip_outer_markdown_fences(Path::new("main.rs"), original).into_owned();
        assert_eq!(stripped, original);
    }

    #[test]
    fn strip_outer_fence_preserves_partial_fence_without_closer() {
        let original = "```rust\nfoo\n";
        let stripped = strip_outer_markdown_fences(Path::new("main.rs"), original).into_owned();
        assert_eq!(stripped, original);
    }

    #[test]
    fn strip_outer_fence_preserves_short_closer_with_long_opener() {
        // CommonMark requires the closer to be at least as long as the opener.
        // `````\nfoo\n``` has a shorter closer; must NOT be stripped.
        let original = "`````rust\nfoo\n```\n";
        let stripped = strip_outer_markdown_fences(Path::new("main.rs"), original).into_owned();
        assert_eq!(stripped, original);
    }

    #[test]
    fn strip_outer_fence_preserves_non_fenced_content() {
        let original = "plain text\n";
        let stripped =
            strip_outer_markdown_fences(Path::new("anything.txt"), original).into_owned();
        assert_eq!(stripped, original);
    }

    #[test]
    fn strip_outer_fence_preserves_trailing_newline_after_strip() {
        // The original closer was followed by an extra blank line; the strip
        // must keep that blank line since it lies outside the fence pair.
        let stripped =
            strip_outer_markdown_fences(Path::new(".gitignore"), "```\nfoo\n```\n\n").into_owned();
        assert_eq!(stripped, "foo\n\n");
    }

    #[tokio::test]
    async fn test_write_to_file_strips_outer_markdown_fence_end_to_end() {
        let workspace_root = TempDir::new().unwrap();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "fence-strip".into(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
            false,
        );

        let result = ToolHandler::execute(
            &WriteToFileHandler::new(),
            &ctx,
            serde_json::json!({
                "path": ".gitignore",
                "content": "```\nnode_modules/\n*.log\n```\n"
            }),
        )
        .await
        .unwrap();
        assert!(result.as_str().unwrap().contains("Successfully wrote to"));

        let on_disk = std::fs::read_to_string(workspace_root.path().join(".gitignore")).unwrap();
        assert_eq!(on_disk, "node_modules/\n*.log\n");
        assert!(!on_disk.starts_with("```"));
    }

    #[tokio::test]
    async fn test_write_to_file_preserves_legitimate_code_block_in_readme() {
        let workspace_root = TempDir::new().unwrap();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "fence-preserve".into(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
            false,
        );

        let original = "```rust\ncargo install sned\n```\n";
        let result = ToolHandler::execute(
            &WriteToFileHandler::new(),
            &ctx,
            serde_json::json!({
                "path": "README.md",
                "content": original
            }),
        )
        .await
        .unwrap();
        assert!(result.as_str().unwrap().contains("Successfully wrote to"));

        let on_disk = std::fs::read_to_string(workspace_root.path().join("README.md")).unwrap();
        assert_eq!(on_disk, original);
    }

    #[test]
    fn strip_outer_fence_crlf_with_trailing_blank_lines() {
        let input = "```rust\r\nfn main() {}\r\n```\r\n\r\n";
        let stripped = strip_outer_markdown_fences(Path::new("main.rs"), input).into_owned();
        assert_eq!(stripped, "fn main() {}\r\n\r\n");
    }

    #[tokio::test]
    async fn test_write_to_file_does_not_double_strip_nested_fences() {
        let workspace_root = TempDir::new().unwrap();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "nested-strip".into(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
            false,
        );

        let input = "````markdown\n# Doc\n```rust\nfn inner() {}\n```\n````\n";
        let result = ToolHandler::execute(
            &WriteToFileHandler::new(),
            &ctx,
            serde_json::json!({
                "path": "guide.md",
                "content": input
            }),
        )
        .await
        .unwrap();
        assert!(result.as_str().unwrap().contains("Successfully wrote to"));

        let on_disk = std::fs::read_to_string(workspace_root.path().join("guide.md")).unwrap();
        assert_eq!(on_disk, "# Doc\n```rust\nfn inner() {}\n```\n");
    }

    /// Bug 3 (Audit): write_to_file must mirror edit_file's Phase 4b
    /// cleanup. After a successful write, the three read-tracking maps
    /// (consecutive_reads, last_read_turn, recent_read_windows) must be
    /// cleared for the written file, AND consecutive_edits must be
    /// incremented so a subsequent build failure can surface the
    /// thrashing diagnostic. Without this, full-rewrites leave stale
    /// state pointing at pre-rewrite bytes.
    #[tokio::test]
    async fn test_write_to_file_phase_4b_clears_read_state_and_increments_consecutive_edits() {
        let workspace_root = TempDir::new().unwrap();
        let file_path = workspace_root.path().join("target.c");
        std::fs::write(&file_path, "int original(void) { return 0; }\n").unwrap();

        let handler = WriteToFileHandler::new();
        let state = std::sync::Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        // Seed stale read/edit state for this file.
        {
            let mut guard = state.lock().await;
            let key = crate::core::tools::canonical_path_key(&file_path);
            guard.consecutive_reads.insert(key.clone(), 5);
            guard.last_read_turn.insert(key.clone(), 3);
            let mut ring = std::collections::VecDeque::new();
            ring.push_back((1, 50));
            ring.push_back((60, 100));
            guard.recent_read_windows.insert(key.clone(), ring);
            // Pre-existing edits so we can verify increment (not just set).
            guard.consecutive_edits.insert(key, 2);
        }
        let anchor_mgr = AnchorStateManager::new();
        let ctx = ToolContext::new(
            std::sync::Arc::clone(&state),
            None,
            workspace_root.path().to_path_buf(),
            anchor_mgr,
            false,
            "write-phase4b-task".to_string(),
            None,
            false,
            std::sync::Arc::new(crate::cli::output::StderrOutputWriter),
            false,
        );
        let params = serde_json::json!({
            "path": file_path.to_str().unwrap(),
            "content": "int rewritten(void) { return 42; }\n"
        });
        let result = ToolHandler::execute(&handler, &ctx, params)
            .await
            .expect("write_to_file should succeed");
        assert!(result.as_str().unwrap().contains("Successfully wrote to"));

        // Phase 4b cleanup must have fired for the canonical key.
        let guard = state.lock().await;
        let key = crate::core::tools::canonical_path_key(&file_path);
        assert!(
            !guard.consecutive_reads.contains_key(&key),
            "consecutive_reads must be cleared after a successful write, got: {:?}",
            guard.consecutive_reads
        );
        assert!(
            !guard.last_read_turn.contains_key(&key),
            "last_read_turn must be cleared after a successful write"
        );
        assert!(
            !guard.recent_read_windows.contains_key(&key),
            "recent_read_windows must be cleared after a successful write"
        );
        // consecutive_edits must have been incremented from 2 to 3.
        assert_eq!(
            guard.consecutive_edits.get(&key).copied(),
            Some(3),
            "consecutive_edits must be incremented after a successful write, got: {:?}",
            guard.consecutive_edits
        );
    }
}
