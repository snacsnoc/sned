//! Checkpoint system for tracking workspace state across tool turns.
//!
//! Ports behavior from `dirac/src/integrations/checkpoints/CheckpointTracker.ts`
//! and `dirac/src/integrations/checkpoints/index.ts`.
//!
//! ## Design
//!
//! - `CheckpointTracker` manages a shadow git repository per workspace.
//! - Shadow git repos are stored in `~/.sned/checkpoints/{workspace_hash}/`.
//! - Each checkpoint is a git commit with message `checkpoint-{hash}-{task_id}`.
//! - Checkpoints are created before each tool turn that may modify files.
//! - Restore resets the working directory to a previous checkpoint state.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::warn;

/// A checkpoint is a safety feature, never a reason to leave the agent stuck
/// indefinitely behind a stalled filesystem or Git subprocess.
const CHECKPOINT_GIT_TIMEOUT: Duration = Duration::from_secs(30);
const STALE_INDEX_LOCK_AGE: Duration = Duration::from_secs(90);

/// Cross-process guard for a workspace checkpoint repository. The lock file
/// itself may remain on disk, but the advisory lock is released by the OS when
/// the owning Sned process exits or is killed.
#[derive(Debug)]
struct CheckpointRepoLock {
    _file: File,
}

impl CheckpointRepoLock {
    fn acquire(path: &Path, cancelled: Option<&AtomicBool>) -> Result<Self, CheckpointError> {
        Self::acquire_with_timeout(path, cancelled, CHECKPOINT_GIT_TIMEOUT)
    }

    fn acquire_with_timeout(
        path: &Path,
        cancelled: Option<&AtomicBool>,
        timeout: Duration,
    ) -> Result<Self, CheckpointError> {
        let parent = path
            .parent()
            .ok_or_else(|| CheckpointError::InvalidPath(path.to_path_buf()))?;
        std::fs::create_dir_all(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let started = Instant::now();
            loop {
                let result =
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if result == 0 {
                    return Ok(Self { _file: file });
                }

                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::WouldBlock {
                    return Err(CheckpointError::Io(error));
                }
                if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    return Err(CheckpointError::Cancelled);
                }
                if started.elapsed() >= timeout {
                    return Err(CheckpointError::RepositoryBusy(path.to_path_buf()));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        #[cfg(not(unix))]
        {
            let _ = cancelled;
            Ok(Self { _file: file })
        }
    }
}

/// Tracks workspace state via a shadow git repository.
#[derive(Clone)]
pub struct CheckpointTracker {
    task_id: String,
    cwd: PathBuf,
    cwd_hash: String,
    shadow_git_path: PathBuf,
}

impl CheckpointTracker {
    /// Create a new checkpoint tracker.
    ///
    /// Returns `None` if checkpoints are disabled or git is not installed.
    pub fn new(
        task_id: String,
        enable_checkpoints: bool,
        workspace_path: &str,
    ) -> Result<Option<Self>, CheckpointError> {
        if !enable_checkpoints {
            return Ok(None);
        }

        // Verify git is installed
        match Command::new("git").arg("--version").output() {
            Ok(output) if output.status.success() => {}
            _ => return Err(CheckpointError::GitNotInstalled),
        }

        let cwd = PathBuf::from(workspace_path);
        let cwd_hash = hash_working_dir(&cwd);
        let shadow_git_path = get_shadow_git_path(&cwd_hash)?;

        let tracker = Self {
            task_id,
            cwd,
            cwd_hash,
            shadow_git_path,
        };

        // Initialization mutates the shared checkpoint repo too, so it needs
        // the same cross-process guard as checkpoint commits.
        tracker.with_repo_lock(None, || tracker.init_shadow_git())?;

        Ok(Some(tracker))
    }

    /// Initialize the shadow git repository.
    fn init_shadow_git(&self) -> Result<(), CheckpointError> {
        let shadow_dir = self
            .shadow_git_path
            .parent()
            .ok_or_else(|| CheckpointError::InvalidPath(self.shadow_git_path.clone()))?;

        std::fs::create_dir_all(shadow_dir)?;

        if !self.shadow_git_path.exists() {
            // Initialize bare repo - use shadow_dir as cwd since .git doesn't exist yet
            Self::run_git_cmd(
                shadow_dir,
                &[
                    "init",
                    "--bare",
                    self.shadow_git_path.to_str().unwrap_or("."),
                ],
            )?;

            // Configure git identity - now .git exists, we can use it as cwd
            Self::run_git_cmd(
                &self.shadow_git_path,
                &["config", "user.email", "sned@checkpoint.local"],
            )?;
            Self::run_git_cmd(
                &self.shadow_git_path,
                &["config", "user.name", "Sned Checkpoint"],
            )?;
        }

        // Set worktree to the actual workspace
        Self::run_git_cmd(
            &self.shadow_git_path,
            &["config", "core.worktree", self.cwd.to_str().unwrap_or(".")],
        )?;

        Ok(())
    }

    fn repo_lock_path(&self) -> Result<PathBuf, CheckpointError> {
        let parent = self
            .shadow_git_path
            .parent()
            .ok_or_else(|| CheckpointError::InvalidPath(self.shadow_git_path.clone()))?;
        Ok(parent.join("checkpoint.lock"))
    }

    fn with_repo_lock<T>(
        &self,
        cancelled: Option<&AtomicBool>,
        operation: impl FnOnce() -> Result<T, CheckpointError>,
    ) -> Result<T, CheckpointError> {
        let lock_path = self.repo_lock_path()?;
        let _lock = CheckpointRepoLock::acquire(&lock_path, cancelled)?;
        self.remove_stale_index_lock()?;
        operation()
    }

    fn remove_stale_index_lock(&self) -> Result<(), CheckpointError> {
        let index_lock = self.shadow_git_path.join("index.lock");
        let Ok(metadata) = std::fs::metadata(&index_lock) else {
            return Ok(());
        };
        let age = metadata.modified()?.elapsed().unwrap_or_default();
        if age < STALE_INDEX_LOCK_AGE {
            return Ok(());
        }

        std::fs::remove_file(&index_lock)?;
        warn!(
            path = %index_lock.display(),
            age_secs = age.as_secs(),
            "[checkpoints] Removed stale Git index lock after acquiring checkpoint repository lock"
        );
        Ok(())
    }

    /// Create a checkpoint commit of the current workspace state.
    ///
    /// Returns the commit hash, or `None` if the commit failed.
    pub fn commit(&self) -> Result<Option<String>, CheckpointError> {
        self.commit_with_cancellation(None)
    }

    fn commit_with_cancellation(
        &self,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Option<String>, CheckpointError> {
        self.with_repo_lock(cancelled, || self.commit_locked(cancelled))
    }

    fn commit_locked(
        &self,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Option<String>, CheckpointError> {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(CheckpointError::Cancelled);
        }
        // Stage all changes (including deletions and files outside cwd)
        let add_result = Self::run_git_cmd_with_worktree_cancellable(
            &self.shadow_git_path,
            &self.cwd,
            &["add", "--all"],
            cancelled,
        );

        if let Err(e) = add_result {
            warn!("[checkpoints] Warning: failed to stage files: {}", e);
        }

        let commit_message = format!("checkpoint-{}-{}", self.cwd_hash, self.task_id);

        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(CheckpointError::Cancelled);
        }
        let commit_result = Self::run_git_cmd_with_worktree_cancellable(
            &self.shadow_git_path,
            &self.cwd,
            &[
                "commit",
                "-m",
                &commit_message,
                "--allow-empty",
                "--no-verify",
            ],
            cancelled,
        );

        if let Err(e) = commit_result {
            let err_str = e.to_string();
            // If nothing to commit, that's ok
            if err_str.contains("nothing to commit") || err_str.contains("no changes added") {
                return self.get_head_commit();
            }
            return Err(CheckpointError::CommandFailed(format!(
                "git commit failed: {err_str}"
            )));
        }

        // Get the commit hash reliably using rev-parse
        self.get_head_commit()
    }

    /// Reset the working directory to a specific checkpoint commit.
    /// Fails if there are uncommitted changes in the working tree.
    pub fn restore(&self, commit_hash: &str) -> Result<(), CheckpointError> {
        self.with_repo_lock(None, || self.restore_locked(commit_hash))
    }

    fn restore_locked(&self, commit_hash: &str) -> Result<(), CheckpointError> {
        // Check for uncommitted changes to prevent destructive reset
        let status_output = Command::new("git")
            .args([
                "--git-dir",
                self.shadow_git_path.to_str().unwrap_or("."),
                "--work-tree",
                self.cwd.to_str().unwrap_or("."),
                "status",
                "--porcelain",
            ])
            .output()
            .map_err(|e| CheckpointError::CommandFailed(format!("git status failed: {e}")))?;

        if status_output.status.success() {
            let status_text = String::from_utf8_lossy(&status_output.stdout).to_string();
            let uncommitted: Vec<&str> = status_text.lines().filter(|s| !s.is_empty()).collect();

            if !uncommitted.is_empty() {
                return Err(CheckpointError::CommandFailed(format!(
                    "Cannot restore checkpoint: {} uncommitted change(s) detected. \
                     Commit or stash changes before restoring: {}",
                    uncommitted.len(),
                    uncommitted.join(", ")
                )));
            }
        }

        Self::run_git_cmd_with_worktree(
            &self.shadow_git_path,
            &self.cwd,
            &["reset", "--hard", commit_hash],
        )?;

        Ok(())
    }

    /// Get the current HEAD commit hash.
    pub fn get_head_commit(&self) -> Result<Option<String>, CheckpointError> {
        let mut command = Command::new("git");
        command.args([
            "--git-dir",
            self.shadow_git_path.to_str().unwrap_or("."),
            "rev-parse",
            "HEAD",
        ]);
        let output = Self::run_git_command_with_timeout(&mut command)
            .map_err(|e| CheckpointError::CommandFailed(format!("git rev-parse failed: {e}")))?;

        if !output.status.success() {
            return Ok(None);
        }

        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if hash.is_empty() {
            return Ok(None);
        }

        Ok(Some(hash))
    }

    /// List all checkpoint commits (newest first).
    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>, CheckpointError> {
        let mut command = Command::new("git");
        command.args([
            "--git-dir",
            self.shadow_git_path.to_str().unwrap_or("."),
            "--work-tree",
            self.cwd.to_str().unwrap_or("."),
            "log",
            "--oneline",
            "-n",
            "50",
        ]);
        let output = Self::run_git_command_with_timeout(&mut command)
            .map_err(|e| CheckpointError::CommandFailed(format!("git log failed: {e}")))?;

        if !output.status.success() {
            return Err(CheckpointError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        let commits: Vec<CheckpointInfo> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .enumerate()
            .map(|(i, line)| {
                let parts: Vec<&str> = line.splitn(2, ' ').collect();
                let hash = parts.first().unwrap_or(&"").to_string();
                let message = parts.get(1).unwrap_or(&"").to_string();
                CheckpointInfo {
                    number: i + 1,
                    hash,
                    message,
                }
            })
            .collect();

        Ok(commits)
    }

    /// Get the list of changed files between two commits.
    ///
    /// If `rhs_hash` is `None`, compares `lhs_hash` to the working directory.
    pub fn get_changed_files(
        &self,
        lhs_hash: &str,
        rhs_hash: Option<&str>,
    ) -> Result<Vec<String>, CheckpointError> {
        let diff_range = match rhs_hash {
            Some(rhs) => format!("{lhs_hash}..{rhs}"),
            None => lhs_hash.to_string(),
        };

        let mut command = Command::new("git");
        command.args([
            "--git-dir",
            self.shadow_git_path.to_str().unwrap_or("."),
            "--work-tree",
            self.cwd.to_str().unwrap_or("."),
            "diff",
            "--name-only",
            &diff_range,
        ]);
        let output = Self::run_git_command_with_timeout(&mut command)
            .map_err(|e| CheckpointError::CommandFailed(format!("git diff failed: {e}")))?;

        if !output.status.success() {
            return Err(CheckpointError::CommandFailed(format!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(std::string::ToString::to_string)
            .filter(|s| !s.is_empty())
            .collect();

        Ok(files)
    }

    /// Run a git command and return an error if it fails.
    fn run_git_cmd(git_dir: &Path, args: &[&str]) -> Result<(), CheckpointError> {
        let mut command = Command::new("git");
        command.current_dir(git_dir).args(args.iter().copied());
        let output = Self::run_git_command_with_timeout(&mut command)
            .map_err(|e| CheckpointError::CommandFailed(format!("git command failed: {e}")))?;

        if !output.status.success() {
            return Err(CheckpointError::CommandFailed(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }

    /// Run Git with a hard wall-clock deadline. This keeps both direct and
    /// `spawn_blocking` callers bounded when Git or the workspace stalls.
    fn run_git_command_with_timeout(command: &mut Command) -> io::Result<Output> {
        Self::run_command_with_timeout_and_cancellation(command, CHECKPOINT_GIT_TIMEOUT, None)
    }

    #[cfg(test)]
    fn run_command_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
        Self::run_command_with_timeout_and_cancellation(command, timeout, None)
    }

    fn run_command_with_timeout_and_cancellation(
        command: &mut Command,
        timeout: Duration,
        cancelled: Option<&AtomicBool>,
    ) -> io::Result<Output> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let mut stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("git command stdout pipe was not captured"))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("git command stderr pipe was not captured"))?;
        // Drain both pipes while the child is running. Waiting for Git to exit
        // before reading stdout can deadlock once a large diff fills the OS
        // pipe buffer.
        let stdout_reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            stdout_pipe.read_to_end(&mut output).map(|_| output)
        });
        let stderr_reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            stderr_pipe.read_to_end(&mut output).map(|_| output)
        });
        let started = Instant::now();

        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                let _ = child.kill();
                let _ = child.wait();
                drop(child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "git checkpoint command cancelled",
                ));
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                drop(child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "git checkpoint command exceeded {} seconds",
                        timeout.as_secs_f64()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        };

        let stdout = stdout_reader
            .join()
            .map_err(|_| io::Error::other("git stdout reader panicked"))??;
        let stderr = stderr_reader
            .join()
            .map_err(|_| io::Error::other("git stderr reader panicked"))??;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Run a git command with --git-dir and --work-tree set.
    fn run_git_cmd_with_worktree(
        git_dir: &Path,
        work_tree: &Path,
        args: &[&str],
    ) -> Result<(), CheckpointError> {
        Self::run_git_cmd_with_worktree_cancellable(git_dir, work_tree, args, None)
    }

    fn run_git_cmd_with_worktree_cancellable(
        git_dir: &Path,
        work_tree: &Path,
        args: &[&str],
        cancelled: Option<&AtomicBool>,
    ) -> Result<(), CheckpointError> {
        let mut cmd_args = Vec::with_capacity(4 + args.len());
        cmd_args.push("--git-dir");
        cmd_args.push(git_dir.to_str().unwrap_or("."));
        cmd_args.push("--work-tree");
        cmd_args.push(work_tree.to_str().unwrap_or("."));
        cmd_args.extend_from_slice(args);

        let mut command = Command::new("git");
        command.current_dir(git_dir).args(cmd_args.iter().copied());
        let output = Self::run_command_with_timeout_and_cancellation(
            &mut command,
            CHECKPOINT_GIT_TIMEOUT,
            cancelled,
        )
        .map_err(|error| CheckpointError::CommandFailed(format!("git command failed: {error}")))?;
        if !output.status.success() {
            return Err(CheckpointError::CommandFailed(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }
}

/// Errors that can occur during checkpoint operations.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("Git is not installed")]
    GitNotInstalled,
    #[error("Invalid path: {0}")]
    InvalidPath(PathBuf),
    #[error("Checkpoint operation cancelled")]
    Cancelled,
    #[error("Checkpoint repository is busy: {0}")]
    RepositoryBusy(PathBuf),
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Command failed: {0}")]
    CommandFailed(String),
}

/// Hash a working directory path to a unique identifier.
fn hash_working_dir(cwd: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Namespace the hash so checkpoint locations are intentionally scoped to
    // sned's workspace hashing policy, not an implied cross-version ABI.
    const WORKING_DIR_HASH_NAMESPACE: &str = "sned::checkpoint-workspace-hash::v1";

    let mut hasher = DefaultHasher::new();
    WORKING_DIR_HASH_NAMESPACE.hash(&mut hasher);
    cwd.canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Get the shadow git repository path for a given workspace hash.
fn get_shadow_git_path(cwd_hash: &str) -> Result<PathBuf, CheckpointError> {
    let checkpoints_dir = if let Ok(base_dir) = std::env::var("SNED_CHECKPOINTS_BASE_DIR") {
        PathBuf::from(base_dir)
    } else {
        let home =
            dirs::home_dir().ok_or_else(|| CheckpointError::InvalidPath(PathBuf::from("~")))?;
        home.join(".sned").join("checkpoints")
    };

    let checkpoints_dir = checkpoints_dir.join(cwd_hash);
    Ok(checkpoints_dir.join(".git"))
}

/// High-level checkpoint manager for tasks.
pub struct TaskCheckpointManager {
    tracker: Option<CheckpointTracker>,
    checkpoint_history: Vec<String>,
    error_message: Option<String>,
}

impl TaskCheckpointManager {
    /// Create a new task checkpoint manager.
    pub fn new(task_id: String, enable_checkpoints: bool, workspace_path: &str) -> Self {
        let tracker = match CheckpointTracker::new(task_id, enable_checkpoints, workspace_path) {
            Ok(t) => t,
            Err(e) => {
                warn!("[checkpoints] Failed to initialize: {}", e);
                None
            }
        };

        Self {
            tracker,
            checkpoint_history: Vec::new(),
            error_message: None,
        }
    }

    /// Save a checkpoint of the current workspace state.
    /// Runs git commands in a blocking thread to avoid stalling the async runtime.
    pub async fn save_checkpoint(&mut self) -> Option<String> {
        self.save_checkpoint_with_cancellation(None).await
    }

    /// Save a checkpoint, terminating its Git subprocess promptly if the
    /// enclosing agent task is cancelled.
    pub async fn save_checkpoint_with_cancellation(
        &mut self,
        cancelled: Option<std::sync::Arc<AtomicBool>>,
    ) -> Option<String> {
        let Some(tracker) = &self.tracker else {
            return None;
        };
        let tracker_for_commit = tracker.clone();
        match tokio::task::spawn_blocking(move || {
            tracker_for_commit.commit_with_cancellation(cancelled.as_deref())
        })
        .await
        {
            Ok(Ok(Some(hash))) => {
                self.checkpoint_history.push(hash.clone());
                Some(hash)
            }
            Ok(Ok(None)) => {
                // No changes to commit, return the current HEAD
                tracker.get_head_commit().ok().flatten()
            }
            Ok(Err(e)) => {
                let msg = format!("Failed to save checkpoint: {e}");
                warn!("[checkpoints] {}", msg);
                self.error_message = Some(msg);
                None
            }
            Err(e) => {
                let msg = format!("Checkpoint task panicked: {e}");
                warn!("[checkpoints] {}", msg);
                self.error_message = Some(msg);
                None
            }
        }
    }

    /// Restore the workspace to a specific checkpoint.
    /// Runs git commands in a blocking thread to avoid stalling the async runtime.
    pub async fn restore_checkpoint(&self, commit_hash: &str) -> Result<(), CheckpointError> {
        let tracker = match &self.tracker {
            Some(t) => t.clone(),
            None => {
                return Err(CheckpointError::CommandFailed(
                    "No checkpoint tracker available".to_string(),
                ));
            }
        };

        let commit_hash = commit_hash.to_string();
        match tokio::task::spawn_blocking(move || tracker.restore(&commit_hash)).await {
            Ok(result) => result,
            Err(e) => Err(CheckpointError::CommandFailed(format!(
                "Checkpoint restore task panicked: {e}"
            ))),
        }
    }

    /// Get the list of changed files between two checkpoints.
    /// Runs git commands in a blocking thread to avoid stalling the async runtime.
    pub async fn get_changed_files(
        &self,
        lhs_hash: &str,
        rhs_hash: Option<&str>,
    ) -> Result<Vec<String>, CheckpointError> {
        let tracker = match &self.tracker {
            Some(t) => t.clone(),
            None => {
                return Err(CheckpointError::CommandFailed(
                    "No checkpoint tracker available".to_string(),
                ));
            }
        };

        let lhs_hash = lhs_hash.to_string();
        let rhs_hash = rhs_hash.map(std::string::ToString::to_string);
        match tokio::task::spawn_blocking(move || {
            tracker.get_changed_files(&lhs_hash, rhs_hash.as_deref())
        })
        .await
        {
            Ok(result) => result,
            Err(e) => Err(CheckpointError::CommandFailed(format!(
                "Checkpoint get_changed_files task panicked: {e}"
            ))),
        }
    }

    /// Get the last checkpoint hash, if any.
    #[must_use]
    pub fn last_checkpoint(&self) -> Option<&String> {
        self.checkpoint_history.last()
    }

    /// Get the checkpoint error message, if any.
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    /// List all checkpoint commits (newest first).
    /// Runs git commands in a blocking thread to avoid stalling the async runtime.
    pub async fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>, CheckpointError> {
        let tracker = match &self.tracker {
            Some(t) => t.clone(),
            None => {
                return Err(CheckpointError::CommandFailed(
                    "No checkpoint tracker available".to_string(),
                ));
            }
        };

        match tokio::task::spawn_blocking(move || tracker.list_checkpoints()).await {
            Ok(result) => result,
            Err(e) => Err(CheckpointError::CommandFailed(format!(
                "Checkpoint list_checkpoints task panicked: {e}"
            ))),
        }
    }

    /// Restore workspace to checkpoint by number (1 = oldest, N = newest).
    pub async fn restore_by_number(&self, number: usize) -> Result<(), CheckpointError> {
        let checkpoints = self.list_checkpoints().await?;

        if number == 0 || number > checkpoints.len() {
            return Err(CheckpointError::CommandFailed(format!(
                "Invalid checkpoint number: {}. Available: 1-{}",
                number,
                checkpoints.len()
            )));
        }

        // Convert 1-based number to index (1 = oldest = first in log)
        let checkpoint = &checkpoints[checkpoints.len() - number];
        self.restore_checkpoint(&checkpoint.hash).await
    }
}

/// Lightweight checkpoint info for CLI display.
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub number: usize,
    pub hash: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    fn ensure_test_checkpoint_base_dir() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let base_dir = PathBuf::from("/tmp/sned-checkpoints-tests");
            std::fs::create_dir_all(&base_dir).unwrap();
            // SAFETY: called once via Once; no concurrent env mutation possible
            unsafe { std::env::set_var("SNED_CHECKPOINTS_BASE_DIR", &base_dir) };
        });
    }
    use std::io::Write;

    #[test]
    fn test_hash_working_dir() {
        let path = PathBuf::from("/tmp/test_workspace");
        let hash1 = hash_working_dir(&path);
        let hash2 = hash_working_dir(&path);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 16);
    }

    #[test]
    fn test_get_shadow_git_path() {
        // Clear env var that may be set by other tests
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { std::env::remove_var("SNED_CHECKPOINTS_BASE_DIR") };

        let hash = "abc123";
        let path = get_shadow_git_path(hash).unwrap();
        assert!(path.to_string_lossy().contains(".sned/checkpoints/abc123"));
        assert_eq!(path.file_name().unwrap(), ".git");
    }

    #[test]
    fn test_checkpoint_error_display() {
        assert_eq!(
            format!("{}", CheckpointError::GitNotInstalled),
            "Git is not installed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_checkpoint_repo_lock_serializes_independent_handles() {
        let temp_dir = tempfile::tempdir().unwrap();
        let lock_path = temp_dir.path().join("checkpoint.lock");
        let _first =
            CheckpointRepoLock::acquire_with_timeout(&lock_path, None, Duration::from_millis(25))
                .expect("first checkpoint lock should succeed");

        let error =
            CheckpointRepoLock::acquire_with_timeout(&lock_path, None, Duration::from_millis(25))
                .expect_err("second independent checkpoint lock must wait");
        assert!(matches!(error, CheckpointError::RepositoryBusy(path) if path == lock_path));
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[test]
    fn test_checkpoint_command_timeout_kills_child() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 1"]);

        let error =
            CheckpointTracker::run_command_with_timeout(&mut command, Duration::from_millis(25))
                .expect_err("checkpoint command should time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[cfg(unix)]
    #[test]
    fn test_checkpoint_command_cancellation_kills_child() {
        let cancelled = AtomicBool::new(true);
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 1"]);

        let error = CheckpointTracker::run_command_with_timeout_and_cancellation(
            &mut command,
            Duration::from_secs(2),
            Some(&cancelled),
        )
        .expect_err("cancelled checkpoint command must stop");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[cfg(unix)]
    #[test]
    fn test_checkpoint_command_drains_large_output_before_waiting() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "i=0; while [ $i -lt 20000 ]; do printf 'changed-%s\\n' \"$i\"; i=$((i + 1)); done",
        ]);

        let output =
            CheckpointTracker::run_command_with_timeout(&mut command, Duration::from_secs(2))
                .expect("large Git-like output should not deadlock on the pipe buffer");
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("changed-19999"));
    }

    #[tokio::test]
    async fn test_checkpoint_save_and_restore() {
        if !git_available() {
            eprintln!("Skipping test: git not available");
            return;
        }

        ensure_test_checkpoint_base_dir();

        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let test_file = workspace.join("test.txt");
        {
            let mut file = std::fs::File::create(&test_file).unwrap();
            file.write_all(b"initial content").unwrap();
        }

        let mut manager = TaskCheckpointManager::new(
            "test-task-123".to_string(),
            true,
            workspace.to_str().unwrap(),
        );

        let checkpoint1 = manager.save_checkpoint().await;
        assert!(checkpoint1.is_some(), "First checkpoint should be created");

        {
            let mut file = std::fs::File::create(&test_file).unwrap();
            file.write_all(b"modified content").unwrap();
        }

        let checkpoint2 = manager.save_checkpoint().await;
        assert!(checkpoint2.is_some(), "Second checkpoint should be created");
        assert_ne!(
            checkpoint1, checkpoint2,
            "Checkpoints should have different hashes"
        );

        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "modified content");

        let result = manager
            .restore_checkpoint(checkpoint1.as_ref().unwrap())
            .await;
        assert!(result.is_ok(), "Restore should succeed: {:?}", result);

        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(
            content, "initial content",
            "File should be reverted to initial state"
        );

        let changed_files = manager
            .get_changed_files(checkpoint1.as_ref().unwrap(), checkpoint2.as_deref())
            .await;
        assert!(changed_files.is_ok());
        let files = changed_files.unwrap();
        assert!(
            files.contains(&"test.txt".to_string()),
            "test.txt should be in changed files"
        );
    }

    #[tokio::test]
    async fn test_changed_files_compares_checkpoint_to_working_tree() {
        if !git_available() {
            eprintln!("Skipping test: git not available");
            return;
        }

        ensure_test_checkpoint_base_dir();

        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = temp_dir.path().join("working-tree-diff");
        std::fs::create_dir_all(&workspace).unwrap();

        let test_file = workspace.join("test.txt");
        std::fs::write(&test_file, "initial content").unwrap();

        let mut manager = TaskCheckpointManager::new(
            "test-task-working-tree-diff".to_string(),
            true,
            workspace.to_str().unwrap(),
        );
        let checkpoint = manager.save_checkpoint().await.unwrap();

        std::fs::write(&test_file, "modified content").unwrap();

        let files = manager.get_changed_files(&checkpoint, None).await.unwrap();
        assert!(files.contains(&"test.txt".to_string()));
    }

    #[tokio::test]
    async fn test_disabled_checkpoints() {
        let mut manager = TaskCheckpointManager::new("test-task".to_string(), false, "/tmp");

        let checkpoint = manager.save_checkpoint().await;
        assert!(checkpoint.is_none());
    }

    #[tokio::test]
    async fn test_checkpoint_history() {
        if !git_available() {
            eprintln!("Skipping test: git not available");
            return;
        }

        ensure_test_checkpoint_base_dir();

        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = temp_dir.path().join("workspace2");
        std::fs::create_dir_all(&workspace).unwrap();

        let test_file = workspace.join("test.txt");
        {
            let mut file = std::fs::File::create(&test_file).unwrap();
            file.write_all(b"v1").unwrap();
        }

        let mut manager = TaskCheckpointManager::new(
            "test-task-456".to_string(),
            true,
            workspace.to_str().unwrap(),
        );

        assert!(manager.last_checkpoint().is_none());

        let cp1 = manager.save_checkpoint().await;
        assert!(cp1.is_some());
        assert_eq!(manager.last_checkpoint(), cp1.as_ref());

        {
            let mut file = std::fs::File::create(&test_file).unwrap();
            file.write_all(b"v2").unwrap();
        }

        let cp2 = manager.save_checkpoint().await;
        assert!(cp2.is_some());
        assert_eq!(manager.last_checkpoint(), cp2.as_ref());
        assert_eq!(manager.checkpoint_history.len(), 2);
    }
}
