//! Search files tool handler for sned CLI.
//!

use crate::core::agent_loop::TaskState;
use crate::core::process_output::{capture_async, configured_output_limit};
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

use std::process::Output;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Default max search timeout (30s), configurable via SNED_SEARCH_TIMEOUT_SECS env var.
fn search_timeout() -> Duration {
    static TIMEOUT: OnceLock<Duration> = OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        let secs = std::env::var("SNED_SEARCH_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        Duration::from_secs(secs)
    })
}

/// Default max lines to return from search (rg --max-count)
const DEFAULT_SEARCH_MAX_LINES: u32 = 100;
/// Environment variable to configure search result limit
const SEARCH_MAX_LINES_ENV: &str = "SNED_SEARCH_MAX_LINES";
const DEFAULT_SEARCH_OUTPUT_LIMIT: usize = 100 * 1024;

fn search_output_limit() -> usize {
    configured_output_limit("SNED_SEARCH_OUTPUT_LIMIT", DEFAULT_SEARCH_OUTPUT_LIMIT)
}

/// Cached ripgrep availability check (checked once per process lifetime)
static RIPGREP_AVAILABLE: OnceLock<bool> = OnceLock::new();

#[derive(Debug, Clone, Default)]
pub struct SearchFilesHandler;

impl SearchFilesHandler {
    /// Search for files matching a regex pattern.
    ///
    /// `workspace_root` is the directory ripgrep chdirs into so it walks the
    /// correct tree regardless of the process's launch directory, and so the
    /// emitted paths stay workspace-relative (or external-root-relative when
    /// `path` resolves outside the workspace).
    pub async fn search_files(
        &self,
        workspace_root: &Path,
        path: Option<&Path>,
        regex: &str,
        file_pattern: Option<&str>,
    ) -> anyhow::Result<String> {
        let (cwd, search_target) = resolve_cwd_and_target(workspace_root, path);

        // Check ripgrep availability once per process lifetime
        let use_ripgrep = *RIPGREP_AVAILABLE.get_or_init(|| {
            std::process::Command::new("rg")
                .arg("--version")
                .output()
                .is_ok()
        });

        let mut cmd = if use_ripgrep {
            // ripgrep flags:
            // -n: line number
            // --color: never (we handle highlighting)
            // -H: print filename (always, even for single file)
            // --max-count: limit matches per file (prevents huge single-file results)
            // --hidden: match grep -r behavior for dotfiles and hidden directories
            let mut c = Command::new("rg");
            c.args([
                "--line-number",
                "--color=never",
                "--with-filename",
                "--hidden",
            ]);

            // Limit per-file to prevent huge results from single files
            let max_per_file = std::env::var(SEARCH_MAX_LINES_ENV)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(DEFAULT_SEARCH_MAX_LINES);
            c.arg("--max-count").arg(max_per_file.to_string());

            // With --hidden in play, rg happily walks .git, node_modules,
            // target, and build caches. Add default exclude globs so broad
            // searches do not waste time (and context) on vendor artifacts.
            // Vendor/registry directories (`.git`, `node_modules`) can appear
            // at any depth (submodules, nested packages) so they stay
            // unanchored. Build caches are anchored with a leading slash so
            // a `src/build/codegen.rs` source tree is not silently pruned.
            for glob in [
                "!.git",
                "!node_modules",
                "!/target",
                "!/build",
                "!/.build",
                "!/dist",
                "!/.cache",
                "!/.next",
            ] {
                c.arg("--glob").arg(glob);
            }

            c
        } else {
            // grep flags:
            // -r: recursive
            // -n: line number
            // -E: extended regex
            // -I: skip binary files
            // -H: print filename
            // grep has no built-in .gitignore awareness, so explicit
            // --exclude-dir is required to keep it out of vendor caches and
            // packfile directories on systems without rg installed.
            let mut c = Command::new("grep");
            c.arg("-rnEIH");
            c.arg("--exclude-dir=.git")
                .arg("--exclude-dir=node_modules")
                .arg("--exclude-dir=target")
                .arg("--exclude-dir=build")
                .arg("--exclude-dir=.build")
                .arg("--exclude-dir=dist")
                .arg("--exclude-dir=.cache")
                .arg("--exclude-dir=.next");
            c
        };

        if let Some(pattern) = file_pattern {
            if use_ripgrep {
                if pattern.contains('"')
                    || pattern.contains('\'')
                    || pattern.contains(';')
                    || pattern.contains('|')
                    || pattern.contains('&')
                    || pattern.contains('$')
                    || pattern.contains('`')
                {
                    return Err(anyhow::anyhow!(
                        "file_pattern contains disallowed shell metacharacters"
                    ));
                }
                cmd.arg("--glob").arg(pattern);
            } else {
                if pattern.contains(',')
                    || pattern.contains('"')
                    || pattern.contains('\'')
                    || pattern.contains(';')
                    || pattern.contains('|')
                    || pattern.contains('&')
                    || pattern.contains('$')
                    || pattern.contains('`')
                {
                    return Err(anyhow::anyhow!(
                        "file_pattern contains disallowed characters (commas, quotes, shell metacharacters)"
                    ));
                }
                cmd.arg("--include").arg(pattern);
            }
        }

        // Keep a leading '-' in the pattern from being interpreted as a search-tool option.
        cmd.current_dir(&cwd);
        cmd.arg("--").arg(regex).arg(&search_target);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = run_with_timeout(cmd, search_timeout()).await?;

        if !output.status.success() && output.stdout.is_empty() {
            if output.status.code() == Some(1) {
                let err = crate::cli::actionable_errors::search_no_results(regex);
                return Ok(err.display());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("invalid regex") || stderr.contains("unmatched") {
                let err = crate::cli::actionable_errors::invalid_regex(regex, &stderr);
                return Err(anyhow::anyhow!("{}", err.display()));
            }
            return Err(anyhow::anyhow!("grep failed: {stderr}"));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();

        if lines.is_empty() {
            let err = crate::cli::actionable_errors::search_no_results(regex);
            Ok(err.display())
        } else {
            let max_lines = std::env::var(SEARCH_MAX_LINES_ENV)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(DEFAULT_SEARCH_MAX_LINES as usize);

            // rg's --max-count is per-file, so a result of, say, 400 lines
            // can come from 10 files each emitting their own 40 matches. The
            // agent-visible output must still be capped to `max_lines` total
            // so context windows are not blown by a single broad search.
            let truncated = lines.len() >= max_lines;
            let emitted: &[&str] = if lines.len() > max_lines {
                &lines[..max_lines]
            } else {
                &lines
            };
            let mut result = emitted.join("\n");
            if truncated {
                result.push_str(&format!(
                    "\n\n(Too many matches, showing first {max_lines}. Please refine your search.)"
                ));
            }
            Ok(result)
        }
    }

    pub async fn execute(
        &self,
        _state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        self.execute_without_state(Path::new("."), params).await
    }

    async fn execute_without_state(
        &self,
        workspace_root: &Path,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        self.execute_with_external_roots(workspace_root, &[], params)
            .await
    }

    async fn execute_with_external_roots(
        &self,
        workspace_root: &Path,
        allowed_external_roots: &[std::path::PathBuf],
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let regex = params["regex"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("Missing 'regex' parameter".to_string()))?;

        if regex.len() > 500 {
            return Err(ToolError::InvalidInput(
                "Regex pattern too long (max 500 characters)".to_string(),
            ));
        }

        let group_count = regex_group_count(regex);
        if group_count > 10 {
            return Err(ToolError::InvalidInput(
                "Regex pattern too complex (max 10 groups)".to_string(),
            ));
        }
        let path = params["path"].as_str();
        let file_pattern = params["file_pattern"].as_str();

        let sanitized_path = path.map(|p| {
            crate::core::tools::resolve_authorized_path(workspace_root, allowed_external_roots, p)
        });
        let search_path = match sanitized_path {
            Some(Ok(p)) => Some(p),
            Some(Err(e)) => return Err(e),
            None => None,
        };

        self.search_files(workspace_root, search_path.as_deref(), regex, file_pattern)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
    }
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

fn regex_group_count(regex: &str) -> usize {
    let mut count = 0usize;
    let mut consecutive_backslashes = 0usize;

    for ch in regex.chars() {
        if ch == '\\' {
            consecutive_backslashes += 1;
            continue;
        }

        if ch == '(' && consecutive_backslashes.is_multiple_of(2) {
            count += 1;
        }

        consecutive_backslashes = 0;
    }

    count
}

/// Compute the directory ripgrep should chdir into and the search target to
/// pass it, given the agent's workspace root and an optional user-supplied
/// path. Paths that already live inside `workspace_root` are reduced to a
/// workspace-relative form so rg's emitted paths stay compact; paths that
/// resolve outside the workspace (e.g. an authorized external root) keep
/// their absolute form and become the new cwd, with rg searching `.`.
fn resolve_cwd_and_target(workspace_root: &Path, path: Option<&Path>) -> (PathBuf, PathBuf) {
    match path {
        None => (workspace_root.to_path_buf(), PathBuf::from(".")),
        Some(p) => {
            if p.is_absolute() {
                if let Ok(canonical_root) = std::fs::canonicalize(workspace_root)
                    && let Ok(canonical_target) = std::fs::canonicalize(p)
                    && let Ok(rel) = canonical_target.strip_prefix(&canonical_root)
                {
                    let target = if rel.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        rel.to_path_buf()
                    };
                    return (workspace_root.to_path_buf(), target);
                }
                if p.is_dir() {
                    (p.to_path_buf(), PathBuf::from("."))
                } else if let (Some(parent), Some(name)) = (p.parent(), p.file_name())
                    && (parent.is_dir() || !parent.as_os_str().is_empty())
                {
                    (parent.to_path_buf(), PathBuf::from(name))
                } else {
                    (workspace_root.to_path_buf(), p.to_path_buf())
                }
            } else {
                let target = if p.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    p.to_path_buf()
                };
                (workspace_root.to_path_buf(), target)
            }
        }
    }
}

async fn run_with_timeout(mut cmd: Command, timeout_duration: Duration) -> anyhow::Result<Output> {
    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("search command did not capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("search command did not capture stderr"))?;

    let output_limit = search_output_limit();
    let stdout_task = tokio::spawn(capture_async(stdout, output_limit));
    let stderr_task = tokio::spawn(capture_async(stderr, output_limit));

    let status = if let Ok(status) = timeout(timeout_duration, child.wait()).await {
        status?
    } else {
        let _ = child.kill().await;
        // Drop reader task handles instead of awaiting them — they will
        // complete in the background once the killed process's pipes close.
        drop(stdout_task);
        drop(stderr_task);
        let _ = child.wait().await;
        let err = crate::cli::actionable_errors::command_timeout(
            "search_files",
            timeout_duration.as_secs(),
        );
        return Err(anyhow::anyhow!("{}", err.display()));
    };

    let stdout = stdout_task
        .await
        .map_err(|error| anyhow::anyhow!("search stdout task failed: {error}"))??;
    let stderr = stderr_task
        .await
        .map_err(|error| anyhow::anyhow!("search stderr task failed: {error}"))??;

    Ok(Output {
        status,
        stdout: stdout.into_display_bytes(output_limit, "search stdout"),
        stderr: stderr.into_display_bytes(output_limit, "search stderr"),
    })
}

impl ToolHandler for SearchFilesHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            handler
                .execute_with_external_roots(
                    &ctx.workspace_root,
                    &ctx.allowed_external_roots,
                    params,
                )
                .await
                .map(serde_json::Value::String)
        })
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let regex = params["regex"].as_str().unwrap_or("unknown regex");
        format!("Searching for / {regex} /")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;
    use tempfile::TempDir;

    // Mutex to serialize env var mutations across tests
    static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn test_limited_search_reader_drains_after_reaching_its_budget() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64);
        let writer_task = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 16 * 1024]).await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let output = capture_async(reader, 1024).await.unwrap();
        writer_task.await.unwrap();

        assert!(
            output
                .display(1024, "search stdout")
                .starts_with(&"x".repeat(1024))
        );
        assert!(
            output
                .display(1024, "search stdout")
                .contains("retaining 1024 of 16384 bytes")
        );
    }

    #[tokio::test]
    async fn test_search_files_basic() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "hello world\nfoo bar").unwrap();
        fs::write(temp_dir.path().join("file2.txt"), "hello rust").unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .search_files(temp_dir.path(), None, "hello", None)
            .await
            .unwrap();

        assert!(result.contains("file1.txt:1:hello world"));
        assert!(result.contains("file2.txt:1:hello rust"));
    }

    #[tokio::test]
    async fn test_search_files_no_matches() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "hello world").unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .search_files(temp_dir.path(), None, "nonexistent", None)
            .await
            .unwrap();

        assert!(
            result.contains("No matches found"),
            "expected no-matches message, got: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_search_files_treats_leading_dash_regex_as_pattern() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "hello world").unwrap();

        let result = SearchFilesHandler::new()
            .search_files(temp_dir.path(), None, "--files", None)
            .await
            .unwrap();

        assert!(result.contains("No matches found"));
        assert!(!result.contains("file1.txt"));
    }

    #[tokio::test]
    async fn test_search_files_with_pattern() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "hello world").unwrap();
        fs::write(temp_dir.path().join("file1.rs"), "hello rust").unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .search_files(temp_dir.path(), None, "hello", Some("*.rs"))
            .await
            .unwrap();

        assert!(result.contains("file1.rs:1:hello rust"));
        assert!(!result.contains("file1.txt"));
    }

    #[tokio::test]
    async fn test_search_files_includes_hidden_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join(".hidden.txt"), "secret needle").unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .search_files(temp_dir.path(), None, "needle", None)
            .await
            .unwrap();

        assert!(result.contains(".hidden.txt:1:secret needle"));
    }

    #[tokio::test]
    async fn test_search_files_respects_max_count_per_file() {
        let temp_dir = TempDir::new().unwrap();
        let content = (0..150)
            .map(|i| format!("match {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(temp_dir.path().join("large_file.txt"), content).unwrap();

        let result;
        let line_count;
        {
            let _guard = ENV_MUTEX.lock().await;
            // SAFETY: setting env var under mutex lock
            unsafe {
                std::env::set_var(SEARCH_MAX_LINES_ENV, "10");
            }
            result = SearchFilesHandler::new()
                .search_files(temp_dir.path(), None, "match", None)
                .await
                .unwrap();

            line_count = result
                .lines()
                .filter(|l| l.contains("large_file.txt:"))
                .count();
            // SAFETY: restoring env under mutex lock
            unsafe {
                std::env::remove_var(SEARCH_MAX_LINES_ENV);
            }
        }
        assert!(
            line_count <= 10,
            "expected <= 10 match lines, got {}",
            line_count
        );
        assert!(result.contains("Too many matches"));
    }

    #[tokio::test]
    async fn test_search_files_truncates_globally_across_multiple_files() {
        // 5 files × 5 matches = 25 total matches against a 10-line cap; the
        // emitted body must be sliced to 10 lines and the banner must report
        // the cap (10), not the un-truncated count (25).
        let temp_dir = TempDir::new().unwrap();
        for i in 0..5 {
            fs::write(
                temp_dir.path().join(format!("file_{i}.txt")),
                "needle\nneedle\nneedle\nneedle\nneedle\n",
            )
            .unwrap();
        }

        let result;
        let match_lines;
        {
            let _guard = ENV_MUTEX.lock().await;
            unsafe {
                std::env::set_var(SEARCH_MAX_LINES_ENV, "10");
            }
            result = SearchFilesHandler::new()
                .search_files(temp_dir.path(), None, "needle", None)
                .await
                .unwrap();
            match_lines = result.lines().filter(|l| l.contains(":needle")).count();
            unsafe {
                std::env::remove_var(SEARCH_MAX_LINES_ENV);
            }
        }
        assert_eq!(
            match_lines, 10,
            "global cap must hold across files: {result}"
        );
        assert!(
            result.contains("showing first 10"),
            "banner must report the cap, not the un-truncated count: {result}"
        );
    }

    #[tokio::test]
    async fn test_search_files_custom_max_count_via_env() {
        let temp_dir = TempDir::new().unwrap();
        let content = (0..50)
            .map(|i| format!("match {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(temp_dir.path().join("large_file.txt"), content).unwrap();

        let result;
        let line_count;
        {
            let _guard = ENV_MUTEX.lock().await;
            // SAFETY: setting env var under mutex lock
            unsafe {
                std::env::set_var(SEARCH_MAX_LINES_ENV, "3");
            }
            result = SearchFilesHandler::new()
                .search_files(temp_dir.path(), None, "match", None)
                .await
                .unwrap();
            line_count = result
                .lines()
                .filter(|l| l.contains("large_file.txt:"))
                .count();
            // SAFETY: restoring env under mutex lock
            unsafe {
                std::env::remove_var(SEARCH_MAX_LINES_ENV);
            }
        }
        assert!(
            line_count <= 3,
            "expected <= 3 match lines, got {}",
            line_count
        );
        assert!(result.contains("Too many matches"));
    }

    #[tokio::test]
    async fn test_search_command_timeout() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5");
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let started = Instant::now();
        let err = run_with_timeout(cmd, Duration::from_millis(100))
            .await
            .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn test_ripgrep_availability_cached() {
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static TEST_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
        let test_once = OnceLock::new();

        // Reset counter
        TEST_CALL_COUNT.store(0, Ordering::Relaxed);

        // Simulate the caching logic
        let check_availability = || {
            TEST_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
            std::process::Command::new("rg")
                .arg("--version")
                .output()
                .is_ok()
        };

        // First call initializes
        let result1 = *test_once.get_or_init(&check_availability);

        // Verify cache is initialized
        assert!(test_once.get().is_some());
        let calls_after_first = TEST_CALL_COUNT.load(Ordering::Relaxed);

        // Second call should use cached value
        let result2 = *test_once.get_or_init(&check_availability);
        let calls_after_second = TEST_CALL_COUNT.load(Ordering::Relaxed);

        // Results should be consistent
        assert_eq!(result1, result2);

        // Call count should not increase on second call
        assert_eq!(
            calls_after_first, calls_after_second,
            "ripgrep availability was checked twice ({} vs {})",
            calls_after_first, calls_after_second
        );
        assert_eq!(
            calls_after_first, 1,
            "expected exactly one availability check"
        );
    }

    #[test]
    fn test_regex_group_count_handles_double_escaped_backslashes() {
        assert_eq!(regex_group_count(r"\(literal\)"), 0);
        assert_eq!(regex_group_count(r"\\(a)(b)"), 2);
    }

    #[tokio::test]
    async fn test_execute_rejects_too_many_groups_after_escaped_backslash() {
        let regex = r"\\((a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)";
        let err = SearchFilesHandler::new()
            .execute_without_state(Path::new("."), serde_json::json!({ "regex": regex }))
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput(message) => {
                assert!(message.contains("max 10 groups"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_search_files_skips_default_excluded_directories() {
        // Vendor and build dirs must be skipped even though --hidden is on, so
        // that broad searches do not blow context on generated artifacts.
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("keep.txt"), "needle\n").unwrap();
        fs::create_dir(temp_dir.path().join("node_modules")).unwrap();
        fs::write(temp_dir.path().join("node_modules/pkg.js"), "needle\n").unwrap();
        fs::create_dir(temp_dir.path().join("target")).unwrap();
        fs::write(temp_dir.path().join("target/bundle.js"), "needle\n").unwrap();
        fs::create_dir(temp_dir.path().join(".git")).unwrap();
        fs::write(temp_dir.path().join(".git/HEAD"), "needle\n").unwrap();

        let result = SearchFilesHandler::new()
            .search_files(temp_dir.path(), None, "needle", None)
            .await
            .unwrap();

        assert!(
            result.contains("keep.txt"),
            "must surface in-workspace hit: {result}"
        );
        assert!(
            !result.contains("node_modules"),
            "must skip node_modules: {result}"
        );
        assert!(!result.contains("target/"), "must skip target/: {result}");
        assert!(!result.contains(".git/"), "must skip .git/: {result}");
    }

    #[test]
    fn test_resolve_cwd_and_target_uses_workspace_root_when_path_none() {
        let workspace = Path::new("/custom/workspace");
        let (cwd, target) = resolve_cwd_and_target(workspace, None);
        assert_eq!(cwd, workspace);
        assert_eq!(target, Path::new("."));
    }

    #[test]
    fn test_resolve_cwd_and_target_workspace_root_path_resolves_to_dot() {
        let workspace = TempDir::new().unwrap();
        let (cwd, target) = resolve_cwd_and_target(workspace.path(), Some(Path::new(".")));
        assert_eq!(cwd, workspace.path());
        assert_eq!(target, Path::new("."));

        let (cwd_abs, target_abs) =
            resolve_cwd_and_target(workspace.path(), Some(workspace.path()));
        assert_eq!(cwd_abs, workspace.path());
        assert_eq!(target_abs, Path::new("."));
    }

    #[tokio::test]
    async fn test_search_files_explicit_dot_path_succeeds() {
        let workspace = TempDir::new().unwrap();
        std::fs::write(workspace.path().join("hello.txt"), "needle in dot path\n").unwrap();

        let result = SearchFilesHandler::new()
            .search_files(workspace.path(), Some(Path::new(".")), "needle", None)
            .await
            .unwrap();

        assert!(
            result.contains("hello.txt:1:needle in dot path"),
            "search with path '.' must succeed without IO error: {result}"
        );
    }

    #[test]
    fn test_resolve_cwd_and_target_external_file_uses_parent_dir() {
        let workspace = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let external_file = external.path().join("sub/file.rs");
        std::fs::create_dir_all(external.path().join("sub")).unwrap();
        std::fs::write(&external_file, "fn main() {}\n").unwrap();

        let (cwd, target) = resolve_cwd_and_target(workspace.path(), Some(&external_file));
        assert_eq!(cwd, external.path().join("sub"));
        assert_eq!(target, Path::new("file.rs"));
    }

    #[tokio::test]
    async fn test_search_files_does_not_swallow_src_build_directory() {
        // Build caches must be anchored to the workspace root so a source
        // tree legitimately named `build/` (e.g. src/build/codegen.rs) is
        // not silently pruned by the default excludes.
        let temp_dir = TempDir::new().unwrap();
        fs::create_dir_all(temp_dir.path().join("src/build")).unwrap();
        fs::write(temp_dir.path().join("src/build/codegen.rs"), "needle\n").unwrap();

        let result = SearchFilesHandler::new()
            .search_files(temp_dir.path(), None, "needle", None)
            .await
            .unwrap();

        assert!(
            result.contains("src/build/codegen.rs"),
            "must surface src/build source tree: {result}"
        );
    }

    #[tokio::test]
    async fn test_search_files_external_file_does_not_enotdir() {
        let workspace = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let external_canonical = external.path().canonicalize().unwrap();
        let external_file = external_canonical.join("external_config.json");
        fs::write(&external_file, "{\n  \"port\": 8080\n}\n").unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .execute_with_external_roots(
                workspace.path(),
                &[external_canonical],
                serde_json::json!({
                    "path": external_file.to_str().unwrap(),
                    "regex": "port",
                }),
            )
            .await
            .expect("must successfully search external file without ENOTDIR");

        assert!(
            result.contains("8080"),
            "must find content in external file: {result}"
        );
    }

    #[tokio::test]
    async fn test_search_files_external_nonexistent_file_under_existing_dir() {
        // Path lives under an authorized external root but the file itself
        // has not been written yet. canonicalize fails, so the resolver must
        // fall back to "chdir to parent dir, search basename" rather than
        // walking the drive root.
        let workspace = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let external_canonical = external.path().canonicalize().unwrap();
        let nonexistent = external_canonical.join("not_yet_created.rs");
        assert!(!nonexistent.exists());

        let handler = SearchFilesHandler::new();
        // We do not require this to succeed — rg returning no matches or a
        // graceful error is fine. What we MUST NOT do is walk up to the drive
        // root and emit absolute paths like "/Users/.../something".
        let outcome = handler
            .execute_with_external_roots(
                workspace.path(),
                &[external_canonical],
                serde_json::json!({
                    "path": nonexistent.to_str().unwrap(),
                    "regex": "needle",
                }),
            )
            .await;

        match outcome {
            Ok(text) => assert!(
                !text.lines().any(|line| line.starts_with('/')),
                "must not walk the drive root: {text}"
            ),
            Err(_) => {}
        }
    }
}
