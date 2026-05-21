//! Execute command tool handler for sned CLI.
//!

use crate::core::agent_loop::TaskState;
use crate::core::approval::CommandSafetyChecker;
use crate::core::tools::{ToolContext, ToolError, ToolHandler, coerce_string_array};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct ExecuteCommandHandler {
    safety_checker: CommandSafetyChecker,
}

impl Default for ExecuteCommandHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecuteCommandHandler {
    /// Resolve command timeout based on command patterns.
    ///
    fn resolve_timeout(cmd_str: &str) -> std::time::Duration {
        use std::sync::LazyLock;
        use std::time::Duration;

        static LONG_RUNNING_PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
            let patterns = [
                r"\b(?:npm|pnpm|yarn|bun)\s+(?:install|ci|build|test)\b",
                r"\b(?:npm|pnpm|yarn|bun)\s+run\s+(?:build|test|lint|typecheck|check)\b",
                r"\b(?:pip|pip3|uv)\s+install\b",
                r"\b(?:poetry|pipenv)\s+install\b",
                r"\b(?:cargo|go|mvn|gradle|gradlew)\s+(?:build|test|check|install)\b",
                r"\bdocker\s+(?:build|pull)\b",
                r"\bmake\b",
                r"\bcmake\b",
                r"\bwebpack\b",
                r"\bvite\b",
                r"\btsup\b",
                r"\brollup\b",
                r"\besbuild\b",
                r"\bnext\s+build\b",
                r"\bnuxt\s+build\b",
                r"\btsc\b",
                r"\btsc\s+--build\b",
            ];
            patterns
                .iter()
                .map(|p| {
                    regex::Regex::new(p).expect("LONG_RUNNING_PATTERNS contains invalid regex")
                })
                .collect()
        });

        for pattern in LONG_RUNNING_PATTERNS.iter() {
            if pattern.is_match(cmd_str) {
                return Duration::from_secs(300);
            }
        }

        Duration::from_secs(30)
    }

    /// Get the live streaming output line limit (default 20, configurable via SNED_STREAM_OUTPUT_LINES).
    fn stream_output_line_limit() -> usize {
        static LIMIT: OnceLock<usize> = OnceLock::new();
        *LIMIT.get_or_init(|| {
            std::env::var("SNED_STREAM_OUTPUT_LINES")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&v| v > 0)
                .unwrap_or(20)
        })
    }

    /// Execute one or more CLI commands.
    ///
    /// # Security
    ///
    /// Commands are passed to `sh -c` (or `cmd /C` on Windows) for execution.
    /// The `CommandSafetyChecker` validates commands against a safe list before
    /// execution, but callers should still ensure proper shell escaping when
    /// constructing command strings.
    ///
    /// ## Shell Escaping Requirements
    ///
    /// When constructing commands that include user-provided or model-generated
    /// arguments, ensure proper shell escaping to prevent injection:
    ///
    /// - Quote arguments containing spaces: `"file with spaces.txt"`
    /// - Escape special characters: `$`, `` ` ``, `!`, `*`, `?`, `[`, `]`
    /// - Avoid command substitution: `$()` and backticks
    /// - Escape quotes within quoted strings: `"arg with \"quotes\""`
    ///
    /// The safety checker rejects commands with `$()` and backticks, but proper
    /// escaping is still the caller's responsibility for other metacharacters.
    ///
    pub async fn execute_commands(
        &self,
        commands: Vec<String>,
        cwd: Option<&Path>,
    ) -> anyhow::Result<String> {
        // Default: apply safety checks (not explicitly approved)
        self.execute_commands_with_timeout(commands, cwd, None, false, None, false, false)
            .await
    }

    /// Execute commands with optional safety checking.
    ///
    /// When `explicitly_approved` is true, skip safety checks because the user
    /// has already reviewed and approved the specific command. Safety checks
    /// still apply for auto-approved commands (from "always" selection).
    async fn execute_commands_with_safety(
        &self,
        commands: Vec<String>,
        cwd: Option<&Path>,
        explicitly_approved: bool,
        task_state: Option<Arc<Mutex<TaskState>>>,
        raw_output: bool,
        json_output: bool,
    ) -> anyhow::Result<String> {
        self.execute_commands_with_timeout(
            commands,
            cwd,
            None,
            explicitly_approved,
            task_state,
            raw_output,
            json_output,
        )
        .await
    }

    async fn execute_commands_with_timeout(
        &self,
        commands: Vec<String>,
        cwd: Option<&Path>,
        timeout_override: Option<Duration>,
        explicitly_approved: bool,
        task_state: Option<Arc<Mutex<TaskState>>>,
        _raw_output: bool,
        json_output: bool,
    ) -> anyhow::Result<String> {
        self.execute_commands_tokio(
            commands,
            cwd,
            timeout_override,
            explicitly_approved,
            task_state,
            json_output,
        )
        .await
    }

    async fn execute_commands_tokio(
        &self,
        commands: Vec<String>,
        cwd: Option<&Path>,
        timeout_override: Option<Duration>,
        explicitly_approved: bool,
        task_state: Option<Arc<Mutex<TaskState>>>,
        json_output: bool,
    ) -> anyhow::Result<String> {
        use std::process::Stdio;
        use tokio::process::Command;
        use tokio::time::timeout;

        #[cfg(unix)]
        use libc;

        let mut combined_output = String::new();

        for cmd_str in commands {
            // Safety check: validate command against safe list and patterns
            // Skip safety checks for explicitly user-approved commands
            if !explicitly_approved && let Err(e) = self.safety_checker.is_safe(&cmd_str) {
                tracing::warn!(command = %cmd_str, reason = %e, "command rejected by safety checker");
                return Err(anyhow::anyhow!("{}", e));
            }
            tracing::debug!(command = %cmd_str, cwd = ?cwd, "executing command");

            if !json_output {
                eprintln!(
                    "{}",
                    crate::cli::colors::info(&format!("Running: {}", cmd_str))
                );
            }

            // Execute via shell for portability and shell feature support
            let mut cmd = if cfg!(target_os = "windows") {
                let mut c = Command::new("cmd");
                c.args(["/C", &cmd_str]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", &cmd_str]);
                // Create a new process group so we can kill all children on timeout
                #[cfg(unix)]
                c.process_group(0);
                c
            };

            if let Some(dir) = cwd {
                if !dir.exists() || !dir.is_dir() {
                    let err = crate::cli::actionable_errors::directory_not_found(
                        &dir.display().to_string(),
                    );
                    return Err(anyhow::anyhow!("{}", err.display()));
                }
                cmd.current_dir(dir);
            }

            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

            let timeout_duration =
                timeout_override.unwrap_or_else(|| Self::resolve_timeout(&cmd_str));
            let mut child = cmd.spawn()?;
            #[cfg(unix)]
            let child_pid = child.id().unwrap_or(0) as i32;

            // Register PID for cancellation tracking
            #[cfg(unix)]
            if child_pid != 0
                && let Some(ref state) = task_state
            {
                let mut state = state.lock().await;
                state.running_command_pids.push(child_pid);
                tracing::debug!("Registered command PID {} for cancellation", child_pid);
            }

            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("Command stdout was not captured"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| anyhow::anyhow!("Command stderr was not captured"))?;

            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stderr_reader = BufReader::new(stderr).lines();

            let mut stdout_collected = String::new();
            let mut stderr_collected = String::new();

            // Head+tail streaming condensation state
            let stream_limit = Self::stream_output_line_limit();
            let half = stream_limit / 2;
            let mut displayed: usize = 0;
            let mut truncated = false;
            let mut tail_buffer: VecDeque<String> = VecDeque::with_capacity(half);

            let output = loop {
                tokio::select! {
                    result = stdout_reader.next_line() => {
                        match result {
                            Ok(Some(line)) => {
                                displayed += 1;
                                if !json_output {
                                    if displayed <= half {
                                        // Head: print live
                                        eprintln!("{}", crate::cli::colors::colorize(&line, crate::cli::colors::style::DIM));
                                    } else if displayed == half + 1 && !truncated {
                                        // First skipped line: emit condensed note once
                                        eprintln!("{}", crate::cli::colors::colorize("... (stream condensed, set SNED_STREAM_OUTPUT_LINES for more)", crate::cli::colors::style::DIM));
                                        truncated = true;
                                    }
                                }
                                if truncated {
                                    // Keep tail ring buffer
                                    tail_buffer.push_back(line.clone());
                                    if tail_buffer.len() > half {
                                        tail_buffer.pop_front();
                                    }
                                }
                                stdout_collected.push_str(&line);
                                stdout_collected.push('\n');
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!("Failed to read stdout line: {}", e),
                        }
                    }
                    result = stderr_reader.next_line() => {
                        match result {
                            Ok(Some(line)) => {
                                displayed += 1;
                                if !json_output {
                                    if displayed <= half {
                                        // Head: print live
                                        eprintln!("{}", crate::cli::colors::colorize(&line, crate::cli::colors::style::YELLOW));
                                    } else if displayed == half + 1 && !truncated {
                                        // First skipped line: emit condensed note once
                                        eprintln!("{}", crate::cli::colors::colorize("... (stream condensed, set SNED_STREAM_OUTPUT_LINES for more)", crate::cli::colors::style::DIM));
                                        truncated = true;
                                    }
                                }
                                if truncated {
                                    // Keep tail ring buffer
                                    tail_buffer.push_back(line.clone());
                                    if tail_buffer.len() > half {
                                        tail_buffer.pop_front();
                                    }
                                }
                                stderr_collected.push_str(&line);
                                stderr_collected.push('\n');
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!("Failed to read stderr line: {}", e),
                        }
                    }
                    // Periodic cancellation check to allow Ctrl+C to interrupt long-running commands
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                        // Check cancellation flag using try_lock (synchronous)
                        let is_cancelled = task_state
                            .as_ref()
                            .and_then(|s| s.try_lock().ok())
                            .map(|state| state.is_cancelled_atomic.load(std::sync::atomic::Ordering::Acquire))
                            .unwrap_or(false);
                        if is_cancelled {
                            // Kill the process group on cancellation
                            #[cfg(unix)]
                            {
                                let _ = unsafe { libc::kill(-child_pid, libc::SIGTERM) };
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                let _ = unsafe { libc::kill(-child_pid, libc::SIGKILL) };
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = child.kill().await;
                            }
                            let _ = child.wait().await;
                            return Err(anyhow::anyhow!("Command cancelled by user"));
                        }
                    }
                    result = timeout(timeout_duration, child.wait()) => {
                        break match result {
                            Ok(Ok(status)) => {
                                while let Ok(Some(line)) = stdout_reader.next_line().await {
                                    displayed += 1;
                                    if !json_output {
                                        if displayed <= half {
                                            eprintln!("{}", crate::cli::colors::colorize(&line, crate::cli::colors::style::DIM));
                                        } else if displayed == half + 1 && !truncated {
                                            eprintln!("{}", crate::cli::colors::colorize("... (stream condensed, set SNED_STREAM_OUTPUT_LINES for more)", crate::cli::colors::style::DIM));
                                            truncated = true;
                                        }
                                    }
                                    if truncated {
                                        tail_buffer.push_back(line.clone());
                                        if tail_buffer.len() > half {
                                            tail_buffer.pop_front();
                                        }
                                    }
                                    stdout_collected.push_str(&line);
                                    stdout_collected.push('\n');
                                }
                                while let Ok(Some(line)) = stderr_reader.next_line().await {
                                    displayed += 1;
                                    if !json_output {
                                        if displayed <= half {
                                            eprintln!("{}", crate::cli::colors::colorize(&line, crate::cli::colors::style::YELLOW));
                                        } else if displayed == half + 1 && !truncated {
                                            eprintln!("{}", crate::cli::colors::colorize("... (stream condensed, set SNED_STREAM_OUTPUT_LINES for more)", crate::cli::colors::style::DIM));
                                            truncated = true;
                                        }
                                    }
                                    if truncated {
                                        tail_buffer.push_back(line.clone());
                                        if tail_buffer.len() > half {
                                            tail_buffer.pop_front();
                                        }
                                    }
                                    stderr_collected.push_str(&line);
                                    stderr_collected.push('\n');
                                }
                                std::process::Output {
                                    status,
                                    stdout: stdout_collected.into_bytes(),
                                    stderr: stderr_collected.into_bytes(),
                                }
                            }
                            Ok(Err(e)) => return Err(anyhow::anyhow!("Command failed: {}", e)),
                            Err(_) => {
                                // Kill the entire process group to ensure grandchildren are terminated
                                #[cfg(unix)]
                                {
                                    // Send SIGKILL to the process group (negative PID)
                                    let _ = unsafe { libc::kill(-child_pid, libc::SIGKILL) };
                                }
                                #[cfg(not(unix))]
                                {
                                    let _ = child.kill().await;
                                }
                                let _ = child.wait().await;
                                let err = crate::cli::actionable_errors::command_timeout(&cmd_str, timeout_duration.as_secs());
                                return Err(anyhow::anyhow!("{}\nStdout: {}\nStderr: {}", err.display(), stdout_collected, stderr_collected));
                            }
                        };
                    }
                }
            };

            // Print tail lines after command completes (if we truncated)
            if truncated && !tail_buffer.is_empty() && !json_output {
                let total = displayed;
                eprintln!(
                    "{}",
                    crate::cli::colors::colorize(
                        &format!("--- last {} of {} lines ---", tail_buffer.len(), total),
                        crate::cli::colors::style::DIM
                    )
                );
                for line in tail_buffer.iter() {
                    eprintln!(
                        "{}",
                        crate::cli::colors::colorize(line, crate::cli::colors::style::DIM)
                    );
                }
            }

            // Unregister PID after command completes
            #[cfg(unix)]
            if child_pid != 0
                && let Some(ref state) = task_state
            {
                let mut state = state.lock().await;
                if let Some(pos) = state
                    .running_command_pids
                    .iter()
                    .position(|&p| p == child_pid)
                {
                    state.running_command_pids.remove(pos);
                    tracing::debug!("Unregistered command PID {} after completion", child_pid);
                }
            }

            // Increment commands_executed counter for session summary
            if let Some(ref state) = task_state {
                let mut state = state.lock().await;
                state.commands_executed = state.commands_executed.saturating_add(1);
                state.last_executed_command = Some(cmd_str.clone());
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            tracing::debug!(
                command = %cmd_str,
                stdout_bytes = output.stdout.len(),
                stderr_bytes = output.stderr.len(),
                exit_code = output.status.code(),
                "command completed"
            );

            if !combined_output.is_empty() {
                combined_output.push_str("\n---\n");
            }

            if !stdout.is_empty() {
                combined_output.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !combined_output.is_empty() && !combined_output.ends_with('\n') {
                    combined_output.push('\n');
                }
                combined_output.push_str("Stderr:\n");
                combined_output.push_str(&stderr);
            }

            if !output.status.success() {
                let err = crate::cli::actionable_errors::command_exit_code(
                    &cmd_str,
                    output.status.code(),
                );
                combined_output.push_str(&format!("\n{}", err.display()));
                break;
            }
        }

        let truncated = combined_output.len() > 10 * 1024;
        tracing::info!(
            output_len = combined_output.len(),
            truncated,
            "execute_command result assembled"
        );

        if combined_output.is_empty() {
            Ok("Command executed successfully with no output.".to_string())
        } else {
            // Truncate output if it's too large (default 10KB, configurable via SNED_COMMAND_OUTPUT_LIMIT)
            let limit_bytes = std::env::var("SNED_COMMAND_OUTPUT_LIMIT")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&v| v > 0)
                .unwrap_or(10 * 1024);

            if combined_output.len() > limit_bytes {
                // Use floor_char_boundary to avoid splitting multi-byte UTF-8 characters
                let safe_end = combined_output.floor_char_boundary(limit_bytes);
                let mut truncated = combined_output[..safe_end].to_string();
                truncated.push_str("\n\n(Output truncated due to size limit.)");
                Ok(truncated)
            } else {
                Ok(combined_output)
            }
        }
    }

    /// Execute a script in a specific language.
    pub async fn execute_script(
        &self,
        script: &str,
        language: &str,
        cwd: Option<&Path>,
    ) -> anyhow::Result<String> {
        self.execute_script_with_timeout(script, language, cwd, None, false)
            .await
    }

    async fn execute_script_with_timeout(
        &self,
        script: &str,
        language: &str,
        cwd: Option<&Path>,
        timeout_override: Option<Duration>,
        explicitly_approved: bool,
    ) -> anyhow::Result<String> {
        use std::process::Stdio;
        use tokio::io::AsyncReadExt;
        use tokio::process::Command;
        use tokio::time::timeout;

        // Apply safety checker for shell scripts (bash/sh/zsh)
        // Python and Node.js scripts are not validated by the safety checker
        if !explicitly_approved
            && matches!(language, "bash" | "sh" | "zsh")
            && let Err(e) = self.safety_checker.is_safe(script)
        {
            tracing::warn!(script = %script, reason = %e, "script rejected by safety checker");
            return Err(anyhow::anyhow!("{}", e));
        }

        let (shell, args) = match language {
            "python" | "python3" => ("python3", vec!["-c", script]),
            "node" | "javascript" => ("node", vec!["-e", script]),
            "bash" | "sh" | "zsh" => ("sh", vec!["-c", script]),
            _ => {
                let err = crate::cli::actionable_errors::unsupported_language(language);
                return Err(anyhow::anyhow!("{}", err.display()));
            }
        };

        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = Command::new(shell);
            for arg in args {
                c.arg(arg);
            }
            c
        } else {
            let mut c = Command::new(shell);
            for arg in args {
                c.arg(arg);
            }
            #[cfg(unix)]
            c.process_group(0);
            c
        };

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let timeout_duration = timeout_override.unwrap_or_else(|| Self::resolve_timeout(script));
        let mut child = cmd.spawn()?;
        #[cfg(unix)]
        let child_pid = child.id().unwrap_or(0) as i32;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Script stdout was not captured"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Script stderr was not captured"))?;

        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).await.map(|_| buf)
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf).await.map(|_| buf)
        });

        let output = match timeout(timeout_duration, child.wait()).await {
            Ok(Ok(status)) => {
                let stdout = stdout_task
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to join stdout reader: {}", e))?
                    .map_err(|e| anyhow::anyhow!("Failed to read stdout: {}", e))?;
                let stderr = stderr_task
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to join stderr reader: {}", e))?
                    .map_err(|e| anyhow::anyhow!("Failed to read stderr: {}", e))?;

                std::process::Output {
                    status,
                    stdout,
                    stderr,
                }
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!("Script failed to execute: {}", e)),
            Err(_) => {
                // Kill the entire process group to ensure grandchildren are terminated
                #[cfg(unix)]
                {
                    if child_pid > 0 {
                        let _ = unsafe { libc::kill(-child_pid, libc::SIGKILL) };
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = child.kill().await;
                }
                let _ = child.wait().await;
                let stdout = stdout_task
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .map(|buf| String::from_utf8_lossy(&buf).to_string())
                    .unwrap_or_default();
                let stderr = stderr_task
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .map(|buf| String::from_utf8_lossy(&buf).to_string())
                    .unwrap_or_default();
                let err = crate::cli::actionable_errors::command_timeout(
                    script,
                    timeout_duration.as_secs(),
                );
                return Err(anyhow::anyhow!(
                    "{}\nStdout (partial): {}\nStderr (partial): {}",
                    err.display(),
                    stdout,
                    stderr,
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut combined = stdout.to_string();
        if !stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str("Stderr:\n");
            combined.push_str(&stderr);
        }

        if !output.status.success() {
            let err = crate::cli::actionable_errors::command_exit_code(
                &format!("{} script", language),
                output.status.code(),
            );
            combined.push_str(&format!("\n{}", err.display()));
        }

        Ok(combined)
    }
    pub fn new() -> Self {
        Self {
            safety_checker: CommandSafetyChecker::new(),
        }
    }

    pub fn with_yolo(mut self, yolo: bool) -> Self {
        self.safety_checker = self.safety_checker.with_yolo(yolo);
        self
    }

    pub async fn execute(
        &self,
        _state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        self.execute_without_state(None, params, None, false, None, false)
            .await
    }

    async fn execute_without_state(
        &self,
        cwd: Option<&Path>,
        params: serde_json::Value,
        _task_id: Option<&str>,
        explicitly_approved: bool,
        task_state: Option<Arc<Mutex<TaskState>>>,
        json_output: bool,
    ) -> Result<String, ToolError> {
        let commands = coerce_string_array(&params, "commands", "command");
        let commands = if commands.is_empty() {
            None
        } else {
            Some(commands)
        };

        let script = params["script"].as_str();
        let language = params["language"].as_str().unwrap_or("bash");
        let raw_output = params["raw_output"].as_bool().unwrap_or(false);

        let result = if let Some(cmds) = commands {
            self.execute_commands_with_safety(
                cmds,
                cwd,
                explicitly_approved,
                task_state,
                raw_output,
                json_output,
            )
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
        } else if let Some(s) = script {
            self.execute_script_with_timeout(s, language, cwd, None, explicitly_approved)
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
        } else {
            Err(ToolError::InvalidInput(
                "Provide exactly one of {commands, script}".to_string(),
            ))
        }?;

        Ok(result)
    }
}

#[async_trait]
impl ToolHandler for ExecuteCommandHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        self.execute_without_state(
            Some(ctx.workspace_root.as_path()),
            params,
            Some(&ctx.task_id),
            ctx.explicitly_approved,
            Some(ctx.state.clone()),
            ctx.json_output,
        )
        .await
        .map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        if let Some(cmds) = params["commands"].as_array() {
            format!("Executing {} commands", cmds.len())
        } else if let Some(lang) = params["language"].as_str() {
            format!("Executing {} script", lang)
        } else {
            "Executing command".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use crate::core::file_editor::AnchorStateManager;
    use crate::core::tools::{ToolContext, ToolHandler};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_execute_commands_success() {
        let handler = ExecuteCommandHandler::new();
        let result = handler
            .execute_commands(vec!["echo hello".to_string()], None)
            .await
            .unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn test_execute_commands_failure() {
        let handler = ExecuteCommandHandler::new();
        let result = handler
            .execute_commands(vec!["false".to_string()], None)
            .await
            .unwrap();
        assert!(result.contains("Command failed with exit code"));
    }

    #[tokio::test]
    async fn test_execute_script_python() {
        let handler = ExecuteCommandHandler::new();
        let result = handler
            .execute_script("print('hello from python')", "python3", None)
            .await
            .unwrap();
        assert!(result.contains("hello from python"));
    }

    #[tokio::test]
    async fn test_execute_commands_timeout_kills_child() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("pid.txt");
        let command = format!("echo $$ > {}; while :; do :; done", pid_file.display());

        let result = handler
            .execute_commands_with_timeout(
                vec![command],
                None,
                Some(Duration::from_millis(100)),
                false,
                None,
                false,
                false,
            )
            .await;

        let err = result.expect_err("command should time out");
        let err_text = err.to_string();
        assert!(err_text.contains("Command timed out after"));
        assert!(err_text.contains("while :; do :; done"));

        let pid_text = std::fs::read_to_string(&pid_file).unwrap();
        let pid = pid_text.trim().parse::<i32>().unwrap();

        let mut alive = false;
        for _ in 0..20 {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -0 {}", pid))
                .status()
                .unwrap();
            if !status.success() {
                alive = false;
                break;
            }
            alive = true;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(!alive, "timed-out command should be terminated");
    }

    #[tokio::test]
    async fn test_execute_script_timeout_kills_child() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("pid.txt");
        let script = format!("echo $$ > {}; while :; do :; done", pid_file.display());

        let result = handler
            .execute_script_with_timeout(
                &script,
                "bash",
                None,
                Some(Duration::from_millis(100)),
                false,
            )
            .await;

        let err = result.expect_err("script should time out");
        let err_text = err.to_string();
        assert!(
            err_text.contains("timed out after"),
            "expected timeout message, got: {}",
            err_text
        );
        assert!(err_text.contains("while :; do :; done"));

        let pid_text = std::fs::read_to_string(&pid_file).unwrap();
        let pid = pid_text.trim().parse::<i32>().unwrap();

        let mut alive = false;
        for _ in 0..20 {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -0 {}", pid))
                .status()
                .unwrap();
            if !status.success() {
                alive = false;
                break;
            }
            alive = true;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(!alive, "timed-out script should be terminated");
    }

    struct CwdGuard {
        original: std::path::PathBuf,
    }

    impl CwdGuard {
        fn set_to(path: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[tokio::test]
    async fn test_execute_uses_workspace_root_not_process_cwd() {
        let handler = ExecuteCommandHandler::new();
        let workspace_root = tempfile::tempdir().unwrap();
        let wrong_cwd = tempfile::tempdir().unwrap();
        let _guard = CwdGuard::set_to(wrong_cwd.path());

        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
        );

        let result = ToolHandler::execute(&handler, &ctx, serde_json::json!({"commands": ["pwd"]}))
            .await
            .unwrap();

        let result = result
            .as_str()
            .expect("execute_command should return a string");
        assert!(
            result.contains(workspace_root.path().to_str().unwrap()),
            "expected execute_command to run in workspace root, got: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_script_uses_workspace_root_not_process_cwd() {
        let handler = ExecuteCommandHandler::new();
        let workspace_root = tempfile::tempdir().unwrap();
        let wrong_cwd = tempfile::tempdir().unwrap();
        let _guard = CwdGuard::set_to(wrong_cwd.path());

        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
        );

        let result = ToolHandler::execute(
            &handler,
            &ctx,
            serde_json::json!({"script": "pwd", "language": "bash"}),
        )
        .await
        .unwrap();

        let result = result
            .as_str()
            .expect("execute_command should return a string");
        assert!(
            result.contains(workspace_root.path().to_str().unwrap()),
            "expected execute_command script to run in workspace root, got: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_command_timeout_no_task_leak() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("pid.txt");
        let command = format!("echo $$ > {}; sleep 10", pid_file.display());

        let initial_tasks = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();

        let result = handler
            .execute_commands_with_timeout(
                vec![command],
                None,
                Some(Duration::from_millis(100)),
                false,
                None,
                false,
                false,
            )
            .await;

        let err = result.expect_err("command should time out");
        let err_text = err.to_string();
        assert!(err_text.contains("Command timed out after"));
        assert!(err_text.contains("sleep 10"));

        tokio::time::sleep(Duration::from_millis(200)).await;

        let final_tasks = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        assert!(
            final_tasks <= initial_tasks + 1,
            "task leak: {} before, {} after",
            initial_tasks,
            final_tasks
        );

        let pid_text = std::fs::read_to_string(&pid_file).unwrap();
        let pid = pid_text.trim().parse::<i32>().unwrap();
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {}", pid))
            .status()
            .unwrap();
        assert!(!status.success(), "timed-out command should be terminated");
    }

    #[tokio::test]
    async fn test_execute_command_timeout_kills_grandchildren() {
        // Test that timeout kills not just the shell process but also any
        // grandchild processes spawned by the command (e.g., background jobs).
        #[cfg(unix)]
        {
            let handler = ExecuteCommandHandler::new().with_yolo(true);
            let temp_dir = tempfile::tempdir().unwrap();
            let grandchild_pid_file = temp_dir.path().join("grandchild_pid.txt");

            // Spawn a shell that creates a background grandchild process
            // The grandchild writes its PID to a file so we can check if it's alive
            let command = format!(
                "(sleep 300 & echo $! > {}); while :; do :; done",
                grandchild_pid_file.display()
            );

            let result = handler
                .execute_commands_with_timeout(
                    vec![command],
                    None,
                    Some(Duration::from_millis(100)),
                    false,
                    None,
                    false,
                    false,
                )
                .await;

            let err = result.expect_err("command should time out");
            let err_text = err.to_string();
            assert!(err_text.contains("Command timed out after"));

            // Give the kill signal time to propagate
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Check if grandchild is still alive
            if let Ok(grandchild_pid_text) = std::fs::read_to_string(&grandchild_pid_file) {
                let grandchild_pid = grandchild_pid_text.trim().parse::<i32>().unwrap();
                let grandchild_alive = std::process::Command::new("kill")
                    .arg("-0")
                    .arg(grandchild_pid.to_string())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);

                assert!(
                    !grandchild_alive,
                    "grandchild process (PID {}) should have been killed by timeout",
                    grandchild_pid
                );
            }
        }
    }

    #[tokio::test]
    async fn test_execute_script_timeout_kills_grandchildren() {
        // Test that script timeout kills not just the interpreter process but also
        // any grandchild processes spawned by the script (e.g., background jobs).
        #[cfg(unix)]
        {
            let handler = ExecuteCommandHandler::new().with_yolo(true);
            let temp_dir = tempfile::tempdir().unwrap();
            let grandchild_pid_file = temp_dir.path().join("grandchild_pid.txt");

            // Spawn a bash script that creates a background grandchild process
            // The grandchild writes its PID to a file so we can check if it's alive
            let script = format!(
                "(sleep 300 & echo $! > {}); while :; do :; done",
                grandchild_pid_file.display()
            );

            let result = handler
                .execute_script_with_timeout(
                    &script,
                    "bash",
                    None,
                    Some(Duration::from_millis(200)),
                    false,
                )
                .await;

            let err = result.expect_err("script should time out");
            let err_text = err.to_string();
            assert!(err_text.contains("timed out after"));

            // Give the kill signal time to propagate
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Check if grandchild is still alive
            if let Ok(grandchild_pid_text) = std::fs::read_to_string(&grandchild_pid_file) {
                let grandchild_pid = grandchild_pid_text.trim().parse::<i32>().unwrap();
                let grandchild_alive = std::process::Command::new("kill")
                    .arg("-0")
                    .arg(grandchild_pid.to_string())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);

                assert!(
                    !grandchild_alive,
                    "grandchild process (PID {}) should have been killed by timeout",
                    grandchild_pid
                );
            }
        }
    }

    #[tokio::test]
    async fn test_execute_command_missing_directory_error() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let non_existent = std::path::Path::new("/tmp/sned_test_nonexistent_dir_xyz");

        let result = handler
            .execute_commands(vec!["echo hello".to_string()], Some(non_existent))
            .await;

        let err = result.expect_err("should fail with non-existent directory");
        let err_text = err.to_string();
        assert!(
            err_text.contains("Working directory does not exist or is not a directory"),
            "expected directory error, got: {}",
            err_text
        );
        assert!(
            err_text.contains("/tmp/sned_test_nonexistent_dir_xyz"),
            "error should mention the directory path, got: {}",
            err_text
        );
    }

    #[tokio::test]
    async fn test_yolo_mode_allows_unsafe_commands() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        // gcc is not in the safe list, but should be allowed in yolo mode
        let result = handler
            .execute_commands(vec!["gcc --version".to_string()], None)
            .await;
        // Should NOT fail with safety checker error
        // gcc may not be installed, but the error should be about execution, not safety
        if let Err(e) = result {
            let err_text = e.to_string();
            // Should NOT be a safety checker error
            assert!(
                !err_text.contains("not in safe list"),
                "yolo mode should bypass safety checker, got: {}",
                err_text
            );
        }
    }

    #[tokio::test]
    async fn test_non_yolo_mode_rejects_unsafe_commands() {
        let handler = ExecuteCommandHandler::new().with_yolo(false);
        // gcc is not in the safe list
        let result = handler
            .execute_commands(vec!["gcc --version".to_string()], None)
            .await;
        // Should fail with safety checker error
        let err = result.expect_err("should fail safety check");
        let err_text = err.to_string();
        assert!(
            err_text.contains("not in safe list"),
            "expected safety checker error, got: {}",
            err_text
        );
    }

    #[test]
    fn test_stream_output_line_limit_default() {
        // Clear any cached value from previous tests
        unsafe { std::env::remove_var("SNED_STREAM_OUTPUT_LINES") };
        // Reset the OnceLock by calling the function (it will cache default)
        let limit = ExecuteCommandHandler::stream_output_line_limit();
        assert_eq!(limit, 20, "default stream limit should be 20");
    }

    #[test]
    fn test_stream_output_line_limit_env_parsing() {
        // Test the env var parsing logic (OnceLock caches first value, so we test the parsing inline)
        unsafe { std::env::set_var("SNED_STREAM_OUTPUT_LINES", "50") };
        let env_val = std::env::var("SNED_STREAM_OUTPUT_LINES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(20);
        assert_eq!(env_val, 50, "should parse valid positive integer");
        unsafe { std::env::remove_var("SNED_STREAM_OUTPUT_LINES") };
    }

    #[test]
    fn test_stream_output_line_limit_invalid_env_falls_back() {
        unsafe { std::env::set_var("SNED_STREAM_OUTPUT_LINES", "invalid") };
        let env_val = std::env::var("SNED_STREAM_OUTPUT_LINES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(20);
        assert_eq!(env_val, 20, "invalid env should fall back to default");
        unsafe { std::env::remove_var("SNED_STREAM_OUTPUT_LINES") };
    }
}
