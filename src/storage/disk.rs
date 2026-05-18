use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use std::env;
use tokio::fs as tokio_fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

static ATOMIC_WRITES: Lazy<AtomicWriteTracker> = Lazy::new(AtomicWriteTracker::default);

#[derive(Default)]
struct AtomicWriteTracker {
    active: AtomicUsize,
    notify: Notify,
}

struct AtomicWriteGuard<'a> {
    tracker: &'a AtomicWriteTracker,
}

impl<'a> AtomicWriteGuard<'a> {
    fn begin(tracker: &'a AtomicWriteTracker) -> Self {
        tracker.active.fetch_add(1, Ordering::SeqCst);
        Self { tracker }
    }
}

impl Drop for AtomicWriteGuard<'_> {
    fn drop(&mut self) {
        self.tracker.active.fetch_sub(1, Ordering::SeqCst);
        self.tracker.notify.notify_waiters();
    }
}

pub fn active_atomic_write_count() -> usize {
    active_atomic_write_count_on(&ATOMIC_WRITES)
}

fn active_atomic_write_count_on(tracker: &AtomicWriteTracker) -> usize {
    tracker.active.load(Ordering::SeqCst)
}

async fn wait_for_atomic_writes_on(tracker: &AtomicWriteTracker, timeout: Duration) -> bool {
    let drain = async {
        loop {
            let notified = tracker.notify.notified();
            if active_atomic_write_count_on(tracker) == 0 {
                return;
            }
            notified.await;
        }
    };

    tokio::time::timeout(timeout, drain).await.is_ok()
}

pub async fn wait_for_atomic_writes(timeout: Duration) -> bool {
    wait_for_atomic_writes_on(&ATOMIC_WRITES, timeout).await
}

/// Get the base Sned directory (~/.sned or SNED_DIR)
pub fn get_sned_dir() -> PathBuf {
    if let Ok(dir) = env::var("SNED_DIR") {
        PathBuf::from(dir)
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".sned")
    }
}

/// Get the Sned data directory (~/.sned/data or SNED_DATA_DIR)
pub fn get_data_dir() -> PathBuf {
    if let Ok(dir) = env::var("SNED_DATA_DIR") {
        PathBuf::from(dir)
    } else {
        get_sned_dir().join("data")
    }
}

/// Get the tasks directory (data/tasks)
pub fn get_tasks_dir() -> PathBuf {
    get_data_dir().join("tasks")
}

/// Get the state directory (data/state)
pub fn get_state_dir() -> PathBuf {
    get_data_dir().join("state")
}

/// Get the settings directory (data/settings)
pub fn get_settings_dir() -> PathBuf {
    get_data_dir().join("settings")
}

/// Atomically write data to a file using temp file + rename pattern.
///
/// Uses the POSIX atomic rename semantics: if rename() succeeds, the file is
/// guaranteed to exist with the new content. If rename() fails, the original
/// file is untouched and the temp file is cleaned up.
///
/// # Errors
/// Returns error if:
/// - Cannot write temp file
/// - Rename fails (including cross-filesystem moves)
/// - Temp file cleanup fails (after rename failure)
///
/// Note: Does NOT fall back to non-atomic write on rename failure, as this
/// could cause data corruption if the process crashes mid-write.
pub fn atomic_write_file<P: AsRef<Path>>(file_path: P, data: &str) -> io::Result<()> {
    let _write_guard = AtomicWriteGuard::begin(&ATOMIC_WRITES);
    let file_path = file_path.as_ref();
    let parent = file_path.parent().unwrap_or_else(|| Path::new(""));

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    let rand_str: String = std::iter::repeat_with(fastrand::alphanumeric)
        .take(7)
        .collect();

    let file_name = file_path.file_name().unwrap_or_default().to_string_lossy();
    let tmp_name = format!("{}.tmp.{}.{}.json", file_name, now, rand_str);
    let tmp_path = parent.join(tmp_name);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Create temp file with restrictive permissions (owner read/write only)
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)?;
        file.write_all(data.as_bytes())?;
        // Sync data to disk before rename to ensure durability across crashes
        file.sync_data()?;
    }

    #[cfg(windows)]
    {
        // Windows: write file normally. The file inherits parent directory ACLs.
        // For sensitive data, ensure parent directory has restrictive ACLs.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(data.as_bytes())?;
        file.sync_data()?;
    }

    #[cfg(not(any(unix, windows)))]
    {
        // Other platforms: basic write
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(data.as_bytes())?;
        file.sync_data()?;
    }

    match fs::rename(&tmp_path, file_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Clean up temp file and propagate error
            // Do NOT fall back to non-atomic write - that risks data corruption
            let _ = fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// Async version of atomic_write_file using tokio.
///
/// Uses the POSIX atomic rename semantics: if rename() succeeds, the file is
/// guaranteed to exist with the new content. If rename() fails, the original
/// file is untouched and the temp file is cleaned up.
///
/// # Errors
/// Returns error if:
/// - Cannot write temp file
/// - Rename fails (including cross-filesystem moves)
/// - Temp file cleanup fails (after rename failure)
///
/// Note: Does NOT fall back to non-atomic write on rename failure, as this
/// could cause data corruption if the process crashes mid-write.
pub async fn atomic_write_file_async<P: AsRef<Path>>(file_path: P, data: &str) -> io::Result<()> {
    let _write_guard = AtomicWriteGuard::begin(&ATOMIC_WRITES);
    let file_path = file_path.as_ref();
    let parent = file_path.parent().unwrap_or_else(|| Path::new(""));

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    let rand_str: String = std::iter::repeat_with(fastrand::alphanumeric)
        .take(7)
        .collect();

    let file_name = file_path.file_name().unwrap_or_default().to_string_lossy();
    let tmp_name = format!("{}.tmp.{}.{}.json", file_name, now, rand_str);
    let tmp_path = parent.join(tmp_name);

    #[cfg(unix)]
    {
        // Create temp file with restrictive permissions (owner read/write only)
        let mut file = tokio_fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
            .await?;
        file.write_all(data.as_bytes()).await?;
        // Sync data to disk before rename to ensure durability across crashes
        file.sync_all().await?;
    }

    #[cfg(windows)]
    {
        // Windows: write file normally. The file inherits parent directory ACLs.
        // For sensitive data, ensure parent directory has restrictive ACLs.
        let mut file = tokio_fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await?;
        file.write_all(data.as_bytes()).await?;
        file.sync_all().await?;
    }

    #[cfg(not(any(unix, windows)))]
    {
        let mut file = tokio_fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await?;
        file.write_all(data.as_bytes()).await?;
        file.sync_all().await?;
    }

    match tokio_fs::rename(&tmp_path, file_path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Clean up temp file and propagate error
            // Do NOT fall back to non-atomic write - that risks data corruption
            let _ = tokio_fs::remove_file(&tmp_path).await;
            Err(e)
        }
    }
}

/// Clean up orphaned atomic write temp files older than a threshold.
///
/// This prevents disk space exhaustion from repeated crashes during
/// atomic writes, which leave behind `.tmp.*.json` files.
///
/// # Arguments
///
/// * `dir` - Directory to scan for temp files
/// * `max_age` - Maximum age of temp files to keep (older ones are deleted)
///
/// Returns the count of files deleted.
pub fn cleanup_orphaned_temp_files(dir: &Path, max_age: Duration) -> io::Result<usize> {
    let mut deleted = 0usize;
    let now = SystemTime::now();

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };

        // Match temp file pattern: *.tmp.*.json
        if !file_name.contains(".tmp.") || !file_name.ends_with(".json") {
            continue;
        }

        // Check file age
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let modified = match metadata.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if now.duration_since(modified).unwrap_or(Duration::ZERO) > max_age
            && fs::remove_file(&path).is_ok()
        {
            deleted += 1;
        }
    }

    Ok(deleted)
}

pub struct GlobalFileNames;

impl GlobalFileNames {
    pub const API_CONVERSATION_HISTORY: &'static str = "api_conversation_history.json";
    pub const CONTEXT_HISTORY: &'static str = "context_history.json";
    pub const UI_MESSAGES: &'static str = "ui_messages.json";
    pub const COMPACTED_SUMMARY: &'static str = "compacted_summary.json";
    pub const SNED_RECOMMENDED_MODELS: &'static str = "sned_recommended_models.json";
    pub const SNED_MODELS: &'static str = "sned_models.json";
    pub const OPENROUTER_MODELS: &'static str = "openrouter_models.json";
    pub const VERCEL_AI_GATEWAY_MODELS: &'static str = "vercel_ai_gateway_models.json";
    pub const GROQ_MODELS: &'static str = "groq_models.json";
    pub const BASETEN_MODELS: &'static str = "baseten_models.json";
    pub const SNED_RULES: &'static str = ".snedrules";
    pub const WORKFLOWS: &'static str = ".snedrules/workflows";
    pub const HOOKS_DIR: &'static str = ".snedrules/hooks";
    pub const SNEDRULE_SKILLS_DIR: &'static str = ".snedrules/skills";
    pub const SNED_SKILLS_DIR: &'static str = ".sned/skills";
    pub const CLAUDE_SKILLS_DIR: &'static str = ".claude/skills";
    pub const AGENTS_SKILLS_DIR: &'static str = ".agents/skills";
    pub const AI_SKILLS_DIR: &'static str = ".ai/skills";
    pub const CURSOR_RULES_DIR: &'static str = ".cursor/rules";
    pub const CURSOR_RULES_FILE: &'static str = ".cursorrules";
    pub const WINDSURF_RULES: &'static str = ".windsurfrules";
    pub const AGENTS_RULES_FILE: &'static str = "AGENTS.md";
    pub const TASK_METADATA: &'static str = "task_metadata.json";
    pub const ENDPOINTS_JSON: &'static str = "endpoints.json";

    pub fn remote_config(org_id: &str) -> String {
        format!("remote_config_{}.json", org_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_wait_for_atomic_writes_returns_when_idle() {
        let tracker = AtomicWriteTracker::default();
        assert!(wait_for_atomic_writes_on(&tracker, Duration::from_millis(10)).await);
    }

    #[tokio::test]
    async fn test_wait_for_atomic_writes_waits_for_active_guard() {
        let tracker = AtomicWriteTracker::default();
        let guard = AtomicWriteGuard::begin(&tracker);
        let waiter = wait_for_atomic_writes_on(&tracker, Duration::from_secs(1));

        tokio::task::yield_now().await;
        assert_eq!(active_atomic_write_count_on(&tracker), 1);
        drop(guard);

        assert!(waiter.await);
        assert_eq!(active_atomic_write_count_on(&tracker), 0);
    }
}
