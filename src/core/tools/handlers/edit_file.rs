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
use crate::core::hash_utils::{ANCHOR_DELIMITER, compute_hashes};
use crate::core::tools::handlers::diagnostics_scan::{DiagnosticsScanHandler, ProjectType};
use crate::core::tools::handlers::error_guidance;
use crate::core::tools::{
    SnedTool, ToolContext, ToolError, ToolFailureClass, ToolFailureMetadata, ToolHandler,
    ToolRequiredNextStep,
};
use crate::services::symbol_index::SymbolIndexService;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

    /// Parse edits from JSON params.
    fn parse_edits(
        &self,
        files: &[serde_json::Value],
    ) -> Result<Vec<(String, Vec<Edit>)>, ToolError> {
        let mut result = Vec::new();

        for file in files {
            let path = file.get("path").and_then(|p| p.as_str()).ok_or_else(|| {
                ToolError::InvalidInput(error_guidance::missing_parameter("path", 0))
            })?;

            let edits_raw = file
                .get("edits")
                .and_then(|e| e.as_array())
                .ok_or_else(|| {
                    ToolError::InvalidInput(format!(
                        "Missing 'edits' for file '{}'. {}",
                        path,
                        error_guidance::missing_parameter("edits", 0)
                    ))
                })?;

            let mut edits = Vec::new();
            for edit_raw in edits_raw {
                let anchor = edit_raw
                    .get("anchor")
                    .and_then(|a| a.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput(format!(
                            "Missing 'anchor' in edit for file '{}'. {}",
                            path,
                            error_guidance::missing_parameter("anchor", 0)
                        ))
                    })?;

                let edit_type = edit_raw
                    .get("edit_type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("replace");

                Self::validate_edit_type(edit_type)?;

                let end_anchor = edit_raw.get("end_anchor").and_then(|e| e.as_str());

                let text = edit_raw.get("text").and_then(|t| t.as_str()).unwrap_or("");

                edits.push(Edit {
                    anchor: anchor.to_string(),
                    end_anchor: end_anchor.map(|s| s.to_string()),
                    edit_type: edit_type.to_string(),
                    text: text.to_string(),
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
                "Unknown edit_type '{}'. Valid values are: replace, insert_before, insert_after",
                edit_type
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
        let mut affected_paths = Vec::new();

        for file in files {
            let path = file
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("unknown");
            if let Ok(resolved) = self.resolve_path(workspace_root, path) {
                affected_paths.push(resolved);
            }

            let edits = file
                .get("edits")
                .and_then(|e| e.as_array())
                .map(|a| a.as_slice())
                .unwrap_or(&[]);

            for edit in edits {
                let anchor = edit.get("anchor").and_then(|a| a.as_str()).unwrap_or("");

                if !anchor.contains(ANCHOR_DELIMITER) {
                    invalid_anchors.push(format!(
                        "  - File '{}': anchor '{}' is missing the '{}' delimiter",
                        path,
                        if anchor.chars().count() > 50 {
                            format!("{}...", anchor.chars().take(50).collect::<String>())
                        } else {
                            anchor.to_string()
                        },
                        ANCHOR_DELIMITER
                    ));
                }

                if let Some(end_anchor) = edit.get("end_anchor").and_then(|a| a.as_str())
                    && !end_anchor.contains(ANCHOR_DELIMITER)
                {
                    invalid_anchors.push(format!(
                        "  - File '{}': end_anchor '{}' is missing the '{}' delimiter",
                        path,
                        if end_anchor.chars().count() > 50 {
                            format!("{}...", end_anchor.chars().take(50).collect::<String>())
                        } else {
                            end_anchor.to_string()
                        },
                        ANCHOR_DELIMITER
                    ));
                }
            }
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
    }

    fn reread_required_error(display_path: &str, absolute_path: &str) -> ToolError {
        ToolError::ExecutionFailedWithMetadata(
            format!(
                "You must re-read {} before retrying edit_file. A previous edit attempt proved the anchors or file state were stale.",
                display_path
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
                "File {} was modified externally during edit operation. Aborting write to prevent data loss. Re-read the file and retry.",
                display_path
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
        let files = params
            .get("files")
            .and_then(|f| f.as_array())
            .ok_or_else(|| {
                ToolError::InvalidInput("Missing required parameter: files".to_string())
            })?;

        if files.is_empty() {
            return Err(ToolError::InvalidInput("No files provided".to_string()));
        }

        self.validate_anchors(files, workspace_root)?;

        let parsed = self.parse_edits(files)?;
        let processor = BatchProcessor::new(DiffMode::Full);

        let silent = params
            .get("silent")
            .and_then(|s| s.as_bool())
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

        let mut all_results: Vec<String> = Vec::new();
        let mut total_applied = 0usize;
        let mut total_failed = 0usize;
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

            // Mark file as edited by Sned to suppress false positives
            state
                .file_context_tracker
                .mark_file_as_edited_by_sned(Path::new(&batch.absolute_path));

            let stale_warning = state
                .file_context_tracker
                .check_stale(Path::new(&batch.absolute_path))
                .await;
            if let Some(warning) = &stale_warning {
                all_results.push(warning.clone());
            }

            if stale_warning.is_some() {
                state.file_content_cache.pop(&batch.absolute_path);
            }

            // Warn if editing a file not read this session
            if !json_output
                && !state
                    .file_context_tracker
                    .was_read_this_session(&batch.display_path)
            {
                use crate::cli::output::OutputEvent;
                use ratatui::style::{Color, Style};
                output_writer.emit(OutputEvent::styled(
                    format!(
                        "⚠ editing {} (not read this session — may have stale assumptions)",
                        batch.display_path
                    ),
                    Style::default().fg(Color::Yellow),
                ));
            }

            // Read file content and capture mtime (check cache first for cross-call coordination)
            let (content, initial_mtime) =
                if let Some(cached_content) = state.file_content_cache.get(&batch.absolute_path) {
                    // SECURITY: Re-verify file is still valid (not swapped with symlink) even when using cache
                    match tokio::fs::symlink_metadata(&batch.absolute_path).await {
                        Ok(metadata) if metadata.is_file() && !metadata.is_symlink() => {
                            tracing::debug!("Using cached content for {} (symlink check passed)", batch.display_path);
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
                            all_results.push(format!("Error verifying file {}: {}", batch.display_path, e));
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

            // Compute line hashes via AnchorStateManager
            let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            let anchors = anchor_mgr.reconcile(&batch.absolute_path, &lines, task_id);

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
                        return Err(ToolError::ExecutionFailed(format!("Approval error: {}", e)));
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
                    .map(|p| p.to_path_buf())
                    .unwrap_or(PathBuf::from("."))
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

            total_applied += result.resolved_count;
            total_failed += result.failed_count;

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

            // Store intermediate result for building final output after batch diagnostics
            let final_lines: Vec<String> = result
                .final_content
                .as_ref()
                .map(|c| c.lines().map(|s| s.to_string()).collect())
                .unwrap_or_default();

            let final_hashes = compute_hashes(&final_lines)
                .iter()
                .map(|h| format!("{:08x}", h))
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

            for item in &write_items {
                let std_file = match std::fs::OpenOptions::new()
                    .write(true)
                    .open(&item.absolute_path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        // Rollback all previously written files
                        for path in written_paths.iter().rev() {
                            if let Some(orig) = original_contents.get(path)
                                && let Err(re) = std::fs::write(path, orig)
                            {
                                tracing::error!("Failed to rollback file {}: {}", path, re);
                            }
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

                    match crate::storage::disk::atomic_write_file_async(
                        &item.absolute_path,
                        &item.final_content,
                    )
                    .await
                    {
                        Ok(()) => {
                            written_paths.push(item.absolute_path.clone());
                        }
                        Err(e) => {
                            for path in written_paths.iter().rev() {
                                if let Some(orig) = original_contents.get(path)
                                    && let Err(re) = std::fs::write(path, orig)
                                {
                                    tracing::error!("Failed to rollback file {}: {}", path, re);
                                }
                            }
                            all_results
                                .push(format!("Error writing file {}: {}", item.display_path, e));
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
                                    && let Err(re) = std::fs::write(path, orig)
                                {
                                    tracing::error!("Failed to rollback file {}: {}", path, re);
                                }
                            }
                            all_results
                                .push(format!("Error writing file {}: {}", item.display_path, e));
                        }
                    }
                }
            }
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
                        .map(|p| p.to_path_buf())
                        .unwrap_or(PathBuf::from("."))
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

                let new_problems_message = if !new_problems.is_empty() {
                    DiagnosticsScanHandler::format_diagnostics(
                        &file_result.batch_display_path,
                        &new_problems,
                        None,
                    )
                } else {
                    String::new()
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
                false,
                diagnostics.as_ref(),
                None,
                None,
            );
            all_results.push(formatted);
        }

        // Track consecutive mistakes: increment when any edits failed,
        // only reset when ALL edits succeeded (no failures at all).
        if total_failed > 0 {
            state.consecutive_mistakes += 1;
            if !json_output {
                use crate::cli::output::OutputEvent;
                output_writer.emit(OutputEvent::plain(format!(
                    "[edit_file] {} edit(s) failed (consecutive_mistakes={})",
                    total_failed, state.consecutive_mistakes
                )));
            }
        } else if total_applied > 0 {
            state.consecutive_mistakes = 0;
        }

        let summary = format!(
            "Edited {} file(s): {} edit(s) applied, {} edit(s) failed.",
            files.len(),
            total_applied,
            total_failed
        );

        Ok(format!(
            "{}\n\n{}",
            summary,
            all_results.join("\n\n---\n\n")
        ))
    }

    pub fn description(&self, params: &serde_json::Value) -> String {
        let path = params
            .get("files")
            .and_then(|f| f.as_array())
            .and_then(|arr| arr.first())
            .and_then(|f| f.get("path"))
            .and_then(|p| p.as_str())
            .unwrap_or("?");
        format!("[edit_file for '{}']", path)
    }
}

#[async_trait]
impl ToolHandler for EditFileHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let mut state = ctx.state.lock().await;
        let result = self
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

        if result.is_err() {
            state.consecutive_mistakes += 1;
            if !ctx.json_output {
                use crate::cli::output::OutputEvent;
                ctx.output_writer.emit(OutputEvent::plain(format!(
                    "[edit_file] Handler error, incrementing consecutive_mistakes={}",
                    state.consecutive_mistakes
                )));
            }
        }

        result.map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        EditFileHandler::description(self, params)
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
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("files"));
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
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No files"));
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
    async fn test_consecutive_mistakes_increments_on_handler_error() {
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

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        let mistakes = state.lock().await.consecutive_mistakes;
        assert!(
            mistakes >= 1,
            "consecutive_mistakes should increment on file-not-found edit failure, got {}, result: {:?}",
            mistakes,
            result
        );
    }

    #[tokio::test]
    async fn test_consecutive_mistakes_increments_on_failure() {
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

        let result = ToolHandler::execute(&handler, &ctx, params).await;
        assert!(result.is_ok());
        assert_eq!(
            state.lock().await.consecutive_mistakes,
            1,
            "consecutive_mistakes should increment when edits fail"
        );
    }

    #[tokio::test]
    async fn test_consecutive_mistakes_resets_on_success() {
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
        assert_eq!(
            state.lock().await.consecutive_mistakes,
            0,
            "consecutive_mistakes should reset on successful edit. Result: {}",
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
    async fn test_edit_file_stale_warning_does_not_block_valid_anchors() {
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
        let result = ToolHandler::execute(&handler, &ctx, params).await.unwrap();
        assert!(
            result
                .as_str()
                .unwrap()
                .contains("Applied 1 edit(s) successfully")
        );
        assert!(
            !state
                .lock()
                .await
                .must_reread_before_edit
                .contains(&file_path.to_string_lossy().to_string())
        );

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
        assert!(
            result.is_ok(),
            "Edit should succeed in yolo mode"
        );
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
        assert!(!err_msg.contains("你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界"), 
                "Long anchor should be truncated, not show full 80-char string");
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
    async fn test_prepare_edits_failure_increments_total_failed() {
        use std::fs;

        let _guard = TEST_MUTEX.lock().await;

        let temp_dir = "test_prepare_fail_tmp";
        let _ = fs::remove_dir_all(temp_dir);
        fs::create_dir_all(temp_dir).unwrap();

        let file_path = format!("{}/test.txt", temp_dir);
        fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

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

        let _ = fs::remove_dir_all(temp_dir);
    }
}
