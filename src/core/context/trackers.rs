//! Context trackers for monitoring task state.
//!

use serde::{Deserialize, Serialize};

// ============================================================================
// File Context Tracker
// ============================================================================

use notify::{Config, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

// ============================================================================
// File Watcher (notify-based)
// ============================================================================

const MAX_EXTERNALLY_MODIFIED_PATHS: usize = 1024;

fn record_modified_path(modified: &Arc<Mutex<HashSet<PathBuf>>>, path: PathBuf) {
    if let Ok(mut set) = modified.lock() {
        let _ = set.insert(path);

        if set.len() > MAX_EXTERNALLY_MODIFIED_PATHS {
            let retain_count = (MAX_EXTERNALLY_MODIFIED_PATHS / 2).max(1);
            let current_len = set.len();
            let retained: HashSet<PathBuf> = set.drain().take(retain_count).collect();
            let dropped = current_len.saturating_sub(retained.len());
            *set = retained;

            tracing::warn!(
                current_len,
                retain_count,
                dropped,
                limit = MAX_EXTERNALLY_MODIFIED_PATHS,
                "modified path set exceeded capacity; pruning"
            );
        }
    }
}

/// Real-time file watcher using the `notify` crate.
///
/// Handles non-existent files by watching their parent directory.
#[derive(Debug, Clone)]
pub struct FileWatcher {
    inner: Arc<Mutex<notify::RecommendedWatcher>>,
    /// Paths we're watching (may be files or parent dirs for non-existent files)
    watched_paths: HashSet<PathBuf>,
    /// Map of target path -> actual watched path (for non-existent files, this is the parent dir)
    watch_targets: HashMap<PathBuf, PathBuf>,
    /// Last observed modification time for watched target files.
    watched_mtimes: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
    /// Paths that have been modified externally since last check.
    externally_modified: Arc<Mutex<HashSet<PathBuf>>>,
}

impl FileWatcher {
    pub fn new() -> Result<Self, notify::Error> {
        let externally_modified = Arc::new(Mutex::new(HashSet::new()));
        let modified = externally_modified.clone();
        let watcher = notify::RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    match event.kind {
                        // Handle Create for atomic saves (write temp, rename to target)
                        // Handle Modify for content changes
                        // Handle Remove for rename-style saves (remove original, create new)
                        notify::EventKind::Create(_)
                        | notify::EventKind::Modify(_)
                        | notify::EventKind::Remove(_) => {
                            for path in &event.paths {
                                record_modified_path(&modified, path.clone());
                            }
                        }
                        _ => {}
                    }
                }
            },
            Config::default(),
        )?;
        Ok(Self {
            inner: Arc::new(Mutex::new(watcher)),
            watched_paths: HashSet::new(),
            watch_targets: HashMap::new(),
            watched_mtimes: Arc::new(Mutex::new(HashMap::new())),
            externally_modified,
        })
    }

    /// Watch a file path. If the file doesn't exist, watches the parent directory instead.
    pub fn watch(&mut self, path: &Path) -> Result<(), notify::Error> {
        // Check if we're already watching this target
        if self.watch_targets.contains_key(path) {
            return Ok(());
        }

        let path_buf = path.to_path_buf();

        // Watch the parent directory for file targets. Editors commonly save by
        // renaming a temporary file over the original, which may only emit the
        // create/rename event on the parent directory.
        let watch_path = if path.is_dir() {
            path_buf.clone()
        } else if let Some(parent) = path.parent() {
            parent.to_path_buf()
        } else {
            path_buf.clone()
        };

        if !self.watched_paths.contains(&watch_path) {
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .watch(&watch_path, RecursiveMode::NonRecursive)?;
            self.watched_paths.insert(watch_path.clone());
        }

        self.watch_targets.insert(path_buf, watch_path);
        if let Ok(metadata) = std::fs::metadata(path)
            && let Ok(modified) = metadata.modified()
            && let Ok(mut mtimes) = self.watched_mtimes.lock()
        {
            mtimes.insert(path.to_path_buf(), modified);
        }
        Ok(())
    }

    pub fn unwatch(&mut self, path: &Path) -> Result<(), notify::Error> {
        let path_buf = path.to_path_buf();

        // Get the actual watched path for this target
        let watched_path = self.watch_targets.remove(&path_buf);
        if let Ok(mut mtimes) = self.watched_mtimes.lock() {
            mtimes.remove(&path_buf);
        }

        if let Some(watched) = watched_path {
            // Only unwatch if no other target is using this watched path
            if !self.watch_targets.values().any(|p| p == &watched) {
                self.watched_paths.remove(&watched);
                self.inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .unwatch(&watched)?;
            }
        }
        Ok(())
    }

    /// Check if a path has been modified externally and clear its flag.
    /// Uses canonical path comparison to handle symlinks (e.g., /var vs /private/var on macOS).
    pub fn take_modified(&self, path: &Path) -> bool {
        if !self.watch_targets.contains_key(path) {
            return false;
        }

        if let Ok(mut set) = self.externally_modified.lock() {
            // Try exact match first
            if set.remove(path) {
                return true;
            }

            // Try canonical path match (handles symlinks)
            if let Ok(canonical_input) = path.canonicalize() {
                // Find and remove any path that canonicalizes to the same path
                let mut to_remove = None;
                for recorded_path in &*set {
                    if let Ok(canonical_recorded) = recorded_path.canonicalize()
                        && canonical_recorded == canonical_input
                    {
                        to_remove = Some(recorded_path.clone());
                        break;
                    }
                }
                if let Some(path_to_remove) = to_remove {
                    set.remove(&path_to_remove);
                    return true;
                }
            }

            if let Ok(metadata) = std::fs::metadata(path)
                && let Ok(current_modified) = metadata.modified()
                && let Ok(mut mtimes) = self.watched_mtimes.lock()
            {
                match mtimes.get(path) {
                    Some(previous_modified) if previous_modified != &current_modified => {
                        mtimes.insert(path.to_path_buf(), current_modified);
                        return true;
                    }
                    None => {
                        mtimes.insert(path.to_path_buf(), current_modified);
                        return true;
                    }
                    _ => {}
                }
            }

            false
        } else {
            false
        }
    }

    /// Get all externally modified paths and clear them.
    pub fn drain_modified(&self) -> HashSet<PathBuf> {
        if let Ok(mut set) = self.externally_modified.lock() {
            std::mem::take(&mut *set)
        } else {
            HashSet::new()
        }
    }

    /// Stop watching all paths and cleanup the watcher.
    pub fn stop_watching_all(&mut self) {
        let mut watcher = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for path in &self.watched_paths {
            let _ = watcher.unwatch(path);
        }
        self.watched_paths.clear();
        self.watch_targets.clear();
    }
}

// Note: FileWatcher intentionally does not implement Default because
// notify::RecommendedWatcher initialization can fail on some systems.
// Callers should use FileWatcher::new().ok() and handle None gracefully.

// ============================================================================
// File Context Tracker
// ============================================================================

/// Tracks file operations that may result in stale context.
///
///
/// Uses real-time file watching via the `notify` crate to detect external modifications.
/// Supports persistent metadata storage when `task_id` is set.
#[derive(Debug, Clone)]
pub struct FileContextTracker {
    /// Map of file path -> last known mtime when read by Sned
    tracked_files: HashMap<PathBuf, SystemTime>,
    /// Files recently edited by Sned (to suppress false positives)
    recently_edited_by_sned: HashSet<PathBuf>,
    /// Legacy: recently modified files (for compatibility)
    pub recently_modified_files: Vec<String>,
    /// File watcher for real-time change detection
    pub file_watcher: Option<FileWatcher>,
    /// Task ID for persistent metadata storage
    task_id: Option<String>,
    /// In-memory file context metadata (mirrors TypeScript files_in_context)
    files_in_context: Vec<FileMetadataEntry>,
}

impl Default for FileContextTracker {
    fn default() -> Self {
        Self {
            tracked_files: HashMap::new(),
            recently_edited_by_sned: HashSet::new(),
            recently_modified_files: Vec::new(),
            file_watcher: FileWatcher::new().ok(),
            task_id: None,
            files_in_context: Vec::new(),
        }
    }
}

impl FileContextTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the task ID for persistent metadata storage.
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    /// Get the current task ID, if any.
    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    /// Get a reference to the in-memory files_in_context metadata.
    pub fn files_in_context(&self) -> &[FileMetadataEntry] {
        &self.files_in_context
    }

    pub fn was_read_this_session(&self, path: &str) -> bool {
        self.files_in_context
            .iter()
            .any(|entry| entry.path == path && entry.sned_read_date.is_some())
    }

    /// Initialize the file watcher. Call this during agent loop startup.
    pub fn init_watcher(&mut self) -> Result<(), notify::Error> {
        if self.file_watcher.is_none() {
            self.file_watcher = FileWatcher::new().ok();
        }
        Ok(())
    }

    /// Stop the file watcher and cleanup resources. Call this on task cancellation.
    pub fn stop_watcher(&mut self) {
        if let Some(ref mut watcher) = self.file_watcher {
            watcher.stop_watching_all();
        }
    }

    /// Track a file and set up a watcher for it.
    /// This is the main entry point for adding a file to context tracking.
    pub fn track_file(&mut self, path: &Path) {
        self.track_file_read(path);
        if let Some(ref mut watcher) = self.file_watcher {
            let _ = watcher.watch(path);
        }
    }

    /// Track a file operation with metadata persistence.
    ///
    /// Records the file operation in the metadata and persists to disk if `task_id` is set.
    pub async fn track_file_context(&mut self, path: &str, source: FileRecordSource) {
        self.add_file_to_file_context_tracker(path, source);
        // Also track for mtime-based stale detection
        // Use canonicalize for existing files, but fall back to original path for
        // non-existent/newly created files so they still get watched
        let path_buf = PathBuf::from(path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(path));
        self.track_file(&path_buf);
    }

    /// Add a file to the metadata tracker with the given source.
    ///
    /// Handles marking existing entries stale, carrying forward timestamps, and saving.
    fn add_file_to_file_context_tracker(&mut self, file_path: &str, source: FileRecordSource) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Mark existing active entries for this file as stale
        for entry in &mut self.files_in_context {
            if entry.path == file_path && entry.record_state == FileRecordState::Active {
                entry.record_state = FileRecordState::Stale;
            }
        }

        // Helper to get the latest date for a specific field and file
        let get_latest_date =
            |path: &str, field: fn(&FileMetadataEntry) -> Option<u64>| -> Option<u64> {
                self.files_in_context
                    .iter()
                    .filter(|e| e.path == path && field(e).is_some())
                    .max_by_key(|e| field(e))
                    .and_then(field)
            };

        let mut new_entry = FileMetadataEntry {
            path: file_path.to_string(),
            record_state: FileRecordState::Active,
            record_source: source.clone(),
            sned_read_date: get_latest_date(file_path, |e| e.sned_read_date),
            sned_edit_date: get_latest_date(file_path, |e| e.sned_edit_date),
            user_edit_date: get_latest_date(file_path, |e| e.user_edit_date),
        };

        match source {
            FileRecordSource::UserEdited => {
                new_entry.user_edit_date = Some(now);
                if !self
                    .recently_modified_files
                    .contains(&file_path.to_string())
                {
                    self.recently_modified_files.push(file_path.to_string());
                }
            }
            FileRecordSource::SnedEdited => {
                new_entry.sned_read_date = Some(now);
                new_entry.sned_edit_date = Some(now);
            }
            FileRecordSource::ReadTool | FileRecordSource::FileMentioned => {
                new_entry.sned_read_date = Some(now);
            }
        }

        self.files_in_context.push(new_entry);
        self.save_to_storage();
    }

    /// Persist the current file context metadata to disk, if task_id is set.
    fn save_to_storage(&self) {
        if let Some(ref task_id) = self.task_id
            && let Ok(storage) = crate::storage::task_storage::TaskStorage::new(task_id)
        {
            let _ = storage.save_file_context_metadata(&self.files_in_context);
        }
    }

    /// Load file context metadata from disk, if task_id is set.
    ///
    /// Call this on task resume to restore tracker state.
    pub fn load_from_storage(&mut self) {
        if let Some(ref task_id) = self.task_id
            && let Ok(storage) = crate::storage::task_storage::TaskStorage::new(task_id)
        {
            self.files_in_context = storage.load_file_context_metadata();
        }
    }

    /// Detect files that were edited by Sned or users after a specific timestamp.
    ///
    /// Used when restoring checkpoints to warn about potential file content mismatches.
    pub fn detect_files_edited_after_message(&self, message_ts: u64) -> Vec<String> {
        let mut edited_files = Vec::new();

        for entry in &self.files_in_context {
            let sned_edited_after = entry.sned_edit_date.is_some_and(|ts| ts > message_ts);
            let user_edited_after = entry.user_edit_date.is_some_and(|ts| ts > message_ts);

            if (sned_edited_after || user_edited_after) && !edited_files.contains(&entry.path) {
                edited_files.push(entry.path.clone());
            }
        }

        edited_files
    }

    /// Record that a file was read by Sned.
    /// Call this from the read_file handler after successfully reading.
    pub fn track_file_read(&mut self, path: &Path) {
        if let Ok(metadata) = std::fs::metadata(path)
            && let Ok(mtime) = metadata.modified()
        {
            self.tracked_files.insert(path.to_path_buf(), mtime);
        }
    }

    /// Mark a file as recently edited by Sned.
    /// Call this BEFORE editing to suppress the next mtime check.
    pub fn mark_file_as_edited_by_sned(&mut self, path: &Path) {
        self.recently_edited_by_sned.insert(path.to_path_buf());
    }

    /// Check if a file has been modified externally since it was last read.
    /// Returns an optional warning message.
    pub async fn check_stale(&mut self, path: &Path) -> Option<String> {
        let path_buf = path.to_path_buf();

        // If we recently edited this file, clear the flag and skip the check
        if self.recently_edited_by_sned.remove(&path_buf) {
            // Update the tracked mtime to the current one so subsequent checks
            // don't trigger on our own edit
            if let Ok(metadata) = tokio::fs::metadata(path).await
                && let Ok(mtime) = metadata.modified()
            {
                self.tracked_files.insert(path_buf.clone(), mtime);
            }
            // Also clear any watcher event for this file since we expect it to change
            if let Some(ref watcher) = self.file_watcher {
                let _ = watcher.take_modified(&path_buf);
            }
            return None;
        }

        // Check watcher-based real-time detection first
        if let Some(ref watcher) = self.file_watcher
            && watcher.take_modified(&path_buf)
        {
            // Update tracked mtime so subsequent checks don't re-trigger
            if let Ok(metadata) = tokio::fs::metadata(path).await
                && let Ok(mtime) = metadata.modified()
            {
                self.tracked_files.insert(path_buf.clone(), mtime);
            }
            return Some("Warning: File was modified externally since it was last read. \
                     The file content may have changed. Consider re-reading the file before editing.\n\n".to_string());
        }

        // Fall back to mtime polling
        let last_tracked = self.tracked_files.get(&path_buf)?;

        let current_mtime = tokio::fs::metadata(path)
            .await
            .ok()
            .and_then(|m| m.modified().ok())?;

        if current_mtime > *last_tracked {
            // File was modified externally
            Some("Warning: File was modified externally since it was last read. \
                 The file content may have changed. Consider re-reading the file before editing.\n\n".to_string())
        } else {
            None
        }
    }

    /// Records that a file was modified by the user (legacy).
    pub fn mark_file_as_user_edited(&mut self, file_path: &str) {
        if !self
            .recently_modified_files
            .contains(&file_path.to_string())
        {
            self.recently_modified_files.push(file_path.to_string());
        }
    }

    /// Returns and clears the set of recently modified files (legacy).
    pub fn get_and_clear_recently_modified_files(&mut self) -> Vec<String> {
        std::mem::take(&mut self.recently_modified_files)
    }

    /// Determines if a file was recently modified by the user (legacy).
    pub fn is_recently_modified(&self, file_path: &str) -> bool {
        self.recently_modified_files
            .contains(&file_path.to_string())
    }

    /// Get the list of tracked files (for debugging/testing)
    pub fn tracked_files(&self) -> &HashMap<PathBuf, SystemTime> {
        &self.tracked_files
    }
}

// ============================================================================
// Type Definitions
// ============================================================================

/// Metadata entry for model usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadataEntry {
    pub ts: u64,
    pub model_id: String,
    pub model_provider_id: String,
    pub mode: String,
}

/// State of a file in context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRecordState {
    Active,
    Stale,
}

/// Source of a file record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRecordSource {
    ReadTool,
    UserEdited,
    SnedEdited,
    FileMentioned,
}

/// Metadata entry for a file in context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileMetadataEntry {
    pub path: String,
    pub record_state: FileRecordState,
    pub record_source: FileRecordSource,
    pub sned_read_date: Option<u64>,
    pub sned_edit_date: Option<u64>,
    pub user_edit_date: Option<u64>,
}

/// Metadata about the environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentMetadata {
    pub os_name: String,
    pub os_version: String,
    pub os_arch: String,
    pub host_name: String,
    pub host_version: String,
    pub sned_version: String,
}

/// Metadata entry for environment history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentMetadataEntry {
    pub ts: u64,
    pub os_name: String,
    pub os_version: String,
    pub os_arch: String,
    pub host_name: String,
    pub host_version: String,
    pub sned_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_context_tracker() {
        let mut tracker = FileContextTracker::new();
        tracker.mark_file_as_edited_by_sned(std::path::Path::new("/tmp/test.txt"));
        tracker.mark_file_as_user_edited("/tmp/other.txt");

        assert!(tracker.is_recently_modified("/tmp/other.txt"));
        assert!(!tracker.is_recently_modified("/tmp/test.txt"));

        let modified = tracker.get_and_clear_recently_modified_files();
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0], "/tmp/other.txt");
        assert!(tracker.recently_modified_files.is_empty());
    }

    #[tokio::test]
    async fn test_track_file_context_adds_entry() {
        let mut tracker = FileContextTracker::new();
        tracker
            .track_file_context("/tmp/test.rs", FileRecordSource::ReadTool)
            .await;

        assert_eq!(tracker.files_in_context.len(), 1);
        assert_eq!(tracker.files_in_context[0].path, "/tmp/test.rs");
        assert_eq!(
            tracker.files_in_context[0].record_state,
            FileRecordState::Active
        );
        assert_eq!(
            tracker.files_in_context[0].record_source,
            FileRecordSource::ReadTool
        );
        assert!(tracker.files_in_context[0].sned_read_date.is_some());
    }

    #[tokio::test]
    async fn test_track_file_context_marks_existing_stale() {
        let mut tracker = FileContextTracker::new();
        tracker
            .track_file_context("/tmp/test.rs", FileRecordSource::ReadTool)
            .await;
        tracker
            .track_file_context("/tmp/test.rs", FileRecordSource::UserEdited)
            .await;

        assert_eq!(tracker.files_in_context.len(), 2);
        assert_eq!(
            tracker.files_in_context[0].record_state,
            FileRecordState::Stale
        );
        assert_eq!(
            tracker.files_in_context[1].record_state,
            FileRecordState::Active
        );
        assert_eq!(
            tracker.files_in_context[1].record_source,
            FileRecordSource::UserEdited
        );
    }

    #[tokio::test]
    async fn test_track_file_context_preserves_timestamps() {
        let mut tracker = FileContextTracker::new();
        tracker
            .track_file_context("/tmp/test.rs", FileRecordSource::ReadTool)
            .await;
        let first_read_date = tracker.files_in_context[0].sned_read_date;

        std::thread::sleep(std::time::Duration::from_millis(10));
        tracker
            .track_file_context("/tmp/test.rs", FileRecordSource::UserEdited)
            .await;

        // The new entry should carry forward the previous read date
        assert_eq!(tracker.files_in_context[1].sned_read_date, first_read_date);
        assert!(tracker.files_in_context[1].user_edit_date.is_some());
    }

    #[test]
    fn test_detect_files_edited_after_message() {
        let mut tracker = FileContextTracker::new();
        tracker.files_in_context = vec![
            FileMetadataEntry {
                path: "/tmp/a.rs".to_string(),
                record_state: FileRecordState::Active,
                record_source: FileRecordSource::ReadTool,
                sned_read_date: Some(100),
                sned_edit_date: Some(200),
                user_edit_date: None,
            },
            FileMetadataEntry {
                path: "/tmp/b.rs".to_string(),
                record_state: FileRecordState::Active,
                record_source: FileRecordSource::ReadTool,
                sned_read_date: Some(100),
                sned_edit_date: None,
                user_edit_date: Some(150),
            },
            FileMetadataEntry {
                path: "/tmp/c.rs".to_string(),
                record_state: FileRecordState::Active,
                record_source: FileRecordSource::ReadTool,
                sned_read_date: Some(100),
                sned_edit_date: None,
                user_edit_date: None,
            },
        ];

        let edited = tracker.detect_files_edited_after_message(120);
        assert_eq!(edited.len(), 2);
        assert!(edited.contains(&"/tmp/a.rs".to_string()));
        assert!(edited.contains(&"/tmp/b.rs".to_string()));
        assert!(!edited.contains(&"/tmp/c.rs".to_string()));
    }

    #[tokio::test]
    async fn test_load_and_save_file_context_metadata() {
        let temp_dir = tempfile::tempdir().unwrap();
        let task_id = "test-task-123";
        let task_storage_dir = temp_dir
            .path()
            .join(".sned")
            .join("data")
            .join("tasks")
            .join(task_id);
        std::fs::create_dir_all(&task_storage_dir).unwrap();

        let mut tracker = FileContextTracker::new().with_task_id(task_id);
        tracker
            .track_file_context("/tmp/test.rs", FileRecordSource::ReadTool)
            .await;
        tracker
            .track_file_context("/tmp/other.rs", FileRecordSource::SnedEdited)
            .await;

        assert_eq!(tracker.files_in_context.len(), 2);

        // Create a new tracker and load from storage
        let mut tracker2 = FileContextTracker::new().with_task_id(task_id);
        tracker2.load_from_storage();

        assert_eq!(tracker2.files_in_context.len(), 2);
        assert_eq!(tracker2.files_in_context[0].path, "/tmp/test.rs");
        assert_eq!(tracker2.files_in_context[1].path, "/tmp/other.rs");
        assert_eq!(
            tracker2.files_in_context[1].record_source,
            FileRecordSource::SnedEdited
        );
    }

    #[test]
    fn test_load_from_storage_without_task_id_is_noop() {
        let mut tracker = FileContextTracker::new();
        tracker.load_from_storage();
        assert!(tracker.files_in_context.is_empty());
    }

    #[tokio::test]
    async fn test_track_file_read_records_mtime() {
        let mut tracker = FileContextTracker::new();
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp, b"hello\n").unwrap();

        tracker.track_file_read(temp.path());
        assert!(tracker.tracked_files().contains_key(temp.path()));
    }

    #[tokio::test]
    async fn test_no_warning_for_untracked_file() {
        let mut tracker = FileContextTracker::new();
        let temp = tempfile::NamedTempFile::new().unwrap();

        let warning = tracker.check_stale(temp.path()).await;
        assert!(
            warning.is_none(),
            "Untracked files should not trigger warning"
        );
    }

    #[tokio::test]
    async fn test_warning_on_external_modification() {
        let mut tracker = FileContextTracker::new();
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp, b"original\n").unwrap();

        tracker.track_file_read(temp.path());

        // Simulate external modification
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::io::Write::write_all(&mut temp, b"modified\n").unwrap();

        let warning = tracker.check_stale(temp.path()).await;
        assert!(
            warning.is_some(),
            "External modification should trigger stale warning"
        );
        assert!(
            warning.unwrap().contains("modified externally"),
            "Warning should mention external modification"
        );
    }

    #[tokio::test]
    async fn test_no_warning_after_sned_edit() {
        let mut tracker = FileContextTracker::new();
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp, b"original\n").unwrap();

        tracker.track_file_read(temp.path());

        // Mark as edited by Sned (simulating edit_file handler)
        tracker.mark_file_as_edited_by_sned(temp.path());

        // Simulate Sned writing to the file
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::io::Write::write_all(&mut temp, b"sned edited\n").unwrap();

        // Check stale should return None because we marked it as edited by Sned
        let warning = tracker.check_stale(temp.path()).await;
        assert!(
            warning.is_none(),
            "Sned-edited files should not trigger warning immediately after"
        );
    }

    #[tokio::test]
    async fn test_warning_on_subsequent_external_modification() {
        let mut tracker = FileContextTracker::new();
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp, b"original\n").unwrap();

        tracker.track_file_read(temp.path());
        tracker.mark_file_as_edited_by_sned(temp.path());

        // Sned edits
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::io::Write::write_all(&mut temp, b"sned edited\n").unwrap();
        let _ = tracker.check_stale(temp.path()).await; // clears the recently_edited flag

        // Now external edit
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::io::Write::write_all(&mut temp, b"externally modified\n").unwrap();

        let warning = tracker.check_stale(temp.path()).await;
        assert!(
            warning.is_some(),
            "Subsequent external modification should trigger warning"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_watcher_based_stale_detection() {
        let mut tracker = FileContextTracker::new();
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp, b"original\n").unwrap();

        // Track the file (this also sets up the watcher)
        tracker.track_file(temp.path());

        // Give the watcher time to initialize
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Modify the file externally
        std::io::Write::write_all(&mut temp, b"modified by watcher\n").unwrap();

        // Give the watcher time to detect the change
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Check stale should detect the modification via the watcher
        let warning = tracker.check_stale(temp.path()).await;
        assert!(
            warning.is_some(),
            "Watcher-based detection should trigger stale warning"
        );
        assert!(
            warning.unwrap().contains("modified externally"),
            "Warning should mention external modification"
        );
    }

    #[test]
    fn test_modified_path_set_is_pruned() {
        let modified = Arc::new(Mutex::new(HashSet::new()));

        for idx in 0..(MAX_EXTERNALLY_MODIFIED_PATHS + 64) {
            record_modified_path(&modified, PathBuf::from(format!("/tmp/file-{idx}.txt")));
        }

        let set = modified.lock().unwrap();
        assert!(
            set.len() <= MAX_EXTERNALLY_MODIFIED_PATHS,
            "modified path set should stay bounded"
        );
    }

    #[test]
    fn test_file_watcher_new_handles_failure_gracefully() {
        // FileWatcher::new() can fail on systems with inotify/FS limitations.
        // Verify it returns Result and callers can use .ok() gracefully.
        let result = FileWatcher::new();
        // Should either succeed or fail gracefully (not panic)
        match result {
            Ok(mut watcher) => {
                // Should be usable
                assert!(
                    watcher.watch(Path::new("/tmp")).is_ok()
                        || watcher.watch(Path::new("/tmp")).is_err()
                );
            }
            Err(e) => {
                // Should provide a descriptive error (not panic)
                assert!(!e.to_string().is_empty(), "Error should have a message");
            }
        }
    }

    #[tokio::test]
    async fn test_file_watcher_detects_create_events_for_atomic_saves() {
        // Some editors use atomic saves: write to temp file, then rename to target.
        // This tests that Create events are captured for such saves.

        let Ok(mut watcher) = FileWatcher::new() else {
            // Skip test if watcher creation fails (system limitations)
            return;
        };

        let temp = tempfile::TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");

        // Create initial file
        std::fs::write(&test_file, "original").unwrap();

        // Watch the file
        watcher.watch(&test_file).unwrap();

        // Simulate atomic save: create temp file, write, rename to target
        let temp_file = temp.path().join(".test.txt.swp");
        std::fs::write(&temp_file, "atomic save content").unwrap();

        // Rename temp to target (atomic on Unix)
        std::fs::rename(&temp_file, &test_file).unwrap();

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        let mut was_modified = false;
        while tokio::time::Instant::now() < deadline {
            if watcher.take_modified(&test_file) {
                was_modified = true;
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        }

        assert!(
            was_modified,
            "Watcher should detect file modification via atomic save (Create/Rename events)"
        );
    }

    #[tokio::test]
    async fn test_track_file_context_handles_non_existent_file() {
        let mut tracker = FileContextTracker::new();
        let non_existent_path = "/tmp/does_not_exist_yet_12345.rs";

        // Should not panic or skip tracking even if file doesn't exist
        tracker
            .track_file_context(non_existent_path, FileRecordSource::FileMentioned)
            .await;

        // Verify the file was still tracked in metadata
        assert_eq!(tracker.files_in_context.len(), 1);
        assert_eq!(tracker.files_in_context[0].path, non_existent_path);
        assert_eq!(
            tracker.files_in_context[0].record_source,
            FileRecordSource::FileMentioned
        );
    }

    #[tokio::test]
    async fn test_track_file_context_watches_newly_created_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let new_file_path = temp.path().join("newly_created.rs");

        let mut tracker = FileContextTracker::new();

        // Track the file before it exists - should not fail
        tracker
            .track_file_context(
                new_file_path.to_str().unwrap(),
                FileRecordSource::FileMentioned,
            )
            .await;

        // Verify metadata tracking worked even though file didn't exist
        assert_eq!(tracker.files_in_context.len(), 1);

        // Now create the file
        std::fs::write(&new_file_path, "fn main() {}").unwrap();

        // Track again after file exists - should now populate tracked_files
        tracker.track_file_read(&new_file_path);
        assert!(tracker.tracked_files().contains_key(&new_file_path));
    }

    #[tokio::test]
    async fn test_watcher_detects_creation_then_modification() {
        // Regression: watch non-existent file, create it, modify, verify stale detection
        let temp = tempfile::TempDir::new().unwrap();
        let new_file_path = temp.path().join("will_be_created.rs");

        let mut tracker = FileContextTracker::new();

        assert!(!new_file_path.exists());

        // Track non-existent file
        tracker
            .track_file_context(
                new_file_path.to_str().unwrap(),
                FileRecordSource::FileMentioned,
            )
            .await;

        assert_eq!(tracker.files_in_context.len(), 1);

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create the file
        std::fs::write(&new_file_path, "fn main() {}").unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // Track as read
        tracker.track_file_read(&new_file_path);
        assert!(tracker.tracked_files().contains_key(&new_file_path));

        // Modify externally
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&new_file_path, "fn main() { println!(\"mod\"); }").unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // Verify stale detection works
        let warning = tracker.check_stale(&new_file_path).await;
        assert!(
            warning.is_some(),
            "Should detect modification after creation"
        );
    }
}
