//! Search files tool handler for sned CLI.
//!

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
use async_trait::async_trait;
use std::path::Path;

use std::io;
use std::process::Output;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::AsyncReadExt;
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

/// Cached ripgrep availability check (checked once per process lifetime)
static RIPGREP_AVAILABLE: OnceLock<bool> = OnceLock::new();

#[derive(Debug, Clone, Default)]
pub struct SearchFilesHandler;

impl SearchFilesHandler {
    /// Search for files matching a regex pattern.
    ///
    pub async fn search_files(
        &self,
        path: Option<&str>,
        regex: &str,
        file_pattern: Option<&str>,
    ) -> anyhow::Result<String> {
        let search_path = path.unwrap_or(".");

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
            let mut c = Command::new("rg");
            c.args(["--line-number", "--color=never", "--with-filename"]);

            // Limit per-file to prevent huge results from single files
            let max_per_file = std::env::var(SEARCH_MAX_LINES_ENV)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(DEFAULT_SEARCH_MAX_LINES);
            c.arg("--max-count").arg(max_per_file.to_string());
            c
        } else {
            // grep flags:
            // -r: recursive
            // -n: line number
            // -E: extended regex
            // -I: skip binary files
            // -H: print filename
            let mut c = Command::new("grep");
            c.arg("-rnEIH");
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

        cmd.arg(regex).arg(search_path);
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
            return Err(anyhow::anyhow!("grep failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();

        if lines.is_empty() {
            let err = crate::cli::actionable_errors::search_no_results(regex);
            Ok(err.display())
        } else {
            // rg already limited results via --max-count
            let max_lines = std::env::var(SEARCH_MAX_LINES_ENV)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(DEFAULT_SEARCH_MAX_LINES as usize);

            if lines.len() >= max_lines {
                let mut result = lines.join("\n");
                result.push_str(&format!(
                    "\n\n(Too many matches, showing first {}. Please refine your search.)",
                    lines.len()
                ));
                Ok(result)
            } else {
                Ok(lines.join("\n"))
            }
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
        let regex = params["regex"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("Missing 'regex' parameter".to_string()))?;

        if regex.len() > 500 {
            return Err(ToolError::InvalidInput(
                "Regex pattern too long (max 500 characters)".to_string(),
            ));
        }

        let nesting = regex.chars().filter(|&c| c == '(').count();
        if nesting > 10 {
            return Err(ToolError::InvalidInput(
                "Regex pattern too complex (max 10 groups)".to_string(),
            ));
        }
        let path = params["path"].as_str();
        let file_pattern = params["file_pattern"].as_str();

        let sanitized_path = path.map(|p| resolve_sanitized_path(workspace_root, p));
        let search_path = match sanitized_path {
            Some(Ok(p)) => Some(p.to_string_lossy().into_owned()),
            Some(Err(e)) => return Err(e),
            None => None,
        };

        self.search_files(search_path.as_deref(), regex, file_pattern)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
    }
    pub fn new() -> Self {
        Self
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

    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let mut reader = tokio::io::BufReader::new(stdout);
        reader.read_to_end(&mut buffer).await?;
        Ok::<_, io::Error>(buffer)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let mut reader = tokio::io::BufReader::new(stderr);
        reader.read_to_end(&mut buffer).await?;
        Ok::<_, io::Error>(buffer)
    });

    let status = match timeout(timeout_duration, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let err = crate::cli::actionable_errors::command_timeout(
                "search_files",
                timeout_duration.as_secs(),
            );
            return Err(anyhow::anyhow!("{}", err.display()));
        }
    };

    let stdout = stdout_task
        .await
        .map_err(|error| anyhow::anyhow!("search stdout task failed: {}", error))??;
    let stderr = stderr_task
        .await
        .map_err(|error| anyhow::anyhow!("search stderr task failed: {}", error))??;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[async_trait]
impl ToolHandler for SearchFilesHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        self.execute_without_state(&ctx.workspace_root, params)
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let regex = params["regex"].as_str().unwrap_or("unknown regex");
        format!("Searching for / {} /", regex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_search_files_basic() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "hello world\nfoo bar").unwrap();
        fs::write(temp_dir.path().join("file2.txt"), "hello rust").unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .search_files(Some(temp_dir.path().to_str().unwrap()), "hello", None)
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
            .search_files(Some(temp_dir.path().to_str().unwrap()), "nonexistent", None)
            .await
            .unwrap();

        assert!(
            result.contains("No matches found"),
            "expected no-matches message, got: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_search_files_with_pattern() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "hello world").unwrap();
        fs::write(temp_dir.path().join("file1.rs"), "hello rust").unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .search_files(
                Some(temp_dir.path().to_str().unwrap()),
                "hello",
                Some("*.rs"),
            )
            .await
            .unwrap();

        assert!(result.contains("file1.rs:1:hello rust"));
        assert!(!result.contains("file1.txt"));
    }

    #[tokio::test]
    async fn test_search_files_respects_max_count_per_file() {
        // Create a file with more matches than the default limit (100)
        let temp_dir = TempDir::new().unwrap();
        let content = (0..150)
            .map(|i| format!("match {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(temp_dir.path().join("large_file.txt"), content).unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .search_files(Some(temp_dir.path().to_str().unwrap()), "match", None)
            .await
            .unwrap();

        // Should be limited to DEFAULT_SEARCH_MAX_LINES (100) per file
        // Count only lines with filename:linenum: pattern (actual rg output)
        let line_count = result
            .lines()
            .filter(|l| l.contains("large_file.txt:"))
            .count();
        assert!(
            line_count <= 100,
            "expected <= 100 match lines, got {}",
            line_count
        );
        assert!(result.contains("Too many matches"));
    }

    #[tokio::test]
    async fn test_search_files_custom_max_count_via_env() {
        // Test with custom limit via environment variable
        // Use a unique value to avoid interference from other tests
        // SAFETY: single-threaded test; sequential env mutation
        unsafe {
            std::env::set_var(SEARCH_MAX_LINES_ENV, "10");
        }

        let temp_dir = TempDir::new().unwrap();
        // Create a single file with more matches than the limit
        let content = (0..50)
            .map(|i| format!("match {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(temp_dir.path().join("large_file.txt"), content).unwrap();

        let handler = SearchFilesHandler::new();
        let result = handler
            .search_files(Some(temp_dir.path().to_str().unwrap()), "match", None)
            .await
            .unwrap();

        // Should be limited to 10 per file (plus "Too many matches" message)
        // Count only lines with filename:linenum: pattern (actual rg output)
        let line_count = result
            .lines()
            .filter(|l| l.contains("large_file.txt:"))
            .count();
        assert!(
            line_count <= 10,
            "expected <= 10 match lines, got {}",
            line_count
        );
        assert!(result.contains("Too many matches"));

        // SAFETY: single-threaded test; restoring env after test
        unsafe {
            std::env::remove_var(SEARCH_MAX_LINES_ENV);
        }
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
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Create a fresh OnceLock for testing by using a separate static
        static TEST_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

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
        let result1 = *RIPGREP_AVAILABLE.get_or_init(&check_availability);

        // Verify cache is initialized
        assert!(RIPGREP_AVAILABLE.get().is_some());
        let calls_after_first = TEST_CALL_COUNT.load(Ordering::Relaxed);

        // Second call should use cached value
        let result2 = *RIPGREP_AVAILABLE.get_or_init(&check_availability);
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
}
