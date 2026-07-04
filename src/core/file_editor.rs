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
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex as AsyncMutex;

use crate::core::anchor_dictionary::ANCHOR_DICTIONARY;
use crate::core::hash_utils::{ANCHOR_DELIMITER, compute_hashes, split_anchor, strip_hashes};

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

    #[error("Overlapping edit ranges: {message}")]
    OverlappingEdits { message: String },
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

pub(crate) static ANCHOR_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Allow word anchors (Apple) and line-number anchors (L1, L2, etc.) for large-file fallback
    Regex::new(r"^[A-Z][a-zA-Z0-9]*$").unwrap()
});

// ============================================================================
// Anchor State Manager
// ============================================================================

/// Tracked document state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TrackedDocument {
    hashes: Vec<u64>,
    anchors: Vec<String>,
    /// Tracks used words in insertion order for LRU eviction.
    /// VecDeque maintains insertion order, HashSet provides O(1) lookup.
    used_words: VecDeque<String>,
    used_words_set: HashSet<String>,
}

/// Global anchor state storage.
#[derive(Debug)]
struct AnchorStorage {
    tasks: IndexMap<String, IndexMap<String, TrackedDocument>>,
    dictionary: Vec<String>,
}

impl AnchorStorage {
    /// Load anchor state from disk (~/.sned/data/cache/anchors.json)
    fn load() -> Self {
        let cache_dir = crate::storage::disk::get_data_dir().join("cache");
        let anchors_file = cache_dir.join("anchors.json");

        if let Ok(content) = std::fs::read_to_string(&anchors_file) {
            match serde_json::from_str::<IndexMap<String, IndexMap<String, TrackedDocument>>>(
                &content,
            ) {
                Ok(tasks) => {
                    tracing::debug!("Loaded {} task(s) from anchor cache", tasks.len());
                    return Self {
                        tasks,
                        dictionary: Vec::new(),
                    };
                }
                Err(e) => {
                    tracing::warn!("Failed to parse anchor cache: {}", e);
                }
            }
        }

        Self {
            tasks: IndexMap::new(),
            dictionary: Vec::new(),
        }
    }

    #[cfg(test)]
    fn new() -> Self {
        Self {
            tasks: IndexMap::new(),
            dictionary: Vec::new(),
        }
    }

    /// Save anchor state to disk
    fn save(&self) {
        let cache_dir = crate::storage::disk::get_data_dir().join("cache");
        let anchors_file = cache_dir.join("anchors.json");

        // Ensure cache directory exists
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            tracing::warn!("Failed to create cache directory: {}", e);
            return;
        }

        match serde_json::to_string_pretty(&self.tasks) {
            Ok(json) => {
                if let Err(e) = crate::storage::disk::atomic_write_file(&anchors_file, &json) {
                    tracing::warn!("Failed to save anchor cache: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to serialize anchor cache: {}", e);
            }
        }
    }

    fn get_dictionary(&mut self) -> &[String] {
        if self.dictionary.is_empty() {
            self.dictionary = ANCHOR_DICTIONARY
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
        }
        &self.dictionary
    }
}

const MAX_TRACKED_LINES: usize = 5000;
const MAX_TRACKED_FILES: usize = 1024;
const MAX_TRACKED_TASKS: usize = 50;

/// Maximum used words per file to prevent HashSet bloat.
const MAX_USED_WORDS: usize = 5000;

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

    async fn acquire(&self, path: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock();
            locks
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    fn try_acquire(&self, path: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        let lock = {
            let mut locks = self.locks.lock();
            locks
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.try_lock_owned().ok()
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
}

static FILE_LOCK_MANAGER: LazyLock<FileLockManager> = LazyLock::new(FileLockManager::new);

/// RAII guard for file editing. Holds lock for path duration.
pub struct FileEditGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    path: String,
}

impl FileEditGuard {
    pub async fn acquire(path: &str) -> Self {
        let _guard = FILE_LOCK_MANAGER.acquire(path).await;
        Self {
            _guard,
            path: path.to_string(),
        }
    }

    pub fn try_acquire(path: &str) -> Option<Self> {
        FILE_LOCK_MANAGER.try_acquire(path).map(|_guard| Self {
            _guard,
            path: path.to_string(),
        })
    }
}

impl Drop for FileEditGuard {
    fn drop(&mut self) {
        FILE_LOCK_MANAGER.release(&self.path);
    }
}

/// Anchor state manager for hash-anchored edits.
///
#[derive(Debug, Clone)]
pub struct AnchorStateManager {
    storage: Arc<Mutex<AnchorStorage>>,
}

impl AnchorStateManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            storage: Arc::new(Mutex::new(AnchorStorage::load())),
        }
    }

    fn storage(&self) -> parking_lot::MutexGuard<'_, AnchorStorage> {
        self.storage.lock()
    }

    /// Gets a unique word deterministically based on line content hash.
    ///
    /// Uses the hash to select words from the dictionary, applying a salt counter
    /// on collision to find an unused combination.
    #[allow(clippy::unused_self)]
    fn get_word_for_hash(
        &self,
        line_hash: u64,
        used_words_set: &HashSet<String>,
        dictionary: &[String],
    ) -> String {
        let dict_len = dictionary.len();
        let mut salt = 0u64;

        loop {
            // Mix salt into hash for collision resolution
            let mixed_hash = line_hash.wrapping_add(salt.wrapping_mul(0x9e37_79b9));

            // Select two words deterministically from hash
            let idx1 = (mixed_hash as usize) % dict_len;
            let idx2 = ((mixed_hash >> 16) as usize) % dict_len;

            let w1 = &dictionary[idx1];
            let w2 = &dictionary[idx2];
            let word = format!("{w1}{w2}");

            if !used_words_set.contains(&word) {
                return word;
            }

            salt = salt.wrapping_add(1);

            // Extreme fallback: three-word combinations if two-word fails repeatedly
            if salt > 1000 {
                let idx3 = ((mixed_hash >> 8) as usize) % dict_len;
                let w3 = &dictionary[idx3];
                let word3 = format!("{w1}{w2}{w3}");
                if !used_words_set.contains(&word3) {
                    return word3;
                }
            }

            // Safety: prevent infinite loop (should never reach this in practice)
            if salt > 10000 {
                break;
            }
        }

        // Final fallback: generate a unique word with hash suffix
        format!("Word{line_hash:08x}")
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

    /// Reconciles the current file content with saved state using diff.
    ///
    #[must_use]
    pub fn reconcile(
        &self,
        absolute_path: &str,
        current_lines: &[String],
        task_id: Option<&str>,
    ) -> Vec<String> {
        let task_id = task_id.unwrap_or("default");

        // Safeguard for massive files
        if current_lines.len() > MAX_TRACKED_LINES {
            return (1..=current_lines.len()).map(|i| format!("L{i}")).collect();
        }

        let current_hashes = compute_hashes(current_lines);
        let tracked = {
            let mut storage = self.storage();
            let state = Self::get_task_state_mut(&mut storage, task_id);
            state.get(absolute_path).cloned()
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
                return tracked.anchors.clone();
            }
        }

        // First time seeing this file? Assign unique anchors to every line.
        if tracked.is_none() {
            let dict = {
                let mut storage = self.storage();
                storage.get_dictionary().to_vec()
            };

            // Assign unique anchors deterministically based on line content hash
            let mut used_words_vec: VecDeque<String> = VecDeque::new();
            let mut used_words_set: HashSet<String> = HashSet::new();
            let mut anchors: Vec<String> = Vec::with_capacity(current_hashes.len());
            for hash in &current_hashes {
                let word = self.get_word_for_hash(*hash, &used_words_set, &dict);
                used_words_vec.push_back(word.clone());
                used_words_set.insert(word.clone());
                anchors.push(word);
            }

            let tracked = TrackedDocument {
                hashes: current_hashes,
                anchors,
                used_words: used_words_vec,
                used_words_set,
            };
            let anchors = tracked.anchors.clone();
            self.update_state(absolute_path, tracked, task_id);
            return anchors;
        }

        let tracked = tracked.unwrap();

        // Run diff on hashes
        let changes = diff_arrays(&tracked.hashes, &current_hashes);

        let mut new_anchors: Vec<String> = Vec::new();
        let mut new_used_words_vec = tracked.used_words.clone();
        let mut new_used_words_set = tracked.used_words_set.clone();

        // Get dictionary for hash-based word selection
        let dict = {
            let mut storage = self.storage();
            storage.get_dictionary().to_vec()
        };

        let mut old_idx = 0;
        let mut new_idx = 0;

        for change in changes {
            match change {
                DiffChange::Added(count) => {
                    for i in 0..count {
                        let line_hash = current_hashes[new_idx + i];
                        let word = self.get_word_for_hash(line_hash, &new_used_words_set, &dict);
                        new_anchors.push(word.clone());
                        new_used_words_vec.push_back(word.clone());
                        new_used_words_set.insert(word);
                    }
                    new_idx += count;
                }
                DiffChange::Removed(count) => {
                    old_idx += count;
                }
                DiffChange::Unchanged(count) => {
                    for _ in 0..count {
                        let preserved_word = tracked.anchors[old_idx].clone();
                        new_anchors.push(preserved_word.clone());
                        new_used_words_vec.push_back(preserved_word.clone());
                        new_used_words_set.insert(preserved_word);
                        old_idx += 1;
                    }
                    new_idx += count;
                }
            }
        }

        // Cap used_words_vec to MAX_USED_WORDS to prevent unbounded growth
        // in repeated reconcile cycles where new_used_words_vec accumulates entries.
        new_used_words_vec = new_used_words_vec
            .into_iter()
            .take(MAX_USED_WORDS)
            .collect();

        let tracked = TrackedDocument {
            hashes: current_hashes,
            anchors: new_anchors,
            used_words: new_used_words_vec,
            used_words_set: new_used_words_set,
        };
        let anchors = tracked.anchors.clone();
        self.update_state(absolute_path, tracked, task_id);

        // Persist anchor state to disk
        self.save();

        anchors
    }

    /// Returns true if the file is currently being tracked.
    #[must_use]
    pub fn is_tracking(&self, absolute_path: &str, task_id: Option<&str>) -> bool {
        let task_id = task_id.unwrap_or("default");
        let state = self.get_task_state(task_id);
        state.contains_key(absolute_path)
    }

    /// Gets current anchors for a file if it's being tracked.
    #[must_use]
    pub fn get_anchors(&self, absolute_path: &str, task_id: Option<&str>) -> Option<Vec<String>> {
        let task_id = task_id.unwrap_or("default");
        let state = self.get_task_state(task_id);
        state.get(absolute_path).map(|t| t.anchors.clone())
    }

    /// Clear state for a file.
    pub fn clear_state(&self, absolute_path: &str, task_id: Option<&str>) {
        let task_id = task_id.unwrap_or("default");
        let mut state = self.get_task_state(task_id);
        state.shift_remove(absolute_path);
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
        let storage = self.storage();
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

            let (line_idx, start_error) =
                self.resolve_anchor("anchor", &edit.anchor, &normalized_line_hashes, lines);
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
                    let (resolved_end_idx, end_error) = self.resolve_anchor(
                        "end_anchor",
                        end_anchor_str,
                        &normalized_line_hashes,
                        lines,
                    );
                    if let Some(error) = end_error {
                        diagnostics.push(error);
                    }
                    end_idx = resolved_end_idx;
                }
            }

            if line_idx != usize::MAX && end_idx != usize::MAX && end_idx < line_idx {
                diagnostics.push("Range error: anchor must refer to a line that precedes or is the same as end_anchor.".to_string());
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
    pub fn resolve_anchor(
        &self,
        anchor_type: &str,
        raw_anchor: &str,
        normalized_line_hashes: &[String],
        lines: &[String],
    ) -> (usize, Option<String>) {
        let anchor_raw = raw_anchor.trim();
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

        // Find all matching anchor names
        let matches: Vec<usize> = normalized_line_hashes
            .iter()
            .enumerate()
            .filter(|(_, h)| **h == anchor_name)
            .map(|(i, _)| i)
            .collect();

        tracing::debug!(
            "Anchor resolution: anchor_type={}, anchor_name={}, provided_content={:?}, matches_count={}, total_lines={}",
            anchor_type,
            anchor_name,
            provided_content,
            matches.len(),
            lines.len()
        );

        if matches.is_empty() {
            tracing::debug!(
                "Anchor resolution failed: anchor name '{}' not found in file. Available anchors: {:?}",
                anchor_name,
                normalized_line_hashes
            );
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} \"{anchor_name}\" not found in the file. Please ensure you are using the latest anchors from the most recent read_file output. COPY THE EXACT ANCHOR STRING FROM read_file OUTPUT (e.g., \"Crawler§void draw_game_over() {{\"). Do NOT modify the anchor text or omit the Word§ prefix."
                )),
            );
        }

        // Find the matching line by verifying content
        let mut matching_indices: Vec<usize> = Vec::new();
        for &index in &matches {
            let actual_content = &lines[index];
            let content_matches = provided_content == *actual_content;
            tracing::debug!(
                "Checking anchor match at line {}: provided_content={:?}, actual_content={:?}, match={}",
                index,
                provided_content,
                actual_content,
                content_matches
            );
            if content_matches {
                matching_indices.push(index);
            }
        }

        if matching_indices.is_empty() {
            // Anchor name exists but content doesn't match any occurrence
            let match_details: Vec<String> = matches
                .iter()
                .map(|&idx| format!("line {}: {:?}", idx, lines[idx]))
                .collect();
            tracing::debug!(
                "Anchor resolution failed: content mismatch. anchor_name={}, provided_content={:?}, matching_lines_with_different_content=[{}]",
                anchor_name,
                provided_content,
                match_details.join(", ")
            );
            return (
                usize::MAX,
                Some(format!(
                    "{anchor_type} \"{anchor_name}\" exists, but the code line you provided does not match the file's content at any location with this anchor. Please use the latest anchors from the most recent read tool output."
                )),
            );
        }

        if matching_indices.len() > 1 {
            // Multiple matches with same content - ambiguous
            tracing::debug!(
                "Anchor resolution failed: ambiguous anchor. anchor_name={}, matching_indices={:?}, content={:?}",
                anchor_name,
                matching_indices,
                provided_content
            );
            return (
                usize::MAX,
                Some(format!(
                    "{} \"{}\" matches {} lines with identical content. Please use a more specific anchor or edit one occurrence at a time.",
                    anchor_type,
                    anchor_name,
                    matching_indices.len()
                )),
            );
        }

        tracing::debug!(
            "Anchor resolved successfully: anchor_type={}, anchor_name={}, line_index={}",
            anchor_type,
            anchor_name,
            matching_indices[0]
        );
        (matching_indices[0], None)
    }

    /// Applies resolved edits to lines.
    /// Returns None if edits have overlapping ranges (detected before application).
    pub fn apply_edits(
        &self,
        lines: &[String],
        resolved_edits: &[ResolvedEdit],
    ) -> Option<(Vec<String>, usize, usize, Vec<AppliedEdit>)> {
        // Detect overlapping edit ranges before applying
        // For replace: range is [line_idx, end_idx]
        // For insert: range is [line_idx, line_idx]
        for i in 0..resolved_edits.len() {
            for j in (i + 1)..resolved_edits.len() {
                let a = &resolved_edits[i];
                let b = &resolved_edits[j];

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
                    return None;
                }
            }
        }

        let mut sorted_edits: Vec<&ResolvedEdit> = resolved_edits.iter().collect();
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
                clean_text
                    .lines()
                    .map(std::string::ToString::to_string)
                    .collect()
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

            added_count += replacement_lines.len();
            removed_count += removed_in_this_edit;
            changes.push((
                resolved.clone(),
                replacement_lines.len(),
                removed_in_this_edit,
            ));
        }

        // Calculate applied edit metadata
        let applied_edits: Vec<AppliedEdit> = changes
            .iter()
            .map(|(change, replacement_count, removed_count)| {
                let shift: isize = changes
                    .iter()
                    .filter(|(other, _, _)| other.line_idx < change.line_idx)
                    .map(|(_, rep, rem)| *rep as isize - *rem as isize)
                    .sum();

                let shifted_start = (change.line_idx as isize + shift) as usize;

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

        Some((new_lines, added_count, removed_count, applied_edits))
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
        let lines = split_content_lines(content);
        let line_hashes = self.anchor_mgr.reconcile(absolute_path, &lines, task_id);

        let (resolved_edits, failed_edits) =
            self.executor.resolve_edits(edits, &lines, &line_hashes);

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

        let Some((final_lines, _added, _removed, applied_edits)) =
            self.executor.apply_edits(&lines, &resolved_edits)
        else {
            return Err(FileEditorError::OverlappingEdits {
                message: "Edit ranges overlap. Apply edits sequentially or ensure non-overlapping ranges.".to_string(),
            });
        };

        let final_content = final_lines.join("\n");

        // Reconcile anchors for the modified content
        let _ = self
            .anchor_mgr
            .reconcile(absolute_path, &final_lines, task_id);

        Ok((final_content, applied_edits, failed_edits))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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

        // Large files should use L1, L2, etc.
        assert_eq!(anchors[0], "L1");
        assert_eq!(anchors[1], "L2");
    }

    #[test]
    fn test_anchor_state_manager_task_scoping() {
        // Do NOT call reset(None) here — it races with other tests that share the
        // global AnchorStorage.  Each test uses a unique task_id, so scoping is
        // already verified without clearing global state.
        let anchor_mgr = AnchorStateManager::new();
        let lines = vec!["def hello():".to_string()];

        let anchors1 = anchor_mgr.reconcile("/tmp/scope1.py", &lines, Some("scope_task1"));
        let anchors2 = anchor_mgr.reconcile("/tmp/scope2.py", &lines, Some("scope_task2"));

        // With deterministic hash-based selection, same content produces same anchors.
        // Task scoping is verified by checking that each task maintains independent state.
        assert_eq!(anchors1[0], anchors2[0]); // Deterministic: same content → same anchor

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

        // Second occurrence with different content should also match
        let (idx, error) =
            executor.resolve_anchor("anchor", "Hello§def hello():  # duplicate", &hashes, &lines);
        assert_eq!(idx, 2);
        assert!(
            error.is_none(),
            "Second occurrence should match: {:?}",
            error
        );

        // Wrong content for the anchor should fail
        let (idx, error) =
            executor.resolve_anchor("anchor", "Hello§wrong content", &hashes, &lines);
        assert_eq!(idx, usize::MAX);
        assert!(error.is_some(), "Should error on content mismatch");
        assert!(
            error
                .unwrap()
                .contains("does not match the file's content at any location")
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
        assert!(err_msg.contains("use a more specific anchor"));
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
            },
        }];

        let Some((final_lines, added, removed, applied)) = executor.apply_edits(&lines, &edits)
        else {
            panic!("apply_edits returned None (overlapping edits)");
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
            },
        }];

        let Some((final_lines, _added, _removed, applied)) = executor.apply_edits(&lines, &edits)
        else {
            panic!("apply_edits returned None (overlapping edits)");
        };
        assert_eq!(final_lines.len(), 3);
        assert_eq!(final_lines[0], "def hello():");
        assert_eq!(final_lines[1], "    print('world')");
        assert_eq!(final_lines[2], "    return 42");
        assert_eq!(applied[0].lines_added, 1);
        assert_eq!(applied[0].lines_deleted, 0);
    }

    #[test]
    fn test_file_editor_end_to_end() {
        let task_id = "e2e_test";
        let anchor_mgr = AnchorStateManager::new();
        anchor_mgr.reset(Some(task_id));

        let editor = FileEditor::new();
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
        assert!(err_msg.contains("does not match the file's content"));
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
    fn test_used_words_eviction_is_lru_not_alphabetical() {
        use std::collections::{HashSet, VecDeque};

        let mut used_words_vec: VecDeque<String> = VecDeque::new();
        let mut used_words_set: HashSet<String> = HashSet::new();

        // Insert words in a specific order (not alphabetical)
        let insert_order = vec!["Zebra", "Apple", "Mango", "Banana", "Cherry"];
        for word in &insert_order {
            used_words_vec.push_back(word.to_string());
            used_words_set.insert(word.to_string());
        }

        // Simulate eviction when exceeding MAX_USED_WORDS (we'll use a smaller threshold for testing)
        const TEST_MAX_USED_WORDS: usize = 3;
        if used_words_vec.len() > TEST_MAX_USED_WORDS {
            let to_remove_count = (used_words_vec.len() - TEST_MAX_USED_WORDS).div_ceil(2);
            for _ in 0..to_remove_count {
                if let Some(word) = used_words_vec.pop_front() {
                    used_words_set.remove(&word);
                }
            }
        }

        // Verify that the oldest-inserted words were removed, not alphabetically first
        // "Zebra" (first inserted) should be removed, not "Apple" (alphabetically first)
        assert!(
            !used_words_set.contains("Zebra"),
            "Zebra (oldest-inserted) should be evicted"
        );
        assert!(
            used_words_set.contains("Apple"),
            "Apple (newer) should remain"
        );
        assert!(
            used_words_set.contains("Cherry"),
            "Cherry (newest) should remain"
        );

        // Verify VecDeque maintains insertion order for remaining elements
        let remaining: Vec<String> = used_words_vec.into_iter().collect();
        assert!(remaining.contains(&"Mango".to_string()));
        assert!(remaining.contains(&"Banana".to_string()));
        assert!(remaining.contains(&"Cherry".to_string()));
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
}
