//! Execute command tool handler for sned CLI.
//!

use crate::cli::output::OutputEvent;
use crate::core::agent_loop::TaskState;
use crate::core::approval::CommandSafetyChecker;
use crate::core::process_output::{capture_async, configured_output_limit};
use crate::core::tools::{ToolContext, ToolError, ToolHandler, coerce_string_array};
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Mutex;

const DEFAULT_COMMAND_COLLECT_LIMIT: usize = 10 * 1024 * 1024;
const DEFAULT_SCRIPT_OUTPUT_LIMIT: usize = 1024 * 1024;
const MAX_STREAM_LINE_BYTES: usize = 1024 * 1024;
const COMMAND_STREAM_READ_CHUNK_BYTES: usize = 8 * 1024;

fn command_collect_limit() -> usize {
    configured_output_limit("SNED_COMMAND_COLLECT_LIMIT", DEFAULT_COMMAND_COLLECT_LIMIT)
}

fn script_output_limit() -> usize {
    configured_output_limit("SNED_SCRIPT_OUTPUT_LIMIT", DEFAULT_SCRIPT_OUTPUT_LIMIT)
}

enum StreamLine {
    Text(String),
    Overlong,
}

impl StreamLine {
    fn into_text(self, stream: &str) -> String {
        match self {
            Self::Text(text) => text,
            Self::Overlong => format!(
                "({stream} line exceeded {} bytes and was discarded.)",
                MAX_STREAM_LINE_BYTES
            ),
        }
    }
}

struct BoundedLineReader<R> {
    reader: R,
    pending: Vec<u8>,
    line: Vec<u8>,
    discarding_line: bool,
    raw_output: bool,
}

impl<R> BoundedLineReader<R>
where
    R: tokio::io::AsyncRead + Unpin,
{
    #[cfg(test)]
    fn new(reader: R) -> Self {
        Self::with_raw_output(reader, false)
    }

    fn with_raw_output(reader: R, raw_output: bool) -> Self {
        Self {
            reader,
            pending: Vec::with_capacity(COMMAND_STREAM_READ_CHUNK_BYTES),
            line: Vec::with_capacity(COMMAND_STREAM_READ_CHUNK_BYTES),
            discarding_line: false,
            raw_output,
        }
    }

    async fn next_line(&mut self) -> std::io::Result<Option<StreamLine>> {
        use tokio::io::AsyncReadExt;

        loop {
            if self.pending.is_empty() {
                let mut chunk = [0_u8; COMMAND_STREAM_READ_CHUNK_BYTES];
                let read = self.reader.read(&mut chunk).await?;
                if read == 0 {
                    if self.discarding_line {
                        self.discarding_line = false;
                        self.line.clear();
                        return Ok(Some(StreamLine::Overlong));
                    }
                    if self.line.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(StreamLine::Text(self.take_line())));
                }
                self.pending.extend_from_slice(&chunk[..read]);
            }

            let newline = self.pending.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(self.pending.len(), |index| index + 1);
            let complete_line = newline.is_some();
            let mut fragment = self.pending.drain(..consumed).collect::<Vec<_>>();
            if complete_line {
                fragment.pop();
            }

            if !self.discarding_line {
                let remaining = MAX_STREAM_LINE_BYTES.saturating_sub(self.line.len());
                if fragment.len() > remaining {
                    self.line.extend_from_slice(&fragment[..remaining]);
                    self.discarding_line = true;
                } else {
                    self.line.extend_from_slice(&fragment);
                }
            }

            if complete_line {
                if self.discarding_line {
                    self.discarding_line = false;
                    self.line.clear();
                    return Ok(Some(StreamLine::Overlong));
                }
                return Ok(Some(StreamLine::Text(self.take_line())));
            }
        }
    }

    fn take_line(&mut self) -> String {
        if !self.raw_output && self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned()
    }
}

fn append_limited_output(
    output: &mut String,
    line: &str,
    limit: usize,
    truncated: &mut bool,
    total_bytes: &mut u64,
) {
    *total_bytes = total_bytes.saturating_add(line.len().saturating_add(1) as u64);
    if *truncated {
        return;
    }

    let available = limit.saturating_sub(output.len());
    let needed = line.len().saturating_add(1);
    if needed <= available {
        output.push_str(line);
        output.push('\n');
        return;
    }

    let end = line.floor_char_boundary(available);
    output.push_str(&line[..end]);
    *truncated = true;
}

fn append_limited_text(
    output: &mut String,
    text: &str,
    limit: usize,
    truncated: &mut bool,
    total_bytes: &mut u64,
) {
    *total_bytes = total_bytes.saturating_add(text.len() as u64);
    if *truncated {
        return;
    }

    let available = limit.saturating_sub(output.len());
    if text.len() <= available {
        output.push_str(text);
        return;
    }

    output.push_str(&text[..text.floor_char_boundary(available)]);
    *truncated = true;
}

fn finalize_collected_output(
    mut output: String,
    truncated: bool,
    stream: &str,
    total_bytes: u64,
) -> String {
    if truncated {
        output.push_str(&format!(
            "\n({stream} output truncated after retaining {} of {total_bytes} bytes.)\n",
            output.len()
        ));
    }
    output
}

#[derive(Debug, Clone)]
pub struct ExecuteCommandHandler {
    safety_checker: CommandSafetyChecker,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SandboxEnvReport {
    not_allowlisted: Vec<String>,
    sensitive: Vec<String>,
}

impl SandboxEnvReport {
    fn record(&mut self, name: String) {
        if is_secret_like(&name) {
            self.sensitive.push(name);
        } else {
            self.not_allowlisted.push(name);
        }
    }

    fn extend(&mut self, other: Self) {
        self.not_allowlisted.extend(other.not_allowlisted);
        self.sensitive.extend(other.sensitive);
    }

    fn normalize(&mut self) {
        self.not_allowlisted.sort();
        self.not_allowlisted.dedup();
        self.sensitive.sort();
        self.sensitive.dedup();
    }

    fn is_empty(&self) -> bool {
        self.not_allowlisted.is_empty() && self.sensitive.is_empty()
    }

    fn total(&self) -> usize {
        self.not_allowlisted.len() + self.sensitive.len()
    }
}

fn is_secret_like(name: &str) -> bool {
    let upper = name.to_uppercase();
    upper.ends_with("_KEY")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_PASSWORD")
        || upper.ends_with("_PASSWD")
        || upper.ends_with("_CREDENTIAL")
        || upper.ends_with("_PRIVATE_KEY")
        || matches!(upper.as_str(), "KEY" | "SECRET" | "TOKEN" | "PASSWORD")
}

fn format_sandbox_env_note(report: &SandboxEnvReport) -> String {
    // Use `Note:` prefix and a divider line so the model cannot parse
    // the sandbox diagnostic as a tool failure. The bracketed
    // `[Sandbox: ...]` framing historically tripped the model into
    // treating the diagnostic as an error and rerunning the command.
    let mut note = String::from("\n\n--- Note (informational, not a tool error) ---\n");
    note.push_str(&format!(
        "Sandbox withheld {} environment variables from this command.",
        report.total()
    ));
    if !report.not_allowlisted.is_empty() {
        note.push_str("\n  Not allowlisted: ");
        note.push_str(&report.not_allowlisted.join(", "));
    }
    if !report.sensitive.is_empty() {
        note.push_str("\n  Sensitive and always blocked: ");
        note.push_str(&report.sensitive.join(", "));
    }
    note.push_str(
        "\n  To pass non-sensitive variables, set SNED_ALLOW_ENV=VAR1,VAR2; sensitive variables remain blocked.",
    );
    note
}

fn command_output_limit() -> usize {
    std::env::var("SNED_COMMAND_OUTPUT_LIMIT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0 && v <= 1024 * 1024)
        .unwrap_or(10 * 1024)
}

fn format_bounded_sandbox_env_note(report: &SandboxEnvReport, limit_bytes: usize) -> String {
    let note = format_sandbox_env_note(report);
    if note.len() <= limit_bytes {
        return note;
    }

    let compact_note = format!(
        "\n\n--- Note (informational, not a tool error) ---\n\
Sandbox: {} environment variables withheld; diagnostic truncated by output limit. \
Increase SNED_COMMAND_OUTPUT_LIMIT to see all names.",
        report.total()
    );
    let safe_end = compact_note.floor_char_boundary(limit_bytes);
    compact_note[..safe_end].to_string()
}

fn truncate_command_output(output: String, limit_bytes: usize) -> String {
    if output.len() <= limit_bytes {
        return output;
    }

    let marker = "\n\n(Output truncated due to size limit.)";
    let safe_end = output.floor_char_boundary(limit_bytes.saturating_sub(marker.len()));
    let mut truncated = output[..safe_end].to_string();
    if truncated.len() + marker.len() <= limit_bytes {
        truncated.push_str(marker);
    }
    truncated
}

fn assemble_sandboxed_output(
    output: String,
    report: &SandboxEnvReport,
    limit_bytes: usize,
) -> String {
    if report.is_empty() {
        return truncate_command_output(output, limit_bytes);
    }

    let note = format_bounded_sandbox_env_note(report, limit_bytes);
    if note.len() >= limit_bytes {
        return note;
    }

    let output_budget = limit_bytes - note.len();
    if output.len() <= output_budget {
        let mut result = output;
        result.push_str(&note);
        return result;
    }

    let marker = "\n\n(Output truncated due to size limit.)";
    let safe_end = output.floor_char_boundary(output_budget.saturating_sub(marker.len()));
    let mut result = output[..safe_end].to_string();
    if result.len() + marker.len() <= output_budget {
        result.push_str(marker);
    }
    result.push_str(&note);
    result
}

impl Default for ExecuteCommandHandler {
    fn default() -> Self {
        Self::new()
    }
}

fn is_serialized_command_container(value: &str) -> bool {
    let value = value.trim_start();

    if let Some(rest) = value.strip_prefix('{') {
        let rest = rest.trim_start();
        let is_command_key = rest
            .strip_prefix("'command'")
            .or_else(|| rest.strip_prefix("\"command\""))
            .is_some_and(|rest| rest.trim_start().starts_with(':'));
        if is_command_key {
            return true;
        }
    }

    value
        .strip_prefix('[')
        .is_some_and(|rest| matches!(rest.trim_start().as_bytes().first(), Some(b'\'' | b'\"')))
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

    /// Opt-in cap on live-streamed command output. When unset, every
    /// line streams into the transcript so the trailing context is
    /// always visible; the 10k-line transcript eviction and scrollback
    /// handle unbounded bursts on their own. Set
    /// `SNED_STREAM_OUTPUT_LINES=N` to switch to a head+tail display
    /// capped at `N` live rows (used as an emergency valve for
    /// pathological bursts).
    ///
    /// Caches the first value so concurrent tests that mutate
    /// `SNED_STREAM_OUTPUT_LINES` do not race each other mid-stream.
    fn condensation_line_limit() -> Option<usize> {
        static LIMIT: OnceLock<Option<usize>> = OnceLock::new();
        *LIMIT.get_or_init(|| {
            std::env::var("SNED_STREAM_OUTPUT_LINES")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&v| v > 0)
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
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        // Default: apply safety checks (not explicitly approved)
        self.execute_commands_with_timeout(
            commands,
            cwd,
            None,
            false,
            None,
            None,
            false,
            false,
            &output_writer,
        )
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
        session_command_scope_approved: bool,
        task_state: Option<Arc<Mutex<TaskState>>>,
        cancellation_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
        json_output: bool,
        raw_output: bool,
        output_writer: &crate::cli::output::OutputWriterArc,
    ) -> anyhow::Result<String> {
        if session_command_scope_approved {
            let scope_params = serde_json::json!({"commands": &commands});
            if crate::core::approval::command_approval_scopes(&scope_params).is_none() {
                tracing::warn!(commands = ?commands, "command rejected by scoped approval safety checker");
                return Err(anyhow::anyhow!(
                    "command no longer qualifies for scoped approval"
                ));
            }
        }

        self.execute_commands_with_timeout(
            commands,
            cwd,
            None,
            explicitly_approved || session_command_scope_approved,
            task_state,
            cancellation_flag,
            json_output,
            raw_output,
            output_writer,
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
        cancellation_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
        json_output: bool,
        raw_output: bool,
        output_writer: &crate::cli::output::OutputWriterArc,
    ) -> anyhow::Result<String> {
        self.execute_commands_tokio(
            commands,
            cwd,
            timeout_override,
            explicitly_approved,
            task_state,
            cancellation_flag,
            json_output,
            raw_output,
            output_writer,
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
        cancellation_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
        json_output: bool,
        raw_output: bool,
        output_writer: &crate::cli::output::OutputWriterArc,
    ) -> anyhow::Result<String> {
        use std::process::Stdio;
        use tokio::process::Command;
        use tokio::time::timeout;

        let mut combined_output = String::new();
        let combined_output_limit = command_collect_limit();
        let mut combined_output_truncated = false;
        let mut combined_output_total_bytes = 0_u64;
        let mut sandbox_env_report = SandboxEnvReport::default();
        let mut command_failed = false;

        for cmd_str in commands {
            // Safety check: validate command against safe list and patterns
            // Skip safety checks for explicitly user-approved commands
            if !explicitly_approved && let Err(e) = self.safety_checker.is_safe(&cmd_str) {
                tracing::warn!(command = %cmd_str, reason = %e, "command rejected by safety checker");
                return Err(anyhow::anyhow!("{e}"));
            }
            tracing::debug!(command = %cmd_str, cwd = ?cwd, "executing command");

            if !json_output {
                use crate::cli::tui::theme::INFO_FG;
                use ratatui::style::{Modifier, Style};
                let header = Line::from(Span::styled(
                    format!("Running: {cmd_str}"),
                    Style::default().fg(INFO_FG).add_modifier(Modifier::DIM),
                ));
                output_writer.emit(OutputEvent::CommandHeaderLine(header));
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

            let (sandboxed_env, env_report) = Self::build_sandbox_env(cwd);
            cmd.env_clear().envs(sandboxed_env);
            sandbox_env_report.extend(env_report);

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

            let mut stdout_reader = BoundedLineReader::with_raw_output(stdout, raw_output);
            let mut stderr_reader = BoundedLineReader::with_raw_output(stderr, raw_output);

            let mut stdout_collected = String::new();
            let mut stderr_collected = String::new();
            let collect_limit = command_collect_limit();
            let mut stdout_collect_truncated = false;
            let mut stderr_collect_truncated = false;
            let mut stdout_total_bytes = 0_u64;
            let mut stderr_total_bytes = 0_u64;

            // Per-stream head+tail condensation.  Disabled by default (the env
            // var is opt-in) so the trailing context is always visible;
            // when enabled, each stream independently keeps a short head
            // window plus a rolling tail buffer that is flushed verbatim
            // after the child exits so no trailing context is lost.
            let stream_limit = Self::condensation_line_limit();
            let half = stream_limit.map(|limit| limit / 2).unwrap_or(0);
            let mut stdout_displayed: usize = 0;
            let mut stdout_truncated = false;
            let mut stdout_tail_buffer: VecDeque<String> = VecDeque::new();
            let mut stderr_displayed: usize = 0;
            let mut stderr_truncated = false;
            let mut stderr_tail_buffer: VecDeque<String> = VecDeque::new();

            let output = loop {
                tokio::select! {
                    result = stdout_reader.next_line() => {
                        match result {
                            Ok(Some(line)) => {
                                let line = line.into_text("stdout");
                                stdout_displayed += 1;
                                if !json_output {
                                    if stream_limit.is_none() {
                                    // No cap: stream every line.
                                    use crate::cli::output::OutputEvent;
                                    use ratatui::style::{Modifier, Style};
                                    output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                        line.clone(),
                                        Style::default().add_modifier(Modifier::DIM),
                                    ))));
                                } else if stdout_displayed <= half {
                                    // Head: print live
                                    use crate::cli::output::OutputEvent;
                                    use ratatui::style::{Modifier, Style};
                                    output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                        line.clone(),
                                        Style::default().add_modifier(Modifier::DIM),
                                    ))));
                                } else if stdout_displayed == half + 1 && !stdout_truncated {
                                    // First skipped line on this stream: emit condensed note once
                                    use ratatui::style::{Modifier, Style};
                                    output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                        "… stdout".to_string(),
                                        Style::default().add_modifier(Modifier::DIM),
                                    ))));
                                    stdout_truncated = true;
                                }
                                }
                                if stdout_truncated {
                                    // Keep tail ring buffer
                                    stdout_tail_buffer.push_back(line.clone());
                                    if stdout_tail_buffer.len() > half {
                                        stdout_tail_buffer.pop_front();
                                    }
                                }
                                append_limited_output(
                                    &mut stdout_collected,
                                    &line,
                                    collect_limit,
                                    &mut stdout_collect_truncated,
                                    &mut stdout_total_bytes,
                                );
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!("Failed to read stdout line: {}", e),
                        }
                    }
                    result = stderr_reader.next_line() => {
                        match result {
                            Ok(Some(line)) => {
                                let line = line.into_text("stderr");
                                stderr_displayed += 1;
                                if !json_output {
                                    if stream_limit.is_none() {
                                        // No cap: stream every line.
                                        use crate::cli::output::OutputEvent;
                                        use crate::cli::tui::theme::WARNING_FG;
                                        use ratatui::style::Style;
                                        output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                            line.clone(),
                                            Style::default().fg(WARNING_FG),
                                        ))));
                                    } else if stderr_displayed <= half {
                                        // Head: print live
                                        use crate::cli::output::OutputEvent;
                                        use crate::cli::tui::theme::WARNING_FG;
                                        use ratatui::style::Style;
                                        output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                            line.clone(),
                                            Style::default().fg(WARNING_FG),
                                        ))));
                                    } else if stderr_displayed == half + 1 && !stderr_truncated {
                                        // First skipped line on this stream: emit condensed note once
                                        use ratatui::style::{Modifier, Style};
                                        output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                            "… stderr".to_string(),
                                            Style::default().add_modifier(Modifier::DIM),
                                        ))));
                                        stderr_truncated = true;
                                    }
                                }
                                if stderr_truncated {
                                    // Keep tail ring buffer
                                    stderr_tail_buffer.push_back(line.clone());
                                    if stderr_tail_buffer.len() > half {
                                        stderr_tail_buffer.pop_front();
                                    }
                                }
                                append_limited_output(
                                    &mut stderr_collected,
                                    &line,
                                    collect_limit,
                                    &mut stderr_collect_truncated,
                                    &mut stderr_total_bytes,
                                );
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!("Failed to read stderr line: {}", e),
                        }
                    }
                    // Periodic cancellation check to allow Ctrl+C to interrupt long-running commands
                    () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                        // Check cancellation flag using try_lock (synchronous)
                        let is_cancelled = cancellation_flag.as_ref().is_some_and(|flag| {
                            flag.load(std::sync::atomic::Ordering::Acquire)
                        }) || cancellation_flag.is_none()
                            && task_state.as_ref().is_some_and(|s| {
                                s.try_lock().ok().is_some_and(|state| {
                                    state.is_cancelled_atomic.load(
                                        std::sync::atomic::Ordering::Acquire,
                                    )
                                })
                            });
                        if is_cancelled {
                            // Kill the process group on cancellation
                            #[cfg(unix)]
                            {
                                crate::core::cancellation::terminate_process_group(
                                    child_pid,
                                    Duration::from_millis(100),
                                )
                                .await;
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = child.kill().await;
                            }
                            let _ = child.wait().await;
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
                                    tracing::debug!(
                                        "Unregistered command PID {} after cancellation",
                                        child_pid
                                    );
                                }
                            }
                            return Err(anyhow::anyhow!("Command cancelled by user"));
                        }
                    }
                    result = timeout(timeout_duration, child.wait()) => {
                        break match result {
                            Ok(Ok(status)) => {
                                while let Ok(Some(line)) = stdout_reader.next_line().await {
                                    let line = line.into_text("stdout");
                                    stdout_displayed += 1;
                                    if !json_output {
                                        if stream_limit.is_none() {
                                            use crate::cli::output::OutputEvent;
                                            use ratatui::style::{Modifier, Style};
                                            output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                                line.clone(),
                                                Style::default().add_modifier(Modifier::DIM),
                                            ))));
                                        } else if stdout_displayed <= half {
                                            use crate::cli::output::OutputEvent;
                                            use ratatui::style::{Modifier, Style};
                                            output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                                line.clone(),
                                                Style::default().add_modifier(Modifier::DIM),
                                            ))));
                                        } else if stdout_displayed == half + 1 && !stdout_truncated {
                                            use ratatui::style::{Modifier, Style};
                                            output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                                "… stdout".to_string(),
                                                Style::default().add_modifier(Modifier::DIM),
                                            ))));
                                            stdout_truncated = true;
                                        }
                                    }
                                    if stdout_truncated {
                                        stdout_tail_buffer.push_back(line.clone());
                                        if stdout_tail_buffer.len() > half {
                                            stdout_tail_buffer.pop_front();
                                        }
                                    }
                                    append_limited_output(
                                        &mut stdout_collected,
                                        &line,
                                        collect_limit,
                                        &mut stdout_collect_truncated,
                                        &mut stdout_total_bytes,
                                    );
                                }
                                while let Ok(Some(line)) = stderr_reader.next_line().await {
                                    let line = line.into_text("stderr");
                                    stderr_displayed += 1;
                                    if !json_output {
                                        if stream_limit.is_none() {
                                            use crate::cli::output::OutputEvent;
                                            use crate::cli::tui::theme::WARNING_FG;
                                            use ratatui::style::Style;
                                            output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                                line.clone(),
                                                Style::default().fg(WARNING_FG),
                                            ))));
                                        } else if stderr_displayed <= half {
                                            use crate::cli::output::OutputEvent;
                                            use crate::cli::tui::theme::WARNING_FG;
                                            use ratatui::style::Style;
                                            output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                                line.clone(),
                                                Style::default().fg(WARNING_FG),
                                            ))));
                                        } else if stderr_displayed == half + 1 && !stderr_truncated {
                                            use ratatui::style::{Modifier, Style};
                                            output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                                                "… stderr".to_string(),
                                                Style::default().add_modifier(Modifier::DIM),
                                            ))));
                                            stderr_truncated = true;
                                        }
                                    }
                                    if stderr_truncated {
                                        stderr_tail_buffer.push_back(line.clone());
                                        if stderr_tail_buffer.len() > half {
                                            stderr_tail_buffer.pop_front();
                                        }
                                    }
                                    append_limited_output(
                                        &mut stderr_collected,
                                        &line,
                                        collect_limit,
                                        &mut stderr_collect_truncated,
                                        &mut stderr_total_bytes,
                                    );
                                }
                                std::process::Output {
                                    status,
                                    stdout: finalize_collected_output(
                                        stdout_collected,
                                        stdout_collect_truncated,
                                        "stdout",
                                        stdout_total_bytes,
                                    )
                                    .into_bytes(),
                                    stderr: finalize_collected_output(
                                        stderr_collected,
                                        stderr_collect_truncated,
                                        "stderr",
                                        stderr_total_bytes,
                                    )
                                    .into_bytes(),
                                }
                            }
                            Ok(Err(e)) => return Err(anyhow::anyhow!("Command failed: {e}")),
                            Err(_) => {
                                // Kill the entire process group to ensure grandchildren are terminated
                                #[cfg(unix)]
                                {
                                    crate::core::cancellation::terminate_process_group(
                                        child_pid,
                                        Duration::from_millis(100),
                                    )
                                    .await;
                                }
                                #[cfg(not(unix))]
                                {
                                    let _ = child.kill().await;
                                }
                                let _ = child.wait().await;

                                // The process group may have buffered output
                                // before it was killed. Drain both pipes with
                                // a bound so timeout errors retain all output
                                // that is available without hanging on a
                                // descendant that inherited a pipe.
                                let mut stdout_done = false;
                                let mut stderr_done = false;
                                let drain = async {
                                    while !stdout_done || !stderr_done {
                                        tokio::select! {
                                            result = stdout_reader.next_line(), if !stdout_done => {
                                                match result {
                                                    Ok(Some(line)) => append_limited_output(
                                                        &mut stdout_collected,
                                                        &line.into_text("stdout"),
                                                        collect_limit,
                                                        &mut stdout_collect_truncated,
                                                        &mut stdout_total_bytes,
                                                    ),
                                                    Ok(None) | Err(_) => stdout_done = true,
                                                }
                                            }
                                            result = stderr_reader.next_line(), if !stderr_done => {
                                                match result {
                                                    Ok(Some(line)) => append_limited_output(
                                                        &mut stderr_collected,
                                                        &line.into_text("stderr"),
                                                        collect_limit,
                                                        &mut stderr_collect_truncated,
                                                        &mut stderr_total_bytes,
                                                    ),
                                                    Ok(None) | Err(_) => stderr_done = true,
                                                }
                                            }
                                        }
                                    }
                                };
                                let _ = timeout(Duration::from_secs(1), drain).await;

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
                                        tracing::debug!(
                                            "Unregistered command PID {} after timeout",
                                            child_pid
                                        );
                                    }
                                }
                                let err = crate::cli::actionable_errors::command_timeout(&cmd_str, timeout_duration.as_secs());
                                return Err(anyhow::anyhow!(
                                    "{}\nStdout: {}\nStderr: {}",
                                    err.display(),
                                    finalize_collected_output(
                                        stdout_collected,
                                        stdout_collect_truncated,
                                        "stdout",
                                        stdout_total_bytes,
                                    ),
                                    finalize_collected_output(
                                        stderr_collected,
                                        stderr_collect_truncated,
                                        "stderr",
                                        stderr_total_bytes,
                                    )
                                ));
                            }
                        };
                    }
                }
            };

            // Print tail lines after command completes (if we truncated).
            // Flush each stream independently so attribution survives.
            if !json_output {
                if stdout_truncated && !stdout_tail_buffer.is_empty() {
                    use crate::cli::output::OutputEvent;
                    use ratatui::style::{Modifier, Style};
                    output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                        format!(
                            "--- stdout: last {} of {} lines ---",
                            stdout_tail_buffer.len(),
                            stdout_displayed
                        ),
                        Style::default().add_modifier(Modifier::DIM),
                    ))));
                    for line in &stdout_tail_buffer {
                        output_writer.emit(OutputEvent::CommandOutputLine(Line::from(
                            Span::styled(
                                line.clone(),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                        )));
                    }
                }
                if stderr_truncated && !stderr_tail_buffer.is_empty() {
                    use crate::cli::output::OutputEvent;
                    use ratatui::style::{Modifier, Style};
                    output_writer.emit(OutputEvent::CommandOutputLine(Line::from(Span::styled(
                        format!(
                            "--- stderr: last {} of {} lines ---",
                            stderr_tail_buffer.len(),
                            stderr_displayed
                        ),
                        Style::default().add_modifier(Modifier::DIM),
                    ))));
                    for line in &stderr_tail_buffer {
                        output_writer.emit(OutputEvent::CommandOutputLine(Line::from(
                            Span::styled(
                                line.clone(),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                        )));
                    }
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
                append_limited_text(
                    &mut combined_output,
                    "\n---\n",
                    combined_output_limit,
                    &mut combined_output_truncated,
                    &mut combined_output_total_bytes,
                );
            }

            if !stdout.is_empty() {
                append_limited_text(
                    &mut combined_output,
                    &stdout,
                    combined_output_limit,
                    &mut combined_output_truncated,
                    &mut combined_output_total_bytes,
                );
            }
            if !stderr.is_empty() {
                if !combined_output.is_empty() && !combined_output.ends_with('\n') {
                    append_limited_text(
                        &mut combined_output,
                        "\n",
                        combined_output_limit,
                        &mut combined_output_truncated,
                        &mut combined_output_total_bytes,
                    );
                }
                append_limited_text(
                    &mut combined_output,
                    "Stderr:\n",
                    combined_output_limit,
                    &mut combined_output_truncated,
                    &mut combined_output_total_bytes,
                );
                append_limited_text(
                    &mut combined_output,
                    &stderr,
                    combined_output_limit,
                    &mut combined_output_truncated,
                    &mut combined_output_total_bytes,
                );
            }

            if !output.status.success() {
                let err = crate::cli::actionable_errors::command_exit_code(
                    &cmd_str,
                    output.status.code(),
                );
                append_limited_text(
                    &mut combined_output,
                    &format!("\n{}", err.display()),
                    combined_output_limit,
                    &mut combined_output_truncated,
                    &mut combined_output_total_bytes,
                );
                command_failed = true;
                break;
            }
        }

        if combined_output.is_empty() {
            combined_output.push_str("Command executed successfully with no output.");
        }

        let combined_output = finalize_collected_output(
            combined_output,
            combined_output_truncated,
            "command result",
            combined_output_total_bytes,
        );

        sandbox_env_report.normalize();
        let limit_bytes = command_output_limit();
        let truncated = combined_output.len() > limit_bytes;
        tracing::info!(
            output_len = combined_output.len(),
            truncated,
            "execute_command result assembled"
        );

        let assembled_output =
            assemble_sandboxed_output(combined_output, &sandbox_env_report, limit_bytes);

        if command_failed {
            Err(anyhow::anyhow!(assembled_output))
        } else {
            Ok(assembled_output)
        }
    }

    /// Execute a script in a specific language.
    pub async fn execute_script(
        &self,
        script: &str,
        language: &str,
        cwd: Option<&Path>,
    ) -> anyhow::Result<String> {
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        self.execute_script_with_timeout(
            script,
            language,
            cwd,
            None,
            false,
            None,
            None,
            false,
            &output_writer,
        )
        .await
    }

    async fn execute_script_with_timeout(
        &self,
        script: &str,
        language: &str,
        cwd: Option<&Path>,
        timeout_override: Option<Duration>,
        explicitly_approved: bool,
        task_state: Option<Arc<Mutex<TaskState>>>,
        cancellation_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
        _raw_output: bool,
        output_writer: &crate::cli::output::OutputWriterArc,
    ) -> anyhow::Result<String> {
        use std::process::Stdio;
        use tokio::process::Command;
        use tokio::time::timeout;

        if !explicitly_approved {
            let check = if matches!(language, "bash" | "sh" | "zsh") {
                self.safety_checker.is_safe(script)
            } else {
                self.safety_checker.is_safe_non_shell(script)
            };
            if let Err(e) = check {
                tracing::warn!(script = %script, reason = %e, "script rejected by safety checker");
                return Err(anyhow::anyhow!("{e}"));
            }
        }

        let (shell, args) = match language {
            "python" | "python3" => ("python3", vec!["-c", script]),
            "node" | "javascript" => ("node", vec!["-e", script]),
            "bash" => ("bash", vec!["-c", script]),
            "sh" | "zsh" => ("sh", vec!["-c", script]),
            _ => {
                let err = crate::cli::actionable_errors::unsupported_language(language);
                return Err(anyhow::anyhow!("{}", err.display()));
            }
        };

        let (sandboxed_env, env_report) = Self::build_sandbox_env(cwd);
        let mut cmd = Command::new(shell);
        for arg in args {
            cmd.arg(arg);
        }
        if !cfg!(target_os = "windows") {
            #[cfg(unix)]
            cmd.process_group(0);
        }
        cmd.env_clear().envs(sandboxed_env.clone());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let timeout_duration = timeout_override.unwrap_or_else(|| Self::resolve_timeout(script));
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) if language == "bash" && error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("bash was not found on PATH; falling back to sh");
                let mut fallback = Command::new("sh");
                fallback.args(["-c", script]);
                if !cfg!(target_os = "windows") {
                    #[cfg(unix)]
                    fallback.process_group(0);
                }
                fallback.env_clear().envs(sandboxed_env);
                if let Some(dir) = cwd {
                    fallback.current_dir(dir);
                }
                fallback.stdout(Stdio::piped()).stderr(Stdio::piped());
                fallback.spawn()?
            }
            Err(error) => return Err(error.into()),
        };
        #[cfg(unix)]
        let child_pid = child.id().unwrap_or(0) as i32;

        // Scripts run in their own process group, just like command arrays.
        // Register that group so Ctrl+C can terminate a script even though the
        // agent task itself is aborted while it awaits the child.
        #[cfg(unix)]
        if child_pid != 0
            && let Some(ref state) = task_state
        {
            state.lock().await.running_command_pids.push(child_pid);
            tracing::debug!("Registered script PID {} for cancellation", child_pid);
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Script stdout was not captured"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Script stderr was not captured"))?;

        let output_limit = script_output_limit();
        let stdout_task = tokio::spawn(capture_async(stdout, output_limit));
        let stderr_task = tokio::spawn(capture_async(stderr, output_limit));

        let wait_result = timeout(timeout_duration, async {
            loop {
                tokio::select! {
                    result = child.wait() => break result.map_err(anyhow::Error::from),
                    () = tokio::time::sleep(Duration::from_millis(500)) => {
                        let is_cancelled = cancellation_flag.as_ref().is_some_and(|flag| {
                            flag.load(std::sync::atomic::Ordering::Acquire)
                        }) || cancellation_flag.is_none()
                            && task_state.as_ref().is_some_and(|s| {
                                s.try_lock().ok().is_some_and(|state| {
                                    state.is_cancelled_atomic.load(
                                        std::sync::atomic::Ordering::Acquire,
                                    )
                                })
                            });
                        if is_cancelled {
                            #[cfg(unix)]
                            if child_pid > 0 {
                                crate::core::cancellation::terminate_process_group(
                                    child_pid,
                                    Duration::from_millis(100),
                                )
                                .await;
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = child.kill().await;
                            }
                            let _ = child.wait().await;
                            Self::unregister_command_pid(&task_state, child_pid).await;
                            return Err(anyhow::anyhow!("Script cancelled by user"));
                        }
                    }
                }
            }
        })
        .await;

        let output = match wait_result {
            Ok(Ok(status)) => {
                let stdout = stdout_task
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to join stdout reader: {e}"))?
                    .map_err(|e| anyhow::anyhow!("Failed to read stdout: {e}"))?;
                let stderr = stderr_task
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to join stderr reader: {e}"))?
                    .map_err(|e| anyhow::anyhow!("Failed to read stderr: {e}"))?;

                (status, stdout, stderr)
            }
            Ok(Err(e)) => {
                if e.to_string() == "Script cancelled by user" {
                    return Err(e);
                }
                Self::unregister_command_pid(&task_state, child_pid).await;
                return Err(anyhow::anyhow!("Script failed to execute: {e}"));
            }
            Err(_) => {
                // Kill the entire process group to ensure grandchildren are terminated
                #[cfg(unix)]
                {
                    if child_pid > 0 {
                        crate::core::cancellation::terminate_process_group(
                            child_pid,
                            Duration::from_millis(100),
                        )
                        .await;
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = child.kill().await;
                }
                let _ = child.wait().await;
                Self::unregister_command_pid(&task_state, child_pid).await;
                let stdout = stdout_task
                    .await
                    .ok()
                    .and_then(std::result::Result::ok)
                    .map(|captured| captured.display(output_limit, "stdout"))
                    .unwrap_or_default();
                let stderr = stderr_task
                    .await
                    .ok()
                    .and_then(std::result::Result::ok)
                    .map(|captured| captured.display(output_limit, "stderr"))
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

        let (status, stdout, stderr) = output;
        let stdout = stdout.display(output_limit, "stdout");
        let stderr = stderr.display(output_limit, "stderr");

        let mut combined = stdout;
        if !stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str("Stderr:\n");
            combined.push_str(&stderr);
        }

        let script_failed = !status.success();
        if script_failed {
            let err = crate::cli::actionable_errors::command_exit_code(
                &format!("{language} script"),
                status.code(),
            );
            combined.push_str(&format!("\n{}", err.display()));
        }

        let combined = assemble_sandboxed_output(combined, &env_report, command_output_limit());

        Self::unregister_command_pid(&task_state, child_pid).await;

        if !combined.is_empty() {
            use crate::cli::output::OutputEvent;
            output_writer.emit(OutputEvent::RawAnsi(combined.clone()));
        }

        if script_failed {
            Err(anyhow::anyhow!(combined))
        } else {
            Ok(combined)
        }
    }

    async fn unregister_command_pid(task_state: &Option<Arc<Mutex<TaskState>>>, child_pid: i32) {
        #[cfg(unix)]
        if child_pid != 0
            && let Some(state) = task_state
        {
            let mut state = state.lock().await;
            if let Some(position) = state
                .running_command_pids
                .iter()
                .position(|pid| *pid == child_pid)
            {
                state.running_command_pids.remove(position);
            }
        }
    }
    fn build_sandbox_env(
        cwd: Option<&Path>,
    ) -> (std::collections::HashMap<String, String>, SandboxEnvReport) {
        use std::collections::HashMap;

        static BASE_ALLOWLIST: &[&str] = &[
            "PATH",
            "HOME",
            "USER",
            "LANG",
            "LC_ALL",
            "TERM",
            "TERM_PROGRAM",
            "TZ",
            "SHELL",
            "PWD",
            "TMPDIR",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "EDITOR",
            "VISUAL",
            "PAGER",
            "LESS",
            "MORE",
            "LOGNAME",
            "HOSTNAME",
        ];

        static SNED_ALLOW_ENV: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        let extra = SNED_ALLOW_ENV.get_or_init(|| {
            std::env::var("SNED_ALLOW_ENV")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| {
                    if s.is_empty() || s.starts_with("SNED_") {
                        return false;
                    }
                    if is_secret_like(s) {
                        tracing::warn!(var = %s, "SNED_ALLOW_ENV entry blocked (secret-like name ending)");
                        return false;
                    }
                    true
                })
                .collect::<Vec<_>>()
        });

        let allow_set: HashMap<&str, bool> = BASE_ALLOWLIST
            .iter()
            .copied()
            .chain(extra.iter().map(std::string::String::as_str))
            .map(|k| (k, true))
            .collect();

        let mut env = HashMap::new();
        let mut report = SandboxEnvReport::default();

        for (k, v) in std::env::vars() {
            if allow_set.contains_key(k.as_str()) && !k.starts_with("SNED_") {
                env.insert(k, v);
            } else if !k.starts_with("SNED_") {
                report.record(k);
            }
        }

        if let Some(dir) = cwd {
            env.insert("PWD".to_string(), dir.display().to_string());
        }

        report.normalize();
        (env, report)
    }

    #[must_use]
    pub fn new() -> Self {
        Self {
            safety_checker: CommandSafetyChecker::new(),
        }
    }

    #[must_use]
    pub fn with_yolo(mut self, yolo: bool) -> Self {
        self.safety_checker = self.safety_checker.with_yolo(yolo);
        self
    }

    pub async fn execute(
        &self,
        _state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        self.execute_without_state(
            None,
            params,
            None,
            false,
            false,
            None,
            None,
            false,
            &output_writer,
        )
        .await
    }

    async fn execute_without_state(
        &self,
        cwd: Option<&Path>,
        params: serde_json::Value,
        _task_id: Option<&str>,
        explicitly_approved: bool,
        session_command_scope_approved: bool,
        task_state: Option<Arc<Mutex<TaskState>>>,
        cancellation_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
        json_output: bool,
        output_writer: &crate::cli::output::OutputWriterArc,
    ) -> Result<String, ToolError> {
        if params
            .get("commands")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_serialized_command_container)
        {
            return Err(ToolError::InvalidInput(
                "`commands` must be a JSON array of command strings (e.g. `[\"ls\"]`), not a serialized Python-style list or dict string (e.g. `\"['ls']\"` or `\"{'command': 'ls'}\"`). Re-issue with the literal JSON array, not a stringified representation of one."
                    .to_string(),
            ));
        }

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
                session_command_scope_approved,
                task_state,
                cancellation_flag.clone(),
                json_output,
                raw_output,
                output_writer,
            )
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
        } else if let Some(s) = script {
            self.execute_script_with_timeout(
                s,
                language,
                cwd,
                None,
                explicitly_approved,
                task_state,
                cancellation_flag,
                raw_output,
                output_writer,
            )
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

impl ToolHandler for ExecuteCommandHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            handler
                .execute_without_state(
                    Some(ctx.workspace_root.as_path()),
                    params,
                    Some(&ctx.task_id),
                    ctx.explicitly_approved,
                    ctx.session_command_scope_approved,
                    Some(ctx.state.clone()),
                    ctx.cancellation_flag.clone(),
                    ctx.json_output,
                    &ctx.output_writer,
                )
                .await
                .map(serde_json::Value::String)
        })
    }

    fn description(&self, params: &serde_json::Value) -> String {
        if let Some(cmds) = params["commands"].as_array() {
            format!("Executing {} commands", cmds.len())
        } else if let Some(lang) = params["language"].as_str() {
            format!("Executing {lang} script")
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

    #[test]
    fn test_serialized_command_containers_are_recognized() {
        for value in [
            "{'command': \"echo invalid\"}",
            "{ \"command\" : \"echo invalid\"}",
            "['echo invalid']",
            "[ \"echo invalid\" ]",
        ] {
            assert!(
                is_serialized_command_container(value),
                "should reject serialized container: {value}"
            );
        }

        assert!(!is_serialized_command_container("echo valid"));
        assert!(!is_serialized_command_container("{ echo valid; }"));
    }

    #[tokio::test]
    async fn test_execute_rejects_serialized_command_object_without_running_it() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let workspace_root = tempfile::tempdir().unwrap();
        let marker = workspace_root.path().join("serialized-command-ran");
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        let params = serde_json::json!({
            "commands": format!("{{'command': \"touch {}\"}}", marker.display()),
        });

        let err = handler
            .execute_without_state(
                Some(workspace_root.path()),
                params,
                None,
                false,
                false,
                None,
                None,
                false,
                &output_writer,
            )
            .await
            .expect_err("serialized command object should be rejected");

        assert!(matches!(err, ToolError::InvalidInput(_)));
        let err_text = err.to_string();
        assert!(
            err_text.contains("must be a JSON array of command strings"),
            "expected shape guidance in error, got: {err_text}"
        );
        assert!(
            err_text.contains("not a serialized"),
            "expected explicit anti-pattern guidance, got: {err_text}"
        );
        assert!(!marker.exists(), "rejected command must not execute");
    }

    /// Model-facing contract: when a model accidentally sends a stringified
    /// list (e.g. `"[\"ls\"]"`) under the `commands` key, the rejection
    /// message must guide it back to the literal JSON-array form so the
    /// next turn is recoverable without re-reading the schema. Field
    /// incident: `run.json` index 56, tool id
    /// `chatcmpl-tool-9987d224ab5e0836`.
    #[tokio::test]
    async fn model_sim_serialized_commands_recovery_message_names_correct_shape() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        let params = serde_json::json!({
            "commands": "[\"python3.11 -m pytest tests/\"]"
        });

        let err = handler
            .execute_without_state(
                None,
                params,
                None,
                false,
                false,
                None,
                None,
                false,
                &output_writer,
            )
            .await
            .expect_err("serialized string form must be rejected");

        let err_text = err.to_string();
        assert!(
            err_text.contains("JSON array"),
            "error must name the correct shape, got: {err_text}"
        );
        assert!(
            err_text.contains("not a serialized"),
            "error must explicitly call out the anti-pattern, got: {err_text}"
        );
        assert!(
            err_text.contains("[\"ls\"]"),
            "error must include a correct-shape example, got: {err_text}"
        );
    }

    #[tokio::test]
    async fn test_execute_accepts_scalar_and_array_commands() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);

        for (params, expected) in [
            (serde_json::json!({"commands": "printf scalar"}), "scalar"),
            (serde_json::json!({"commands": ["printf array"]}), "array"),
        ] {
            let output = handler
                .execute_without_state(
                    None,
                    params,
                    None,
                    false,
                    false,
                    None,
                    None,
                    false,
                    &output_writer,
                )
                .await
                .expect("valid command form should execute");
            assert!(output.contains(expected), "unexpected output: {output}");
        }
    }

    #[tokio::test]
    async fn test_scoped_approval_rechecks_structural_safety() {
        let handler = ExecuteCommandHandler::new();
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);

        let result = handler
            .execute_commands_with_safety(
                vec!["cargo --version".to_string()],
                None,
                false,
                true,
                None,
                None,
                true,
                false,
                &output_writer,
            )
            .await;
        assert!(
            result.is_ok(),
            "scoped command should skip allowlist: {result:?}"
        );

        let result = handler
            .execute_commands_with_safety(
                vec!["rm -rf /tmp/sned-scoped-approval-test".to_string()],
                None,
                false,
                true,
                None,
                None,
                true,
                false,
                &output_writer,
            )
            .await;
        let error = result.expect_err("scoped approval must retain structural safety checks");
        assert!(
            error
                .to_string()
                .contains("no longer qualifies for scoped approval")
        );
    }

    #[tokio::test]
    async fn test_execute_commands_failure() {
        let handler = ExecuteCommandHandler::new();
        let result = handler
            .execute_commands(vec!["false".to_string()], None)
            .await
            .expect_err("a non-zero command exit must be a tool failure");
        assert!(result.to_string().contains("Command failed with exit code"));
    }

    #[tokio::test]
    async fn test_execute_script_failure() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let result = handler
            .execute_script("exit 7", "bash", None)
            .await
            .expect_err("a non-zero script exit must be a tool failure");
        assert!(result.to_string().contains("Command failed with exit code"));
    }

    #[tokio::test]
    async fn test_execute_script_python() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let result = handler
            .execute_script("print('hello from python')", "python3", None)
            .await
            .unwrap();
        assert!(result.contains("hello from python"));
    }

    #[tokio::test]
    async fn test_limited_reader_drains_after_reaching_its_budget() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64);
        let writer_task = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 16 * 1024]).await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let captured = capture_async(reader, 1024).await.unwrap();
        writer_task.await.unwrap();

        assert_eq!(captured.retained_len(), 1024);
        assert_eq!(captured.total_bytes(), 16 * 1024);
        assert!(captured.is_truncated(1024));
    }

    #[tokio::test]
    async fn test_line_reader_discards_a_newline_less_oversized_line() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64);
        let writer_task = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_STREAM_LINE_BYTES + 16 * 1024])
                .await
                .unwrap();
            writer.write_all(b"\nnext\n").await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let mut reader = BoundedLineReader::new(reader);
        assert!(matches!(
            reader.next_line().await.unwrap(),
            Some(StreamLine::Overlong)
        ));
        assert!(matches!(
            reader.next_line().await.unwrap(),
            Some(StreamLine::Text(line)) if line == "next"
        ));
        assert!(reader.next_line().await.unwrap().is_none());
        writer_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_line_reader_raw_output_preserves_trailing_carriage_return() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64);
        let writer_task = tokio::spawn(async move {
            writer.write_all(b"progress\r\n").await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let mut normalized_reader = BoundedLineReader::new(reader);
        let normalized = normalized_reader
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .into_text("stdout");
        assert_eq!(normalized, "progress");
        writer_task.await.unwrap();

        let (mut writer, reader) = tokio::io::duplex(64);
        let writer_task = tokio::spawn(async move {
            writer.write_all(b"progress\r\n").await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let mut raw_reader = BoundedLineReader::with_raw_output(reader, true);
        let raw = raw_reader
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .into_text("stdout");
        assert_eq!(raw, "progress\r");
        writer_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_execute_script_limits_captured_output() {
        let _guard = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let original = std::env::var_os("SNED_SCRIPT_OUTPUT_LIMIT");
        unsafe { std::env::set_var("SNED_SCRIPT_OUTPUT_LIMIT", "1024") };

        let result = ExecuteCommandHandler::new()
            .with_yolo(true)
            .execute_script("import sys; sys.stdout.write('x' * 8192)", "python3", None)
            .await;

        unsafe {
            match original {
                Some(value) => std::env::set_var("SNED_SCRIPT_OUTPUT_LIMIT", value),
                None => std::env::remove_var("SNED_SCRIPT_OUTPUT_LIMIT"),
            }
        }

        let result = result.unwrap();
        assert!(result.contains("stdout output truncated after retaining 1024 of 8192 bytes"));
    }

    #[tokio::test]
    async fn test_execute_command_limits_collected_output() {
        let _guard = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let original = std::env::var_os("SNED_COMMAND_COLLECT_LIMIT");
        unsafe { std::env::set_var("SNED_COMMAND_COLLECT_LIMIT", "1024") };

        let result = ExecuteCommandHandler::new()
            .with_yolo(true)
            .execute_commands(
                vec!["python3 -c \"print(('x' * 128 + '\\n') * 64, end='')\"".to_string()],
                None,
            )
            .await;

        unsafe {
            match original {
                Some(value) => std::env::set_var("SNED_COMMAND_COLLECT_LIMIT", value),
                None => std::env::remove_var("SNED_COMMAND_COLLECT_LIMIT"),
            }
        }

        let result = result.unwrap();
        assert!(result.contains("command result output truncated after retaining 1024 of"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_execute_bash_script_uses_bash_syntax() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let result = handler
            .execute_script("cat < <(printf 'bash works\\n')", "bash", None)
            .await
            .unwrap();
        assert!(result.contains("bash works"));
    }

    #[tokio::test]
    async fn test_execute_commands_timeout_kills_child() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("pid.txt");
        let command = format!("echo $$ > {}; while :; do :; done", pid_file.display());
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));

        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        let result = handler
            .execute_commands_with_timeout(
                vec![command],
                None,
                Some(Duration::from_millis(100)),
                false,
                Some(state.clone()),
                None,
                false,
                false,
                &output_writer,
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

        let state = state.lock().await;
        assert!(
            state.running_command_pids.is_empty(),
            "timed-out command PID should be removed from cancellation tracking"
        );
    }

    #[tokio::test]
    async fn test_execute_commands_timeout_retains_buffered_output() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);

        let result = handler
            .execute_commands_with_timeout(
                vec!["printf 'before-timeout\\n'; sleep 5".to_string()],
                None,
                Some(Duration::from_millis(100)),
                false,
                None,
                None,
                false,
                false,
                &output_writer,
            )
            .await;

        let err = result.expect_err("command should time out").to_string();
        assert!(err.contains("before-timeout"), "timeout output: {err}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_execute_script_registers_process_group_for_cancellation() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        let run_state = Arc::clone(&state);
        let run = tokio::spawn(async move {
            handler
                .execute_script_with_timeout(
                    "sleep 300",
                    "bash",
                    None,
                    Some(Duration::from_secs(10)),
                    false,
                    Some(run_state),
                    None,
                    false,
                    &output_writer,
                )
                .await
        });

        let pid = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(pid) = state.lock().await.running_command_pids.first().copied() {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("script process group should be registered");

        crate::core::cancellation::terminate_process_group(pid, Duration::from_millis(10)).await;
        let result = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("terminated script should return")
            .expect("script task should not panic");
        assert!(result.is_err());
        assert!(state.lock().await.running_command_pids.is_empty());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_execute_script_cancellation_kills_child_while_state_lock_is_held() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let cancellation_flag = state.lock().await.is_cancelled_atomic.clone();
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        let run_state = Arc::clone(&state);
        let run_flag = Arc::clone(&cancellation_flag);
        let run = tokio::spawn(async move {
            handler
                .execute_script_with_timeout(
                    "sleep 300",
                    "bash",
                    None,
                    Some(Duration::from_secs(10)),
                    false,
                    Some(run_state),
                    Some(run_flag),
                    false,
                    &output_writer,
                )
                .await
        });

        let _pid = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if state.lock().await.running_command_pids.first().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("script process group should be registered");

        let state_guard = state.lock().await;
        cancellation_flag.store(true, std::sync::atomic::Ordering::Release);
        tokio::time::sleep(Duration::from_millis(700)).await;
        drop(state_guard);

        let result = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("cancelled script should return")
            .expect("script task should not panic");
        assert!(result.is_err());
        assert!(state.lock().await.running_command_pids.is_empty());
    }

    #[tokio::test]
    async fn test_execute_script_timeout_kills_child() {
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("pid.txt");
        let script = format!("echo $$ > {}; while :; do :; done", pid_file.display());
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);

        let result = handler
            .execute_script_with_timeout(
                &script,
                "bash",
                None,
                Some(Duration::from_millis(100)),
                false,
                None,
                None,
                false,
                &output_writer,
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

    #[tokio::test]
    async fn test_execute_uses_workspace_root_not_process_cwd() {
        let _guard = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let handler = ExecuteCommandHandler::new();
        let workspace_root = tempfile::tempdir().unwrap();

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
            Arc::new(crate::cli::output::StderrOutputWriter),
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
        let _guard = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let handler = ExecuteCommandHandler::new();
        let workspace_root = tempfile::tempdir().unwrap();

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
            Arc::new(crate::cli::output::StderrOutputWriter),
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

        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        let result = handler
            .execute_commands_with_timeout(
                vec![command],
                None,
                Some(Duration::from_millis(100)),
                false,
                None,
                None,
                false,
                false,
                &output_writer,
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
            let output_writer: crate::cli::output::OutputWriterArc =
                Arc::new(crate::cli::output::StderrOutputWriter);

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
                    None,
                    false,
                    false,
                    &output_writer,
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
            let output_writer: crate::cli::output::OutputWriterArc =
                Arc::new(crate::cli::output::StderrOutputWriter);

            let result = handler
                .execute_script_with_timeout(
                    &script,
                    "bash",
                    None,
                    Some(Duration::from_millis(200)),
                    false,
                    None,
                    None,
                    false,
                    &output_writer,
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
    fn test_sandbox_allows_base_vars() {
        let (env, report) = ExecuteCommandHandler::build_sandbox_env(None);
        assert!(env.contains_key("PATH"), "PATH should be allowed");
        assert!(env.contains_key("HOME"), "HOME should be allowed");
        assert!(
            !report
                .not_allowlisted
                .iter()
                .chain(report.sensitive.iter())
                .any(|k| k == "PATH"),
            "PATH should not be in filtered list"
        );
    }

    #[test]
    fn test_sandbox_filters_sensitive_vars() {
        let (env, _report) = ExecuteCommandHandler::build_sandbox_env(None);
        for key in &[
            "API_KEY",
            "SECRET_TOKEN",
            "MY_PASSWORD",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!env.contains_key(*key), "{} should be filtered out", key);
        }
    }

    #[test]
    fn test_sandbox_silently_drops_sned_internal() {
        let (env, report) = ExecuteCommandHandler::build_sandbox_env(None);
        for key in &["SNED_PROVIDER", "SNED_API_KEY", "SNED_DIR"] {
            assert!(!env.contains_key(*key), "SNED_* internal should not leak");
            assert!(
                !report
                    .not_allowlisted
                    .iter()
                    .chain(report.sensitive.iter())
                    .any(|f| f == *key),
                "SNED_* should be silently dropped, not reported as filtered"
            );
        }
    }

    #[test]
    fn test_sandbox_env_note_lists_all_names_and_policy() {
        let mut report = SandboxEnvReport {
            not_allowlisted: vec![
                "SSH_CONNECTION".to_string(),
                "SHLVL".to_string(),
                "SSH_CONNECTION".to_string(),
            ],
            sensitive: vec![
                "SALAD_API_KEY".to_string(),
                "MINI_MAX_TOKEN_API_KEY".to_string(),
            ],
        };
        report.normalize();

        let note = format_sandbox_env_note(&report);
        assert!(
            note.contains("informational, not a tool error"),
            "sandbox note must be framed as informational so the model cannot parse it as a failure: {note}"
        );
        assert!(note.contains("Sandbox withheld 4 environment variables"));
        assert!(note.contains("Not allowlisted: SHLVL, SSH_CONNECTION"));
        assert!(
            note.contains("Sensitive and always blocked: MINI_MAX_TOKEN_API_KEY, SALAD_API_KEY")
        );
        assert!(note.contains("SNED_ALLOW_ENV=VAR1,VAR2"));
        assert!(!note.contains("e.g."));
    }

    #[test]
    fn test_sandbox_output_limit_includes_metadata() {
        let report = SandboxEnvReport {
            not_allowlisted: vec!["WORKSPACE_ID".to_string()],
            sensitive: vec!["SERVICE_API_KEY".to_string()],
        };
        let result = assemble_sandboxed_output("output".repeat(1024), &report, 512);

        assert!(result.len() <= 512);
        assert!(result.contains("WORKSPACE_ID"));
        assert!(result.contains("SERVICE_API_KEY"));
        assert!(result.contains("(Output truncated due to size limit.)"));
    }

    #[test]
    fn test_secret_like_names_are_always_blocked() {
        for name in [
            "API_KEY",
            "SECRET_TOKEN",
            "MY_PASSWORD",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(is_secret_like(name), "{name} should be sensitive");
        }
        assert!(!is_secret_like("SSH_CONNECTION"));
        assert!(!is_secret_like("SHLVL"));
    }

    #[test]
    fn test_silent_command_reports_success_before_sandbox_notification() {
        let _guard = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let env_var = "EXECUTE_COMMAND_TEST_SECRET";
        let original = std::env::var_os(env_var);
        unsafe { std::env::set_var(env_var, "filtered") };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let handler = ExecuteCommandHandler::new();
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        let result = rt.block_on(handler.execute_commands_with_timeout(
            vec!["true".to_string()],
            None,
            None,
            true,
            None,
            None,
            false,
            false,
            &output_writer,
        ));

        unsafe {
            match original {
                Some(value) => std::env::set_var(env_var, value),
                None => std::env::remove_var(env_var),
            }
        }

        let result = result.unwrap();
        assert!(
            result.starts_with("Command executed successfully with no output.\n\n--- Note"),
            "silent success should remain explicit before sandbox note, got: {result}"
        );
        assert!(
            result.contains("informational, not a tool error"),
            "sandbox note must be explicitly framed as informational so the model cannot parse it as a failure: {result}"
        );
        assert!(result.contains("EXECUTE_COMMAND_TEST_SECRET"));
        assert!(result.contains("Sensitive and always blocked"));
        assert!(!result.contains("filtered"));
    }

    #[tokio::test]
    async fn test_sandbox_note_survives_command_output_truncation() {
        let _guard = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let env_var = "EXECUTE_COMMAND_TRUNCATED_SECRET";
        let original_env = std::env::var_os(env_var);
        let original_limit = std::env::var_os("SNED_COMMAND_OUTPUT_LIMIT");
        unsafe {
            std::env::set_var(env_var, "truncated-secret-value");
            std::env::set_var("SNED_COMMAND_OUTPUT_LIMIT", "1");
        }

        let handler = ExecuteCommandHandler::new();
        let output_writer: crate::cli::output::OutputWriterArc =
            Arc::new(crate::cli::output::StderrOutputWriter);
        let result = handler
            .execute_commands_with_timeout(
                vec!["printf output".to_string()],
                None,
                None,
                true,
                None,
                None,
                false,
                false,
                &output_writer,
            )
            .await;

        unsafe {
            match original_env {
                Some(value) => std::env::set_var(env_var, value),
                None => std::env::remove_var(env_var),
            }
            match original_limit {
                Some(value) => std::env::set_var("SNED_COMMAND_OUTPUT_LIMIT", value),
                None => std::env::remove_var("SNED_COMMAND_OUTPUT_LIMIT"),
            }
        }

        let result = result.unwrap();
        assert!(result.len() <= 1);
        assert!(!result.contains("truncated-secret-value"));
    }

    #[tokio::test]
    async fn test_script_reports_sandbox_names_without_values() {
        let _guard = crate::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let env_var = "EXECUTE_SCRIPT_TEST_SECRET";
        let original = std::env::var_os(env_var);
        unsafe { std::env::set_var(env_var, "script-secret-value") };

        let result = ExecuteCommandHandler::new()
            .with_yolo(true)
            .execute_script("print('script output')", "python3", None)
            .await
            .unwrap();

        unsafe {
            match original {
                Some(value) => std::env::set_var(env_var, value),
                None => std::env::remove_var(env_var),
            }
        }

        assert!(result.contains("EXECUTE_SCRIPT_TEST_SECRET"));
        assert!(result.contains("Sensitive and always blocked"));
        assert!(!result.contains("script-secret-value"));
    }

    /// Captures `CommandOutputLine` events so a streaming condensation
    /// test can inspect what the TUI actually saw.
    #[derive(Default)]
    struct RecordingOutputWriter {
        lines: std::sync::Mutex<Vec<String>>,
    }

    impl crate::cli::output::OutputWriter for RecordingOutputWriter {
        fn emit(&self, event: crate::cli::output::OutputEvent) {
            if let crate::cli::output::OutputEvent::CommandOutputLine(line) = event {
                self.lines
                    .lock()
                    .expect("recorder mutex poisoned")
                    .push(line.to_string());
            }
        }
        fn flush(&self) {}
    }

    /// Default behaviour: stream every line. The 10k transcript eviction
    /// and scrollback handle unbounded bursts on their own.
    #[tokio::test]
    async fn test_stream_default_streams_every_line() {
        let script = "\
            for i in $(seq 1 30); do echo \"line-$i\"; done";
        let handler = ExecuteCommandHandler::new().with_yolo(true);
        let recorder = Arc::new(RecordingOutputWriter::default());
        let writer: crate::cli::output::OutputWriterArc = recorder.clone();

        let _ = handler
            .execute_commands_with_timeout(
                vec![script.to_string()],
                None,
                Some(Duration::from_secs(10)),
                false,
                None,
                None,
                false,
                false,
                &writer,
            )
            .await
            .unwrap();

        let lines: Vec<String> = recorder
            .lines
            .lock()
            .expect("recorder mutex poisoned")
            .clone();
        // Every line should reach the transcript — no condensed note,
        // no post-completion tail attribution. This test must run with
        // `SNED_STREAM_OUTPUT_LINES` unset; the harness does not mutate
        // the env var so the inherited process environment determines
        // the cap.
        let streamed: usize = lines
            .iter()
            .filter(|line| line.starts_with("line-"))
            .count();
        assert!(
            streamed >= 30,
            "default behaviour should stream every line; saw {streamed} lines of 30: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line == "… stdout"),
            "no condensed stdout note when uncapped; saw {lines:?}"
        );
    }
}
