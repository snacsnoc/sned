use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use crate::cli::tui::BlockKind;
use crate::core::context::context_manager::PersistedApiReqInfo;
use crate::providers::StorageMessage;
use crate::storage::disk::GlobalFileNames;

pub const DEFAULT_TRANSCRIPT_CAP: usize = 1_000;
pub const CURRENT_TRANSCRIPT_FORMAT_VERSION: u32 = 1;
const ASYNC_LOCK_RETRY_DELAYS_MS: &[u64] = &[100, 200, 400, 800, 1_600, 3_200];

fn default_transcript_format_version() -> u32 {
    CURRENT_TRANSCRIPT_FORMAT_VERSION
}

/// RAII guard that releases the task storage lock on drop.
pub struct LockGuard {
    _file: fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

/// Task metadata (mirrors TypeScript TaskMetadata)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMetadata {
    #[serde(default)]
    pub files_in_context: Vec<crate::core::context::trackers::FileMetadataEntry>,
    #[serde(default)]
    pub model_usage: Vec<ModelUsageEntry>,
    #[serde(default)]
    pub environment_history: Vec<EnvironmentMetadataEntry>,
    /// Initial task creation info (preserved from create_initial_metadata)
    #[serde(default)]
    pub initial_info: Option<TaskInitialInfo>,
    #[serde(default = "default_transcript_format_version")]
    pub transcript_format_version: u32,
    #[serde(default)]
    pub last_api_req_info: Option<PersistedApiReqInfo>,
}

impl Default for TaskMetadata {
    fn default() -> Self {
        Self {
            files_in_context: Vec::new(),
            model_usage: Vec::new(),
            environment_history: Vec::new(),
            initial_info: None,
            transcript_format_version: default_transcript_format_version(),
            last_api_req_info: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub kind: BlockKind,
    pub ts: u64,
    pub markdown: String,
}

enum TranscriptWriterCommand {
    Append(Vec<TranscriptEntry>),
    Flush(std_mpsc::Sender<io::Result<()>>),
    Shutdown(std_mpsc::Sender<io::Result<()>>),
}

/// Background writer for interactive task transcripts.
pub struct TaskTranscriptWriter {
    sender: std_mpsc::Sender<TranscriptWriterCommand>,
    errors: std_mpsc::Receiver<String>,
    handle: Option<JoinHandle<()>>,
}

impl TaskTranscriptWriter {
    pub fn start(storage: TaskStorage) -> io::Result<Self> {
        let (sender, receiver) = std_mpsc::channel();
        let (error_sender, errors) = std_mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("sned-task-transcript-writer".to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
                        TranscriptWriterCommand::Append(entries) => {
                            if let Err(error) = storage.write_transcript_entries(&entries) {
                                let _ = error_sender.send(error.to_string());
                            }
                        }
                        TranscriptWriterCommand::Flush(response) => {
                            let _ = response.send(Ok(()));
                        }
                        TranscriptWriterCommand::Shutdown(response) => {
                            let _ = response.send(Ok(()));
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            sender,
            errors,
            handle: Some(handle),
        })
    }

    pub fn append(&self, entries: Vec<TranscriptEntry>) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.sender
            .send(TranscriptWriterCommand::Append(entries))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "transcript writer stopped"))
    }

    pub fn flush(&self) -> io::Result<()> {
        let (sender, receiver) = std_mpsc::channel();
        self.sender
            .send(TranscriptWriterCommand::Flush(sender))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "transcript writer stopped"))?;
        receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "transcript writer stopped"))?
    }

    pub fn take_error(&self) -> Option<String> {
        let mut latest = None;
        while let Ok(error) = self.errors.try_recv() {
            latest = Some(error);
        }
        latest
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let (sender, receiver) = std_mpsc::channel();
        let result = match self.sender.send(TranscriptWriterCommand::Shutdown(sender)) {
            Ok(()) => receiver.recv().unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "transcript writer stopped",
                ))
            }),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "transcript writer stopped",
            )),
        };
        if handle.join().is_err() {
            return Err(io::Error::other("transcript writer panicked"));
        }
        result
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInitialInfo {
    pub created_at: i64,
    pub cwd: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageEntry {
    pub ts: i64,
    pub model_id: String,
    pub model_provider_id: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentMetadataEntry {
    pub ts: i64,
    pub os_name: String,
    pub os_version: String,
    pub os_arch: String,
    pub host_name: String,
    pub host_version: String,
    pub sned_version: String,
}

/// Manages per-task file storage at ~/.sned/data/tasks/{taskId}/
pub struct TaskStorage {
    task_dir: PathBuf,
}

impl Clone for TaskStorage {
    fn clone(&self) -> Self {
        Self {
            task_dir: self.task_dir.clone(),
        }
    }
}

impl TaskStorage {
    fn read_task_metadata_from_path(path: &Path) -> TaskMetadata {
        match fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(data) => data,
                Err(e) => {
                    // Create backup of corrupted file before discarding
                    if let Ok(backup_path) = crate::storage::disk::create_backup(path) {
                        tracing::warn!(
                            file_path = %path.display(),
                            backup_path = %backup_path.display(),
                            error = %e,
                            "Created backup of corrupted task metadata JSON"
                        );
                    } else {
                        tracing::warn!(
                            file_path = %path.display(),
                            error = %e,
                            "Failed to parse task metadata JSON and backup failed"
                        );
                    }
                    TaskMetadata::default()
                }
            },
            Err(_) => TaskMetadata::default(),
        }
    }

    #[allow(clippy::unused_self)]
    fn read_json_with_backup<T>(&self, file_path: &Path) -> Option<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        match fs::read_to_string(file_path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(data) => Some(data),
                Err(e) => {
                    // Create backup of corrupted file before discarding
                    if let Ok(backup_path) = crate::storage::disk::create_backup(file_path) {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            backup_path = %backup_path.display(),
                            error = %e,
                            "Created backup of corrupted settings JSON"
                        );
                    } else {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            error = %e,
                            "Failed to parse settings JSON and backup failed"
                        );
                    }
                    None
                }
            },
            Err(_) => None,
        }
    }

    pub fn new(task_id: &str) -> io::Result<Self> {
        let task_dir = get_tasks_dir().join(task_id);
        fs::create_dir_all(&task_dir)?;

        Ok(Self { task_dir })
    }

    pub fn new_with_dir(task_id: &str, base_dir: &Path) -> io::Result<Self> {
        let task_dir = base_dir.join("data").join("tasks").join(task_id);
        fs::create_dir_all(&task_dir)?;

        Ok(Self { task_dir })
    }

    /// Open a storage handle for an existing task directory.
    pub fn from_task_dir(task_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(task_dir)?;
        Ok(Self {
            task_dir: task_dir.to_path_buf(),
        })
    }

    #[must_use]
    pub fn task_dir(&self) -> &Path {
        &self.task_dir
    }

    /// Read API conversation history (Anthropic MessageParam format)
    pub fn read_api_conversation_history(&self) -> Vec<StorageMessage> {
        let file_path = self
            .task_dir
            .join(GlobalFileNames::API_CONVERSATION_HISTORY);
        match fs::read_to_string(&file_path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(data) => data,
                Err(e) => {
                    // Create backup of corrupted file before discarding
                    if let Ok(backup_path) = crate::storage::disk::create_backup(&file_path) {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            backup_path = %backup_path.display(),
                            error = %e,
                            "Created backup of corrupted API conversation history JSON"
                        );
                    } else {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            error = %e,
                            "Failed to parse API conversation history JSON and backup failed"
                        );
                    }
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        }
    }

    /// Write API conversation history
    pub fn write_api_conversation_history(&self, history: &[StorageMessage]) -> io::Result<()> {
        self.with_lock(|| {
            let data = serde_json::to_string(history)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let file_path = self
                .task_dir
                .join(GlobalFileNames::API_CONVERSATION_HISTORY);
            crate::storage::disk::atomic_write_file(&file_path, &data)
        })
    }

    /// Async version — avoids blocking a tokio worker on large history serialization + disk write.
    pub async fn write_api_conversation_history_async(
        &self,
        history: &[StorageMessage],
    ) -> io::Result<()> {
        let data = serde_json::to_string(history)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let file_path = self
            .task_dir
            .join(GlobalFileNames::API_CONVERSATION_HISTORY);

        let _guard = self.acquire_lock_with_retry().await?;
        crate::storage::disk::atomic_write_file_async(&file_path, &data).await
    }

    /// Read compacted summary
    pub fn read_compacted_summary(
        &self,
    ) -> Option<crate::core::context::context_manager::CompactedSummary> {
        use crate::core::context::context_manager::CompactedSummary;
        let file_path = self.task_dir.join(GlobalFileNames::COMPACTED_SUMMARY);
        match fs::read_to_string(&file_path) {
            Ok(contents) => match serde_json::from_str::<CompactedSummary>(&contents) {
                Ok(data) => Some(data),
                Err(e) => {
                    // Create backup of corrupted file before discarding
                    if let Ok(backup_path) = crate::storage::disk::create_backup(&file_path) {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            backup_path = %backup_path.display(),
                            error = %e,
                            "Created backup of corrupted compacted summary JSON"
                        );
                    } else {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            error = %e,
                            "Failed to parse compacted summary JSON and backup failed"
                        );
                    }
                    None
                }
            },
            Err(_) => None,
        }
    }

    /// Write compacted summary
    pub fn write_compacted_summary(
        &self,
        summary: &crate::core::context::context_manager::CompactedSummary,
    ) -> io::Result<()> {
        self.with_lock(|| {
            let data = serde_json::to_string(summary)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let file_path = self.task_dir.join(GlobalFileNames::COMPACTED_SUMMARY);
            crate::storage::disk::atomic_write_file(&file_path, &data)
        })
    }

    /// Async version of write_compacted_summary
    pub async fn write_compacted_summary_async(
        &self,
        summary: &crate::core::context::context_manager::CompactedSummary,
    ) -> io::Result<()> {
        let data = serde_json::to_string(summary)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let file_path = self.task_dir.join(GlobalFileNames::COMPACTED_SUMMARY);

        let _guard = self.acquire_lock_with_retry().await?;
        crate::storage::disk::atomic_write_file_async(&file_path, &data).await
    }

    /// Read context history
    pub fn read_context_history<T>(&self) -> Vec<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let file_path = self.task_dir.join(GlobalFileNames::CONTEXT_HISTORY);
        match fs::read_to_string(&file_path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(data) => data,
                Err(e) => {
                    // Create backup of corrupted file before discarding
                    if let Ok(backup_path) = crate::storage::disk::create_backup(&file_path) {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            backup_path = %backup_path.display(),
                            error = %e,
                            "Created backup of corrupted context history JSON"
                        );
                    } else {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            error = %e,
                            "Failed to parse context history JSON and backup failed"
                        );
                    }
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        }
    }

    /// Write context history
    pub fn write_context_history<T>(&self, history: &[T]) -> io::Result<()>
    where
        T: Serialize,
    {
        self.with_lock(|| {
            let data = serde_json::to_string(history)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let file_path = self.task_dir.join(GlobalFileNames::CONTEXT_HISTORY);
            crate::storage::disk::atomic_write_file(&file_path, &data)
        })
    }

    /// Read task metadata
    #[must_use]
    pub fn read_task_metadata(&self) -> TaskMetadata {
        let file_path = self.task_dir.join(GlobalFileNames::TASK_METADATA);
        Self::read_task_metadata_from_path(&file_path)
    }

    /// Write task metadata
    pub fn write_task_metadata(&self, metadata: &TaskMetadata) -> io::Result<()> {
        self.with_lock(|| self.write_task_metadata_unlocked(metadata))
    }

    /// Write task metadata without acquiring lock (caller must hold lock).
    fn write_task_metadata_unlocked(&self, metadata: &TaskMetadata) -> io::Result<()> {
        let data = serde_json::to_string(metadata)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let file_path = self.task_dir.join(GlobalFileNames::TASK_METADATA);
        crate::storage::disk::atomic_write_file(&file_path, &data)
    }

    /// Read task metadata without acquiring lock (caller must hold lock).
    fn read_task_metadata_unlocked(&self) -> TaskMetadata {
        let file_path = self.task_dir.join(GlobalFileNames::TASK_METADATA);
        Self::read_task_metadata_from_path(&file_path)
    }

    /// Create initial task metadata for a new task
    pub fn create_initial_metadata(&self, cwd: &str, model: Option<&str>) -> io::Result<()> {
        use chrono::Utc;

        self.with_lock(|| {
            // Write TaskMetadata-compatible JSON to preserve created_at/cwd/model
            let metadata = TaskMetadata {
                files_in_context: Vec::new(),
                model_usage: Vec::new(),
                environment_history: Vec::new(),
                initial_info: Some(TaskInitialInfo {
                    created_at: Utc::now().timestamp_millis(),
                    cwd: cwd.to_string(),
                    model: model.unwrap_or("default").to_string(),
                }),
                transcript_format_version: default_transcript_format_version(),
                last_api_req_info: None,
            };

            let data = serde_json::to_string(&metadata)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let file_path = self.task_dir.join(GlobalFileNames::TASK_METADATA);
            crate::storage::disk::atomic_write_file(&file_path, &data)
        })
    }

    /// Read per-task settings
    #[must_use]
    pub fn read_settings<T>(&self) -> Option<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let file_path = self.task_dir.join("settings.json");
        self.read_json_with_backup(&file_path)
    }

    /// Write per-task settings (merging with existing)
    pub fn write_settings<T>(&self, settings: &T) -> io::Result<()>
    where
        T: Serialize,
    {
        self.with_lock(|| {
            let file_path = self.task_dir.join("settings.json");
            let mut existing = serde_json::Map::new();

            // Read existing settings if they exist
            if let Ok(contents) = fs::read_to_string(&file_path) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let serde_json::Value::Object(map) = val {
                        existing = map;
                    }
                } else if let Ok(backup_path) = crate::storage::disk::create_backup(&file_path) {
                    tracing::warn!(
                        file_path = %file_path.display(),
                        backup_path = %backup_path.display(),
                        "Created backup of corrupted settings JSON before overwrite"
                    );
                }
            }

            // Merge new settings
            if let Ok(serde_json::Value::Object(new_map)) = serde_json::to_value(settings) {
                for (key, value) in new_map {
                    existing.insert(key, value);
                }
            }

            let data = serde_json::to_string(&existing)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            crate::storage::disk::atomic_write_file(&file_path, &data)
        })
    }

    /// Write conversation history as JSON for hook consumption
    pub fn write_conversation_history_json<T>(
        &self,
        history: &[T],
        timestamp: Option<i64>,
    ) -> io::Result<String>
    where
        T: Serialize,
    {
        self.with_lock(|| {
            let ts = timestamp.unwrap_or_else(|| {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64
            });
            let file_name = format!("conversation_history_{ts}.json");
            let file_path = self.task_dir.join(&file_name);

            let data = serde_json::to_string(history)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            crate::storage::disk::atomic_write_file(&file_path, &data)?;

            Ok(file_path.to_string_lossy().to_string())
        })
    }

    /// Clean up a temporary conversation history file
    pub fn cleanup_conversation_history_file(&self, file_path: &str) -> io::Result<()> {
        let path = Path::new(file_path);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Acquire an exclusive lock on the task directory.
    ///
    /// Returns a LockGuard that automatically releases the lock when dropped.
    pub fn acquire_lock(&self) -> io::Result<LockGuard> {
        let lock_path = self.task_dir.join(".lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        file.try_lock().map_err(|e| {
            let message = format!("Task is locked by another process: {e}");
            match e {
                std::fs::TryLockError::WouldBlock => {
                    io::Error::new(io::ErrorKind::WouldBlock, message)
                }
                std::fs::TryLockError::Error(_) => io::Error::other(message),
            }
        })?;

        Ok(LockGuard { _file: file })
    }

    /// Retry short-lived cross-process contention without blocking a Tokio worker.
    async fn acquire_lock_with_retry(&self) -> io::Result<LockGuard> {
        for attempt in 0..=ASYNC_LOCK_RETRY_DELAYS_MS.len() {
            match self.acquire_lock() {
                Ok(guard) => return Ok(guard),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && attempt < ASYNC_LOCK_RETRY_DELAYS_MS.len() =>
                {
                    tokio::time::sleep(Duration::from_millis(ASYNC_LOCK_RETRY_DELAYS_MS[attempt]))
                        .await;
                }
                Err(error) => return Err(error),
            }
        }

        unreachable!("lock retry loop always returns after its final attempt")
    }

    /// Acquire an exclusive lock on the task directory, blocking until available.
    ///
    /// Returns a LockGuard that automatically releases the lock when dropped.
    fn acquire_lock_blocking(&self) -> io::Result<LockGuard> {
        let lock_path = self.task_dir.join(".lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        file.lock()
            .map_err(|e| io::Error::other(format!("Failed to acquire lock: {e}")))?;

        Ok(LockGuard { _file: file })
    }

    /// Execute a closure while holding an exclusive lock on the task directory.
    ///
    /// The lock is automatically released when the closure returns.
    pub fn with_lock<T, F>(&self, f: F) -> io::Result<T>
    where
        F: FnOnce() -> io::Result<T>,
    {
        let _guard = self.acquire_lock_blocking()?;
        f()
    }
}

impl TaskStorage {
    /// Atomically update task metadata with a closure.
    ///
    /// Holds the lock across the entire read-modify-write operation to prevent
    /// concurrent updates from clobbering each other.
    pub fn update_metadata<F>(&self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut TaskMetadata),
    {
        self.with_lock(|| {
            let mut metadata = self.read_task_metadata_unlocked();
            f(&mut metadata);
            self.write_task_metadata_unlocked(&metadata)
        })
    }

    /// Save file context metadata (files_in_context) to task metadata.
    ///
    /// Uses atomic update to prevent concurrent tracker updates from clobbering each other.
    pub fn save_file_context_metadata(
        &self,
        entries: &[crate::core::context::trackers::FileMetadataEntry],
    ) -> io::Result<()> {
        self.update_metadata(|metadata| {
            metadata.files_in_context = entries.to_vec();
        })
    }

    /// Load file context metadata (files_in_context) from task metadata.
    #[must_use]
    pub fn load_file_context_metadata(
        &self,
    ) -> Vec<crate::core::context::trackers::FileMetadataEntry> {
        self.read_task_metadata().files_in_context
    }

    /// Read the newest `cap` transcript entries in chronological order.
    /// Corruption is isolated to the transcript: the original file is backed
    /// up and resume continues with no visible transcript entries. The task
    /// lock covers the read so a live reader cannot observe a partial append.
    pub fn read_transcript(&self, cap: usize) -> io::Result<Vec<TranscriptEntry>> {
        self.with_lock(|| self.read_transcript_unlocked(cap))
    }

    fn read_transcript_unlocked(&self, cap: usize) -> io::Result<Vec<TranscriptEntry>> {
        let metadata = self.read_task_metadata_unlocked();
        if metadata.transcript_format_version > CURRENT_TRANSCRIPT_FORMAT_VERSION {
            tracing::warn!(
                version = metadata.transcript_format_version,
                supported = CURRENT_TRANSCRIPT_FORMAT_VERSION,
                "Unsupported task transcript format version"
            );
            return Ok(Vec::new());
        }

        let path = self.task_dir.join(GlobalFileNames::TRANSCRIPT);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };

        let mut entries = VecDeque::with_capacity(cap.min(DEFAULT_TRANSCRIPT_CAP));
        let mut truncated = false;
        for line in io::BufReader::new(file).lines() {
            let line = line?;
            let entry = match serde_json::from_str::<TranscriptEntry>(&line) {
                Ok(entry) => entry,
                Err(error) => {
                    if let Ok(backup_path) = crate::storage::disk::create_backup(&path) {
                        tracing::warn!(
                            file_path = %path.display(),
                            backup_path = %backup_path.display(),
                            error = %error,
                            "Created backup of corrupted transcript JSONL"
                        );
                    } else {
                        tracing::warn!(
                            file_path = %path.display(),
                            error = %error,
                            "Failed to parse transcript JSONL and backup failed"
                        );
                    }
                    return Ok(Vec::new());
                }
            };

            if cap == 0 {
                continue;
            }
            if entries.len() == cap {
                entries.pop_front();
                truncated = true;
            }
            entries.push_back(entry);
        }

        if truncated {
            tracing::debug!(cap, "Transcript read retained newest entries at cap");
        }
        Ok(entries.into_iter().collect())
    }

    /// Append a visible transcript entry while holding the task lock.
    pub fn write_transcript_entry(&self, entry: &TranscriptEntry) -> io::Result<()> {
        self.write_transcript_entries(std::slice::from_ref(entry))
    }

    /// Append visible transcript entries in one locked, durable batch.
    pub fn write_transcript_entries(&self, entries: &[TranscriptEntry]) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if entries
            .iter()
            .any(|entry| matches!(entry.kind, BlockKind::Completion | BlockKind::Error))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "completion and error entries are not persisted in transcripts",
            ));
        }

        self.with_lock(|| {
            let path = self.task_dir.join(GlobalFileNames::TRANSCRIPT);
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            for entry in entries {
                let mut line = serde_json::to_vec(entry)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                line.push(b'\n');
                file.write_all(&line)?;
            }
            file.sync_data()?;
            self.compact_transcript_unlocked()
        })
    }

    fn compact_transcript_unlocked(&self) -> io::Result<()> {
        let path = self.task_dir.join(GlobalFileNames::TRANSCRIPT);
        let file = fs::File::open(&path)?;
        let mut entries = VecDeque::with_capacity(DEFAULT_TRANSCRIPT_CAP);
        for line in io::BufReader::new(file).lines() {
            let line = line?;
            let entry = match serde_json::from_str::<TranscriptEntry>(&line) {
                Ok(entry) => entry,
                Err(_) => return Ok(()),
            };
            if entries.len() == DEFAULT_TRANSCRIPT_CAP {
                entries.pop_front();
            }
            entries.push_back(entry);
        }

        let line_count = entries.len();
        let file_line_count = fs::read_to_string(&path)?.lines().count();
        if file_line_count <= DEFAULT_TRANSCRIPT_CAP {
            return Ok(());
        }

        let temp_path = path.with_extension(format!("jsonl.tmp.{}", std::process::id()));
        let result = (|| {
            let mut temp = fs::File::create(&temp_path)?;
            for entry in entries {
                serde_json::to_writer(&mut temp, &entry)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                temp.write_all(b"\n")?;
            }
            temp.sync_data()?;
            fs::rename(&temp_path, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        tracing::debug!(line_count, "Compacted task transcript");
        result
    }

    /// Delete this task's transcript when it is older than `days`.
    pub fn auto_purge(&self, days: u32) -> io::Result<()> {
        if days == 0 {
            return Ok(());
        }

        let path = self.task_dir.join(GlobalFileNames::TRANSCRIPT);
        let modified = match fs::metadata(&path) {
            Ok(metadata) => metadata.modified()?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        if age >= Duration::from_secs(u64::from(days) * 24 * 60 * 60) {
            self.with_lock(|| match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            })?;
        }
        Ok(())
    }
}

/// Get the tasks directory (~/.sned/data/tasks/)
fn get_tasks_dir() -> PathBuf {
    crate::storage::disk::get_tasks_dir()
}

/// Purge stale transcripts across all task directories without creating new
/// storage directories for inactive tasks.
pub fn purge_all_stale_transcripts(tasks_root: &Path, days: u32) -> io::Result<usize> {
    if days == 0 {
        return Ok(0);
    }

    let entries = match fs::read_dir(tasks_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut deleted = 0;
    for entry in entries.flatten() {
        let task_dir = entry.path();
        if !task_dir.is_dir() {
            continue;
        }
        let transcript = task_dir.join(GlobalFileNames::TRANSCRIPT);
        let existed = transcript.exists();
        TaskStorage { task_dir }.auto_purge(days)?;
        if existed && !transcript.exists() {
            deleted += 1;
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_task_storage_creation() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");

        // Create storage directly without going through TaskStorage::new
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        assert_eq!(storage.task_dir(), task_dir);
    }

    #[test]
    fn test_create_initial_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        let cwd = "/tmp/test";
        let model = "claude-sonnet-4-20250514";
        storage.create_initial_metadata(cwd, Some(model)).unwrap();

        let metadata_path = task_dir.join(GlobalFileNames::TASK_METADATA);
        assert!(metadata_path.exists());

        let contents = fs::read_to_string(&metadata_path).unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&contents).unwrap();

        assert_eq!(metadata["initial_info"]["cwd"], "/tmp/test");
        assert_eq!(
            metadata["initial_info"]["model"],
            "claude-sonnet-4-20250514"
        );
        assert!(metadata["initial_info"]["created_at"].is_number());
    }

    #[test]
    fn test_old_metadata_without_api_usage_still_loads() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("old-metadata");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(
            task_dir.join(GlobalFileNames::TASK_METADATA),
            r#"{"files_in_context":[],"model_usage":[],"environment_history":[]}"#,
        )
        .unwrap();

        let storage = TaskStorage { task_dir };
        let metadata = storage.read_task_metadata();
        assert!(metadata.last_api_req_info.is_none());
        assert_eq!(
            metadata.transcript_format_version,
            CURRENT_TRANSCRIPT_FORMAT_VERSION
        );
    }

    #[test]
    fn test_read_write_api_conversation_history() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        use crate::providers::{MessageContent, MessageRole};

        let history = vec![
            StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("Hello".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            },
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text("Hi".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            },
        ];

        storage.write_api_conversation_history(&history).unwrap();
        let read = storage.read_api_conversation_history();

        assert_eq!(read, history);
    }

    #[test]
    fn test_save_load_file_context_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        use crate::core::context::trackers::{
            FileMetadataEntry, FileRecordSource, FileRecordState,
        };

        let entries = vec![
            FileMetadataEntry {
                path: "/tmp/test.rs".to_string(),
                record_state: FileRecordState::Active,
                record_source: FileRecordSource::ReadTool,
                sned_read_date: Some(1000),
                sned_edit_date: None,
                user_edit_date: None,
            },
            FileMetadataEntry {
                path: "/tmp/other.rs".to_string(),
                record_state: FileRecordState::Stale,
                record_source: FileRecordSource::UserEdited,
                sned_read_date: Some(500),
                sned_edit_date: Some(1500),
                user_edit_date: Some(2000),
            },
        ];

        storage.save_file_context_metadata(&entries).unwrap();
        let loaded = storage.load_file_context_metadata();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].path, "/tmp/test.rs");
        assert_eq!(loaded[0].record_state, FileRecordState::Active);
        assert_eq!(loaded[0].record_source, FileRecordSource::ReadTool);
        assert_eq!(loaded[0].sned_read_date, Some(1000));
        assert_eq!(loaded[1].path, "/tmp/other.rs");
        assert_eq!(loaded[1].record_state, FileRecordState::Stale);
        assert_eq!(loaded[1].user_edit_date, Some(2000));
    }

    #[test]
    fn test_load_file_context_metadata_empty() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        let loaded = storage.load_file_context_metadata();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_save_file_context_metadata_overwrites() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        use crate::core::context::trackers::{
            FileMetadataEntry, FileRecordSource, FileRecordState,
        };

        let entries1 = vec![FileMetadataEntry {
            path: "/tmp/a.rs".to_string(),
            record_state: FileRecordState::Active,
            record_source: FileRecordSource::ReadTool,
            sned_read_date: Some(1000),
            sned_edit_date: None,
            user_edit_date: None,
        }];

        let entries2 = vec![FileMetadataEntry {
            path: "/tmp/b.rs".to_string(),
            record_state: FileRecordState::Active,
            record_source: FileRecordSource::SnedEdited,
            sned_read_date: Some(2000),
            sned_edit_date: Some(2000),
            user_edit_date: None,
        }];

        storage.save_file_context_metadata(&entries1).unwrap();
        storage.save_file_context_metadata(&entries2).unwrap();
        let loaded = storage.load_file_context_metadata();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, "/tmp/b.rs");
        assert_eq!(loaded[0].record_source, FileRecordSource::SnedEdited);
    }

    #[test]
    fn test_save_file_context_metadata_creates_backup_on_corrupt_task_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        let metadata_path = task_dir.join(GlobalFileNames::TASK_METADATA);
        fs::write(&metadata_path, "{not valid json").unwrap();

        use crate::core::context::trackers::{
            FileMetadataEntry, FileRecordSource, FileRecordState,
        };

        let entries = vec![FileMetadataEntry {
            path: "/tmp/task.rs".to_string(),
            record_state: FileRecordState::Active,
            record_source: FileRecordSource::ReadTool,
            sned_read_date: Some(42),
            sned_edit_date: None,
            user_edit_date: None,
        }];

        storage.save_file_context_metadata(&entries).unwrap();

        let backup = fs::read_dir(&task_dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .find(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| {
                        name.starts_with("task_metadata.json.") && name.ends_with(".bak")
                    })
            });

        assert!(
            backup.is_some(),
            "corrupt task metadata should be backed up"
        );

        let loaded = storage.load_file_context_metadata();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, "/tmp/task.rs");
    }

    #[test]
    fn test_write_settings_creates_backup_on_corrupt_settings_json() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        let settings_path = task_dir.join("settings.json");
        fs::write(&settings_path, "{not valid json").unwrap();

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct TestSettings {
            theme: String,
        }

        storage
            .write_settings(&TestSettings {
                theme: "dark".to_string(),
            })
            .unwrap();

        let backup = fs::read_dir(&task_dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .find(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| {
                        name.starts_with("settings.json.") && name.ends_with(".bak")
                    })
            });

        assert!(backup.is_some(), "corrupt settings should be backed up");

        let loaded: Option<TestSettings> = storage.read_settings();
        assert_eq!(
            loaded,
            Some(TestSettings {
                theme: "dark".to_string()
            })
        );
    }

    #[test]
    fn test_corrupt_api_conversation_history_returns_empty_with_log() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        // Write corrupt JSON
        let file_path = task_dir.join(GlobalFileNames::API_CONVERSATION_HISTORY);
        fs::write(&file_path, "this is not json {[").expect("failed to write corrupt history");

        let read = storage.read_api_conversation_history();
        assert!(read.is_empty(), "corrupt history should return empty Vec");
    }

    #[test]
    fn test_corrupt_context_history_returns_empty_with_log() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        // Write corrupt JSON
        let file_path = task_dir.join(GlobalFileNames::CONTEXT_HISTORY);
        fs::write(&file_path, "not json at all").expect("failed to write corrupt context history");

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct TestCtx {
            id: String,
        }

        let read: Vec<TestCtx> = storage.read_context_history();
        assert!(
            read.is_empty(),
            "corrupt context history should return empty Vec"
        );
    }

    #[test]
    fn test_lock_released_on_drop() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-lock-drop");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };
        let _guard = storage.acquire_lock().unwrap();

        drop(_guard);

        let storage2 = TaskStorage {
            task_dir: task_dir.clone(),
        };
        storage2.acquire_lock().unwrap();
    }

    #[test]
    fn test_concurrent_writes_blocked_by_lock() {
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-concurrent");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = Arc::new(TaskStorage {
            task_dir: task_dir.clone(),
        });

        let storage1 = Arc::clone(&storage);
        let handle1 = thread::spawn(move || {
            let _guard = storage1.acquire_lock().unwrap();
            thread::sleep(Duration::from_millis(100));
        });

        thread::sleep(Duration::from_millis(10));

        let storage2 = Arc::clone(&storage);
        let handle2 = thread::spawn(move || {
            let result = storage2.acquire_lock();
            assert!(
                result.is_err(),
                "Second lock should fail while first is held"
            );
        });

        handle1.join().unwrap();
        handle2.join().unwrap();
    }

    #[tokio::test]
    async fn test_async_history_write_retries_short_lock_contention() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::thread;

        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("async-history-lock");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage { task_dir };
        let holder_storage = storage.clone();
        let released = Arc::new(AtomicBool::new(false));
        let holder_released = Arc::clone(&released);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let holder = thread::spawn(move || {
            let _guard = holder_storage.acquire_lock().unwrap();
            ready_tx.send(()).unwrap();
            while !holder_released.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        });
        ready_rx.recv().unwrap();

        let writer_storage = storage.clone();
        let write = tokio::spawn(async move {
            writer_storage
                .write_api_conversation_history_async(&[])
                .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!write.is_finished(), "write should wait for the held lock");
        released.store(true, Ordering::Release);

        assert!(write.await.unwrap().is_ok());
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn test_async_summary_write_retries_short_lock_contention() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::thread;

        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("async-summary-lock");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage { task_dir };
        let holder_storage = storage.clone();
        let released = Arc::new(AtomicBool::new(false));
        let holder_released = Arc::clone(&released);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let holder = thread::spawn(move || {
            let _guard = holder_storage.acquire_lock().unwrap();
            ready_tx.send(()).unwrap();
            while !holder_released.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        });
        ready_rx.recv().unwrap();

        let writer_storage = storage.clone();
        let write = tokio::spawn(async move {
            writer_storage
                .write_compacted_summary_async(
                    &crate::core::context::context_manager::CompactedSummary::new(
                        "summary".to_string(),
                        1,
                    ),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!write.is_finished(), "write should wait for the held lock");
        released.store(true, Ordering::Release);

        assert!(write.await.unwrap().is_ok());
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn test_async_lock_retry_is_bounded() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::thread;

        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("async-lock-timeout");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage { task_dir };
        let holder_storage = storage.clone();
        let released = Arc::new(AtomicBool::new(false));
        let holder_released = Arc::clone(&released);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let holder = thread::spawn(move || {
            let _guard = holder_storage.acquire_lock().unwrap();
            ready_tx.send(()).unwrap();
            while !holder_released.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        });
        ready_rx.recv().unwrap();

        let result =
            tokio::time::timeout(Duration::from_secs(8), storage.acquire_lock_with_retry())
                .await
                .expect("lock retry should be bounded");
        let error = match result {
            Ok(_) => panic!("held lock should exhaust retries"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        released.store(true, Ordering::Release);
        holder.join().unwrap();
    }

    /// Regression test: concurrent metadata updates do not clobber each other.
    ///
    /// Simulates two threads updating different fields of task metadata.
    /// The lock serializes access, ensuring both updates are preserved.
    #[test]
    fn test_concurrent_metadata_updates_not_clobbered() {
        use crate::core::context::trackers::FileRecordState;
        use crate::core::context::trackers::{FileMetadataEntry, FileRecordSource};
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-concurrent-metadata");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = Arc::new(TaskStorage {
            task_dir: task_dir.clone(),
        });

        // Thread 1: Update files_in_context (holds lock briefly)
        let storage1 = Arc::clone(&storage);
        let handle1 = thread::spawn(move || {
            let entries = vec![FileMetadataEntry {
                path: "/tmp/file1.rs".to_string(),
                record_state: FileRecordState::Active,
                record_source: FileRecordSource::SnedEdited,
                sned_read_date: Some(1000),
                sned_edit_date: Some(1000),
                user_edit_date: None,
            }];
            storage1
                .update_metadata(|metadata| {
                    metadata.files_in_context = entries;
                })
                .unwrap();
        });

        // Thread 2: Update model_usage (may need to wait for lock)
        let storage2 = Arc::clone(&storage);
        let handle2 = thread::spawn(move || {
            // Small delay to increase chance of contention
            thread::sleep(Duration::from_millis(5));
            storage2
                .update_metadata(|metadata| {
                    metadata.model_usage.push(ModelUsageEntry {
                        ts: 2000,
                        model_id: "test-model".to_string(),
                        model_provider_id: "test-provider".to_string(),
                        mode: "test".to_string(),
                    });
                })
                .unwrap();
        });

        // Wait for both threads
        handle1.join().unwrap();
        handle2.join().unwrap();

        // Verify both updates were preserved (lock serialized them)
        let final_metadata = storage.read_task_metadata();
        assert_eq!(
            final_metadata.files_in_context.len(),
            1,
            "files_in_context update should be preserved"
        );
        assert_eq!(final_metadata.files_in_context[0].path, "/tmp/file1.rs");
        assert_eq!(
            final_metadata.model_usage.len(),
            1,
            "model_usage update should be preserved"
        );
        assert_eq!(final_metadata.model_usage[0].model_id, "test-model");
    }

    /// Regression test: rapid successive updates to same field do not lose data.
    #[test]
    fn test_rapid_successive_metadata_updates() {
        use crate::core::context::trackers::FileRecordState;
        use crate::core::context::trackers::{FileMetadataEntry, FileRecordSource};

        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-rapid-updates");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        // Perform 10 rapid successive updates to files_in_context
        for i in 0..10 {
            let entries = vec![FileMetadataEntry {
                path: format!("/tmp/file{}.rs", i),
                record_state: FileRecordState::Active,
                record_source: FileRecordSource::SnedEdited,
                sned_read_date: Some(i * 100),
                sned_edit_date: Some(i * 100),
                user_edit_date: None,
            }];
            storage.save_file_context_metadata(&entries).unwrap();
        }

        // Verify the last update is present
        let final_metadata = storage.read_task_metadata();
        assert_eq!(final_metadata.files_in_context.len(), 1);
        assert_eq!(final_metadata.files_in_context[0].path, "/tmp/file9.rs");
    }

    #[test]
    fn test_read_api_conversation_history_creates_backup_on_corruption() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("test-task");
        fs::create_dir_all(&task_dir).unwrap();

        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };

        // Write corrupted JSON to api_conversation_history.json
        let history_path = storage
            .task_dir
            .join(GlobalFileNames::API_CONVERSATION_HISTORY);

        let corrupted_content = r#"[{"role": "user", "corrupted": json"#;
        fs::write(&history_path, corrupted_content).unwrap();

        // Read API conversation history - should create backup and return empty
        let result = storage.read_api_conversation_history();
        assert!(
            result.is_empty(),
            "Should return empty vec with corrupted file"
        );

        // Verify backup was created
        let backup_path = history_path.with_extension("json.bak");
        assert!(
            backup_path.exists(),
            "Backup file should be created for corrupted JSON"
        );

        // Verify backup contains original corrupted content
        let backup_content = fs::read_to_string(&backup_path).unwrap();
        assert_eq!(
            backup_content, corrupted_content,
            "Backup should contain original corrupted content"
        );
    }

    #[test]
    fn test_transcript_round_trip_preserves_kind_and_markdown() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("transcript-round-trip");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage { task_dir };
        let entries = vec![
            TranscriptEntry {
                kind: BlockKind::Model,
                ts: 1,
                markdown: "model".to_string(),
            },
            TranscriptEntry {
                kind: BlockKind::ToolHeader,
                ts: 2,
                markdown: "▶ read_file".to_string(),
            },
            TranscriptEntry {
                kind: BlockKind::ToolOutput,
                ts: 3,
                markdown: "file contents".to_string(),
            },
        ];

        for entry in &entries {
            storage.write_transcript_entry(entry).unwrap();
        }

        assert_eq!(
            storage.read_transcript(DEFAULT_TRANSCRIPT_CAP).unwrap(),
            entries
        );
    }

    #[test]
    fn test_transcript_cap_keeps_newest_entries_in_order() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("transcript-cap");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage { task_dir };
        for index in 0..5 {
            storage
                .write_transcript_entry(&TranscriptEntry {
                    kind: BlockKind::Model,
                    ts: index,
                    markdown: format!("line {index}"),
                })
                .unwrap();
        }

        let entries = storage.read_transcript(3).unwrap();
        assert_eq!(
            entries.iter().map(|entry| entry.ts).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn test_transcript_batch_compacts_file_to_retention_cap() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("transcript-batch-cap");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };
        let entries = (0..DEFAULT_TRANSCRIPT_CAP + 5)
            .map(|index| TranscriptEntry {
                kind: BlockKind::Model,
                ts: index as u64,
                markdown: format!("line {index}"),
            })
            .collect::<Vec<_>>();

        storage.write_transcript_entries(&entries).unwrap();

        let on_disk_lines = fs::read_to_string(task_dir.join(GlobalFileNames::TRANSCRIPT))
            .unwrap()
            .lines()
            .count();
        assert_eq!(on_disk_lines, DEFAULT_TRANSCRIPT_CAP);
        let retained = storage.read_transcript(DEFAULT_TRANSCRIPT_CAP).unwrap();
        assert_eq!(retained.first().unwrap().ts, 5);
        assert_eq!(retained.last().unwrap().ts, 1004);
    }

    #[test]
    fn test_corrupt_transcript_returns_empty_and_creates_backup() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("transcript-corrupt");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };
        let path = task_dir.join(GlobalFileNames::TRANSCRIPT);
        fs::write(&path, "not json\n").unwrap();

        assert!(
            storage
                .read_transcript(DEFAULT_TRANSCRIPT_CAP)
                .unwrap()
                .is_empty()
        );
        assert!(path.with_extension("jsonl.bak").exists());
    }

    #[test]
    fn test_transcript_excludes_completion_and_error_entries() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("transcript-filter");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage { task_dir };
        for kind in [BlockKind::Completion, BlockKind::Error] {
            assert!(
                storage
                    .write_transcript_entry(&TranscriptEntry {
                        kind,
                        ts: 1,
                        markdown: "transient".to_string(),
                    })
                    .is_err()
            );
        }
        assert!(
            storage
                .read_transcript(DEFAULT_TRANSCRIPT_CAP)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_transcript_future_format_is_not_replayed() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("transcript-future-format");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };
        storage
            .write_transcript_entry(&TranscriptEntry {
                kind: BlockKind::Model,
                ts: 1,
                markdown: "future".to_string(),
            })
            .unwrap();
        storage
            .write_task_metadata(&TaskMetadata {
                transcript_format_version: CURRENT_TRANSCRIPT_FORMAT_VERSION + 1,
                ..TaskMetadata::default()
            })
            .unwrap();

        assert!(
            storage
                .read_transcript(DEFAULT_TRANSCRIPT_CAP)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_auto_purge_removes_old_transcript_but_zero_is_disabled() {
        let temp_dir = TempDir::new().unwrap();
        let task_dir = temp_dir.path().join("transcript-purge");
        fs::create_dir_all(&task_dir).unwrap();
        let storage = TaskStorage {
            task_dir: task_dir.clone(),
        };
        storage
            .write_transcript_entry(&TranscriptEntry {
                kind: BlockKind::Model,
                ts: 1,
                markdown: "old".to_string(),
            })
            .unwrap();
        let path = task_dir.join(GlobalFileNames::TRANSCRIPT);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(
            fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60)),
        )
        .unwrap();

        storage.auto_purge(0).unwrap();
        assert!(path.exists());
        storage.auto_purge(1).unwrap();
        assert!(!path.exists());
    }
}
