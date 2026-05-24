//! Interactive shell implementation.
//!
//! Extracted from `cli/mod.rs` — handles raw mode, terminal rendering,
//! file picker, input queuing, and agent lifecycle.

use crate::cli::{HistoryOptions, RootOnlyOptions, TaskOptions};
use crate::cli::output::{OutputEvent, OutputWriterArc, ChannelOutputWriter};
use crate::cli::tui::{App, ansi_to_ratatui_lines};
use futures::FutureExt;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, mpsc};
use unicode_width::UnicodeWidthChar;

/// Format a duration as a human-readable string (e.g., "2m 30s", "45s", "1h 15m")
fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    if total_secs >= 3600 {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        format!("{}h {}m", hours, mins)
    } else if total_secs >= 60 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", total_secs)
    }
}

// --------------------------------------------------------------------------
// Command History
// --------------------------------------------------------------------------

/// Get the path to the CLI history file (~/.sned/cli_history)
fn get_history_file_path() -> PathBuf {
    crate::storage::disk::get_sned_dir().join("cli_history")
}

/// Load command history from the history file.
/// Returns the last N lines (most recent last), where N is configurable.
fn load_command_history() -> Vec<String> {
    let history_path = get_history_file_path();
    if !history_path.exists() {
        return Vec::new();
    }

    match std::fs::read_to_string(&history_path) {
        Ok(content) => {
            // Split into lines, filter empty, take last N (configurable)
            let mut lines: Vec<String> = content
                .lines()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .collect();

            // Keep only the most recent lines
            let max = max_history_lines();
            if lines.len() > max {
                lines = lines[lines.len() - max..].to_vec();
            }

            lines
        }
        Err(_) => Vec::new(),
    }
}

/// Append a command to the history file.
/// Creates the file if it doesn't exist.
/// Trims the file to the configured max (default 10000) if it grows too large.
fn append_to_history(command: &str) {
    if command.trim().is_empty() {
        return;
    }

    let history_path = get_history_file_path();
    let default_path = PathBuf::from(".");
    let dir = history_path.parent().unwrap_or(&default_path);

    // Ensure directory exists
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("Warning: Failed to create history directory: {}", e);
        return;
    }

    // Read existing history
    let mut history = load_command_history();

    // Add new command
    history.push(command.to_string());

    // Trim to max size
    let max = max_history_lines();
    if history.len() > max {
        history = history[history.len() - max..].to_vec();
    }

    // Write back atomically
    let content = history.join("\n") + "\n";
    if let Err(e) = crate::storage::disk::atomic_write_file(&history_path, &content) {
        eprintln!("Warning: Failed to save command history: {}", e);
    }
}

/// Maximum number of history lines to keep in the history file.
/// Configurable via `SNED_HISTORY_LINES` environment variable.
fn max_history_lines() -> usize {
    std::env::var("SNED_HISTORY_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(10_000)
}

/// Delete the word before the cursor, handling multi-byte characters correctly.
/// Returns the byte index where deletion started (new cursor position).
pub(crate) fn delete_word_backward(input_buf: &mut String, cursor_pos: usize) -> usize {
    let prefix = &input_buf[..cursor_pos];
    let start = prefix
        .char_indices()
        .rev()
        .skip_while(|(_, c)| c.is_whitespace())
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    input_buf.drain(start..cursor_pos);
    start
}

/// Calculate the display width of a string, handling multi-byte characters correctly.
/// - CJK characters: 2 columns
/// - Emoji: 2 columns
/// - Combining marks: 0 columns
/// - ANSI escape sequences: 0 columns (skipped)
/// - Normal ASCII: 1 column
fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Consume entire escape sequence
            if let Some(next) = chars.next() {
                if next == '[' {
                    // CSI sequence - consume until final byte (0x40-0x7E)
                    // Handles sequences like \x1b[31;1m, \x1b[38;2;R;G;Bm, \x1b[10;20H
                    for c in chars.by_ref() {
                        if (0x40..=0x7E).contains(&(c as u32)) {
                            break;
                        }
                    }
                } else if next == ']' {
                    // OSC sequence - consume until ST (\x1b\\) or BEL (\x07)
                    // Handles sequences like \x1b]8;;url\x1b\\ (hyperlinks)
                    // or \x1b]0;title\x07 (BEL-terminated)
                    let mut prev = next;
                    for c in chars.by_ref() {
                        if c == '\x07' || (prev == '\x1b' && c == '\\') {
                            break;
                        }
                        prev = c;
                    }
                } else if next == '(' || next == ')' {
                    // Character set selection (e.g., \x1b(B for ASCII)
                    let _ = chars.next();
                }
                // Other escape sequences consume until terminator
            }
            continue;
        }
        width += UnicodeWidthChar::width(ch).unwrap_or(1);
    }

    width
}

/// Sanitize input string for single-line terminal rendering.
///
/// Replaces newlines and carriage returns with spaces to prevent
/// terminal cursor movement during rendering. This is defensive:
/// paste events already sanitize, but this catches any edge cases.
fn sanitize_input(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// Render the input line at the bottom row of the terminal.
///
/// Clears the input area, writes the prompt prefix and input buffer,
/// and positions the cursor. When `cursor_to_scroll_region` is true,
/// moves the cursor to the bottom of the scroll region after rendering
/// (so agent stderr output continues in the scroll area, not at the
/// input line).
fn render_input_line(
    stdout: &mut impl std::io::Write,
    input_buf: &str,
    cursor_pos: usize,
    prompt_prefix: &str,
    term_cols: u16,
    term_rows: u16,
    cursor_to_scroll_region: bool,
) -> std::io::Result<()> {
    let sanitized_buf = sanitize_input(input_buf);
    let full_input = format!("{}{}", prompt_prefix, &sanitized_buf);
    let input_lines = (display_width(&full_input) as u16).div_ceil(term_cols).max(1);
    let base_row = term_rows - 1;

    let mut buf = String::with_capacity(full_input.len() + 128);
    for i in 0..input_lines {
        let row = base_row.saturating_sub(i);
        buf.push_str(&format!("\x1b[{};1H\x1b[K", row + 1));
    }
    buf.push_str(&format!("\x1b[{};1H{}", base_row + 1, &full_input));
    if cursor_pos < input_buf.len() {
        let right_of_cursor = &sanitized_buf[cursor_pos..];
        let move_left = display_width(right_of_cursor);
        buf.push_str(&format!("\x1b[{}D", move_left));
    }
    if cursor_to_scroll_region {
        // Move cursor to scroll region bottom (one row above pinned input)
        // so agent stderr output scrolls there, not over the input line
        buf.push_str(&format!("\x1b[{};1H", term_rows.saturating_sub(1)));
        buf.push_str("\x1b[?25l");
    } else {
        buf.push_str("\x1b[?25h");
    }
    stdout.write_all(buf.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// Set the scroll region to rows 1 through (rows-1), pinning the bottom
/// row for the input line. Agent output scrolls within the region;
/// the input line stays fixed.
fn set_scroll_region(stdout: &mut impl std::io::Write, term_rows: u16) -> std::io::Result<()> {
    if term_rows > 2 {
        write!(stdout, "\x1b[1;{}r", term_rows.saturating_sub(1))?;
        write!(stdout, "\x1b[{};1H", term_rows.saturating_sub(1))?;
        stdout.flush()?;
    }
    Ok(())
}

/// Reset the scroll region to the full screen.
fn reset_scroll_region(stdout: &mut impl std::io::Write) -> std::io::Result<()> {
    write!(stdout, "\x1b[r")?;
    stdout.flush()?;
    Ok(())
}

/// Format context window size as human-readable string (e.g., "200K context").
fn format_context_window(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M context", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}K context", tokens / 1_000)
    } else {
        format!("{} context", tokens)
    }
}

pub struct InteractiveSession {
    agent_loop: crate::core::agent_loop::AgentLoop,
    hook_manager: Arc<crate::core::hooks::HookManager>,
    state_manager: Arc<crate::storage::state_manager::StateManager>,
    task_opts: TaskOptions,
    root_opts: RootOnlyOptions,
}

impl InteractiveSession {
    pub async fn build(task_opts: TaskOptions, root_opts: RootOnlyOptions) -> anyhow::Result<Self> {
        Self::build_with_mode(task_opts, root_opts, false).await
    }

    pub async fn build_interactive(
        task_opts: TaskOptions,
        root_opts: RootOnlyOptions,
    ) -> anyhow::Result<Self> {
        Self::build_with_mode(task_opts, root_opts, true).await
    }

    pub async fn build_with_writer(
        task_opts: TaskOptions,
        root_opts: RootOnlyOptions,
        output_writer: Option<crate::cli::output::OutputWriterArc>,
    ) -> anyhow::Result<Self> {
        Self::build_with_mode_and_writer(task_opts, root_opts, true, output_writer).await
    }

    async fn build_with_mode(
        task_opts: TaskOptions,
        root_opts: RootOnlyOptions,
        interactive_mode: bool,
    ) -> anyhow::Result<Self> {
        Self::build_with_mode_and_writer(task_opts, root_opts, interactive_mode, None).await
    }

    async fn build_with_mode_and_writer(
        task_opts: TaskOptions,
        root_opts: RootOnlyOptions,
        interactive_mode: bool,
        output_writer: Option<crate::cli::output::OutputWriterArc>,
    ) -> anyhow::Result<Self> {
        let mut components =
            crate::cli::build_task_components(task_opts.clone(), root_opts.clone(), output_writer).await?;
        components.config.interactive_mode = interactive_mode;

        let agent_loop = crate::core::agent_loop::AgentLoop::new(components.config)
            .with_system_prompt_context(components.system_prompt_context)
            .with_tools(components.registry)
            .with_task_storage(components.task_storage)
            .with_context_loader(components.context_loader)
            .with_approval_manager(components.approval_manager)
            .with_hooks(components.hook_manager.clone())
            .with_checkpoint_manager(components.checkpoint_mgr);

        crate::core::cancellation::setup_ctrl_c_handler(agent_loop.state_handle()).await;

        Ok(Self {
            agent_loop,
            hook_manager: components.hook_manager,
            state_manager: components.state_manager,
            task_opts,
            root_opts,
        })
    }

    fn queue_handle(&self) -> crate::core::agent_loop::MessageQueueHandle {
        self.agent_loop.message_queue_handle()
    }

    fn state_handle(&self) -> Arc<tokio::sync::Mutex<crate::core::agent_types::TaskState>> {
        self.agent_loop.state_handle()
    }

    async fn clear_compacted_summary(&mut self) -> bool {
        self.agent_loop.clear_compacted_summary().await
    }

    /// Get startup info line showing provider, model, task ID, mode, and context window.
    pub fn get_startup_info(&self) -> String {
        use crate::core::context::context_window::get_context_window_info;

        let provider = self.agent_loop.get_provider();
        let provider_name = provider.name();
        let model = provider.get_model();
        let model_name = self.task_opts.model.as_deref().unwrap_or(&model.id);
        let task_id = self.agent_loop.task_id();
        let mode = if self.task_opts.plan { "PLAN" } else { "ACT" };
        let context_info = get_context_window_info(provider);
        let context_window = format_context_window(context_info.context_window);

        // Use stderr-aware color functions since this is printed via eprint_info()
        format!(
            "{} {}/{} | task {} | {} mode | {}",
            crate::cli::colors::badge_stderr("sned"),
            provider_name,
            model_name,
            crate::cli::colors::colorize_stderr(task_id, crate::cli::colors::style::DIM),
            crate::cli::colors::badge_stderr(mode),
            context_window,
        )
    }

    /// Check if quiet mode is enabled (via --json flag which suppresses info output).
    pub fn is_quiet(&self) -> bool {
        self.task_opts.json
    }

    /// Print resume summary showing previous session state.
    async fn print_resume_summary(agent: &crate::core::agent_loop::AgentLoop) {
        use crate::providers::{AssistantContentBlock, MessageContent, MessageRole};

        let state_handle = agent.state_handle();
        let state = state_handle.lock().await;
        let turns_completed = state.turns_completed;
        let total_tokens = state.cumulative_tokens_in + state.cumulative_tokens_out;
        let files_tracked = state.file_context_tracker.tracked_files().len();
        drop(state);

        // Get last action from conversation history
        let last_action = {
            let history = agent.get_conversation_history().await;
            history.iter().rev().find_map(|msg| {
                if msg.role == MessageRole::Assistant {
                    match &msg.content {
                        MessageContent::AssistantBlocks(blocks) => {
                            for block in blocks {
                                if let AssistantContentBlock::ToolUse(tool) = block {
                                    return Some(format!("{} (...)", tool.name));
                                }
                            }
                            // Check for text response
                            for block in blocks {
                                if let AssistantContentBlock::Text(text) = block {
                                    let preview = text.text.chars().take(50).collect::<String>();
                                    return Some(format!("Response: {}...", preview));
                                }
                            }
                        }
                        MessageContent::Text(text) => {
                            let preview = text.chars().take(50).collect::<String>();
                            return Some(format!("Response: {}...", preview));
                        }
                        _ => {}
                    }
                }
                None
            })
        };

        eprintln!(
            "{}",
            crate::cli::colors::section_header(&format!(
                "Resumed task {} · {} turn{}",
                agent.task_id(),
                turns_completed,
                if turns_completed == 1 { "" } else { "s" }
            ))
        );

        if let Some(action) = last_action {
            eprintln!(
                "{}",
                crate::cli::colors::colorize(
                    &format!("  📌 Last action: {}", action),
                    crate::cli::colors::style::DIM
                )
            );
        }

        if files_tracked > 0 {
            eprintln!(
                "{}",
                crate::cli::colors::colorize(
                    &format!("  📁 Files changed: {}", files_tracked),
                    crate::cli::colors::style::DIM
                )
            );
        }

        eprintln!(
            "{}",
            crate::cli::colors::colorize(
                &format!("  📊 Tokens: {}", total_tokens),
                crate::cli::colors::style::DIM
            )
        );

        crate::cli::colors::print_horizontal_rule();
    }

    pub async fn run(&mut self, prompt: Option<String>) -> anyhow::Result<()> {
        tracing::debug!(target: "sned::session", "InteractiveSession::run() called, prompt={}", prompt.as_ref().map(|s| format!("{} chars", s.len())).unwrap_or("None".to_string()));
        let agent = &mut self.agent_loop;
        let state_manager = self.state_manager.clone();

        let mut initial_messages = Vec::new();

        let is_resuming = self.root_opts.continue_task || self.root_opts.task_id.is_some();
        if is_resuming {
            let loaded = agent.load_conversation_history().await;
            agent.load_file_context_tracker().await;

            // Fire TaskResume hook after loading state
            let _ = self.hook_manager.task_resume(agent.task_id());

            if loaded && !self.task_opts.json {
                Self::print_resume_summary(agent).await;
            }
        }

        if let Some(p) = prompt {
            let processed_prompt = crate::cli::slash_commands::process_slash_command(&p);
            let (clean_prompt, parsed_image_paths) =
                crate::cli::image_input::parse_images_from_input(&processed_prompt);
            let mut all_image_paths = self.task_opts.image.clone();
            for path in parsed_image_paths {
                if !all_image_paths.contains(&path) {
                    all_image_paths.push(path);
                }
            }

            let model_info = agent.get_provider().get_model().info;
            let supports_images = model_info.supports_images.unwrap_or(false);
            let image_blocks = if !all_image_paths.is_empty() && !supports_images {
                if !self.task_opts.json {
                    crate::cli::colors::eprint_warning(&format!(
                        "Model '{}' does not support images. Ignoring {} image(s).",
                        model_info.name.as_deref().unwrap_or("unknown"),
                        all_image_paths.len()
                    ));
                }
                Vec::new()
            } else {
                crate::cli::image_input::load_images_to_content_blocks(&all_image_paths)
            };

            let user_content = if image_blocks.is_empty() {
                crate::providers::MessageContent::Text(clean_prompt)
            } else {
                let mut blocks: Vec<crate::providers::UserContentBlock> = Vec::new();
                if !clean_prompt.is_empty() {
                    blocks.push(crate::providers::UserContentBlock::Text(
                        crate::providers::TextContentBlock {
                            text: clean_prompt,
                            shared: crate::providers::SharedContentFields {
                                call_id: None,
                                signature: None,
                            },
                            reasoning_details: None,
                        },
                    ));
                }
                for img_block in image_blocks {
                    blocks.push(crate::providers::UserContentBlock::Image(img_block));
                }
                crate::providers::MessageContent::UserBlocks(blocks)
            };

            initial_messages.push(crate::providers::StorageMessage {
                id: None,
                role: crate::providers::MessageRole::User,
                content: user_content,
                model_info: None,
                metrics: None,
                ts: Some(chrono::Utc::now().timestamp_millis() as u64),
            });
        }

        agent
            .run(initial_messages, state_manager)
            .await
            .map_err(|e| anyhow::anyhow!("Agent error: {}", e))?;

        if let Some(export_path) = self.task_opts.export.clone() {
            let history = agent.get_conversation_history().await;
            let mut export_data = serde_json::to_string_pretty(&history)
                .map_err(|e| anyhow::anyhow!("Failed to serialize conversation: {}", e))?;
            // Redact secrets from export
            export_data = crate::cli::redact::redact_secrets(&export_data).into_owned();
            crate::storage::disk::atomic_write_file(&export_path, &export_data)
                .map_err(|e| anyhow::anyhow!("Failed to write export file: {}", e))?;
            println!(
                "Conversation exported to: {} (secrets redacted)",
                export_path
            );
        }

        Ok(())
    }
}

pub fn query_cursor_position() -> io::Result<(u16, u16)> {
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[6n")?;
    stdout.flush()?;

    let mut stdin = io::stdin();
    let mut buf = [0u8; 32];
    let mut response = String::new();

    // Read the CPR response: ESC [ row ; col R
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            return Err(io::Error::other("no CPR response"));
        }
        response.push_str(&String::from_utf8_lossy(&buf[..n]));
        if response.contains('R') {
            break;
        }
    }

    // Parse \x1b[row;colR
    if let Some(start) = response.rfind('\x1b') {
        let seq = &response[start..];
        if seq.starts_with("\x1b[") && seq.ends_with('R') {
            let inner = &seq[2..seq.len() - 1];
            let parts: Vec<&str> = inner.split(';').collect();
            if parts.len() == 2 {
                let row = parts[0].parse().unwrap_or(1);
                let col = parts[1].parse().unwrap_or(1);
                return Ok((row, col));
            }
        }
    }

    // Default to position (1, 1) if parsing fails
    Ok((1, 1))
}

pub fn cleanup_terminal(
    raw_guard: Option<crate::terminal::input::RawModeGuard>,
) -> std::io::Result<()> {
    let mut stdout = std::io::stdout();

    reset_scroll_region(&mut stdout)?;
    let _ = stdout.write_all(b"\x1b[?2004l");
    drop(raw_guard);

    let _ = stdout.write_all(b"\x1b[?25h");
    let _ = stdout.write_all(b"\x1b[0m");
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();

    Ok(())
}

pub fn restore_raw_mode(
    raw_guard: &mut Option<crate::terminal::input::RawModeGuard>,
) -> anyhow::Result<()> {
    match crate::terminal::input::enter_raw_mode() {
        Ok(guard) => {
            *raw_guard = Some(guard);
            Ok(())
        }
        Err(e) => {
            let _ = cleanup_terminal(None);
            Err(e.into())
        }
    }
}


/// Action returned by key event handler.
enum Action {
    Quit,
    Submit(String),
    CancelAgent,
}

/// Drain output channel into app buffer.
fn drain_output(rx: &mut mpsc::UnboundedReceiver<OutputEvent>, app: &mut App) {
    while let Ok(event) = rx.try_recv() {
        match event {
            OutputEvent::Line(line) => app.push_output(line),
            OutputEvent::RawAnsi(s) => {
                let lines = ansi_to_ratatui_lines(&s);
                for line in lines {
                    app.push_output(line);
                }
            }
        }
    }
}

/// Spawn agent task with proper state management.
async fn spawn_agent_task(
    session: &Arc<Mutex<InteractiveSession>>,
    prompt: &str,
    agent_busy: &Arc<AtomicBool>,
    agent_done: &Arc<tokio::sync::Notify>,
    agent_start_time: &Arc<Mutex<Option<Instant>>>,
    agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
) -> anyhow::Result<()> {
    agent_busy.store(true, Ordering::Relaxed);
    *agent_start_time.lock().await = Some(Instant::now());
    
    let session_clone = Arc::clone(session);
    let prompt = prompt.to_string();
    let agent_busy_clone = Arc::clone(agent_busy);
    let agent_done_clone = Arc::clone(agent_done);
    
    let handle = tokio::spawn(async move {
        let mut sess = session_clone.lock().await;
        let result = sess.run(Some(prompt)).await;
        drop(sess);
        
        agent_busy_clone.store(false, Ordering::Relaxed);
        agent_done_clone.notify_one();
        
        if let Err(e) = result {
            tracing::error!("Agent task failed: {}", e);
        }
    });
    
    *agent_task.lock().await = Some(handle);
    Ok(())
}

/// Cancel running agent task.
async fn cancel_agent(
    state_handle: &Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    agent_done: &Arc<tokio::sync::Notify>,
) -> anyhow::Result<()> {
    // Set cancellation flag
    if let Some(sh) = state_handle.lock().await.as_ref() {
        let mut state = sh.lock().await;
        state.is_cancelled = true;
        state.is_cancelled_atomic.store(true, Ordering::Release);
        
        // Kill running PIDs
        #[cfg(unix)]
        for &pid in &state.running_command_pids.clone() {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
        }
        state.running_command_pids.clear();
    }
    
    // Abort agent task
    if let Some(task) = agent_task.lock().await.take() {
        task.abort();
        // Wait briefly for cleanup
        tokio::time::timeout(Duration::from_secs(2), async {
            agent_done.notified().await
        }).await.ok();
    }
    
    Ok(())
}

/// Handle key events in ratatui loop.
async fn handle_key_event(
    key: KeyEvent,
    app: &mut App,
    session: &Arc<Mutex<InteractiveSession>>,
    task_id: &str,
    agent_busy: &AtomicBool,
    agent_done: &Arc<tokio::sync::Notify>,
    agent_start_time: &Arc<Mutex<Option<Instant>>>,
    state_handle: &Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
) -> anyhow::Result<Option<Action>> {
    use crate::core::approval::{is_followup_question_active, take_followup_sender};
    use ratatui::widgets::Block;
    
    // Ctrl+C handling
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if agent_busy.load(Ordering::Relaxed) {
            // Cancel agent
            cancel_agent(state_handle, agent_task, agent_done).await?;
            return Ok(Some(Action::CancelAgent));
        } else if app.input.lines().join("\n").is_empty() {
            // Exit cleanly
            return Ok(Some(Action::Quit));
        } else {
            // Clear input - create new textarea with empty lines
            let title = if agent_busy.load(Ordering::Relaxed) {
                format!("{} Working...", app.spinner_char())
            } else {
                "❯".to_string()
            };
            let mut new_input = tui_textarea::TextArea::new(Vec::new());
            new_input.set_block(Block::bordered().title(title));
            app.input = new_input;
            return Ok(None);
        }
    }
    
    // Enter key - intercept before passing to textarea
    if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
        // Check for followup question
        if is_followup_question_active(task_id) {
            if let Some(sender) = take_followup_sender(task_id) {
                let text = app.input.lines().join("");
                let _ = sender.send(text);
                let mut new_input = tui_textarea::TextArea::new(Vec::new());
                new_input.set_block(Block::bordered().title("❯"));
                app.input = new_input;
            }
            return Ok(None);
        }
        
        // Normal submit
        let text = app.input.lines().join("");
        if !text.is_empty() {
            // Echo prompt to output pane
            app.push_styled(
                format!("❯ {}", text),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            );
            // Clear textarea - create new one with empty lines
            let mut new_input = tui_textarea::TextArea::new(Vec::new());
            new_input.set_block(Block::bordered().title("❯"));
            app.input = new_input;
            // Submit to agent
            return Ok(Some(Action::Submit(text)));
        }
        return Ok(None);
    }
    
    // Shift+Up/Down for manual scroll
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        if key.code == KeyCode::Up {
            app.auto_scroll = false;
            app.scroll_offset = app.scroll_offset.saturating_sub(1);
            return Ok(None);
        }
        if key.code == KeyCode::Down {
            app.scroll_offset = app.scroll_offset.saturating_add(1);
            // Re-enable auto-scroll if scrolled to bottom
            app.auto_scroll = true;
            return Ok(None);
        }
    }
    
    // All other keys go to textarea
    use tui_textarea::Input;
    app.input.input(Input::from(key));
    Ok(None)
}

/// Main ratatui event loop.
async fn run_main_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    output_rx: &mut mpsc::UnboundedReceiver<OutputEvent>,
    session: Arc<Mutex<InteractiveSession>>,
    task_id: String,
    agent_busy: Arc<AtomicBool>,
    agent_done: Arc<tokio::sync::Notify>,
    agent_start_time: Arc<Mutex<Option<Instant>>>,
    state_handle: Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    agent_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
) -> anyhow::Result<()> {
    loop {
        // 1. Drain channel into app
        drain_output(output_rx, app);
        
        // 2. Render
        terminal.draw(|f| app.render(f))?;
        
        // 3. Poll for events (blocking, 50ms timeout)
        if ratatui::crossterm::event::poll(Duration::from_millis(50))? {
            match ratatui::crossterm::event::read()? {
                Event::Key(key) => {
                    if let Some(action) = handle_key_event(
                        key,
                        app,
                        &session,
                        &task_id,
                        &agent_busy,
                        &agent_done,
                        &agent_start_time,
                        &state_handle,
                        &agent_task,
                    ).await? {
                        match action {
                            Action::Quit => return Ok(()),
                            Action::Submit(text) => {
                                spawn_agent_task(
                                    &session,
                                    &text,
                                    &agent_busy,
                                    &agent_done,
                                    &agent_start_time,
                                    &agent_task,
                                ).await?;
                            }
                            Action::CancelAgent => {
                                app.push_plain("^C");
                            }
                        }
                    }
                }
                Event::Resize(_, _) => {
                    // Ratatui handles resize automatically on next draw
                }
                _ => {}
            }
        }
        
        // 4. Check agent completion (non-blocking)
        if agent_busy.load(Ordering::Relaxed) {
            if agent_done.notified().now_or_never().is_some() {
                agent_busy.store(false, Ordering::Relaxed);
                if let Some(start) = agent_start_time.lock().await.take() {
                    let elapsed = start.elapsed();
                    app.push_styled(
                        format!("⏱ Elapsed: {}", format_duration(elapsed)),
                        Style::default().add_modifier(Modifier::DIM),
                    );
                }
            }
        }
        
        // 5. Tick spinner
        app.tick_spinner();
    }
}

pub async fn run_interactive_shell_inner(
    task_opts: TaskOptions,
    root_opts: RootOnlyOptions,
) -> anyhow::Result<()> {
    use crate::core::file_search::{extract_mention_query, insert_mention, search_workspace_files};
    use crate::terminal::input::{InputParser, TerminalEvent, enter_raw_mode, install_panic_hook};
    use crate::terminal::picker::FilePicker;
    use std::sync::atomic::{AtomicBool, Ordering};

    install_panic_hook();

    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().into_owned();

    let mut input_buf = String::new();
    let mut cursor_pos: usize = 0;
    let mut parser = InputParser::new();
    let mut picker_active = false;
    let mut picker = FilePicker::new(13, 80);

    let mut raw_guard = Some(enter_raw_mode()?);

    let mut stdout = io::stdout();

    // Enable bracketed paste mode so pasted multi-line text is treated as a single input.
    // Flush immediately to ensure the terminal processes this before we start reading.
    write!(stdout, "\x1b[?2004h")?;
    stdout.flush()?;

    // Set scroll region: rows 1..(rows-1) scroll, bottom row is pinned for input.
    let (_init_cols, init_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    set_scroll_region(&mut stdout, init_rows)?;

    // Give the terminal a moment to process the bracketed paste enable sequence.
    // Without this, rapid pastes immediately after startup might arrive before the
    // terminal has processed the escape sequence.
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let mut input_row: u16 = 0;
    let mut last_picker_row: Option<usize> = None;
    let mut last_picker_height: usize = 0;
    let mut stdin = tokio::io::stdin();
    let mut byte_buf = [0u8; 4096];

    let agent_busy = Arc::new(AtomicBool::new(false));
    let agent_done = Arc::new(tokio::sync::Notify::new());
    let agent_start_time: Arc<tokio::sync::Mutex<Option<std::time::Instant>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let queue_handle: Arc<tokio::sync::Mutex<Option<crate::core::agent_loop::MessageQueueHandle>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let state_handle: Arc<
        tokio::sync::Mutex<Option<Arc<tokio::sync::Mutex<crate::core::agent_types::TaskState>>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));
    let agent_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let auto_approve = task_opts.yolo || task_opts.auto_approve_all;
    
    // Phase 1: Create output channel and drain to stderr
    let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel();
    let output_writer: crate::cli::output::OutputWriterArc =
        Arc::new(crate::cli::output::ChannelOutputWriter::new(output_tx));
    
    // Spawn drain task (Phase 1 only - forwards to stderr)
    tokio::spawn(async move {
        use crate::cli::output::OutputEvent;
        while let Some(event) = output_rx.recv().await {
            match event {
                OutputEvent::Line(line) => eprintln!("{}", line),
                OutputEvent::RawAnsi(s) => eprint!("{}", s),
            }
        }
    });
    
    let session = Arc::new(tokio::sync::Mutex::new(
        InteractiveSession::build_with_writer(task_opts.clone(), root_opts.clone(), Some(output_writer.clone())).await?,
    ));
    let task_id = {
        let sess = session.lock().await;
        sess.agent_loop.task_id().to_string()
    };

     // Print startup info line (respects NO_COLOR and --quiet)
     {
         let sess = session.lock().await;
         if !sess.is_quiet() {
             let startup_info = sess.get_startup_info();
             let startup_text = format!(
                 "{}\n{}\n{}",
                 startup_info,
                 crate::cli::colors::info(
                     "type a prompt and press Enter; type 'exit' or 'quit' to leave",
                 ),
                 crate::cli::colors::info(
                     "type /help for slash commands, @ to search and mention files",
                 ),
             );
             crate::cli::colors::eprint_raw(&startup_text);
         }
     }

    // Load command history for up/down arrow navigation
    let command_history = load_command_history();
    let mut history_index: Option<usize> = None;

    {
        let sess = session.lock().await;
        let mut qh = queue_handle.lock().await;
        *qh = Some(sess.queue_handle());
        let mut sh = state_handle.lock().await;
        *sh = Some(sess.state_handle());
    }

    // Track last render state to avoid redundant re-renders
    let mut last_input_snapshot = String::new();
    let mut last_cursor_pos = 0;
     let mut last_picker_active = false;

     'main: loop {
        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));

        let prompt_prefix =
            render_interactive_prompt_prefix();

        // Check if re-render is necessary (skip if nothing changed and picker state unchanged)
        let current_snapshot = format!(
            "{}|{}|{}",
            input_buf,
            cursor_pos,
            agent_busy.load(Ordering::Relaxed)
        );
        let needs_render = current_snapshot != last_input_snapshot
            || cursor_pos != last_cursor_pos
            || picker_active != last_picker_active;

        // Full TUI rendering (with picker) is skipped while agent is busy.
        if needs_render && !agent_busy.load(Ordering::Relaxed) {
            last_input_snapshot = current_snapshot.clone();
            last_cursor_pos = cursor_pos;
            last_picker_active = picker_active;

            render_input_line(&mut stdout, &input_buf, cursor_pos, &prompt_prefix, term_cols, term_rows, false)?;

            if picker_active {
                let picker_height = picker.overlay_height();

                let cursor_row = (term_rows - 1) as usize;

                let picker_start = if cursor_row + 1 + picker_height <= term_rows as usize {
                    cursor_row + 1
                } else {
                    cursor_row.saturating_sub(picker_height)
                };

                write!(stdout, "\x1b[s")?;
                for i in 0..picker_height {
                    write!(stdout, "\x1b[{};1H\x1b[K", picker_start + i + 1)?;
                }
                picker.render_at(&mut stdout, picker_start)?;
                write!(stdout, "\x1b[u")?;
                stdout.flush()?;

                last_picker_row = Some(picker_start);
                last_picker_height = picker_height;
            } else if let Some(start_row) = last_picker_row.take() {
                write!(stdout, "\x1b[s")?;
                for i in 0..last_picker_height {
                    write!(stdout, "\x1b[{};1H\x1b[K", start_row + i + 1)?;
                }
                write!(stdout, "\x1b[u")?;
                stdout.flush()?;
            }
        }

        // Render input during agent execution. Scroll region pins the bottom row
        // so agent output scrolls above it. After rendering, cursor returns to
        // the scroll region bottom so agent stderr continues there.
        if agent_busy.load(Ordering::Relaxed) && needs_render {
            last_input_snapshot = current_snapshot;
            last_cursor_pos = cursor_pos;
            last_picker_active = picker_active;

            render_input_line(&mut stdout, &input_buf, cursor_pos, &prompt_prefix, term_cols, term_rows, true)?;
        }

        let n = loop {
            // Shorter poll delay while agent is busy for responsive UI after completion.
            let reprompt_delay = if agent_busy.load(Ordering::Relaxed) {
                tokio::time::Duration::from_millis(100)
            } else {
                tokio::time::Duration::from_millis(500)
            };

            tokio::select! {
                biased;
                _ = agent_done.notified() => {
                    if let Some(start) = *agent_start_time.lock().await {
                        let elapsed = start.elapsed();
                        let elapsed_str = format_duration(elapsed);
                        writeln!(
                            stdout,
                            "\r\n{}",
                            crate::cli::colors::info(&format!("⏱ Elapsed: {}", elapsed_str))
                        )?;
                        stdout.flush()?;
                    }
                    render_input_line(&mut stdout, &input_buf, cursor_pos, &prompt_prefix, term_cols, term_rows, false)?;
                    last_input_snapshot = format!("{}|{}|{}", input_buf, cursor_pos, agent_busy.load(Ordering::Relaxed));
                    last_cursor_pos = cursor_pos;
                    last_picker_active = picker_active;
                    continue 'main;
                }
                result = stdin.read(&mut byte_buf) => {
                    break match result {
                        Ok(0) => 0,
                        Ok(n) => n,
                        Err(e) => {
                            if e.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            0
                        }
                    };
                }
                _ = tokio::time::sleep(reprompt_delay) => {
                    // Periodic re-render while agent is busy
                    continue 'main;
                }
            }
        };
        if n == 0 {
            break;
        }

        parser.feed(&byte_buf[..n]);
        let events = parser.drain_events();

        for event in events {
            match event {
                TerminalEvent::Paste(content) => {
                    // Append pasted content as a single block
                    // Replace newlines with spaces to keep input single-line
                    let sanitized: String = content
                        .chars()
                        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                        .collect();
                    let line_count = content.lines().count();
                    tracing::debug!(target: "sned::input", "Handling paste: {} bytes, {} lines", content.len(), line_count);
                    input_buf.push_str(&sanitized);
                    cursor_pos = input_buf.len();
                    // Show feedback for multi-line pastes
                    if line_count > 1 {
                        writeln!(
                            stdout,
                            "\r\n📋 Pasted {} lines (use Enter to submit)",
                            line_count
                        )?;
                        stdout.flush()?;
                    }
                    continue;
                }
                TerminalEvent::Return => {
                    tracing::debug!(target: "sned::input", "Return event: input_buf length={} chars", input_buf.len());
                    if picker_active {
                        if let Some(selected) = picker.selected() {
                            let mq = extract_mention_query(&input_buf);
                            if mq.in_mention_mode && mq.at_index >= 0 {
                                input_buf = insert_mention(
                                    &input_buf,
                                    mq.at_index as usize,
                                    &selected.path,
                                );
                                cursor_pos = input_buf.len();
                            }
                        }
                        picker_active = false;
                        continue;
                    }

                    // If an approval prompt is active, Enter does nothing
                    if crate::core::approval::is_approval_prompt_active() {
                        input_buf.clear();
                        cursor_pos = 0;
                        continue;
                    }

                    // If a followup question is active, forward the response via channel
                    if crate::core::approval::is_followup_question_active(&task_id) {
                        let response = input_buf.clone();
                        if let Some(sender) = crate::core::approval::take_followup_sender(&task_id) {
                            let _ = sender.send(response);
                            crate::core::approval::clear_followup_sender(&task_id);
                            crate::core::approval::set_followup_question_active(&task_id, false);
                            input_buf.clear();
                            cursor_pos = 0;
                            writeln!(stdout)?;
                            continue;
                        }
                    }

                    let prompt = input_buf.trim().to_string();
                    // Echo prompt to stdout only if agent is not busy.
                    // When agent is busy, stdout cursor positioning races with agent stderr output.
                    // The prompt text itself is still submitted to the agent via message queue.
                    if !agent_busy.load(Ordering::Relaxed) {
                        let (cols, _) = crossterm::terminal::size().unwrap_or((80, 24));
                        let full = format!("{}{}", prompt_prefix, &input_buf);
                        let dw = display_width(&full);
                        let span = (dw as u16).div_ceil(cols.max(1));
                        // Clear rows in scroll region (rows 1 to term_rows-1)
                        for i in 0..span {
                            let row = term_rows.saturating_sub(i + 2);
                            if row > 0 {
                                write!(stdout, "\x1b[{};1H\x1b[K", row)?;
                            }
                        }
                        // Position cursor at bottom of scroll region and write prompt
                        // Cursor MUST end at scroll region bottom, NOT the pinned input row
                        write!(stdout, "\x1b[{};1H", term_rows.saturating_sub(1))?;
                        if !prompt.is_empty() {
                            write!(stdout, "{}{}", prompt_prefix, &prompt)?;
                        }
                        // Move cursor DOWN one row to the pinned input row
                        write!(stdout, "\x1b[{};1H", term_rows)?;
                        stdout.flush()?;
                    }
                    tracing::debug!(target: "sned::input", "Checking if prompt is empty");

                    if prompt.is_empty() {
                        input_buf.clear();
                        cursor_pos = 0;
                        input_row = input_row.saturating_add(1);
                        continue;
                    }

                    tracing::debug!(target: "sned::input", "Saving to history");
                    // Save non-empty prompt to history
                    append_to_history(&prompt);
                    // Reset history index when submitting a new command
                    history_index = None;
                    tracing::debug!(target: "sned::input", "Processing slash commands");

                    // Check for slash commands
                    let processed_prompt =
                        crate::cli::slash_commands::process_slash_command(&prompt);
                    tracing::debug!(target: "sned::input", "Processed slash command: {} -> {}", prompt.escape_debug(), processed_prompt.escape_debug());

                    // Handle CLI-only commands locally
                    if let Some(cli_cmd) = crate::cli::slash_commands::get_cli_only_command(&prompt)
                    {
                        tracing::debug!(target: "sned::input", "CLI command: {:?}", cli_cmd);
                        match cli_cmd {
                            crate::cli::slash_commands::CliOnlyCommand::Exit
                            | crate::cli::slash_commands::CliOnlyCommand::Quit => {
                                cleanup_terminal(raw_guard.take())?;
                                return Ok(());
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Clear => {
                                write!(stdout, "\x1b[2J\x1b[H")?;
                                eprintln!("Conversation cleared.");
                            }
                            crate::cli::slash_commands::CliOnlyCommand::History => {
                                drop(raw_guard.take());
                                let _ = crate::cli::subcommands::run_history(HistoryOptions {
                                    limit: 10,
                                    page: 1,
                                    favorites_only: false,
                                    workspace_only: false,
                                    search: None,
                                    sort: "newest".to_string(),
                                    config: task_opts.config.clone(),
                                });
                                restore_raw_mode(&mut raw_guard)?;
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Skills => {
                                // Discover and list skills
                                let skills_text = if let Ok(cwd) = std::env::current_dir() {
                                    let project_skills =
                                        crate::core::context::discover_skills(&cwd);
                                    let all_skills =
                                        crate::core::context::get_available_skills(project_skills);
                                    if all_skills.is_empty() {
                                        "No skills found.".to_string()
                                    } else {
                                        let mut lines =
                                            vec!["Available Skills:".to_string(), String::new()];
                                        for skill in all_skills {
                                            lines.push(format!(
                                                "  {} - {}",
                                                skill.name, skill.description
                                            ));
                                        }
                                        lines.join("\n")
                                    }
                                } else {
                                    "No skills found.".to_string()
                                };
                                crate::cli::colors::eprint_raw(&skills_text);
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Help => {
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else {
                                    crate::cli::colors::eprint_raw(
                                        &crate::cli::slash_commands::format_help_text(),
                                    );
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::HelpOption(cmd) => {
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else {
                                    crate::cli::colors::eprint_raw(
                                        &crate::cli::slash_commands::format_help_for_command(&cmd),
                                    );
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Settings => {
                                let provider = task_opts.provider.as_deref().unwrap_or("anthropic");
                                let model =
                                    task_opts.model.as_deref().unwrap_or("claude-3-5-sonnet");
                                let mode = if task_opts.plan { "plan" } else { "act" };
                                crate::cli::colors::eprint_raw(
                                    &crate::cli::slash_commands::format_settings_text(
                                        provider,
                                        model,
                                        mode,
                                        auto_approve,
                                    ),
                                );
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Models => {
                                crate::cli::colors::eprint_raw(
                                    &crate::cli::slash_commands::format_models_text(),
                                );
                            }
                            crate::cli::slash_commands::CliOnlyCommand::ResetCompact => {
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else {
                                    let mut sess = session.lock().await;
                                    if sess.clear_compacted_summary().await {
                                        eprintln!(
                                            "Compacted summary cleared. You can now use /compact again."
                                        );
                                    } else {
                                        eprintln!("No compacted summary to clear.");
                                    }
                                    drop(sess);
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Stats => {
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else {
                                    let sess = session.lock().await;
                                    let state_handle = sess.agent_loop.state_handle();
                                    let state = state_handle.lock().await;
                                    let stats = crate::cli::slash_commands::format_stats_text(&state);
                                    eprintln!("{}", stats);
                                    drop(sess);
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Changes => {
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else {
                                    let sess = session.lock().await;
                                    let state_handle = sess.agent_loop.state_handle();
                                    let state = state_handle.lock().await;
                                    let changes =
                                        crate::cli::slash_commands::format_changes_text(&state);
                                    eprintln!("{}", changes);
                                    drop(sess);
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Undo
                            | crate::cli::slash_commands::CliOnlyCommand::CheckpointUndo => {
                                // Use the checkpoint manager to restore the most recent checkpoint
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else {
                                    let sess = session.lock().await;
                                    let checkpoint_mgr = sess.agent_loop.checkpoint_manager();

                                    if checkpoint_mgr.is_none() {
                                        eprintln!("Checkpoint manager is not initialized.");
                                        drop(sess);
                                        continue;
                                    }

                                    let checkpoint_mgr = checkpoint_mgr.unwrap();

                                // Get the most recent checkpoint
                                let checkpoints = match checkpoint_mgr.list_checkpoints().await {
                                    Ok(cps) => cps,
                                    Err(e) => {
                                        let actionable = crate::cli::actionable_errors::checkpoint_operation_failed("list", &e.to_string());
                                        eprintln!("{}", actionable);
                                        drop(sess);
                                        continue;
                                    }
                                };

                                if checkpoints.is_empty() {
                                    eprintln!("No checkpoints available to undo.");
                                    drop(sess);
                                    continue;
                                }

                                // Most recent checkpoint is first in the list (git log order: newest first)
                                let most_recent = &checkpoints[0];

                                // Get files that will be reverted
                                let current_hash =
                                    checkpoint_mgr.last_checkpoint().map(|h| h.as_str());
                                let changed_files = if let Some(current) = current_hash {
                                    checkpoint_mgr
                                        .get_changed_files(&most_recent.hash, Some(current))
                                        .await
                                        .unwrap_or_else(|_| vec![])
                                } else {
                                    vec![]
                                };

                                if !changed_files.is_empty() {
                                    eprintln!(
                                        "⚠ /undo will revert the following files to the previous checkpoint:"
                                    );
                                    for f in &changed_files {
                                        eprintln!("  - {}", f);
                                    }
                                    eprintln!("Continue? (y to cancel, Enter to confirm): ");

                                    // Use channel-based input to avoid stdin race with TUI async reader
                                    // Same pattern as condense tool (A9/A18 fix)
                                    let (sender, receiver) = std::sync::mpsc::channel();
                                    crate::core::approval::set_followup_question_active(&task_id, true);
                                    crate::core::approval::set_followup_sender(&task_id, sender);

                                    // Wait for user input via channel (forwarded by TUI loop on Enter)
                                    // Timeout after 30 seconds to prevent indefinite blocking
                                    let response_result = tokio::task::spawn_blocking(move || {
                                        receiver.recv_timeout(std::time::Duration::from_secs(30))
                                    }).await;

                                    // Clean up regardless of result
                                    crate::core::approval::clear_followup_sender(&task_id);
                                    crate::core::approval::set_followup_question_active(&task_id, false);

                                    let confirm = match response_result {
                                        Ok(Ok(r)) => r,
                                        Ok(Err(_)) | Err(_) => String::new(), // Channel closed = no response or timeout
                                    };

                                    // Empty input (Enter) confirms by default; 'y' cancels
                                    if !confirm.trim().is_empty() && confirm.trim().to_lowercase() == "y" {
                                        eprintln!("Undo cancelled.");
                                        drop(sess);
                                        continue;
                                    }
                                }

                                // Restore to the most recent checkpoint
                                match checkpoint_mgr.restore_checkpoint(&most_recent.hash).await {
                                    Ok(()) => {
                                        eprintln!(
                                            "Restored to checkpoint {} — {} file(s) reverted",
                                            most_recent.number,
                                            changed_files.len()
                                        );

                                        // Show diff
                                        if !changed_files.is_empty() {
                                            eprintln!("\nReverted files:");
                                            for f in &changed_files {
                                                eprintln!("  - {}", f);
                                            }
                                        }

                                        // Remove last turn from conversation history
                                        let removed = sess.agent_loop.remove_last_turn().await;
                                        if removed > 0 {
                                            eprintln!(
                                                "Removed {} message(s) from conversation history.",
                                                removed
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        let actionable = crate::cli::actionable_errors::git_operation_failed("undo", &e.to_string());
                                        eprintln!("{}", actionable);
                                    }
                                }

                                drop(sess);
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Diff => {
                                if let Ok(workspace_root) = std::env::current_dir() {
                                    if !crate::core::shadow_git::is_initialized(&workspace_root) {
                                        eprintln!(
                                            "Change tracking is not enabled. Use --track-changes to enable automatic undo/versioning."
                                        );
                                    } else {
                                        match crate::core::shadow_git::diff_turns(
                                            &workspace_root,
                                            1,
                                            0,
                                        ) {
                                            Ok(diff) => {
                                                if diff.is_empty() {
                                                    eprintln!("No changes.");
                                                } else {
                                                    eprintln!("{}", diff);
                                                }
                                            }
                                            Err(e) => {
                                                let actionable = crate::cli::actionable_errors::git_operation_failed("diff", &e.to_string());
                                                eprintln!("{}", actionable);
                                            }
                                        }
                                    }
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Log => {
                                if let Ok(workspace_root) = std::env::current_dir() {
                                    if !crate::core::shadow_git::is_initialized(&workspace_root) {
                                        eprintln!(
                                            "Change tracking is not enabled. Use --track-changes to enable automatic undo/versioning."
                                        );
                                    } else {
                                        match crate::core::shadow_git::log(
                                            &workspace_root,
                                            Some(10),
                                        ) {
                                            Ok(log) => {
                                                if log.is_empty() {
                                                    eprintln!("No log entries.");
                                                } else {
                                                    eprintln!("{}", log);
                                                }
                                            }
                                            Err(e) => {
                                                let actionable = crate::cli::actionable_errors::git_operation_failed("log", &e.to_string());
                                                eprintln!("{}", actionable);
                                            }
                                        }
                                    }
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Commit => {
                                // Extract commit message from prompt
                                let commit_msg = if prompt.starts_with("/commit ") {
                                    prompt
                                        .strip_prefix("/commit ")
                                        .map(|s| s.trim_matches('"').trim_matches('\'').to_string())
                                } else {
                                    None
                                };

                                if let Some(msg) = commit_msg {
                                    if let Ok(workspace_root) = std::env::current_dir() {
                                        if !crate::core::shadow_git::is_initialized(&workspace_root)
                                        {
                                            eprintln!(
                                                "Change tracking is not enabled. Use --track-changes to enable automatic undo/versioning."
                                            );
                                        } else {
                                            // Show diff for confirmation
                                            match crate::core::shadow_git::diff_turns(
                                                &workspace_root,
                                                1,
                                                0,
                                            ) {
                                                Ok(diff) => {
                                                    if diff.is_empty() {
                                                        eprintln!("No changes to commit.");
                                                    } else {
                                                        eprintln!("Changes to commit:");
                                                        eprintln!("{}", diff);
                                                        eprintln!(
                                                            "Commit to your git repo? (y/n): "
                                                        );

                                                        // Use channel-based input to avoid stdin race with TUI async reader
                                                        // Same pattern as condense tool (A9/A18 fix)
                                                        let (sender, receiver) = std::sync::mpsc::channel();
                                                        crate::core::approval::set_followup_question_active(&task_id, true);
                                                        crate::core::approval::set_followup_sender(&task_id, sender);

                                                        // Wait for user input via channel (forwarded by TUI loop on Enter)
                                                        // Timeout after 30 seconds to prevent indefinite blocking
                                                        let response_result = tokio::task::spawn_blocking(move || {
                                                            receiver.recv_timeout(std::time::Duration::from_secs(30))
                                                        }).await;

                                                        // Clean up regardless of result
                                                        crate::core::approval::clear_followup_sender(&task_id);
                                                        crate::core::approval::set_followup_question_active(&task_id, false);

                                                        let confirm = match response_result {
                                                            Ok(Ok(r)) => r,
                                                            Ok(Err(_)) | Err(_) => String::new(), // Channel closed = no response
                                                        };

                                                        // Empty input (Enter) confirms by default
                                                        if confirm.trim().is_empty() || confirm.trim().to_lowercase() == "y" {
                                                            match crate::core::shadow_git::commit_to_real_git(&workspace_root, &msg) {
                                                                Ok(files) => {
                                                                    eprintln!("Committed {} file(s) to your git repo.", files.len());
                                                                }
                                                                Err(e) => {
                                                                    let actionable = crate::cli::actionable_errors::git_operation_failed("commit", &e.to_string());
                                                                    eprintln!("{}", actionable);
                                                                }
                                                            }
                                                        } else {
                                                            eprintln!("Commit cancelled.");
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    let actionable = crate::cli::actionable_errors::git_operation_failed("get diff", &e.to_string());
                                                    eprintln!("{}", actionable);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    eprintln!("Usage: /commit \"commit message\"");
                                    eprintln!("Example: /commit \"fix: auth bug\"");
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::CheckpointList => {
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else {
                                    let sess = session.lock().await;
                                    let checkpoint_mgr = sess.agent_loop.checkpoint_manager();

                                    if checkpoint_mgr.is_none() {
                                        eprintln!("Checkpoint manager is not initialized.");
                                        drop(sess);
                                        continue;
                                    }

                                    let checkpoint_mgr = checkpoint_mgr.unwrap();

                                    match checkpoint_mgr.list_checkpoints().await {
                                    Ok(checkpoints) => {
                                        if checkpoints.is_empty() {
                                            eprintln!("No checkpoints found.");
                                        } else {
                                            eprintln!("Available checkpoints:");
                                            eprintln!("  #  Hash      Message");
                                            eprintln!("  ──────────────────────────");
                                            for cp in checkpoints.iter().rev() {
                                                eprintln!(
                                                    "  {}  {}  {}",
                                                    crate::cli::colors::colorize(
                                                        &cp.number.to_string(),
                                                        crate::cli::colors::style::BOLD
                                                    ),
                                                    crate::cli::colors::colorize(
                                                        &cp.hash,
                                                        crate::cli::colors::style::DIM
                                                    ),
                                                    cp.message
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("Failed to list checkpoints: {}", e);
                                    }
                                }
                                drop(sess);
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::CheckpointRestore => {
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else {
                                    let sess = session.lock().await;
                                    let checkpoint_mgr = sess.agent_loop.checkpoint_manager();

                                    if checkpoint_mgr.is_none() {
                                        eprintln!("Checkpoint manager is not initialized.");
                                        drop(sess);
                                        continue;
                                    }

                                let checkpoint_mgr = checkpoint_mgr.unwrap();

                                match checkpoint_mgr.list_checkpoints().await {
                                    Ok(checkpoints) => {
                                        if checkpoints.is_empty() {
                                            eprintln!("No checkpoints to restore.");
                                        } else {
                                            // Try to parse checkpoint number from command
                                            let checkpoint_num = crate::cli::slash_commands::parse_checkpoint_restore(&prompt);

                                            let num = if let Some(n) = checkpoint_num {
                                                n
                                            } else {
                                                // Show list and ask interactively
                                                eprintln!("Available checkpoints:");
                                                eprintln!("  #  Hash      Message");
                                                eprintln!("  ──────────────────────────");
                                                for cp in checkpoints.iter().rev() {
                                                    eprintln!(
                                                        "  {}  {}  {}",
                                                        crate::cli::colors::colorize(
                                                            &cp.number.to_string(),
                                                            crate::cli::colors::style::BOLD
                                                        ),
                                                        crate::cli::colors::colorize(
                                                            &cp.hash,
                                                            crate::cli::colors::style::DIM
                                                        ),
                                                        cp.message
                                                    );
                                                }
                                                eprintln!();
                                                eprintln!("Enter checkpoint number to restore:");
                                                
                                                        // Use channel-based input to avoid stdin race with TUI async reader
                                                        // Same pattern as condense tool (A9/A18 fix)
                                                        let (sender, receiver) = std::sync::mpsc::channel();
                                                        crate::core::approval::set_followup_question_active(&task_id, true);
                                                        crate::core::approval::set_followup_sender(&task_id, sender);

                                                        // Wait for user input via channel (forwarded by TUI loop on Enter)
                                                        // Timeout after 30 seconds to prevent indefinite blocking
                                                        let response_result = tokio::task::spawn_blocking(move || {
                                                            receiver.recv_timeout(std::time::Duration::from_secs(30))
                                                        }).await;

                                                        // Clean up regardless of result
                                                        crate::core::approval::clear_followup_sender(&task_id);
                                                        crate::core::approval::set_followup_question_active(&task_id, false);

                                                let input = match response_result {
                                                    Ok(Ok(r)) => r,
                                                    Ok(Err(_)) | Err(_) => String::new(), // Channel closed = no response
                                                };

                                                input.trim().parse::<usize>().unwrap_or(0)
                                            };

                                            if num == 0 || num > checkpoints.len() {
                                                eprintln!(
                                                    "Invalid checkpoint number. Available: 1-{}",
                                                    checkpoints.len()
                                                );
                                                drop(sess);
                                                input_buf.clear();
                                                cursor_pos = 0;
                                                input_row = input_row.saturating_add(1);
                                                continue;
                                            }

                                            if let Some(checkpoint) = checkpoints.get(num - 1) {
                                                let current_hash = checkpoint_mgr
                                                    .last_checkpoint()
                                                    .map(|h| h.as_str())
                                                    .unwrap_or("HEAD");
                                                match checkpoint_mgr.get_changed_files(
                                                    &checkpoint.hash,
                                                    Some(current_hash),
                                                )
                                                .await
                                                {
                                                    Ok(changed_files) => {
                                                        if !changed_files.is_empty() {
                                                            eprintln!(
                                                                "\nFiles that will be restored:"
                                                            );
                                                            for file in &changed_files {
                                                                eprintln!("  - {}", file);
                                                            }
                                                            eprintln!();
                                                        eprintln!("Continue? (y to cancel, Enter to confirm): ");

                                                        // Use channel-based input to avoid stdin race with TUI async reader
                                                        // Same pattern as condense tool (A9/A18 fix)
                                                        let (sender, receiver) = std::sync::mpsc::channel();
                                                        crate::core::approval::set_followup_question_active(&task_id, true);
                                                        crate::core::approval::set_followup_sender(&task_id, sender);

                                                        // Wait for user input via channel (forwarded by TUI loop on Enter)
                                                        // Timeout after 30 seconds to prevent indefinite blocking
                                                        let response_result = tokio::task::spawn_blocking(move || {
                                                            receiver.recv_timeout(std::time::Duration::from_secs(30))
                                                        }).await;

                                                        // Clean up regardless of result
                                                        crate::core::approval::clear_followup_sender(&task_id);
                                                        crate::core::approval::set_followup_question_active(&task_id, false);

                                                        let confirm = match response_result {
                                                            Ok(Ok(r)) => r,
                                                            Ok(Err(_)) | Err(_) => String::new(), // Channel closed = no response or timeout
                                                        };

                                                        // Empty input (Enter) confirms by default; 'y' cancels
                                                        if !confirm.trim().is_empty() && confirm.trim().to_lowercase() == "y" {
                                                            eprintln!("Restore cancelled.");
                                                            drop(sess);
                                                            input_buf.clear();
                                                            cursor_pos = 0;
                                                            input_row = input_row.saturating_add(1);
                                                            continue;
                                                        }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        eprintln!(
                                                            "Warning: Could not determine changed files: {}",
                                                            e
                                                        );
                                                    }
                                                }

                                                match checkpoint_mgr.restore_by_number(num).await {
                                                    Ok(()) => {
                                                        eprintln!(
                                                            "Checkpoint {} ({}) restored successfully.",
                                                            num, checkpoint.hash
                                                        );
                                                    }
                                                    Err(e) => {
                                                        let actionable = crate::cli::actionable_errors::checkpoint_operation_failed("restore", &e.to_string());
                                                        eprintln!("{}", actionable);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("Failed to list checkpoints: {}", e);
                                    }
                                }
                                drop(sess);
                                }
                            }
                            crate::cli::slash_commands::CliOnlyCommand::Expand => {
                                if agent_busy.load(Ordering::Relaxed) {
                                    crate::cli::colors::eprint_warning(
                                        "Agent is busy. Wait for it to finish before running this command.",
                                    );
                                } else if let Some(index) =
                                    crate::cli::slash_commands::parse_expand_index(&prompt)
                                {
                                    let sess = session.lock().await;
                                    let state_handle = sess.agent_loop.state_handle();
                                    drop(sess);

                                    let state = state_handle.lock().await;
                                    if let Some(block) = state
                                        .snipped_code_blocks
                                        .iter()
                                        .find(|block| block.index == index)
                                    {
                                        if block.language.is_empty() {
                                            eprintln!("```");
                                        } else {
                                            eprintln!("```{}", block.language);
                                        }
                                        let highlighted =
                                            crate::cli::syntax_highlight::highlight_code(
                                                &block.code,
                                                &block.language,
                                            );
                                        for line in highlighted.lines() {
                                            eprintln!("{}", line);
                                        }
                                        eprintln!("```");
                                    } else {
                                        eprintln!("No snipped code block {}.", index);
                                    }
                                } else {
                                    eprintln!("Usage: /expand N");
                                }
                            }
                        }

                        input_buf.clear();
                        cursor_pos = 0;
                        input_row = input_row.saturating_add(1);
                        continue;
                    }

                    // Check if prompt was processed (base slash command)
                    let effective_prompt = if processed_prompt != prompt {
                        Some(processed_prompt)
                    } else {
                        Some(prompt)
                    };

                    let _ = stdout.flush();

                    tracing::debug!(target: "sned::input", "Agent busy={}, raw_guard={}", agent_busy.load(Ordering::Relaxed), raw_guard.is_some());

                    if agent_busy.load(Ordering::Relaxed) {
                        let qh = queue_handle.lock().await;
                        if let Some(handle) = qh.as_ref() {
                            let msg = effective_prompt.unwrap_or_default();
                            if !msg.is_empty() {
                                handle.enqueue_text_message(msg).await;
                                let count = handle.queued_message_count().await;
                                crate::cli::colors::eprint_info(&format!(
                                    "Message queued ({} in queue)",
                                    count
                                ));
                            }
                        }
                    } else {
                        agent_busy.store(true, Ordering::Relaxed);
                        let start_time = std::time::Instant::now();
                        *agent_start_time.lock().await = Some(start_time);
                        let busy = agent_busy.clone();
                        let sess = session.clone();
                        let at = agent_task.clone();
                        let ast = agent_start_time.clone();

                        tracing::debug!(target: "sned::agent", "Spawning agent task, prompt length={}", effective_prompt.as_ref().map(|s| s.len()).unwrap_or(0));

                        let agent_done_clone = agent_done.clone();
                        let handle = tokio::spawn(async move {
                            tracing::debug!(target: "sned::agent", "Inside spawned task, acquiring session lock");
                            let mut s = sess.lock().await;
                            tracing::debug!(target: "sned::agent", "Session lock acquired, calling run()");
                            let result = s.run(effective_prompt).await;
                            tracing::debug!(target: "sned::agent", "session.run() returned: {:?}", result.as_ref().map(|_| "Ok").unwrap_or("Err"));
                            if let Err(e) = result {
                                crate::cli::colors::eprint_error(&e.to_string());
                            }
                            busy.store(false, Ordering::Relaxed);
                            *ast.lock().await = None;
                            agent_done_clone.notify_one();
                            // Clean up the task handle
                            let mut task = at.lock().await;
                            *task = None;
                        });

                        {
                            let mut task = agent_task.lock().await;
                            *task = Some(handle);
                        }
                    }

                    input_buf.clear();
                    cursor_pos = 0;
                    let (term_rows, term_cols) = crossterm::terminal::size().unwrap_or((24, 80));
                    input_row = term_rows.saturating_sub(2);
                    
                    // Render empty input line at pinned row, cursor at scroll region bottom
                    render_input_line(&mut stdout, &input_buf, cursor_pos, &prompt_prefix, term_cols, term_rows, true)?;
                    stdout.flush()?;
                }
                TerminalEvent::Char(c) => {
                    // Skip newline characters - they should be Return events, but filter defensively
                    if c == '\n' || c == '\r' {
                        continue;
                    }

                    // Without this guard, tokio::io::stdin() and the approval
                    // prompt's libc::read() would both read from the same fd,
                    // causing dropped or duplicated characters.
                    //
                    // INVARIANT: approval.rs must always reset APPROVAL_PROMPT_ACTIVE
                    // to false before returning. If the flag leaks true, this loop
                    // permanently skips all stdin.
                    if crate::core::approval::is_approval_prompt_active() {
                        continue;
                    }

                    // If a followup question is active, forward the full line response
                    // when user presses Enter (handled in TerminalEvent::Return branch)
                    if crate::core::approval::is_followup_question_active(&task_id) {
                        // Just continue accumulating in input_buf until Enter
                    }

                    // Insert at byte index, then advance cursor to next char boundary
                    input_buf.insert(cursor_pos, c);
                    cursor_pos += c.len_utf8();

                    let mq = extract_mention_query(&input_buf);
                    if mq.in_mention_mode {
                        picker_active = true;
                        picker.set_query(&mq.query);
                        let results = search_workspace_files(&mq.query, &cwd_str, 20).await;
                        picker.update_results(results);
                    } else if picker_active {
                        picker_active = false;
                    }
                }
                TerminalEvent::Backspace => {
                    if cursor_pos > 0 {
                        // Find previous char boundary
                        let prev_pos = input_buf[..cursor_pos]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        input_buf.remove(prev_pos);
                        cursor_pos = prev_pos;
                    }

                    if picker_active {
                        let mq = extract_mention_query(&input_buf);
                        if mq.in_mention_mode {
                            picker.set_query(&mq.query);
                            let results = search_workspace_files(&mq.query, &cwd_str, 20).await;
                            picker.update_results(results);
                        } else {
                            picker_active = false;
                        }
                    }
                }
                TerminalEvent::Delete if cursor_pos < input_buf.len() => {
                    input_buf.remove(cursor_pos);
                }
                TerminalEvent::Arrow(dir) => {
                    use crate::terminal::input::ArrowDirection;
                    if picker_active {
                        match dir {
                            ArrowDirection::Up => picker.up(),
                            ArrowDirection::Down => picker.down(),
                            ArrowDirection::Left | ArrowDirection::Right => {
                                picker_active = false;
                                if dir == ArrowDirection::Left && cursor_pos > 0 {
                                    // Move to previous char boundary
                                    cursor_pos = input_buf[..cursor_pos]
                                        .char_indices()
                                        .next_back()
                                        .map(|(i, _)| i)
                                        .unwrap_or(0);
                                } else if dir == ArrowDirection::Right
                                    && cursor_pos < input_buf.len()
                                {
                                    // Move to next char boundary
                                    if let Some((i, ch)) =
                                        input_buf[cursor_pos..].char_indices().next()
                                    {
                                        cursor_pos = cursor_pos + i + ch.len_utf8();
                                    }
                                }
                            }
                        }
                    } else {
                        match dir {
                            ArrowDirection::Left if cursor_pos > 0 => {
                                cursor_pos = input_buf[..cursor_pos]
                                    .char_indices()
                                    .next_back()
                                    .map(|(i, _)| i)
                                    .unwrap_or(0);
                            }
                            ArrowDirection::Right if cursor_pos < input_buf.len() => {
                                if let Some((i, ch)) = input_buf[cursor_pos..].char_indices().next()
                                {
                                    cursor_pos = cursor_pos + i + ch.len_utf8();
                                }
                            }
                            ArrowDirection::Up => {
                                let mq = extract_mention_query(&input_buf);
                                if mq.in_mention_mode {
                                    picker_active = true;
                                    picker.set_query(&mq.query);
                                    let results =
                                        search_workspace_files(&mq.query, &cwd_str, 20).await;
                                    picker.update_results(results);
                                } else if !command_history.is_empty() {
                                    // History navigation: go to previous entry
                                    let new_index = match history_index {
                                        None => command_history.len() - 1,
                                        Some(i) if i > 0 => i - 1,
                                        Some(i) => i, // Stay at first entry
                                    };
                                    history_index = Some(new_index);
                                    input_buf = command_history[new_index].clone();
                                    cursor_pos = input_buf.len();
                                }
                            }
                            ArrowDirection::Down => {
                                let mq = extract_mention_query(&input_buf);
                                if mq.in_mention_mode {
                                    // Stay in mention mode
                                } else if !command_history.is_empty() {
                                    // History navigation: go to next entry or clear
                                    match history_index {
                                        Some(i) if i < command_history.len() - 1 => {
                                            history_index = Some(i + 1);
                                            input_buf = command_history[i + 1].clone();
                                        }
                                        Some(_) => {
                                            // Past last entry, clear buffer
                                            history_index = None;
                                            input_buf.clear();
                                        }
                                        None => {
                                            // Not in history, stay cleared
                                        }
                                    }
                                    cursor_pos = input_buf.len();
                                }
                            }
                            _ => {}
                        }
                    }
                }
                TerminalEvent::Escape if picker_active => {
                    picker_active = false;
                }
                TerminalEvent::Tab if picker_active => {
                    if let Some(selected) = picker.selected() {
                        let mq = extract_mention_query(&input_buf);
                        if mq.in_mention_mode && mq.at_index >= 0 {
                            input_buf =
                                insert_mention(&input_buf, mq.at_index as usize, &selected.path);
                            cursor_pos = input_buf.len();
                        }
                    }
                    picker_active = false;
                }
                TerminalEvent::Ctrl('c') => {
                    if picker_active {
                        picker_active = false;
                    } else if agent_busy.load(Ordering::Relaxed) {
                        // Cancel the running agent
                        {
                            let sh = state_handle.lock().await;
                            if let Some(handle) = sh.as_ref() {
                                let mut state = handle.lock().await;
                                state.is_cancelled = true;
                                state
                                    .is_cancelled_atomic
                                    .store(true, std::sync::atomic::Ordering::Release);
                                // Kill any registered command PIDs to prevent orphans
                                let pids = state.running_command_pids.clone();
                                drop(state);
                                if !pids.is_empty() {
                                    #[cfg(unix)]
                                    {
                                        // Spawn a task to handle SIGTERM→sleep→SIGKILL asynchronously
                                        // to avoid blocking the tokio event loop
                                        let pids_clone = pids.clone();
                                        tokio::spawn(async move {
                                            // Send SIGTERM (check liveness to avoid recycled PIDs)
                                            for pid in &pids_clone {
                                                if unsafe { libc::kill(*pid, 0) } == 0 {
                                                    let _ = unsafe { libc::kill(*pid, libc::SIGTERM) };
                                                }
                                            }
                                            // Async pause for SIGTERM to take effect
                                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                            // Force kill any survivors
                                            for pid in &pids_clone {
                                                if unsafe { libc::kill(*pid, 0) } == 0 {
                                                    let _ = unsafe { libc::kill(*pid, libc::SIGKILL) };
                                                }
                                            }
                                        });
                                    }
                                    crate::cli::colors::eprint_info(&format!(
                                        "Killing {} running command(s)...",
                                        pids.len()
                                    ));
                                }
                            }
                        };
                        // Cancel any pending approval prompt so its receiver wakes up
                        crate::core::approval::clear_approval_sender();
                        // Abort the agent task and wait for it to fully unwind (including Drop handlers)
                        // to prevent Spinner::Drop from clearing the prompt line after we re-render.
                        // Spinner::Drop writes \r\x1b[K asynchronously after abort() returns — if we
                        // continue immediately and re-draw the prompt, the late Drop clears it, leaving
                        // the user with an invisible prompt until they type.
                        {
                            let mut task = agent_task.lock().await;
                            if let Some(t) = task.take() {
                                t.abort();
                                // Wait for task to fully unwind (Drop handlers run after abort)
                                // Use timeout to avoid hanging if task doesn't respond
                                let _ = tokio::time::timeout(
                                    std::time::Duration::from_millis(500),
                                    t
                                ).await;
                            }
                        }
                        // Ensure busy flag is cleared even if abort skipped cleanup
                        agent_busy.store(false, Ordering::Relaxed);
                        input_buf.clear();
                        cursor_pos = 0;
                        writeln!(stdout, "^C")?;
                    } else if input_buf.is_empty() {
                        writeln!(stdout, "^C")?;
                        cleanup_terminal(raw_guard.take())?;
                        return Ok(());
                    } else {
                        input_buf.clear();
                        cursor_pos = 0;
                        writeln!(stdout, "^C")?;
                    }
                }
                TerminalEvent::Ctrl('a') | TerminalEvent::Home => {
                    cursor_pos = 0;
                }
                TerminalEvent::Ctrl('e') | TerminalEvent::End => {
                    cursor_pos = input_buf.len();
                }
                TerminalEvent::Ctrl('u') => {
                    input_buf.drain(..cursor_pos);
                    cursor_pos = 0;
                }
                TerminalEvent::Ctrl('k') => {
                    input_buf.drain(cursor_pos..);
                }
                TerminalEvent::Ctrl('w') if cursor_pos > 0 => {
                    cursor_pos = delete_word_backward(&mut input_buf, cursor_pos);
                }
                TerminalEvent::Resize { cols, rows } => {
                    set_scroll_region(&mut stdout, rows)?;
                    let _ = (cols, rows);
                    continue 'main;
                }
                _ => {}
            }
        }
    }

    cleanup_terminal(raw_guard.take())?;
    Ok(())
}

pub fn should_start_interactive_shell(
    has_prompt: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    json: bool,
) -> bool {
    !has_prompt && stdin_is_tty && stdout_is_tty && !json
}

pub fn render_interactive_prompt_prefix() -> String {
    crate::cli::colors::colorize(
        "❯ ",
        &format!(
            "{}{}",
            crate::cli::colors::style::BOLD,
            crate::cli::colors::style::GREEN
        ),
    )
}

pub fn print_undo_result(added: Vec<String>, modified: Vec<String>) {
    if !added.is_empty() {
        eprintln!("Deleted {} file(s) created in last turn:", added.len());
        for f in &added {
            eprintln!("  - {}", f);
        }
    }
    if !modified.is_empty() {
        eprintln!("Restored {} file(s) to previous state:", modified.len());
        for f in &modified {
            eprintln!("  - {}", f);
        }
    }
    if added.is_empty() && modified.is_empty() {
        eprintln!("No changes to undo.");
    } else {
        eprintln!("Undone last turn.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(150)), "2m 30s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 0m");
        assert_eq!(format_duration(Duration::from_secs(3660)), "1h 1m");
        assert_eq!(format_duration(Duration::from_secs(4500)), "1h 15m");
        assert_eq!(format_duration(Duration::from_secs(7320)), "2h 2m");
    }

    #[test]
    fn test_resume_summary_section_header_format() {
        // Verify section_header format for resume summary
        let header = crate::cli::colors::section_header("Resumed task abc123 · 3 turns");
        assert!(header.contains("Resumed task"));
        assert!(header.contains("3 turns"));
        assert!(header.contains("═══"));
    }

    #[test]
    fn test_display_width_ascii() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("a"), 1);
    }

    #[test]
    fn test_display_width_cjk() {
        assert_eq!(display_width("你好"), 4);
        assert_eq!(display_width("你好 hello"), 10);
        assert_eq!(display_width("🎉"), 2);
        assert_eq!(display_width("🎉hello"), 7);
    }

    #[test]
    fn test_ctrl_w_deletes_word_cjk() {
        let mut buf = String::from("hello 世界");
        let len = buf.len();
        let pos = delete_word_backward(&mut buf, len);
        assert_eq!(buf, "hello ");
        assert_eq!(pos, 6);
    }

    #[test]
    fn test_ctrl_w_deletes_word_with_multiple_spaces() {
        let mut buf = String::from("hello   世界");
        let len = buf.len();
        let pos = delete_word_backward(&mut buf, len);
        assert_eq!(buf, "hello   ");
        assert_eq!(pos, 8);
    }

    #[test]
    fn test_ctrl_w_deletes_single_word() {
        let mut buf = String::from("hello");
        let pos = delete_word_backward(&mut buf, 5);
        assert_eq!(buf, "");
        assert_eq!(pos, 0);
    }

    #[test]
    fn test_ctrl_w_no_whitespace_deletes_all() {
        let mut buf = String::from("hello世界test");
        let len = buf.len();
        let pos = delete_word_backward(&mut buf, len);
        assert_eq!(buf, "");
        assert_eq!(pos, 0);
    }

    #[test]
    fn test_ctrl_w_emoji_word() {
        let mut buf = String::from("hello 🎉🎊");
        let len = buf.len();
        let pos = delete_word_backward(&mut buf, len);
        assert_eq!(buf, "hello ");
        assert_eq!(pos, 6);
    }

    #[test]
    fn test_input_lines_calculation_cjk() {
        let term_cols: u16 = 80;
        let prompt_prefix = "❯ ";
        let input_buf = "你好 hello";
        let full_input = format!("{}{}", prompt_prefix, input_buf);
        let input_lines = (display_width(&full_input) as u16).div_ceil(term_cols);
        let input_lines = input_lines.max(1);
        assert_eq!(input_lines, 1);

        let long_cjk = "你好世界 hello world test";
        let full_input = format!("{}{}", prompt_prefix, long_cjk);
        let input_lines = (display_width(&full_input) as u16).div_ceil(term_cols);
        let input_lines = input_lines.max(1);
        assert_eq!(input_lines, 1);

        let very_long_cjk =
            "你好世界 hello world test 你好世界 hello world test 你好世界 hello world test 你好";
        let full_input = format!("{}{}", prompt_prefix, very_long_cjk);
        let input_lines = (display_width(&full_input) as u16).div_ceil(term_cols);
        let input_lines = input_lines.max(1);
        assert!(
            input_lines > 1,
            "Long CJK input should wrap to multiple lines"
        );
    }

    #[test]
    fn test_cursor_move_left_cjk() {
        let input_buf = "你好 hello";
        let cursor_pos = 7;
        let right_of_cursor = &input_buf[cursor_pos..];
        let move_left = display_width(right_of_cursor);
        assert_eq!(move_left, 5);

        let cursor_pos = 0;
        let right_of_cursor = &input_buf[cursor_pos..];
        let move_left = display_width(right_of_cursor);
        assert_eq!(move_left, 10);
    }
}
