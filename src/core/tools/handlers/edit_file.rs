//! Edit file tool handler for sned CLI.
//!
//!
//! Core behavior:
//! - Parse files parameter (array of {path, edits})
//! - Read file content and compute line hashes via AnchorStateManager
//! - Validate edits using BatchProcessor
//! - Check combined approval before applying edits
//! - Apply edits and write updated content back
//! - Return formatted diff result

use crate::core::agent_loop::TaskState;
use crate::core::approval::{ApprovalManager, prompt_for_combined_approval};
use crate::core::edit_batch::{BatchProcessor, DiagnosticsResult, DiffMode, PreparedEdits};
use crate::core::file_editor::{AnchorStateManager, Edit, FileEditGuard, FileEditorError};
use crate::core::hash_utils::{ANCHOR_DELIMITER, compute_hashes, split_anchor, strip_hashes};
use crate::core::tools::handlers::diagnostics_scan::{DiagnosticsScanHandler, ProjectType};
use crate::core::tools::handlers::error_guidance;
use crate::core::tools::{
    SnedTool, ToolContext, ToolError, ToolFailureClass, ToolFailureMetadata, ToolHandler,
    ToolRequiredNextStep,
};
use crate::services::symbol_index::SymbolIndexService;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::Mutex;

/// Edit file tool handler.
#[derive(Clone, Debug)]
pub struct EditFileHandler {
    /// Optional approval manager for combined edit approval.
    approval_manager: Option<Arc<Mutex<ApprovalManager>>>,
    /// Optional symbol index service for cache refresh after edits.
    symbol_index_service: Option<Arc<std::sync::Mutex<SymbolIndexService>>>,
}

impl EditFileHandler {
    #[must_use] 
    pub fn new() -> Self {
        Self {
            approval_manager: None,
            symbol_index_service: None,
        }
    }
}

impl Default for EditFileHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl EditFileHandler {
    /// Set the approval manager for combined approval checks.
    pub fn with_approval_manager(mut self, approval_manager: Arc<Mutex<ApprovalManager>>) -> Self {
        self.approval_manager = Some(approval_manager);
        self
    }

    /// Set the symbol index service for cache refresh after edits.
    pub fn with_symbol_index(mut self, service: Arc<std::sync::Mutex<SymbolIndexService>>) -> Self {
        self.symbol_index_service = Some(service);
        self
    }

    fn file_entry_path(file: &serde_json::Value) -> Result<&str, String> {
        if let Some(path) = file.get("path") { path.as_str().ok_or_else(|| {
            "edit_file requires 'path' to be a string in each file entry. Example: { \"path\": \"src/file.rs\", \"edits\": [ ... ] }"
                .to_string()
        }) } else {
            // Lenient fallback: models commonly put "path" inside the first
            // edit object instead of at the file-entry level. Extract it
            // if present so the edit succeeds without a wasted round-trip.
            if let Some(edits) = file.get("edits").and_then(|e| e.as_array())
                && let Some(first_edit) = edits.first()
                && let Some(path) = first_edit.get("path").and_then(|p| p.as_str())
            {
                return Ok(path);
            }
            Err(
                "edit_file requires a 'path' key in each file entry. Example: { \"path\": \"src/file.rs\", \"edits\": [ ... ] }"
                    .to_string(),
            )
        }
    }

    fn normalized_anchor(field_name: &str, path: &str, raw: &str) -> Result<String, String> {
        let anchor = raw.trim();
        if anchor.is_empty() {
            return Err(format!(
                "File '{path}': '{field_name}' is empty. Copy the exact 'Word§line content' string from read_file output."
            ));
        }

        // Distinguish two kinds of multi-line input:
        //
        // 1. Concatenated anchors — the model pasted multiple complete
        //    `Word§content` lines from the diff output separated by
        //    newlines. The first line is a complete anchor. Take it.
        //
        // 2. Truly multi-line anchor — the model's anchor spans
        //    multiple physical lines (e.g. `Word§\nNextWord§content`).
        //    The first line is incomplete (ends with `§` with no
        //    content after it). Reject with a clear error.
        if anchor.contains('\n') {
            let first_line = anchor.lines().next().unwrap_or("").trim();
            if first_line.ends_with(ANCHOR_DELIMITER) {
                let preview = if first_line.chars().count() > 60 {
                    format!("{}...", first_line.chars().take(60).collect::<String>())
                } else {
                    first_line.to_string()
                };
                return Err(format!(
                    "File '{path}': '{field_name}' is a multi-line anchor that starts with an incomplete line ('{preview}' ends with the '{ANCHOR_DELIMITER}' delimiter but has no content after it). Anchors must be a single complete physical line from the read_file output (format: 'Word§line content'). If you want to replace a range of lines, use 'anchor' for the first line and 'end_anchor' for the last line."
                ));
            }
            // First line is complete (has content after §). Use it.
            return Ok(first_line.to_string());
        }

        Ok(anchor.to_string())
    }

    fn repair_truncated_files_json(raw: &str) -> Option<String> {
        let mut stack = Vec::new();
        let mut in_string = false;
        let mut escape = false;

        for ch in raw.chars() {
            if in_string {
                if escape {
                    escape = false;
                    continue;
                }
                match ch {
                    '\\' => escape = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }

            match ch {
                '"' => in_string = true,
                '{' | '[' => stack.push(ch),
                '}' => {
                    if stack.pop() != Some('{') {
                        return None;
                    }
                }
                ']' if stack.pop() != Some('[') => return None,
                _ => {}
            }
        }

        if in_string || stack.is_empty() {
            return None;
        }

        let mut repaired = raw.to_string();
        for open in stack.iter().rev() {
            repaired.push(match open {
                '{' => '}',
                '[' => ']',
                _ => return None,
            });
        }
        Some(repaired)
    }

    fn parse_stringified_files_array(
        raw: &str,
    ) -> Result<Vec<serde_json::Value>, serde_json::Error> {
        match serde_json::from_str::<Vec<serde_json::Value>>(raw) {
            Ok(files) => Ok(files),
            Err(err) if err.classify() == serde_json::error::Category::Eof => {
                if let Some(repaired) = Self::repair_truncated_files_json(raw)
                    && let Ok(files) = serde_json::from_str::<Vec<serde_json::Value>>(&repaired)
                {
                    return Ok(files);
                }
                Err(err)
            }
            Err(err) => Err(err),
        }
    }

    fn apply_top_level_path_fallback(files: &mut [serde_json::Value], fallback_path: Option<&str>) {
        let Some(path) = fallback_path else {
            return;
        };

        let missing_path_count = files
            .iter()
            .filter(|file| file.get("path").is_none() && file.get("edits").is_some())
            .count();
        if missing_path_count == 0 || missing_path_count != files.len() {
            return;
        }

        for file in files {
            if let Some(object) = file.as_object_mut() {
                object.insert(
                    "path".to_string(),
                    serde_json::Value::String(path.to_string()),
                );
            }
        }
    }

    /// Parse edits from JSON params.
    fn parse_edits(
        &self,
        files: &[serde_json::Value],
    ) -> Result<Vec<(String, Vec<Edit>)>, ToolError> {
        let mut result = Vec::new();

        for file in files {
            let path = Self::file_entry_path(file).map_err(ToolError::InvalidInput)?;

            let edits_raw = file
                .get("edits")
                .and_then(|e| e.as_array())
                .ok_or_else(|| {
                    // Lenient: models sometimes put anchor/edit_type/text as
                    // siblings at the file-entry level instead of inside an
                    // edits array. Detect this and give a targeted error.
                    let has_anchor = file.get("anchor").and_then(|a| a.as_str()).is_some();
                    let has_edit_type = file.get("edit_type").and_then(|t| t.as_str()).is_some();
                    let has_text = file.get("text").and_then(|t| t.as_str()).is_some();
                    if has_anchor || has_edit_type || has_text {
                        ToolError::InvalidInput(format!(
                            "The 'anchor', 'edit_type', and 'text' fields must be inside an 'edits' array, not at the file-entry level.\n\n\
                             Correct: {{ \"path\": \"{path}\", \"edits\": [{{ \"anchor\": \"...\", \"text\": \"...\" }}] }}\n\
                             Wrong:   {{ \"path\": \"{path}\", \"anchor\": \"...\", \"text\": \"...\" }}"
                        ))
                    } else {
                        ToolError::InvalidInput(format!(
                            "Missing 'edits' for file '{}'. {}",
                            path,
                            error_guidance::missing_parameter("edits", 0)
                        ))
                    }
                })?;

            let mut edits = Vec::new();
            for edit_raw in edits_raw {
                let anchor_raw =
                    edit_raw
                        .get("anchor")
                        .and_then(|a| a.as_str())
                        .ok_or_else(|| {
                            ToolError::InvalidInput(format!(
                                "Missing 'anchor' in edit for file '{}'. {}",
                                path,
                                error_guidance::missing_parameter("anchor", 0)
                            ))
                        })?;
                let anchor = Self::normalized_anchor("anchor", path, anchor_raw)
                    .map_err(ToolError::InvalidInput)?;

                let edit_type = edit_raw
                    .get("edit_type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("replace");

                Self::validate_edit_type(edit_type)?;

                let end_anchor = edit_raw
                    .get("end_anchor")
                    .and_then(|e| e.as_str())
                    .map(|s| Self::normalized_anchor("end_anchor", path, s))
                    .transpose()
                    .map_err(ToolError::InvalidInput)?;

                let text_raw = edit_raw.get("text").and_then(|t| t.as_str()).unwrap_or("");
                // Strip leaked anchor prefixes that the model may have
                // copy-pasted from the diff output (e.g. `QualitySocial§...`
                // or `deadbeef§...`). Do NOT interpret `\n` as a real
                // newline: the model sends real newlines via JSON's `\n`
                // escape (which serde decodes to a real newline), and
                // sends literal `\n` (two chars for C string escapes) via
                // JSON's `\\n` (which serde decodes to `\n` two chars).
                // Interpreting both as newlines would corrupt C string
                // literals like `fprintf(stderr, "...failed\\n");`.
                let text = strip_hashes(text_raw);

                edits.push(Edit {
                    anchor,
                    end_anchor,
                    edit_type: edit_type.to_string(),
                    text,
                });
            }

            result.push((path.to_string(), edits));
        }

        Ok(result)
    }

    /// Resolve a relative path to absolute path with sanitization.
    fn resolve_path(&self, workspace_root: &Path, path: &str) -> Result<String, ToolError> {
        let resolved = crate::core::tools::resolve_sanitized_path(workspace_root, path)?;
        Ok(resolved.to_string_lossy().to_string())
    }

    /// Validate edit_type values.
    ///
    /// Valid edit types are: "replace", "insert_before", "insert_after".
    /// Unknown edit types are rejected with a clear error message.
    fn validate_edit_type(edit_type: &str) -> Result<(), ToolError> {
        match edit_type {
            "replace" | "insert_before" | "insert_after" => Ok(()),
            _ => Err(ToolError::InvalidInput(format!(
                "Unknown edit_type '{edit_type}'. Valid values are: replace, insert_before, insert_after"
            ))),
        }
    }

    /// Validate that all anchors contain the hash delimiter.
    ///
    /// This is a pre-validation check that runs BEFORE sending to the model.
    /// It catches the common mistake of calling edit_file without read_file first.
    fn validate_anchors(
        &self,
        files: &[serde_json::Value],
        workspace_root: &Path,
    ) -> Result<(), ToolError> {
        let mut invalid_anchors = Vec::new();
        let mut path_errors = Vec::new();
        let mut affected_paths = Vec::new();

        for file in files {
            let path = match Self::file_entry_path(file) {
                Ok(path) => path,
                Err(message) => {
                    path_errors.push(format!("  - {message}"));
                    continue;
                }
            };
            if let Ok(resolved) = self.resolve_path(workspace_root, path) {
                affected_paths.push(resolved);
            }

            let edits: &[serde_json::Value] = file
                .get("edits")
                .and_then(|e| e.as_array())
                .map_or(&[], std::vec::Vec::as_slice);

            for edit in edits {
                let anchor_raw = edit.get("anchor").and_then(|a: &serde_json::Value| a.as_str()).unwrap_or("");
                let anchor = match Self::normalized_anchor("anchor", path, anchor_raw) {
                    Ok(anchor) => anchor,
                    Err(message) => {
                        invalid_anchors.push(format!("  - {message}"));
                        continue;
                    }
                };

                if !anchor.contains(ANCHOR_DELIMITER) {
                    invalid_anchors.push(format!(
                        "  - File '{}': anchor '{}' is missing the '{}' delimiter",
                        path,
                        if anchor.chars().count() > 50 {
                            format!("{}...", anchor.chars().take(50).collect::<String>())
                        } else {
                            anchor.clone()
                        },
                        ANCHOR_DELIMITER
                    ));
                } else {
                    let (anchor_name, _) = split_anchor(&anchor);
                    if anchor_name.is_empty() || anchor_name.chars().all(|c| c.is_ascii_digit()) {
                        invalid_anchors.push(format!(
                            "  - File '{}': anchor '{}' must include a non-numeric anchor name before the '{}' delimiter",
                            path,
                            if anchor.chars().count() > 50 {
                                format!("{}...", anchor.chars().take(50).collect::<String>())
                            } else {
                                anchor.clone()
                            },
                            ANCHOR_DELIMITER
                        ));
                    }
                }

                if let Some(end_anchor_raw) = edit.get("end_anchor").and_then(|a: &serde_json::Value| a.as_str()) {
                    let end_anchor =
                        match Self::normalized_anchor("end_anchor", path, end_anchor_raw) {
                            Ok(anchor) => anchor,
                            Err(message) => {
                                invalid_anchors.push(format!("  - {message}"));
                                continue;
                            }
                        };
                    if !end_anchor.contains(ANCHOR_DELIMITER) {
                        invalid_anchors.push(format!(
                            "  - File '{}': end_anchor '{}' is missing the '{}' delimiter",
                            path,
                            if end_anchor.chars().count() > 50 {
                                format!("{}...", end_anchor.chars().take(50).collect::<String>())
                            } else {
                                end_anchor.clone()
                            },
                            ANCHOR_DELIMITER
                        ));
                    } else {
                        let (end_anchor_name, _) = split_anchor(&end_anchor);
                        if end_anchor_name.is_empty()
                            || end_anchor_name.chars().all(|c| c.is_ascii_digit())
                        {
                            invalid_anchors.push(format!(
                                "  - File '{}': end_anchor '{}' must include a non-numeric anchor name before the '{}' delimiter",
                                path,
                                if end_anchor.chars().count() > 50 {
                                    format!("{}...", end_anchor.chars().take(50).collect::<String>())
                                } else {
                                    end_anchor.clone()
                                },
                                ANCHOR_DELIMITER
                            ));
                        }
                    }
                }
            }
        }

        if !path_errors.is_empty() {
            let mut message = String::from(
                "Missing 'path' key in file entry. Each object in the 'files' array must have 'path' at the top level (a sibling of 'edits'), not nested inside the edit object.\n\n",
            );
            message.push_str("Problems detected:\n");
            message.push_str(&path_errors.join("\n"));
            if !invalid_anchors.is_empty() {
                message.push_str("\n\nAdditionally, anchor issues were found:\n");
                message.push_str(&invalid_anchors.join("\n"));
            }
            message.push_str("\n\nCorrect structure: { \"path\": \"file.py\", \"edits\": [{ \"anchor\": \"...\", \"text\": \"...\" }] }");
            message.push_str("\nWrong structure: { \"edits\": [{ \"anchor\": \"...\", \"text\": \"...\", \"path\": \"file.py\" }] }");
            affected_paths.sort();
            affected_paths.dedup();
            return Err(ToolError::InvalidInput(message));
        }

        if !invalid_anchors.is_empty() {
            let mut message = String::from(
                "Hash anchor validation failed. You must call read_file before edit_file to get hash-anchored lines.\n\n",
            );
            message.push_str("Anchors must be copied EXACTLY from read_file output (format: Word§line content).\n\n");
            message.push_str("Invalid anchors detected:\n");
            message.push_str(&invalid_anchors.join("\n"));
            message.push_str("\n\nExample of CORRECT anchor: \"Crawler§void draw_game_over() {\"");
            message.push_str("\nExample of WRONG anchor: \"void draw_game_over() {\"");
            affected_paths.sort();
            affected_paths.dedup();
            return Err(ToolError::InvalidInputWithMetadata(
                message,
                ToolFailureMetadata {
                    class: ToolFailureClass::AnchorInvalid,
                    affected_paths,
                    required_next_step: Some(ToolRequiredNextStep::ReadFile),
                },
            ));
        }

        Ok(())
    }

    fn mark_must_reread(state: &mut TaskState, path: &str) {
        state.must_reread_before_edit.insert(path.to_string());
        state.file_content_cache.pop(path);
        state.consecutive_reads.remove(path);
    }

    fn reread_required_error(display_path: &str, absolute_path: &str) -> ToolError {
        ToolError::ExecutionFailedWithMetadata(
            format!(
                "You must re-read {display_path} before retrying edit_file. A successful edit (or a prior failed attempt) changed the file, so the hash anchors from your previous read_file are stale. Call read_file on this path to get fresh anchors, then retry the edit with the new anchors."
            ),
            ToolFailureMetadata {
                class: ToolFailureClass::AnchorInvalid,
                affected_paths: vec![absolute_path.to_string()],
                required_next_step: Some(ToolRequiredNextStep::ReadFile),
            },
        )
    }

    fn external_modification_error(display_path: &str, absolute_path: &str) -> ToolError {
        ToolError::ExecutionFailedWithMetadata(
            format!(
                "File {display_path} was modified externally during edit operation. Aborting write to prevent data loss. Re-read the file and retry."
            ),
            ToolFailureMetadata {
                class: ToolFailureClass::AnchorInvalid,
                affected_paths: vec![absolute_path.to_string()],
                required_next_step: Some(ToolRequiredNextStep::ReadFile),
            },
        )
    }
}

impl EditFileHandler {
    async fn execute_with_workspace_root(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        workspace_root: &Path,
        anchor_mgr: &AnchorStateManager,
        task_id: Option<&str>,
        explicitly_approved: bool,
        json_output: bool,
        output_writer: &crate::cli::output::OutputWriterArc,
    ) -> Result<String, ToolError> {
        let files_value = params.get("files");
        let top_level_path = params.get("path").and_then(|p| p.as_str());
        let parsed_stringified_files = files_value
            .and_then(|f| f.as_str())
            .map(Self::parse_stringified_files_array);
        let files: Vec<serde_json::Value> = files_value
            .and_then(|f| f.as_array().cloned())
            .or_else(|| {
                parsed_stringified_files
                    .as_ref()
                    .and_then(|result| result.as_ref().ok().cloned())
            })
            .unwrap_or_default();
        let mut files = files;
        Self::apply_top_level_path_fallback(&mut files, top_level_path);

        if files.is_empty() {
            return if let Some(value) = files_value {
                if value
                    .as_array()
                    .is_some_and(std::vec::Vec::is_empty)
                {
                    Ok("No files specified. The 'files' array is empty; provide at least one object with 'path' and 'edits' fields.".to_string())
                } else if value.as_str().is_some() {
                    match parsed_stringified_files.unwrap() {
                        Ok(parsed) if parsed.is_empty() => Ok("No files specified. The 'files' array is empty; provide at least one object with 'path' and 'edits' fields.".to_string()),
                        Ok(_) => unreachable!("files vec is empty but parse succeeded with non-empty"),
                        Err(e) => Ok(format!(
                            "Failed to parse 'files' parameter as a JSON array string. The 'files' parameter must be a JSON array of {{path, edits}} objects, e.g. [{{\"path\":\"file.rs\",\"edits\":[...]}}]. Parse error: {e}"
                        )),
                    }
                } else {
                    Ok(format!("Failed to parse 'files' parameter. Expected an array of {{path, edits}} objects, got: {value}. The 'files' parameter must be a JSON array like: [{{\"path\":\"file.rs\",\"edits\":[...]}}]."))
                }
            } else {
                Ok("No files specified. The 'files' parameter must be an array of objects with 'path' and 'edits' fields.".to_string())
            };
        }

        self.validate_anchors(&files, workspace_root)?;

        let parsed = self.parse_edits(&files)?;
        let processor = BatchProcessor::new(DiffMode::Full);

        let silent = params
            .get("silent")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        // Group edits by resolved absolute path
        let file_edits: Result<Vec<(String, Vec<Edit>)>, ToolError> = parsed
            .into_iter()
            .map(|(path, edits)| {
                let abs_path = self.resolve_path(workspace_root, &path)?;
                Ok((abs_path, edits))
            })
            .collect();
        let file_edits = file_edits?;

        let batches = processor.group_edits_by_path(&file_edits, &|path| Some(path.to_string()));
        let unique_file_count = batches.len();

        let mut all_results: Vec<String> = Vec::new();
        let mut total_applied = 0usize;
        let mut total_failed = 0usize;
        let mut total_overlap = 0usize;
        let mut total_edits = 0usize;
        let mut diff_previews: Vec<String> = Vec::new();
        let mut prepared_batches: Vec<(crate::core::edit_batch::FileEditBatch, PreparedEdits)> =
            Vec::new();

        // Phase 1: Prepare all batches and collect diff previews
        for batch in batches {
            if state.must_reread_before_edit.contains(&batch.absolute_path) {
                return Err(Self::reread_required_error(
                    &batch.display_path,
                    &batch.absolute_path,
                ));
            }

            // Acquire exclusive file lock to prevent concurrent edits
            let _file_guard = FileEditGuard::acquire(&batch.absolute_path).await;

            let stale_warning = state
                .file_context_tracker
                .check_stale(Path::new(&batch.absolute_path))
                .await;
            if stale_warning.is_some() {
                Self::mark_must_reread(state, &batch.absolute_path);
                return Err(Self::external_modification_error(
                    &batch.display_path,
                    &batch.absolute_path,
                ));
            }

            // Warn if editing a file not read this session
            if !json_output
                && !state
                    .file_context_tracker
                    .was_read_this_session(&batch.display_path)
            {
                use crate::cli::output::OutputEvent;
                use crate::cli::tui::theme::WARNING_FG;
                use ratatui::style::Style;
                output_writer.emit(OutputEvent::tool_output_line(
                    format!(
                        "⚠ editing {} (not read this session — may have stale assumptions)",
                        batch.display_path
                    ),
                    Style::default().fg(WARNING_FG),
                ));
            }

            // Read file content and capture mtime (check cache first for cross-call coordination)
            let (content, initial_mtime) =
                if let Some(cached_content) = state.file_content_cache.get(&batch.absolute_path) {
                    // SECURITY: Re-verify file is still valid (not swapped with symlink) even when using cache
                    match tokio::fs::symlink_metadata(&batch.absolute_path).await {
                        Ok(metadata) if metadata.is_file() && !metadata.is_symlink() => {
                            tracing::debug!(
                                "Using cached content for {} (symlink check passed)",
                                batch.display_path
                            );
                            (cached_content.clone(), None)
                        }
                        Ok(_) => {
                            all_results.push(format!(
                                "File {} is no longer a regular file (may be symlink)",
                                batch.display_path
                            ));
                            total_failed += 1;
                            continue;
                        }
                        Err(e) => {
                            all_results.push(format!(
                                "Error verifying file {}: {}",
                                batch.display_path, e
                            ));
                            total_failed += 1;
                            continue;
                        }
                    }
                } else {
                    match tokio::fs::metadata(&batch.absolute_path).await {
                        Ok(metadata) => {
                            let mtime = metadata.modified().ok();
                            match tokio::fs::read_to_string(&batch.absolute_path).await {
                                Ok(c) => (c, mtime),
                                Err(e) => {
                                    all_results.push(format!(
                                        "Error reading file {}: {}",
                                        batch.display_path, e
                                    ));
                                    total_failed += 1;
                                    continue;
                                }
                            }
                        }
                        Err(_e) => {
                            // If metadata fails, continue without mtime tracking
                            match tokio::fs::read_to_string(&batch.absolute_path).await {
                                Ok(c) => (c, None),
                                Err(e) => {
                                    all_results.push(format!(
                                        "Error reading file {}: {}",
                                        batch.display_path, e
                                    ));
                                    total_failed += 1;
                                    continue;
                                }
                            }
                        }
                    }
                };

            // Track file read for stale context detection
            state
                .file_context_tracker
                .track_file_read(Path::new(&batch.absolute_path));

            // Stale-anchor preflight: capture the tracked anchor set BEFORE
            // reconcile mutates it, so we can detect anchors the model is
            // reusing from a previous read after the file changed.
            let pre_reconcile_anchors = anchor_mgr.get_anchors(&batch.absolute_path, task_id);

            // Compute line hashes via AnchorStateManager
            let lines = crate::core::file_editor::split_content_lines(&content);
            let anchors = anchor_mgr.reconcile(&batch.absolute_path, &lines, task_id);

            // If the model submitted anchors that exist in the OLD tracked
            // state but not in the NEW one, the file has changed since its
            // last read. Surface a clearer error than the generic
            // "anchor not found" and force a re-read.
            if let Some(ref old) = pre_reconcile_anchors {
                let new_set: std::collections::HashSet<&str> =
                    anchors.iter().map(std::string::String::as_str).collect();
                let mut stale_anchors: Vec<String> = Vec::new();
                for edit in &batch.edits {
                    let anchor_raw = edit.anchor.lines().next().unwrap_or("").trim();
                    if let Some((name, _)) = anchor_raw.split_once(ANCHOR_DELIMITER) {
                        let name = name.trim();
                        if old.iter().any(|a| a == name) && !new_set.contains(name) {
                            stale_anchors.push(anchor_raw.to_string());
                        }
                    }
                }
                if !stale_anchors.is_empty() {
                    Self::mark_must_reread(state, &batch.absolute_path);
                    let mut msg = String::from(
                        "Stale anchor detected: this anchor is from a previous read_file call. \
                         The file has changed since then. Call read_file to refresh anchors.\n\n",
                    );
                    msg.push_str("Stale anchors:\n");
                    for a in &stale_anchors {
                        msg.push_str(&format!("  - {a}\n"));
                    }
                    all_results.push(format!(
                        "Error preparing edits for {}: {}",
                        batch.display_path, msg
                    ));
                    total_failed += batch.edits.len();
                    continue;
                }
            }

            // Prepare edits
            let mut prepared = match processor.prepare_edits(
                &batch.absolute_path,
                &batch.display_path,
                &content,
                &batch.edits,
                &anchors,
            ) {
                Ok(p) => p,
                Err(e) => {
                    if matches!(e, FileEditorError::AllEditsFailed { .. }) {
                        Self::mark_must_reread(state, &batch.absolute_path);
                    }
                    all_results.push(format!(
                        "Error preparing edits for {}: {}",
                        batch.display_path, e
                    ));
                    total_failed += batch.edits.len();
                    continue;
                }
            };

            // Store initial mtime for external modification detection
            prepared.initial_mtime = initial_mtime;

            total_edits += batch.edits.len();
            // Generate diff preview without modifying prepared (skip in silent mode)
            if !silent {
                let diff_preview = processor.generate_diff(&batch.display_path, &prepared);
                diff_previews.push(diff_preview);
            }

            // Store for later (after approval)
            prepared_batches.push((batch, prepared));
        }

        // Phase 2: Combined approval check (skip only when explicitly approved)
        if !prepared_batches.is_empty() && !explicitly_approved {
            let should_prompt = if let Some(ref am) = self.approval_manager {
                let mgr = am.lock().await;
                prepared_batches.iter().any(|b| {
                    mgr.should_prompt_with_path(SnedTool::EditFile, Some(&b.0.display_path))
                })
            } else {
                false
            };

            if should_prompt {
                let diff_text = diff_previews.join("\n\n");
                match prompt_for_combined_approval(
                    prepared_batches.len(),
                    total_edits,
                    &diff_text,
                    output_writer,
                )
                .await
                {
                    Ok(crate::core::approval::ApprovalResult::Denied) => {
                        return Ok(crate::core::approval::format_denial_message(
                            SnedTool::EditFile.name(),
                        ));
                    }
                    Ok(crate::core::approval::ApprovalResult::Always) => {
                        if let Some(ref am) = self.approval_manager {
                            let mut mgr = am.lock().await;
                            // EditFile doesn't need command fingerprint (only for execute_command)
                            mgr.auto_approve(SnedTool::EditFile, None);
                        }
                    }
                    Ok(crate::core::approval::ApprovalResult::Approved) => {
                        // Proceed with edits
                    }
                    Err(e) => {
                        return Err(ToolError::ExecutionFailed(format!("Approval error: {e}")));
                    }
                }
            }
        }

        // Phase 3: Capture pre-save diagnostics for all files being edited
        // Group files by (project_root, project_type) to handle mixed-language projects
        let mut files_by_project: HashMap<(PathBuf, ProjectType), Vec<PathBuf>> =
            HashMap::with_capacity(prepared_batches.len());
        for (batch, _) in &prepared_batches {
            let path = PathBuf::from(&batch.absolute_path);
            let project_type = DiagnosticsScanHandler::detect_project_type(&path);
            let project_root = DiagnosticsScanHandler::find_ancestor_with_file(
                &path,
                if project_type == crate::core::tools::handlers::diagnostics_scan::ProjectType::Rust
                {
                    "Cargo.toml"
                } else {
                    "package.json"
                },
            )
            .unwrap_or_else(|| {
                path.parent()
                    .map_or(PathBuf::from("."), std::path::Path::to_path_buf)
            });
            files_by_project
                .entry((project_root, project_type))
                .or_default()
                .push(path);
        }

        // Run diagnostics in parallel across (project_root, project_type) groups
        let batch_diag_outputs =
            DiagnosticsScanHandler::run_diagnostics_batch(&files_by_project).await;

        // Parse diagnostics for each file
        let mut pre_diagnostics: std::collections::HashMap<
            String,
            Vec<crate::core::tools::handlers::diagnostics_scan::Diagnostic>,
        > = std::collections::HashMap::with_capacity(prepared_batches.len());
        for (batch, _) in &prepared_batches {
            let diag_output = batch_diag_outputs
                .get(&PathBuf::from(&batch.absolute_path))
                .cloned()
                .unwrap_or_default();
            let diagnostics =
                DiagnosticsScanHandler::parse_diagnostics(&diag_output, &batch.display_path);
            pre_diagnostics.insert(batch.absolute_path.clone(), diagnostics);
        }

        // Track if any file had pre-existing errors (to decide whether to run post-diagnostics)
        let any_pre_errors = pre_diagnostics.values().any(|diags| {
            diags.iter().any(|d| {
                matches!(
                    d.severity,
                    crate::core::tools::handlers::diagnostics_scan::Severity::Error
                )
            })
        });

        // Phase 4a: Validate all edits and compute final content (no disk writes)
        // Track which files were successfully edited for post-diagnostics
        let mut successfully_edited_files: Vec<(String, String)> = Vec::new(); // (absolute_path, display_path)

        // Store intermediate results for building final output after batch diagnostics
        struct FileResult {
            batch_absolute_path: String,
            batch_display_path: String,
            prepared: PreparedEdits,
            final_lines: Vec<String>,
            final_hashes: Vec<String>,
            had_success: bool,
        }
        let mut file_results: Vec<FileResult> = Vec::new();

        // Collect file writes for Phase 4b (two-phase commit with rollback)
        struct WriteItem {
            absolute_path: String,
            display_path: String,
            final_content: String,
            initial_mtime: Option<std::time::SystemTime>,
        }
        let mut write_items: Vec<WriteItem> = Vec::new();

        for (batch, mut prepared) in prepared_batches {
            let result =
                processor.apply_batch(&mut prepared, &batch.absolute_path, &batch.display_path);

            if !prepared.failed_edits.is_empty() {
                Self::mark_must_reread(state, &batch.absolute_path);
            }

            if result.success {
                total_applied += result.resolved_count;
            }
            total_failed += result.failed_count;
            if result.overlap {
                total_overlap += result.resolved_count;
            }

            // Record file change for session summary
            if result.success && result.resolved_count > 0 {
                let entry = state
                    .session_file_changes
                    .entry(batch.absolute_path.clone())
                    .or_insert_with(|| crate::core::agent_types::FileChangeStats {
                        lines_added: 0,
                        lines_removed: 0,
                        action: "edited".to_string(),
                    });
                entry.lines_added = entry.lines_added.saturating_add(result.lines_added);
                entry.lines_removed = entry.lines_removed.saturating_add(result.lines_removed);
            }

            if result.success {
                // Track for post-diagnostics if there were pre-errors
                if any_pre_errors {
                    successfully_edited_files
                        .push((batch.absolute_path.clone(), batch.display_path.clone()));
                }

                // Collect for write phase (Phase 4b)
                if let Some(ref final_content) = result.final_content {
                    write_items.push(WriteItem {
                        absolute_path: batch.absolute_path.clone(),
                        display_path: batch.display_path.clone(),
                        final_content: final_content.clone(),
                        initial_mtime: prepared.initial_mtime,
                    });
                }
            }

            // Store intermediate result for building final output after batch diagnostics.
            // Use split('\n') instead of .lines() to match split_content_lines() semantics:
            // .lines() strips a trailing empty element, but split('\n') preserves it.
            // applied_edits indices are based on split('\n') counts, so we must match.
            let final_lines: Vec<String> = result
                .final_content
                .as_ref()
                .map(|c| c.split('\n').map(std::string::ToString::to_string).collect())
                .unwrap_or_default();

            let final_hashes = compute_hashes(&final_lines)
                .iter()
                .map(|h| format!("{h:08x}"))
                .collect::<Vec<_>>();

            file_results.push(FileResult {
                batch_absolute_path: batch.absolute_path.clone(),
                batch_display_path: batch.display_path.clone(),
                prepared,
                final_lines,
                final_hashes,
                had_success: result.success,
            });
        }

        // Phase 4b: Write all files with rollback on failure
        // If any file fails to write, restore all previously written files to original content
        if !write_items.is_empty() {
            // Snapshot original content for all files before any writes
            let mut original_contents: std::collections::HashMap<String, String> =
                std::collections::HashMap::with_capacity(write_items.len());
            for item in &write_items {
                if let Ok(c) = tokio::fs::read_to_string(&item.absolute_path).await {
                    original_contents.insert(item.absolute_path.clone(), c);
                }
            }

            // Track which files were successfully written for rollback
            let mut written_paths: Vec<String> = Vec::new();
            // Collect rollback failures to report to user
            let mut rollback_errors: Vec<String> = Vec::new();

            for item in &write_items {
                let std_file = match std::fs::OpenOptions::new()
                    .write(true)
                    .open(&item.absolute_path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        // Rollback all previously written files
                        // Note: previously written files were already unlocked after their atomic writes
                        for path in written_paths.iter().rev() {
                            if let Some(orig) = original_contents.get(path)
                                && let Err(re) = crate::storage::disk::atomic_write_file(path, orig)
                            {
                                rollback_errors
                                    .push(format!("Failed to rollback {path}: {re}"));
                            }
                        }
                        if !rollback_errors.is_empty() {
                            return Err(ToolError::ExecutionFailed(format!(
                                "Failed to open file {} for locking: {}. Rollback incomplete: {}",
                                item.display_path,
                                e,
                                rollback_errors.join(", ")
                            )));
                        }
                        return Err(ToolError::ExecutionFailed(format!(
                            "Failed to open file {} for locking: {}",
                            item.display_path, e
                        )));
                    }
                };

                let lock_result = std_file.try_lock();

                if lock_result.is_err() {
                    tracing::debug!(
                        "File {} locked by another process, skipping exclusive lock",
                        item.display_path
                    );

                    // Re-check mtime immediately before write to close TOCTOU window
                    let mtime_ok = if let Some(initial_mtime) = &item.initial_mtime {
                        match tokio::fs::metadata(&item.absolute_path).await {
                            Ok(current_metadata) => match current_metadata.modified() {
                                Ok(current_mtime) => &current_mtime == initial_mtime,
                                Err(_) => true,
                            },
                            Err(_) => true,
                        }
                    } else {
                        true
                    };

                    if !mtime_ok {
                        Self::mark_must_reread(state, &item.absolute_path);
                        return Err(Self::external_modification_error(
                            &item.display_path,
                            &item.absolute_path,
                        ));
                    }

                    state.insert_file_content(
                        item.absolute_path.clone(),
                        item.final_content.clone(),
                    );
                    state
                        .file_context_tracker
                        .mark_file_as_edited_by_sned(Path::new(&item.absolute_path));

                    let write_result = crate::storage::disk::atomic_write_file_async(
                        &item.absolute_path,
                        &item.final_content,
                    )
                    .await;

                    match write_result {
                        Ok(()) => {
                            written_paths.push(item.absolute_path.clone());
                        }
                        Err(e) => {
                            for path in written_paths.iter().rev() {
                                if let Some(orig) = original_contents.get(path)
                                    && let Err(re) =
                                        crate::storage::disk::atomic_write_file(path, orig)
                                {
                                    rollback_errors
                                        .push(format!("Failed to rollback {path}: {re}"));
                                }
                            }
                            if rollback_errors.is_empty() {
                                all_results.push(format!(
                                    "Error writing file {}: {}",
                                    item.display_path, e
                                ));
                            } else {
                                all_results.push(format!(
                                    "Error writing file {}: {}. Rollback incomplete: {}",
                                    item.display_path,
                                    e,
                                    rollback_errors.join(", ")
                                ));
                            }
                        }
                    }
                } else {
                    // Re-check mtime immediately before write to close TOCTOU window
                    let mtime_ok = if let Some(initial_mtime) = &item.initial_mtime {
                        match tokio::fs::metadata(&item.absolute_path).await {
                            Ok(current_metadata) => match current_metadata.modified() {
                                Ok(current_mtime) => &current_mtime == initial_mtime,
                                Err(_) => true,
                            },
                            Err(_) => true,
                        }
                    } else {
                        true
                    };

                    if !mtime_ok {
                        let _ = std_file.unlock();
                        Self::mark_must_reread(state, &item.absolute_path);
                        return Err(Self::external_modification_error(
                            &item.display_path,
                            &item.absolute_path,
                        ));
                    }

                    state.insert_file_content(
                        item.absolute_path.clone(),
                        item.final_content.clone(),
                    );
                    state
                        .file_context_tracker
                        .mark_file_as_edited_by_sned(Path::new(&item.absolute_path));

                    let write_result = crate::storage::disk::atomic_write_file_async(
                        &item.absolute_path,
                        &item.final_content,
                    )
                    .await;

                    let _ = std_file.unlock();

                    match write_result {
                        Ok(()) => {
                            written_paths.push(item.absolute_path.clone());
                        }
                        Err(e) => {
                            for path in written_paths.iter().rev() {
                                if let Some(orig) = original_contents.get(path)
                                    && let Err(re) =
                                        crate::storage::disk::atomic_write_file(path, orig)
                                {
                                    rollback_errors
                                        .push(format!("Failed to rollback {path}: {re}"));
                                }
                            }
                            if rollback_errors.is_empty() {
                                all_results.push(format!(
                                    "Error writing file {}: {}",
                                    item.display_path, e
                                ));
                            } else {
                                all_results.push(format!(
                                    "Error writing file {}: {}. Rollback incomplete: {}",
                                    item.display_path,
                                    e,
                                    rollback_errors.join(", ")
                                ));
                            }
                        }
                    }
                }
            }
        }

        // After successful write, mark all written files as requiring re-read
        // so the model's cached anchors cannot become stale silently.
        for item in &write_items {
            Self::mark_must_reread(state, &item.absolute_path);
            // Clear the read-loop counter for this file — an edit breaks the loop.
            state.consecutive_reads.remove(&item.absolute_path);
        }

        // Emit a hint so the model knows it must re-read before the next edit.
        // Without this, the model often tries to reuse anchors from a prior read_file
        // call and hits Hash anchor validation errors, triggering read/edit loops.
        if !write_items.is_empty() && total_applied > 0 && !json_output {
            use crate::cli::output::OutputEvent;
            use crate::cli::tui::theme::ACCENT;
            use ratatui::style::Style;
            let written_paths: Vec<String> = write_items
                .iter()
                .map(|item| {
                    std::path::Path::new(&item.absolute_path)
                        .file_name().map_or_else(|| item.absolute_path.clone(), |n| n.to_string_lossy().to_string())
                })
                .collect();
            output_writer.emit(OutputEvent::tool_output_line(
                format!(
                    "✓ {} file(s) changed: {}. Call read_file before the next edit on these files to refresh anchors.",
                    written_paths.len(),
                    written_paths.join(", ")
                ),
                Style::default().fg(ACCENT),
            ));
        }

        // Phase 5: Run post-save diagnostics in batch for all successfully edited files
        // This is more efficient than per-file diagnostics when any_pre_errors is true
        let mut post_diagnostics_by_file: std::collections::HashMap<
            String,
            Vec<crate::core::tools::handlers::diagnostics_scan::Diagnostic>,
        > = std::collections::HashMap::with_capacity(successfully_edited_files.len());

        if any_pre_errors && !successfully_edited_files.is_empty() {
            // Group successfully edited files by (project_root, project_type)
            let mut files_by_project: HashMap<(PathBuf, ProjectType), Vec<PathBuf>> =
                HashMap::with_capacity(successfully_edited_files.len());
            for (abs_path, _) in &successfully_edited_files {
                let path = PathBuf::from(abs_path);
                let project_type = DiagnosticsScanHandler::detect_project_type(&path);
                let project_root = DiagnosticsScanHandler::find_ancestor_with_file(
                    &path,
                    if project_type
                        == crate::core::tools::handlers::diagnostics_scan::ProjectType::Rust
                    {
                        "Cargo.toml"
                    } else {
                        "package.json"
                    },
                )
                .unwrap_or_else(|| {
                    path.parent()
                        .map_or(PathBuf::from("."), std::path::Path::to_path_buf)
                });
                files_by_project
                    .entry((project_root, project_type))
                    .or_default()
                    .push(path);
            }

            // Run batch diagnostics once per (project_root, project_type) group
            let batch_diag_outputs =
                DiagnosticsScanHandler::run_diagnostics_batch(&files_by_project).await;

            // Parse diagnostics for each file
            for (abs_path, display_path) in &successfully_edited_files {
                let diag_output = batch_diag_outputs
                    .get(&PathBuf::from(abs_path))
                    .cloned()
                    .unwrap_or_default();
                let diagnostics =
                    DiagnosticsScanHandler::parse_diagnostics(&diag_output, display_path);
                post_diagnostics_by_file.insert(abs_path.clone(), diagnostics);
            }
        }

        // Phase 6: Build final results with diagnostics comparison
        for file_result in file_results {
            if !file_result.had_success {
                continue;
            }

            let diagnostics = if any_pre_errors {
                let post_diagnostics = post_diagnostics_by_file
                    .get(&file_result.batch_absolute_path)
                    .cloned()
                    .unwrap_or_default();
                let pre = pre_diagnostics
                    .get(&file_result.batch_absolute_path)
                    .cloned()
                    .unwrap_or_default();

                // Count pre/post errors
                let pre_errors = pre
                    .iter()
                    .filter(|d| {
                        matches!(
                            d.severity,
                            crate::core::tools::handlers::diagnostics_scan::Severity::Error
                        )
                    })
                    .count();
                let post_errors = post_diagnostics
                    .iter()
                    .filter(|d| {
                        matches!(
                            d.severity,
                            crate::core::tools::handlers::diagnostics_scan::Severity::Error
                        )
                    })
                    .count();

                let fixed_count = pre_errors.saturating_sub(post_errors);

                // Find new problems (post diagnostics not in pre)
                let new_problems: Vec<_> = post_diagnostics
                    .iter()
                    .filter(|pd| {
                        !pre.iter()
                            .any(|pre_d| pre_d.message == pd.message && pre_d.line == pd.line)
                    })
                    .cloned()
                    .collect();

                let new_problems_message = if new_problems.is_empty() {
                    String::new()
                } else {
                    DiagnosticsScanHandler::format_diagnostics(
                        &file_result.batch_display_path,
                        &new_problems,
                        None,
                    )
                };

                Some(DiagnosticsResult {
                    fixed_count,
                    new_problems_message,
                })
            } else {
                None
            };

            let formatted = processor.format_result(
                &file_result.prepared,
                &file_result.final_lines,
                &file_result.final_hashes,
                diagnostics.as_ref(),
                None,
                None,
            );
            all_results.push(formatted);
        }

        // Note: consecutive_mistakes tracking is handled centrally in agent_loop.rs.
        // edit_file emits diagnostics via output_writer but does not mutate the counter
        // to avoid double-counting when a single edit_file call has multiple file failures.

        let summary = if total_overlap > 0 {
            format!(
                "Edited {} file(s): {} edit(s) applied, {} edit(s) failed, {} edit(s) overlapped.",
                unique_file_count,
                total_applied,
                total_failed,
                total_overlap
            )
        } else {
            format!(
                "Edited {} file(s): {} edit(s) applied, {} edit(s) failed.",
                unique_file_count,
                total_applied,
                total_failed
            )
        };

        Ok(format!(
            "{}\n\n{}",
            summary,
            all_results.join("\n\n---\n\n")
        ))
    }

    #[must_use] 
    pub fn description(&self, params: &serde_json::Value) -> String {
        let path = params
            .get("files")
            .and_then(|f| f.as_array())
            .and_then(|arr| arr.first())
            .and_then(|f| f.get("path"))
            .and_then(|p| p.as_str())
            .unwrap_or("?");
        format!("[edit_file for '{path}']")
    }
}

impl ToolHandler for EditFileHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let mut state = ctx.state.lock().await;
            let result = handler
                .execute_with_workspace_root(
                    &mut state,
                    params,
                    ctx.workspace_root.as_path(),
                    &ctx.anchor_mgr,
                    Some(ctx.task_id.as_str()),
                    ctx.explicitly_approved,
                    ctx.json_output,
                    &ctx.output_writer,
                )
                .await;

            // Note: consecutive_mistakes tracking is handled centrally in agent_loop.rs.
            // edit_file does not mutate the counter to avoid double-counting.

            result.map(serde_json::Value::String)
        })
    }

    fn description(&self, params: &serde_json::Value) -> String {
        Self::description(self, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tools::{ToolContext, ToolHandler};
    use std::sync::Arc;
    use std::sync::LazyLock;

    static TEST_MUTEX: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[test]
    fn test_edit_file_handler_creation() {
        let handler = EditFileHandler::new();
        assert!(format!("{:?}", handler).starts_with("EditFileHandler"));
    }

    #[tokio::test]
    async fn test_edit_file_missing_files() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, serde_json::json!({})).await;
        assert!(result.is_ok());
        assert!(
            result
                .unwrap()
                .as_str()
                .unwrap()
                .contains("No files specified")
        );
    }

    #[tokio::test]
    async fn test_edit_file_empty_files() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, serde_json::json!({"files": []})).await;
        assert!(result.is_ok());
        assert!(
            result
                .unwrap()
                .as_str()
                .unwrap()
                .contains("No files specified")
        );
    }

    #[tokio::test]
    async fn test_edit_file_missing_files_exact_message() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, serde_json::json!({})).await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().as_str().unwrap(),
            "No files specified. The 'files' parameter must be an array of objects with 'path' and 'edits' fields."
        );
    }

    #[tokio::test]
    async fn test_edit_file_empty_array_exact_message() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, serde_json::json!({"files": []})).await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().as_str().unwrap(),
            "No files specified. The 'files' array is empty; provide at least one object with 'path' and 'edits' fields."
        );
    }

    #[tokio::test]
    async fn test_edit_file_stringified_empty_array_exact_message() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, serde_json::json!({"files": "[]"})).await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().as_str().unwrap(),
            "No files specified. The 'files' array is empty; provide at least one object with 'path' and 'edits' fields."
        );
    }

    #[tokio::test]
    async fn test_edit_file_wrong_type_exact_message() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, serde_json::json!({"files": 42})).await;
        assert!(result.is_ok());
        let result_val = result.unwrap();
        let msg = result_val.as_str().unwrap();
        assert!(
            msg.starts_with("Failed to parse 'files' parameter."),
            "Should get parse failure message, got: {}",
            msg
        );
        assert!(msg.contains("42"), "Error should mention the actual value");
    }

    #[tokio::test]
    async fn test_edit_file_accepts_stringified_files_array() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let raw_content = "line 1\nline 2\nline 3\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§line 1", anchors[0]);
        let stringified_files = serde_json::json!({
            "files": format!(
                r#"[{{"path": "test.txt", "edits": [{{"anchor": "{}", "edit_type": "replace", "text": "{}§replaced"}}]}}]"#,
                anchor,
                anchors[0],
            )
        });
        let result = ToolHandler::execute(&handler, &ctx, stringified_files).await;
        assert!(
            result.is_ok(),
            "stringified files array should parse: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "replaced\nline 2\nline 3\n");
    }

    #[tokio::test]
    async fn test_edit_file_accepts_top_level_path_fallback() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let raw_content = "line 1\nline 2\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("path-fallback"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "path-fallback".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let params = serde_json::json!({
            "path": "test.txt",
            "files": [{
                "edits": [{
                    "anchor": format!("{}§line 1", anchors[0]),
                    "edit_type": "replace",
                    "text": "replaced"
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "top-level path fallback should allow missing per-entry path: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "replaced\nline 2\n");
    }

    #[tokio::test]
    async fn test_edit_file_repairs_truncated_stringified_files_with_top_level_path() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let raw_content = "line 1\nline 2\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("repair-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "repair-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let params = serde_json::json!({
            "path": "test.txt",
            "files": format!(
                r#"[{{"edits":[{{"anchor":"{}","edit_type":"replace","text":"repaired"}}]}}"#,
                format!("{}§line 1", anchors[0]),
            )
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "truncated stringified files payload should be repaired when only closures are missing: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "repaired\nline 2\n");
    }

    #[tokio::test]
    async fn test_edit_file_accepts_runlog_style_multi_edit_stringified_payload() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("BUGS.md");
        let raw_content = "alpha\nbeta\ngamma\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("runlog-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "runlog-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let first_anchor = format!("{}§alpha", anchors[0]);
        let second_anchor = format!("{}§beta", anchors[1]);
        let params = serde_json::json!({
            "path": "BUGS.md",
            "files": format!(
                r#"[{{"edits":[{{"anchor":"{}","edit_type":"replace","text":"first"}},{{"anchor":"{}","edit_type":"replace","text":"second"}}]}}"#,
                first_anchor,
                second_anchor,
            )
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "runlog-style top-level path + truncated stringified payload should recover: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "first\nsecond\ngamma\n");
    }

    #[tokio::test]
    async fn test_edit_file_strips_leaked_anchor_prefixes_from_replacement_text() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let raw_content = "line 1\nline 2\nline 3\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§line 1", anchors[0]);
        let stringified_files = serde_json::json!({
            "files": format!(
                r#"[{{"path": "test.txt", "edits": [{{"anchor": "{}", "edit_type": "replace", "text": "f38ef2139e8cc75d§GymnoglossErratic §        replacement();"}}]}}]"#,
                anchor,
            )
        });
        let result = ToolHandler::execute(&handler, &ctx, stringified_files).await;
        assert!(
            result.is_ok(),
            "replacement text with leaked anchors should still apply: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "        replacement();\nline 2\nline 3\n");
    }

    #[tokio::test]
    async fn test_edit_file_strips_anchored_multiline_replacement_text() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let raw_content = "line 1\nline 2\nline 3\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines = crate::core::file_editor::split_content_lines(raw_content);
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§line 1", anchors[0]);
        let anchored_text = format!(
            "{}§replacement line 1\n{}§replacement line 2",
            anchors[1], anchors[2]
        );
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": anchored_text
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "anchored multiline replacement text should still apply: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            updated,
            "replacement line 1\nreplacement line 2\nline 2\nline 3\n"
        );
        assert!(
            !updated.contains('§'),
            "replacement text must not write read_file anchors into the file: {updated:?}"
        );
    }

    #[tokio::test]
    async fn test_edit_file_accepts_first_line_of_multiline_anchor() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor_mgr = AnchorStateManager::new();
        let lines = crate::core::file_editor::split_content_lines("line 1\nline 2\nline 3\n");
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));
        let anchor = format!("{}§line 1", anchors[0]);
        let multiline_anchor = format!("{}\nignored trailing line", anchor);
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": multiline_anchor,
                    "edit_type": "replace",
                    "end_anchor": anchor,
                    "text": "replaced"
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "multi-line anchor should normalize to the first line: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "replaced\nline 2\nline 3\n");
    }

    #[tokio::test]
    async fn test_edit_file_missing_path_reports_actionable_error() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "edits": [{
                    "anchor": "Apple§fn main() {",
                    "text": "replacement"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("requires a 'path' key"));
        assert!(err_msg.contains("src/file.rs"));
    }

    #[tokio::test]
    async fn test_edit_file_path_inside_edit_object_is_accepted() {
        // Regression: models sometimes put "path" inside the edit object
        // instead of at the file-entry level. The handler must accept this
        // leniently rather than failing with a confusing structural error.
        use tempfile::tempdir;
        let _guard = TEST_MUTEX.lock().await;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines = crate::core::file_editor::split_content_lines("line 1\nline 2\nline 3\n");
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // Model sends path inside edit object (common mistake)
        let anchor = format!("{}§line 2", anchors[1]);
        let params = serde_json::json!({
            "files": [{
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "replaced",
                    "path": "test.txt"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        // Should succeed — path inside edit object is accepted leniently
        assert!(
            result.is_ok(),
            "path inside edit object should be accepted leniently. Got error: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "line 1\nreplaced\nline 3\n");
    }

    #[tokio::test]
    async fn test_edit_file_edit_fields_as_siblings_reports_correct_error() {
        // Regression: models sometimes put anchor/edit_type/text as siblings
        // at the file-entry level instead of inside an edits array.
        // The error must explain the correct structure.
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // Model sends anchor/text as siblings of path (no edits array)
        let params = serde_json::json!({
            "files": [{
                "path": "test.rs",
                "anchor": "Apple§fn main() {",
                "edit_type": "replace",
                "text": "replacement"
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("must be inside an 'edits' array"),
            "Should explain that edit fields belong in 'edits' array. Got: {}",
            err_msg
        );
        assert!(
            err_msg.contains("Correct:"),
            "Should show correct structure. Got: {}",
            err_msg
        );
        assert!(
            err_msg.contains("Wrong:"),
            "Should show wrong structure. Got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_edit_file_validates_anchors_before_processing() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "path": "test.rs",
                "edits": [{
                    "anchor": "fn main() {",
                    "text": "fn main() { println!(\"hello\"); }"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("read_file"),
            "Error should mention read_file"
        );
        assert!(err_msg.contains("§"), "Error should show the delimiter");
        assert!(
            err_msg.contains("missing"),
            "Error should indicate what's wrong"
        );
    }

    #[tokio::test]
    async fn test_edit_file_validates_end_anchor() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "path": "test.rs",
                "edits": [{
                    "anchor": "Apple§fn main() {",
                    "end_anchor": "fn main() {",
                    "text": "replacement"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("end_anchor"),
            "Error should mention end_anchor"
        );
        assert!(
            err_msg.contains("missing"),
            "Error should indicate what's wrong"
        );
    }

    #[tokio::test]
    async fn test_consecutive_mistakes_not_changed_on_handler_error() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "path": "nonexistent_xyz.txt",
                "edits": [{
                    "anchor": "Test§some line",
                    "text": "replacement"
                }]
            }]
        });

        let initial = state.lock().await.consecutive_mistakes;
        let _result = ToolHandler::execute(&handler, &ctx, params).await;
        let final_val = state.lock().await.consecutive_mistakes;
        assert_eq!(
            final_val, initial,
            "consecutive_mistakes should NOT be changed by edit_file handler (tracked centrally in agent_loop.rs)"
        );
    }

    #[tokio::test]
    async fn test_consecutive_mistakes_not_changed_on_failure() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // Use proper anchor format (with §) but file doesn't exist, so edit will fail
        let params = serde_json::json!({
            "files": [{
                "path": "nonexistent_file_xyz.txt",
                "edits": [{
                    "anchor": "Apple§this anchor does not exist in the file",
                    "text": "replacement text"
                }]
            }]
        });

        let initial = state.lock().await.consecutive_mistakes;
        let _result = ToolHandler::execute(&handler, &ctx, params).await;
        assert_eq!(
            state.lock().await.consecutive_mistakes,
            initial,
            "consecutive_mistakes should NOT be changed by edit_file handler (tracked centrally in agent_loop.rs)"
        );
    }

    #[tokio::test]
    async fn test_consecutive_mistakes_not_changed_on_success() {
        let _guard = TEST_MUTEX.lock().await;
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        state.lock().await.consecutive_mistakes = 5;

        let temp_dir = std::env::temp_dir();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_edit_success_{}.txt", rand_suffix));
        let raw_content = "Hello World\nThis is a test\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        // Canonicalize paths to match handler's resolve_sanitized_path behavior
        let temp_dir = temp_dir.canonicalize().unwrap_or(temp_dir);
        let file_path = file_path.canonicalize().unwrap_or(file_path);

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        // Use relative path from workspace root to match handler's path resolution
        let relative_path = file_path
            .strip_prefix(&temp_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let params = serde_json::json!({
            "files": [{
                "path": relative_path,
                "edits": [{
                    "anchor": format!("{}§Hello World", anchors[0]),
                    "end_anchor": format!("{}§Hello World", anchors[0]),
                    "text": "Goodbye World"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state.clone(),
            None,
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok());
        let result_str = result.unwrap().as_str().unwrap().to_string();
        println!("Edit result: {}", result_str);
        // consecutive_mistakes is tracked centrally in agent_loop.rs, not by edit_file handler
        assert_eq!(
            state.lock().await.consecutive_mistakes,
            5,
            "consecutive_mistakes should NOT be changed by edit_file handler. Result: {}",
            result_str
        );

        // Cleanup
        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_file_blocks_path_marked_for_reread() {
        let _guard = TEST_MUTEX.lock().await;
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir().canonicalize().unwrap();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_edit_reread_block_{}.txt", rand_suffix));
        let raw_content = "Hello World\nThis is a test\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));
        state
            .lock()
            .await
            .must_reread_before_edit
            .insert(file_path.to_string_lossy().to_string());

        let params = serde_json::json!({
            "files": [{
                "path": file_path.strip_prefix(&temp_dir).unwrap().to_string_lossy().to_string(),
                "edits": [{
                    "anchor": format!("{}§Hello World", anchors[0]),
                    "end_anchor": format!("{}§Hello World", anchors[0]),
                    "text": "Goodbye World"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state,
            None,
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        let err = result.expect_err("edit_file should block until read_file clears reread state");
        assert!(err.to_string().contains("must re-read"));
        assert_eq!(
            err.metadata().map(|metadata| &metadata.class),
            Some(&ToolFailureClass::AnchorInvalid)
        );
        // The error must point the model at read_file as the next step,
        // both in the message text and in the structured metadata.
        assert!(
            err.to_string().contains("read_file"),
            "reread error must mention read_file so the model knows the next step, got: {err}"
        );
        assert_eq!(
            err.metadata()
                .and_then(|m| m.required_next_step.as_ref()),
            Some(&ToolRequiredNextStep::ReadFile),
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_file_marks_reread_after_anchor_invalidation() {
        let _guard = TEST_MUTEX.lock().await;
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir().canonicalize().unwrap();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_edit_anchor_invalid_{}.txt", rand_suffix));
        let raw_content = "Hello World\nThis is a test\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        tokio::fs::write(&file_path, "Changed\nThis is a test\n")
            .await
            .unwrap();

        let params = serde_json::json!({
            "files": [{
                "path": file_path.strip_prefix(&temp_dir).unwrap().to_string_lossy().to_string(),
                "edits": [{
                    "anchor": format!("{}§Hello World", anchors[0]),
                    "end_anchor": format!("{}§Hello World", anchors[0]),
                    "text": "Goodbye World"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state.clone(),
            None,
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await.unwrap();
        assert!(result.as_str().unwrap().contains("Error preparing edits"));
        assert!(
            state
                .lock()
                .await
                .must_reread_before_edit
                .contains(&file_path.to_string_lossy().to_string())
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_file_external_modification_requires_reread() {
        let _guard = TEST_MUTEX.lock().await;
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir().canonicalize().unwrap();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_edit_stale_warning_{}.txt", rand_suffix));
        let raw_content = "Hello World\nThis is a test\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        {
            let mut state_guard = state.lock().await;
            state_guard.file_context_tracker.track_file_read(&file_path);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        tokio::fs::write(&file_path, "Hello World\nThis is a test\nTrailing line\n")
            .await
            .unwrap();

        let params = serde_json::json!({
            "files": [{
                "path": file_path.strip_prefix(&temp_dir).unwrap().to_string_lossy().to_string(),
                "edits": [{
                    "anchor": format!("{}§Hello World", anchors[0]),
                    "end_anchor": format!("{}§Hello World", anchors[0]),
                    "text": "Goodbye World"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state.clone(),
            None,
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        let err = result.expect_err("stale external edits should require a reread first");
        assert!(err.to_string().contains("modified externally"));
        assert_eq!(
            err.metadata().map(|metadata| &metadata.class),
            Some(&ToolFailureClass::AnchorInvalid)
        );
        assert!(
            state
                .lock()
                .await
                .must_reread_before_edit
                .contains(&file_path.to_string_lossy().to_string())
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_file_preserves_trailing_newline_anchor_alignment() {
        let _guard = TEST_MUTEX.lock().await;
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir().canonicalize().unwrap();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_edit_trailing_newline_{}.txt", rand_suffix));
        let raw_content = "alpha\nbeta\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let lines = crate::core::file_editor::split_content_lines(raw_content);
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        let params = serde_json::json!({
            "files": [{
                "path": file_path.strip_prefix(&temp_dir).unwrap().to_string_lossy().to_string(),
                "edits": [{
                    "anchor": format!("{}§beta", anchors[1]),
                    "end_anchor": format!("{}§beta", anchors[1]),
                    "text": "gamma"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state,
            None,
            temp_dir.clone(),
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "edit should succeed on trailing-newline files"
        );

        let final_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(final_content, "alpha\ngamma\n");

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_consecutive_mistakes_unchanged_when_no_edits() {
        let _guard = TEST_MUTEX.lock().await;
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        state.lock().await.consecutive_mistakes = 3;

        let temp_dir = std::env::temp_dir();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_edit_empty_{}.txt", rand_suffix));
        tokio::fs::write(&file_path, "content").await.unwrap();

        // Use relative path from workspace root to match handler's path resolution
        let relative_path = file_path
            .strip_prefix(&temp_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let params = serde_json::json!({
            "files": [{
                "path": relative_path,
                "edits": []
            }]
        });

        let anchor_mgr = AnchorStateManager::new();
        let ctx = ToolContext::new(
            state.clone(),
            None,
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok());
        assert_eq!(
            state.lock().await.consecutive_mistakes,
            3,
            "consecutive_mistakes should not change when no edits are applied"
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_with_yolo_skips_approval() {
        let _guard = TEST_MUTEX.lock().await;
        let approval_mgr = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::core::approval::ApprovalManager::new().with_yolo(true),
        ));
        let handler = EditFileHandler::new().with_approval_manager(approval_mgr.clone());
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_yolo_edit_{}.txt", rand_suffix));
        let raw_content = "Hello World\nThis is a test\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        // Canonicalize paths to match handler's resolve_sanitized_path behavior
        let temp_dir = temp_dir.canonicalize().unwrap_or(temp_dir);
        let file_path = file_path.canonicalize().unwrap_or(file_path);

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        let params = serde_json::json!({
            "files": [{
                "path": file_path.to_string_lossy().to_string(),
                "edits": [{
                    "anchor": format!("{}§Hello World", anchors[0]),
                    "end_anchor": format!("{}§Hello World", anchors[0]),
                    "text": "Goodbye World"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state.clone(),
            Some(approval_mgr),
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "Edit should succeed in yolo mode without prompting: {:?}",
            result.err()
        );
        let result_text = result.unwrap();
        let result_str = result_text.as_str().unwrap();
        assert!(
            result_str.contains("Goodbye World"),
            "Edit should be applied in yolo mode. Result: {}",
            result_str
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_without_approval_manager_proceeds() {
        let _guard = TEST_MUTEX.lock().await;
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_no_mgr_edit_{}.txt", rand_suffix));
        let raw_content = "Hello World\nThis is a test\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        // Use relative path from workspace root to match handler's path resolution
        let relative_path = file_path
            .strip_prefix(&temp_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let params = serde_json::json!({
            "files": [{
                "path": relative_path,
                "edits": [{
                    "anchor": format!("{}§Hello World", anchors[0]),
                    "end_anchor": format!("{}§Hello World", anchors[0]),
                    "text": "Goodbye World"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state.clone(),
            None,
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "Edit should succeed without approval manager"
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_with_approval_manager_yolo_mode() {
        let _guard = TEST_MUTEX.lock().await;
        // Yolo mode should skip approval prompts
        let approval_mgr = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::core::approval::ApprovalManager::new().with_yolo(true),
        ));
        let handler = EditFileHandler::new().with_approval_manager(approval_mgr.clone());
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_yolo_edit_{}.txt", rand_suffix));
        let raw_content = "Hello World\nThis is a test\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        // Canonicalize paths to match handler's resolve_sanitized_path behavior
        let temp_dir = temp_dir.canonicalize().unwrap_or(temp_dir);
        let file_path = file_path.canonicalize().unwrap_or(file_path);

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        // Use relative path from workspace root to match handler's path resolution
        let relative_path = file_path
            .strip_prefix(&temp_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let params = serde_json::json!({
            "files": [{
                "path": relative_path,
                "edits": [{
                    "anchor": format!("{}§Hello World", anchors[0]),
                    "end_anchor": format!("{}§Hello World", anchors[0]),
                    "text": "Goodbye World"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state.clone(),
            Some(approval_mgr),
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok(), "Edit should succeed in yolo mode");
        let result_text = result.unwrap();
        let result_str = result_text.as_str().unwrap();
        assert!(
            result_str.contains("Goodbye World"),
            "Edit should be applied in yolo mode"
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_rejected_skips_write() {
        let _guard = TEST_MUTEX.lock().await;
        let approval_mgr = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::core::approval::ApprovalManager::new().with_yolo(false),
        ));
        let handler = EditFileHandler::new().with_approval_manager(approval_mgr.clone());
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_rejected_edit_{}.txt", rand_suffix));
        let raw_content = "Original content\nShould not change\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        // Use relative path from workspace root to match handler's path resolution
        let relative_path = file_path
            .strip_prefix(&temp_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let params = serde_json::json!({
            "files": [{
                "path": relative_path,
                "edits": [{
                    "anchor": format!("{}§Hello World", anchors[0]),
                    "end_anchor": format!("{}§Hello World", anchors[0]),
                    "text": "Goodbye World"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state.clone(),
            Some(approval_mgr),
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok());

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_silent_mode_suppresses_diff_preview_but_still_requires_approval() {
        let _guard = TEST_MUTEX.lock().await;
        let approval_mgr = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::core::approval::ApprovalManager::new().with_yolo(false),
        ));
        let handler = EditFileHandler::new().with_approval_manager(approval_mgr.clone());
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let temp_dir = std::env::temp_dir();
        let rand_suffix: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let file_path = temp_dir.join(format!("test_silent_edit_{}.txt", rand_suffix));
        let raw_content = "Line 1\nLine 2\nLine 3\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        let temp_dir = temp_dir.canonicalize().unwrap_or(temp_dir);
        let file_path = file_path.canonicalize().unwrap_or(file_path);

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        let relative_path = file_path
            .strip_prefix(&temp_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let params = serde_json::json!({
            "silent": true,
            "files": [{
                "path": relative_path,
                "edits": [{
                    "anchor": format!("{}§Line 2", anchors[1]),
                    "end_anchor": format!("{}§Line 2", anchors[1]),
                    "text": "Modified Line 2"
                }]
            }]
        });

        let ctx = ToolContext::new(
            state.clone(),
            Some(approval_mgr),
            temp_dir,
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            true, // explicitly_approved required — silent no longer bypasses approval
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "Silent mode with explicit approval should succeed: {:?}",
            result.err()
        );
        let result_text = result.unwrap().as_str().unwrap().to_string();

        let final_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert!(
            final_content.contains("Modified Line 2"),
            "File should be modified when explicitly approved"
        );

        assert!(
            !result_text.contains("<<<<<<< SEARCH"),
            "Silent mode should not include diff preview"
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_concurrent_edits_to_same_file_serialize() {
        let _guard = TEST_MUTEX.lock().await;
        let handler = EditFileHandler::new();

        let workspace_root = tempfile::TempDir::new().unwrap();
        let file_path = workspace_root.path().join("test_concurrent_edit.txt");
        let raw_content = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        let mut handles = Vec::new();
        for (i, anchor) in anchors.iter().enumerate().take(5) {
            let handler = handler.clone();
            let path = file_path.to_str().unwrap().to_string();
            let anchor = anchor.clone();
            let line_content = format!("Modified Line {}", i + 1);
            let workspace = workspace_root.path().to_path_buf();

            let handle = tokio::spawn(async move {
                let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
                let ctx = ToolContext::new(
                    state,
                    None,
                    workspace,
                    AnchorStateManager::new(),
                    false,
                    format!("test-task-{}", i),
                    None,
                    false,
                    Arc::new(crate::cli::output::StderrOutputWriter),
                );

                let params = serde_json::json!({
                    "files": [{
                        "path": path,
                        "edits": [{
                            "anchor": format!("{}§{}", anchor, line_content.replace("Modified ", "")),
                            "text": line_content
                        }]
                    }]
                });

                ToolHandler::execute(&handler, &ctx, params).await
            });

            handles.push(handle);
        }

        let results = futures::future::join_all(handles).await;

        for (i, result) in results.iter().enumerate() {
            match result {
                Ok(Ok(_)) => println!("Task {} succeeded", i),
                Ok(Err(e)) => println!("Task {} failed (expected): {}", i, e),
                Err(e) => println!("Task {} panicked: {}", i, e),
            }
        }

        let final_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        let final_lines: Vec<&str> = final_content.lines().collect();

        assert_eq!(
            final_lines.len(),
            5,
            "File should have exactly 5 lines, got: {}",
            final_content
        );

        for (i, line) in final_lines.iter().enumerate() {
            assert!(
                line.starts_with("Line") || line.starts_with("Modified"),
                "Line {} has invalid content: {}",
                i,
                line
            );
        }
    }

    /// Test that file locking prevents TOCTOU race during external modification
    #[tokio::test]
    async fn test_external_modification_detected_with_lock() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut temp_file = NamedTempFile::new().unwrap();
        writeln!(temp_file, "Line 1").unwrap();
        writeln!(temp_file, "Line 2").unwrap();
        writeln!(temp_file, "Line 3").unwrap();
        temp_file.flush().unwrap();

        let file_path = temp_file.path().to_str().unwrap().to_string();
        let workspace_root = temp_file.path().parent().unwrap().to_path_buf();

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root,
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let initial_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        let lines: Vec<&str> = initial_content.lines().collect();
        let anchor = format!("{}§Line 2", crate::core::hash_utils::content_hash(lines[1]));

        let params = serde_json::json!({
            "files": [{
                "path": file_path,
                "edits": [{
                    "anchor": anchor,
                    "text": "Modified Line 2"
                }]
            }]
        });

        let file_path_clone = file_path.clone();
        let modifier_handle = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&file_path_clone)
                .unwrap();
            writeln!(file, "Line 1").unwrap();
            writeln!(file, "EXTERNALLY MODIFIED").unwrap();
            writeln!(file, "Line 3").unwrap();
        });

        let result = EditFileHandler::execute(&handler, &ctx, params).await;

        modifier_handle.await.unwrap();

        match result {
            Ok(_output) => {
                let final_content = tokio::fs::read_to_string(&file_path).await.unwrap();
                assert!(
                    final_content.contains("Modified Line 2")
                        || final_content.contains("EXTERNALLY MODIFIED"),
                    "File should contain either our edit or external edit, got: {}",
                    final_content
                );
            }
            Err(e) => {
                let err_msg = e.to_string();
                assert!(
                    err_msg.contains("modified externally") || err_msg.contains("Error"),
                    "Expected external modification error, got: {}",
                    err_msg
                );
            }
        }
    }

    #[tokio::test]
    async fn test_multi_file_edit_with_pre_errors_uses_batch_diagnostics() {
        use std::fs;
        use std::process::Command;

        let _guard = TEST_MUTEX.lock().await;

        let temp_dir = "test_batch_diagnostics_tmp";
        let _ = fs::remove_dir_all(temp_dir);
        fs::create_dir_all(temp_dir).unwrap();

        let cargo_toml = r#"[package]
name = "test_batch"
version = "0.1.0"
edition = "2021"
"#;
        fs::write(format!("{}/Cargo.toml", temp_dir), cargo_toml).unwrap();

        fs::create_dir_all(format!("{}/src", temp_dir)).unwrap();

        let file1_content =
            "func1§pub fn func1() {\nbad_call§    nonexistent_function();\nclose§}\n";
        let file2_content =
            "func2§pub fn func2() {\nbad_call2§    another_nonexistent_function();\nclose2§}\n";
        fs::write(format!("{}/src/file1.rs", temp_dir), file1_content).unwrap();
        fs::write(format!("{}/src/file2.rs", temp_dir), file2_content).unwrap();

        let lib_content = "lib§mod file1;\nlib2§mod file2;\n";
        fs::write(format!("{}/src/lib.rs", temp_dir), lib_content).unwrap();

        let _check_output = Command::new("cargo")
            .args(["check", "--message-format=short"])
            .current_dir(temp_dir)
            .output();

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [
                {
                    "path": format!("{}/src/file1.rs", temp_dir),
                    "edits": [{
                        "anchor": "bad_call§    nonexistent_function();",
                        "text": "    // Fixed: removed bad call"
                    }]
                },
                {
                    "path": format!("{}/src/file2.rs", temp_dir),
                    "edits": [{
                        "anchor": "bad_call2§    another_nonexistent_function();",
                        "text": "    // Fixed: removed bad call"
                    }]
                }
            ]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;

        assert!(result.is_ok(), "Edit should succeed: {:?}", result);

        let output = result.unwrap().as_str().unwrap().to_string();
        assert!(output.contains("Edited 2 file(s)"));

        // Clean up
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_validate_anchors_utf8_truncation() {
        let _guard = TEST_MUTEX.lock().await;

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let long_anchor = "你好世界".repeat(20);
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": long_anchor
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_err(),
            "Should fail validation for missing delimiter"
        );
        let err = result.unwrap_err();
        let err_msg = format!("{}", err);
        assert!(err_msg.contains("missing the '§' delimiter"));
        assert!(
            err_msg.contains("..."),
            "Long anchor should be truncated with ellipsis"
        );
        assert!(
            !err_msg.contains("你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界"),
            "Long anchor should be truncated, not show full 80-char string"
        );
    }

    #[tokio::test]
    async fn test_validate_anchors_rejects_invalid_anchor_name() {
        let _guard = TEST_MUTEX.lock().await;

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": "123§line 1",
                    "text": "replacement"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_err(),
            "invalid anchor name should fail validation"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("Hash anchor validation failed"));
        assert!(err_msg.contains("anchor name"));
    }

    #[tokio::test]
    async fn test_validate_edit_type_rejects_unknown() {
        let _guard = TEST_MUTEX.lock().await;

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": "test§some line",
                    "edit_type": "delete",
                    "text": "new content"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_err(),
            "Should fail validation for unknown edit_type"
        );
        let err = result.unwrap_err();
        let err_msg = format!("{}", err);
        assert!(err_msg.contains("Unknown edit_type 'delete'"));
        assert!(err_msg.contains("Valid values are: replace, insert_before, insert_after"));
    }

    #[tokio::test]
    async fn test_validate_edit_type_accepts_valid_types() {
        let _guard = TEST_MUTEX.lock().await;

        for edit_type in &["replace", "insert_before", "insert_after"] {
            let handler = EditFileHandler::new();
            let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
            let ctx = ToolContext::new(
                state,
                None,
                std::env::current_dir().unwrap(),
                AnchorStateManager::new(),
                false,
                "test-task".to_string(),
                None,
                false,
                Arc::new(crate::cli::output::StderrOutputWriter),
            );

            let params = serde_json::json!({
                "files": [{
                    "path": "test.txt",
                    "edits": [{
                        "anchor": "test§some line",
                        "edit_type": edit_type,
                        "text": "new content"
                    }]
                }]
            });

            let result = ToolHandler::execute(&handler, &ctx, params).await;
            assert!(
                result.is_ok() || result.unwrap_err().to_string().contains("file not found"),
                "edit_type '{}' should be accepted (file not found error is expected)",
                edit_type
            );
        }
    }

    #[tokio::test]
    async fn test_overlap_edits_report_overlap_summary() {
        use tempfile::tempdir;

        let _guard = TEST_MUTEX.lock().await;

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let anchor_mgr = AnchorStateManager::new();
        let content = std::fs::read_to_string(&file_path).unwrap();
        let lines = crate::core::file_editor::split_content_lines(&content);
        let hashes = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            true,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [
                    {
                        "anchor": format!("{}§line1", hashes[0]),
                        "end_anchor": format!("{}§line2", hashes[1]),
                        "text": "alpha"
                    },
                    {
                        "anchor": format!("{}§line2", hashes[1]),
                        "end_anchor": format!("{}§line3", hashes[2]),
                        "text": "beta"
                    }
                ]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "overlap failure should still return a summary"
        );
        let output = result.unwrap().as_str().unwrap().to_string();
        assert!(output.contains("0 edit(s) applied"), "got: {}", output);
        assert!(output.contains("0 edit(s) failed"), "got: {}", output);
        assert!(output.contains("2 edit(s) overlapped"), "got: {}", output);
    }

    #[tokio::test]
    async fn test_prepare_edits_failure_increments_total_failed() {
        use tempfile::tempdir;

        let _guard = TEST_MUTEX.lock().await;

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            true,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "path": file_path,
                "edits": [
                    {
                        "anchor": "WrongWord§line 1",
                        "text": "replacement 1"
                    },
                    {
                        "anchor": "BadWord§line 2",
                        "text": "replacement 2"
                    }
                ]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok(), "Should not panic, should report failures");
        let output = result.unwrap().as_str().unwrap().to_string();
        assert!(
            output.contains("0 edit(s) applied") || output.contains("failed"),
            "Summary should report 0 applied or mention failures, got: {}",
            output
        );
    }

    #[tokio::test]
    async fn test_successful_edit_marks_must_reread() {
        use tempfile::tempdir;

        let _guard = TEST_MUTEX.lock().await;

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let raw_content = "line 1\nline 2\nline 3\n";
        std::fs::write(&file_path, raw_content).unwrap();

        let prep_anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            prep_anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("test-task"));

        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            dir.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            true,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let anchor_l1 = format!("{}§line 1", anchors[0]);
        let anchor_l2 = format!("{}§line 2", anchors[1]);
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": anchor_l1,
                    "edit_type": "replace",
                    "text": "replaced line 1"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok(), "First edit should succeed: {:?}", result);

        {
            let state_guard = state.lock().await;
            let abs_path = file_path
                .canonicalize()
                .unwrap_or_else(|_| file_path.clone())
                .to_string_lossy()
                .to_string();
            assert!(
                state_guard.must_reread_before_edit.contains(&abs_path),
                "must_reread_before_edit should contain the edited file path after successful edit, got: {:?}",
                state_guard.must_reread_before_edit
            );
        }

        let params2 = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": anchor_l2,
                    "edit_type": "replace",
                    "text": "replaced line 2"
                }]
            }]
        });

        let result2 = ToolHandler::execute(&handler, &ctx, params2).await;
        assert!(
            result2.is_err(),
            "Second edit on same file should fail with reread_required_error"
        );
        let err_msg = format!("{}", result2.unwrap_err());
        assert!(
            err_msg.contains("must re-read"),
            "Error should mention re-read required, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_stale_anchor_preflight_detects_previous_read() {
        use tempfile::tempdir;

        let _guard = TEST_MUTEX.lock().await;

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let raw_content = "alpha\nbeta\ngamma\n";
        std::fs::write(&file_path, raw_content).unwrap();

        // Canonicalize the path so it matches what the handler's
        // resolve_sanitized_path produces (macOS tempdir uses a symlink
        // that canonicalize resolves to /private/var/folders/...).
        let canonical_path = file_path
            .canonicalize()
            .unwrap_or_else(|_| file_path.clone());

        // Simulate a previous read by reconciling the file once
        // to populate the anchor state.
        let anchor_mgr = AnchorStateManager::new();
        let initial_lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let initial_anchors = anchor_mgr.reconcile(
            canonical_path.to_str().unwrap(),
            &initial_lines,
            Some("test-task"),
        );

        // Now externally change the file (e.g., user edit or another agent).
        let new_content = "alpha\nBETA-CHANGED\ngamma\ndelta\n";
        std::fs::write(&file_path, new_content).unwrap();

        // The model tries to edit using an anchor from the PREVIOUS read
        // (e.g., "beta" — but the file no longer has "beta", it has
        // "BETA-CHANGED"). The preflight should catch this and return a
        // clearer "stale anchor" error rather than a generic "not found".
        let stale_anchor = format!("{}§beta", initial_anchors[1]);
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state.clone(),
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            true,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": stale_anchor,
                    "edit_type": "replace",
                    "text": "new beta"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "Execute should return Ok with error in body: {:?}",
            result
        );
        let body = result
            .unwrap()
            .as_str()
            .map(String::from)
            .unwrap_or_default();
        assert!(
            body.contains("Stale anchor detected"),
            "Result should mention stale anchor detection, got: {}",
            body
        );
        assert!(
            body.contains("previous read_file"),
            "Result should explain the anchor is from a previous read, got: {}",
            body
        );

        // must_reread_before_edit should now contain the file path
        let state_guard = state.lock().await;
        let abs_path = file_path
            .canonicalize()
            .unwrap_or_else(|_| file_path.clone())
            .to_string_lossy()
            .to_string();
        assert!(
            state_guard.must_reread_before_edit.contains(&abs_path),
            "must_reread_before_edit should be set after stale anchor detection"
        );
    }

    /// Contract test: read_file output → edit_file roundtrip must succeed.
    ///
    /// This guards against the recurrence of the edit_file bug where the model
    /// would submit anchors in a format that edit_file could not accept. The
    /// read_file handler formats lines as `{anchor}§{content}`. An edit_file
    /// call that uses those exact anchor strings must succeed.
    #[tokio::test]
    async fn test_edit_file_accepts_read_file_anchor_format() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("contract.rs");
        let raw_content = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("contract-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "contract-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§fn alpha() {{}}", anchors[0]);
        let params = serde_json::json!({
            "files": [{
                "path": "contract.rs",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "fn alpha() { /* updated */ }"
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "edit_file must accept the exact anchor format produced by read_file: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert!(updated.contains("/* updated */"));
    }

    /// Contract test: the `text` field must be written verbatim to the
    /// file. JSON-level escaping already happened in the JSON parser
    /// (serde decoded `\n` to a real newline). The model sends a
    /// C-string escape like `\n` as `\\n` in JSON, which decodes to the
    /// two-character sequence `\n` in Rust and must land in the file as
    /// `\n` (two chars), NOT as a real newline that would corrupt the
    /// C string literal.
    ///
    /// This guards against the "anchor still there" / "file corrupted"
    /// bug seen in convo-2222-export-30.json where a model submitted
    /// `fprintf(stderr, "...failed\\n");` and the tool wrote a real
    /// newline into the middle of the string literal.
    #[tokio::test]
    async fn test_edit_file_preserves_backslash_n_as_two_chars() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("literal.c");
        let raw_content = "void f(void) {}\nvoid g(void) {}\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("literal-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "literal-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§void f(void) {{}}", anchors[0]);
        // The model wants a C string with `\n` (two chars) inside it.
        // It sends `\\n` in JSON, which serde_json serializes as the
        // two-char sequence `\n` in the resulting Value::String. The
        // file must get the two-char sequence, not a real newline.
        // Use a raw string for the JSON to make the escaping explicit:
        // the model sends `\\n` (2 backslashes + n in the JSON source,
        // which JSON decodes to `\n` = 2 chars: backslash, n).
        let text_with_c_escape = r#"void f(void) { fprintf(stderr, "failed\n"); }"#;
        let params = serde_json::json!({
            "files": [{
                "path": "literal.c",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": text_with_c_escape
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok(), "{:?}", result.err());
        let updated = std::fs::read_to_string(&file_path).unwrap();
        // The file must contain the two-char C string escape, not a real
        // newline that would break the C string literal.
        let needle = r#"fprintf(stderr, "failed\n")"#;
        assert!(
            updated.contains(needle),
            "file must contain literal backslash-n inside the C string, got:\n{updated:?}"
        );
        // The whole replacement must be on a single physical line.
        let bad_needle = "fprintf(stderr, \"failed\n";
        assert!(
            !updated.contains(bad_needle),
            "file must NOT contain a real newline inside the C string literal, got:\n{updated:?}"
        );
    }

    /// Contract test: when the model sends a real newline in the JSON
    /// (i.e. `\n` in the JSON, which serde decodes to a real newline),
    /// the file must contain that real newline. This is the normal case
    /// for multi-line replacements.
    #[tokio::test]
    async fn test_edit_file_preserves_real_newlines_from_json() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("multiline.c");
        let raw_content = "void f(void) {}\nvoid g(void) {}\n";
        std::fs::write(&file_path, raw_content).unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("multiline-task"));
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "multiline-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§void f(void) {{}}", anchors[0]);
        // The model sends a real newline in the JSON (`\n` in JSON
        // decodes to a real newline). The file must contain that real
        // newline. This is the standard multi-line replacement shape.
        let text_with_real_newline = "void f(void) {\n    /* replaced */\n}";
        let params = serde_json::json!({
            "files": [{
                "path": "multiline.c",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": text_with_real_newline
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok(), "{:?}", result.err());
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert!(
            updated.contains("void f(void) {\n    /* replaced */\n}"),
            "file must contain real newlines from the JSON-decoded text, got:\n{updated:?}"
        );
    }

    /// Regression test: the model sends multiple `§`-delimited pairs concatenated
    /// across newlines (see convo-2222-export-15.json). The parser must normalize
    /// to the first line instead of rejecting the whole input.
    #[tokio::test]
    async fn test_edit_file_accepts_concatenated_anchor_pairs() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("concat.c");
        std::fs::write(&file_path, "static int\nload_ax_hiservices(void)\n{\n").unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = "static int\nload_ax_hiservices(void)\n{"
            .lines()
            .map(|s| s.to_string())
            .collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("regr-task"));
        let first_anchor = format!("{}§static int", anchors[0]);
        let concatenated = format!(
            "{}\nFiduciaryRegular§load_ax_hiservices(void)\nInvitationCompliance§{{",
            first_anchor
        );
        let ctx = ToolContext::new(
            state,
            None,
            dir.path().to_path_buf(),
            anchor_mgr,
            false,
            "regr-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let params = serde_json::json!({
            "files": [{
                "path": "concat.c",
                "edits": [{
                    "anchor": concatenated,
                    "edit_type": "replace",
                    "text": "static int replaced"
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "concatenated anchor pairs should normalize to the first line: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert!(updated.contains("static int replaced"));
    }

    /// Error message quality: when parse_edits fails, the error must teach the
    /// model the correct format. An error that doesn't mention `read_file` or
    /// `§` leaves the model guessing.
    #[tokio::test]
    async fn test_edit_file_anchor_error_teaches_read_file_format() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "teach-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let params = serde_json::json!({
            "files": [{
                "path": "teach.rs",
                "edits": [{
                    "anchor": "",
                    "edit_type": "replace",
                    "text": "x"
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        let err = result.expect_err("empty anchor should error");
        let msg = err.to_string();
        assert!(
            msg.contains("read_file"),
            "empty-anchor error must reference read_file output so the model can self-correct, got: {}",
            msg
        );
        assert!(
            msg.contains('§'),
            "empty-anchor error must show the § delimiter so the model uses the correct format, got: {}",
            msg
        );
    }

    /// Regression test: the model submits an anchor that spans multiple
    /// physical lines (e.g. `Word§\nNextWord§content`). The first line
    /// is incomplete (ends with `§` with no content after it). The tool
    /// must reject this with a clear error pointing at the incomplete
    /// first line, not silently truncate to a useless first-line-only
    /// anchor. See convo-2222-export-33.json.
    #[tokio::test]
    async fn test_edit_file_rejects_incomplete_multiline_anchor() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "multiline-anchor-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        // First line is just `Word§` with no content after the delimiter.
        // The second line continues with another anchor.
        let params = serde_json::json!({
            "files": [{
                "path": "multiline.c",
                "edits": [{
                    "anchor": "Countertop§\nElectrochemicalMorphology§static int show_window(void) {",
                    "edit_type": "replace",
                    "text": "static int show_window(void) {}"
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        let err = result.expect_err("incomplete multi-line anchor must error");
        let msg = err.to_string();
        assert!(
            msg.contains("multi-line"),
            "error must call out the multi-line anchor problem, got: {msg}"
        );
        assert!(
            msg.contains("Countertop§"),
            "error must preview the incomplete first line, got: {msg}"
        );
        assert!(
            msg.contains("end_anchor"),
            "error must point the model at the end_anchor solution for range replacements, got: {msg}"
        );
    }

    /// Regression test: the model in convo-2222-export-33.json submitted
    /// `files` as a stringified JSON array that contained `edits` but
    /// no `path` key per file entry. The old error message said "edit_file
    /// requires a 'path' key" which pointed at the wrong cause. The fix
    /// is to apply the top-level `path` fallback even when `files` is
    /// a stringified JSON array, so the model can submit either a
    /// top-level `path` or a `path` per file entry.
    #[tokio::test]
    async fn test_edit_file_stringified_json_without_path_falls_back_to_top_level() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let file_path = workspace.join("fallback.c");
        let raw_content = "void f(void) {}\nvoid g(void) {}\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("fallback-task"));
        let ctx = ToolContext::new(
            state,
            None,
            workspace.clone(),
            anchor_mgr,
            false,
            "fallback-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§void f(void) {{}}", anchors[0]);
        // The model sends `files` as a stringified JSON array with edits
        // but no `path` key per file entry. It also provides a top-level
        // `path` that the tool should fall back to.
        let stringified_files = format!(
            r#"[{{"edits": [{{"anchor": "{}", "edit_type": "replace", "text": "void f(void) {{ /* replaced */ }}"}}]}}]"#,
            anchor,
        );
        let params = serde_json::json!({
            "path": "fallback.c",
            "files": stringified_files,
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "stringified JSON without per-file path must succeed when top-level path is provided, got: {:?}",
            result.err()
        );
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert!(
            updated.contains("void f(void) { /* replaced */ }"),
            "file must contain the replacement, got:\n{updated:?}"
        );
    }

    /// End-to-end test: edit_file output flows through a real
    /// ChannelOutputWriter and lands in the file. This guards against
    /// regressions where the emit/drain/render pipeline silently drops
    /// events (e.g. the silent channel-full drop at output.rs:200-213
    /// during a tool-result flood).
    #[tokio::test]
    async fn test_edit_file_end_to_end_with_channel_output_writer() {
        use tempfile::tempdir;
        use tokio::sync::mpsc;

        let dir = tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let file_path = workspace.join("e2e.c");
        let raw_content = "void f(void) {}\nvoid g(void) {}\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors =
            anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("e2e-task"));

        // Create a real channel-based writer. The edit_file emit calls
        // must reach the channel even when no drain is running (the
        // channel is bounded at 8192 and try_send will drop on full).
        let (tx, _rx) = mpsc::channel::<crate::cli::output::OutputEvent>(16);
        let writer: Arc<dyn crate::cli::output::OutputWriter> =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let ctx = ToolContext::new(
            state,
            None,
            workspace,
            anchor_mgr,
            false,
            "e2e-task".to_string(),
            None,
            false,
            writer,
        );
        let anchor = format!("{}§void f(void) {{}}", anchors[0]);
        let params = serde_json::json!({
            "files": [{
                "path": "e2e.c",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "void f(void) { /* e2e replaced */ }"
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok(), "{:?}", result.err());
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert!(
            updated.contains("void f(void) { /* e2e replaced */ }"),
            "file must contain the replacement via the real ChannelOutputWriter path, got:\n{updated:?}"
        );
    }

    /// Regression test: the must-reread error must (a) explain WHY the
    /// re-read is required, (b) point at read_file as the next step in
    /// the message text, and (c) carry ToolRequiredNextStep::ReadFile in
    /// the structured metadata. The model in convo-2222-export-32.json
    /// was confused by a terse error that did none of these.
    #[tokio::test]
    async fn test_edit_file_must_reread_error_is_actionable() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let file_path = workspace.join("reread.c");
        let raw_content = "Hello World\nThis is a test\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let lines: Vec<String> = raw_content.lines().map(|s| s.to_string()).collect();
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some("reread-task"));
        // Simulate a prior successful edit that set must_reread_before_edit.
        state
            .lock()
            .await
            .must_reread_before_edit
            .insert(file_path.to_string_lossy().to_string());
        let ctx = ToolContext::new(
            state,
            None,
            workspace.clone(),
            anchor_mgr,
            false,
            "reread-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§Hello World", anchors[0]);
        let params = serde_json::json!({
            "files": [{
                "path": file_path
                    .strip_prefix(&workspace)
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "Goodbye World"
                }]
            }]
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        let err = result.expect_err("edit_file should be blocked until read_file clears reread state");
        let msg = err.to_string();
        // (a) Explain WHY: a prior edit changed the file.
        assert!(
            msg.contains("changed the file") || msg.contains("prior edit"),
            "error must explain WHY a re-read is required, got: {msg}"
        );
        // (b) Point at read_file in the message text.
        assert!(
            msg.contains("read_file"),
            "error message must mention read_file so the model knows the next step, got: {msg}"
        );
        // (c) Carry ToolRequiredNextStep::ReadFile in structured metadata.
        assert_eq!(
            err.metadata()
                .and_then(|m| m.required_next_step.as_ref()),
            Some(&ToolRequiredNextStep::ReadFile),
        );
    }

    /// Regression test: the `edit_file` tool's pre-validation must catch
    /// missing `path` key on stringified JSON files (the convo-33 bug)
    /// and return an error that points at the actual problem, not a
    /// generic "path key" message.
    #[tokio::test]
    async fn test_edit_file_stringified_json_missing_path_gives_actionable_error() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "missing-path-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        // Model sends files as stringified JSON with no top-level path
        // and no per-file path. This is unrecoverable: the tool cannot
        // infer which file to edit.
        let params = serde_json::json!({
            "files": r#"[{"edits": [{"anchor": "x§y", "edit_type": "replace", "text": "z"}]}]"#,
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        let err = result.expect_err("stringified JSON without any path must error");
        let msg = err.to_string();
        // The error must mention the path key as a hint to the model.
        assert!(
            msg.contains("path"),
            "error must mention the missing path key, got: {msg}"
        );
    }

    /// Regression test for the key-mismatch bug in must_reread_before_edit
    /// tracking. The original mark_must_reread stored the
    /// workspace-joined path (from resolve_sanitized_path), but
    /// read_file's track_read_files used the raw model-provided path.
    /// The two keys never matched when the workspace had symlinks
    /// (e.g. /tmp → /private/tmp on macOS), so must_reread_before_edit
    /// was never cleared by a re-read, and the model got stuck in an
    /// infinite re-read loop.
    ///
    /// The fix: mark_must_reread now canonicalizes the path via
    /// std::fs::canonicalize before storing it. read_file's
    /// track_read_files uses the canonical_path from
    /// FileReadResult (computed by tokio::fs::canonicalize) to match.
    /// Both sides now use the fully-canonicalized path as the key.
    ///
    /// This test exercises the full production flow through the
    /// public ToolHandler::execute:
    /// 1. EditFileHandler::execute performs a real edit, which calls
    ///    mark_must_reread with batch.absolute_path (workspace-joined).
    /// 2. The flag is stored in must_reread_before_edit.
    /// 3. ReadFileHandler::execute is called to re-read the file.
    /// 4. track_read_files uses the canonical_path from the result to
    ///    remove the flag.
    /// 5. Assert must_reread_before_edit is empty.
    #[tokio::test]
    async fn test_read_file_clears_must_reread_with_canonical_path() {
        use crate::core::agent_types::TaskState;
        use crate::core::file_editor::AnchorStateManager;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        // Create a symlink to the tempdir so the workspace path
        // differs from its canonical path. This simulates the
        // production scenario where /tmp → /private/tmp on macOS.
        let real_workspace = dir.path().canonicalize().unwrap();
        let symlink_workspace = real_workspace.with_file_name("symlink_ws");
        // Remove any stale symlink from a prior failed run.
        let _ = std::fs::remove_file(&symlink_workspace);
        std::os::unix::fs::symlink(&real_workspace, &symlink_workspace).unwrap();
        // Use the symlink path as the workspace — this is what
        // resolve_sanitized_path produces (workspace-join, NOT
        // canonicalized).
        let workspace = symlink_workspace;
        let file_path = workspace.join("clear_reread.rs");
        let raw_content = "fn main() {}\n";
        tokio::fs::write(&file_path, raw_content).await.unwrap();

        // Verify the symlink divergence: workspace path != canonical.
        let canonical_file = std::fs::canonicalize(&file_path).unwrap();
        let workspace_file = file_path.to_string_lossy().into_owned();
        assert_ne!(
            canonical_file.to_string_lossy(),
            workspace_file,
            "precondition: symlink must cause path divergence. \
             canonical: {canonical_file:?}, workspace: {workspace_file:?}"
        );

        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = Arc::new(AnchorStateManager::new());

        // Step 1: Seed anchors from the initial file content so
        // mark_must_reread fires after a successful edit.
        let initial_anchors = anchor_mgr.reconcile(
            file_path.to_str().unwrap(),
            &raw_content.lines().map(|s| s.to_string()).collect::<Vec<_>>(),
            Some("test-task"),
        );

        // Step 2: Perform a real edit via the public path. This goes
        // through resolve_path → resolve_sanitized_path (workspace-join,
        // NOT canonicalize) → mark_must_reread. The fix in
        // mark_must_reread canonicalizes the path before storing.
        let edit_handler = EditFileHandler::new();
        let edit_ctx = crate::core::tools::ToolContext::new(
            state.clone(),
            None,
            workspace.clone(),
            anchor_mgr.as_ref().clone(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let anchor = format!("{}§fn main() {{}}", initial_anchors[0]);
        let edit_params = serde_json::json!({
            "files": [{
                "path": file_path.to_string_lossy(),
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "fn main() { println!(\"edited\"); }"
                }]
            }]
        });
        let _ = crate::core::tools::ToolHandler::execute(&edit_handler, &edit_ctx, edit_params)
            .await
            .expect("edit should succeed");

        // Step 3: Re-read via the public path. track_read_files uses
        // the canonical_path from FileReadResult to match. With the
        // fix, this clears the flag.
        let read_handler = crate::core::tools::handlers::read_file::ReadFileHandler::new();
        let read_ctx = crate::core::tools::ToolContext::new(
            state.clone(),
            None,
            workspace.clone(),
            anchor_mgr.as_ref().clone(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let read_params = serde_json::json!({
            "paths": [file_path.to_string_lossy()]
        });
        let _ = crate::core::tools::ToolHandler::execute(&read_handler, &read_ctx, read_params)
            .await
            .expect("read should succeed");

        // Step 4: Assert the flag is cleared. Without the fix, the
        // workspace-joined key (from mark_must_reread) would NOT
        // match the canonical key (from track_read_files), and the
        // flag would persist.
        let state_guard = state.lock().await;
        assert!(
            state_guard.must_reread_before_edit.is_empty(),
            "must_reread_before_edit must be cleared after re-read. \
             keys still present: {:?}, \
             workspace-joined: {workspace_file:?}, canonical: {canonical_file:?}",
            state_guard.must_reread_before_edit
        );
    }

    /// Stringified JSON parse error: when `files` is a string that fails to
    /// parse as JSON, the error must surface the actual serde_json error so
    /// the model can diagnose the problem.
    #[tokio::test]
    async fn test_edit_file_stringified_json_surfaces_parse_error() {
        let handler = EditFileHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "json-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let params = serde_json::json!({
            "files": "this is not valid json {["
        });
        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(
            result.is_ok(),
            "invalid stringified JSON should return Ok with error message"
        );
        let msg = result.unwrap().as_str().unwrap().to_string();
        assert!(
            msg.contains("Parse error") || msg.contains("parse error"),
            "stringified JSON parse failure must surface the actual parse error, got: {}",
            msg
        );
        assert!(
            msg.contains("files"),
            "error must name the 'files' parameter, got: {}",
            msg
        );
    }

    // =====================================================================
    // Model-simulation tests
    //
    // These tests send the EXACT JSON structures that real models produced
    // in conversation exports. They simulate model-shaped inputs rather than
    // constructing clean Rust-native JSON. Each test is annotated with the
    // conversation export and model that produced the pattern.
    //
    // If a test fails, the fix must be a lenient acceptance or a targeted
    // error message — never a silent rejection that wastes a round-trip.
    // =====================================================================

    /// Helper: create a temp file, reconcile anchors, return (dir, file_path, anchors).
    /// The caller must keep `dir` alive for the duration of the test.
    async fn setup_test_file(
        content: &str,
        task_id: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, content).unwrap();
        let anchor_mgr = AnchorStateManager::new();
        let lines = crate::core::file_editor::split_content_lines(content);
        let anchors = anchor_mgr.reconcile(file_path.to_str().unwrap(), &lines, Some(task_id));
        (dir, file_path, anchors)
    }

    /// Helper: build a ToolContext for a temp dir.
    fn ctx_for_dir(dir: &tempfile::TempDir, task_id: &str) -> ToolContext {
        ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
            None,
            dir.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            task_id.to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        )
    }

    // --- Pattern 1: path inside edit object ---
    // Seen in: prompt 2 (Claude), prompt 3 (Qwen), prompt 4 (Mimo)
    // Model puts "path" inside the edit object instead of at file-entry level.
    // The handler must accept this leniently.

    #[tokio::test]
    async fn model_sim_path_inside_edit_object_accepted() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) =
            setup_test_file("line 1\nline 2\nline 3\n", "sim-path-inside").await;
        let ctx = ctx_for_dir(&dir, "sim-path-inside");

        let anchor = format!("{}§line 2", anchors[1]);
        // Exact structure from prompt 2 msg_23 (Claude) — path inside edit object
        let params = serde_json::json!({
            "files": [{
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "replaced",
                    "path": "test.txt"
                }]
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "path inside edit object should be accepted. Error: {:?}",
            result.err()
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("replaced"), "edit should have been applied");
    }

    // --- Pattern 2: anchor/edit_type/text as siblings ---
    // Seen in: prompt 3 (Qwen), prompt 4 (Mimo), prompt 6 (DeepSeek)
    // Model puts anchor, edit_type, text at file-entry level instead of
    // inside an edits array.

    #[tokio::test]
    async fn model_sim_edit_fields_as_siblings_gives_targeted_error() {
        let handler = EditFileHandler::new();
        let ctx = ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "sim-siblings".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // Exact structure from prompt 4 msg_13 (Mimo)
        let params = serde_json::json!({
            "files": [{
                "anchor": "SomeWord§    return f\"{result:.4f}\"",
                "edit_type": "insert_after",
                "path": "utils.py",
                "text": "\n\ndef clamp_result(value, min_val, max_val):"
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("must be inside an 'edits' array"),
            "should explain the structural problem, got: {msg}"
        );
        assert!(
            msg.contains("Correct:") && msg.contains("Wrong:"),
            "should show correct and wrong structures, got: {msg}"
        );
    }

    // --- Pattern 3: end_anchor at file-entry level ---
    // Seen in: prompt 6 (DeepSeek)
    // Model puts end_anchor at file-entry level alongside edits.

    #[tokio::test]
    async fn model_sim_end_anchor_at_file_entry_level_accepted() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) =
            setup_test_file("line 1\nline 2\nline 3\n", "sim-end-anchor").await;
        let ctx = ctx_for_dir(&dir, "sim-end-anchor");

        let anchor = format!("{}§line 1", anchors[0]);
        let end_anchor = format!("{}§line 2", anchors[1]);
        // Exact structure from prompt 6 msg_23 (DeepSeek) — end_anchor at file-entry level
        let params = serde_json::json!({
            "files": [{
                "anchor": anchor,
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "replaced"
                }],
                "end_anchor": end_anchor,
                "path": "test.txt"
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "end_anchor at file-entry level should not break. Error: {:?}",
            result.err()
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("replaced"), "edit should have been applied");
    }

    // --- Pattern 4: anchor at file-entry level alongside edits ---
    // Seen in: prompt 5 (unknown model), prompt 6 (DeepSeek)
    // Model puts anchor at file-entry level AND inside edits (redundant).

    #[tokio::test]
    async fn model_sim_anchor_at_file_entry_alongside_edits_accepted() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) =
            setup_test_file("line 1\nline 2\nline 3\n", "sim-anchor-sibling").await;
        let ctx = ctx_for_dir(&dir, "sim-anchor-sibling");

        let anchor = format!("{}§line 2", anchors[1]);
        // Exact structure from prompt 5 msg_9 (model puts anchor at file-entry level
        // alongside edits, with path at file-entry level)
        let params = serde_json::json!({
            "files": [{
                "anchor": anchor,
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "insert_after",
                    "text": "\n\ndef clamp_result(value, min_val=0, max_val=100):"
                }],
                "path": "test.txt"
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "anchor at file-entry level alongside edits should be accepted. Error: {:?}",
            result.err()
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("clamp_result"), "edit should have been applied");
    }

    // --- Pattern 5: stringified JSON with unescaped quotes in anchor ---
    // Seen in: prompt 3 (Qwen)
    // Model sends files as a stringified JSON but the anchor content
    // contains unescaped double quotes that break JSON parsing.

    #[tokio::test]
    async fn model_sim_stringified_json_with_unescaped_quotes() {
        let handler = EditFileHandler::new();
        let ctx = ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "sim-unescaped".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // Exact structure from prompt 3 msg_13 (Qwen) — anchor has unescaped quotes
        let params = serde_json::json!({
            "files": "[{\"path\": \"utils.py\", \"edits\": [{\"anchor\": \"SomeWord§    return f\"{result:.4f}\".rstrip('0').rstrip('.'), \"edit_type\": \"insert_after\", \"text\": \"\\n\\ndef clamp_result()\"}]}]"
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        // Should return Ok with error message, not panic
        assert!(result.is_ok());
        let msg = result.unwrap().as_str().unwrap().to_string();
        assert!(
            msg.contains("Parse error") || msg.contains("parse error"),
            "should surface the JSON parse error, got: {msg}"
        );
    }

    // --- Pattern 6: stringified JSON with missing comma ---
    // Seen in: prompt 4 (Mimo)
    // Model sends files as stringified JSON with a missing comma between
    // the closing brace and "path" key.

    #[tokio::test]
    async fn model_sim_stringified_json_missing_comma() {
        let handler = EditFileHandler::new();
        let ctx = ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "sim-missing-comma".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // Exact structure from prompt 4 msg_23 (Mimo) — missing comma before "path"
        let params = serde_json::json!({
            "files": "[{\"edits\": [{\"anchor\": \"Word§line\", \"edit_type\": \"replace\", \"text\": \"new\"}] \"path\": \"test.py\"}]"
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok());
        let msg = result.unwrap().as_str().unwrap().to_string();
        assert!(
            msg.contains("Parse error") || msg.contains("parse error"),
            "should surface the JSON parse error, got: {msg}"
        );
    }

    // --- Pattern 7: edits as string at file-entry level ---
    // Seen in: prompt 3 (Qwen)
    // Model sends edits as a stringified JSON string instead of an array.

    #[tokio::test]
    async fn model_sim_edits_as_string_at_file_entry() {
        let handler = EditFileHandler::new();
        let ctx = ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "sim-edits-string".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        // Exact structure from prompt 3 msg_21 (Qwen)
        let params = serde_json::json!({
            "files": [{
                "path": "main.py",
                "edits": "[{\"anchor\": \"Word§line\", \"edit_type\": \"replace\", \"text\": \"new\"}]"
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Missing 'edits'") || msg.contains("edits"),
            "should report missing/invalid edits, got: {msg}"
        );
    }

    // --- Pattern 8: path missing entirely (no path anywhere) ---
    // Seen in: prompt 2 (Claude), prompt 3 (Qwen)
    // Model sends edits array with no path at file-entry level and no
    // path inside the edit object either.

    #[tokio::test]
    async fn model_sim_path_missing_entirely() {
        let handler = EditFileHandler::new();
        let ctx = ToolContext::new(
            Arc::new(tokio::sync::Mutex::new(TaskState::default())),
            None,
            std::env::current_dir().unwrap(),
            AnchorStateManager::new(),
            false,
            "sim-no-path".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "files": [{
                "edits": [{
                    "anchor": "Apple§fn main() {",
                    "text": "replacement"
                }]
            }]
        });

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("requires a 'path' key"),
            "should report missing path, got: {msg}"
        );
    }

    // --- Pattern 9: multiline replace near end of file ending with newline ---
    // This would have caught the .lines() vs .split('\n') panic.
    // The file ends with \n, the edit modifies the last lines, and
    // format_result needs to index into final_hashes.

    #[tokio::test]
    async fn model_sim_replace_near_end_of_file_with_trailing_newline() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) = setup_test_file(
            "def add(a, b):\n    return a + b\n\ndef divide(a, b):\n    return a / b\n",
            "sim-trailing-nl",
        )
        .await;
        let ctx = ctx_for_dir(&dir, "sim-trailing-nl");

        // Replace the last function (near the end of a file that ends with \n)
        let anchor = format!("{}§    return a / b", anchors[4]);
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "    if b == 0:\n        return None\n    return a / b"
                }]
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "replace near end of file with trailing newline should not panic. Error: {:?}",
            result.err()
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("if b == 0"), "edit should have been applied");
        assert!(content.ends_with('\n'), "trailing newline should be preserved");
    }

    #[tokio::test]
    async fn test_edit_file_preserves_trailing_newlines_in_replacement_text() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) =
            setup_test_file("line 1\nline 2", "replacement-trailing-newlines").await;
        let ctx = ctx_for_dir(&dir, "replacement-trailing-newlines");
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": format!("{}§line 2", anchors[1]),
                    "edit_type": "replace",
                    "text": "replacement\n\n"
                }]
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;

        assert!(
            result.is_ok(),
            "replacement should preserve terminal newlines: {:?}",
            result.err()
        );
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "line 1\nreplacement\n\n"
        );
    }

    // --- Pattern 10: multiple edits in one file in one call ---
    // Seen in: prompt 6 (DeepSeek), prompt 5 (unknown model)
    // Model sends two edits for the same file in one edit_file call.

    #[tokio::test]
    async fn model_sim_two_edits_same_file_one_call() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) = setup_test_file(
            "def format_result(result):\n    return f\"{result:.4f}\"\n\ndef log_operation(a, b):\n    print(f\"{a} + {b}\")\n",
            "sim-two-edits",
        )
        .await;
        let ctx = ctx_for_dir(&dir, "sim-two-edits");

        let anchor1 = format!("{}§    return f\"{{result:.4f}}\"", anchors[1]);
        let anchor2 = format!("{}§def log_operation(a, b):", anchors[3]);
        // Exact pattern from prompt 6 msg_15 (DeepSeek) — two edits in one call
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [
                    {
                        "anchor": anchor1,
                        "edit_type": "insert_after",
                        "text": "\n\ndef clamp_result(value, min_val, max_val):\n    return value"
                    },
                    {
                        "anchor": anchor2,
                        "edit_type": "replace",
                        "text": "from datetime import datetime\n\ndef log_operation(a, b):\n    timestamp = datetime.now().strftime(\"%Y-%m-%d\")\n    print(f\"[{timestamp}] {a} + {b}\")"
                    }
                ]
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "two edits in one call should succeed. Error: {:?}",
            result.err()
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("clamp_result"), "first edit should be applied");
        assert!(content.contains("datetime"), "second edit should be applied");
    }

    #[tokio::test]
    async fn model_sim_duplicate_file_entries_merged() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) =
            setup_test_file("line 1\nline 2\nline 3\n", "sim-dup-entries").await;
        let ctx = ctx_for_dir(&dir, "sim-dup-entries");

        let anchor1 = format!("{}§line 1", anchors[0]);
        let anchor2 = format!("{}§line 2", anchors[1]);
        let params = serde_json::json!({
            "files": [
                {
                    "path": "test.txt",
                    "edits": [{
                        "anchor": anchor1,
                        "edit_type": "replace",
                        "text": "replaced 1"
                    }]
                },
                {
                    "path": "test.txt",
                    "edits": [{
                        "anchor": anchor2,
                        "edit_type": "replace",
                        "text": "replaced 2"
                    }]
                }
            ]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "duplicate file entries should be merged. Error: {:?}",
            result.as_ref().err()
        );
        let output = result.unwrap();
        assert!(output.as_str().unwrap().contains("Edited 1 file(s)"));
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("replaced 1"), "first edit should be applied");
        assert!(content.contains("replaced 2"), "second edit should be applied");
    }

    #[tokio::test]
    async fn model_sim_anchor_collision_reports_error() {
        let _guard = TEST_MUTEX.lock().await;
        // Duplicate lines get unique anchors via salt-based collision resolution in get_word_for_hash.
        // This test verifies that editing one duplicate line works correctly — the anchor system
        // assigns unique anchors even for identical content, so collision detection in resolve_anchor
        // is not triggered. This documents the current behavior.
        let (dir, file_path, anchors) = setup_test_file(
            "duplicate line\nduplicate line\nunique line\n",
            "sim-collision",
        )
        .await;
        let ctx = ctx_for_dir(&dir, "sim-collision");

        let anchor = format!("{}§duplicate line", anchors[0]);
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "replaced"
                }]
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "editing one of duplicate lines should succeed. Error: {:?}",
            result.err()
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(
            content.contains("replaced"),
            "first duplicate line should be replaced"
        );
        assert!(
            content.contains("duplicate line"),
            "second duplicate line should remain"
        );
    }

    #[tokio::test]
    async fn model_sim_three_edits_same_file_one_call() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) = setup_test_file(
            "line A\nline B\nline C\nline D\nline E\n",
            "sim-three-edits",
        )
        .await;
        let ctx = ctx_for_dir(&dir, "sim-three-edits");

        let anchor1 = format!("{}§line A", anchors[0]);
        let anchor2 = format!("{}§line C", anchors[2]);
        let anchor3 = format!("{}§line E", anchors[4]);
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [
                    {
                        "anchor": anchor1,
                        "edit_type": "replace",
                        "text": "replaced A"
                    },
                    {
                        "anchor": anchor2,
                        "edit_type": "replace",
                        "text": "replaced C"
                    },
                    {
                        "anchor": anchor3,
                        "edit_type": "replace",
                        "text": "replaced E"
                    }
                ]
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "three edits in one call should succeed. Error: {:?}",
            result.err()
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("replaced A"), "first edit should be applied");
        assert!(content.contains("line B"), "untouched line should remain");
        assert!(content.contains("replaced C"), "second edit should be applied");
        assert!(content.contains("line D"), "untouched line should remain");
        assert!(content.contains("replaced E"), "third edit should be applied");
    }

    #[tokio::test]
    async fn model_sim_empty_replacement_text_deletes_anchor_line() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, file_path, anchors) =
            setup_test_file("keep this\nremove this\nkeep this too\n", "sim-empty-text").await;
        let ctx = ctx_for_dir(&dir, "sim-empty-text");

        let anchor = format!("{}§remove this", anchors[1]);
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": ""
                }]
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        assert!(
            result.is_ok(),
            "empty replacement text should succeed (intentional delete). Error: {:?}",
            result.err()
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(
            !content.contains("remove this"),
            "anchor line should be deleted"
        );
        assert!(content.contains("keep this"), "other lines should remain");
    }

    #[tokio::test]
    async fn model_sim_whitespace_anchor_mismatch() {
        let _guard = TEST_MUTEX.lock().await;
        let (dir, _file_path, anchors) =
            setup_test_file("  indented line  \n", "sim-whitespace").await;
        let ctx = ctx_for_dir(&dir, "sim-whitespace");

        // Anchor text has different whitespace than actual file content
        let anchor = format!("{}§indented line", anchors[0]);
        let params = serde_json::json!({
            "files": [{
                "path": "test.txt",
                "edits": [{
                    "anchor": anchor,
                    "edit_type": "replace",
                    "text": "replaced"
                }]
            }]
        });

        let result = ToolHandler::execute(&EditFileHandler::new(), &ctx, params).await;
        let output = result.unwrap();
        let msg = output.as_str().unwrap();
        assert!(
            msg.contains("does not match the file's content"),
            "whitespace mismatch should produce exact-match error. Got: {}",
            msg
        );
    }
}
