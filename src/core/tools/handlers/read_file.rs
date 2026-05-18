//! Read file tool handler for sned CLI.
//!
//!
//! Core behavior:
//! - Read single or multiple files
//! - Support line ranges (start_line, end_line)
//! - Enforce 100KB size limit for full file reads
//! - Calculate FNV-1a content hash
//! - Return file content with hash-anchored lines for edit compatibility
//! - Handle errors gracefully

use crate::core::agent_loop::TaskState;
use crate::core::file_editor::AnchorStateManager;
use crate::core::hash_utils::{content_hash, format_line_with_hash};
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};

const MAX_FILE_READ_SIZE: usize = 100 * 1024; // 100KB limit

/// Result of reading a single file.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct FileReadResult {
    path: String,
    content: String,
    hash: String,
    success: bool,
    error: Option<String>,
}

/// Read file tool handler.
#[derive(Debug, Clone, Default)]
pub struct ReadFileHandler;

impl ReadFileHandler {
    pub fn new() -> Self {
        Self
    }

    /// Read one or more files.
    ///
    async fn read_files(
        &self,
        paths: Vec<String>,
        start_line: Option<usize>,
        end_line: Option<usize>,
        anchor_mgr: &AnchorStateManager,
        task_id: Option<&str>,
    ) -> Vec<FileReadResult> {
        let mut results = Vec::with_capacity(paths.len());

        for path in paths {
            let result = self
                .read_file(&path, start_line, end_line, anchor_mgr, task_id)
                .await;
            results.push(result);
        }

        results
    }

    /// Read a single file with optional line range.
    ///
    /// Returns the file content with hash-anchored lines if successful,
    /// or an error message if the file cannot be read.
    async fn read_file(
        &self,
        path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
        anchor_mgr: &AnchorStateManager,
        task_id: Option<&str>,
    ) -> FileReadResult {
        // Check if file exists and get metadata
        let metadata = match tokio::fs::metadata(path).await {
            Ok(m) => m,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return FileReadResult {
                    path: path.to_string(),
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                };
            }
        };

        // Check if it's a file (not a directory)
        if !metadata.is_file() {
            let err = crate::cli::actionable_errors::file_not_found(
                path,
                &format!("{} is not a file", path),
            );
            return FileReadResult {
                path: path.to_string(),
                content: String::new(),
                hash: String::new(),
                success: false,
                error: Some(err.display()),
            };
        }

        // Check if it's a regular file (not a pipe, socket, device, etc.)
        if !metadata.is_file() {
            let err = crate::cli::actionable_errors::file_not_found(
                path,
                &format!(
                    "{} is not a regular file (type: {:?})",
                    path,
                    metadata.file_type()
                ),
            );
            return FileReadResult {
                path: path.to_string(),
                content: String::new(),
                hash: String::new(),
                success: false,
                error: Some(err.display()),
            };
        }

        // Check file size limit for full reads
        let is_full_read = start_line.is_none() && end_line.is_none();
        if is_full_read && metadata.len() > MAX_FILE_READ_SIZE as u64 {
            let size_kb = metadata.len() / 1024;
            let max_kb = MAX_FILE_READ_SIZE as u64 / 1024;
            let err = crate::cli::actionable_errors::file_too_large(size_kb, max_kb);
            return FileReadResult {
                path: path.to_string(),
                content: String::new(),
                hash: String::new(),
                success: false,
                error: Some(err.display()),
            };
        }

        let (content_for_hash, lines, clamping_note) = if start_line.is_some() || end_line.is_some()
        {
            match self.read_lines_range(path, start_line, end_line).await {
                Ok(v) => v,
                Err(e) => return e,
            }
        } else {
            match self.read_full_file(path).await {
                Ok((content, lines)) => (content, lines, None),
                Err(e) => return e,
            }
        };

        // Generate per-line anchors for edit compatibility
        let anchors = anchor_mgr.reconcile(path, &lines, task_id);

        if lines.len() != anchors.len() {
            return FileReadResult {
                path: path.to_string(),
                content: String::new(),
                hash: String::new(),
                success: false,
                error: Some(format!(
                    "Internal error: anchor/line length mismatch for {}: {} lines vs {} anchors",
                    path,
                    lines.len(),
                    anchors.len()
                )),
            };
        }

        // Format each line with its hash anchor
        let anchored_content = lines
            .iter()
            .zip(anchors.iter())
            .map(|(line, anchor)| format_line_with_hash(line, anchor))
            .collect::<Vec<_>>()
            .join("\n");

        // Calculate file-level hash
        let hash = content_hash(&content_for_hash);

        let mut content = format!("[File: {}, Hash: {}]\n{}", path, hash, anchored_content);
        if let Some(note) = clamping_note {
            content = format!("{}\n{}", note, content);
        }

        FileReadResult {
            path: path.to_string(),
            content,
            hash,
            success: true,
            error: None,
        }
    }

    /// Stream-read only the requested line range using BufReader.
    ///
    /// Avoids loading the entire file into memory when only a subset is needed.
    /// Skips lines before start_line without allocating Strings for them.
    async fn read_lines_range(
        &self,
        path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<(String, Vec<String>, Option<String>), FileReadResult> {
        let file = match tokio::fs::File::open(path).await {
            Ok(f) => f,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return Err(FileReadResult {
                    path: path.to_string(),
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                });
            }
        };
        let mut reader = BufReader::new(file);

        // First pass: count total lines for clamping logic
        let mut total_lines: usize = 0;
        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) => break,
                Ok(_) => total_lines += 1,
                Err(e) => {
                    let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                    return Err(FileReadResult {
                        path: path.to_string(),
                        content: String::new(),
                        hash: String::new(),
                        success: false,
                        error: Some(err.display()),
                    });
                }
            }
        }

        // Calculate the actual range (with clamping)
        let original_start = start_line.unwrap_or(1);
        let mut clamped_start = original_start;
        let mut clamped_end = end_line;

        if clamped_start > total_lines {
            clamped_start = total_lines.saturating_sub(50).max(1);
        }
        if let Some(ref mut e) = clamped_end
            && *e > total_lines
        {
            *e = total_lines;
        }

        let start_idx = clamped_start.saturating_sub(1);
        let end_exclusive = clamped_end.unwrap_or(total_lines);

        // Second pass: re-open file and read only the lines we need
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| FileReadResult {
                path: path.to_string(),
                content: String::new(),
                hash: String::new(),
                success: false,
                error: Some(format!("Failed to re-open file: {}", e)),
            })?;
        let mut reader = BufReader::new(file);

        let mut collected_lines: Vec<String> = Vec::new();
        let mut current_line: usize = 0;
        let mut buf = String::new();

        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    if current_line >= start_idx && current_line < end_exclusive {
                        let line_content = if buf.ends_with('\n') {
                            buf[..buf.len() - 1].to_string()
                        } else {
                            buf.clone()
                        };
                        collected_lines.push(line_content);
                    }
                    current_line += 1;
                    // Early exit if we've read past the end of our range
                    if current_line >= end_exclusive {
                        break;
                    }
                }
                Err(e) => {
                    let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                    return Err(FileReadResult {
                        path: path.to_string(),
                        content: String::new(),
                        hash: String::new(),
                        success: false,
                        error: Some(err.display()),
                    });
                }
            }
        }

        let clamping_note = if clamped_start != original_start {
            Some(format!(
                "[Note: start_line was clamped from {} to {} (file has {} lines)]",
                original_start, clamped_start, total_lines
            ))
        } else {
            None
        };

        let hash_content = collected_lines.join("\n");
        Ok((hash_content, collected_lines, clamping_note))
    }

    /// Read the entire file (current behavior for full reads).
    async fn read_full_file(&self, path: &str) -> Result<(String, Vec<String>), FileReadResult> {
        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => {
                // Handle binary files or encoding errors gracefully
                let err_msg = if e.kind() == std::io::ErrorKind::InvalidData {
                    format!(
                        "File appears to be binary or contains invalid UTF-8. \
                         Use a line range to read specific portions, or use a hex editor for binary files. \
                         Original error: {}",
                        e
                    )
                } else {
                    format!("Error reading file: {}", e)
                };
                let err = crate::cli::actionable_errors::file_not_found(path, &err_msg);
                return Err(FileReadResult {
                    path: path.to_string(),
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                });
            }
        };
        let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
        Ok((content, lines))
    }

    async fn execute_with_results(
        &self,
        params: serde_json::Value,
        anchor_mgr: &AnchorStateManager,
        task_id: Option<&str>,
    ) -> Result<(Vec<String>, Vec<FileReadResult>), ToolError> {
        let (paths, start_line, end_line) = Self::parse_params(&params)?;
        let results = self
            .read_files(paths.clone(), start_line, end_line, anchor_mgr, task_id)
            .await;
        Ok((paths, results))
    }

    fn track_read_files(state: &mut TaskState, paths: &[String], results: &[FileReadResult]) {
        for (path_str, res) in paths.iter().zip(results.iter()) {
            if res.success {
                let path = std::path::Path::new(path_str);
                state.file_context_tracker.track_file_read(path);
            }
        }
    }

    pub async fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        anchor_mgr: &AnchorStateManager,
        task_id: Option<&str>,
    ) -> Result<String, ToolError> {
        let (paths, results) = self
            .execute_with_results(params, anchor_mgr, task_id)
            .await?;
        Self::track_read_files(state, &paths, &results);
        Ok(Self::format_results(results))
    }

    fn parse_params(
        params: &serde_json::Value,
    ) -> Result<(Vec<String>, Option<usize>, Option<usize>), ToolError> {
        let paths = crate::core::tools::coerce_string_array(params, "paths", "path");
        if paths.is_empty() {
            return Err(ToolError::InvalidInput(
                "Missing 'paths' parameter".to_string(),
            ));
        }

        let start_line = params["start_line"].as_u64().map(|n| n as usize);
        let end_line = params["end_line"].as_u64().map(|n| n as usize);
        Ok((paths, start_line, end_line))
    }

    fn format_results(results: Vec<FileReadResult>) -> String {
        let mut output = String::new();
        for res in results {
            if !output.is_empty() {
                output.push_str("\n---\n");
            }
            if res.success {
                output.push_str(&res.content);
            } else {
                output.push_str(&format!(
                    "Error reading {}: {}",
                    res.path,
                    res.error.unwrap_or_default()
                ));
            }
        }

        output
    }
}

#[async_trait]
impl ToolHandler for ReadFileHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let (paths, start_line, end_line) = Self::parse_params(&params)?;

        let sanitized: Result<Vec<String>, ToolError> = paths
            .iter()
            .map(|p| {
                crate::core::tools::resolve_sanitized_path(&ctx.workspace_root, p)
                    .map(|pb| pb.to_string_lossy().to_string())
            })
            .collect();
        let paths = sanitized?;

        let results = self
            .read_files(
                paths.clone(),
                start_line,
                end_line,
                &ctx.anchor_mgr,
                Some(ctx.task_id.as_str()),
            )
            .await;
        {
            let mut state = ctx.state.lock().await;
            Self::track_read_files(&mut state, &paths, &results);
        }
        Ok(serde_json::Value::String(Self::format_results(results)))
    }

    fn description(&self, params: &serde_json::Value) -> String {
        if let Some(paths) = params["paths"].as_array() {
            format!("Reading {} files", paths.len())
        } else {
            "Reading files".to_string()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use crate::core::file_editor::AnchorStateManager;
    use crate::core::tools::{ToolContext, ToolHandler};
    use std::io::Write;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    #[test]
    fn test_content_hash_empty() {
        let hash = content_hash("");
        assert_eq!(hash.len(), 8);
        // FNV-1a of empty string is offset basis
        assert_eq!(hash, "811c9dc5");
    }

    #[test]
    fn test_content_hash_known() {
        let hash = content_hash("hello");
        assert_eq!(hash.len(), 8);
        // FNV-1a hash for "hello"
        assert_eq!(hash, "4f9f2cab");
    }

    #[test]
    fn test_content_hash_deterministic() {
        let h1 = content_hash("test content");
        let h2 = content_hash("test content");
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn test_read_file_success() {
        let mut temp_file = NamedTempFile::new().unwrap();
        writeln!(temp_file, "Hello, world!").unwrap();

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                None,
                None,
                &anchor_mgr,
                Some("test-task"),
            )
            .await;

        assert!(result.success);
        assert!(result.content.contains("Hello, world!"));
        assert!(result.content.contains("[File: "));
        assert_eq!(result.error, None);
    }

    #[tokio::test]
    #[ignore]
    async fn test_dispatched_read_file_tracks_stale_context() {
        let workspace_root = std::env::current_dir().unwrap();
        let temp_dir = tempfile::tempdir_in(&workspace_root).unwrap();
        let file_path = temp_dir.path().join("test_stale.txt");
        std::fs::write(&file_path, "Hello, world!\n").unwrap();

        let handler = ReadFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            workspace_root,
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
        );

        let _ = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({"paths": [file_path.to_str().unwrap()]}),
        )
        .await
        .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::write(&file_path, "Hello, modified world!\n").unwrap();

        let mut state = state.lock().await;
        let warning = state.file_context_tracker.check_stale(&file_path).await;
        assert!(
            warning.is_some(),
            "expected dispatched read_file to record the file for stale-context tracking"
        );
    }

    #[tokio::test]
    async fn test_read_file_line_range() {
        let mut temp_file = NamedTempFile::new().unwrap();
        writeln!(temp_file, "line 1").unwrap();
        writeln!(temp_file, "line 2").unwrap();
        writeln!(temp_file, "line 3").unwrap();
        writeln!(temp_file, "line 4").unwrap();

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                Some(2),
                Some(3),
                &anchor_mgr,
                Some("test-task"),
            )
            .await;

        assert!(result.success);
        assert!(result.content.contains("line 2"));
        assert!(result.content.contains("line 3"));
        assert!(!result.content.contains("line 1"));
        assert!(!result.content.contains("line 4"));
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                "/nonexistent/path/file.txt",
                None,
                None,
                &anchor_mgr,
                Some("test-task"),
            )
            .await;

        assert!(!result.success);
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("Error reading file"));
    }

    #[tokio::test]
    async fn test_read_file_too_large() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let data = "x".repeat(101 * 1024);
        temp_file.write_all(data.as_bytes()).unwrap();

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                None,
                None,
                &anchor_mgr,
                Some("test-task"),
            )
            .await;

        assert!(!result.success);
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("exceeds the 100KB limit"));
    }

    #[tokio::test]
    async fn test_read_file_large_with_line_range() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let data = "x".repeat(101 * 1024);
        temp_file.write_all(data.as_bytes()).unwrap();

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                Some(1),
                Some(10),
                &anchor_mgr,
                Some("test-task"),
            )
            .await;

        assert!(result.success);
    }

    #[tokio::test]
    async fn test_read_multi_files() {
        let mut file1 = NamedTempFile::new().unwrap();
        writeln!(file1, "content 1").unwrap();

        let mut file2 = NamedTempFile::new().unwrap();
        writeln!(file2, "content 2").unwrap();

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let paths = vec![
            file1.path().to_str().unwrap().to_string(),
            file2.path().to_str().unwrap().to_string(),
        ];
        let results = handler
            .read_files(paths, None, None, &anchor_mgr, Some("test-task"))
            .await;

        assert_eq!(results.len(), 2);
        assert!(results[0].success);
        assert!(results[1].success);
        assert!(results[0].content.contains("content 1"));
        assert!(results[1].content.contains("content 2"));
    }

    #[tokio::test]
    async fn test_read_file_start_line_exceeds_length_clamps() {
        let mut temp_file = NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(temp_file, "line {}", i).unwrap();
        }

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                Some(999),
                None,
                &anchor_mgr,
                Some("test-task"),
            )
            .await;

        assert!(
            result.success,
            "expected success, got error: {:?}",
            result.error
        );
        assert!(
            result
                .content
                .contains("[Note: start_line was clamped from 999 to 1 (file has 10 lines)]")
        );
    }

    #[tokio::test]
    async fn test_read_file_end_line_exceeds_length_clamped() {
        let mut temp_file = NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(temp_file, "line {}", i).unwrap();
        }

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                Some(1),
                Some(999),
                &anchor_mgr,
                Some("test-task"),
            )
            .await;

        assert!(result.success);
        assert!(result.content.contains("line 10"));
        assert!(!result.content.contains("[Note:"));
    }

    #[tokio::test]
    async fn test_read_file_start_line_clamped_shows_last_50() {
        let mut temp_file = NamedTempFile::new().unwrap();
        for i in 1..=100 {
            writeln!(temp_file, "line {}", i).unwrap();
        }

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                Some(200),
                None,
                &anchor_mgr,
                Some("test-task"),
            )
            .await;

        assert!(result.success);
        assert!(result.content.contains("line 100"));
        assert!(result.content.contains("line 50"));
        assert!(!result.content.contains("line 49"));
        assert!(
            result
                .content
                .contains("[Note: start_line was clamped from 200 to 50 (file has 100 lines)]")
        );
    }
}
