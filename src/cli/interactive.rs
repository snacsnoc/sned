//! Interactive shell implementation.
//!
//! Extracted from `cli/mod.rs` — handles raw mode, terminal rendering,
//! file picker, input queuing, and agent lifecycle.

use crate::cli::{RootOnlyOptions, TaskOptions};
use crate::cli::output::{OutputEvent, OutputWriterArc, ChannelOutputWriter};
use crate::cli::tui::{App, ansi_to_ratatui_lines};
use futures::FutureExt;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
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
    // 1. Initialize ratatui (replaces enter_raw_mode, scroll_region, bracketed paste)
    let mut terminal = ratatui::init();
    let mut app = App::new();
    
    // 2. Create output channel (Phase 1 infrastructure, now drains to App)
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();
    let output_writer: OutputWriterArc = Arc::new(ChannelOutputWriter::new(output_tx));
    
    // 3. Build session
    let session = Arc::new(Mutex::new(
        InteractiveSession::build_with_writer(
            task_opts.clone(), 
            root_opts.clone(), 
            Some(output_writer.clone())
        ).await?,
    ));
    
    let task_id = {
        let sess = session.lock().await;
        sess.agent_loop.task_id().to_string()
    };
    
    // 4. Startup banner → app.push_output()
    {
        let sess = session.lock().await;
        if !sess.is_quiet() {
            let startup_info = sess.get_startup_info();
            app.push_plain(startup_info);
            app.push_styled("type a prompt and press Enter; type /exit to leave", 
                Style::default().add_modifier(Modifier::DIM));
            app.push_styled("type /help for slash commands, @ to search and mention files",
                Style::default().add_modifier(Modifier::DIM));
        }
    }
    
    // 5. Shared state (same as current)
    let agent_busy = Arc::new(AtomicBool::new(false));
    let agent_done = Arc::new(tokio::sync::Notify::new());
    let agent_start_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let queue_handle: Arc<Mutex<Option<crate::core::agent_loop::MessageQueueHandle>>> =
        Arc::new(Mutex::new(None));
    let state_handle: Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>> =
        Arc::new(Mutex::new(None));
    let agent_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(None));
    
    // Load command history (for Phase 3.3 integration with tui-textarea)
    let _command_history = load_command_history();
    
    {
        let sess = session.lock().await;
        let mut qh = queue_handle.lock().await;
        *qh = Some(sess.queue_handle());
        let mut sh = state_handle.lock().await;
        *sh = Some(sess.state_handle());
    }
    
    // 6. Main loop
    let result = run_main_loop(
        &mut terminal,
        &mut app,
        &mut output_rx,
        session,
        task_id,
        agent_busy,
        agent_done,
        agent_start_time,
        state_handle,
        agent_task,
    ).await;
    
    // 7. Always restore terminal
    ratatui::restore();
    result
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
