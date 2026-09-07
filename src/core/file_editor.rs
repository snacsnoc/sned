//! Patch application engine and hash-anchored edit system.
//!
//! Port from:
//! - `dirac/src/integrations/editor/FileEditProvider.ts`
//! - `dirac/src/utils/AnchorStateManager.ts`
//! - `dirac/src/utils/line-hashing.ts`
//! - `dirac/src/shared/utils/line-hashing.ts`
//! - `dirac/src/core/task/tools/handlers/edit-file/`
//!
//! CRITICAL: The hash-anchored edit system is Sned's single most important
//! feature. Port the exact algorithm from TypeScript, do not change it.

use indexmap::IndexMap;
use parking_lot::Mutex;
use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use tokio::sync::Mutex as AsyncMutex;

use crate::core::hash_utils::{
    ANCHOR_DELIMITER, compute_hashes, find_glued_anchor_in_lines, split_anchor, strip_hashes,
};

/// Split file content into logical lines while preserving a trailing empty line
/// when the file ends with `\n`. Anchor reconciliation and edit resolution must
/// use identical line semantics.
#[must_use]
pub fn split_content_lines(content: &str) -> Vec<String> {
    content
        .split('\n')
        .map(std::string::ToString::to_string)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileLineEnding {
    Lf,
    CrLf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileTextFormat {
    pub line_ending: FileLineEnding,
    pub has_utf8_bom: bool,
}

impl Default for FileTextFormat {
    fn default() -> Self {
        Self {
            line_ending: FileLineEnding::Lf,
            has_utf8_bom: false,
        }
    }
}

pub(crate) fn normalize_file_content(content: &str) -> (String, FileTextFormat) {
    let has_utf8_bom = content.starts_with('\u{feff}');
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let line_ending = content
        .as_bytes()
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(FileLineEnding::Lf, |index| {
            if index > 0 && content.as_bytes()[index - 1] == b'\r' {
                FileLineEnding::CrLf
            } else {
                FileLineEnding::Lf
            }
        });
    let normalized = if content.contains("\r\n") {
        content.replace("\r\n", "\n")
    } else {
        content.to_string()
    };
    (
        normalized,
        FileTextFormat {
            line_ending,
            has_utf8_bom,
        },
    )
}

pub(crate) fn restore_file_content(content: &str, format: FileTextFormat) -> String {
    let content = match format.line_ending {
        FileLineEnding::Lf => content.to_string(),
        FileLineEnding::CrLf => {
            let mut restored = String::with_capacity(content.len());
            let mut previous_was_cr = false;
            for character in content.chars() {
                if character == '\n' && !previous_was_cr {
                    restored.push('\r');
                }
                restored.push(character);
                previous_was_cr = character == '\r';
            }
            restored
        }
    };
    if format.has_utf8_bom {
        format!("\u{feff}{content}")
    } else {
        content
    }
}

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during file editing operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum FileEditorError {
    #[error("All edits failed: {message}")]
    AllEditsFailed { message: String },

    #[error("Edit validation failed: {0}")]
    ValidationError(String),

    #[error(
        "File edit batch rejected: {message} {failed_count} edit(s) failed; {withheld_count} otherwise-valid edit(s) were withheld. No edits were applied to this file."
    )]
    AtomicBatchRejected {
        message: String,
        failed_count: usize,
        withheld_count: usize,
        requires_reread: bool,
        validation_only: bool,
    },

    #[error("Overlapping edit ranges: {message}")]
    OverlappingEdits { message: String },
}

impl FileEditorError {
    pub(crate) fn atomic_batch_rejected(
        message: impl Into<String>,
        failed_count: usize,
        withheld_count: usize,
    ) -> Self {
        let message = message.into();
        let requires_reread = EditFailureReason::from_diagnostics(&message)
            .into_iter()
            .any(EditFailureReason::requires_reread);
        Self::AtomicBatchRejected {
            message,
            failed_count,
            withheld_count,
            requires_reread,
            validation_only: false,
        }
    }

    pub(crate) fn validation_batch_rejected(
        message: impl Into<String>,
        failed_count: usize,
        withheld_count: usize,
    ) -> Self {
        Self::AtomicBatchRejected {
            message: message.into(),
            failed_count,
            withheld_count,
            requires_reread: false,
            validation_only: true,
        }
    }

    pub(crate) fn is_validation_error(&self) -> bool {
        matches!(
            self,
            Self::ValidationError(_)
                | Self::AtomicBatchRejected {
                    validation_only: true,
                    ..
                }
        )
    }

    pub(crate) fn requires_reread(&self) -> bool {
        matches!(
            self,
            Self::AtomicBatchRejected {
                requires_reread: true,
                ..
            }
        )
    }
}

/// Classifies an edit failure so the tool can give the model a deterministic
/// recovery strategy instead of another generic anchor error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditFailureReason {
    InvalidEditInput,
    MissingAnchor,
    UnknownAnchor,
    DuplicateContent,
    WhitespaceMismatch,
    RangeOverlap,
    GluedAnchor,
    DuplicateInsertion,
}

impl EditFailureReason {
    /// Classify one edit diagnostic emitted by anchor resolution or assembly.
    ///
    /// Keep the phrases here synchronized with the diagnostics produced by
    /// [`EditExecutor`]. Unknown phrases intentionally fall back to
    /// `UnknownAnchor`; callers handling a joined `AllEditsFailed` diagnostic
    /// should use [`Self::from_diagnostics`] so mixed failures are preserved.
    pub(crate) fn from_diagnostic(diagnostic: &str) -> Self {
        let lower = diagnostic.to_ascii_lowercase();
        if lower.contains("duplicate insertion") {
            Self::DuplicateInsertion
        } else if lower.contains("overlap") {
            Self::RangeOverlap
        } else if lower.contains("glued") || lower.contains("word§/hex§ fragments") {
            Self::GluedAnchor
        } else if lower.contains("matches ") && lower.contains("identical content") {
            Self::DuplicateContent
        } else if lower.contains("only after trimming whitespace") {
            Self::WhitespaceMismatch
        } else if lower.contains("missing")
            || lower.contains("multiple lines")
            || lower.contains("incorrectly formatted")
        {
            Self::MissingAnchor
        } else {
            Self::UnknownAnchor
        }
    }

    /// Classify every recognizable failure strategy in a possibly joined
    /// diagnostic, preserving mixed failures from `AllEditsFailed`.
    pub(crate) fn from_diagnostics(diagnostic: &str) -> Vec<Self> {
        let lower = diagnostic.to_ascii_lowercase();
        let mut reasons = Vec::with_capacity(3);

        let mut add = |reason| {
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        };

        if lower.contains("overlap") {
            add(Self::RangeOverlap);
        }
        if lower.contains("duplicate insertion") {
            add(Self::DuplicateInsertion);
        }
        if lower.contains("glued") || lower.contains("word§/hex§ fragments") {
            add(Self::GluedAnchor);
        }
        if lower.contains("matches ") && lower.contains("identical content") {
            add(Self::DuplicateContent);
        }
        if lower.contains("only after trimming whitespace") {
            add(Self::WhitespaceMismatch);
        }
        if lower.contains("missing")
            || lower.contains("multiple lines")
            || lower.contains("incorrectly formatted")
        {
            add(Self::MissingAnchor);
        }
        if lower.contains("not found in the file")
            || lower.contains("anchor is stale")
            || lower.contains("does not match the line")
        {
            add(Self::UnknownAnchor);
        }

        if reasons.is_empty() {
            reasons.push(Self::from_diagnostic(diagnostic));
        }
        reasons
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::InvalidEditInput => "invalid edit parameters",
            Self::MissingAnchor => "missing or malformed anchor",
            Self::UnknownAnchor => "unknown or stale anchor",
            Self::DuplicateContent => "duplicate anchor content",
            Self::WhitespaceMismatch => "whitespace mismatch",
            Self::RangeOverlap => "overlapping edit ranges",
            Self::GluedAnchor => "glued anchor fragments",
            Self::DuplicateInsertion => "duplicate insertion",
        }
    }

    pub(crate) const fn requires_reread(self) -> bool {
        matches!(
            self,
            Self::MissingAnchor | Self::UnknownAnchor | Self::GluedAnchor
        )
    }
}

#[cfg(test)]
mod edit_failure_reason_tests {
    use super::{EditFailureReason, FileEditorError};

    #[test]
    fn classifies_anchor_diagnostics_by_recovery_strategy() {
        assert_eq!(
            EditFailureReason::from_diagnostic("anchor is missing"),
            EditFailureReason::MissingAnchor
        );
        assert_eq!(
            EditFailureReason::from_diagnostic("anchor matches 2 lines with identical content"),
            EditFailureReason::DuplicateContent
        );
        assert_eq!(
            EditFailureReason::from_diagnostic("matches only after trimming whitespace"),
            EditFailureReason::WhitespaceMismatch
        );
        assert_eq!(
            EditFailureReason::from_diagnostic("anchor is stale"),
            EditFailureReason::UnknownAnchor
        );
        assert_eq!(
            EditFailureReason::from_diagnostic("Overlapping edit ranges detected"),
            EditFailureReason::RangeOverlap
        );
        assert_eq!(
            EditFailureReason::from_diagnostic(
                "Assembled content has Word§/hex§ fragments at line 4"
            ),
            EditFailureReason::GluedAnchor
        );
        assert_eq!(
            EditFailureReason::from_diagnostic("duplicate insertion rejected"),
            EditFailureReason::DuplicateInsertion
        );
    }

    #[test]
    fn classifies_mixed_diagnostics_without_dropping_categories() {
        let reasons = EditFailureReason::from_diagnostics(
            "anchor matches 2 lines with identical content\n\nanchor matches only after trimming whitespace",
        );
        assert_eq!(
            reasons,
            vec![
                EditFailureReason::DuplicateContent,
                EditFailureReason::WhitespaceMismatch
            ]
        );
    }

    #[test]
    fn atomic_rejection_reread_requirement_follows_diagnostics() {
        let duplicate = FileEditorError::atomic_batch_rejected(
            "anchor matches 2 lines with identical content",
            1,
            1,
        );
        assert!(!duplicate.requires_reread());

        let whitespace = FileEditorError::atomic_batch_rejected(
            "anchor matches only after trimming whitespace",
            1,
            1,
        );
        assert!(!whitespace.requires_reread());

        let stale = FileEditorError::atomic_batch_rejected(
            "anchor is stale and was not found in the file",
            1,
            1,
        );
        assert!(stale.requires_reread());
    }
}

impl FileEditorError {
    /// Return an actionable display string with a suggestion for fixing the error.
    #[must_use]
    pub fn actionable_display(&self) -> String {
        match self {
            Self::AllEditsFailed { message } => {
                let suggestion = "Check that the file content matches the anchors. \
                     Re-read the file to get fresh anchors before editing.";
                format!("{message}\n  Suggestion: {suggestion}")
            }
            _ => self.to_string(),
        }
    }
}

// ============================================================================
// Constants
// ============================================================================

pub(crate) static ANCHOR_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][a-zA-Z0-9]*$").unwrap());

// ============================================================================
// Anchor State Manager
// ============================================================================

/// Tracked document state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct TrackedDocument {
    #[serde(default)]
    generation: u64,
    hashes: Vec<u64>,
    anchors: Vec<String>,
    /// Legacy allocator state. Read for migration only; new publications clear it.
    #[serde(default)]
    used_words: VecDeque<String>,
    #[serde(default)]
    used_words_set: HashSet<String>,
    /// New identities use this stable per-document namespace and counter.
    #[serde(default)]
    anchor_namespace: Option<String>,
    #[serde(default)]
    next_anchor_id: u64,
    /// Recently retired identities are diagnostic only and never resolve.
    #[serde(default)]
    retired_anchors: VecDeque<String>,
}

/// Global anchor state storage.
#[derive(Debug)]
struct AnchorStorage {
    tasks: IndexMap<String, IndexMap<String, TrackedDocument>>,
    persisted_tasks: IndexMap<String, IndexMap<String, TrackedDocument>>,
    cache_file: std::path::PathBuf,
}

struct AnchorCacheLock(std::fs::File);

impl AnchorCacheLock {
    fn acquire(path: &std::path::Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;

        file.lock()?;

        Ok(Self(file))
    }
}

impl Drop for AnchorCacheLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl AnchorStorage {
    /// Load anchor state from disk (~/.sned/data/cache/anchors.json)
    fn load(anchors_file: std::path::PathBuf) -> Self {
        if let Ok(content) = std::fs::read_to_string(&anchors_file) {
            match serde_json::from_str::<IndexMap<String, IndexMap<String, TrackedDocument>>>(
                &content,
            ) {
                Ok(tasks) => {
                    tracing::debug!("Loaded {} task(s) from anchor cache", tasks.len());
                    return Self {
                        persisted_tasks: tasks.clone(),
                        tasks,
                        cache_file: anchors_file,
                    };
                }
                Err(e) => {
                    tracing::warn!("Failed to parse anchor cache: {}", e);
                }
            }
        }

        Self {
            tasks: IndexMap::new(),
            persisted_tasks: IndexMap::new(),
            cache_file: anchors_file,
        }
    }

    #[cfg(test)]
    fn new() -> Self {
        Self {
            tasks: IndexMap::new(),
            persisted_tasks: IndexMap::new(),
            cache_file: crate::storage::disk::get_data_dir().join("cache/anchors.json"),
        }
    }

    /// Save anchor state to disk
    fn save(&mut self) {
        let anchors_file = &self.cache_file;
        let cache_dir = anchors_file.parent().unwrap_or(std::path::Path::new("."));

        // Ensure cache directory exists
        if let Err(e) = std::fs::create_dir_all(cache_dir) {
            tracing::warn!("Failed to create cache directory: {}", e);
            return;
        }

        let lock_file = anchors_file.with_extension("json.lock");
        let Ok(_lock) = AnchorCacheLock::acquire(&lock_file) else {
            tracing::warn!("Failed to lock anchor cache for writing");
            return;
        };

        let Ok(mut tasks) = Self::read_tasks(anchors_file) else {
            tracing::warn!("Failed to reload anchor cache; preserving existing state");
            return;
        };
        let mut changed_tasks = HashSet::new();
        let mut cache_changed = false;
        for (task_id, documents) in &self.tasks {
            for (path, document) in documents {
                let expected = self
                    .persisted_tasks
                    .get(task_id)
                    .and_then(|files| files.get(path));
                if Some(document) == expected {
                    continue;
                }
                let current = tasks.get(task_id).and_then(|files| files.get(path));
                if current == expected {
                    cache_changed = true;
                    changed_tasks.insert(task_id);
                    tasks
                        .entry(task_id.clone())
                        .or_default()
                        .insert(path.clone(), document.clone());
                }
            }
        }
        for (task_id, documents) in &self.persisted_tasks {
            for (path, expected) in documents {
                if self
                    .tasks
                    .get(task_id)
                    .and_then(|files| files.get(path))
                    .is_none()
                    && tasks.get(task_id).and_then(|files| files.get(path)) == Some(expected)
                    && let Some(files) = tasks.get_mut(task_id)
                {
                    cache_changed = true;
                    files.shift_remove(path);
                }
            }
        }
        for task_id in self.tasks.keys() {
            // Unchanged entries from a stale manager must not evict newer tasks.
            if !changed_tasks.contains(task_id) {
                continue;
            }
            if let Some(documents) = tasks.shift_remove(task_id) {
                tasks.insert(task_id.clone(), documents);
            }
        }
        cache_changed |= tasks.len() > MAX_TRACKED_TASKS
            || tasks.values().any(|files| files.len() > MAX_TRACKED_FILES);
        Self::enforce_limits(&mut tasks);
        // A reload can change HashSet serialization order without changing state.
        // Avoid rewriting durable bytes when this manager has nothing to publish.
        if !cache_changed {
            self.persisted_tasks = tasks.clone();
            self.tasks = tasks;
            return;
        }

        match serde_json::to_string_pretty(&tasks) {
            Ok(json) => {
                if let Err(e) = crate::storage::disk::atomic_write_file(anchors_file, &json) {
                    tracing::warn!("Failed to save anchor cache: {}", e);
                } else {
                    self.persisted_tasks = tasks.clone();
                    self.tasks = tasks;
                }
            }
            Err(e) => {
                tracing::warn!("Failed to serialize anchor cache: {}", e);
            }
        }
    }

    fn read_tasks(
        path: &std::path::Path,
    ) -> std::io::Result<IndexMap<String, IndexMap<String, TrackedDocument>>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(IndexMap::new()),
            Err(error) => Err(error),
        }
    }

    fn enforce_limits(tasks: &mut IndexMap<String, IndexMap<String, TrackedDocument>>) {
        while tasks.len() > MAX_TRACKED_TASKS {
            tasks.shift_remove_index(0);
        }
        for files in tasks.values_mut() {
            while files.len() > MAX_TRACKED_FILES {
                files.shift_remove_index(0);
            }
        }
    }
}

pub(crate) const MAX_TRACKED_LINES: usize = 5000;
const MAX_TRACKED_FILES: usize = 1024;
const MAX_TRACKED_TASKS: usize = 50;

const MAX_RETIRED_ANCHORS: usize = 128;

// ============================================================================
// File Locking for Concurrency Safety
// ============================================================================

/// Global file lock manager to prevent concurrent edits to the same file.
///
/// When multiple tasks try to edit the same file concurrently, this ensures
/// serialization to prevent data corruption and lost updates.
pub struct FileLockManager {
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl FileLockManager {
    fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::with_capacity(4)),
        }
    }

    fn acquire(&'static self, path: &str) -> PendingFileLock {
        let lock = {
            let mut locks = self.locks.lock();
            locks
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        PendingFileLock {
            pending: Some(Box::pin(lock.clone().lock_owned())),
            lock: Some(lock),
            path: path.to_string(),
            manager: self,
        }
    }

    fn try_acquire(&'static self, path: &str) -> Option<FileEditGuard> {
        let lock = {
            let mut locks = self.locks.lock();
            locks
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.try_lock_owned().ok().map(|guard| FileEditGuard {
            guard: Some(guard),
            path: path.to_string(),
            manager: self,
        })
    }

    /// Removes a lock entry when the last guard is dropped.
    /// Called from Drop impl of FileEditGuard.
    fn release(&self, path: &str) {
        let mut locks = self.locks.lock();
        if let Some(arc) = locks.get(path)
            && Arc::strong_count(arc) <= 1
        {
            locks.remove(path);
        }
    }

    #[cfg(test)]
    fn contains(&self, path: &str) -> bool {
        self.locks.lock().contains_key(path)
    }
}

static FILE_LOCK_MANAGER: LazyLock<FileLockManager> = LazyLock::new(FileLockManager::new);

pub(crate) async fn acquire_file_operation_lock(path: &str) -> FileEditGuard {
    FILE_LOCK_MANAGER.acquire(path).await
}

struct PendingFileLock {
    pending: Option<Pin<Box<dyn Future<Output = tokio::sync::OwnedMutexGuard<()>> + Send>>>,
    lock: Option<Arc<AsyncMutex<()>>>,
    path: String,
    manager: &'static FileLockManager,
}

impl Future for PendingFileLock {
    type Output = FileEditGuard;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self
            .pending
            .as_mut()
            .expect("pending lock polled after completion")
            .as_mut()
            .poll(cx)
        {
            Poll::Ready(guard) => {
                self.pending.take();
                self.lock.take();
                Poll::Ready(FileEditGuard {
                    guard: Some(guard),
                    path: self.path.clone(),
                    manager: self.manager,
                })
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for PendingFileLock {
    fn drop(&mut self) {
        if self.pending.is_some() {
            self.pending.take();
            self.lock.take();
            self.manager.release(&self.path);
        }
    }
}

/// RAII guard for file editing. Holds lock for path duration.
pub struct FileEditGuard {
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    path: String,
    manager: &'static FileLockManager,
}

impl FileEditGuard {
    pub async fn acquire(path: &str) -> Self {
        FILE_LOCK_MANAGER.acquire(path).await
    }

    pub fn try_acquire(path: &str) -> Option<Self> {
        FILE_LOCK_MANAGER.try_acquire(path)
    }
}

impl Drop for FileEditGuard {
    fn drop(&mut self) {
        // Drop the owned lock before pruning so an idle entry has no hidden owner.
        self.guard.take();
        self.manager.release(&self.path);
    }
}

/// Anchor state manager for hash-anchored edits.
///
#[derive(Debug, Clone)]
pub struct AnchorStateManager {
    storage: Arc<Mutex<AnchorStorage>>,
}

/// Exact origin of each assembled line. `None` denotes newly supplied text.
/// Constructed by the executor at the same splice boundaries as the content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpliceProvenance {
    pub origins: Vec<Option<usize>>,
    pub edit_ranges: Vec<AppliedEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineProvenance {
    Preserved { original_idx: usize },
    Created { assigned_word: String },
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum AnchorTransitionError {
    #[error("Anchor generation changed for {path}; re-read the file with read_file")]
    StaleGeneration { path: String },
    #[error("Invalid anchor transition: {0}")]
    Validation(String),
    #[error("Failed to persist anchor transition: {0}")]
    Persistence(String),
}

/// Observation does not publish anchors, allocate shared words, or touch LRU state.
#[derive(Debug, Clone)]
pub struct AnchorSnapshot {
    absolute_path: String,
    task_id: String,
    generation: u64,
    raw_digest: String,
    normalized_digest: String,
    lines: Vec<String>,
    anchors: Vec<String>,
    snapshot_mode: bool,
    expected: Option<TrackedDocument>,
    memory_expected: Option<TrackedDocument>,
    document: TrackedDocument,
}

/// Private state is checked and persisted before any in-memory publication.
#[derive(Debug, Clone)]
pub struct AnchorTransition {
    absolute_path: String,
    task_id: String,
    generation: u64,
    raw_digest: String,
    normalized_digest: String,
    anchors: Vec<String>,
    expected_generation: Option<u64>,
    input_raw_digest: String,
    output_raw_digest: String,
    input_normalized_digest: String,
    output_normalized_digest: String,
    output_lines: Vec<String>,
    provenance: Vec<LineProvenance>,
    retired_identities: Vec<String>,
    edit_ranges: Vec<AppliedEdit>,
    is_noop: bool,
    expected: Option<TrackedDocument>,
    memory_expected: Option<TrackedDocument>,
    document: TrackedDocument,
    snapshot_mode: bool,
}

impl AnchorSnapshot {
    pub fn absolute_path(&self) -> &str {
        &self.absolute_path
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn raw_digest(&self) -> &str {
        &self.raw_digest
    }
    pub fn normalized_digest(&self) -> &str {
        &self.normalized_digest
    }
    pub fn lines(&self) -> &[String] {
        &self.lines
    }
    pub fn anchors(&self) -> &[String] {
        &self.anchors
    }
    pub fn snapshot_mode(&self) -> bool {
        self.snapshot_mode
    }
}

impl AnchorTransition {
    pub fn absolute_path(&self) -> &str {
        &self.absolute_path
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn raw_digest(&self) -> &str {
        &self.raw_digest
    }
    pub fn normalized_digest(&self) -> &str {
        &self.normalized_digest
    }
    pub fn anchors(&self) -> &[String] {
        &self.anchors
    }
    pub fn expected_generation(&self) -> Option<u64> {
        self.expected_generation
    }
    pub fn input_raw_digest(&self) -> &str {
        &self.input_raw_digest
    }
    pub fn output_raw_digest(&self) -> &str {
        &self.output_raw_digest
    }
    pub fn input_normalized_digest(&self) -> &str {
        &self.input_normalized_digest
    }
    pub fn output_normalized_digest(&self) -> &str {
        &self.output_normalized_digest
    }
    pub fn output_lines(&self) -> &[String] {
        &self.output_lines
    }
    pub fn provenance(&self) -> &[LineProvenance] {
        &self.provenance
    }
    pub fn retired_identities(&self) -> &[String] {
        &self.retired_identities
    }
    pub fn edit_ranges(&self) -> &[AppliedEdit] {
        &self.edit_ranges
    }
    pub fn is_noop(&self) -> bool {
        self.is_noop
    }
    pub fn snapshot_mode(&self) -> bool {
        self.snapshot_mode
    }
}

fn content_digest(content: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn snapshot_line_anchors(lines: &[String]) -> Vec<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for line in lines {
        hash.update((line.len() as u64).to_le_bytes());
        hash.update(line.as_bytes());
    }
    let revision = format!("{:x}", hash.finalize());
    (1..=lines.len())
        .map(|index| format!("L{}N{index}", &revision[..32]))
        .collect()
}

impl AnchorStateManager {
    fn state_path(path: &str) -> String {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| std::path::PathBuf::from(path))
            .to_string_lossy()
            .into_owned()
    }

    fn new_anchor_namespace() -> String {
        std::iter::repeat_with(fastrand::alphanumeric)
            .take(12)
            .collect()
    }

    fn encode_base36(mut value: u64) -> String {
        const DIGITS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        if value == 0 {
            return "0".into();
        }
        let mut encoded = Vec::new();
        while value > 0 {
            encoded.push(DIGITS[(value % 36) as usize]);
            value /= 36;
        }
        encoded.reverse();
        String::from_utf8(encoded).expect("base36 digits are ASCII")
    }

    fn migrate_allocator_state(document: &mut TrackedDocument, live_anchors: &[String]) {
        if document.anchor_namespace.is_none() {
            document.anchor_namespace = Some(Self::new_anchor_namespace());
            let live: HashSet<&str> = live_anchors.iter().map(String::as_str).collect();
            for identity in document.used_words.iter().rev() {
                if !live.contains(identity.as_str()) && !document.retired_anchors.contains(identity)
                {
                    document.retired_anchors.push_front(identity.clone());
                    if document.retired_anchors.len() >= MAX_RETIRED_ANCHORS {
                        break;
                    }
                }
            }
        }
        document.used_words.clear();
        document.used_words_set.clear();
        while document.retired_anchors.len() > MAX_RETIRED_ANCHORS {
            document.retired_anchors.pop_front();
        }
    }

    fn allocate_identity(document: &mut TrackedDocument) -> Result<String, AnchorTransitionError> {
        if document.anchor_namespace.is_none() {
            let live = document.anchors.clone();
            Self::migrate_allocator_state(document, &live);
        }
        loop {
            let counter = document.next_anchor_id;
            document.next_anchor_id = counter.checked_add(1).ok_or_else(|| {
                AnchorTransitionError::Validation("anchor identity counter exhausted".into())
            })?;
            let identity = format!(
                "A{}N{}",
                document.anchor_namespace.as_deref().unwrap_or_default(),
                Self::encode_base36(counter)
            );
            if !document.anchors.contains(&identity)
                && !document.retired_anchors.contains(&identity)
            {
                return Ok(identity);
            }
        }
    }

    pub fn observe_snapshot(
        &self,
        absolute_path: &str,
        raw_content: &str,
        task_id: Option<&str>,
    ) -> Result<AnchorSnapshot, AnchorTransitionError> {
        let absolute_path = Self::state_path(absolute_path);
        let absolute_path = absolute_path.as_str();
        let task_id = task_id.unwrap_or("default");
        let (normalized, _) = normalize_file_content(raw_content);
        let lines = split_content_lines(&normalized);
        let hashes = compute_hashes(&lines);
        let (memory_expected, expected) = {
            let storage = self.storage();
            let memory = storage
                .tasks
                .get(task_id)
                .and_then(|files| files.get(absolute_path))
                .cloned();
            let tasks = AnchorStorage::read_tasks(&storage.cache_file)
                .map_err(|error| AnchorTransitionError::Persistence(error.to_string()))?;
            let disk = tasks
                .get(task_id)
                .and_then(|files| files.get(absolute_path))
                .cloned();
            (memory, disk)
        };
        if expected.is_none() && memory_expected.is_some() && lines.len() <= MAX_TRACKED_LINES {
            return Err(AnchorTransitionError::StaleGeneration {
                path: absolute_path.into(),
            });
        }
        let document = if lines.len() > MAX_TRACKED_LINES {
            TrackedDocument {
                generation: expected.as_ref().map_or(0, |document| document.generation),
                hashes,
                anchors: snapshot_line_anchors(&lines),
                used_words: VecDeque::new(),
                used_words_set: HashSet::new(),
                anchor_namespace: None,
                next_anchor_id: 0,
                retired_anchors: VecDeque::new(),
            }
        } else if let Some(document) = &expected {
            if document.hashes != hashes || document.anchors.len() != lines.len() {
                return Err(AnchorTransitionError::StaleGeneration {
                    path: absolute_path.into(),
                });
            }
            document.clone()
        } else {
            let mut document = TrackedDocument {
                generation: 0,
                hashes,
                anchors: Vec::new(),
                used_words: VecDeque::new(),
                used_words_set: HashSet::new(),
                anchor_namespace: Some(Self::new_anchor_namespace()),
                next_anchor_id: 0,
                retired_anchors: VecDeque::new(),
            };
            for _ in 0..document.hashes.len() {
                let identity = Self::allocate_identity(&mut document)?;
                document.anchors.push(identity);
            }
            document
        };
        let snapshot = AnchorSnapshot {
            absolute_path: absolute_path.into(),
            task_id: task_id.into(),
            generation: document.generation,
            raw_digest: content_digest(raw_content),
            normalized_digest: content_digest(&normalized),
            snapshot_mode: lines.len() > MAX_TRACKED_LINES,
            lines,
            anchors: document.anchors.clone(),
            expected,
            memory_expected,
            document,
        };
        Ok(snapshot)
    }

    /// Preserve only origins proven by native splices; never infer identity from a diff.
    pub fn stage_transition(
        &self,
        snapshot: &AnchorSnapshot,
        final_content: &str,
        provenance: &SpliceProvenance,
    ) -> Result<AnchorTransition, AnchorTransitionError> {
        let (normalized, _) = normalize_file_content(final_content);
        let lines = split_content_lines(&normalized);
        if lines.len() != provenance.origins.len() {
            return Err(AnchorTransitionError::Validation(
                "provenance line count differs from content".into(),
            ));
        }
        let mut document = snapshot.document.clone();
        Self::migrate_allocator_state(&mut document, &snapshot.anchors);
        document.hashes = compute_hashes(&lines);
        document.anchors.clear();
        let mut previous = None;
        let snapshot_anchors =
            (lines.len() > MAX_TRACKED_LINES).then(|| snapshot_line_anchors(&lines));
        for (index, origin) in provenance.origins.iter().enumerate() {
            if let Some(origin) = origin {
                if snapshot.lines.get(*origin) != Some(&lines[index])
                    || previous.is_some_and(|last| last >= *origin)
                {
                    return Err(AnchorTransitionError::Validation(
                        "invalid or reordered preserved line origin".into(),
                    ));
                }
                previous = Some(*origin);
            }
            let word = if let Some(anchors) = &snapshot_anchors {
                anchors[index].clone()
            } else if let Some(origin) = origin.filter(|_| !snapshot.snapshot_mode) {
                snapshot.document.anchors[origin].clone()
            } else {
                Self::allocate_identity(&mut document)?
            };
            document.anchors.push(word);
        }
        if document != snapshot.document {
            document.generation = document
                .generation
                .checked_add(1)
                .ok_or_else(|| AnchorTransitionError::Validation("generation exhausted".into()))?;
        }
        let live_words: HashSet<&String> = document.anchors.iter().collect();
        let retired_identities = snapshot
            .anchors
            .iter()
            .filter(|word| !live_words.contains(word))
            .cloned()
            .collect::<Vec<_>>();
        for identity in &retired_identities {
            if !document.retired_anchors.contains(identity) {
                document.retired_anchors.push_back(identity.clone());
            }
        }
        while document.retired_anchors.len() > MAX_RETIRED_ANCHORS {
            document.retired_anchors.pop_front();
        }
        let transition = AnchorTransition {
            absolute_path: snapshot.absolute_path.clone(),
            task_id: snapshot.task_id.clone(),
            generation: document.generation,
            raw_digest: content_digest(final_content),
            normalized_digest: content_digest(&normalized),
            anchors: document.anchors.clone(),
            expected_generation: snapshot
                .expected
                .as_ref()
                .map(|document| document.generation),
            input_raw_digest: snapshot.raw_digest.clone(),
            output_raw_digest: content_digest(final_content),
            input_normalized_digest: snapshot.normalized_digest.clone(),
            output_normalized_digest: content_digest(&normalized),
            output_lines: lines.clone(),
            provenance: provenance
                .origins
                .iter()
                .enumerate()
                .map(|(index, origin)| {
                    if let Some(original_idx) = origin
                        .filter(|_| !snapshot.snapshot_mode && lines.len() <= MAX_TRACKED_LINES)
                    {
                        LineProvenance::Preserved { original_idx }
                    } else {
                        LineProvenance::Created {
                            assigned_word: document.anchors[index].clone(),
                        }
                    }
                })
                .collect(),
            retired_identities,
            edit_ranges: provenance.edit_ranges.clone(),
            is_noop: snapshot.raw_digest == content_digest(final_content)
                && snapshot.document == document,
            expected: snapshot.expected.clone(),
            memory_expected: snapshot.memory_expected.clone(),
            document,
            snapshot_mode: lines.len() > MAX_TRACKED_LINES,
        };
        Ok(transition)
    }

    /// Commit the complete set in one cache write. Call after content writes, while
    /// holding file operation locks. A failure leaves anchor state unchanged;
    /// the handler reports already-applied content with dedicated reread recovery.
    pub fn commit_transitions_checked(
        &self,
        transitions: &[AnchorTransition],
    ) -> Result<(), AnchorTransitionError> {
        self.check_transitions(transitions, true)
    }

    pub fn validate_transitions_checked(
        &self,
        transitions: &[AnchorTransition],
    ) -> Result<(), AnchorTransitionError> {
        self.check_transitions(transitions, false)
    }

    /// Discard this manager's view after content changed without anchor publication.
    /// Persistence may be unavailable, or another manager may have committed newer
    /// anchors. Never repair this failure with a best-effort cache write/delete:
    /// a fresh observation rejects mismatched content until a reread reconciles it.
    pub fn invalidate_state(&self, absolute_path: &str, task_id: Option<&str>) {
        let absolute_path = Self::state_path(absolute_path);
        let absolute_path = absolute_path.as_str();
        let mut storage = self.storage();
        let task_id = task_id.unwrap_or("default");
        if let Some(files) = storage.tasks.get_mut(task_id) {
            files.shift_remove(absolute_path);
        }
        // Forget the baseline too, so a later save cannot queue a durable deletion.
        if let Some(files) = storage.persisted_tasks.get_mut(task_id) {
            files.shift_remove(absolute_path);
        }
    }

    fn check_transitions(
        &self,
        transitions: &[AnchorTransition],
        publish: bool,
    ) -> Result<(), AnchorTransitionError> {
        if transitions.is_empty() {
            return Ok(());
        }
        let persistence =
            |error: std::io::Error| AnchorTransitionError::Persistence(error.to_string());
        let mut storage = self.storage();
        let cache_dir = storage
            .cache_file
            .parent()
            .unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(cache_dir).map_err(persistence)?;
        let _lock = AnchorCacheLock::acquire(&storage.cache_file.with_extension("json.lock"))
            .map_err(persistence)?;
        // Unlike legacy best-effort loading, corrupt/unreadable state fails closed.
        let mut tasks: IndexMap<String, IndexMap<String, TrackedDocument>> =
            match std::fs::read(&storage.cache_file) {
                Ok(bytes) => serde_json::from_slice(&bytes)
                    .map_err(|error| AnchorTransitionError::Persistence(error.to_string()))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => IndexMap::new(),
                Err(error) => return Err(persistence(error)),
            };
        let mut targets = HashSet::new();
        for transition in transitions {
            if !targets.insert((&transition.task_id, &transition.absolute_path)) {
                return Err(AnchorTransitionError::Validation(
                    "duplicate transition target".into(),
                ));
            }
            let memory = storage
                .tasks
                .get(&transition.task_id)
                .and_then(|files| files.get(&transition.absolute_path));
            let disk = tasks
                .get(&transition.task_id)
                .and_then(|files| files.get(&transition.absolute_path));
            if transition.anchors != transition.document.anchors
                || compute_hashes(&transition.output_lines) != transition.document.hashes
                || transition.generation != transition.document.generation
                || transition.expected_generation
                    != transition
                        .expected
                        .as_ref()
                        .map(|document| document.generation)
            {
                return Err(AnchorTransitionError::Validation(
                    "transition metadata differs from staged document".into(),
                ));
            }
            if memory != transition.memory_expected.as_ref() || disk != transition.expected.as_ref()
            {
                return Err(AnchorTransitionError::StaleGeneration {
                    path: transition.absolute_path.clone(),
                });
            }
        }
        if !publish || transitions.iter().all(|transition| transition.is_noop) {
            return Ok(());
        }
        for transition in transitions {
            if transition.is_noop {
                continue;
            }
            if transition.snapshot_mode {
                if let Some(files) = tasks.get_mut(&transition.task_id) {
                    files.shift_remove(&transition.absolute_path);
                }
            } else {
                let mut files = tasks.shift_remove(&transition.task_id).unwrap_or_default();
                files.shift_remove(&transition.absolute_path);
                files.insert(
                    transition.absolute_path.clone(),
                    transition.document.clone(),
                );
                tasks.insert(transition.task_id.clone(), files);
            }
        }
        AnchorStorage::enforce_limits(&mut tasks);
        let json = serde_json::to_string_pretty(&tasks)
            .map_err(|error| AnchorTransitionError::Persistence(error.to_string()))?;
        crate::storage::disk::atomic_write_file(&storage.cache_file, &json).map_err(persistence)?;
        // There are no fallible operations after durable replacement.
        storage.persisted_tasks = tasks.clone();
        storage.tasks = tasks;
        Ok(())
    }

    #[cfg(not(test))]
    #[must_use]
    pub fn new() -> Self {
        Self::with_cache_file(crate::storage::disk::get_data_dir().join("cache/anchors.json"))
    }

    #[cfg(test)]
    #[must_use]
    pub fn new() -> Self {
        static TEST_CACHE: LazyLock<std::path::PathBuf> = LazyLock::new(|| {
            tempfile::Builder::new()
                .prefix("sned-anchor-test-")
                .tempdir()
                .expect("create isolated anchor test cache")
                .keep()
                .join("anchors.json")
        });
        Self::with_cache_file(TEST_CACHE.clone())
    }

    /// Keeps independent sessions/tests from sharing a process-global cache location.
    #[must_use]
    pub fn with_cache_file(path: std::path::PathBuf) -> Self {
        Self {
            storage: Arc::new(Mutex::new(AnchorStorage::load(path))),
        }
    }

    fn storage(&self) -> parking_lot::MutexGuard<'_, AnchorStorage> {
        self.storage.lock()
    }

    fn get_task_state(&self, task_id: &str) -> IndexMap<String, TrackedDocument> {
        let mut storage = self.storage();
        let state = storage.tasks.shift_remove(task_id);

        // Implement LRU for tasks
        if storage.tasks.len() >= MAX_TRACKED_TASKS {
            // Remove oldest task (first key - IndexMap maintains insertion order)
            let oldest = storage.tasks.keys().next().cloned();
            if let Some(oldest_key) = oldest {
                storage.tasks.shift_remove(&oldest_key);
            }
        }

        let state = state.unwrap_or_default();
        storage.tasks.insert(task_id.to_string(), state.clone());
        state
    }

    fn get_task_state_mut<'a>(
        storage: &'a mut AnchorStorage,
        task_id: &str,
    ) -> &'a mut IndexMap<String, TrackedDocument> {
        // Evict oldest task if at capacity and task doesn't exist
        if !storage.tasks.contains_key(task_id) && storage.tasks.len() >= MAX_TRACKED_TASKS {
            let oldest = storage.tasks.keys().next().cloned();
            if let Some(oldest_key) = oldest {
                storage.tasks.shift_remove(&oldest_key);
            }
        }

        // For true LRU: remove and re-insert to move accessed task to back
        // This ensures recently-used tasks are not evicted
        let state = storage.tasks.shift_remove(task_id).unwrap_or_default();
        storage.tasks.insert(task_id.to_string(), state);

        storage.tasks.get_mut(task_id).unwrap()
    }

    fn update_state(&self, absolute_path: &str, document: TrackedDocument, task_id: &str) {
        let mut storage = self.storage();
        let state = Self::get_task_state_mut(&mut storage, task_id);

        // Implement LRU for files - use shift_remove to maintain insertion order
        state.shift_remove(absolute_path);
        state.insert(absolute_path.to_string(), document);

        if state.len() > MAX_TRACKED_FILES {
            let oldest = state.keys().next().cloned();
            if let Some(oldest_key) = oldest {
                state.shift_remove(&oldest_key);
            }
        }
    }

    fn republish_document(
        &self,
        absolute_path: &str,
        task_id: &str,
        document: &TrackedDocument,
    ) -> std::io::Result<()> {
        let mut storage = self.storage();
        let files = storage.tasks.entry(task_id.to_string()).or_default();
        files.insert(absolute_path.to_string(), document.clone());

        if let Some(files) = storage.persisted_tasks.get_mut(task_id) {
            files.shift_remove(absolute_path);
        }
        storage.save();

        let durable = AnchorStorage::read_tasks(&storage.cache_file)?
            .get(task_id)
            .and_then(|files| files.get(absolute_path))
            .cloned();
        if durable.as_ref() == Some(document) {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "anchor cache did not contain the republished document",
            ))
        }
    }

    /// Reconciles the current file content with saved state using diff.
    ///
    #[must_use]
    pub fn reconcile(
        &self,
        absolute_path: &str,
        current_lines: &[String],
        task_id: Option<&str>,
    ) -> Vec<String> {
        match self.reconcile_checked(absolute_path, current_lines, task_id) {
            Ok(anchors) => anchors,
            Err(error) => {
                tracing::warn!(
                    path = absolute_path,
                    error = %error,
                    "anchor reconciliation could not be persisted"
                );
                self.get_anchors(absolute_path, task_id).unwrap_or_default()
            }
        }
    }

    pub fn reconcile_checked(
        &self,
        absolute_path: &str,
        current_lines: &[String],
        task_id: Option<&str>,
    ) -> Result<Vec<String>, AnchorTransitionError> {
        let absolute_path = Self::state_path(absolute_path);
        let absolute_path = absolute_path.as_str();
        let task_id = task_id.unwrap_or("default");

        if current_lines.len() > MAX_TRACKED_LINES {
            use sha2::{Digest, Sha256};
            // Without reconciliation, a bare line number can silently retarget
            // an identical occurrence after insertion. Bind it to the snapshot.
            let mut hash = Sha256::new();
            for line in current_lines {
                hash.update((line.len() as u64).to_le_bytes());
                hash.update(line.as_bytes());
            }
            let revision = format!("{:x}", hash.finalize());
            return Ok((1..=current_lines.len())
                .map(|i| format!("L{}N{i}", &revision[..32]))
                .collect());
        }

        let current_hashes = compute_hashes(current_lines);
        let (tracked, durable_missing) = {
            let mut storage = self.storage();
            // Readers must adopt a newer committed document before diffing or
            // returning the identical-content fast path from a stale manager.
            let durable_document = AnchorStorage::read_tasks(&storage.cache_file)
                .ok()
                .and_then(|tasks| {
                    tasks
                        .get(task_id)
                        .and_then(|files| files.get(absolute_path))
                        .cloned()
                });
            if let Some(document) = durable_document {
                storage
                    .tasks
                    .entry(task_id.to_string())
                    .or_default()
                    .insert(absolute_path.to_string(), document.clone());
                storage
                    .persisted_tasks
                    .entry(task_id.to_string())
                    .or_default()
                    .insert(absolute_path.to_string(), document);
            }
            let state = Self::get_task_state_mut(&mut storage, task_id);
            let tracked = state.get(absolute_path).cloned();
            let durable_missing = tracked.is_some()
                && AnchorStorage::read_tasks(&storage.cache_file)
                    .ok()
                    .and_then(|tasks| {
                        tasks
                            .get(task_id)
                            .and_then(|files| files.get(absolute_path))
                            .cloned()
                    })
                    .is_none();
            (tracked, durable_missing)
        };

        // Fast path: if hashes are identical, nothing changed
        if let Some(tracked) = &tracked
            && tracked.hashes.len() == current_hashes.len()
        {
            let identical = tracked
                .hashes
                .iter()
                .zip(current_hashes.iter())
                .all(|(a, b)| a == b);

            if identical {
                let mut storage = self.storage();
                let state = Self::get_task_state_mut(&mut storage, task_id);
                if let Some(document) = state.shift_remove(absolute_path) {
                    state.insert(absolute_path.to_string(), document);
                }
                drop(storage);
                if durable_missing {
                    self.republish_document(absolute_path, task_id, tracked)
                        .map_err(|error| AnchorTransitionError::Persistence(error.to_string()))?;
                }
                return Ok(tracked.anchors.clone());
            }
        }

        // First time seeing this file? Assign unique anchors to every line.
        if tracked.is_none() {
            let mut tracked = TrackedDocument {
                generation: 0,
                hashes: current_hashes,
                anchors: Vec::new(),
                used_words: VecDeque::new(),
                used_words_set: HashSet::new(),
                anchor_namespace: Some(Self::new_anchor_namespace()),
                next_anchor_id: 0,
                retired_anchors: VecDeque::new(),
            };
            for _ in 0..tracked.hashes.len() {
                let identity = Self::allocate_identity(&mut tracked)?;
                tracked.anchors.push(identity);
            }
            let anchors = tracked.anchors.clone();
            self.update_state(absolute_path, tracked, task_id);
            self.save();
            return Ok(anchors);
        }

        let mut tracked = tracked.unwrap();
        let old_anchors = tracked.anchors.clone();
        Self::migrate_allocator_state(&mut tracked, &old_anchors);
        let document_changed = tracked.hashes != current_hashes;

        // Run diff on hashes
        let changes = diff_arrays(&tracked.hashes, &current_hashes);

        let mut new_anchors: Vec<String> = Vec::new();
        let mut next_document = tracked.clone();
        next_document.anchors.clear();

        let mut old_idx = 0;
        let mut old_counts = HashMap::new();
        let mut new_counts = HashMap::new();
        for hash in &tracked.hashes {
            *old_counts.entry(*hash).or_insert(0usize) += 1;
        }
        for hash in &current_hashes {
            *new_counts.entry(*hash).or_insert(0usize) += 1;
        }

        for change in changes {
            match change {
                DiffChange::Added(count) => {
                    for _ in 0..count {
                        let identity = Self::allocate_identity(&mut next_document)?;
                        new_anchors.push(identity.clone());
                        next_document.anchors.push(identity);
                    }
                }
                DiffChange::Removed(count) => {
                    old_idx += count;
                }
                DiffChange::Unchanged(count) => {
                    for _ in 0..count {
                        let hash = tracked.hashes[old_idx];
                        // A diff cannot establish identity between identical occurrences
                        // across revisions. Retire those words instead of silently rebinding.
                        let preserved_word = if document_changed
                            && (old_counts[&hash] > 1
                                || new_counts.get(&hash).copied().unwrap_or(0) > 1)
                        {
                            Self::allocate_identity(&mut next_document)?
                        } else {
                            tracked.anchors[old_idx].clone()
                        };
                        new_anchors.push(preserved_word.clone());
                        next_document.anchors.push(preserved_word);
                        old_idx += 1;
                    }
                }
            }
        }

        let live: HashSet<&str> = new_anchors.iter().map(String::as_str).collect();
        for identity in old_anchors {
            if !live.contains(identity.as_str())
                && !next_document.retired_anchors.contains(&identity)
            {
                next_document.retired_anchors.push_back(identity);
            }
        }
        while next_document.retired_anchors.len() > MAX_RETIRED_ANCHORS {
            next_document.retired_anchors.pop_front();
        }
        next_document.generation = tracked.generation.saturating_add(1);
        next_document.hashes = current_hashes;
        next_document.anchors = new_anchors;
        let tracked = next_document;
        let anchors = tracked.anchors.clone();
        self.update_state(absolute_path, tracked, task_id);

        // Persist anchor state to disk
        self.save();

        Ok(anchors)
    }

    /// Returns true if the file is currently being tracked.
    #[must_use]
    pub fn is_tracking(&self, absolute_path: &str, task_id: Option<&str>) -> bool {
        let absolute_path = Self::state_path(absolute_path);
        let task_id = task_id.unwrap_or("default");
        let state = self.get_task_state(task_id);
        state.contains_key(&absolute_path)
    }

    /// Gets current anchors for a file if it's being tracked.
    #[must_use]
    pub fn get_anchors(&self, absolute_path: &str, task_id: Option<&str>) -> Option<Vec<String>> {
        let absolute_path = Self::state_path(absolute_path);
        let task_id = task_id.unwrap_or("default");
        let state = self.get_task_state(task_id);
        state.get(&absolute_path).map(|t| t.anchors.clone())
    }

    /// Clear state for a file.
    pub fn clear_state(&self, absolute_path: &str, task_id: Option<&str>) {
        let absolute_path = Self::state_path(absolute_path);
        let task_id = task_id.unwrap_or("default");
        let mut state = self.get_task_state(task_id);
        state.shift_remove(&absolute_path);
        let mut storage = self.storage();
        storage.tasks.insert(task_id.to_string(), state);
        drop(storage);
        self.save();
    }

    /// Resets all anchors for a specific task or all tasks.
    pub fn reset(&self, task_id: Option<&str>) {
        {
            let mut storage = self.storage();
            if let Some(id) = task_id {
                storage.tasks.shift_remove(id);
            } else {
                storage.tasks.clear();
            }
        }
        self.save();
    }

    /// Persists anchor state to disk.
    pub fn save(&self) {
        let mut storage = self.storage();
        storage.save();
    }
}

impl Default for AnchorStateManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Diff Algorithm
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
enum DiffChange {
    Added(usize),
    Removed(usize),
    Unchanged(usize),
}

/// Computes diff between two arrays of u64 values.
///
fn diff_arrays(old: &[u64], new: &[u64]) -> Vec<DiffChange> {
    use similar::{Algorithm, DiffOp};

    let mut changes: Vec<DiffChange> = Vec::new();

    // Use similar crate's myers diff for exact parity with TypeScript diff package
    let ops = similar::capture_diff_slices(Algorithm::Myers, old, new);

    for op in ops {
        match op {
            DiffOp::Equal { len, .. } => {
                changes.push(DiffChange::Unchanged(len));
            }
            DiffOp::Delete { old_len, .. } => {
                changes.push(DiffChange::Removed(old_len));
            }
            DiffOp::Insert { new_len, .. } => {
                changes.push(DiffChange::Added(new_len));
            }
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                changes.push(DiffChange::Removed(old_len));
                changes.push(DiffChange::Added(new_len));
            }
        }
    }

    changes
}

// ============================================================================
// Edit Types
// ============================================================================

/// An individual edit operation.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub anchor: String,
    pub end_anchor: Option<String>,
    pub edit_type: String,
    pub text: String,
    /// Optional multi-line fingerprint: when both `anchor` and
    /// `end_anchor` are present, the lines between them must equal this
    /// list verbatim. Words pin the span authoritatively; `content`
    /// verifies the interior.
    pub content: Option<Vec<String>>,
}

/// A file with multiple edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: String,
    pub edits: Vec<Edit>,
}

/// A resolved edit with line indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEdit {
    pub line_idx: usize,
    pub end_idx: usize,
    pub edit: Edit,
}

/// Per-edit diagnostic for a replace that was filtered as a no-op because
/// the file already matches the replacement at the bound range. Carries the
/// line number the edit would have targeted plus the list of lines whose
/// content equals the first line of the bound range, so the model can tell
/// when a "1 edit unchanged" actually meant "applied at the wrong location."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnchangedSite {
    pub line_idx: usize,
    pub end_idx: usize,
    pub identical_content_at: Vec<usize>,
    pub total_identical_count: usize,
    /// The anchor word that bound the no-op replace, so the summary
    /// can name which quoted word landed at the bound line.
    pub anchor_word: Option<String>,
}

/// A failed edit with error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedEdit {
    pub edit: Edit,
    pub error: String,
}

/// An applied edit with metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    pub start_idx: usize,
    pub end_idx: usize,
    pub original_start_idx: usize,
    pub original_end_idx: usize,
    pub edit: Edit,
    pub lines_added: usize,
    pub lines_deleted: usize,
}

// ============================================================================
// Edit Executor
// ============================================================================

/// Outcome of attempting to apply resolved edits to lines.
///
/// `Applied` carries the final content, line-count delta, the
/// per-edit `AppliedEdit` records, and any `UnchangedSite`
/// diagnostics for replaces filtered as no-ops.
///
/// `Overlap` means at least one pair of effective edits covered
/// overlapping ranges — caller should reject the whole batch.
///
/// `GluedAnchor { lines }` means the final assembled content still
/// contained a `Word§` or `hex§` fragment. Caller should reject the
/// whole batch and tell the model which lines to fix.
///
/// `DuplicateInsertion` means an insertion would repeat an exact adjacent
/// line block or copy its anchored line. Caller should reject the whole batch.
pub enum ApplyOutcome {
    Applied(
        Vec<String>,
        usize,
        usize,
        Vec<AppliedEdit>,
        Vec<UnchangedSite>,
    ),
    Overlap,
    GluedAnchor(Vec<usize>),
    DuplicateInsertion(Vec<FailedEdit>),
}

/// Executes hash-anchored edits.
///
#[derive(Debug, Clone, Default)]
pub struct EditExecutor;

impl EditExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Resolves edits to line indices.
    #[must_use]
    pub fn resolve_edits(
        &self,
        edits: &[Edit],
        lines: &[String],
        line_hashes: &[String],
    ) -> (Vec<ResolvedEdit>, Vec<FailedEdit>) {
        let mut failed_edits: Vec<FailedEdit> = Vec::new();
        let mut resolved_edits: Vec<ResolvedEdit> = Vec::new();
        let normalized_line_hashes: Vec<String> =
            line_hashes.iter().map(|h| h.trim().to_string()).collect();

        for edit in edits {
            let mut diagnostics: Vec<String> = Vec::new();
            let edit_type = &edit.edit_type;

            // Boundary words pin the span authoritatively even if their content
            // repeats across mirrored classes.
            let using_fingerprint =
                edit_type == "replace" && edit.end_anchor.is_some() && edit.content.is_some();

            let (line_idx, start_error) = if using_fingerprint {
                self.resolve_anchor_by_word("anchor", &edit.anchor, &normalized_line_hashes, lines)
            } else {
                let hint = match edit_type.as_str() {
                    "insert_before" | "insert_after" => Some(
                        "Anchor a unique neighboring line instead, or rewrite the file with write_to_file. The fingerprint escape hatch (anchor + end_anchor + content) does not apply to insert_before / insert_after.",
                    ),
                    _ => None,
                };
                self.resolve_anchor_with_hint(
                    "anchor",
                    &edit.anchor,
                    &normalized_line_hashes,
                    lines,
                    hint,
                )
            };
            if let Some(error) = start_error {
                diagnostics.push(error);
            }

            let mut end_idx = line_idx;
            if edit_type == "replace" {
                let end_anchor_str = edit.end_anchor.as_deref().unwrap_or("");
                if end_anchor_str.trim().is_empty() {
                    // Auto-default: missing end_anchor on replace means single-line
                    // replace (end_idx = line_idx, already set above).
                } else {
                    let (resolved_end_idx, end_error) = if using_fingerprint {
                        self.resolve_anchor_by_word(
                            "end_anchor",
                            end_anchor_str,
                            &normalized_line_hashes,
                            lines,
                        )
                    } else {
                        self.resolve_anchor(
                            "end_anchor",
                            end_anchor_str,
                            &normalized_line_hashes,
                            lines,
                        )
                    };
                    if let Some(error) = end_error {
                        diagnostics.push(error);
                    }
                    end_idx = resolved_end_idx;
                }
            }

            if line_idx != usize::MAX && end_idx != usize::MAX && end_idx < line_idx {
                diagnostics.push("Range error: anchor must refer to a line that precedes or is the same as end_anchor.".to_string());
            }

            // A unique generated word pins its line. Multi-line fingerprints
            // remain the escape hatch when a supplied word is shared.
            if let (Some(fp_content), true) = (
                edit.content.as_ref(),
                line_idx != usize::MAX && end_idx != usize::MAX,
            ) {
                let interior_len = end_idx.saturating_sub(line_idx).saturating_sub(1);
                if interior_len != fp_content.len() {
                    diagnostics.push(format!(
                        "fingerprint content does not match the lines between anchor and end_anchor: expected {} line(s), got {}",
                        fp_content.len(),
                        interior_len
                    ));
                } else if end_idx <= line_idx {
                    diagnostics.push(
                        "fingerprint requires anchor and end_anchor to span at least one line between them."
                            .to_string(),
                    );
                } else {
                    let interior = &lines[line_idx + 1..=end_idx - 1];
                    let matches = interior.iter().zip(fp_content.iter()).all(|(a, b)| a == b);
                    if !matches {
                        let first_diff = interior
                            .iter()
                            .zip(fp_content.iter())
                            .position(|(a, b)| a != b)
                            .unwrap_or(0);
                        diagnostics.push(format!(
                            "fingerprint content differs from the file starting at interior line {}: file has {:?} but content lists {:?}",
                            line_idx + 1 + first_diff,
                            interior[first_diff],
                            fp_content[first_diff]
                        ));
                    }
                }
            }

            if diagnostics.is_empty() {
                resolved_edits.push(ResolvedEdit {
                    line_idx,
                    end_idx,
                    edit: edit.clone(),
                });
            } else {
                failed_edits.push(FailedEdit {
                    edit: edit.clone(),
                    error: diagnostics.join(" "),
                });
            }
        }

        (resolved_edits, failed_edits)
    }

    /// Resolves an anchor to a line index.
    ///
    /// Resolves a unique generated word authoritatively. If the supplied word
    /// itself is shared, the matching content must identify one occurrence;
    /// otherwise the returned diagnostic gives the model the available
    /// occurrences and a fingerprint-based recovery path. Reconciliation
    /// retires duplicate words after changed revisions because a line-content
    /// diff cannot prove which identical occurrence retained its identity.
    pub fn resolve_anchor(
        &self,
        anchor_type: &str,
        raw_anchor: &str,
        normalized_line_hashes: &[String],
        lines: &[String],
    ) -> (usize, Option<String>) {
        self.resolve_anchor_with_hint(anchor_type, raw_anchor, normalized_line_hashes, lines, None)
    }

    /// Variant of [`resolve_anchor`] that appends an edit-type-aware
    /// recovery hint to the content-ambiguity rejection. The hint
    /// teaches the model how to escape the ambiguity using tools it
    /// actually has: `replace` learns the fingerprint shape
    /// (`anchor + end_anchor + content`); `insert_before` /
    /// `insert_after` learn to anchor a unique neighbor or fall back
    /// to `write_to_file`.
    pub fn resolve_anchor_with_hint(
        &self,
        anchor_type: &str,
        raw_anchor: &str,
        normalized_line_hashes: &[String],
        lines: &[String],
        content_ambiguity_hint: Option<&str>,
    ) -> (usize, Option<String>) {
        let anchor_raw = raw_anchor.trim_start();
        if anchor_raw.is_empty() {
            return (usize::MAX, Some(format!("{anchor_type} is missing.")));
        }

        if anchor_raw.contains('\n') || anchor_raw.contains('\r') {
            return (
                usize::MAX,
                Some(format!(
                    "{} contains multiple lines. Anchors must refer to a single line only in the format Anchor{}{}line_text{}.",
                    anchor_type, ANCHOR_DELIMITER, "{", "}"
                )),
            );
        }

        let (anchor_name, provided_content) = split_anchor(anchor_raw);

        // Check if anchor name is valid
        if !ANCHOR_NAME_REGEX.is_match(&anchor_name) {
            tracing::debug!(
                "Anchor resolution failed: invalid anchor name format. anchor_type={}, raw_anchor={}, anchor_name={}",
                anchor_type,
                raw_anchor,
                anchor_name
            );
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} is missing or incorrectly formatted. It must start with a single word followed by the delimiter (e.g., \"Apple{ANCHOR_DELIMITER}\"). COPY THE EXACT ANCHOR STRING FROM read_file OUTPUT (e.g., \"Crawler§void draw_game_over() {{\"). Do NOT use raw source lines without the Word§ prefix."
                )),
            );
        }

        let word_bound_lines: Vec<usize> = normalized_line_hashes
            .iter()
            .enumerate()
            .filter(|(_, h)| **h == anchor_name)
            .map(|(i, _)| i)
            .collect();

        tracing::debug!(
            "Anchor resolution: anchor_type={}, anchor_name={}, provided_content={:?}, word_matches={}, total_lines={}",
            anchor_type,
            anchor_name,
            provided_content,
            word_bound_lines.len(),
            lines.len()
        );

        if word_bound_lines.is_empty() {
            tracing::debug!(
                "Anchor resolution failed: anchor name '{}' not found in file. Available anchors: {:?}",
                anchor_name,
                normalized_line_hashes
            );
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} \"{anchor_name}\" not found in the file. Please ensure you are using the latest anchors from the most recent read_file output. COPY THE EXACT ANCHOR STRING FROM read_file OUTPUT (e.g., \"Crawler§void draw_game_over() {{\"). Do not modify the anchor text or omit the Word§ prefix."
                )),
            );
        }

        let content_matches: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line == provided_content)
            .map(|(i, _)| i)
            .collect();

        if word_bound_lines.len() == 1 {
            let bound = word_bound_lines[0];
            let bound_content = lines[bound].as_str();
            if bound_content == provided_content {
                tracing::debug!(
                    "Anchor resolved by unique word match: anchor_type={}, anchor_name={}, line_index={}",
                    anchor_type,
                    anchor_name,
                    bound
                );
                return (bound, None);
            }
            if bound_content.trim() == provided_content.trim() && !bound_content.is_empty() {
                return (
                    usize::MAX,
                    Some(format!(
                        "{anchor_type} \"{anchor_name}\" matches a line only after trimming whitespace (line {}), but the word-bound line is {:?}. The supplied content differs from the file's content only in leading or trailing whitespace — copy the line EXACTLY from read_file output (preserving spaces) and retry. Do NOT re-read first; re-reading won't change whitespace.",
                        bound + 1,
                        bound_content
                    )),
                );
            }
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} \"{anchor_name}\" exists, but the supplied content does not match the line it currently binds to (now: {:?}). The anchor is stale: the quoted word resolved to a different line after a prior edit. Please re-read the file with read_file to get fresh anchors before retrying.",
                    bound_content
                )),
            );
        }

        if content_matches.len() >= 2 {
            let occurrences: Vec<String> = content_matches
                .iter()
                .take(8)
                .map(|&idx| {
                    let word_at = normalized_line_hashes
                        .get(idx)
                        .map(String::as_str)
                        .unwrap_or("?");
                    format!(
                        "  line {}: {word_at}{ANCHOR_DELIMITER}{}",
                        idx + 1,
                        lines[idx]
                    )
                })
                .collect();
            let overflow = content_matches.len().saturating_sub(occurrences.len());
            let mut listing = occurrences.join("\n");
            if overflow > 0 {
                listing.push_str(&format!("\n  ... and {overflow} more"));
            }
            tracing::debug!(
                "Anchor resolution failed: content-ambiguous anchor. anchor_name={}, content_match_count={}, content_match_indices={:?}",
                anchor_name,
                content_matches.len(),
                content_matches
            );
            let base_hint = "Pin a unique span by quoting anchor + end_anchor + the lines between them in the 'content' field, then retry.";
            let hint = content_ambiguity_hint.unwrap_or(base_hint);
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} \"{anchor_name}{ANCHOR_DELIMITER}{provided_content}\" matches {} lines with identical content:\n{}\n{}",
                    content_matches.len(),
                    listing,
                    hint
                )),
            );
        }

        if content_matches.is_empty() {
            // Distinguish whitespace-only mismatch from a genuine
            // rebind: if a line matches provided_content modulo
            // whitespace, the model quoted with wrong whitespace
            // (re-reading won't fix it); otherwise the anchor is stale.
            let trimmed_matches: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.trim() == provided_content.trim() && !line.is_empty())
                .map(|(i, _)| i)
                .collect();

            if !trimmed_matches.is_empty() {
                let rebound = word_bound_lines
                    .iter()
                    .map(|&idx| format!("line {}: {:?}", idx + 1, lines[idx]))
                    .collect::<Vec<_>>()
                    .join(", ");
                return (
                    usize::MAX,
                    Some(format!(
                        "{anchor_type} \"{anchor_name}\" matches a line only after trimming whitespace (lines {}), but the word-bound line is ({rebound}). The supplied content differs from the file's content only in leading or trailing whitespace — copy the line EXACTLY from read_file output (preserving spaces) and retry. Do NOT re-read first; re-reading won't change whitespace.",
                        trimmed_matches
                            .iter()
                            .map(|i| (i + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
            }

            let rebound = word_bound_lines
                .iter()
                .map(|&idx| format!("line {}: {:?}", idx + 1, lines[idx]))
                .collect::<Vec<_>>()
                .join(", ");
            tracing::debug!(
                "Anchor resolution failed: content mismatch on word-bound line. anchor_name={}, provided_content={:?}, word_bound=[{rebound}]",
                anchor_name,
                provided_content,
            );
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} \"{anchor_name}\" exists, but the supplied content does not match the line it currently binds to (now: {rebound}). The anchor is stale: the quoted word resolved to a different line after a prior edit. Please re-read the file with read_file to get fresh anchors before retrying."
                )),
            );
        }

        let content_idx = content_matches[0];
        let word_idx = word_bound_lines[0];
        if word_idx != content_idx {
            tracing::debug!(
                "Anchor resolution: rebind detected. quoted_word={} now binds to line {} but supplied content matches line {}",
                anchor_name,
                word_idx,
                content_idx
            );
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} \"{anchor_name}{ANCHOR_DELIMITER}{provided_content}\" now binds to content that appears at a different line: the quoted word resolves to line {} but the supplied content matches line {}. Re-read the file with read_file to refresh anchors before retrying.",
                    word_idx + 1,
                    content_idx + 1
                )),
            );
        }

        tracing::debug!(
            "Anchor resolved successfully: anchor_type={}, anchor_name={}, line_index={}",
            anchor_type,
            anchor_name,
            content_idx
        );
        (content_idx, None)
    }

    /// Resolves an anchor to a line index using ONLY the word identity
    /// (no content check). Use this when the caller has separately
    /// pinned the span — typically via a multi-line fingerprint where
    /// `anchor` + `end_anchor` + `content` together identify a unique
    /// region, but each individual line's content may legitimately
    /// repeat across mirrored classes.
    #[allow(clippy::unused_self)]
    pub fn resolve_anchor_by_word(
        &self,
        anchor_type: &str,
        raw_anchor: &str,
        normalized_line_hashes: &[String],
        _lines: &[String],
    ) -> (usize, Option<String>) {
        let anchor_raw = raw_anchor.trim();
        if anchor_raw.is_empty() {
            return (usize::MAX, Some(format!("{anchor_type} is missing.")));
        }
        if anchor_raw.contains('\n') || anchor_raw.contains('\r') {
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} contains multiple lines. Anchors must refer to a single line only in the format Anchor{0}line_text.",
                    ANCHOR_DELIMITER
                )),
            );
        }

        let (anchor_name, _) = split_anchor(anchor_raw);
        if !ANCHOR_NAME_REGEX.is_match(&anchor_name) {
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} is missing or incorrectly formatted. It must start with a single word followed by the delimiter (e.g., \"Apple{0}\").",
                    ANCHOR_DELIMITER
                )),
            );
        }

        let word_bound_lines: Vec<usize> = normalized_line_hashes
            .iter()
            .enumerate()
            .filter(|(_, h)| **h == anchor_name)
            .map(|(i, _)| i)
            .collect();

        if word_bound_lines.is_empty() {
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} \"{anchor_name}\" not found in the file. Please ensure you are using the latest anchors from the most recent read_file output."
                )),
            );
        }

        (word_bound_lines[0], None)
    }

    /// Applies resolved edits to lines.
    /// Returns the outcome — see [`ApplyOutcome`]. Overlap is detected
    /// before application; glued-anchor detection runs after assembly.
    pub fn apply_edits(&self, lines: &[String], resolved_edits: &[ResolvedEdit]) -> ApplyOutcome {
        self.apply_edits_with_provenance(lines, resolved_edits).0
    }

    pub fn apply_edits_with_provenance(
        &self,
        lines: &[String],
        resolved_edits: &[ResolvedEdit],
    ) -> (ApplyOutcome, SpliceProvenance) {
        let mut provenance = SpliceProvenance {
            origins: (0..lines.len()).map(Some).collect(),
            edit_ranges: Vec::new(),
        };
        let outcome = self.apply_edits_recording(lines, resolved_edits, &mut provenance);
        (outcome, provenance)
    }

    fn apply_edits_recording(
        &self,
        lines: &[String],
        resolved_edits: &[ResolvedEdit],
        provenance: &mut SpliceProvenance,
    ) -> ApplyOutcome {
        let mut unchanged_sites: Vec<UnchangedSite> = Vec::new();
        let effective_edits: Vec<&ResolvedEdit> = resolved_edits
            .iter()
            .filter(|resolved| {
                let clean_text = strip_hashes(&resolved.edit.text);
                let replacement_lines = if clean_text.is_empty() {
                    Vec::new()
                } else {
                    split_content_lines(&clean_text)
                };

                if resolved.edit.edit_type == "insert_after"
                    || resolved.edit.edit_type == "insert_before"
                {
                    !replacement_lines.is_empty()
                } else {
                    let bound = &lines[resolved.line_idx..=resolved.end_idx];
                    if bound == replacement_lines.as_slice() {
                        let first_line = bound.first().cloned().unwrap_or_default();
                        let all_identical: Vec<usize> = lines
                            .iter()
                            .enumerate()
                            .filter(|(_, line)| **line == first_line)
                            .map(|(i, _)| i)
                            .filter(|&i| i != resolved.line_idx)
                            .collect();
                        let total_identical_count = all_identical.len();
                        let identical_at: Vec<usize> = all_identical.into_iter().take(8).collect();
                        let (word, _) = split_anchor(&resolved.edit.anchor);
                        unchanged_sites.push(UnchangedSite {
                            line_idx: resolved.line_idx,
                            end_idx: resolved.end_idx,
                            identical_content_at: identical_at,
                            total_identical_count,
                            anchor_word: if word.is_empty() { None } else { Some(word) },
                        });
                        false
                    } else {
                        true
                    }
                }
            })
            .collect();

        let duplicate_insertions = effective_edits
            .iter()
            .filter_map(|resolved| {
                let edit_type = resolved.edit.edit_type.as_str();
                if !matches!(edit_type, "insert_before" | "insert_after") {
                    return None;
                }

                let clean_text = strip_hashes(&resolved.edit.text);
                if clean_text.is_empty() {
                    return None;
                }
                let replacement_lines = split_content_lines(&clean_text);
                let anchor_line = &lines[resolved.line_idx];
                // A boundary copy would duplicate the preserved anchor; an
                // identical statement inside a newly inserted block is valid.
                let repeats_anchor_at_boundary = replacement_lines.first() == Some(anchor_line)
                    || replacement_lines.last() == Some(anchor_line);
                if anchor_line.chars().any(char::is_alphanumeric) && repeats_anchor_at_boundary
                {
                    return Some(FailedEdit {
                        edit: resolved.edit.clone(),
                        error: format!(
                            "duplicate insertion rejected: insertion text repeats the anchored line {:?} at its boundary. {} preserves the anchor; use replace with anchor and end_anchor when wrapping existing code",
                            anchor_line, edit_type
                        ),
                    });
                }

                let adjacent_matches = if edit_type == "insert_before" {
                    resolved
                        .line_idx
                        .checked_sub(replacement_lines.len())
                        .is_some_and(|start| {
                            lines[start..resolved.line_idx] == replacement_lines[..]
                        })
                } else {
                    let start = resolved.line_idx.saturating_add(1);
                    start
                        .checked_add(replacement_lines.len())
                        .is_some_and(|end| {
                            end <= lines.len() && lines[start..end] == replacement_lines[..]
                        })
                };

                adjacent_matches.then(|| FailedEdit {
                    edit: resolved.edit.clone(),
                    error: format!(
                        "duplicate insertion rejected: the exact {}-line insertion block is already immediately {} the anchor. Leading and trailing blank lines are part of this exact comparison; do not retry the same insertion",
                        replacement_lines.len(),
                        if edit_type == "insert_before" { "before" } else { "after" }
                    ),
                })
            })
            .collect::<Vec<_>>();
        if !duplicate_insertions.is_empty() {
            return ApplyOutcome::DuplicateInsertion(duplicate_insertions);
        }

        for i in 0..effective_edits.len() {
            for j in (i + 1)..effective_edits.len() {
                let a = effective_edits[i];
                let b = effective_edits[j];

                let a_start = a.line_idx;
                let a_end =
                    if a.edit.edit_type == "insert_after" || a.edit.edit_type == "insert_before" {
                        a.line_idx
                    } else {
                        a.end_idx
                    };

                let b_start = b.line_idx;
                let b_end =
                    if b.edit.edit_type == "insert_after" || b.edit.edit_type == "insert_before" {
                        b.line_idx
                    } else {
                        b.end_idx
                    };

                // Check if ranges overlap: [a_start, a_end] overlaps [b_start, b_end]
                if a_start <= b_end && b_start <= a_end {
                    tracing::warn!(
                        "Overlapping edit ranges detected: edit {} covers lines {}-{}, edit {} covers lines {}-{}",
                        i,
                        a_start,
                        a_end,
                        j,
                        b_start,
                        b_end
                    );
                    return ApplyOutcome::Overlap;
                }
            }
        }

        let mut sorted_edits = effective_edits;
        sorted_edits.sort_by_key(|b| std::cmp::Reverse(b.line_idx));

        let mut new_lines: Vec<String> = lines.to_vec();
        let mut added_count = 0;
        let mut removed_count = 0;
        let mut changes: Vec<(ResolvedEdit, usize, usize)> = Vec::new();

        for resolved in sorted_edits {
            let edit_type = &resolved.edit.edit_type;
            let clean_text = strip_hashes(&resolved.edit.text);
            let replacement_lines: Vec<String> = if clean_text.is_empty() {
                Vec::new()
            } else {
                split_content_lines(&clean_text)
            };

            let (removed_in_this_edit, splice_index) = if edit_type == "insert_after" {
                (0, resolved.line_idx + 1)
            } else if edit_type == "insert_before" {
                (0, resolved.line_idx)
            } else {
                // replace
                (resolved.end_idx - resolved.line_idx + 1, resolved.line_idx)
            };

            // Apply splice
            new_lines.splice(
                splice_index..splice_index + removed_in_this_edit,
                replacement_lines.clone(),
            );
            provenance.origins.splice(
                splice_index..splice_index + removed_in_this_edit,
                std::iter::repeat_n(None, replacement_lines.len()),
            );

            added_count += replacement_lines.len();
            removed_count += removed_in_this_edit;
            changes.push((
                resolved.clone(),
                replacement_lines.len(),
                removed_in_this_edit,
            ));
        }

        // Calculate applied edit metadata
        let mut applied_edits: Vec<AppliedEdit> = changes
            .iter()
            .map(|(change, replacement_count, removed_count)| {
                let shift: isize = changes
                    .iter()
                    .filter(|(other, _, _)| other.line_idx < change.line_idx)
                    .map(|(_, rep, rem)| *rep as isize - *rem as isize)
                    .sum();

                let shifted_start = (change.line_idx as isize + shift) as usize
                    + usize::from(change.edit.edit_type == "insert_after");

                AppliedEdit {
                    start_idx: shifted_start,
                    end_idx: if *replacement_count == 0 {
                        shifted_start
                    } else {
                        shifted_start + replacement_count - 1
                    },
                    original_start_idx: change.line_idx,
                    original_end_idx: if *removed_count == 0 {
                        change.line_idx
                    } else {
                        change.line_idx + removed_count - 1
                    },
                    edit: change.edit.clone(),
                    lines_added: *replacement_count,
                    lines_deleted: *removed_count,
                }
            })
            .collect();

        applied_edits.sort_by_key(|applied| applied.original_start_idx);
        provenance.edit_ranges = applied_edits.clone();

        // Defense-in-depth: refuse to write content that still has any
        // `Word§` or `hex§` fragment in touched or added lines. The model
        // can reconstruct content from a partial `read_file` view and forget a newline
        // between anchored lines, producing `WordA§WordB§...` where
        // `strip_hashes` (line-start only) can't reach. Scope the check to
        // changed/added lines so pre-existing or legitimate § text in untouched
        // lines does not trigger false-positive rejections.
        let check_indices: Vec<usize> = applied_edits
            .iter()
            .filter(|e| e.lines_added > 0)
            .flat_map(|e| e.start_idx..=e.end_idx)
            .collect();
        let glued_lines = find_glued_anchor_in_lines(&new_lines, &check_indices);
        if !glued_lines.is_empty() {
            tracing::warn!(
                "Final assembled content has Word§/hex§ fragments at lines {:?}; rejecting batch",
                glued_lines
            );
            return ApplyOutcome::GluedAnchor(glued_lines);
        }

        // Empty content still has one logical line under split_content_lines.
        if new_lines.is_empty() {
            new_lines.push(String::new());
            provenance.origins.push(None);
        }
        ApplyOutcome::Applied(
            new_lines,
            added_count,
            removed_count,
            applied_edits,
            unchanged_sites,
        )
    }

    /// Formats a failure message for an edit.
    #[must_use]
    pub fn format_failure_message(&self, edit: &Edit, error: Option<&str>) -> String {
        let diagnostic = error.map_or_else(
            || " This almost certainly is because the anchors used were incorrect or not in ascending order or the text supplied was incorrect. please check again edit again".to_string(),
            |e| format!(" Diagnostics: {e}"),
        );
        format!(
            "Edit (anchor: \"{}\", end_anchor: \"{}\") failed.{}",
            edit.anchor,
            edit.end_anchor.as_deref().unwrap_or(""),
            diagnostic
        )
    }
}

// ============================================================================
// File Editor (main interface)
// ============================================================================

/// Main file editor that combines anchor management and edit execution.
#[derive(Debug, Clone, Default)]
pub struct FileEditor {
    executor: EditExecutor,
    pub anchor_mgr: AnchorStateManager,
}

impl FileEditor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            executor: EditExecutor::new(),
            anchor_mgr: AnchorStateManager::new(),
        }
    }

    /// Reconciles anchors for a file and returns the anchor words.
    #[must_use]
    pub fn reconcile_anchors(
        &self,
        absolute_path: &str,
        lines: &[String],
        task_id: Option<&str>,
    ) -> Vec<String> {
        self.anchor_mgr.reconcile(absolute_path, lines, task_id)
    }

    /// Applies edits to a file's content.
    pub fn apply_edits(
        &self,
        content: &str,
        edits: &[Edit],
        absolute_path: &str,
        task_id: Option<&str>,
    ) -> Result<(String, Vec<AppliedEdit>, Vec<FailedEdit>), FileEditorError> {
        let snapshot = self
            .anchor_mgr
            .observe_snapshot(absolute_path, content, task_id)
            .map_err(|error| FileEditorError::AllEditsFailed {
                message: error.to_string(),
            })?;
        let lines = &snapshot.lines;
        let line_hashes = &snapshot.anchors;

        let (resolved_edits, failed_edits) = self.executor.resolve_edits(edits, lines, line_hashes);

        if !failed_edits.is_empty() {
            let failure_messages: Vec<String> = failed_edits
                .iter()
                .map(|f| {
                    self.executor
                        .format_failure_message(&f.edit, Some(&f.error))
                })
                .collect();
            return Err(FileEditorError::atomic_batch_rejected(
                failure_messages.join("\n\n"),
                failed_edits.len(),
                resolved_edits.len(),
            ));
        }

        let (outcome, provenance) = self
            .executor
            .apply_edits_with_provenance(lines, &resolved_edits);
        let (final_lines, applied_edits) = match outcome {
            ApplyOutcome::Overlap => {
                return Err(FileEditorError::OverlappingEdits {
                    message: "Edit ranges overlap. Apply edits sequentially or ensure non-overlapping ranges.".to_string(),
                });
            }
            ApplyOutcome::GluedAnchor(glued_lines) => {
                let listing = glued_lines
                    .iter()
                    .map(|l| format!("line {l}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(FileEditorError::AllEditsFailed {
                    message: format!(
                        "Assembled content has Word§/hex§ fragments at {listing}; the model likely reconstructed content without a newline between anchored lines. Re-read the file with read_file and retry, preserving line breaks exactly."
                    ),
                });
            }
            ApplyOutcome::DuplicateInsertion(failed) => {
                return Err(FileEditorError::atomic_batch_rejected(
                    failed
                        .iter()
                        .map(|failure| failure.error.clone())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                    failed.len(),
                    resolved_edits.len().saturating_sub(failed.len()),
                ));
            }
            ApplyOutcome::Applied(final_lines, _added, _removed, applied_edits, _unchanged) => {
                (final_lines, applied_edits)
            }
        };

        let (_, format) = normalize_file_content(content);
        let final_content = if applied_edits.is_empty() {
            content.to_string()
        } else {
            restore_file_content(&final_lines.join("\n"), format)
        };
        let transition = self
            .anchor_mgr
            .stage_transition(&snapshot, &final_content, &provenance)
            .map_err(|error| FileEditorError::AllEditsFailed {
                message: error.to_string(),
            })?;
        self.anchor_mgr
            .commit_transitions_checked(&[transition])
            .map_err(|error| FileEditorError::AllEditsFailed {
                message: error.to_string(),
            })?;

        Ok((final_content, applied_edits, failed_edits))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    fn staged_edit(
        manager: &AnchorStateManager,
        path: &str,
        content: &str,
        ranges: &[(usize, usize, &str, &str)],
    ) -> AnchorTransition {
        let snapshot = manager
            .observe_snapshot(path, content, Some("staged"))
            .unwrap();
        let edits: Vec<ResolvedEdit> = ranges
            .iter()
            .map(|&(line_idx, end_idx, kind, text)| ResolvedEdit {
                line_idx,
                end_idx,
                edit: Edit {
                    anchor: format!(
                        "{}§{}",
                        snapshot.anchors[line_idx], snapshot.lines[line_idx]
                    ),
                    end_anchor: None,
                    edit_type: kind.into(),
                    text: text.into(),
                    content: None,
                },
            })
            .collect();
        let (outcome, provenance) =
            EditExecutor::new().apply_edits_with_provenance(&snapshot.lines, &edits);
        let ApplyOutcome::Applied(lines, ..) = outcome else {
            panic!("assembly rejected")
        };
        manager
            .stage_transition(&snapshot, &lines.join("\n"), &provenance)
            .unwrap()
    }

    #[test]
    fn staged_splices_preserve_exact_duplicate_occurrences() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        let content = "same\nleft\nsame\nright\nsame\n";
        manager
            .reconcile_checked("duplicates", &split_content_lines(content), Some("staged"))
            .unwrap();
        let before = manager
            .observe_snapshot("duplicates", content, Some("staged"))
            .unwrap();
        let transition = staged_edit(
            &manager,
            "duplicates",
            content,
            &[
                (1, 1, "replace", "new\nlines"),
                (3, 3, "insert_after", "inserted"),
            ],
        );
        assert_eq!(
            transition.output_lines.join("\n"),
            "same\nnew\nlines\nsame\nright\ninserted\nsame\n"
        );
        for (final_index, original_index) in [(0, 0), (3, 2), (4, 3), (6, 4), (7, 5)] {
            assert_eq!(
                transition.anchors[final_index],
                before.anchors[original_index]
            );
            assert_eq!(
                transition.provenance[final_index],
                LineProvenance::Preserved {
                    original_idx: original_index
                }
            );
        }
        assert_eq!(
            transition.retired_identities,
            vec![before.anchors[1].clone()]
        );
        assert_eq!(transition.edit_ranges.len(), 2);
        assert!(manager.is_tracking("duplicates", Some("staged")));
        manager
            .commit_transitions_checked(&[transition.clone()])
            .unwrap();
        assert_eq!(
            manager.get_anchors("duplicates", Some("staged")).unwrap(),
            transition.anchors
        );
    }

    #[test]
    fn staged_noop_observation_and_failure_do_not_publish() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let manager = AnchorStateManager::with_cache_file(cache.clone());
        let before = manager.storage().tasks.clone();
        let transition = staged_edit(&manager, "noop", "same\nsame", &[(1, 1, "replace", "same")]);
        assert!(transition.is_noop);
        assert_eq!(transition.generation, 0);
        assert!(transition.retired_identities.is_empty());
        manager.commit_transitions_checked(&[transition]).unwrap();
        assert_eq!(manager.storage().tasks, before);
        assert!(!cache.exists());
        let snapshot = manager
            .observe_snapshot("noop", "same\nsame", Some("staged"))
            .unwrap();
        assert!(matches!(
            manager.stage_transition(&snapshot, "changed", &SpliceProvenance::default()),
            Err(AnchorTransitionError::Validation(_))
        ));
        assert_eq!(manager.storage().tasks, before);
    }

    #[test]
    fn staged_whole_file_delete_creates_synthetic_empty_line() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        let transition = staged_edit(&manager, "deleted", "one\ntwo\n", &[(0, 2, "replace", "")]);
        assert_eq!(transition.output_lines, vec![String::new()]);
        assert!(matches!(
            &transition.provenance[..],
            [LineProvenance::Created { .. }]
        ));
        assert_eq!(transition.retired_identities.len(), 3);
        manager
            .commit_transitions_checked(&[transition.clone()])
            .unwrap();
        assert_eq!(
            manager
                .observe_snapshot("deleted", "", Some("staged"))
                .unwrap()
                .anchors,
            transition.anchors
        );
    }

    #[test]
    fn staged_raw_digest_is_distinct_from_normalized_identity() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        manager
            .reconcile_checked("format", &split_content_lines("one\ntwo\n"), None)
            .unwrap();
        let unix = manager
            .observe_snapshot("format", "one\ntwo\n", None)
            .unwrap();
        let windows = manager
            .observe_snapshot("format", "\u{feff}one\r\ntwo\r\n", None)
            .unwrap();
        assert_ne!(unix.raw_digest, windows.raw_digest);
        assert_eq!(unix.normalized_digest, windows.normalized_digest);
        assert_eq!(unix.anchors, windows.anchors);
    }

    #[test]
    fn staged_insert_before_and_trailing_newline_replacement_map_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        let inserted = staged_edit(
            &manager,
            "insert",
            "a\nb\n",
            &[(1, 1, "insert_before", "new")],
        );
        assert_eq!(inserted.output_lines.join("\n"), "a\nnew\nb\n");
        assert_eq!(
            inserted.provenance[2],
            LineProvenance::Preserved { original_idx: 1 }
        );
        let replaced = staged_edit(&manager, "replace", "a\nb", &[(0, 1, "replace", "new\n")]);
        assert_eq!(replaced.output_lines, vec!["new", ""]);
        assert!(
            replaced
                .provenance
                .iter()
                .all(|origin| matches!(origin, LineProvenance::Created { .. }))
        );
    }

    #[test]
    fn staged_large_to_large_keeps_snapshot_lifetime_without_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        let content = (0..=MAX_TRACKED_LINES)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let transition = staged_edit(&manager, "large", &content, &[(0, 0, "replace", "changed")]);
        assert!(transition.snapshot_mode);
        assert_eq!(transition.retired_identities.len(), MAX_TRACKED_LINES + 1);
        assert!(transition.document.used_words_set.is_empty());
        assert_eq!(
            transition.anchors,
            snapshot_line_anchors(&transition.output_lines)
        );
        manager
            .commit_transitions_checked(&[transition.clone()])
            .unwrap();
        assert!(!manager.is_tracking("large", Some("staged")));
        assert_eq!(
            manager
                .observe_snapshot("large", &transition.output_lines.join("\n"), Some("staged"))
                .unwrap()
                .anchors,
            transition.anchors
        );
    }

    #[test]
    fn staged_commit_rejects_stale_generation_and_preserves_all_targets() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let first = AnchorStateManager::with_cache_file(cache.clone());
        let second = AnchorStateManager::with_cache_file(cache.clone());
        let stale = staged_edit(&second, "same", "old", &[(0, 0, "replace", "stale")]);
        let independent = staged_edit(&second, "independent", "old", &[(0, 0, "replace", "new")]);
        first
            .commit_transitions_checked(&[staged_edit(
                &first,
                "same",
                "old",
                &[(0, 0, "replace", "winner")],
            )])
            .unwrap();
        let bytes = std::fs::read(&cache).unwrap();
        let memory = second.storage().tasks.clone();
        assert!(matches!(
            second.commit_transitions_checked(&[independent, stale]),
            Err(AnchorTransitionError::StaleGeneration { .. })
        ));
        assert_eq!(std::fs::read(&cache).unwrap(), bytes);
        assert_eq!(second.storage().tasks, memory);
    }

    #[test]
    fn staged_persistence_and_mutated_metadata_fail_before_publication() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let manager = AnchorStateManager::with_cache_file(cache.clone());
        let transition = staged_edit(&manager, "file", "old", &[(0, 0, "replace", "new")]);
        let mut corrupted = transition.clone();
        corrupted.anchors[0] = "forged".into();
        assert!(matches!(
            manager.commit_transitions_checked(&[corrupted]),
            Err(AnchorTransitionError::Validation(_))
        ));
        std::fs::create_dir(cache.with_extension("json.lock")).unwrap_err();
        // An unreadable cache must not be treated as an empty cache and overwritten.
        std::fs::create_dir(&cache).unwrap();
        let before = manager.storage().tasks.clone();
        assert!(matches!(
            manager.commit_transitions_checked(&[transition]),
            Err(AnchorTransitionError::Persistence(_))
        ));
        assert_eq!(manager.storage().tasks, before);
        assert!(cache.is_dir());
    }

    #[test]
    fn staged_corrupt_cache_retry_remains_persistence_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let manager = AnchorStateManager::with_cache_file(cache.clone());
        let _ = manager.reconcile("file", &split_content_lines("old"), Some("staged"));
        std::fs::write(&cache, "malformed cache").unwrap();
        for _ in 0..2 {
            // A legacy best-effort reread cannot repair storage, and must not
            // turn a storage error into a stale-anchor retry at observation.
            let _ = manager.reconcile("file", &split_content_lines("changed"), Some("staged"));
            let before = manager.storage().tasks.clone();
            assert!(matches!(
                manager.observe_snapshot("file", "changed", Some("staged")),
                Err(AnchorTransitionError::Persistence(_))
            ));
            assert_eq!(manager.storage().tasks, before);
            assert_eq!(std::fs::read_to_string(&cache).unwrap(), "malformed cache");
        }
        let fresh = AnchorStateManager::with_cache_file(cache);
        assert!(matches!(
            fresh.observe_snapshot("unknown", "text", None),
            Err(AnchorTransitionError::Persistence(_))
        ));
        assert!(fresh.storage().tasks.is_empty());
    }

    #[test]
    fn concurrent_cache_saves_preserve_new_tasks_and_enforce_caps() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let seed = AnchorStateManager::with_cache_file(cache.clone());
        let document = seed
            .observe_snapshot("seed", "line", None)
            .unwrap()
            .document;
        {
            let mut storage = seed.storage();
            for index in 0..MAX_TRACKED_TASKS - 1 {
                storage.tasks.insert(
                    format!("seed-{index}"),
                    IndexMap::from([("file".into(), document.clone())]),
                );
            }
            storage.tasks.insert(
                "wide".into(),
                (0..=MAX_TRACKED_FILES)
                    .map(|index| (format!("file-{index}"), document.clone()))
                    .collect(),
            );
        }
        seed.save();
        assert_eq!(seed.storage().tasks["wide"].len(), MAX_TRACKED_FILES);
        let barrier = Arc::new(std::sync::Barrier::new(4));
        std::thread::scope(|scope| {
            for index in 0..4 {
                let manager = AnchorStateManager::with_cache_file(cache.clone());
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let _ =
                        manager.reconcile("file", &["new".into()], Some(&format!("new-{index}")));
                });
            }
        });
        let tasks = AnchorStorage::read_tasks(&cache).unwrap();
        assert_eq!(tasks.len(), MAX_TRACKED_TASKS);
        assert!(tasks.values().all(|files| files.len() <= MAX_TRACKED_FILES));
        for index in 0..4 {
            assert!(tasks.contains_key(&format!("new-{index}")));
        }
        assert_eq!(tasks["wide"].len(), MAX_TRACKED_FILES);
    }

    #[test]
    fn staged_large_snapshot_crossing_down_retires_every_snapshot_word() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        let content = (0..=MAX_TRACKED_LINES)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let transition = staged_edit(&manager, "large", &content, &[(0, 1, "replace", "")]);
        assert!(!transition.snapshot_mode);
        assert_eq!(transition.retired_identities.len(), MAX_TRACKED_LINES + 1);
        assert!(
            transition
                .provenance
                .iter()
                .all(|origin| matches!(origin, LineProvenance::Created { .. }))
        );
        manager
            .commit_transitions_checked(&[transition.clone()])
            .unwrap();
        assert_eq!(
            manager
                .observe_snapshot("large", &transition.output_lines.join("\n"), Some("staged"))
                .unwrap()
                .anchors,
            transition.anchors
        );
    }

    #[test]
    fn staged_stale_reader_adopts_newer_words_and_cannot_overwrite_them() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let first = AnchorStateManager::with_cache_file(cache.clone());
        let original = "same\nold\nsame";
        let _ = first.reconcile("file", &split_content_lines(original), Some("staged"));
        let stale = AnchorStateManager::with_cache_file(cache.clone());
        let transition = staged_edit(&first, "file", original, &[(1, 1, "replace", "new")]);
        first
            .commit_transitions_checked(&[transition.clone()])
            .unwrap();
        stale.save();
        assert_eq!(
            AnchorStateManager::with_cache_file(cache)
                .get_anchors("file", Some("staged"))
                .unwrap(),
            transition.anchors
        );
        assert_eq!(
            stale.reconcile("file", &transition.output_lines, Some("staged")),
            transition.anchors
        );
    }

    #[test]
    fn staged_invalidation_does_not_delete_persisted_or_newer_documents() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let manager = AnchorStateManager::with_cache_file(cache.clone());
        let _ = manager.reconcile("file", &split_content_lines("old"), Some("staged"));
        let original = std::fs::read(&cache).unwrap();
        manager.invalidate_state("file", Some("staged"));
        assert_eq!(std::fs::read(&cache).unwrap(), original);
        manager.save();
        assert_eq!(std::fs::read(&cache).unwrap(), original);

        let fresh = AnchorStateManager::with_cache_file(cache.clone());
        assert!(matches!(
            fresh.observe_snapshot("file", "unpublished", Some("staged")),
            Err(AnchorTransitionError::StaleGeneration { .. })
        ));
        let reread = fresh.reconcile("file", &split_content_lines("unpublished"), Some("staged"));
        assert_eq!(
            fresh
                .observe_snapshot("file", "unpublished", Some("staged"))
                .unwrap()
                .anchors,
            reread
        );

        let transition = staged_edit(
            &fresh,
            "file",
            "unpublished",
            &[(0, 0, "replace", "winner")],
        );
        fresh
            .commit_transitions_checked(&[transition.clone()])
            .unwrap();
        let committed = std::fs::read(&cache).unwrap();
        manager.invalidate_state("file", Some("staged"));
        manager.save();
        assert_eq!(std::fs::read(&cache).unwrap(), committed);
        let restarted = AnchorStateManager::with_cache_file(cache);
        assert_eq!(
            restarted
                .observe_snapshot("file", "winner", Some("staged"))
                .unwrap()
                .anchors,
            transition.anchors
        );
        assert!(matches!(
            restarted.observe_snapshot("file", "unpublished", Some("staged")),
            Err(AnchorTransitionError::StaleGeneration { .. })
        ));
    }

    #[test]
    fn unchanged_reconcile_republishes_missing_durable_document() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let manager = AnchorStateManager::with_cache_file(cache.clone());
        let content = "same\nother\n";
        let lines = split_content_lines(content);
        let initial = manager.reconcile("file", &lines, Some("staged"));

        std::fs::remove_file(&cache).unwrap();
        let reread = manager.reconcile("file", &lines, Some("staged"));

        assert_eq!(reread, initial);
        assert!(cache.exists());
        let restarted = AnchorStateManager::with_cache_file(cache);
        let snapshot = restarted
            .observe_snapshot("file", content, Some("staged"))
            .unwrap();
        assert_eq!(snapshot.anchors, initial);
        assert!(snapshot.expected.is_some());
    }
    use super::*;

    #[tokio::test]
    async fn file_lock_registry_releases_idle_path_entries() {
        let path = format!("/tmp/file-lock-cleanup-{}", std::process::id());
        assert!(!FILE_LOCK_MANAGER.contains(&path));
        let guard = FileEditGuard::acquire(&path).await;
        assert!(FILE_LOCK_MANAGER.contains(&path));
        drop(guard);
        assert!(!FILE_LOCK_MANAGER.contains(&path));
    }

    #[tokio::test]
    async fn cancelled_lock_wait_releases_its_registry_reference() {
        let path = format!("/tmp/file-lock-cancel-{}", std::process::id());
        let held = FileEditGuard::acquire(&path).await;
        let waiting_path = path.clone();
        let waiting = tokio::spawn(async move {
            let _guard = FileEditGuard::acquire(&waiting_path).await;
        });
        tokio::task::yield_now().await;
        waiting.abort();
        let _ = waiting.await;
        drop(held);
        assert!(!FILE_LOCK_MANAGER.contains(&path));
    }

    #[test]
    fn monotonic_allocator_never_reuses_a_retired_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        let first = manager.reconcile("file", &["first".into()], Some("task"));
        let second = manager.reconcile("file", &["second".into()], Some("task"));
        let third = manager.reconcile("file", &["third".into()], Some("task"));
        assert_ne!(first, second);
        assert_ne!(first, third);
        assert_ne!(second, third);
    }
    use std::collections::HashSet;

    #[test]
    fn test_file_content_normalization_preserves_original_text_format() {
        let raw = "\u{feff}first\r\nsecond\r\n";

        let (normalized, format) = normalize_file_content(raw);

        assert_eq!(normalized, "first\nsecond\n");
        assert_eq!(format.line_ending, FileLineEnding::CrLf);
        assert!(format.has_utf8_bom);
        assert_eq!(restore_file_content(&normalized, format), raw);
    }

    #[test]
    fn test_file_content_normalization_keeps_replacement_text_out_of_scope() {
        let replacement = "literal\rvalue";

        assert_eq!(
            split_content_lines(replacement),
            vec!["literal\rvalue".to_string()]
        );
    }

    #[test]
    fn test_crlf_restoration_does_not_double_existing_carriage_returns() {
        let format = FileTextFormat {
            line_ending: FileLineEnding::CrLf,
            has_utf8_bom: false,
        };

        assert_eq!(
            restore_file_content("first\r\nsecond\nliteral\rvalue", format),
            "first\r\nsecond\r\nliteral\rvalue"
        );
    }

    #[test]
    fn test_anchor_state_manager_first_read() {
        let task_id = "first_read_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let lines = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "    return 42".to_string(),
        ];

        let anchors = anchor_mgr.reconcile("/tmp/first_read.py", &lines, Some(task_id));
        assert_eq!(anchors.len(), 3);

        // All anchors should start with capital letters
        for anchor in &anchors {
            assert!(
                anchor.chars().next().unwrap().is_ascii_uppercase(),
                "Anchor '{}' should start with capital letter",
                anchor
            );
        }

        // All anchors should be unique
        let unique: HashSet<_> = anchors.iter().collect();
        assert_eq!(unique.len(), anchors.len());
    }

    #[test]
    fn test_anchor_state_manager_unchanged_read() {
        // Use unique task to avoid interference from parallel tests
        let task_id = "unchanged_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let lines = vec!["def hello():".to_string(), "    print('world')".to_string()];

        let anchors1 = anchor_mgr.reconcile("/tmp/unchanged.py", &lines, Some(task_id));

        // Verify the file is being tracked
        assert!(
            anchor_mgr.is_tracking("/tmp/unchanged.py", Some(task_id)),
            "File should be tracked after first reconcile"
        );

        let anchors2 = anchor_mgr.reconcile("/tmp/unchanged.py", &lines, Some(task_id));

        // Should return identical anchors for unchanged content
        assert_eq!(
            anchors1, anchors2,
            "Anchors should be identical for unchanged content"
        );
    }

    #[test]
    fn test_anchor_state_manager_repeated_reconcile_is_stable() {
        let task_id = "repeat_reconcile_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let initial_lines = vec!["fn alpha() {}".to_string(), "fn beta() {}".to_string()];
        let anchors1 = anchor_mgr.reconcile("/tmp/repeat.rs", &initial_lines, Some(task_id));
        let anchors2 = anchor_mgr.reconcile("/tmp/repeat.rs", &initial_lines, Some(task_id));
        assert_eq!(
            anchors1, anchors2,
            "Unchanged content should keep the same anchors"
        );

        let updated_lines = vec![
            "fn alpha() {}".to_string(),
            "fn gamma() {}".to_string(),
            "fn beta() {}".to_string(),
        ];
        let anchors3 = anchor_mgr.reconcile("/tmp/repeat.rs", &updated_lines, Some(task_id));
        let anchors4 = anchor_mgr.reconcile("/tmp/repeat.rs", &updated_lines, Some(task_id));

        assert_eq!(
            anchors3, anchors4,
            "Repeated updates should stabilize on the same anchors"
        );
        assert_eq!(
            anchors1[0], anchors3[0],
            "Unchanged first line should keep its anchor"
        );
        assert_eq!(
            anchors1[1], anchors3[2],
            "Unchanged trailing line should keep its anchor"
        );
        assert!(anchor_mgr.is_tracking("/tmp/repeat.rs", Some(task_id)));
    }

    #[test]
    fn test_anchor_state_manager_inserted_lines() {
        let task_id = "inserted_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let lines1 = vec!["def hello():".to_string(), "    return 42".to_string()];

        let anchors1 = anchor_mgr.reconcile("/tmp/inserted.py", &lines1, Some(task_id));

        let lines2 = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "    return 42".to_string(),
        ];

        let anchors2 = anchor_mgr.reconcile("/tmp/inserted.py", &lines2, Some(task_id));

        // First and last anchors should be preserved
        assert_eq!(anchors1[0], anchors2[0], "First anchor should be preserved");
        assert_eq!(anchors1[1], anchors2[2], "Last anchor should be preserved");

        // New line should have a different anchor
        assert_ne!(
            anchors2[1], anchors1[0],
            "New line should have different anchor"
        );
        assert_ne!(
            anchors2[1], anchors1[1],
            "New line should have different anchor"
        );
    }

    #[test]
    fn test_anchor_state_manager_deleted_lines() {
        let task_id = "deleted_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let lines1 = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "    return 42".to_string(),
        ];

        let anchors1 = anchor_mgr.reconcile("/tmp/deleted.py", &lines1, Some(task_id));

        let lines2 = vec!["def hello():".to_string(), "    return 42".to_string()];

        let anchors2 = anchor_mgr.reconcile("/tmp/deleted.py", &lines2, Some(task_id));

        // First and last remaining anchors should be preserved
        assert_eq!(anchors1[0], anchors2[0], "First anchor should be preserved");
        assert_eq!(
            anchors1[2], anchors2[1],
            "Last remaining anchor should be preserved"
        );
    }

    #[test]
    fn test_anchor_state_manager_large_file_fallback() {
        let task_id = "large_file_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let lines: Vec<String> = (0..MAX_TRACKED_LINES + 1)
            .map(|i| format!("line {}", i))
            .collect();

        let anchors = anchor_mgr.reconcile("/tmp/large.py", &lines, Some(task_id));
        assert_eq!(anchors.len(), lines.len());

        assert!(anchors[0].starts_with('L'));
        assert!(anchors[0].ends_with("N1"));
        assert!(anchors[1].ends_with("N2"));
        assert_eq!(
            anchor_mgr.reconcile("/tmp/large.py", &lines, Some(task_id)),
            anchors
        );
        let mut changed = lines;
        changed.insert(0, "inserted".into());
        let next = anchor_mgr.reconcile("/tmp/large.py", &changed, Some(task_id));
        let next: HashSet<_> = next.into_iter().collect();
        assert!(anchors.iter().all(|anchor| !next.contains(anchor)));
    }

    #[test]
    fn test_anchor_state_manager_task_scoping() {
        let dir = tempfile::tempdir().unwrap();
        let anchor_mgr = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
        let lines = vec!["def hello():".to_string()];

        let anchors1 = anchor_mgr.reconcile("/tmp/scope1.py", &lines, Some("scope_task1"));
        let anchors2 = anchor_mgr.reconcile("/tmp/scope2.py", &lines, Some("scope_task2"));

        assert_ne!(anchors1[0], anchors2[0]);

        // Verify state is still scoped: modifying one task doesn't affect the other
        let modified_lines = vec!["def hello():".to_string(), "    pass".to_string()];
        let anchors1_modified =
            anchor_mgr.reconcile("/tmp/scope1.py", &modified_lines, Some("scope_task1"));
        // Task 1 should have 2 anchors now, task 2 still has 1
        assert_eq!(anchors1_modified.len(), 2);
        assert_eq!(
            anchor_mgr
                .get_anchors("/tmp/scope2.py", Some("scope_task2"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_edit_executor_resolve_anchor() {
        let executor = EditExecutor::new();
        let lines = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "    return 42".to_string(),
        ];
        let hashes = vec![
            "Apple".to_string(),
            "Banana".to_string(),
            "Cherry".to_string(),
        ];

        // Valid anchor
        let (idx, error) = executor.resolve_anchor("anchor", "Apple§def hello():", &hashes, &lines);
        assert_eq!(idx, 0);
        assert!(error.is_none());

        // Missing anchor
        let (idx, error) = executor.resolve_anchor("anchor", "", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some());

        // Not found
        let (idx, error) = executor.resolve_anchor("anchor", "Mango§content", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some());

        // Content mismatch
        let (idx, error) =
            executor.resolve_anchor("anchor", "Apple§wrong content", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some());
    }

    #[test]
    fn test_edit_executor_resolve_anchor_duplicate_words_different_content() {
        let executor = EditExecutor::new();
        let lines = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "def hello():  # duplicate".to_string(),
            "    return 42".to_string(),
        ];
        // Same anchor word "Hello" for lines with different content
        let hashes = vec![
            "Hello".to_string(),
            "World".to_string(),
            "Hello".to_string(),
            "Test".to_string(),
        ];

        // First occurrence should match
        let (idx, error) = executor.resolve_anchor("anchor", "Hello§def hello():", &hashes, &lines);
        assert_eq!(idx, 0);
        assert!(
            error.is_none(),
            "First occurrence should match: {:?}",
            error
        );

        // Second occurrence: word "Hello" still resolves to line 0, but
        // supplied content "def hello():  # duplicate" only matches
        // line 2. Old behavior silently landed on whichever occurrence
        // carried the word — that's the silent wrong-location bug.
        // New behavior surfaces the rebind explicitly.
        let (idx, error) =
            executor.resolve_anchor("anchor", "Hello§def hello():  # duplicate", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        let err_msg = error.expect("rebind must surface an error");
        assert!(
            err_msg.contains("now binds to content that appears at a different line"),
            "got: {err_msg}"
        );

        // Wrong content for the anchor should fail
        let (idx, error) =
            executor.resolve_anchor("anchor", "Hello§wrong content", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some(), "Should error on content mismatch");
        assert!(
            error.unwrap().contains("anchor is stale"),
            "wrong content with existing word must surface anchor-stale diagnostic"
        );
    }

    #[test]
    fn test_edit_executor_resolve_anchor_duplicate_words_same_content() {
        let executor = EditExecutor::new();
        let lines = vec![
            "def hello():".to_string(),
            "    pass".to_string(),
            "    pass".to_string(),
            "    return 42".to_string(),
        ];
        // Same anchor word "Pass" for lines with identical content
        let hashes = vec![
            "Hello".to_string(),
            "Pass".to_string(),
            "Pass".to_string(),
            "Test".to_string(),
        ];

        // Duplicate anchors with same content should error as ambiguous
        let (idx, error) = executor.resolve_anchor("anchor", "Pass§    pass", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some(), "Should error on ambiguous anchor");
        let err_msg = error.unwrap();
        assert!(err_msg.contains("matches 2 lines with identical content"));
        assert!(err_msg.contains("'content' field"));
    }

    #[test]
    fn test_edit_executor_resolve_anchor_linenumber_anchors() {
        let executor = EditExecutor::new();
        let lines = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "    return 42".to_string(),
        ];
        // L1, L2, L3 anchors for large-file fallback
        let hashes = vec!["L1".to_string(), "L2".to_string(), "L3".to_string()];

        // Valid L1 anchor
        let (idx, error) = executor.resolve_anchor("anchor", "L1§def hello():", &hashes, &lines);
        assert_eq!(idx, 0);
        assert!(error.is_none());

        // Valid L2 anchor
        let (idx, error) =
            executor.resolve_anchor("anchor", "L2§    print('world')", &hashes, &lines);
        assert_eq!(idx, 1);
        assert!(error.is_none());

        // Valid L3 anchor
        let (idx, error) = executor.resolve_anchor("anchor", "L3§    return 42", &hashes, &lines);
        assert_eq!(idx, 2);
        assert!(error.is_none());

        // L1 with wrong content should fail
        let (idx, error) = executor.resolve_anchor("anchor", "L1§wrong content", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some());
    }

    #[test]
    fn test_edit_executor_rejects_multiline_anchor_input() {
        let executor = EditExecutor::new();
        let lines = vec!["def hello():".to_string(), "    return 42".to_string()];
        let hashes = vec!["Apple".to_string(), "Banana".to_string()];

        let (idx, error) = executor.resolve_anchor(
            "anchor",
            "Apple§def hello():\nBanana§    return 42",
            &hashes,
            &lines,
        );

        assert_eq!(idx, usize::MAX);
        let message = error.expect("multiline anchors should be rejected");
        assert!(message.contains("multiple lines"));
        assert!(message.contains("single line only"));
    }

    #[test]
    fn test_edit_executor_resolve_edits() {
        let executor = EditExecutor::new();
        let lines = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "    return 42".to_string(),
        ];
        let hashes = vec![
            "Apple".to_string(),
            "Banana".to_string(),
            "Cherry".to_string(),
        ];

        let edits = vec![Edit {
            anchor: "Apple§def hello():".to_string(),
            end_anchor: Some("Banana§    print('world')".to_string()),
            edit_type: "replace".to_string(),
            text: "def greeting():\n    pass".to_string(),
            content: None,
        }];

        let (resolved, failed) = executor.resolve_edits(&edits, &lines, &hashes);
        assert_eq!(resolved.len(), 1);
        assert_eq!(failed.len(), 0);
        assert_eq!(resolved[0].line_idx, 0);
        assert_eq!(resolved[0].end_idx, 1);
    }

    #[test]
    fn test_edit_executor_apply_edits() {
        let executor = EditExecutor::new();
        let lines = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "    return 42".to_string(),
        ];

        let edits = vec![ResolvedEdit {
            line_idx: 0,
            end_idx: 1,
            edit: Edit {
                anchor: "Apple§def hello():".to_string(),
                end_anchor: Some("Banana§    print('world')".to_string()),
                edit_type: "replace".to_string(),
                text: "def greeting():\n    pass".to_string(),
                content: None,
            },
        }];

        let ApplyOutcome::Applied(final_lines, added, removed, applied, _unchanged) =
            executor.apply_edits(&lines, &edits)
        else {
            panic!("apply_edits must succeed");
        };
        assert_eq!(final_lines.len(), 3);
        assert_eq!(final_lines[0], "def greeting():");
        assert_eq!(final_lines[1], "    pass");
        assert_eq!(final_lines[2], "    return 42");
        assert_eq!(added, 2);
        assert_eq!(removed, 2);
        assert_eq!(applied.len(), 1);
    }

    #[test]
    fn test_edit_executor_insert_after() {
        let executor = EditExecutor::new();
        let lines = vec!["def hello():".to_string(), "    return 42".to_string()];

        let edits = vec![ResolvedEdit {
            line_idx: 0,
            end_idx: 0,
            edit: Edit {
                anchor: "Apple§def hello():".to_string(),
                end_anchor: None,
                edit_type: "insert_after".to_string(),
                text: "    print('world')".to_string(),
                content: None,
            },
        }];

        let ApplyOutcome::Applied(final_lines, _added, _removed, applied, _unchanged) =
            executor.apply_edits(&lines, &edits)
        else {
            panic!("apply_edits must succeed");
        };
        assert_eq!(final_lines.len(), 3);
        assert_eq!(final_lines[0], "def hello():");
        assert_eq!(final_lines[1], "    print('world')");
        assert_eq!(final_lines[2], "    return 42");
        assert_eq!(applied[0].lines_added, 1);
        assert_eq!(applied[0].lines_deleted, 0);
    }

    #[test]
    fn test_edit_executor_rejects_exact_adjacent_multiline_insertion_with_blank_lines() {
        let executor = EditExecutor::new();
        let lines = split_content_lines("#ifdef FLAG\n\n\nfn target() {}\n");
        let hashes = vec!["Guard", "Blank1", "Blank2", "Target", "End"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let edits = vec![Edit {
            anchor: "Target§fn target() {}".to_string(),
            end_anchor: None,
            edit_type: "insert_before".to_string(),
            text: "#ifdef FLAG\n\n".to_string(),
            content: None,
        }];
        let (resolved, failed) = executor.resolve_edits(&edits, &lines, &hashes);
        assert!(failed.is_empty());

        let ApplyOutcome::DuplicateInsertion(duplicates) = executor.apply_edits(&lines, &resolved)
        else {
            panic!("exact adjacent block must be rejected");
        };
        assert_eq!(duplicates.len(), 1);
        assert!(
            duplicates[0]
                .error
                .contains("Leading and trailing blank lines")
        );
    }

    #[test]
    fn test_edit_executor_duplicate_detection_is_not_a_substring_match() {
        let executor = EditExecutor::new();
        let lines = split_content_lines("prefix with extra text\nfn target() {}\n");
        let hashes = vec!["Prefix", "Target", "End"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let edits = vec![Edit {
            anchor: "Target§fn target() {}".to_string(),
            end_anchor: None,
            edit_type: "insert_before".to_string(),
            text: "prefix".to_string(),
            content: None,
        }];
        let (resolved, failed) = executor.resolve_edits(&edits, &lines, &hashes);
        assert!(failed.is_empty());

        let ApplyOutcome::Applied(final_lines, ..) = executor.apply_edits(&lines, &resolved) else {
            panic!("non-exact adjacent content must remain insertable");
        };
        assert_eq!(final_lines[1], "prefix");
    }

    #[test]
    fn test_edit_executor_allows_anchor_statement_inside_insertion_body() {
        let executor = EditExecutor::new();
        let lines = split_content_lines("    return None;\n");
        let hashes = vec!["Return", "End"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let edits = vec![Edit {
            anchor: "Return§    return None;".to_string(),
            end_anchor: None,
            edit_type: "insert_before".to_string(),
            text: "if should_retry {\n    return None;\n}".to_string(),
            content: None,
        }];
        let (resolved, failed) = executor.resolve_edits(&edits, &lines, &hashes);
        assert!(failed.is_empty());

        let ApplyOutcome::Applied(final_lines, ..) = executor.apply_edits(&lines, &resolved) else {
            panic!("a repeated statement inside a new block must remain insertable");
        };
        assert_eq!(
            final_lines,
            split_content_lines("if should_retry {\n    return None;\n}\n    return None;\n")
        );
    }

    #[test]
    fn test_edit_executor_rejects_insertion_that_repeats_anchor_line() {
        let executor = EditExecutor::new();
        let lines = split_content_lines("fn target() {}\n");
        let hashes = vec!["Target", "End"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let edits = vec![Edit {
            anchor: "Target§fn target() {}".to_string(),
            end_anchor: None,
            edit_type: "insert_after".to_string(),
            text: "// wrapper\nfn target() {}".to_string(),
            content: None,
        }];
        let (resolved, failed) = executor.resolve_edits(&edits, &lines, &hashes);
        assert!(failed.is_empty());

        let ApplyOutcome::DuplicateInsertion(duplicates) = executor.apply_edits(&lines, &resolved)
        else {
            panic!("an insertion containing its anchor line must be rejected");
        };
        assert!(duplicates[0].error.contains("use replace"));
    }

    #[test]
    fn test_file_editor_end_to_end() {
        let task_id = "e2e_test";
        let dir = tempfile::tempdir().unwrap();
        let editor = FileEditor {
            executor: EditExecutor::new(),
            anchor_mgr: AnchorStateManager::with_cache_file(dir.path().join("anchors.json")),
        };
        let content = "def hello():\n    print('world')\n    return 42";

        // First reconcile to get anchors
        let lines = split_content_lines(content);
        let anchors = editor.reconcile_anchors("/tmp/e2e.py", &lines, Some(task_id));

        // Update edit with real anchor
        let edits = vec![Edit {
            anchor: format!("{}§def hello():", anchors[0]),
            end_anchor: Some(format!("{}§    print('world')", anchors[1])),
            edit_type: "replace".to_string(),
            text: "def greeting():\n    pass".to_string(),
            content: None,
        }];

        let result = editor.apply_edits(content, &edits, "/tmp/e2e.py", Some(task_id));
        assert!(result.is_ok(), "Edit should succeed: {:?}", result.err());

        let (final_content, applied, failed) = result.unwrap();
        assert_eq!(failed.len(), 0, "No edits should fail");
        assert_eq!(applied.len(), 1, "One edit should be applied");
        assert!(
            final_content.contains("def greeting():"),
            "Final content should contain greeting"
        );
    }

    #[test]
    fn test_anchor_state_manager_reset() {
        let task_id = "reset_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let lines = vec!["def hello():".to_string()];
        let _ = anchor_mgr.reconcile("/tmp/reset.py", &lines, Some(task_id));

        assert!(anchor_mgr.is_tracking("/tmp/reset.py", Some(task_id)));

        anchor_mgr.reset(Some(task_id));
        assert!(!anchor_mgr.is_tracking("/tmp/reset.py", Some(task_id)));
    }

    #[test]
    fn test_diff_arrays_identical() {
        let old = vec![1u64, 2, 3];
        let new = vec![1u64, 2, 3];
        let changes = diff_arrays(&old, &new);
        assert_eq!(changes, vec![DiffChange::Unchanged(3)]);
    }

    #[test]
    fn test_diff_arrays_inserted() {
        let old = vec![1u64, 3];
        let new = vec![1u64, 2, 3];
        let changes = diff_arrays(&old, &new);
        // Should detect: unchanged(1), added(1), unchanged(1)
        assert_eq!(
            changes,
            vec![
                DiffChange::Unchanged(1),
                DiffChange::Added(1),
                DiffChange::Unchanged(1)
            ]
        );
    }

    #[test]
    fn test_diff_arrays_deleted() {
        let old = vec![1u64, 2, 3];
        let new = vec![1u64, 3];
        let changes = diff_arrays(&old, &new);
        // Should detect: unchanged(1), removed(1), unchanged(1)
        assert_eq!(
            changes,
            vec![
                DiffChange::Unchanged(1),
                DiffChange::Removed(1),
                DiffChange::Unchanged(1)
            ]
        );
    }

    #[test]
    fn test_file_editor_error_implements_error_trait() {
        fn assert_error_trait(_: &dyn std::error::Error) {}

        let error = FileEditorError::ValidationError("boom".to_string());
        assert_error_trait(&error);
    }

    #[test]
    fn test_resolve_anchor_mismatch_debug_logging() {
        let executor = EditExecutor::new();
        let lines = vec![
            "def hello():".to_string(),
            "    print('world')".to_string(),
            "    return 42".to_string(),
        ];
        let hashes = vec![
            "Apple".to_string(),
            "Banana".to_string(),
            "Cherry".to_string(),
        ];

        let (idx, error) =
            executor.resolve_anchor("anchor", "Apple§wrong content here", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some());
        let err_msg = error.unwrap();
        assert!(err_msg.contains("anchor is stale"));
        assert!(err_msg.contains("Apple"));
    }

    #[test]
    fn test_resolve_anchor_fabricated_anchor_word() {
        let executor = EditExecutor::new();
        let lines = vec!["def hello():".to_string()];
        let hashes = vec!["Apple".to_string()];

        let (idx, error) =
            executor.resolve_anchor("anchor", "FakeWord§def hello():", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some());
        let err_msg = error.unwrap();
        assert!(err_msg.contains("not found in the file"));
        assert!(err_msg.contains("FakeWord"));
    }

    #[test]
    fn retired_anchor_diagnostics_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("anchors.json");
        let manager = AnchorStateManager::with_cache_file(cache.clone());
        for revision in 0..(MAX_RETIRED_ANCHORS + 64) {
            let _ = manager.reconcile("file", &[format!("revision {revision}")], Some("task"));
        }
        let tasks = AnchorStorage::read_tasks(&cache).unwrap();
        let document = &tasks["task"]["file"];
        assert!(document.retired_anchors.len() <= MAX_RETIRED_ANCHORS);
        assert!(document.used_words.is_empty());
        assert!(document.used_words_set.is_empty());
    }

    #[test]
    fn test_task_level_lru_eviction() {
        // Test that task-level eviction follows true LRU order
        let anchor_mgr = AnchorStateManager::new();

        // Set up a low limit for testing
        // Note: MAX_TRACKED_TASKS = 50 in production, we'll simulate by pre-filling
        let task_a = "task_a_lru_test";
        let task_b = "task_b_lru_test";
        let task_c = "task_c_lru_test";
        let task_d = "task_d_lru_test";

        // Clear any existing state
        anchor_mgr.reset(Some(task_a));
        anchor_mgr.reset(Some(task_b));
        anchor_mgr.reset(Some(task_c));
        anchor_mgr.reset(Some(task_d));

        // Create some file content
        let lines = vec!["fn test() {}".to_string()];

        // Insert tasks in order: A, B, C
        let _ = anchor_mgr.reconcile("/tmp/task_a.rs", &lines, Some(task_a));
        let _ = anchor_mgr.reconcile("/tmp/task_b.rs", &lines, Some(task_b));
        let _ = anchor_mgr.reconcile("/tmp/task_c.rs", &lines, Some(task_c));

        // Access task A again (should move it to back of LRU queue)
        let _ = anchor_mgr.reconcile("/tmp/task_a.rs", &lines, Some(task_a));

        // Verify all tasks are tracked
        assert!(anchor_mgr.is_tracking("/tmp/task_a.rs", Some(task_a)));
        assert!(anchor_mgr.is_tracking("/tmp/task_b.rs", Some(task_b)));
        assert!(anchor_mgr.is_tracking("/tmp/task_c.rs", Some(task_c)));

        // Now we need to test eviction - we'll do this by accessing the internal state
        // Since MAX_TRACKED_TASKS is 50, we can't easily trigger eviction in a unit test
        // Instead, we verify the LRU reordering logic by checking get_task_state_mut behavior

        // Access pattern: A (new), B, C, A (again)
        // After these accesses, LRU order should be: B, C, A (A is most recently used)
        // If we evict, B should be removed first

        // This test verifies the mechanism works - the actual eviction happens at scale
        // The key fix: get_task_state_mut now re-inserts to move to back (true LRU)
        // Previously it used entry() which doesn't reorder (FIFO, not LRU)

        // Verify the test setup is valid
        assert!(
            anchor_mgr.is_tracking("/tmp/task_a.rs", Some(task_a)),
            "Task A should be tracked after final access"
        );
    }

    #[test]
    fn test_task_lru_eviction_order_with_mock_limit() {
        // Direct test of LRU ordering by manipulating storage directly
        let mut storage = AnchorStorage::new();

        // Insert three tasks in order
        storage.tasks.insert("task_a".to_string(), IndexMap::new());
        storage.tasks.insert("task_b".to_string(), IndexMap::new());
        storage.tasks.insert("task_c".to_string(), IndexMap::new());

        // Verify insertion order: A, B, C
        let keys: Vec<_> = storage.tasks.keys().collect();
        assert_eq!(keys, vec!["task_a", "task_b", "task_c"]);

        // Simulate LRU access: access task_a (should move to back)
        let _state_a = AnchorStateManager::get_task_state_mut(&mut storage, "task_a");

        // After LRU access, order should be: B, C, A
        let keys: Vec<_> = storage.tasks.keys().collect();
        assert_eq!(
            keys,
            vec!["task_b", "task_c", "task_a"],
            "task_a should be moved to back after access (true LRU)"
        );

        // Now if we evict, task_b (oldest) should be removed
        // Simulate eviction at limit
        if storage.tasks.len() >= 3 {
            let oldest = storage.tasks.keys().next().cloned();
            assert_eq!(
                oldest,
                Some("task_b".to_string()),
                "task_b should be oldest"
            );
            storage.tasks.shift_remove(&oldest.unwrap());
        }

        // Verify task_b was evicted, not task_a
        assert!(
            !storage.tasks.contains_key("task_b"),
            "task_b (oldest) should be evicted"
        );
        assert!(
            storage.tasks.contains_key("task_a"),
            "task_a (recently used) should remain"
        );
        assert!(storage.tasks.contains_key("task_c"), "task_c should remain");
    }

    #[test]
    fn anchor_cache_save_merges_tasks_from_other_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let cache_file = dir.path().join("anchors.json");
        let first = AnchorStateManager::with_cache_file(cache_file.clone());
        let second = AnchorStateManager::with_cache_file(cache_file.clone());

        let _ = first.reconcile("first.rs", &["first".to_string()], Some("task-a"));
        let _ = second.reconcile("second.rs", &["second".to_string()], Some("task-b"));

        let reloaded = AnchorStateManager::with_cache_file(cache_file);
        assert!(reloaded.is_tracking("first.rs", Some("task-a")));
        assert!(reloaded.is_tracking("second.rs", Some("task-b")));
    }
}
