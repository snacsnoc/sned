//! Use subagents tool handler for sned CLI.
//!
//!
//! Runs 1-5 focused subagents in parallel, each with its own prompt.
//! Each subagent gets a configured timeout (default 300s) and optional max turns.

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncRead;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

const MAX_SUBAGENT_PROMPTS: usize = 5;
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const DEFAULT_SUBAGENT_OUTPUT_LIMIT: usize = 1024 * 1024;
const MAX_SUBAGENT_LINE_BYTES: usize = 64 * 1024;
const OUTPUT_READ_CHUNK_BYTES: usize = 8 * 1024;
const COLLECTOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const SUBAGENT_CANCELLATION_POLL: Duration = Duration::from_millis(100);

enum SubagentWaitOutcome {
    Exited(Result<ExitStatus, io::Error>),
    TimedOut,
    Cancelled,
}

fn subagent_output_limit() -> usize {
    std::env::var("SNED_SUBAGENT_OUTPUT_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0 && value <= 64 * 1024 * 1024)
        .unwrap_or(DEFAULT_SUBAGENT_OUTPUT_LIMIT)
}

#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub tool_calls: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_write_tokens: u32,
    pub cache_read_tokens: u32,
    pub total_cost: f64,
    pub context_tokens: u32,
    pub context_window: u32,
    pub context_usage_pct: f64,
}

impl Default for SubagentResult {
    fn default() -> Self {
        Self {
            status: "failed".to_string(),
            result: None,
            error: Some("No result".to_string()),
            tool_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
            total_cost: 0.0,
            context_tokens: 0,
            context_window: 0,
            context_usage_pct: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UseSubagentsHandler;

impl UseSubagentsHandler {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn parse_prompts(params: &serde_json::Value) -> Vec<String> {
        let mut prompts = Vec::new();
        for i in 1..=MAX_SUBAGENT_PROMPTS {
            let key = format!("prompt_{i}");
            if let Some(p) = params.get(&key).and_then(|v| v.as_str()) {
                let trimmed = p.trim();
                if !trimmed.is_empty() {
                    prompts.push(trimmed.to_string());
                }
            }
        }
        prompts
    }

    fn parse_timeout(params: &serde_json::Value) -> u64 {
        params
            .get("timeout")
            .and_then(serde_json::Value::as_i64)
            .map_or(DEFAULT_TIMEOUT_SECS, |t| {
                if t > 0 {
                    t as u64
                } else {
                    DEFAULT_TIMEOUT_SECS
                }
            })
    }

    fn parse_max_turns(params: &serde_json::Value) -> Option<u32> {
        params
            .get("max_turns")
            .and_then(serde_json::Value::as_i64)
            .map(|t| if t > 0 { t as u32 } else { 1 })
    }

    fn append_stream_line(
        collected: &mut String,
        line: &str,
        output_limit: usize,
        truncated: &mut bool,
    ) -> bool {
        if *truncated {
            return false;
        }

        let separator = usize::from(!collected.is_empty());
        let available = output_limit.saturating_sub(collected.len());
        if separator + line.len() <= available {
            if separator > 0 {
                collected.push('\n');
            }
            collected.push_str(line);
            return true;
        }

        if separator > 0 && available > 0 {
            collected.push('\n');
        }
        let remaining = output_limit.saturating_sub(collected.len());
        collected.push_str(&line[..line.floor_char_boundary(remaining)]);
        *truncated = true;
        false
    }

    fn emit_stream_progress(
        prefix: &str,
        line: &str,
        emit_progress: bool,
        output_writer: &Option<crate::cli::output::OutputWriterArc>,
        is_stderr: bool,
    ) {
        if !emit_progress {
            return;
        }
        let Some(writer) = output_writer else {
            return;
        };

        let formatted = format!("{prefix} {line}");
        if is_stderr {
            writer.emit(crate::cli::output::OutputEvent::dim_yellow(formatted));
        } else {
            writer.emit(crate::cli::output::OutputEvent::dim(formatted));
        }
    }

    async fn collect_stream_output<R>(
        reader: R,
        prefix: String,
        emit_progress: bool,
        output_writer: Option<crate::cli::output::OutputWriterArc>,
        is_stderr: bool,
    ) -> String
    where
        R: AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;

        let mut reader = reader;
        let mut collected = String::new();
        let output_limit = subagent_output_limit();
        let mut truncated = false;
        let mut line = Vec::with_capacity(OUTPUT_READ_CHUNK_BYTES);
        let mut discarding_line = false;
        let mut chunk = [0_u8; OUTPUT_READ_CHUNK_BYTES];
        let stream_prefix = if is_stderr {
            format!("{prefix} stderr")
        } else {
            prefix
        };

        loop {
            let read = match reader.read(&mut chunk).await {
                Ok(read) => read,
                Err(error) => {
                    tracing::warn!("failed to read subagent output: {error}");
                    break;
                }
            };
            if read == 0 {
                break;
            }

            for byte in &chunk[..read] {
                if *byte == b'\n' {
                    let line_text = if discarding_line {
                        format!(
                            "(subagent output line exceeded {MAX_SUBAGENT_LINE_BYTES} bytes and was discarded.)"
                        )
                    } else {
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        String::from_utf8_lossy(&line).into_owned()
                    };
                    let retained = Self::append_stream_line(
                        &mut collected,
                        &line_text,
                        output_limit,
                        &mut truncated,
                    );
                    if retained {
                        Self::emit_stream_progress(
                            &stream_prefix,
                            &line_text,
                            emit_progress,
                            &output_writer,
                            is_stderr,
                        );
                    }
                    line.clear();
                    discarding_line = false;
                } else if !discarding_line {
                    if line.len() == MAX_SUBAGENT_LINE_BYTES {
                        line.clear();
                        discarding_line = true;
                    } else {
                        line.push(*byte);
                    }
                }
            }
        }

        if discarding_line || !line.is_empty() {
            let line_text = if discarding_line {
                format!(
                    "(subagent output line exceeded {MAX_SUBAGENT_LINE_BYTES} bytes and was discarded.)"
                )
            } else {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                String::from_utf8_lossy(&line).into_owned()
            };
            let retained =
                Self::append_stream_line(&mut collected, &line_text, output_limit, &mut truncated);
            if retained {
                Self::emit_stream_progress(
                    &stream_prefix,
                    &line_text,
                    emit_progress,
                    &output_writer,
                    is_stderr,
                );
            }
        }

        if truncated {
            collected.push_str(&format!(
                "\n(subagent output truncated at {output_limit} bytes; additional output was discarded.)"
            ));
        }

        collected
    }

    async fn wait_for_cancellation(flag: Arc<AtomicBool>) {
        while !flag.load(Ordering::Acquire) {
            tokio::time::sleep(SUBAGENT_CANCELLATION_POLL).await;
        }
    }

    async fn stop_subagent(child: &mut tokio::process::Child, child_pid: Option<i32>) {
        #[cfg(unix)]
        if let Some(child_pid) = child_pid {
            crate::core::cancellation::terminate_process_group(
                child_pid,
                std::time::Duration::from_millis(100),
            )
            .await;
        } else {
            let _ = child.kill().await;
        }
        #[cfg(not(unix))]
        {
            let _ = child.kill().await;
        }

        let _ = timeout(Duration::from_secs(1), child.wait()).await;
    }

    async fn finish_stream_collector(
        mut handle: tokio::task::JoinHandle<String>,
        stream_name: &'static str,
        subagent_index: usize,
    ) -> (String, bool) {
        match timeout(COLLECTOR_SHUTDOWN_TIMEOUT, &mut handle).await {
            Ok(Ok(output)) => (output, false),
            Ok(Err(error)) => {
                tracing::error!(
                    subagent = subagent_index + 1,
                    stream = stream_name,
                    error = %error,
                    "subagent output collector task failed"
                );
                (format!("[subagent {stream_name} collection failed: {error}]"), false)
            }
            Err(_) => {
                tracing::warn!(
                    subagent = subagent_index + 1,
                    stream = stream_name,
                    "subagent output collector did not stop after child exit"
                );
                handle.abort();
                let _ = handle.await;
                (format!("[subagent {stream_name} collection timed out]"), true)
            }
        }
    }

    async fn run_subagent(
        subagent_index: usize,
        prompt: &str,
        timeout_secs: u64,
        max_turns: Option<u32>,
        cwd: &Path,
        task_state: Option<Arc<Mutex<TaskState>>>,
        cancellation_flag: Option<Arc<AtomicBool>>,
        progress_writer: Option<crate::cli::output::OutputWriterArc>,
    ) -> SubagentResult {
        let mut cmd = Command::new("sned");
        cmd.arg("task");
        cmd.arg("--prompt");
        cmd.arg(prompt);
        cmd.arg("--is-subagent");
        cmd.current_dir(cwd);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);

        if let Some(turns) = max_turns {
            cmd.arg("--max-turns");
            cmd.arg(turns.to_string());
        }

        let emit_progress = progress_writer.is_some();
        if let Some(ref writer) = progress_writer {
            use crate::cli::output::OutputEvent;
            use crate::cli::tui::theme::INFO_FG;
            use ratatui::style::{Modifier, Style};
            writer.emit(OutputEvent::tool_output_line(
                format!("Subagent {} started", subagent_index + 1),
                Style::default().fg(INFO_FG).add_modifier(Modifier::DIM),
            ));
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Some(ref writer) = progress_writer {
                    use crate::cli::output::OutputEvent;
                    use crate::cli::tui::theme::ERROR_FG;
                    use ratatui::style::Style;
                    writer.emit(OutputEvent::tool_output_line(
                        format!("Subagent {} failed to start: {}", subagent_index + 1, e),
                        Style::default().fg(ERROR_FG),
                    ));
                }
                return SubagentResult {
                    status: "failed".to_string(),
                    error: Some(format!("spawn failed: {e}")),
                    ..Default::default()
                };
            }
        };

        #[cfg(unix)]
        let child_pid = child.id().and_then(|pid| i32::try_from(pid).ok());
        #[cfg(not(unix))]
        let child_pid = None;

        #[cfg(unix)]
        if let Some(child_pid) = child_pid
            && let Some(ref state) = task_state
        {
            let mut state = state.lock().await;
            state.running_command_pids.push(child_pid);
            tracing::debug!("Registered subagent PID {} for cancellation", child_pid);
        }

        let stdout_handle = child.stdout.take().map(|stdout| {
            let writer = progress_writer.clone();
            let prefix = format!("[subagent {}]", subagent_index + 1);
            tokio::spawn(Self::collect_stream_output(
                stdout,
                prefix,
                emit_progress,
                writer,
                false,
            ))
        });
        let stderr_handle = child.stderr.take().map(|stderr| {
            let writer = progress_writer.clone();
            let prefix = format!("[subagent {}]", subagent_index + 1);
            tokio::spawn(Self::collect_stream_output(
                stderr,
                prefix,
                emit_progress,
                writer,
                true,
            ))
        });

        let wait_result = if let Some(flag) = cancellation_flag {
            tokio::select! {
                result = timeout(Duration::from_secs(timeout_secs), child.wait()) => {
                    match result {
                        Ok(result) => SubagentWaitOutcome::Exited(result),
                        Err(_) => SubagentWaitOutcome::TimedOut,
                    }
                }
                _ = Self::wait_for_cancellation(flag) => {
                    Self::stop_subagent(&mut child, child_pid).await;
                    SubagentWaitOutcome::Cancelled
                }
            }
        } else {
            match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
                Ok(result) => SubagentWaitOutcome::Exited(result),
                Err(_) => SubagentWaitOutcome::TimedOut,
            }
        };

        if matches!(&wait_result, SubagentWaitOutcome::TimedOut) {
            Self::stop_subagent(&mut child, child_pid).await;
        }

        let (stdout_buf, stdout_stalled) = match stdout_handle {
            Some(handle) => Self::finish_stream_collector(handle, "stdout", subagent_index).await,
            None => (String::new(), false),
        };
        let (stderr_buf, stderr_stalled) = match stderr_handle {
            Some(handle) => Self::finish_stream_collector(handle, "stderr", subagent_index).await,
            None => (String::new(), false),
        };

        if stdout_stalled || stderr_stalled {
            // A descendant can keep an inherited pipe open after the direct
            // child exits. Terminate the process group before returning so
            // repeated subagent calls cannot accumulate orphaned descendants.
            Self::stop_subagent(&mut child, child_pid).await;
        }

        let result = match wait_result {
            SubagentWaitOutcome::Exited(Ok(status)) => {
                if status.success() {
                    if let Some(ref writer) = progress_writer {
                        use crate::cli::output::OutputEvent;
                        use crate::cli::tui::theme::INFO_FG;
                        use ratatui::style::{Modifier, Style};
                        writer.emit(OutputEvent::tool_output_line(
                            format!("Subagent {} completed", subagent_index + 1),
                            Style::default().fg(INFO_FG).add_modifier(Modifier::DIM),
                        ));
                    }
                    SubagentResult {
                        status: "completed".to_string(),
                        result: Some(stdout_buf.trim().to_string()),
                        error: None,
                        ..Default::default()
                    }
                } else {
                    if let Some(ref writer) = progress_writer {
                        use crate::cli::output::OutputEvent;
                        use crate::cli::tui::theme::WARNING_FG;
                        use ratatui::style::Style;
                        writer.emit(OutputEvent::tool_output_line(
                            format!("Subagent {} failed", subagent_index + 1),
                            Style::default().fg(WARNING_FG),
                        ));
                    }
                    SubagentResult {
                        status: "failed".to_string(),
                        result: None,
                        error: Some(if stderr_buf.trim().is_empty() {
                            stdout_buf.trim().to_string()
                        } else {
                            stderr_buf.trim().to_string()
                        }),
                        ..Default::default()
                    }
                }
            }
            SubagentWaitOutcome::Exited(Err(e)) => {
                Self::stop_subagent(&mut child, child_pid).await;
                if let Some(ref writer) = progress_writer {
                    use crate::cli::output::OutputEvent;
                    use crate::cli::tui::theme::ERROR_FG;
                    use ratatui::style::Style;
                    writer.emit(OutputEvent::tool_output_line(
                        format!("Subagent {} wait failed: {}", subagent_index + 1, e),
                        Style::default().fg(ERROR_FG),
                    ));
                }
                SubagentResult {
                    status: "failed".to_string(),
                    error: Some(format!("wait failed: {e}")),
                    ..Default::default()
                }
            }
            SubagentWaitOutcome::TimedOut => {
                if let Some(ref writer) = progress_writer {
                    use crate::cli::output::OutputEvent;
                    use crate::cli::tui::theme::WARNING_FG;
                    use ratatui::style::Style;
                    writer.emit(OutputEvent::tool_output_line(
                        format!(
                            "Subagent {} timed out after {} seconds",
                            subagent_index + 1,
                            timeout_secs
                        ),
                        Style::default().fg(WARNING_FG),
                    ));
                }
                SubagentResult {
                    status: "failed".to_string(),
                    error: Some(format!("Subagent timed out after {timeout_secs} seconds")),
                    ..Default::default()
                }
            }
            SubagentWaitOutcome::Cancelled => SubagentResult {
                status: "cancelled".to_string(),
                error: Some("Subagent cancelled by user".to_string()),
                ..Default::default()
            },
        };

        #[cfg(unix)]
        if let Some(child_pid) = child_pid
            && let Some(ref state) = task_state
        {
            let mut state = state.lock().await;
            if let Some(pos) = state
                .running_command_pids
                .iter()
                .position(|&p| p == child_pid)
            {
                state.running_command_pids.remove(pos);
                tracing::debug!("Unregistered subagent PID {} after completion", child_pid);
            }
        }

        result
    }

    async fn collect_subagent_results(
        handles: Vec<(usize, tokio::task::JoinHandle<SubagentResult>)>,
    ) -> Vec<(usize, SubagentResult)> {
        let mut results = Vec::with_capacity(handles.len());
        for (idx, handle) in handles {
            match handle.await {
                Ok(result) => results.push((idx, result)),
                Err(e) => results.push((
                    idx,
                    SubagentResult {
                        status: "failed".to_string(),
                        error: Some(format!("Join error: {e}")),
                        ..Default::default()
                    },
                )),
            }
        }
        results.sort_by_key(|(idx, _)| *idx);
        results
    }

    async fn execute_with_workspace_root(
        &self,
        state: Arc<Mutex<TaskState>>,
        params: serde_json::Value,
        workspace_root: &Path,
        json_output: bool,
        output_writer: &crate::cli::output::OutputWriterArc,
    ) -> Result<String, ToolError> {
        self.execute_with_workspace_root_and_cancellation(
            state,
            params,
            workspace_root,
            json_output,
            output_writer,
            None,
        )
        .await
    }

    async fn execute_with_workspace_root_and_cancellation(
        &self,
        state: Arc<Mutex<TaskState>>,
        params: serde_json::Value,
        workspace_root: &Path,
        json_output: bool,
        output_writer: &crate::cli::output::OutputWriterArc,
        cancellation_flag: Option<Arc<AtomicBool>>,
    ) -> Result<String, ToolError> {
        {
            let mut state = state.lock().await;
            if state.is_subagent_execution {
                state.consecutive_mistakes += 1;
                tracing::warn!(
                    consecutive_mistakes = state.consecutive_mistakes,
                    "use_subagents: subagent recursion detected"
                );
                return Err(ToolError::ExecutionFailed(
                    "Subagents cannot spawn other subagents.".to_string(),
                ));
            }

            let subagents_enabled = state.subagents_enabled;
            if !subagents_enabled {
                state.consecutive_mistakes += 1;
                tracing::warn!(
                    consecutive_mistakes = state.consecutive_mistakes,
                    "use_subagents: subagents are disabled"
                );
                return Err(ToolError::ExecutionFailed(
                    "Subagents are disabled. Enable them in Settings > Features to use this tool."
                        .to_string(),
                ));
            }
        }

        let prompts = Self::parse_prompts(&params);
        if prompts.is_empty() {
            let mut state = state.lock().await;
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "use_subagents: no prompts provided"
            );
            return Err(ToolError::InvalidInput(
                "Missing required parameter: at least one prompt (prompt_1) must be provided."
                    .to_string(),
            ));
        }

        if prompts.len() > MAX_SUBAGENT_PROMPTS {
            let mut state = state.lock().await;
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                prompt_count = prompts.len(),
                max_allowed = MAX_SUBAGENT_PROMPTS,
                "use_subagents: too many prompts"
            );
            return Err(ToolError::InvalidInput(format!(
                "Too many subagent prompts provided ({}). Maximum is {}.",
                prompts.len(),
                MAX_SUBAGENT_PROMPTS
            )));
        }

        let mut prompt_count_in_json = 0;
        for i in 1..=(MAX_SUBAGENT_PROMPTS + 1) {
            let key = format!("prompt_{i}");
            if params.get(&key).is_some() {
                prompt_count_in_json += 1;
            }
        }
        let timeout_secs = Self::parse_timeout(&params);
        let max_turns = Self::parse_max_turns(&params);

        let validation_error = if prompt_count_in_json > MAX_SUBAGENT_PROMPTS {
            Some((
                format!(
                    "too many prompts in JSON ({}; maximum {})",
                    prompt_count_in_json, MAX_SUBAGENT_PROMPTS
                ),
                ToolError::InvalidInput(format!(
                    "Too many subagent prompts provided ({prompt_count_in_json}). Maximum is {MAX_SUBAGENT_PROMPTS}."
                )),
            ))
        } else if timeout_secs == 0 {
            Some((
                "timeout is zero".to_string(),
                ToolError::InvalidInput("timeout must be a positive number.".to_string()),
            ))
        } else if max_turns == Some(0) {
            Some((
                "max_turns is zero".to_string(),
                ToolError::InvalidInput("max_turns must be a positive number.".to_string()),
            ))
        } else {
            None
        };

        if let Some((reason, error)) = validation_error {
            let mut state = state.lock().await;
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                reason = %reason,
                "use_subagents: invalid configuration"
            );
            return Err(error);
        }

        {
            let mut state = state.lock().await;
            state.consecutive_mistakes = 0;
        }

        let cwd = workspace_root.to_path_buf();

        if !json_output {
            use crate::cli::output::OutputEvent;
            use crate::cli::tui::theme::INFO_FG;
            use ratatui::style::{Modifier, Style};
            output_writer.emit(OutputEvent::tool_output_line(
                format!("Running {} subagent(s) in parallel...", prompts.len()),
                Style::default().fg(INFO_FG).add_modifier(Modifier::DIM),
            ));
        }

        let mut handles = Vec::new();
        let progress_writer = if json_output {
            None
        } else {
            Some(output_writer.clone())
        };
        for (i, prompt) in prompts.iter().enumerate() {
            let prompt_clone = prompt.clone();
            let cwd_clone = cwd.clone();
            let state_clone = Arc::clone(&state);
            let cancellation_flag_clone = cancellation_flag.clone();
            let progress_writer_clone = progress_writer.clone();

            handles.push((
                i,
                tokio::spawn(async move {
                    Self::run_subagent(
                        i,
                        &prompt_clone,
                        timeout_secs,
                        max_turns,
                        cwd_clone.as_path(),
                        Some(state_clone),
                        cancellation_flag_clone,
                        progress_writer_clone,
                    )
                    .await
                }),
            ));
        }

        let results = Self::collect_subagent_results(handles).await;

        let mut successes = 0usize;
        let mut failures = 0usize;
        let mut total_tool_calls = 0u32;
        let mut total_cache_writes = 0u32;
        let mut total_cache_reads = 0u32;
        let mut max_context_tokens = 0u32;
        let mut max_context_window = 0u32;
        let mut max_context_pct = 0.0f64;

        let mut summary_lines = vec![format!("Subagent results:")];
        if timeout_secs != DEFAULT_TIMEOUT_SECS {
            summary_lines.push(format!("Timeout: {timeout_secs}s"));
        }
        if let Some(turns) = max_turns {
            summary_lines.push(format!("Max turns: {turns}"));
        }
        summary_lines.push(format!("Total: {}", results.len()));
        summary_lines.push(String::new());

        for (i, result) in &results {
            let label = format!("[{}]", i + 1);
            match result.status.as_str() {
                "completed" => {
                    successes += 1;
                    if let Some(ref res) = result.result {
                        let excerpt = if res.len() > 200 {
                            let end = res.floor_char_boundary(200);
                            format!("{}...", &res[..end])
                        } else {
                            res.clone()
                        };
                        summary_lines.push(format!("{label} SUCCEEDED\n{excerpt}"));
                    } else {
                        summary_lines.push(format!("{label} SUCCEEDED (no output)"));
                    }
                    total_tool_calls = total_tool_calls.saturating_add(result.tool_calls);
                    total_cache_writes =
                        total_cache_writes.saturating_add(result.cache_write_tokens);
                    total_cache_reads = total_cache_reads.saturating_add(result.cache_read_tokens);
                    if result.context_tokens > max_context_tokens {
                        max_context_tokens = result.context_tokens;
                    }
                    if result.context_window > max_context_window {
                        max_context_window = result.context_window;
                    }
                    if result.context_usage_pct > max_context_pct {
                        max_context_pct = result.context_usage_pct;
                    }
                }
                "failed" => {
                    failures += 1;
                    let err = result.error.as_deref().unwrap_or("Unknown error");
                    let excerpt = if err.len() > 200 {
                        let end = err.floor_char_boundary(200);
                        format!("{}...", &err[..end])
                    } else {
                        err.to_string()
                    };
                    summary_lines.push(format!("{label} FAILED\n{excerpt}"));
                }
                _ => {
                    failures += 1;
                    summary_lines.push(format!("{} FAILED (status: {})", label, result.status));
                }
            }
        }

        summary_lines.push(String::new());
        summary_lines.push(format!("Summary: {successes} succeeded, {failures} failed"));

        if total_tool_calls > 0
            || total_cache_writes > 0
            || total_cache_reads > 0
            || max_context_tokens > 0
        {
            summary_lines.push(String::new());
            summary_lines.push(format!("Tool calls: {total_tool_calls}"));
            summary_lines.push(format!("Cache writes: {total_cache_writes}"));
            summary_lines.push(format!("Cache reads: {total_cache_reads}"));
            if max_context_tokens > 0 && max_context_window > 0 {
                summary_lines.push(format!(
                    "Max context: {max_context_tokens} / {max_context_window} ({max_context_pct:.1}%)"
                ));
            }
        }

        let summary = summary_lines.join("\n");

        if !json_output {
            use crate::cli::output::OutputEvent;
            use crate::cli::tui::theme::INFO_FG;
            use ratatui::style::{Modifier, Style};
            output_writer.emit(OutputEvent::tool_output_line(
                summary.clone(),
                Style::default().fg(INFO_FG).add_modifier(Modifier::DIM),
            ));
        }

        Ok(summary)
    }

    pub async fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let workspace_root = std::env::current_dir()
            .ok()
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        // For tests: create a wrapped state with only the fields we need
        let initial_state = TaskState {
            subagents_enabled: state.subagents_enabled,
            consecutive_mistakes: state.consecutive_mistakes,
            is_subagent_execution: state.is_subagent_execution,
            ..Default::default()
        };
        let state_arc: Arc<Mutex<TaskState>> = Arc::new(Mutex::new(initial_state));
        let result = self
            .execute_with_workspace_root(
                state_arc.clone(),
                params,
                workspace_root.as_path(),
                false,
                &output_writer,
            )
            .await;
        // Sync back consecutive_mistakes for tests
        let guard = state_arc.lock().await;
        state.consecutive_mistakes = guard.consecutive_mistakes;
        result
    }
}

impl ToolHandler for UseSubagentsHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            if !ctx.explicitly_approved {
                let mut state = ctx.state.lock().await;
                state.consecutive_mistakes += 1;
                tracing::warn!(
                    consecutive_mistakes = state.consecutive_mistakes,
                    "use_subagents: requires explicit user approval"
                );
                return Err(ToolError::ExecutionFailed(
                    "Subagent execution requires explicit user approval. Please approve the request."
                        .to_string(),
                ));
            }

            handler
                .execute_with_workspace_root_and_cancellation(
                    ctx.state.clone(),
                    params,
                    ctx.workspace_root.as_path(),
                    ctx.json_output,
                    &ctx.output_writer,
                    ctx.cancellation_flag.clone(),
                )
                .await
                .map(serde_json::Value::String)
        })
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let prompts = Self::parse_prompts(params);
        if prompts.is_empty() {
            "[use_subagents]".to_string()
        } else {
            let count = prompts.len();
            if count == 1 {
                "[use_subagents: 1 prompt]".to_string()
            } else {
                format!("[use_subagents: {count} prompts]")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::output::{OutputEvent, OutputWriter};
    use tokio::io::AsyncWriteExt;

    #[derive(Default)]
    struct RecordingOutputWriter {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl OutputWriter for RecordingOutputWriter {
        fn emit(&self, event: OutputEvent) {
            let text = match event {
                OutputEvent::Line(line) => line.to_string(),
                OutputEvent::ModelUpdateLine(line) => line.to_string(),
                OutputEvent::ToolOutputLine(line) => line.to_string(),
                OutputEvent::RawAnsi(text) => text,
                OutputEvent::Completion(_) => String::new(),
                OutputEvent::TurnEnd { .. } | OutputEvent::QueuedMessageStarted { .. } => {
                    return;
                }
                OutputEvent::TurnIndicator(line) => line.to_string(),
                OutputEvent::ErrorBox(msg) => msg,
                OutputEvent::ToolHeaderLine(line) => line.to_string(),
                OutputEvent::CommandHeaderLine(line) => line.to_string(),
                OutputEvent::CommandOutputLine(line) => line.to_string(),
                OutputEvent::ReasoningChunk(chunk) => chunk,
                OutputEvent::UserPromptLine(line) | OutputEvent::LocalCommandEcho(line) => {
                    line.to_string()
                }
                OutputEvent::ApprovalRequested(request) => {
                    let text = request.details().to_string();
                    request.fail("subagent output has no interactive approval UI");
                    text
                }
                OutputEvent::ApprovalFinished { .. } => String::new(),
            };
            self.events.lock().unwrap().push(text);
        }

        fn flush(&self) {}
    }

    #[test]
    fn test_parse_prompts() {
        let params = serde_json::json!({
            "prompt_1": "Fix the bug",
            "prompt_2": "Write tests",
            "prompt_3": "",
            "prompt_4": "  ",
        });
        let prompts = UseSubagentsHandler::parse_prompts(&params);
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0], "Fix the bug");
        assert_eq!(prompts[1], "Write tests");
    }

    #[test]
    fn test_parse_prompts_none() {
        let params = serde_json::json!({});
        let prompts = UseSubagentsHandler::parse_prompts(&params);
        assert!(prompts.is_empty());
    }

    #[test]
    fn test_parse_timeout_default() {
        let params = serde_json::json!({});
        let timeout = UseSubagentsHandler::parse_timeout(&params);
        assert_eq!(timeout, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn test_parse_timeout_custom() {
        let params = serde_json::json!({"timeout": 600});
        let timeout = UseSubagentsHandler::parse_timeout(&params);
        assert_eq!(timeout, 600);
    }

    #[test]
    fn test_parse_timeout_zero() {
        let params = serde_json::json!({"timeout": 0});
        let timeout = UseSubagentsHandler::parse_timeout(&params);
        assert_eq!(timeout, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn test_parse_max_turns_default() {
        let params = serde_json::json!({});
        let max_turns = UseSubagentsHandler::parse_max_turns(&params);
        assert_eq!(max_turns, None);
    }

    #[test]
    fn test_parse_max_turns_custom() {
        let params = serde_json::json!({"max_turns": 10});
        let max_turns = UseSubagentsHandler::parse_max_turns(&params);
        assert_eq!(max_turns, Some(10));
    }

    #[tokio::test]
    async fn test_collect_subagent_results_preserves_join_failure_index() {
        let handles = vec![
            (
                2,
                tokio::spawn(async {
                    SubagentResult {
                        status: "completed".to_string(),
                        result: Some("third".to_string()),
                        ..Default::default()
                    }
                }),
            ),
            (
                0,
                tokio::spawn(async {
                    panic!("subagent task panic");
                }),
            ),
            (
                1,
                tokio::spawn(async {
                    SubagentResult {
                        status: "completed".to_string(),
                        result: Some("second".to_string()),
                        ..Default::default()
                    }
                }),
            ),
        ];

        let results = UseSubagentsHandler::collect_subagent_results(handles).await;

        assert_eq!(
            results.iter().map(|(idx, _)| *idx).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(results[0].1.status, "failed");
        assert!(
            results[0]
                .1
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Join error"))
        );
    }

    #[tokio::test]
    async fn test_handler_disabled() {
        let handler = UseSubagentsHandler::new();
        let mut state = TaskState {
            subagents_enabled: false,
            ..Default::default()
        };
        let result = handler
            .execute(&mut state, serde_json::json!({"prompt_1": "Test"}))
            .await;
        assert!(result.is_err());
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_handler_missing_prompts() {
        let handler = UseSubagentsHandler::new();
        let mut state = TaskState {
            subagents_enabled: true,
            ..Default::default()
        };
        let result = handler.execute(&mut state, serde_json::json!({})).await;
        assert!(result.is_err());
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_handler_too_many_prompts() {
        let handler = UseSubagentsHandler::new();
        let mut state = TaskState {
            subagents_enabled: true,
            ..Default::default()
        };
        let params = serde_json::json!({
            "prompt_1": "One",
            "prompt_2": "Two",
            "prompt_3": "Three",
            "prompt_4": "Four",
            "prompt_5": "Five",
            "prompt_6": "Six",
        });
        let result = handler.execute(&mut state, params).await;
        assert!(result.is_err());
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_collect_stream_output_emits_progress_lines() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let recorder = Arc::new(RecordingOutputWriter::default());
        let output_writer: crate::cli::output::OutputWriterArc = recorder.clone();

        let handle = tokio::spawn(async move {
            UseSubagentsHandler::collect_stream_output(
                reader,
                "[subagent 1]".to_string(),
                true,
                Some(output_writer),
                false,
            )
            .await
        });

        writer.write_all(b"hello\nworld\n").await.unwrap();
        writer.shutdown().await.unwrap();

        let collected = handle.await.unwrap();
        assert_eq!(collected, "hello\nworld");

        let events = recorder.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.contains("[subagent 1] hello"))
        );
        assert!(
            events
                .iter()
                .any(|event| event.contains("[subagent 1] world"))
        );
    }

    #[tokio::test]
    async fn test_collect_stream_output_bounds_retained_bytes_and_drains() {
        let _guard = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let original = std::env::var_os("SNED_SUBAGENT_OUTPUT_LIMIT");
        unsafe { std::env::set_var("SNED_SUBAGENT_OUTPUT_LIMIT", "1024") };

        let (mut writer, reader) = tokio::io::duplex(64);
        let handle = tokio::spawn(async move {
            UseSubagentsHandler::collect_stream_output(
                reader,
                "[subagent 1]".to_string(),
                false,
                None,
                false,
            )
            .await
        });
        writer.write_all(&b"x\n".repeat(8 * 1024)).await.unwrap();
        writer.shutdown().await.unwrap();

        let collected = handle.await.unwrap();
        unsafe {
            match original {
                Some(value) => std::env::set_var("SNED_SUBAGENT_OUTPUT_LIMIT", value),
                None => std::env::remove_var("SNED_SUBAGENT_OUTPUT_LIMIT"),
            }
        }

        assert!(collected.contains("subagent output truncated at 1024 bytes"));
        assert!(collected.len() < 1200, "collected output was not bounded");
    }

    #[tokio::test]
    async fn test_finish_stream_collector_aborts_stalled_pipe() {
        let (_writer, reader) = tokio::io::duplex(64);
        let handle = tokio::spawn(async move {
            UseSubagentsHandler::collect_stream_output(
                reader,
                "[subagent 1]".to_string(),
                false,
                None,
                false,
            )
            .await
        });

        let (output, stalled) =
            UseSubagentsHandler::finish_stream_collector(handle, "stdout", 0).await;
        assert!(stalled);
        assert!(output.contains("stdout collection timed out"));
    }
}
