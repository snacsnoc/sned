//! Search files tool handler for sned CLI.
//!

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
use async_trait::async_trait;
use std::path::Path;

use std::io;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

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

        // Prefer ripgrep (rg) for better performance on large repos (5-10x faster than grep)
        // Fall back to grep if rg is not available
        let use_ripgrep = Command::new("rg").arg("--version").output().await.is_ok();

        let mut cmd = if use_ripgrep {
            // ripgrep flags:
            // -n: line number
            // --color: never (we handle highlighting)
            // -H: print filename (always, even for single file)
            let mut c = Command::new("rg");
            c.args(["--line-number", "--color=never", "--with-filename"]);
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
                cmd.arg("--glob").arg(pattern);
            } else {
                cmd.arg(format!("--include={}", pattern));
            }
        }

        cmd.arg(regex).arg(search_path);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = run_with_timeout(cmd, SEARCH_TIMEOUT).await?;

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
        let mut lines: Vec<&str> = stdout.lines().collect();

        if lines.len() > 100 {
            lines.truncate(100);
            let mut result = lines.join("\n");
            result
                .push_str("\n\n(Too many matches, showing first 100. Please refine your search.)");
            Ok(result)
        } else if lines.is_empty() {
            let err = crate::cli::actionable_errors::search_no_results(regex);
            Ok(err.display())
        } else {
            Ok(lines.join("\n"))
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
}
