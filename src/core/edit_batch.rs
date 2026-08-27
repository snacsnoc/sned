//! Batch processing for hash-anchored file edits.
//!
//! and `dirac/src/core/task/tools/handlers/edit-file/EditFormatter.ts`.

use crate::core::file_editor::{
    AppliedEdit, ApplyOutcome, Edit, EditExecutor, FailedEdit, FileEditorError, ResolvedEdit,
    UnchangedSite, split_content_lines,
};
use crate::core::hash_utils::format_line_with_hash;
use crate::core::tools::handlers::error_guidance;
use std::path::Path;

/// Bounds for the optional multi-line fingerprint supplied to `edit_file`.
/// Fingerprints should identify a focused range, not carry an entire file.
pub(crate) const MAX_FINGERPRINT_CONTENT_LINES: usize = 4096;
pub(crate) const MAX_FINGERPRINT_CONTENT_BYTES: usize = 1024 * 1024;

fn syntax_language_for_path(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
}

fn highlight_diff_content(content: &str, language: &str, colored: bool) -> String {
    if colored {
        crate::cli::syntax_highlight::highlight_code(content, language)
    } else {
        content.to_string()
    }
}

fn highlighted_line_with_hash(content: &str, hash: &str, language: &str, colored: bool) -> String {
    format_line_with_hash(
        &highlight_diff_content(content, language, colored),
        hash,
        &[],
    )
}

// ============================================================================
// Batch Types
// ============================================================================

/// A batch of edits for a single file.
#[derive(Debug, Clone)]
pub struct FileEditBatch {
    pub absolute_path: String,
    pub display_path: String,
    pub edits: Vec<Edit>,
}

/// Result of preparing edits for a file.
#[derive(Debug, Clone)]
pub struct PreparedEdits {
    pub content: String,
    pub final_content: String,
    pub diff: String,
    pub resolved_edits: Vec<ResolvedEdit>,
    pub failed_edits: Vec<FailedEdit>,
    pub applied_edits: Vec<AppliedEdit>,
    pub lines: Vec<String>,
    pub line_hashes: Vec<String>,
    pub final_lines: Vec<String>,
    pub initial_mtime: Option<std::time::SystemTime>,
}

/// Diagnostics result for a batch.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsResult {
    pub fixed_count: usize,
    pub new_problems_message: String,
}

/// Result of applying a batch.
#[derive(Debug, Clone)]
pub struct BatchResult {
    pub success: bool,
    pub final_content: Option<String>,
    pub resolved_count: usize,
    pub failed_count: usize,
    pub overlap: bool,
    /// Net lines added (additions minus removals) for session summary.
    pub lines_added: u32,
    /// Net lines removed for session summary.
    pub lines_removed: u32,
    /// Per-edit diagnostics for replaces filtered as no-ops, so the
    /// caller can surface line number and content-occurrence count
    /// instead of reporting silent success.
    pub unchanged_sites: Vec<UnchangedSite>,
    /// 1-indexed line numbers in the assembled output that still
    /// contained a `Word§` or `hex§` fragment. Empty when the batch
    /// applied cleanly; populated (with `success: false`) when the
    /// post-assembly § check rejects the batch.
    pub glued_anchor_lines: Vec<usize>,
}

// ============================================================================
// Batch Processor
// ============================================================================

/// Processes batches of hash-anchored edits.
///
#[derive(Debug, Clone, Default)]
pub struct BatchProcessor {
    executor: EditExecutor,
    diff_mode: DiffMode,
}

/// Diff output mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffMode {
    #[default]
    Full,
    AdditionsOnly,
}

fn count_line_hash_changes(original_hashes: &[String], final_hashes: &[String]) -> (usize, usize) {
    let mut counts: std::collections::HashMap<&str, isize> =
        std::collections::HashMap::with_capacity(original_hashes.len() + final_hashes.len());
    for hash in original_hashes {
        *counts.entry(hash.as_str()).or_default() += 1;
    }
    for hash in final_hashes {
        *counts.entry(hash.as_str()).or_default() -= 1;
    }

    counts.values().fold((0, 0), |(added, removed), count| {
        if *count < 0 {
            (added + count.unsigned_abs(), removed)
        } else {
            (added, removed + *count as usize)
        }
    })
}

impl BatchProcessor {
    #[must_use]
    pub fn new(diff_mode: DiffMode) -> Self {
        Self {
            executor: EditExecutor::new(),
            diff_mode,
        }
    }

    /// Groups edits by file path.
    ///
    pub fn group_edits_by_path(
        &self,
        file_edits: &[(String, Vec<Edit>)], // (display_path, edits)
        resolve_path: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<FileEditBatch> {
        let mut batches: Vec<FileEditBatch> = Vec::new();
        let mut seen_paths: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(file_edits.len());

        for (display_path, edits) in file_edits {
            if let Some(absolute_path) = resolve_path(display_path) {
                if let Some(&idx) = seen_paths.get(&absolute_path) {
                    batches[idx].edits.extend_from_slice(edits);
                } else {
                    seen_paths.insert(absolute_path.clone(), batches.len());
                    batches.push(FileEditBatch {
                        absolute_path: absolute_path.clone(),
                        display_path: display_path.clone(),
                        edits: edits.clone(),
                    });
                }
            }
        }

        batches
    }

    /// Validates a single edit.
    ///
    pub fn validate_edit(&self, edit: &Edit) -> Result<(), FileEditorError> {
        let edit_type = &edit.edit_type;
        let has_end_anchor = edit.end_anchor.is_some();
        let is_replace = edit_type == "replace" || edit_type.is_empty();

        if edit.anchor.is_empty() {
            return Err(FileEditorError::ValidationError(
                "Each edit must contain 'anchor'.".to_string(),
            ));
        }

        if is_replace && !has_end_anchor {
            // Auto-default: single-line replace when end_anchor is omitted.
            // end_anchor defaults to anchor at resolve time (file_editor.rs).
        }

        // The `content` field is meaningful only as a multi-line
        // fingerprint interior, which requires both end_anchor and the
        // replace type. Supplying content on an insert or on a
        // single-line replace is silently ignored at resolve time —
        // reject it here so the model gets feedback instead.
        if edit.content.is_some() && !(is_replace && has_end_anchor) {
            return Err(FileEditorError::ValidationError(
                "The 'content' field is only valid for replace edits with an 'end_anchor' (multi-line fingerprint shape). Remove 'content', or add 'end_anchor' to make it a multi-line replace.".to_string(),
            ));
        }

        if let Some(content) = edit.content.as_ref() {
            if content.len() > MAX_FINGERPRINT_CONTENT_LINES {
                return Err(FileEditorError::ValidationError(format!(
                    "The 'content' fingerprint is limited to {MAX_FINGERPRINT_CONTENT_LINES} lines; use a narrower anchor range or write_to_file for a broad rewrite."
                )));
            }
            let content_bytes = content
                .iter()
                .map(String::len)
                .try_fold(0usize, usize::checked_add)
                .unwrap_or(usize::MAX);
            if content_bytes > MAX_FINGERPRINT_CONTENT_BYTES {
                return Err(FileEditorError::ValidationError(format!(
                    "The 'content' fingerprint is limited to {} bytes; use a narrower anchor range or write_to_file for a broad rewrite.",
                    MAX_FINGERPRINT_CONTENT_BYTES
                )));
            }
        }

        if edit.text.is_empty() && edit_type == "replace" {
            // Allow empty text for replace (deletes content)
        }

        Ok(())
    }

    /// Prepares edits for a file.
    ///
    pub fn prepare_edits(
        &self,
        _absolute_path: &str,
        _display_path: &str,
        content: &str,
        edits: &[Edit],
        line_hashes: &[String],
    ) -> Result<PreparedEdits, FileEditorError> {
        let lines = split_content_lines(content);

        // Validate all edits first
        for edit in edits {
            self.validate_edit(edit)?;
        }

        let (resolved_edits, failed_edits) =
            self.executor.resolve_edits(edits, &lines, line_hashes);

        if resolved_edits.is_empty() {
            let failure_messages: Vec<String> = failed_edits
                .iter()
                .map(|f| {
                    self.executor
                        .format_failure_message(&f.edit, Some(&f.error))
                })
                .collect();
            return Err(FileEditorError::AllEditsFailed {
                message: failure_messages.join("\n\n"),
            });
        }

        Ok(PreparedEdits {
            content: content.to_string(),
            final_content: content.to_string(),
            diff: String::new(),
            resolved_edits,
            failed_edits,
            applied_edits: Vec::new(),
            lines,
            line_hashes: line_hashes.to_vec(),
            final_lines: Vec::new(),
            initial_mtime: None,
        })
    }

    /// Applies prepared edits and generates diff.
    ///
    pub fn apply_batch(
        &self,
        batch: &mut PreparedEdits,
        _absolute_path: &str,
        display_path: &str,
    ) -> BatchResult {
        let outcome = self
            .executor
            .apply_edits(&batch.lines, &batch.resolved_edits);
        match outcome {
            ApplyOutcome::Overlap => BatchResult {
                success: false,
                final_content: None,
                resolved_count: batch.resolved_edits.len(),
                failed_count: batch.failed_edits.len(),
                overlap: true,
                lines_added: 0,
                lines_removed: 0,
                unchanged_sites: Vec::new(),
                glued_anchor_lines: Vec::new(),
            },
            ApplyOutcome::GluedAnchor(lines) => BatchResult {
                success: false,
                final_content: None,
                resolved_count: batch.resolved_edits.len(),
                failed_count: batch.failed_edits.len(),
                overlap: false,
                lines_added: 0,
                lines_removed: 0,
                unchanged_sites: Vec::new(),
                glued_anchor_lines: lines,
            },
            ApplyOutcome::Applied(
                final_lines,
                added_count,
                removed_count,
                applied_edits,
                unchanged_sites,
            ) => {
                let resolved_count = applied_edits.len();
                batch.final_lines = final_lines.clone();
                batch.final_content = final_lines.join("\n");
                batch.applied_edits = applied_edits;

                let diff = self.generate_diff(display_path, batch);
                batch.diff = diff;

                BatchResult {
                    success: true,
                    final_content: Some(batch.final_content.clone()),
                    resolved_count,
                    failed_count: batch.failed_edits.len(),
                    overlap: false,
                    lines_added: added_count as u32,
                    lines_removed: removed_count as u32,
                    unchanged_sites,
                    glued_anchor_lines: Vec::new(),
                }
            }
        }
    }

    /// Generates diff for a batch.
    #[must_use]
    pub fn generate_diff(&self, display_path: &str, prepared: &PreparedEdits) -> String {
        let mut diff = String::new();
        let colored = !crate::cli::colors::stdout_colors_disabled();
        let language = syntax_language_for_path(display_path);
        if colored {
            diff.push_str(&format!(
                "{}{} Update File: {}{}\n\n",
                crate::cli::colors::style::BOLD,
                crate::cli::colors::style::CYAN,
                crate::cli::colors::file_path(display_path),
                crate::cli::colors::style::RESET
            ));
        } else {
            diff.push_str(&format!("Update File: {display_path}\n\n"));
        }

        for applied in &prepared.applied_edits {
            let edit_type = &applied.edit.edit_type;
            let search_lines: Vec<String>;
            let replace_lines: Vec<String>;

            if edit_type == "insert_after" {
                search_lines = vec![prepared.lines[applied.original_start_idx].clone()];
                replace_lines = vec![
                    prepared.lines[applied.original_start_idx].clone(),
                    applied.edit.text.clone(),
                ];
            } else if edit_type == "insert_before" {
                search_lines = vec![prepared.lines[applied.original_start_idx].clone()];
                replace_lines = vec![
                    applied.edit.text.clone(),
                    prepared.lines[applied.original_start_idx].clone(),
                ];
            } else {
                search_lines =
                    prepared.lines[applied.original_start_idx..=applied.original_end_idx].to_vec();
                replace_lines = if applied.edit.text.is_empty() {
                    Vec::new()
                } else {
                    split_content_lines(&applied.edit.text)
                };
            }

            if colored {
                diff.push_str(&format!(
                    "{}<<<<<<< SEARCH{}\n",
                    crate::cli::colors::style::RED,
                    crate::cli::colors::style::RESET
                ));
            } else {
                diff.push_str("<<<<<<< SEARCH\n");
            }
            for line in &search_lines {
                diff.push_str(&crate::cli::colors::diff_removal(&highlight_diff_content(
                    line, language, colored,
                )));
                diff.push('\n');
            }
            if colored {
                diff.push_str(&format!("{}=======\n", crate::cli::colors::style::GREEN));
            } else {
                diff.push_str("=======\n");
            }
            for line in &replace_lines {
                diff.push_str(&crate::cli::colors::diff_addition(&highlight_diff_content(
                    line, language, colored,
                )));
                diff.push('\n');
            }
            if colored {
                diff.push_str(&format!(
                    "{}>>>>>>> REPLACE{}\n\n",
                    crate::cli::colors::style::DIM,
                    crate::cli::colors::style::RESET
                ));
            } else {
                diff.push_str(">>>>>>> REPLACE\n\n");
            }
        }

        diff
    }

    /// Formats the final result for a batch.
    ///
    #[must_use]
    pub fn format_result(
        &self,
        display_path: &str,
        prepared: &PreparedEdits,
        final_lines: &[String],
        final_hashes: &[String],
        diagnostics: Option<&DiagnosticsResult>,
    ) -> String {
        self.format_result_with_failures(
            display_path,
            prepared,
            final_lines,
            final_hashes,
            diagnostics,
            0,
        )
    }

    /// Formats the final result with retry-aware recovery guidance.
    #[must_use]
    pub fn format_result_with_failures(
        &self,
        display_path: &str,
        prepared: &PreparedEdits,
        final_lines: &[String],
        final_hashes: &[String],
        diagnostics: Option<&DiagnosticsResult>,
        consecutive_failures: u32,
    ) -> String {
        let colored = !crate::cli::colors::stdout_colors_disabled();
        let language = syntax_language_for_path(display_path);
        let mut total_added = 0;
        let mut total_removed = 0;
        let mut applied_diffs: Vec<String> = Vec::new();

        for applied in &prepared.applied_edits {
            let (added, removed) = count_line_hash_changes(
                &prepared.line_hashes[applied.original_start_idx..=applied.original_end_idx],
                &final_hashes[applied.start_idx..=applied.end_idx],
            );
            total_added += added;
            total_removed += removed;

            let diff_block = if self.diff_mode == DiffMode::AdditionsOnly {
                self.get_addition_only_diff_block(
                    prepared,
                    final_lines,
                    final_hashes,
                    applied,
                    language,
                    colored,
                )
            } else {
                self.get_diff_block(
                    prepared,
                    final_lines,
                    final_hashes,
                    applied,
                    language,
                    colored,
                )
            };
            applied_diffs.push(diff_block);
        }

        let total_diff_lines: usize = applied_diffs.iter().map(|d| d.lines().count()).sum();
        let use_full_file =
            total_diff_lines > (final_lines.len() * 7 / 10) && !final_lines.is_empty();

        let mut results: Vec<String> = Vec::new();

        if self.diff_mode == DiffMode::Full && use_full_file {
            results.push(format!(
                "Because the changes were extensive, the full updated file content with anchors is provided below to ensure clarity:\n\n{}",
                final_lines
                    .iter()
                    .zip(final_hashes.iter())
                    .map(|(line, hash)| {
                        highlighted_line_with_hash(line, hash, language, colored)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        } else {
            results.extend(applied_diffs);
        }

        // Add failure messages
        for failed in &prepared.failed_edits {
            results.push(format!(
                "{}\nRecovery: {}",
                self.executor
                    .format_failure_message(&failed.edit, Some(&failed.error)),
                error_guidance::edit_failure_for_diagnostic(&failed.error, consecutive_failures)
            ));
        }

        let unchanged_count = prepared
            .resolved_edits
            .len()
            .saturating_sub(prepared.applied_edits.len());
        let unchanged_note = if unchanged_count == 0 {
            String::new()
        } else {
            format!(" {unchanged_count} edit(s) already matched the file.")
        };
        let failure_note = if prepared.failed_edits.is_empty() {
            String::new()
        } else {
            format!(" {} edit(s) failed.", prepared.failed_edits.len())
        };
        let summary = if prepared.applied_edits.is_empty() {
            format!("No changes applied.{unchanged_note}{failure_note}")
        } else {
            format!(
                "Applied {} edit(s) successfully (+{total_added}, -{total_removed} lines). NOTE the UPDATED anchors below.{unchanged_note}{failure_note}",
                prepared.applied_edits.len()
            )
        };

        // Add diagnostics feedback
        if let Some(diag) = diagnostics {
            if diag.fixed_count > 0 {
                results.push(format!("Fixed {} linter error(s).", diag.fixed_count));
            }
            if !diag.new_problems_message.is_empty() {
                results.push(format!(
                    "New problems detected after saving the file:\n{}",
                    diag.new_problems_message.trim()
                ));
            }
        }

        format!("{}\n\n{}", summary, results.join("\n\n---\n\n"))
    }

    #[allow(clippy::unused_self)]
    fn get_addition_only_diff_block(
        &self,
        prepared: &PreparedEdits,
        final_lines: &[String],
        final_hashes: &[String],
        applied: &AppliedEdit,
        language: &str,
        colored: bool,
    ) -> String {
        let context_count = 3;
        let mut res: Vec<String> = Vec::new();
        let final_range_end = if applied.start_idx < final_lines.len() {
            Some(applied.end_idx.min(final_lines.len() - 1))
        } else {
            None
        };

        // Context before (from original)
        let before_start = applied.original_start_idx.saturating_sub(context_count);
        for i in before_start..applied.original_start_idx {
            res.push(format!(
                " {}",
                highlighted_line_with_hash(
                    &prepared.lines[i],
                    &prepared.line_hashes[i],
                    language,
                    colored,
                )
            ));
        }

        // Deletion summary
        let final_hashes_set: std::collections::HashSet<&String> = final_range_end
            .map(|end| final_hashes[applied.start_idx..=end].iter().collect())
            .unwrap_or_default();
        let mut truly_removed_count = 0;
        for i in applied.original_start_idx..=applied.original_end_idx {
            if !final_hashes_set.contains(&prepared.line_hashes[i]) {
                truly_removed_count += 1;
            }
        }

        if truly_removed_count > 0 {
            res.push(format!(
                "{} lines between {} and {} have been deleted",
                truly_removed_count,
                applied
                    .edit
                    .anchor
                    .split("§")
                    .next()
                    .unwrap_or(&applied.edit.anchor),
                applied
                    .edit
                    .end_anchor
                    .as_deref()
                    .unwrap_or("")
                    .split("§")
                    .next()
                    .unwrap_or("")
            ));
        }

        // Added/neutral lines (from final)
        let original_hashes_set: std::collections::HashSet<&String> = prepared.line_hashes
            [applied.original_start_idx..=applied.original_end_idx]
            .iter()
            .collect();
        if let Some(end) = final_range_end {
            for i in applied.start_idx..=end {
                let hash = &final_hashes[i];
                let prefix = if original_hashes_set.contains(hash) {
                    " "
                } else {
                    "+"
                };
                res.push(format!(
                    "{}{}",
                    prefix,
                    highlighted_line_with_hash(&final_lines[i], hash, language, colored)
                ));
            }
        }

        // Context after (from final)
        if let Some(last_idx) = final_lines.len().checked_sub(1) {
            let after_start = applied.end_idx.saturating_add(1);
            let after_end = last_idx.min(applied.end_idx.saturating_add(context_count));
            if after_start <= after_end {
                for i in after_start..=after_end {
                    res.push(format!(
                        " {}",
                        highlighted_line_with_hash(
                            &final_lines[i],
                            &final_hashes[i],
                            language,
                            colored,
                        )
                    ));
                }
            }
        }

        res.join("\n")
    }

    #[allow(clippy::unused_self)]
    fn get_diff_block(
        &self,
        prepared: &PreparedEdits,
        final_lines: &[String],
        final_hashes: &[String],
        applied: &AppliedEdit,
        language: &str,
        colored: bool,
    ) -> String {
        let context_before_count = 3;
        let context_after_count = 3;
        let mut res: Vec<String> = Vec::new();
        let final_range_end = if applied.start_idx < final_lines.len() {
            Some(applied.end_idx.min(final_lines.len() - 1))
        } else {
            None
        };

        let before_start = applied
            .original_start_idx
            .saturating_sub(context_before_count);
        for i in before_start..applied.original_start_idx {
            res.push(crate::cli::colors::diff_context(
                &highlighted_line_with_hash(
                    &prepared.lines[i],
                    &prepared.line_hashes[i],
                    language,
                    colored,
                ),
            ));
        }

        let final_hashes_set: std::collections::HashSet<&String> = final_range_end
            .map(|end| final_hashes[applied.start_idx..=end].iter().collect())
            .unwrap_or_default();
        for i in applied.original_start_idx..=applied.original_end_idx {
            if !final_hashes_set.contains(&prepared.line_hashes[i]) {
                res.push(crate::cli::colors::diff_removal(
                    &highlighted_line_with_hash(
                        &prepared.lines[i],
                        &prepared.line_hashes[i],
                        language,
                        colored,
                    ),
                ));
            }
        }

        let original_hashes_set: std::collections::HashSet<&String> = prepared.line_hashes
            [applied.original_start_idx..=applied.original_end_idx]
            .iter()
            .collect();
        if let Some(end) = final_range_end {
            for i in applied.start_idx..=end {
                let hash = &final_hashes[i];
                let line = highlighted_line_with_hash(&final_lines[i], hash, language, colored);
                if original_hashes_set.contains(hash) {
                    res.push(crate::cli::colors::diff_context(&line));
                } else {
                    res.push(crate::cli::colors::diff_addition(&line));
                }
            }
        }

        if let Some(last_idx) = final_lines.len().checked_sub(1) {
            let after_start = applied.end_idx.saturating_add(1);
            let after_end = last_idx.min(applied.end_idx.saturating_add(context_after_count));
            if after_start <= after_end {
                for i in after_start..=after_end {
                    res.push(crate::cli::colors::diff_context(
                        &highlighted_line_with_hash(
                            &final_lines[i],
                            &final_hashes[i],
                            language,
                            colored,
                        ),
                    ));
                }
            }
        }

        res.join("\n")
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file_editor::AnchorStateManager;
    use crate::test_support::env_lock;

    #[test]
    fn test_batch_processor_group_edits() {
        let processor = BatchProcessor::new(DiffMode::Full);

        let edits = vec![
            (
                "src/main.rs".to_string(),
                vec![Edit {
                    anchor: "Apple§fn main()".to_string(),
                    end_anchor: Some("Banana§println!()".to_string()),
                    edit_type: "replace".to_string(),
                    text: "fn new_main()".to_string(),
                    content: None,
                }],
            ),
            (
                "src/lib.rs".to_string(),
                vec![Edit {
                    anchor: "Cherry§pub fn add()".to_string(),
                    end_anchor: None,
                    edit_type: "insert_after".to_string(),
                    text: "pub fn sub()".to_string(),
                    content: None,
                }],
            ),
        ];

        let batches = processor.group_edits_by_path(&edits, &|path| Some(format!("/tmp/{}", path)));

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].absolute_path, "/tmp/src/main.rs");
        assert_eq!(batches[0].edits.len(), 1);
        assert_eq!(batches[1].absolute_path, "/tmp/src/lib.rs");
    }

    #[test]
    fn test_batch_processor_validate_edit() {
        let processor = BatchProcessor::new(DiffMode::Full);

        // Valid replace edit
        let valid = Edit {
            anchor: "Apple§content".to_string(),
            end_anchor: Some("Banana§content".to_string()),
            edit_type: "replace".to_string(),
            text: "new content".to_string(),
            content: None,
        };
        assert!(processor.validate_edit(&valid).is_ok());

        // Omitted edit_type defaults to replace at the tool boundary.
        let default_replace = Edit {
            anchor: "Apple§content".to_string(),
            end_anchor: Some("Banana§content".to_string()),
            edit_type: "".to_string(),
            text: "new".to_string(),
            content: None,
        };
        assert!(processor.validate_edit(&default_replace).is_ok());

        // Missing anchor
        let invalid = Edit {
            anchor: "".to_string(),
            end_anchor: Some("Banana§content".to_string()),
            edit_type: "replace".to_string(),
            text: "new".to_string(),
            content: None,
        };
        assert!(processor.validate_edit(&invalid).is_err());

        // Missing end_anchor for replace: now auto-defaults to anchor (single-line replace)
        let valid = Edit {
            anchor: "Apple§content".to_string(),
            end_anchor: None,
            edit_type: "replace".to_string(),
            text: "new".to_string(),
            content: None,
        };
        assert!(processor.validate_edit(&valid).is_ok());

        // `content` requires both end_anchor and replace — silent
        // ignore would let the model think it supplied a fingerprint.
        let with_content_no_end = Edit {
            anchor: "Apple§content".to_string(),
            end_anchor: None,
            edit_type: "replace".to_string(),
            text: "new".to_string(),
            content: Some(vec!["middle".to_string()]),
        };
        assert!(processor.validate_edit(&with_content_no_end).is_err());

        let content_with_insert = Edit {
            anchor: "Apple§content".to_string(),
            end_anchor: Some("Banana§content".to_string()),
            edit_type: "insert_after".to_string(),
            text: "new".to_string(),
            content: Some(vec!["middle".to_string()]),
        };
        assert!(processor.validate_edit(&content_with_insert).is_err());

        let valid_fingerprint = Edit {
            anchor: "Apple§content".to_string(),
            end_anchor: Some("Banana§content".to_string()),
            edit_type: "replace".to_string(),
            text: "new".to_string(),
            content: Some(vec!["middle".to_string()]),
        };
        assert!(processor.validate_edit(&valid_fingerprint).is_ok());

        let oversized_fingerprint = Edit {
            anchor: "Apple§content".to_string(),
            end_anchor: Some("Banana§content".to_string()),
            edit_type: "replace".to_string(),
            text: "new".to_string(),
            content: Some(vec![
                "middle".to_string();
                MAX_FINGERPRINT_CONTENT_LINES + 1
            ]),
        };
        let error = processor
            .validate_edit(&oversized_fingerprint)
            .expect_err("oversized fingerprints must be rejected");
        assert!(error.to_string().contains("limited to"));
    }

    #[test]
    fn test_batch_processor_prepare_and_apply() {
        let task_id = "batch_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::Full);

        let content = "fn hello() {\n    println!(\"world\");\n    return 42;\n}";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/batch.rs", &lines, Some(task_id));

        let edits = vec![Edit {
            anchor: format!("{}§fn hello() {{", hashes[0]),
            end_anchor: Some(format!("{}§    println!(\"world\");", hashes[1])),
            edit_type: "replace".to_string(),
            text: "fn greeting() {\n    println!(\"hello\");".to_string(),
            content: None,
        }];

        let prepared =
            processor.prepare_edits("/tmp/batch.rs", "batch.rs", content, &edits, &hashes);
        assert!(prepared.is_ok());

        let mut prepared = prepared.unwrap();
        let result = processor.apply_batch(&mut prepared, "batch.rs", "batch.rs");

        assert!(result.success);
        assert_eq!(result.resolved_count, 1);
        assert_eq!(result.failed_count, 0);
        assert!(result.final_content.is_some());
        let final_content = result.final_content.unwrap();
        assert!(final_content.contains("fn greeting()"));
    }

    #[test]
    fn test_replace_preserves_trailing_empty_lines_in_content_and_diff() {
        let _env_lock = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let task_id = "replacement_trailing_lines_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));
        let processor = BatchProcessor::new(DiffMode::Full);
        let content = "line1\nline2";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/trailing.rs", &lines, Some(task_id));
        let edits = vec![Edit {
            anchor: format!("{}§line2", hashes[1]),
            end_anchor: Some(format!("{}§line2", hashes[1])),
            edit_type: "replace".to_string(),
            text: "replacement\n\n".to_string(),
            content: None,
        }];

        let mut prepared = processor
            .prepare_edits("/tmp/trailing.rs", "trailing.rs", content, &edits, &hashes)
            .unwrap();
        let result = processor.apply_batch(&mut prepared, "trailing.rs", "trailing.rs");

        assert!(result.success);
        assert_eq!(
            result.final_content.as_deref(),
            Some("line1\nreplacement\n\n")
        );
        assert_eq!(
            prepared.final_lines,
            split_content_lines("line1\nreplacement\n\n")
        );
        let expected_diff = format!(
            "{}\n{}\n{}\n",
            crate::cli::colors::diff_addition("replacement"),
            crate::cli::colors::diff_addition(""),
            crate::cli::colors::diff_addition("")
        );
        assert!(prepared.diff.contains(&expected_diff));
    }

    #[test]
    fn test_batch_processor_full_diff_handles_deleting_entire_file() {
        let task_id = "full_delete_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::Full);

        let content = "only line";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/full_delete.rs", &lines, Some(task_id));

        let edits = vec![Edit {
            anchor: format!("{}§only line", hashes[0]),
            end_anchor: Some(format!("{}§only line", hashes[0])),
            edit_type: "replace".to_string(),
            text: String::new(),
            content: None,
        }];

        let mut prepared = processor
            .prepare_edits(
                "/tmp/full_delete.rs",
                "full_delete.rs",
                content,
                &edits,
                &hashes,
            )
            .unwrap();
        let result = processor.apply_batch(&mut prepared, "full_delete.rs", "full_delete.rs");

        assert!(result.success);
        assert_eq!(result.final_content.as_deref(), Some(""));
        assert!(prepared.diff.contains("- only line"));
    }

    #[test]
    fn test_batch_processor_additions_only_handles_deleting_last_line() {
        let task_id = "delete_last_line_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::AdditionsOnly);

        let content = "line1\nline2";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/delete_last.rs", &lines, Some(task_id));

        let edits = vec![Edit {
            anchor: format!("{}§line2", hashes[1]),
            end_anchor: Some(format!("{}§line2", hashes[1])),
            edit_type: "replace".to_string(),
            text: String::new(),
            content: None,
        }];

        let mut prepared = processor
            .prepare_edits(
                "/tmp/delete_last.rs",
                "delete_last.rs",
                content,
                &edits,
                &hashes,
            )
            .unwrap();
        let result = processor.apply_batch(&mut prepared, "delete_last.rs", "delete_last.rs");

        assert!(result.success);
        assert_eq!(result.final_content.as_deref(), Some("line1"));
        assert!(!prepared.diff.is_empty());
    }

    #[test]
    fn test_batch_processor_overlap_counts_all_failed_edits() {
        let task_id = "overlap_count_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::Full);

        let content = "line1\nline2\nline3";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/overlap.rs", &lines, Some(task_id));

        let edits = vec![
            Edit {
                anchor: format!("{}§line1", hashes[0]),
                end_anchor: Some(format!("{}§line2", hashes[1])),
                edit_type: "replace".to_string(),
                text: "alpha".to_string(),
                content: None,
            },
            Edit {
                anchor: format!("{}§line2", hashes[1]),
                end_anchor: Some(format!("{}§line3", hashes[2])),
                edit_type: "replace".to_string(),
                text: "beta".to_string(),
                content: None,
            },
            Edit {
                anchor: "bogus§missing".to_string(),
                end_anchor: Some("bogus§missing".to_string()),
                edit_type: "replace".to_string(),
                text: "gamma".to_string(),
                content: None,
            },
        ];

        let mut prepared = processor
            .prepare_edits("/tmp/overlap.rs", "overlap.rs", content, &edits, &hashes)
            .unwrap();
        assert_eq!(prepared.resolved_edits.len(), 2);
        assert_eq!(prepared.failed_edits.len(), 1);

        let result = processor.apply_batch(&mut prepared, "overlap.rs", "overlap.rs");

        assert!(!result.success);
        assert!(result.overlap);
        assert_eq!(result.resolved_count, 2);
        assert_eq!(result.failed_count, 1);
    }

    #[test]
    fn test_batch_processor_format_result() {
        let task_id = "format_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::Full);

        let content = "line1\nline2\nline3";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/format.rs", &lines, Some(task_id));

        let edits = vec![Edit {
            anchor: format!("{}§line2", hashes[1]),
            end_anchor: Some(format!("{}§line2", hashes[1])),
            edit_type: "replace".to_string(),
            text: "new_line2".to_string(),
            content: None,
        }];

        let mut prepared = processor
            .prepare_edits("/tmp/format.rs", "format.rs", content, &edits, &hashes)
            .unwrap();
        let _result = processor.apply_batch(&mut prepared, "format.rs", "format.rs");

        let final_lines = prepared.final_lines.clone();
        let final_hashes = anchor_mgr.reconcile("/tmp/format.rs", &final_lines, Some(task_id));

        let formatted =
            processor.format_result("format.rs", &prepared, &final_lines, &final_hashes, None);

        assert!(formatted.contains("Applied 1 edit(s) successfully"));
        assert!(formatted.contains("NOTE the UPDATED anchors below"));
    }

    #[test]
    fn test_format_result_counts_duplicate_hash_occurrences() {
        let edit = Edit {
            anchor: "Apple§same".to_string(),
            end_anchor: Some("Banana§same".to_string()),
            edit_type: "replace".to_string(),
            text: "same".to_string(),
            content: None,
        };
        let prepared = PreparedEdits {
            content: "same\nsame".to_string(),
            final_content: "same".to_string(),
            diff: String::new(),
            resolved_edits: vec![ResolvedEdit {
                line_idx: 0,
                end_idx: 1,
                edit: edit.clone(),
            }],
            failed_edits: Vec::new(),
            applied_edits: vec![AppliedEdit {
                start_idx: 0,
                end_idx: 0,
                original_start_idx: 0,
                original_end_idx: 1,
                edit,
                lines_added: 1,
                lines_deleted: 2,
            }],
            lines: vec!["same".to_string(), "same".to_string()],
            line_hashes: vec!["duplicate".to_string(), "duplicate".to_string()],
            final_lines: vec!["same".to_string()],
            initial_mtime: None,
        };
        let processor = BatchProcessor::new(DiffMode::Full);
        let formatted = processor.format_result(
            "duplicate.rs",
            &prepared,
            &prepared.final_lines,
            &["duplicate".to_string()],
            None,
        );

        assert!(formatted.contains("(+0, -1 lines)"));
    }

    #[test]
    fn test_batch_processor_additions_only_mode() {
        let task_id = "additions_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::AdditionsOnly);

        let content = "line1\nline2\nline3";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/additions.rs", &lines, Some(task_id));

        let edits = vec![Edit {
            anchor: format!("{}§line2", hashes[1]),
            end_anchor: Some(format!("{}§line2", hashes[1])),
            edit_type: "replace".to_string(),
            text: "new_line2".to_string(),
            content: None,
        }];

        let mut prepared = processor
            .prepare_edits(
                "/tmp/additions.rs",
                "additions.rs",
                content,
                &edits,
                &hashes,
            )
            .unwrap();
        let _result = processor.apply_batch(&mut prepared, "additions.rs", "additions.rs");

        let final_lines = prepared.final_lines.clone();
        let final_hashes = anchor_mgr.reconcile("/tmp/additions.rs", &final_lines, Some(task_id));

        let formatted =
            processor.format_result("additions.rs", &prepared, &final_lines, &final_hashes, None);

        // In additions-only mode, deleted lines should show as "X lines have been deleted"
        assert!(formatted.contains("Applied 1 edit(s) successfully"));
    }

    #[test]
    fn test_format_result_diagnostics() {
        let task_id = "diagnostics_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::Full);

        let content = "line1\nline2\nline3";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/diag.rs", &lines, Some(task_id));

        let edits = vec![Edit {
            anchor: format!("{}§line2", hashes[1]),
            end_anchor: Some(format!("{}§line2", hashes[1])),
            edit_type: "replace".to_string(),
            text: "new_line2".to_string(),
            content: None,
        }];

        let mut prepared = processor
            .prepare_edits("/tmp/diag.rs", "diag.rs", content, &edits, &hashes)
            .unwrap();
        let _result = processor.apply_batch(&mut prepared, "diag.rs", "diag.rs");

        let final_lines = prepared.final_lines.clone();
        let final_hashes = anchor_mgr.reconcile("/tmp/diag.rs", &final_lines, Some(task_id));

        let diagnostics = DiagnosticsResult {
            fixed_count: 2,
            new_problems_message:
                "error[E0425]: cannot find value `x` in this scope\n --> src/main.rs:2:5"
                    .to_string(),
        };

        let formatted = processor.format_result(
            "diag.rs",
            &prepared,
            &final_lines,
            &final_hashes,
            Some(&diagnostics),
        );

        assert!(formatted.contains("Fixed 2 linter error(s)."));
        assert!(formatted.contains("New problems detected after saving the file:"));
        assert!(formatted.contains("error[E0425]"));
    }

    #[test]
    fn test_generate_diff_colors_additions_and_removals() {
        let task_id = "diff_color_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::Full);

        let content = "line1\nline2\nline3";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/color_test.rs", &lines, Some(task_id));

        let edits = vec![Edit {
            anchor: format!("{}§line2", hashes[1]),
            end_anchor: Some(format!("{}§line2", hashes[1])),
            edit_type: "replace".to_string(),
            text: "new_line2\nnew_line2b".to_string(),
            content: None,
        }];

        let mut prepared = processor
            .prepare_edits(
                "/tmp/color_test.rs",
                "color_test.rs",
                content,
                &edits,
                &hashes,
            )
            .unwrap();

        // Apply the edits to populate applied_edits
        let _result = processor.apply_batch(&mut prepared, "color_test.rs", "color_test.rs");

        let diff = processor.generate_diff("color_test.rs", &prepared);

        // In test environment (non-TTY), colors are disabled, so we verify the structure
        // The diff should contain the markers with proper line prefixes
        assert!(
            diff.contains("<<<<<<< SEARCH"),
            "Should contain SEARCH marker"
        );
        assert!(diff.contains("======="), "Should contain separator");
        assert!(
            diff.contains(">>>>>>> REPLACE"),
            "Should contain REPLACE marker"
        );
        assert!(
            diff.contains("- line2"),
            "Should contain removal line with prefix"
        );
        assert!(
            diff.contains("+ new_line2"),
            "Should contain addition line with prefix"
        );
        assert!(
            diff.contains("+ new_line2b"),
            "Should contain second addition line"
        );

        // Note: ANSI color codes won't be present in non-TTY test environment
        // This is correct behavior - colors respect NO_COLOR and TTY detection
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn test_diff_content_uses_file_language_for_highlighting() {
        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let previous = std::env::var_os("NO_COLOR");
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        let highlighted = highlight_diff_content("let value: i32 = 1;", "rs", true);
        unsafe {
            match previous {
                Some(value) => std::env::set_var("NO_COLOR", value),
                None => std::env::remove_var("NO_COLOR"),
            }
        }

        assert!(highlighted.contains("\x1b["));
        assert!(highlighted.contains("let"));
        assert!(highlighted.contains("i32"));
    }

    #[test]
    fn test_generate_diff_respects_no_color() {
        // Set NO_COLOR to test plain text output
        // SAFETY: single-threaded test; sequential env mutation
        let _env_lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }

        let task_id = "diff_no_color_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let processor = BatchProcessor::new(DiffMode::Full);

        let content = "line1\nline2\nline3";
        let lines = split_content_lines(content);
        let hashes = anchor_mgr.reconcile("/tmp/no_color_test.rs", &lines, Some(task_id));

        let edits = vec![Edit {
            anchor: format!("{}§line2", hashes[1]),
            end_anchor: Some(format!("{}§line2", hashes[1])),
            edit_type: "replace".to_string(),
            text: "new_line2".to_string(),
            content: None,
        }];

        let mut prepared = processor
            .prepare_edits(
                "/tmp/no_color_test.rs",
                "no_color_test.rs",
                content,
                &edits,
                &hashes,
            )
            .unwrap();

        let _result = processor.apply_batch(&mut prepared, "no_color_test.rs", "no_color_test.rs");

        let diff = processor.generate_diff("no_color_test.rs", &prepared);

        // With NO_COLOR set, diff should NOT contain ANSI codes for line prefixes
        assert!(!diff.contains("\x1b[91m- ")); // Should be plain "- " not colored
        assert!(!diff.contains("\x1b[92m+ ")); // Should be plain "+ " not colored

        // Cleanup
        // SAFETY: single-threaded test; restoring env after test
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
    }
}
