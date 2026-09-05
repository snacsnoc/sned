//! Shadow git repository for agent change tracking.
//!
//! Maintains a separate git repository at `.sned/.git-agent/` that tracks
//! agent changes without polluting the user's real git history.
//!
//! Features:
//! - Auto-commit after each file-modifying turn
//! - /undo to revert last turn
//! - /diff to review changes
//! - /log to see change history
//! - /commit to finalize changes to user's real git

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const SHADOW_GIT_DIR: &str = ".sned/.git-agent";
const SHADOW_GIT_AUTHOR_NAME: &str = "Sned";
const SHADOW_GIT_AUTHOR_EMAIL: &str = "sned@localhost";
const SHADOW_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const STALE_INDEX_LOCK_AGE: Duration = Duration::from_secs(90);

fn porcelain_paths(status: &[u8]) -> Vec<String> {
    let mut records = status
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty());
    let mut paths = Vec::new();

    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }

        let kind = &record[..2];
        paths.push(String::from_utf8_lossy(&record[3..]).into_owned());
        if kind.contains(&b'R') || kind.contains(&b'C') {
            let _ = records.next();
        }
    }

    paths
}

pub(crate) struct ShadowGitLock {
    _file: File,
}

impl ShadowGitLock {
    fn acquire(workspace_root: &Path) -> Result<Self> {
        Self::acquire_path(&workspace_root.join(".sned/.git-agent/sned.lock"))
    }

    fn acquire_path(lock_path: &Path) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)?;

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
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(error.into());
                }
                if started.elapsed() >= SHADOW_LOCK_TIMEOUT {
                    anyhow::bail!("shadow git repository is busy: {}", lock_path.display());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        #[cfg(not(unix))]
        {
            Ok(Self { _file: file })
        }
    }
}

fn cleanup_stale_index_lock(workspace_root: &Path) -> Result<()> {
    let path = workspace_root.join(SHADOW_GIT_DIR).join("index.lock");
    let Ok(metadata) = fs::metadata(&path) else {
        return Ok(());
    };
    let age = metadata.modified()?.elapsed().unwrap_or_default();
    if age >= STALE_INDEX_LOCK_AGE {
        fs::remove_file(&path)?;
        tracing::warn!(path = %path.display(), age_secs = age.as_secs(), "removed stale shadow git index lock");
    }
    Ok(())
}

/// Serialize Sned operations that read or mutate a workspace through Git.
///
/// Checkpoint repositories use a separate Git directory, but share the same
/// worktree. They must acquire this guard before their repository-specific
/// lock so a checkpoint restore cannot race a shadow-git snapshot.
pub(crate) fn acquire_workspace_git_lock(workspace_root: &Path) -> Result<ShadowGitLock> {
    let _lock = ShadowGitLock::acquire(workspace_root)?;
    cleanup_stale_index_lock(workspace_root)?;
    Ok(_lock)
}

fn with_shadow_lock<T>(workspace_root: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _lock = acquire_workspace_git_lock(workspace_root)?;
    operation()
}

fn with_real_git_lock<T>(
    workspace_root: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock_path = workspace_root.join(".sned/.git-agent/real-git.lock");
    let _lock = ShadowGitLock::acquire_path(&lock_path)?;
    operation()
}

/// Initialize the shadow git repository if it doesn't exist.
pub fn init_shadow_repo(workspace_root: &Path) -> Result<()> {
    with_shadow_lock(workspace_root, || init_shadow_repo_locked(workspace_root))
}

fn init_shadow_repo_locked(workspace_root: &Path) -> Result<()> {
    let shadow_git_path = workspace_root.join(SHADOW_GIT_DIR);

    if shadow_git_path.join("HEAD").exists() {
        // Already initialized
        return Ok(());
    }

    // Create parent directories
    std::fs::create_dir_all(&shadow_git_path)
        .with_context(|| format!("Failed to create shadow git dir: {shadow_git_path:?}"))?;

    // Initialize git repo
    let output = Command::new("git")
        .arg("init")
        .arg("--initial-branch=main")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to initialize shadow git repo")?;

    if !output.status.success() {
        anyhow::bail!(
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Create initial commit (force=true to allow empty commit)
    commit_turn_internal(workspace_root, "[sned] session start", true)?;

    Ok(())
}

/// Commit the current state to the shadow repo.
pub fn commit_turn(workspace_root: &Path, message: &str) -> Result<()> {
    with_shadow_lock(workspace_root, || {
        commit_turn_internal(workspace_root, message, false)
    })
}

fn commit_turn_internal(workspace_root: &Path, message: &str, force: bool) -> Result<()> {
    let shadow_git_path = workspace_root.join(SHADOW_GIT_DIR);

    // Create a temporary exclude file with default ignores
    let default_ignores = [
        "target/",
        "node_modules/",
        ".git/",
        "*.pyc",
        "__pycache__/",
        "*.o",
        "*.so",
        "*.dll",
        "*.exe",
        "dist/",
        "build/",
        ".sned/",
        ".git-agent/",
        "*.db",
        "*.sqlite",
        "*.sqlite3",
    ];

    let exclude_file = workspace_root.join(".sned/.git-agent/excludes");
    let exclude_file_abs = exclude_file.canonicalize().unwrap_or(exclude_file);
    fs::create_dir_all(exclude_file_abs.parent().unwrap()).ok();
    let mut f = fs::File::create(&exclude_file_abs).context("Failed to create exclude file")?;
    for ignore in &default_ignores {
        writeln!(f, "{ignore}").ok();
    }
    drop(f);

    // Set core.excludesFile in shadow repo config
    let config_output = Command::new("git")
        .args([
            "config",
            "core.excludesFile",
            exclude_file_abs.to_string_lossy().as_ref(),
        ])
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to set git config")?;

    if !config_output.status.success() {
        anyhow::bail!(
            "git config failed: {}",
            String::from_utf8_lossy(&config_output.stderr)
        );
    }

    // Stage all changes with ignores
    let output = Command::new("git")
        .arg("add")
        .arg("-A")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to stage files for shadow commit")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("no files added") {
            anyhow::bail!("git add failed: {stderr}");
        }
        // No files to add is OK for initial commit
    }

    // Check if there's anything to commit
    let diff_output = Command::new("git")
        .arg("diff")
        .arg("--cached")
        .arg("--quiet")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to check for changes")?;

    if diff_output.status.success() && !force {
        // No changes to commit
        return Ok(());
    }

    // Commit
    let mut commit_cmd = Command::new("git");
    commit_cmd
        .arg("commit")
        .arg("-m")
        .arg(message)
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .env("GIT_AUTHOR_NAME", SHADOW_GIT_AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", SHADOW_GIT_AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", SHADOW_GIT_AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", SHADOW_GIT_AUTHOR_EMAIL);

    if force {
        commit_cmd.arg("--allow-empty");
    }

    let output = commit_cmd
        .output()
        .context("Failed to commit to shadow repo")?;

    if !output.status.success() {
        anyhow::bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

/// Undo the last turn by reverting to the previous shadow commit.
/// Returns a list of files that were reverted.
pub fn undo_last_turn(workspace_root: &Path) -> Result<(Vec<String>, Vec<String>)> {
    with_shadow_lock(workspace_root, || undo_last_turn_locked(workspace_root))
}

fn undo_last_turn_locked(workspace_root: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let shadow_git_path = workspace_root.join(SHADOW_GIT_DIR);

    if !shadow_git_path.join("HEAD").exists() {
        anyhow::bail!("Shadow git repo not initialized");
    }

    // Check we have at least 2 commits (can't undo the initial commit)
    let rev_list_output = Command::new("git")
        .arg("rev-list")
        .arg("--count")
        .arg("HEAD")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to count shadow commits")?;

    let commit_count: usize = String::from_utf8_lossy(&rev_list_output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    if commit_count < 2 {
        anyhow::bail!("No turns to undo");
    }

    // Get files added in the last turn (these need to be deleted)
    let added_files_output = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("--diff-filter=A")
        .arg("HEAD~1")
        .arg("HEAD")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to get added files")?;

    let added_files: Vec<String> = String::from_utf8_lossy(&added_files_output.stdout)
        .lines()
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string)
        .collect();

    // Get files modified in the last turn
    let modified_files_output = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("HEAD~1")
        .arg("HEAD")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to get modified files")?;

    let modified_files: Vec<String> = String::from_utf8_lossy(&modified_files_output.stdout)
        .lines()
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string)
        .collect();

    // Check for uncommitted changes outside the last turn's modified files
    // to prevent destructive reset from destroying unrelated user edits
    let status_output = Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .arg("-z")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to check working tree status")?;

    if status_output.status.success() {
        let uncommitted_files = porcelain_paths(&status_output.stdout);

        // Check if any uncommitted file is outside the last turn's modified files
        let unexpected_changes: Vec<&String> = uncommitted_files
            .iter()
            .filter(|f| !modified_files.contains(f) && !added_files.contains(f))
            .collect();

        if !unexpected_changes.is_empty() {
            anyhow::bail!(
                "Cannot undo: uncommitted changes detected in files not modified in the last turn: {}. \
                 Commit or stash these changes before undoing, or use /diff to review pending changes.",
                unexpected_changes
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // Reset shadow repo HEAD to previous commit (restores working tree)
    let output = Command::new("git")
        .arg("reset")
        .arg("--hard")
        .arg("HEAD~1")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to reset shadow repo")?;

    if !output.status.success() {
        anyhow::bail!(
            "git reset failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Delete files that were added in the last turn (now untracked after reset)
    for file in &added_files {
        let file_path = workspace_root.join(file);
        if file_path.exists() {
            std::fs::remove_file(&file_path)
                .with_context(|| format!("Failed to remove added file: {file}"))?;
        }
        // Remove empty parent directories to avoid accumulating orphan dirs
        if let Some(parent) = file_path.parent()
            && parent != workspace_root
        {
            // Remove empty parent dirs (fails gracefully if non-empty)
            let _ = std::fs::remove_dir(parent);
        }
    }

    Ok((added_files, modified_files))
}

/// Show diff between two turns.
pub fn diff_turns(workspace_root: &Path, from: usize, to: usize) -> Result<String> {
    with_shadow_lock(workspace_root, || {
        diff_turns_locked(workspace_root, from, to)
    })
}

fn diff_turns_locked(workspace_root: &Path, from: usize, to: usize) -> Result<String> {
    let shadow_git_path = workspace_root.join(SHADOW_GIT_DIR);

    if !shadow_git_path.join("HEAD").exists() {
        anyhow::bail!("Shadow git repo not initialized");
    }

    let from_ref = format!("HEAD~{from}");
    let to_ref = if to == 0 {
        "HEAD".to_string()
    } else {
        format!("HEAD~{to}")
    };

    let output = Command::new("git")
        .arg("diff")
        .arg(&from_ref)
        .arg(&to_ref)
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to get diff")?;

    if !output.status.success() {
        anyhow::bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Show the shadow git log.
pub fn log(workspace_root: &Path, limit: Option<usize>) -> Result<String> {
    with_shadow_lock(workspace_root, || log_locked(workspace_root, limit))
}

fn log_locked(workspace_root: &Path, limit: Option<usize>) -> Result<String> {
    let shadow_git_path = workspace_root.join(SHADOW_GIT_DIR);

    if !shadow_git_path.join("HEAD").exists() {
        anyhow::bail!("Shadow git repo not initialized");
    }

    let mut cmd = Command::new("git");
    cmd.arg("log")
        .arg("--oneline")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root);

    if let Some(n) = limit {
        cmd.arg("-n").arg(n.to_string());
    }

    let output = cmd.output().context("Failed to get shadow git log")?;

    if !output.status.success() {
        anyhow::bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Commit shadow changes to the user's real git repo.
pub fn commit_to_real_git(workspace_root: &Path, message: &str) -> Result<Vec<String>> {
    with_shadow_lock(workspace_root, || {
        with_real_git_lock(workspace_root, || {
            commit_to_real_git_locked(workspace_root, message)
        })
    })
}

fn commit_to_real_git_locked(workspace_root: &Path, message: &str) -> Result<Vec<String>> {
    let shadow_git_path = workspace_root.join(SHADOW_GIT_DIR);
    let real_git_path = workspace_root.join(".git");

    if !shadow_git_path.join("HEAD").exists() {
        anyhow::bail!("Shadow git repo not initialized");
    }

    if !real_git_path.exists() {
        anyhow::bail!("Not a git repository");
    }

    // Get list of changed files in shadow repo
    let diff_output = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("HEAD~1")
        .arg("HEAD")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to get changed files")?;

    let changed_files: Vec<String> = String::from_utf8_lossy(&diff_output.stdout)
        .lines()
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string)
        .collect();

    if changed_files.is_empty() {
        anyhow::bail!("No changes to commit");
    }

    // Stage only the changed files to real git (not all uncommitted changes)
    for file in &changed_files {
        let output = Command::new("git")
            .arg("add")
            .arg(file)
            .current_dir(workspace_root)
            .output()
            .context("Failed to stage file")?;

        if !output.status.success() {
            anyhow::bail!(
                "git add failed for {}: {}",
                file,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let output = Command::new("git")
        .arg("commit")
        .arg("-m")
        .arg(message)
        .current_dir(workspace_root)
        .output()
        .context("Failed to commit to real git")?;

    if !output.status.success() {
        anyhow::bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(changed_files)
}

/// Check if shadow git is initialized.
#[must_use]
pub fn is_initialized(workspace_root: &Path) -> bool {
    workspace_root.join(SHADOW_GIT_DIR).join("HEAD").exists()
}

/// Get files that were modified by the user since the last shadow commit.
/// Excludes files that were already modified by the agent in the last turn.
pub fn get_user_edits_since_last_turn(workspace_root: &Path) -> Result<Vec<String>> {
    with_shadow_lock(workspace_root, || {
        get_user_edits_since_last_turn_locked(workspace_root)
    })
}

fn get_user_edits_since_last_turn_locked(workspace_root: &Path) -> Result<Vec<String>> {
    let shadow_git_path = workspace_root.join(SHADOW_GIT_DIR);

    if !shadow_git_path.join("HEAD").exists() {
        anyhow::bail!("Shadow git repo not initialized");
    }

    // Get files modified in the last turn
    let modified_files_output = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("HEAD~1")
        .arg("HEAD")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to get modified files")?;

    let modified_files: Vec<String> = String::from_utf8_lossy(&modified_files_output.stdout)
        .lines()
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string)
        .collect();

    // Get files added in the last turn
    let added_files_output = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("--diff-filter=A")
        .arg("HEAD~1")
        .arg("HEAD")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to get added files")?;

    let added_files: Vec<String> = String::from_utf8_lossy(&added_files_output.stdout)
        .lines()
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string)
        .collect();

    // Check for user edits (files modified in working tree but not in last turn)
    // Use git status --porcelain to catch both tracked changes and untracked files
    let status_output = Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .arg("-z")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output()
        .context("Failed to check working tree status")?;

    let user_edited: Vec<String> = porcelain_paths(&status_output.stdout)
        .into_iter()
        .filter(|s| !modified_files.contains(s) && !added_files.contains(s))
        .collect();

    Ok(user_edited)
}

/// Get the number of turns (commits) in the shadow repo.
#[must_use]
pub fn turn_count(workspace_root: &Path) -> usize {
    let shadow_git_path = workspace_root.join(SHADOW_GIT_DIR);

    if !shadow_git_path.join("HEAD").exists() {
        return 0;
    }

    let output = Command::new("git")
        .arg("rev-list")
        .arg("--count")
        .arg("HEAD")
        .current_dir(workspace_root)
        .env("GIT_DIR", &shadow_git_path)
        .env("GIT_WORK_TREE", workspace_root)
        .output();

    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn git_config_for_tests(workspace_root: &Path) {
        let _ = Command::new("git")
            .args(["config", "user.name", "Sned Test"])
            .current_dir(workspace_root)
            .output();
        let _ = Command::new("git")
            .args(["config", "user.email", "test@sned.run"])
            .current_dir(workspace_root)
            .output();
    }

    #[test]
    fn test_init_shadow_repo_creates_head() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();
        assert!(is_initialized(temp.path()));
        assert!(temp.path().join(SHADOW_GIT_DIR).join("HEAD").exists());
    }

    #[test]
    fn test_init_shadow_repo_idempotent() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();
        init_shadow_repo(temp.path()).unwrap();
        assert_eq!(turn_count(temp.path()), 1);
    }

    #[test]
    fn test_commit_turn_tracks_changes() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();

        fs::write(temp.path().join("test.txt"), "hello").unwrap();
        commit_turn(temp.path(), "test commit").unwrap();
        assert_eq!(turn_count(temp.path()), 2);
    }

    #[test]
    fn test_shadow_commits_use_internal_identity() {
        let temp = TempDir::new().unwrap();
        init_shadow_repo(temp.path()).unwrap();

        fs::write(temp.path().join("test.txt"), "hello").unwrap();
        commit_turn(temp.path(), "test commit").unwrap();

        let output = Command::new("git")
            .args(["log", "-1", "--format=%an <%ae>|%cn <%ce>"])
            .current_dir(temp.path())
            .env("GIT_DIR", temp.path().join(SHADOW_GIT_DIR))
            .env("GIT_WORK_TREE", temp.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "Sned <sned@localhost>|Sned <sned@localhost>"
        );
    }

    #[test]
    fn test_commit_turn_no_changes_skips() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();
        let count_before = turn_count(temp.path());
        commit_turn(temp.path(), "no-op commit").unwrap();
        assert_eq!(turn_count(temp.path()), count_before);
    }

    #[test]
    fn test_undo_last_turn_reverts_changes() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();

        fs::write(temp.path().join("test.txt"), "hello").unwrap();
        commit_turn(temp.path(), "add test file").unwrap();

        let (added, _modified) = undo_last_turn(temp.path()).unwrap();
        assert!(added.contains(&"test.txt".to_string()));
        assert!(!temp.path().join("test.txt").exists());
        assert_eq!(turn_count(temp.path()), 1);
    }

    #[test]
    fn test_undo_last_turn_requires_two_commits() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();
        let err = undo_last_turn(temp.path()).unwrap_err();
        assert!(err.to_string().contains("No turns to undo"));
    }

    #[test]
    fn test_diff_turns_shows_changes() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();

        fs::write(temp.path().join("file.txt"), "content").unwrap();
        commit_turn(temp.path(), "add file").unwrap();

        let diff = diff_turns(temp.path(), 1, 0).unwrap();
        assert!(!diff.is_empty());
        assert!(diff.contains("file.txt"));
    }

    #[test]
    fn test_log_shows_commits() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();

        fs::write(temp.path().join("a.txt"), "a").unwrap();
        commit_turn(temp.path(), "add a").unwrap();

        let log = log(temp.path(), Some(5)).unwrap();
        assert!(!log.is_empty());
        assert!(log.contains("add a"));
    }

    #[test]
    fn test_is_initialized_false_before_init() {
        let temp = TempDir::new().unwrap();
        assert!(!is_initialized(temp.path()));
    }

    #[test]
    fn test_turn_count_zero_before_init() {
        let temp = TempDir::new().unwrap();
        assert_eq!(turn_count(temp.path()), 0);
    }

    #[test]
    fn test_get_user_edits_detects_user_changes() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();

        fs::write(temp.path().join("agent_file.txt"), "agent").unwrap();
        commit_turn(temp.path(), "agent change").unwrap();

        fs::write(temp.path().join("user_file.txt"), "user").unwrap();
        let edits = get_user_edits_since_last_turn(temp.path()).unwrap();
        assert!(edits.contains(&"user_file.txt".to_string()));
        assert!(!edits.contains(&"agent_file.txt".to_string()));
    }

    #[test]
    fn test_get_user_edits_preserves_paths_with_spaces() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());
        init_shadow_repo(temp.path()).unwrap();

        fs::write(temp.path().join("agent file.txt"), "agent").unwrap();
        commit_turn(temp.path(), "agent change").unwrap();

        fs::write(temp.path().join("user file.txt"), "user").unwrap();
        let edits = get_user_edits_since_last_turn(temp.path()).unwrap();
        assert_eq!(edits, vec!["user file.txt"]);
    }

    #[test]
    fn test_porcelain_paths_preserve_spaces_and_renames() {
        assert_eq!(
            porcelain_paths(b"?? a directory/user file.txt\0"),
            vec!["a directory/user file.txt"]
        );
        assert_eq!(
            porcelain_paths(b"R  new file.txt\0old file.txt\0"),
            vec!["new file.txt"]
        );
    }

    #[test]
    fn test_commit_to_real_git_stages_changed_files() {
        let temp = TempDir::new().unwrap();
        git_config_for_tests(temp.path());

        // Initialize real git repo
        let _ = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(temp.path())
            .output();
        git_config_for_tests(temp.path());

        init_shadow_repo(temp.path()).unwrap();

        fs::write(temp.path().join("shadow_file.txt"), "shadow").unwrap();
        commit_turn(temp.path(), "shadow commit").unwrap();

        let files = commit_to_real_git(temp.path(), "promote to real git").unwrap();
        assert!(files.contains(&"shadow_file.txt".to_string()));
    }
}
