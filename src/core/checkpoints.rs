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

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

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

        // Initialize shadow git if needed
        tracker.init_shadow_git()?;

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

    /// Create a checkpoint commit of the current workspace state.
    ///
    /// Returns the commit hash, or `None` if the commit failed.
    pub fn commit(&self) -> Result<Option<String>, CheckpointError> {
        // Stage all changes (including deletions and files outside cwd)
        let add_result =
            Self::run_git_cmd_with_worktree(&self.shadow_git_path, &self.cwd, &["add", "--all"]);

        if let Err(e) = add_result {
            eprintln!("[checkpoints] Warning: failed to stage files: {}", e);
        }

        let commit_message = format!("checkpoint-{}-{}", self.cwd_hash, self.task_id);

        let commit_result = Self::run_git_cmd_with_worktree(
            &self.shadow_git_path,
            &self.cwd,
            &[
                "commit",
                "-m",
                &commit_message,
                "--allow-empty",
                "--no-verify",
            ],
        );

        if let Err(e) = commit_result {
            let err_str = e.to_string();
            // If nothing to commit, that's ok
            if err_str.contains("nothing to commit") || err_str.contains("no changes added") {
                return self.get_head_commit();
            }
            return Err(CheckpointError::CommandFailed(format!(
                "git commit failed: {}",
                err_str
            )));
        }

        // Get the commit hash reliably using rev-parse
        self.get_head_commit()
    }

    /// Reset the working directory to a specific checkpoint commit.
    /// Fails if there are uncommitted changes in the working tree.
    pub fn restore(&self, commit_hash: &str) -> Result<(), CheckpointError> {
        // Check for uncommitted changes to prevent destructive reset
        let status_output = Self::run_git_cmd_with_worktree(
            &self.shadow_git_path,
            &self.cwd,
            &["status", "--porcelain"],
        );

        if let Err(e) = status_output {
            return Err(CheckpointError::CommandFailed(format!(
                "git status failed: {}",
                e
            )));
        }

        // status_output returns Ok(()) on success, but we need the actual output
        // Re-run to get the output for checking uncommitted changes
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
            .map_err(|e| CheckpointError::CommandFailed(format!("git status failed: {}", e)))?;

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
        let output = Command::new("git")
            .args([
                "--git-dir",
                self.shadow_git_path.to_str().unwrap_or("."),
                "rev-parse",
                "HEAD",
            ])
            .output()
            .map_err(|e| CheckpointError::CommandFailed(format!("git rev-parse failed: {}", e)))?;

        if !output.status.success() {
            return Ok(None);
        }

        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if hash.is_empty() {
            return Ok(None);
        }

        Ok(Some(hash))
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
            Some(rhs) => format!("{}..{}", lhs_hash, rhs),
            None => lhs_hash.to_string(),
        };

        let output = Command::new("git")
            .args([
                "--git-dir",
                self.shadow_git_path.to_str().unwrap_or("."),
                "--work-tree",
                self.cwd.to_str().unwrap_or("."),
                "diff",
                "--name-only",
                &diff_range,
            ])
            .output()
            .map_err(|e| CheckpointError::CommandFailed(format!("git diff failed: {}", e)))?;

        if !output.status.success() {
            return Err(CheckpointError::CommandFailed(format!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(files)
    }

    /// Run a git command and return an error if it fails.
    fn run_git_cmd(git_dir: &Path, args: &[&str]) -> Result<(), CheckpointError> {
        let output = Command::new("git")
            .current_dir(git_dir)
            .args(args.iter().copied())
            .output()
            .map_err(|e| CheckpointError::CommandFailed(format!("git command failed: {}", e)))?;

        if !output.status.success() {
            return Err(CheckpointError::CommandFailed(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }

    /// Run a git command with --git-dir and --work-tree set.
    fn run_git_cmd_with_worktree(
        git_dir: &Path,
        work_tree: &Path,
        args: &[&str],
    ) -> Result<(), CheckpointError> {
        let mut cmd_args = Vec::with_capacity(4 + args.len());
        cmd_args.push("--git-dir");
        cmd_args.push(git_dir.to_str().unwrap_or("."));
        cmd_args.push("--work-tree");
        cmd_args.push(work_tree.to_str().unwrap_or("."));
        cmd_args.extend_from_slice(args);

        Self::run_git_cmd(git_dir, &cmd_args)
    }
}

/// Errors that can occur during checkpoint operations.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("Git is not installed")]
    GitNotInstalled,
    #[error("Invalid path: {0}")]
    InvalidPath(PathBuf),
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
                eprintln!("[checkpoints] Failed to initialize: {}", e);
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
        let tracker = match &self.tracker {
            Some(t) => t,
            None => return None,
        };

        let tracker_for_commit = tracker.clone();
        match tokio::task::spawn_blocking(move || tracker_for_commit.commit()).await {
            Ok(Ok(Some(hash))) => {
                self.checkpoint_history.push(hash.clone());
                Some(hash)
            }
            Ok(Ok(None)) => {
                // No changes to commit, return the current HEAD
                tracker.get_head_commit().ok().flatten()
            }
            Ok(Err(e)) => {
                let msg = format!("Failed to save checkpoint: {}", e);
                eprintln!("[checkpoints] {}", msg);
                self.error_message = Some(msg);
                None
            }
            Err(e) => {
                let msg = format!("Checkpoint task panicked: {}", e);
                eprintln!("[checkpoints] {}", msg);
                self.error_message = Some(msg);
                None
            }
        }
    }

    /// Restore the workspace to a specific checkpoint.
    pub fn restore_checkpoint(&self, commit_hash: &str) -> Result<(), CheckpointError> {
        match &self.tracker {
            Some(tracker) => tracker.restore(commit_hash),
            None => Err(CheckpointError::CommandFailed(
                "No checkpoint tracker available".to_string(),
            )),
        }
    }

    /// Get the list of changed files between two checkpoints.
    pub fn get_changed_files(
        &self,
        lhs_hash: &str,
        rhs_hash: Option<&str>,
    ) -> Result<Vec<String>, CheckpointError> {
        match &self.tracker {
            Some(tracker) => tracker.get_changed_files(lhs_hash, rhs_hash),
            None => Err(CheckpointError::CommandFailed(
                "No checkpoint tracker available".to_string(),
            )),
        }
    }

    /// Get the last checkpoint hash, if any.
    pub fn last_checkpoint(&self) -> Option<&String> {
        self.checkpoint_history.last()
    }

    /// Get the checkpoint error message, if any.
    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    /// List all checkpoint commits (newest first).
    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>, CheckpointError> {
        let tracker = match &self.tracker {
            Some(t) => t,
            None => {
                return Err(CheckpointError::CommandFailed(
                    "No checkpoint tracker available".to_string(),
                ));
            }
        };

        let output = Command::new("git")
            .args([
                "--git-dir",
                tracker.shadow_git_path.to_str().unwrap_or("."),
                "--work-tree",
                tracker.cwd.to_str().unwrap_or("."),
                "log",
                "--oneline",
                "-n",
                "50",
            ])
            .output()
            .map_err(|e| CheckpointError::CommandFailed(format!("git log failed: {}", e)))?;

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

    /// Restore workspace to checkpoint by number (1 = oldest, N = newest).
    pub fn restore_by_number(&self, number: usize) -> Result<(), CheckpointError> {
        let checkpoints = self.list_checkpoints()?;

        if number == 0 || number > checkpoints.len() {
            return Err(CheckpointError::CommandFailed(format!(
                "Invalid checkpoint number: {}. Available: 1-{}",
                number,
                checkpoints.len()
            )));
        }

        // Convert 1-based number to index (1 = oldest = first in log)
        let checkpoint = &checkpoints[checkpoints.len() - number];
        self.restore_checkpoint(&checkpoint.hash)
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
            let base_dir = PathBuf::from("/private/tmp/sned-checkpoints-tests");
            std::fs::create_dir_all(&base_dir).unwrap();
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

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
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

        let result = manager.restore_checkpoint(checkpoint1.as_ref().unwrap());
        assert!(result.is_ok(), "Restore should succeed: {:?}", result);

        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(
            content, "initial content",
            "File should be reverted to initial state"
        );

        let changed_files =
            manager.get_changed_files(checkpoint1.as_ref().unwrap(), checkpoint2.as_deref());
        assert!(changed_files.is_ok());
        let files = changed_files.unwrap();
        assert!(
            files.contains(&"test.txt".to_string()),
            "test.txt should be in changed files"
        );
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
