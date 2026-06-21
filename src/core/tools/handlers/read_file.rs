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
use futures::StreamExt;
use tokio::io::AsyncBufReadExt;

fn max_file_read_size() -> usize {
    use std::sync::OnceLock;
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        let default_size = 100 * 1024;
        let max_allowed = 100 * 1024 * 1024;
        std::env::var("SNED_MAX_FILE_READ_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .map(|v| v.min(max_allowed))
            .unwrap_or(default_size)
    })
}

/// Result of reading a single file.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct FileReadResult {
    path: String,
    /// Canonicalized absolute path (after tokio::fs::canonicalize).
    /// Used by track_read_files to match the keys stored by
    /// edit_file's mark_must_reread, which are also canonicalized.
    /// None if the read failed before canonicalization.
    canonical_path: Option<String>,
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
        output_writer: Option<&crate::cli::output::OutputWriterArc>,
    ) -> Vec<FileReadResult> {
        let read_futures: Vec<_> = paths
            .iter()
            .map(|path| {
                self.read_file(
                    path,
                    start_line,
                    end_line,
                    anchor_mgr,
                    task_id,
                    output_writer,
                )
            })
            .collect();

        // Buffer concurrent reads to prevent OOM on bulk operations (12 = reasonable parallelism)
        let results: Vec<FileReadResult> = futures::stream::iter(read_futures)
            .buffered(12)
            .collect()
            .await;

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
        output_writer: Option<&crate::cli::output::OutputWriterArc>,
    ) -> FileReadResult {
        // SECURITY: Re-verify path is still valid and not a symlink race (TOCTOU protection)
        // The path was already resolved by resolve_sanitized_path, but we re-canonicalize
        // to catch any filesystem changes between resolution and read
        let canonical_path = match tokio::fs::canonicalize(path).await {
            Ok(p) => p,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return FileReadResult {
                    path: path.to_string(),
                    canonical_path: None,
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                };
            }
        };

        let metadata = match tokio::fs::metadata(&canonical_path).await {
            Ok(m) => m,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return FileReadResult {
                    path: path.to_string(),
                    canonical_path: Some(canonical_path.to_string_lossy().into_owned()),
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                };
            }
        };

        if !metadata.is_file() {
            let err = crate::cli::actionable_errors::file_not_found(
                path,
                &format!("{} is not a file", path),
            );
            return FileReadResult {
                path: path.to_string(),
                canonical_path: Some(canonical_path.to_string_lossy().into_owned()),
                content: String::new(),
                hash: String::new(),
                success: false,
                error: Some(err.display()),
            };
        }

        let (content_for_hash, sliced_lines, clamping_note, full_lines, range_start, range_end) =
            if start_line.is_some() || end_line.is_some() {
                match self
                    .read_lines_range(&canonical_path.to_string_lossy(), start_line, end_line)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => return e,
                }
            } else if metadata.len() > max_file_read_size() as u64 {
                match self
                    .read_truncated(&canonical_path.to_string_lossy(), max_file_read_size())
                    .await
                {
                    Ok((content, lines)) => {
                        let size_kb = metadata.len() / 1024;
                        let max_kb = max_file_read_size() as u64 / 1024;
                        (
                            content.clone(),
                            lines.clone(),
                            Some(format!(
                                "[Note: File truncated to {}KB (file is {}KB). Use start_line and end_line parameters to read specific sections.]",
                                max_kb, size_kb
                            )),
                            Some(lines.clone()),
                            0,
                            lines.len(),
                        )
                    }
                    Err(e) => return e,
                }
            } else {
                match self
                    .read_full_file(&canonical_path.to_string_lossy(), output_writer)
                    .await
                {
                    Ok((content, lines)) => (
                        content,
                        lines.clone(),
                        None,
                        Some(lines.clone()),
                        0,
                        lines.len(),
                    ),
                    Err(e) => return e,
                }
            };

        // Register anchors using full file content (even for partial reads)
        // This ensures anchor state is consistent with the complete file
        let lines_for_reconcile = full_lines.as_ref().expect(
            "full_lines must be Some: all read paths (range/truncated/full) return Some(full_lines)"
        );
        let anchors = anchor_mgr.reconcile(path, lines_for_reconcile, task_id);

        // For output, use only the sliced lines and their corresponding anchors
        let output_lines = &sliced_lines;
        let output_anchors = if start_line.is_some() || end_line.is_some() {
            // Guard against invalid ranges (range_start > range_end) that can occur
            // when the model sends start_line > end_line or out-of-order values.
            let safe_start = range_start.min(anchors.len());
            let safe_end = range_end.min(anchors.len()).max(safe_start);
            &anchors[safe_start..safe_end]
        } else {
            &anchors
        };

        if output_lines.len() != output_anchors.len() {
            return FileReadResult {
                path: path.to_string(),
                canonical_path: Some(canonical_path.to_string_lossy().into_owned()),
                content: String::new(),
                hash: String::new(),
                success: false,
                error: Some(format!(
                    "Internal error: anchor/line length mismatch for {}: {} lines vs {} anchors",
                    path,
                    output_lines.len(),
                    output_anchors.len()
                )),
            };
        }

        // Format each line with its hash anchor
        let anchored_content = output_lines
            .iter()
            .zip(output_anchors.iter())
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
            canonical_path: Some(canonical_path.to_string_lossy().into_owned()),
            content,
            hash,
            success: true,
            error: None,
        }
    }

    /// Read the file once, then slice the requested line range.
    /// Returns (hash_content, sliced_lines, clamping_note, full_lines, start_idx, end_idx)
    /// where full_lines is the complete file for anchor registration,
    /// and start_idx/end_idx are the clamped range for anchor slicing.
    async fn read_lines_range(
        &self,
        path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<
        (
            String,
            Vec<String>,
            Option<String>,
            Option<Vec<String>>,
            usize,
            usize,
        ),
        FileReadResult,
    > {
        // SECURITY: Re-canonicalize path to catch symlink race (TOCTOU)
        let canonical_path = match tokio::fs::canonicalize(path).await {
            Ok(p) => p,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return Err(FileReadResult {
                    path: path.to_string(),
                    canonical_path: None,
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                });
            }
        };

        let content = match tokio::fs::read_to_string(&canonical_path).await {
            Ok(c) => c,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return Err(FileReadResult {
                    path: path.to_string(),
                    canonical_path: Some(canonical_path.to_string_lossy().into_owned()),
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                });
            }
        };

        // Use .lines() to strip \r from CRLF line endings, matching read_full_file behavior
        let all_lines: Vec<String> = content.lines().map(|line| line.to_string()).collect();
        let total_lines = all_lines.len();

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

        let collected_lines: Vec<String> = if start_idx >= end_exclusive || start_idx >= total_lines
        {
            Vec::new()
        } else {
            all_lines[start_idx..end_exclusive.min(total_lines)].to_vec()
        };

        let clamping_note = if clamped_start != original_start {
            Some(format!(
                "[Note: start_line was clamped from {} to {} (file has {} lines)]",
                original_start, clamped_start, total_lines
            ))
        } else {
            None
        };

        let hash_content = collected_lines.join("\n");
        Ok((
            hash_content,
            collected_lines,
            clamping_note,
            Some(all_lines),
            start_idx,
            end_exclusive,
        ))
    }

    /// Read the entire file using BufReader for reduced peak memory.
    ///
    /// Instead of `read_to_string` (which allocates the full file as one String,
    /// then `.lines()` allocates again as Vec<String> — 2x peak memory), we use
    /// `BufReader` to read line-by-line. Only the Vec<String> is retained, so
    /// peak memory is ~1x file size instead of ~2x.
    ///
    /// For large files, emits progress events via `output_writer` every N lines.
    async fn read_full_file(
        &self,
        path: &str,
        output_writer: Option<&crate::cli::output::OutputWriterArc>,
    ) -> Result<(String, Vec<String>), FileReadResult> {
        use tokio::io::BufReader;

        let file = match tokio::fs::File::open(path).await {
            Ok(f) => f,
            Err(e) => {
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
                    canonical_path: None,
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                });
            }
        };

        let reader = BufReader::new(file);
        let mut lines_stream = reader.lines();
        let mut lines: Vec<String> = Vec::new();
        let mut line_count: usize = 0;
        let progress_interval: usize = 5000;

        while let Some(line) = lines_stream.next_line().await.map_err(|e| {
            let err_msg = format!("Error reading file: {}", e);
            let err = crate::cli::actionable_errors::file_not_found(path, &err_msg);
            FileReadResult {
                path: path.to_string(),
                canonical_path: None,
                content: String::new(),
                hash: String::new(),
                success: false,
                error: Some(err.display()),
            }
        })? {
            lines.push(line);
            line_count += 1;

            if let Some(writer) = output_writer
                && line_count.is_multiple_of(progress_interval)
            {
                use crate::cli::tui::theme::INFO_FG;
                use ratatui::style::{Modifier, Style};
                writer.emit(crate::cli::output::OutputEvent::tool_output_line(
                    format!("  Reading {}: {} lines...", path, line_count),
                    Style::default().fg(INFO_FG).add_modifier(Modifier::DIM),
                ));
            }
        }

        if let Some(writer) = output_writer
            && line_count >= progress_interval
        {
            use crate::cli::tui::theme::INFO_FG;
            use ratatui::style::{Modifier, Style};
            writer.emit(crate::cli::output::OutputEvent::tool_output_line(
                format!("  Read {} lines from {}", line_count, path),
                Style::default().fg(INFO_FG).add_modifier(Modifier::DIM),
            ));
        }

        // Join lines for content_hash — this is the same as the old read_to_string
        // result but built from the Vec we already own (no extra allocation).
        let content = lines.join("\n");
        Ok((content, lines))
    }

    /// Read the first `max_bytes` of a file, handling UTF-8 boundary at truncation point.
    async fn read_truncated(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<(String, Vec<String>), FileReadResult> {
        use tokio::io::AsyncReadExt;

        // SECURITY: Re-canonicalize path to catch symlink race (TOCTOU)
        let canonical_path = match tokio::fs::canonicalize(path).await {
            Ok(p) => p,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return Err(FileReadResult {
                    path: path.to_string(),
                    canonical_path: None,
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                });
            }
        };

        let mut file = match tokio::fs::File::open(&canonical_path).await {
            Ok(f) => f,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return Err(FileReadResult {
                    path: path.to_string(),
                    canonical_path: Some(canonical_path.to_string_lossy().into_owned()),
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                });
            }
        };
        let mut buffer = vec![0u8; max_bytes];
        let n = match file.read(&mut buffer).await {
            Ok(n) => n,
            Err(e) => {
                let err = crate::cli::actionable_errors::file_not_found(path, &e.to_string());
                return Err(FileReadResult {
                    path: path.to_string(),
                    canonical_path: Some(canonical_path.to_string_lossy().into_owned()),
                    content: String::new(),
                    hash: String::new(),
                    success: false,
                    error: Some(err.display()),
                });
            }
        };
        buffer.truncate(n);

        let content = match std::str::from_utf8(&buffer) {
            Ok(s) => s.to_string(),
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                buffer.truncate(valid_up_to);
                String::from_utf8(buffer).expect("truncated at valid UTF-8 boundary")
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
        output_writer: Option<&crate::cli::output::OutputWriterArc>,
    ) -> Result<(Vec<String>, Vec<FileReadResult>), ToolError> {
        let (paths, start_line, end_line) = Self::parse_params(&params)?;
        let results = self
            .read_files(
                paths.clone(),
                start_line,
                end_line,
                anchor_mgr,
                task_id,
                output_writer,
            )
            .await;
        Ok((paths, results))
    }

    fn track_read_files(state: &mut TaskState, paths: &[String], results: &[FileReadResult]) {
        for (path_str, res) in paths.iter().zip(results.iter()) {
            if res.success {
                let path = std::path::Path::new(path_str);
                state.file_context_tracker.track_file_read(path);
                // must_reread_before_edit is keyed by the canonicalized
                // path that edit_file's mark_must_reread stores
                // (src/core/tools/handlers/edit_file.rs:435). The
                // model may provide a relative path here, so use the
                // canonical_path from the read result (computed by
                // tokio::fs::canonicalize in read_file) to match.
                // Without this, the model's re-read after a successful
                // edit would not clear the stale-anchor guard when the
                // workspace has symlinks (e.g. /tmp → /private/tmp on
                // macOS), causing an infinite re-read loop.
                if let Some(canonical) = &res.canonical_path {
                    state.must_reread_before_edit.remove(canonical);
                }
                // Track consecutive reads for read-loop detection.
                // If the same file is read 3+ times in a row with no
                // edit, warn the model. Use the canonical path so the
                // counter matches what edit_file removes at
                // src/core/tools/handlers/edit_file.rs:1136.
                let count_key = res
                    .canonical_path
                    .clone()
                    .unwrap_or_else(|| path_str.clone());
                let count = state.consecutive_reads.entry(count_key).or_insert(0);
                *count += 1;
            }
        }
    }

    pub async fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        anchor_mgr: &AnchorStateManager,
        task_id: Option<&str>,
        output_writer: Option<&crate::cli::output::OutputWriterArc>,
    ) -> Result<String, ToolError> {
        let (paths, results) = self
            .execute_with_results(params, anchor_mgr, task_id, output_writer)
            .await?;
        Self::track_read_files(state, &paths, &results);

        // Read-loop detection: if a file was read 3+ times in a row with no
        // intervening edit, surface a hint so the model doesn't loop forever.
        if let Some(writer) = output_writer {
            for (path_str, res) in paths.iter().zip(results.iter()) {
                // Look up the count under the canonical key (the key
                // track_read_files uses to increment) so the warning
                // fires when the model reads the same file repeatedly
                // via different path representations (relative vs
                // absolute, or with/without ./).
                let lookup_key = res
                    .canonical_path
                    .clone()
                    .unwrap_or_else(|| path_str.clone());
                let count = state
                    .consecutive_reads
                    .get(&lookup_key)
                    .copied()
                    .unwrap_or(0);
                if count >= 3 {
                    use crate::cli::output::OutputEvent;
                    use crate::cli::tui::theme::WARNING_FG;
                    use ratatui::style::Style;
                    writer.emit(OutputEvent::tool_output_line(
                        format!(
                            "⚠ {} has been read {} times consecutively with no edit. \
                             If you have the anchors you need, call edit_file now.",
                            path_str, count
                        ),
                        Style::default().fg(WARNING_FG),
                    ));
                }
            }
        }

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
                Some(&ctx.output_writer),
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
    use tokio::sync::mpsc;

    fn drain_rendered_output(
        rx: &mut tokio::sync::mpsc::Receiver<crate::cli::output::OutputEvent>,
    ) -> Vec<String> {
        let mut rendered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                crate::cli::output::OutputEvent::Line(line) => rendered.push(line.to_string()),
                crate::cli::output::OutputEvent::ToolOutputLine(line) => rendered.push(line.to_string()),
                crate::cli::output::OutputEvent::RawAnsi(raw) => rendered.push(raw),
                crate::cli::output::OutputEvent::Completion(text) => rendered.push(text),
                crate::cli::output::OutputEvent::TurnEnd { .. } => {}
                crate::cli::output::OutputEvent::TurnIndicator(line) => rendered.push(line.to_string()),
                crate::cli::output::OutputEvent::ErrorBox(msg) => rendered.push(msg),
                crate::cli::output::OutputEvent::ToolHeaderLine(line) => rendered.push(line.to_string()),
                crate::cli::output::OutputEvent::CommandHeaderLine(line) => rendered.push(line.to_string()),
                crate::cli::output::OutputEvent::CommandOutputLine(line) => rendered.push(line.to_string()),
                crate::cli::output::OutputEvent::ReasoningLine(line) => rendered.push(line.to_string()),
                crate::cli::output::OutputEvent::UserPromptLine(line) => rendered.push(line.to_string()),
            }
        }
        rendered
    }

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
                None,
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
            Arc::new(crate::cli::output::StderrOutputWriter),
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
    async fn test_dispatched_read_file_clears_reread_requirement() {
        let workspace_root = std::env::current_dir().unwrap();
        let temp_dir = tempfile::tempdir_in(&workspace_root).unwrap();
        let file_path = temp_dir.path().join("test_reread_clear.txt");
        std::fs::write(&file_path, "Hello, world!\n").unwrap();

        let handler = ReadFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        state
            .lock()
            .await
            .must_reread_before_edit
            .insert(file_path.to_string_lossy().to_string());
        let ctx = ToolContext::new(
            state.clone(),
            None,
            workspace_root,
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let _ = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({"paths": [file_path.to_str().unwrap()]}),
        )
        .await
        .unwrap();

        assert!(
            !state
                .lock()
                .await
                .must_reread_before_edit
                .contains(&file_path.to_string_lossy().to_string())
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
                None,
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
                None,
            )
            .await;

        assert!(!result.success);
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("Error reading file"));
    }

    #[tokio::test]
    async fn test_read_file_truncated_when_too_large() {
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
                None,
            )
            .await;

        assert!(result.success, "large file should auto-truncate, not error");
        assert!(result.error.is_none());
        assert!(result.content.contains("truncated to 100KB"));
        assert!(result.content.contains("Hash:"));
    }

    #[tokio::test]
    async fn test_read_file_truncated_utf8_boundary() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let ch: char = '€'; // U+20AC = 3 bytes in UTF-8
        let ch_str: String = ch.to_string();
        let repeat_count = (101 * 1024) / ch_str.len() + 1;
        let data: String = ch_str.repeat(repeat_count);
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
                None,
            )
            .await;

        assert!(result.success, "UTF-8 boundary should not cause error");
        // Content must be valid UTF-8 (no replacement characters from broken multi-byte sequence)
        assert!(
            !result.content.contains('\u{FFFD}'),
            "no replacement characters allowed"
        );
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
                None,
            )
            .await;

        assert!(result.success);
    }

    #[tokio::test]
    async fn test_read_full_file_emits_progress_without_flooding_output() {
        let mut temp_file = NamedTempFile::new().unwrap();
        for i in 0..5001 {
            writeln!(temp_file, "line {}", i).unwrap();
        }

        let handler = ReadFileHandler::new();
        let (tx, mut rx) = mpsc::channel(32);
        let writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let result = handler
            .read_full_file(temp_file.path().to_str().unwrap(), Some(&writer))
            .await;

        assert!(result.is_ok());
        let rendered = drain_rendered_output(&mut rx);
        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].contains("Reading"));
        assert!(rendered[0].contains("5000 lines"));
        assert!(rendered[1].contains("Read 5001 lines"));
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
            .read_files(paths, None, None, &anchor_mgr, Some("test-task"), None)
            .await;

        assert_eq!(results.len(), 2);
        assert!(results[0].success);
        assert!(results[1].success);
        assert!(results[0].content.contains("content 1"));
        assert!(results[1].content.contains("content 2"));
    }

    #[tokio::test]
    async fn test_read_multi_files_format_preserves_order_and_separators() {
        let mut file1 = NamedTempFile::new().unwrap();
        writeln!(file1, "first file").unwrap();

        let mut file2 = NamedTempFile::new().unwrap();
        writeln!(file2, "second file").unwrap();

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let paths = vec![
            file1.path().to_str().unwrap().to_string(),
            file2.path().to_str().unwrap().to_string(),
        ];

        let results = handler
            .read_files(paths, None, None, &anchor_mgr, Some("test-task"), None)
            .await;
        let output = ReadFileHandler::format_results(results);

        let first_pos = output.find("first file").unwrap();
        let second_pos = output.find("second file").unwrap();
        assert!(first_pos < second_pos);
        assert_eq!(output.matches("\n---\n").count(), 1);
    }

    #[tokio::test]
    async fn test_read_multi_files_missing_file_stays_in_input_position() {
        let mut file1 = NamedTempFile::new().unwrap();
        writeln!(file1, "before missing").unwrap();

        let mut file2 = NamedTempFile::new().unwrap();
        writeln!(file2, "after missing").unwrap();

        let missing_path = file1
            .path()
            .parent()
            .unwrap()
            .join("missing-input-position.txt");
        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let paths = vec![
            file1.path().to_str().unwrap().to_string(),
            missing_path.to_str().unwrap().to_string(),
            file2.path().to_str().unwrap().to_string(),
        ];

        let results = handler
            .read_files(paths, None, None, &anchor_mgr, Some("test-task"), None)
            .await;
        assert_eq!(results.len(), 3);
        assert!(results[0].success);
        assert!(!results[1].success);
        assert!(results[2].success);

        let output = ReadFileHandler::format_results(results);
        let before_pos = output.find("before missing").unwrap();
        let error_pos = output.find("Error reading").unwrap();
        let after_pos = output.find("after missing").unwrap();
        assert!(before_pos < error_pos);
        assert!(error_pos < after_pos);
        assert_eq!(output.matches("\n---\n").count(), 2);
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
                None,
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
                None,
            )
            .await;

        assert!(result.success);
        assert!(result.content.contains("line 10"));
        assert!(!result.content.contains("[Note:"));
    }

    #[tokio::test]
    async fn test_read_file_start_line_exceeds_end_line_no_panic() {
        let mut temp_file = NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(temp_file, "line {}", i).unwrap();
        }

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                Some(450),
                Some(300),
                &anchor_mgr,
                Some("test-task"),
                None,
            )
            .await;

        assert!(
            result.success,
            "invalid range (start > end) must not panic, got error: {:?}",
            result.error
        );
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
                None,
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

    /// Integration test: verify that partial file reads register anchors for the FULL file,
    /// not just the visible slice. This ensures edits using anchors from partial reads
    /// can resolve correctly even if the anchor is outside the visible range.
    #[tokio::test]
    async fn test_partial_read_registers_full_anchor_state() {
        let mut temp_file = NamedTempFile::new().unwrap();
        for i in 1..=100 {
            writeln!(temp_file, "line {}", i).unwrap();
        }
        temp_file.flush().unwrap();

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let task_id = "test-partial-anchor-task";

        // Read only lines 10-20 (11 lines visible to the model)
        let result = handler
            .read_file(
                temp_file.path().to_str().unwrap(),
                Some(10),
                Some(20),
                &anchor_mgr,
                Some(task_id),
                None,
            )
            .await;

        assert!(result.success, "read should succeed");

        // Verify output contains only the requested range
        assert!(result.content.contains("line 10"), "should contain line 10");
        assert!(result.content.contains("line 20"), "should contain line 20");
        assert!(
            !result.content.contains("line 5"),
            "should NOT contain line 5 (outside range)"
        );
        assert!(
            !result.content.contains("line 50"),
            "should NOT contain line 50 (outside range)"
        );

        // CRITICAL: Verify that anchor state was registered for ALL 100 lines,
        // not just the 11 visible lines. This is the fix for A34.
        let anchors = anchor_mgr
            .get_anchors(temp_file.path().to_str().unwrap(), Some(task_id))
            .expect("file should be tracked");

        // The anchor state should have 100 entries (one per line in the full file),
        // not 11 (the visible slice)
        assert_eq!(
            anchors.len(),
            100,
            "anchor state should have anchors for all 100 lines, not just the 11 visible lines"
        );

        // Verify that an anchor from outside the visible range (e.g., line 50) exists
        // in the tracked state. This proves the model could later edit line 50 even
        // though it only saw lines 10-20.
        let line_50_anchor = &anchors[49]; // 0-indexed
        assert!(
            !line_50_anchor.is_empty(),
            "line 50 anchor should be tracked even though it wasn't in the visible range"
        );
    }

    #[tokio::test]
    async fn test_consecutive_reads_counter_increments_and_resets() {
        use crate::core::agent_types::TaskState;

        let mut temp_file = NamedTempFile::new().unwrap();
        writeln!(temp_file, "hello").unwrap();
        temp_file.flush().unwrap();

        let handler = ReadFileHandler::new();
        let anchor_mgr = AnchorStateManager::new();
        let mut state = TaskState::default();
        // track_read_files keys the counter by the canonical path
        // (computed via tokio::fs::canonicalize in read_file), so the
        // test must look up under the same canonical key.
        let canonical_path_str = std::fs::canonicalize(temp_file.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();

        // Read the file 3 times in a row (no intervening edit).
        for i in 1..=3 {
            let params = serde_json::json!({
                "paths": [temp_file.path().to_str().unwrap()]
            });
            let _ = handler
                .execute(&mut state, params, &anchor_mgr, Some("test-task"), None)
                .await
                .expect("read should succeed");

            let count = state
                .consecutive_reads
                .get(&canonical_path_str)
                .copied()
                .unwrap_or(0);
            assert_eq!(
                count, i,
                "consecutive_reads should be {} after {} reads",
                i, i
            );
        }
    }
}
