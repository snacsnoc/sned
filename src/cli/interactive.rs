//! Interactive shell implementation.
//!
//! Extracted from `cli/mod.rs` — handles raw mode, terminal rendering,
//! file picker, input queuing, and agent lifecycle.

use crate::cli::output::{ChannelOutputWriter, OutputEvent, OutputWriterArc};
use crate::cli::tui::history::append_to_history;
use crate::cli::tui::{App, ansi_to_ratatui_lines, format_duration, theme};
use crate::cli::{RootOnlyOptions, TaskOptions};
use crate::core::approval::{ApprovalResult, is_approval_prompt_active, take_approval_sender};
use futures::FutureExt;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::style::Style;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};

/// RAII guard that restores ratatui terminal state on drop.
/// Prevents terminal from being left in alternate screen on early returns or errors.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Disable bracketed paste and mouse capture before restoring terminal
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture
        );
        ratatui::restore();
    }
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
    /// Get a reference to the underlying AgentLoop.
    pub fn agent_loop(&self) -> &crate::core::agent_loop::AgentLoop {
        &self.agent_loop
    }

    /// Get a mutable reference to the underlying AgentLoop.
    pub fn agent_loop_mut(&mut self) -> &mut crate::core::agent_loop::AgentLoop {
        &mut self.agent_loop
    }

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
            crate::cli::build_task_components(task_opts.clone(), root_opts.clone(), output_writer)
                .await?;
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

    /// Get the message queue handle for checking queued messages.
    pub fn message_queue_handle(&self) -> Option<crate::core::agent_loop::MessageQueueHandle> {
        Some(self.agent_loop.message_queue_handle())
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
    async fn print_resume_summary(
        agent: &crate::core::agent_loop::AgentLoop,
        writer: &crate::cli::output::OutputWriterArc,
    ) {
        use crate::cli::output::OutputEvent;
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

        writer.emit(OutputEvent::RawAnsi(format!(
            "{}\n",
            crate::cli::colors::section_header(&format!(
                "Resumed task {} · {} turn{}",
                agent.task_id(),
                turns_completed,
                if turns_completed == 1 { "" } else { "s" }
            ))
        )));

        if let Some(action) = last_action {
            writer.emit(OutputEvent::RawAnsi(format!(
                "{}\n",
                crate::cli::colors::colorize(
                    &format!("  📌 Last action: {}", action),
                    crate::cli::colors::style::DIM
                )
            )));
        }

        if files_tracked > 0 {
            writer.emit(OutputEvent::RawAnsi(format!(
                "{}\n",
                crate::cli::colors::colorize(
                    &format!("  📁 Files changed: {}", files_tracked),
                    crate::cli::colors::style::DIM
                )
            )));
        }

        writer.emit(OutputEvent::RawAnsi(format!(
            "{}\n",
            crate::cli::colors::colorize(
                &format!("  📊 Tokens: {}", total_tokens),
                crate::cli::colors::style::DIM
            )
        )));

        crate::cli::colors::print_horizontal_rule_writer(writer);
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
                Self::print_resume_summary(agent, agent.output_writer()).await;
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
                    agent
                        .output_writer()
                        .emit(crate::cli::output::OutputEvent::warning(format!(
                            "Model '{}' does not support images. Ignoring {} image(s).",
                            model_info.name.as_deref().unwrap_or("unknown"),
                            all_image_paths.len()
                        )));
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

/// Action returned by key event handler.
enum Action {
    Submit(String),
}

fn is_shutdown_submit(text: &str) -> bool {
    crate::cli::slash_commands::get_cli_only_command(text).is_some_and(|cmd| cmd.is_shutdown())
}

/// Drain output channel into app buffer.
fn drain_output(rx: &mut mpsc::Receiver<OutputEvent>, app: &mut App) {
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
    // Keep approval prompts visible even if the user had manually scrolled away.
    if crate::core::approval::is_approval_prompt_active() {
        app.auto_scroll = true;
        app.scroll_offset = 0;
    }
    // Also force the initial prompt render once even if it arrives between frames.
    if crate::core::approval::take_approval_prompt_scroll() {
        app.auto_scroll = true;
        app.scroll_offset = 0;
    }
}

fn approval_result_for_key(key: &KeyEvent) -> Option<ApprovalResult> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(ApprovalResult::Denied)
        }
        KeyCode::Char('y' | 'Y') => Some(ApprovalResult::Approved),
        KeyCode::Char('n' | 'N') => Some(ApprovalResult::Denied),
        KeyCode::Char('a' | 'A') => Some(ApprovalResult::Always),
        KeyCode::Esc => Some(ApprovalResult::Denied),
        _ => None,
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
///
/// Uses the same graceful shutdown sequence as CancellationHandler::abort_task:
/// SIGTERM → 100ms wait → SIGKILL. This gives running commands a chance to
/// clean up (flush output, close files, etc.) before being force-killed.
async fn cancel_agent(
    state_handle: &Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    agent_done: &Arc<tokio::sync::Notify>,
) -> anyhow::Result<()> {
    if let Some(sh) = state_handle.lock().await.as_ref() {
        let mut state = sh.lock().await;
        state.is_cancelled = true;
        state.is_cancelled_atomic.store(true, Ordering::Release);

        #[cfg(unix)]
        {
            let pids = state.running_command_pids.clone();

            for pid in &pids {
                if unsafe { libc::kill(-*pid, 0) } == 0 {
                    let _ = unsafe { libc::kill(-*pid, libc::SIGTERM) };
                }
            }

            drop(state);

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let mut state = sh.lock().await;
            for pid in &pids {
                if unsafe { libc::kill(-*pid, 0) } == 0 {
                    let _ = unsafe { libc::kill(-*pid, libc::SIGKILL) };
                }
            }
            state.running_command_pids.clear();
        }

        #[cfg(not(unix))]
        {
            state.running_command_pids.clear();
        }
    }

    if let Some(task) = agent_task.lock().await.take() {
        task.abort();
        tokio::time::timeout(Duration::from_secs(2), async {
            agent_done.notified().await
        })
        .await
        .ok();
    }

    Ok(())
}

/// Handle key events in ratatui loop (non-Ctrl+C keys).
async fn handle_key_event(
    key: KeyEvent,
    app: &mut App,
    session: &Arc<Mutex<InteractiveSession>>,
    task_id: &str,
) -> anyhow::Result<Option<Action>> {
    use crate::core::approval::{is_followup_question_active, take_followup_sender};

    // Tab or Enter with active file picker -> insert selection (must come before Enter handler)
    if app.picker_active
        && !app.picker_results.is_empty()
        && (key.code == KeyCode::Tab || key.code == KeyCode::Enter)
    {
        let text = app.input.lines().join("\n");
        let mq = crate::core::file_search::extract_mention_query(&text);
        if mq.in_mention_mode {
            let result = &app.picker_results[app.picker_index];
            let (new_text, cursor_pos) =
                crate::core::file_search::insert_mention(&text, mq.at_index as usize, &result.path);
            app.input = App::new_textarea(vec![new_text]);
            app.input
                .move_cursor(tui_textarea::CursorMove::Jump(0, cursor_pos as u16));
            app.picker_active = false;
            app.picker_results.clear();
            return Ok(None);
        }
        // Picker active but mention mode lost — dismiss picker and fall through
        app.picker_active = false;
        app.picker_results.clear();
        // Fall through to normal Enter/Tab handling
    }

    // Enter key - intercept before passing to textarea
    if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
        // Check for followup question (used by /undo, /commit, /checkpoint-restore)
        if is_followup_question_active(task_id) {
            if let Some(sender) = take_followup_sender(task_id) {
                let text = app.get_input_with_expanded_pastes();
                // Echo followup response to output pane
                {
                    let sess = session.lock().await;
                    let writer = sess.agent_loop().output_writer();
                    app.push_user_message(&text, writer);
                }
                let _ = sender.send(text);
                app.input = App::new_textarea(Vec::new());
            }
            return Ok(None);
        }

        // Normal submit - expand all paste markers before sending
        let text = app.get_input_with_expanded_pastes();
        if !text.is_empty() {
            // Shutdown commands should bypass the session echo lock so /quit still works
            // even if the agent is currently holding the session mutex.
            if is_shutdown_submit(&text) {
                app.input = App::new_textarea(Vec::new());
                app.clear_pastes();
                return Ok(Some(Action::Submit(text)));
            }

            // Turn separator before user message (only if a previous turn completed)
            // Check if output already has a turn separator (from previous agent completion)
            if !app.output_lines.is_empty()
                && app.output_lines.back().is_some_and(|line| {
                    line.spans
                        .first()
                        .is_some_and(|span| span.content.as_ref().starts_with('─'))
                })
            {
                app.push_turn_separator();
            }
            // Echo prompt to output pane
            {
                let sess = session.lock().await;
                let writer = sess.agent_loop().output_writer();
                app.push_user_message(&text, writer);
            }
            // Clear textarea and paste tracking
            app.input = App::new_textarea(Vec::new());
            app.clear_pastes();
            // Submit to agent
            return Ok(Some(Action::Submit(text)));
        }
        return Ok(None);
    }

    // PageUp/PageDown for scrolling
    if key.code == KeyCode::PageUp {
        if app.auto_scroll {
            let total = app.output_lines.len();
            app.scroll_offset = total.saturating_sub(app.last_content_height) as u16;
        }
        app.auto_scroll = false;
        app.scroll_offset = app.scroll_offset.saturating_sub(10);
        return Ok(None);
    }
    if key.code == KeyCode::PageDown {
        if app.auto_scroll {
            let total = app.output_lines.len();
            app.scroll_offset = total.saturating_sub(app.last_content_height) as u16;
        }
        app.auto_scroll = false;
        app.scroll_offset = app.scroll_offset.saturating_add(10);
        return Ok(None);
    }

    // Handle pending clear confirmation
    if app.pending_clear.is_some() {
        if key.code == KeyCode::Char('y')
            || key.code == KeyCode::Char('Y')
            || key.code == KeyCode::Enter
        {
            app.output_lines.clear();
            app.scroll_offset = 0;
            app.auto_scroll = true;
            let trigger = app.pending_clear.take().unwrap();
            app.push_plain(format!("Conversation cleared (confirmed via {}).", trigger));
        } else {
            app.pending_clear = None;
            app.push_styled("Clear cancelled.", theme::dim_style());
        }
        return Ok(None);
    }

    // Shift+Up/Down for manual scroll
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        if key.code == KeyCode::Up {
            if app.auto_scroll {
                let total = app.output_lines.len();
                app.scroll_offset = total.saturating_sub(app.last_content_height) as u16;
            }
            app.auto_scroll = false;
            app.scroll_offset = app.scroll_offset.saturating_sub(1);
            return Ok(None);
        }
        if key.code == KeyCode::Down {
            if app.auto_scroll {
                let total = app.output_lines.len();
                app.scroll_offset = total.saturating_sub(app.last_content_height) as u16;
            }
            app.scroll_offset = app.scroll_offset.saturating_add(1);
            return Ok(None);
        }
    }

    // Up/Down for command history navigation (only when picker is not active)
    // Always allow history navigation regardless of cursor position
    if key.code == KeyCode::Up && !app.picker_active {
        if !app.history.is_navigating() {
            app.history_draft = Some(app.input.lines().join("\n"));
        }
        if let Some(entry) = app.history.navigate_up() {
            app.input = App::new_textarea(vec![entry.to_string()]);
        }
        return Ok(None);
    }
    if key.code == KeyCode::Down && !app.picker_active && app.history.is_navigating() {
        if let Some(entry) = app.history.navigate_down() {
            app.input = App::new_textarea(vec![entry.to_string()]);
        } else {
            let draft = app.history_draft.take().unwrap_or_default();
            app.input = if draft.is_empty() {
                App::new_textarea(Vec::new())
            } else {
                App::new_textarea(draft.split('\n').map(|s| s.to_string()).collect())
            };
        }
        return Ok(None);
    }

    // Up/Down for file picker navigation (when picker is active)
    if app.picker_active && !app.picker_results.is_empty() {
        if key.code == KeyCode::Up {
            app.picker_index = app.picker_index.saturating_sub(1);
            return Ok(None);
        }
        if key.code == KeyCode::Down {
            app.picker_index = (app.picker_index + 1).min(app.picker_results.len() - 1);
            return Ok(None);
        }
    }

    // Escape key - dismiss picker or clear input mode
    if key.code == KeyCode::Esc && app.picker_active {
        app.picker_active = false;
        app.picker_results.clear();
        return Ok(None);
    }

    // Ctrl+L - clear output screen (with confirmation)
    if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.pending_clear = Some("ctrl_l".to_string());
        app.push_styled(
            "Clear output? (y to confirm, any other key to cancel): ",
            Style::default().fg(theme::WARNING_FG),
        );
        return Ok(None);
    }

    // Ctrl+A - move cursor to start of line
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.input.move_cursor(tui_textarea::CursorMove::Head);
        return Ok(None);
    }

    // Ctrl+E - move cursor to end of line
    if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.input.move_cursor(tui_textarea::CursorMove::End);
        return Ok(None);
    }

    // All other keys go to textarea
    use tui_textarea::Input;
    app.input.input(Input::from(key));

    // Check for @ mention mode - show file picker overlay
    let input_text = app.input.lines().join("\n");
    let mq = crate::core::file_search::extract_mention_query(&input_text);
    if mq.in_mention_mode && !app.cwd.is_empty() {
        let query = mq.query.clone();
        let cwd = app.cwd.clone();
        let results = crate::core::file_search::search_workspace_files(&query, &cwd, 10).await;
        app.picker_active = true;
        app.picker_results = results;
        app.picker_index = 0;
    } else {
        app.picker_active = false;
        app.picker_results.clear();
    }
    Ok(None)
}

/// Handle CLI-only slash commands, routing output to the App buffer.
/// Returns `true` if the caller should exit the main loop (for /exit, /quit).
async fn handle_cli_only_command(
    cli_cmd: crate::cli::slash_commands::CliOnlyCommand,
    text: &str,
    app: &mut App,
    session: &Arc<Mutex<InteractiveSession>>,
    task_id: &str,
    agent_busy: &Arc<AtomicBool>,
    agent_done: &Arc<tokio::sync::Notify>,
    agent_start_time: &Arc<Mutex<Option<Instant>>>,
    agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    _state_handle: &Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    task_opts: &TaskOptions,
    auto_approve: bool,
) -> anyhow::Result<bool> {
    use crate::cli::slash_commands::{
        CliOnlyCommand, format_changes_text, format_help_for_command, format_help_text,
        format_settings_text, format_stats_text,
    };

    // Local commands execute immediately; only agent-required commands are blocked
    if agent_busy.load(Ordering::Relaxed) && !cli_cmd.is_local_command() {
        app.push_styled(
            "Agent is busy. Wait for it to finish before running this command.",
            Style::default().fg(theme::WARNING_FG),
        );
        return Ok(false);
    }

    match cli_cmd {
        CliOnlyCommand::Exit | CliOnlyCommand::Quit => {
            return Ok(true);
        }
        CliOnlyCommand::Clear => {
            app.pending_clear = Some("slash".to_string());
            app.push_styled(
                "Clear output? (y to confirm, any other key to cancel): ",
                Style::default().fg(theme::WARNING_FG),
            );
        }
        CliOnlyCommand::History => {
            let last_n: Vec<String> = app
                .history
                .entries()
                .iter()
                .rev()
                .take(10)
                .cloned()
                .collect();
            if last_n.is_empty() {
                app.push_plain("No command history.");
            } else {
                app.push_plain("Recent history (last 10):");
                for (i, entry) in last_n.iter().rev().enumerate() {
                    app.push_plain(format!("  {}  {}", i + 1, entry));
                }
            }
        }
        CliOnlyCommand::Skills => {
            let skills_text = if let Ok(cwd) = std::env::current_dir() {
                let project_skills = crate::core::context::discover_skills(&cwd);
                let all_skills = crate::core::context::get_available_skills(project_skills);
                if all_skills.is_empty() {
                    "No skills found.".to_string()
                } else {
                    let mut lines = vec!["Available Skills:".to_string(), String::new()];
                    for skill in all_skills {
                        lines.push(format!("  {} - {}", skill.name, skill.description));
                    }
                    lines.join("\n")
                }
            } else {
                "No skills found.".to_string()
            };
            for line in ansi_to_ratatui_lines(&skills_text) {
                app.push_output(line);
            }
        }
        CliOnlyCommand::Help => {
            let help_text = format_help_text();
            for line in ansi_to_ratatui_lines(&help_text) {
                app.push_output(line);
            }
        }
        CliOnlyCommand::HelpOption(cmd) => {
            let help_text = format_help_for_command(&cmd);
            for line in ansi_to_ratatui_lines(&help_text) {
                app.push_output(line);
            }
        }
        CliOnlyCommand::Settings => {
            let provider = task_opts.provider.as_deref().unwrap_or("anthropic");
            let model = task_opts.model.as_deref().unwrap_or("claude-3-5-sonnet");
            let mode = if task_opts.plan { "plan" } else { "act" };
            let settings_text = format_settings_text(provider, model, mode, auto_approve);
            for line in ansi_to_ratatui_lines(&settings_text) {
                app.push_output(line);
            }
        }
        CliOnlyCommand::Models => {
            let models_text = crate::cli::slash_commands::format_models_text();
            for line in ansi_to_ratatui_lines(&models_text) {
                app.push_output(line);
            }
        }
        CliOnlyCommand::ResetCompact => {
            let mut sess = session.lock().await;
            if sess.clear_compacted_summary().await {
                app.push_plain("Compacted summary cleared. You can now use /compact again.");
            } else {
                app.push_plain("No compacted summary to clear.");
            }
        }
        CliOnlyCommand::Stats => {
            let sess = session.lock().await;
            let sh = sess.agent_loop().state_handle();
            let state = sh.lock().await;
            let stats = format_stats_text(&state);
            app.push_plain(stats);
        }
        CliOnlyCommand::Changes => {
            let sess = session.lock().await;
            let sh = sess.agent_loop().state_handle();
            let state = sh.lock().await;
            let changes = format_changes_text(&state);
            app.push_plain(changes);
        }
        CliOnlyCommand::Queue => {
            let sess = session.lock().await;
            if let Some(qh) = sess.message_queue_handle() {
                let count = qh.queued_message_count().await;
                if count == 0 {
                    app.push_plain("No messages queued.");
                } else {
                    app.push_plain(format!("{} message(s) queued:", count));
                    // Show queue preview (first few messages)
                    let messages = qh.peek_queued_messages(3).await;
                    for (i, msg) in messages.iter().enumerate() {
                        let preview = if msg.len() > 60 {
                            format!("{}...", &msg[..60])
                        } else {
                            msg.clone()
                        };
                        app.push_plain(format!("  {}. {}", i + 1, preview));
                    }
                    if count > 3 {
                        app.push_plain(format!("  ... and {} more", count - 3));
                    }
                }
            } else {
                app.push_plain("No message queue available.");
            }
        }
        CliOnlyCommand::Undo | CliOnlyCommand::CheckpointUndo => {
            let sess = session.lock().await;
            let checkpoint_mgr = sess
                .agent_loop()
                .checkpoint_manager()
                .expect("checkpoint manager should be initialized");
            let checkpoints = match checkpoint_mgr.list_checkpoints().await {
                Ok(cps) => cps,
                Err(e) => {
                    app.push_plain(format!("Failed to list checkpoints: {}", e));
                    return Ok(false);
                }
            };

            if checkpoints.is_empty() {
                app.push_plain("No checkpoints available to undo.");
                return Ok(false);
            }

            let most_recent = &checkpoints[0];
            let current_hash = checkpoint_mgr.last_checkpoint().map(|h| h.as_str());
            let changed_files = if let Some(current) = current_hash {
                checkpoint_mgr
                    .get_changed_files(&most_recent.hash, Some(current))
                    .await
                    .unwrap_or_else(|_| vec![])
            } else {
                vec![]
            };

            if !changed_files.is_empty() {
                app.push_styled(
                    "/undo will revert the following files to the previous checkpoint:",
                    Style::default().fg(theme::WARNING_FG),
                );
                for f in &changed_files {
                    app.push_plain(format!("  - {}", f));
                }
                app.push_plain("Continue? (y to cancel, Enter to confirm): ");

                let (sender, receiver) = std::sync::mpsc::channel();
                crate::core::approval::set_followup_question_active(task_id, true);
                crate::core::approval::set_followup_sender(task_id, sender);

                let response_result = tokio::task::spawn_blocking(move || {
                    receiver.recv_timeout(std::time::Duration::from_secs(30))
                })
                .await;

                crate::core::approval::clear_followup_sender(task_id);
                crate::core::approval::set_followup_question_active(task_id, false);

                let confirm = match response_result {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) | Err(_) => String::new(),
                };

                // Timeout or error: default to cancel (safe default)
                if confirm.trim().is_empty() {
                    app.push_styled(
                        "Confirmation timeout — cancelled.",
                        Style::default().fg(theme::WARNING_FG),
                    );
                    return Ok(false);
                }

                if confirm.trim().to_lowercase() == "y" {
                    app.push_styled("Undo cancelled.", Style::default().fg(theme::WARNING_FG));
                    return Ok(false);
                }
            }

            match checkpoint_mgr.restore_checkpoint(&most_recent.hash).await {
                Ok(()) => {
                    app.push_plain(format!(
                        "Restored to checkpoint {} — {} file(s) reverted",
                        most_recent.number,
                        changed_files.len()
                    ));
                    if !changed_files.is_empty() {
                        app.push_plain("\nReverted files:");
                        for f in &changed_files {
                            app.push_plain(format!("  - {}", f));
                        }
                    }
                    let removed = sess.agent_loop().remove_last_turn().await;
                    if removed > 0 {
                        app.push_plain(format!(
                            "Removed {} message(s) from conversation history.",
                            removed
                        ));
                    }
                }
                Err(e) => {
                    app.push_styled(
                        format!("Undo failed: {}", e),
                        Style::default().fg(theme::ERROR_FG),
                    );
                }
            }
        }
        CliOnlyCommand::Diff => {
            if let Ok(workspace_root) = std::env::current_dir() {
                if !crate::core::shadow_git::is_initialized(&workspace_root) {
                    app.push_plain("Change tracking is not enabled. Use --track-changes to enable automatic undo/versioning.");
                } else {
                    match crate::core::shadow_git::diff_turns(&workspace_root, 1, 0) {
                        Ok(diff) => {
                            if diff.is_empty() {
                                app.push_plain("No changes.");
                            } else {
                                for line in ansi_to_ratatui_lines(&diff) {
                                    app.push_output(line);
                                }
                            }
                        }
                        Err(e) => {
                            app.push_styled(
                                format!("Failed to get diff: {}", e),
                                Style::default().fg(theme::ERROR_FG),
                            );
                        }
                    }
                }
            }
        }
        CliOnlyCommand::Log => {
            if let Ok(workspace_root) = std::env::current_dir() {
                if !crate::core::shadow_git::is_initialized(&workspace_root) {
                    app.push_plain("Change tracking is not enabled. Use --track-changes to enable automatic undo/versioning.");
                } else {
                    match crate::core::shadow_git::log(&workspace_root, Some(10)) {
                        Ok(log) => {
                            if log.is_empty() {
                                app.push_plain("No log entries.");
                            } else {
                                for line in ansi_to_ratatui_lines(&log) {
                                    app.push_output(line);
                                }
                            }
                        }
                        Err(e) => {
                            app.push_styled(
                                format!("Failed to get log: {}", e),
                                Style::default().fg(theme::ERROR_FG),
                            );
                        }
                    }
                }
            }
        }
        CliOnlyCommand::Commit => {
            let commit_msg = if text.starts_with("/commit ") {
                text.strip_prefix("/commit ")
                    .map(|s| s.trim_matches('"').trim_matches('\'').to_string())
            } else {
                None
            };

            if let Some(msg) = commit_msg {
                if let Ok(workspace_root) = std::env::current_dir() {
                    if !crate::core::shadow_git::is_initialized(&workspace_root) {
                        app.push_plain("Change tracking is not enabled. Use --track-changes to enable automatic undo/versioning.");
                    } else {
                        match crate::core::shadow_git::diff_turns(&workspace_root, 1, 0) {
                            Ok(diff) => {
                                if diff.is_empty() {
                                    app.push_plain("No changes to commit.");
                                } else {
                                    app.push_styled(
                                        "Changes to commit:",
                                        Style::default().fg(theme::ACCENT),
                                    );
                                    for line in ansi_to_ratatui_lines(&diff) {
                                        app.push_output(line);
                                    }
                                    app.push_plain("Commit to your git repo? (y/n): ");

                                    let (sender, receiver) = std::sync::mpsc::channel();
                                    crate::core::approval::set_followup_question_active(
                                        task_id, true,
                                    );
                                    crate::core::approval::set_followup_sender(task_id, sender);

                                    let response_result = tokio::task::spawn_blocking(move || {
                                        receiver.recv_timeout(std::time::Duration::from_secs(30))
                                    })
                                    .await;

                                    crate::core::approval::clear_followup_sender(task_id);
                                    crate::core::approval::set_followup_question_active(
                                        task_id, false,
                                    );

                                    let confirm = match response_result {
                                        Ok(Ok(r)) => r,
                                        Ok(Err(_)) | Err(_) => String::new(),
                                    };

                                    // Timeout or error: default to cancel (safe default)
                                    if confirm.trim().is_empty() {
                                        app.push_styled(
                                            "Confirmation timeout — cancelled.",
                                            Style::default().fg(theme::WARNING_FG),
                                        );
                                    } else if confirm.trim().to_lowercase() == "y" {
                                        match crate::core::shadow_git::commit_to_real_git(
                                            &workspace_root,
                                            &msg,
                                        ) {
                                            Ok(files) => {
                                                app.push_plain(format!(
                                                    "Committed {} file(s) to your git repo.",
                                                    files.len()
                                                ));
                                            }
                                            Err(e) => {
                                                app.push_styled(
                                                    format!("Commit failed: {}", e),
                                                    Style::default().fg(theme::ERROR_FG),
                                                );
                                            }
                                        }
                                    } else {
                                        app.push_styled(
                                            "Commit cancelled.",
                                            Style::default().fg(theme::WARNING_FG),
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                app.push_styled(
                                    format!("Failed to get diff: {}", e),
                                    Style::default().fg(theme::ERROR_FG),
                                );
                            }
                        }
                    }
                }
            } else {
                app.push_plain("Usage: /commit <message>");
            }
        }
        CliOnlyCommand::CheckpointList => {
            let sess = session.lock().await;
            let checkpoint_mgr = sess
                .agent_loop()
                .checkpoint_manager()
                .expect("checkpoint manager should be initialized");
            match checkpoint_mgr.list_checkpoints().await {
                Ok(checkpoints) => {
                    if checkpoints.is_empty() {
                        app.push_plain("No checkpoints found.");
                    } else {
                        app.push_plain("Available checkpoints:");
                        app.push_plain("  #  Hash      Message");
                        app.push_plain("  ──────────────────────────");
                        for cp in checkpoints.iter().rev() {
                            app.push_plain(format!("  {}  {}  {}", cp.number, cp.hash, cp.message));
                        }
                    }
                }
                Err(e) => {
                    app.push_plain(format!("Failed to list checkpoints: {}", e));
                }
            }
        }
        CliOnlyCommand::CheckpointRestore => {
            let sess = session.lock().await;
            let checkpoint_mgr = sess
                .agent_loop()
                .checkpoint_manager()
                .expect("checkpoint manager should be initialized");
            let checkpoints = match checkpoint_mgr.list_checkpoints().await {
                Ok(cps) => cps,
                Err(e) => {
                    app.push_plain(format!("Failed to list checkpoints: {}", e));
                    return Ok(false);
                }
            };

            if checkpoints.is_empty() {
                app.push_plain("No checkpoints to restore.");
                return Ok(false);
            }

            let checkpoint_num = crate::cli::slash_commands::parse_checkpoint_restore(text);
            let num = if let Some(n) = checkpoint_num {
                n
            } else {
                app.push_plain("Available checkpoints:");
                app.push_plain("  #  Hash      Message");
                app.push_plain("  ──────────────────────────");
                for cp in checkpoints.iter().rev() {
                    app.push_plain(format!("  {}  {}  {}", cp.number, cp.hash, cp.message));
                }
                app.push_plain("Enter checkpoint number to restore:");

                let (sender, receiver) = std::sync::mpsc::channel();
                crate::core::approval::set_followup_question_active(task_id, true);
                crate::core::approval::set_followup_sender(task_id, sender);

                let response_result = tokio::task::spawn_blocking(move || {
                    receiver.recv_timeout(std::time::Duration::from_secs(30))
                })
                .await;

                crate::core::approval::clear_followup_sender(task_id);
                crate::core::approval::set_followup_question_active(task_id, false);

                let input = match response_result {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) | Err(_) => String::new(),
                };
                input.trim().parse::<usize>().unwrap_or(0)
            };

            if num == 0 || num > checkpoints.len() {
                app.push_plain(format!(
                    "Invalid checkpoint number. Available: 1-{}",
                    checkpoints.len()
                ));
                return Ok(false);
            }

            if let Some(checkpoint) = checkpoints.get(num - 1) {
                let current_hash = checkpoint_mgr
                    .last_checkpoint()
                    .map(|h| h.as_str())
                    .unwrap_or("HEAD");
                match checkpoint_mgr
                    .get_changed_files(&checkpoint.hash, Some(current_hash))
                    .await
                {
                    Ok(changed_files) => {
                        if !changed_files.is_empty() {
                            app.push_styled(
                                "Files that will be restored:",
                                Style::default().fg(theme::WARNING_FG),
                            );
                            for file in &changed_files {
                                app.push_plain(format!("  - {}", file));
                            }
                            app.push_plain("Continue? (y to cancel, Enter to confirm): ");

                            let (sender, receiver) = std::sync::mpsc::channel();
                            crate::core::approval::set_followup_question_active(task_id, true);
                            crate::core::approval::set_followup_sender(task_id, sender);

                            let response_result = tokio::task::spawn_blocking(move || {
                                receiver.recv_timeout(std::time::Duration::from_secs(30))
                            })
                            .await;

                            crate::core::approval::clear_followup_sender(task_id);
                            crate::core::approval::set_followup_question_active(task_id, false);

                            let confirm = match response_result {
                                Ok(Ok(r)) => r,
                                Ok(Err(_)) | Err(_) => String::new(),
                            };

                            // Timeout or error: default to cancel (safe default)
                            if confirm.trim().is_empty() {
                                app.push_styled(
                                    "Confirmation timeout — cancelled.",
                                    Style::default().fg(theme::WARNING_FG),
                                );
                                return Ok(false);
                            }

                            if confirm.trim().to_lowercase() == "y" {
                                app.push_styled(
                                    "Restore cancelled.",
                                    Style::default().fg(theme::WARNING_FG),
                                );
                                return Ok(false);
                            }
                        }
                    }
                    Err(e) => {
                        app.push_plain(format!(
                            "Warning: Could not determine changed files: {}",
                            e
                        ));
                    }
                }

                match checkpoint_mgr.restore_by_number(num).await {
                    Ok(()) => {
                        app.push_plain(format!(
                            "Checkpoint {} ({}) restored successfully.",
                            num, checkpoint.hash
                        ));
                    }
                    Err(e) => {
                        app.push_plain(format!("Failed to restore checkpoint: {}", e));
                    }
                }
            }
        }
        CliOnlyCommand::Expand => {
            if let Some(index) = crate::cli::slash_commands::parse_expand_index(text) {
                let sess = session.lock().await;
                let sh = sess.agent_loop().state_handle();
                drop(sess);
                let state = sh.lock().await;
                if let Some(block) = state
                    .snipped_code_blocks
                    .iter()
                    .find(|block| block.index == index)
                {
                    if block.language.is_empty() {
                        app.push_plain("```");
                    } else {
                        app.push_plain(format!("```{}", block.language));
                    }
                    let highlighted =
                        crate::cli::syntax_highlight::highlight_code(&block.code, &block.language);
                    for line in ansi_to_ratatui_lines(&highlighted) {
                        app.push_output(line);
                    }
                    app.push_plain("```");
                } else {
                    app.push_plain(format!("No snipped code block {}.", index));
                }
            } else {
                app.push_plain("Usage: /expand N");
            }
        }
        CliOnlyCommand::PlanPrompt(_) => {
            app.push_plain("Plan prompt should be handled by the main loop.");
        }
        CliOnlyCommand::PlanAbort => {
            let mut sess = session.lock().await;
            let sh = sess.agent_loop().state_handle();
            let mut state = sh.lock().await;
            if state.plan_state.is_some() {
                state.plan_state = None;
                state.strict_plan_mode_enabled = true;
                drop(state);
                sess.agent_loop_mut()
                    .set_mode(crate::core::agent_types::AgentMode::Act);
                app.mode = "ACT".to_string();
                app.update_placeholder();
                app.push_plain("Plan aborted. Already-applied changes are kept.");
            } else {
                app.push_plain("No active plan to abort.");
            }
        }
        CliOnlyCommand::Plan(_)
        | CliOnlyCommand::PlanApprove
        | CliOnlyCommand::PlanPause
        | CliOnlyCommand::PlanResume
        | CliOnlyCommand::PlanComplete
        | CliOnlyCommand::PlanFail => {
            use crate::cli::slash_commands::PlanSubcommand;
            let mut sess = session.lock().await;
            let sh = sess.agent_loop().state_handle();
            let mut state = sh.lock().await;
            if let Some(plan) = &mut state.plan_state {
                match cli_cmd {
                    CliOnlyCommand::Plan(cmd) => {
                        match cmd {
                            PlanSubcommand::Status => {
                                app.push_plain(plan.status_summary());
                                app.push_plain(plan.format_display());
                            }
                            PlanSubcommand::Edit(step_num, new_desc) => {
                                if plan.approved && !plan.paused {
                                    app.push_plain(
                                        "Cannot edit while plan is running. Use /plan pause first.",
                                    );
                                } else if step_num == 0 || step_num > plan.steps.len() {
                                    app.push_plain(format!(
                                        "Invalid step number. Plan has {} steps (1-{}).",
                                        plan.steps.len(),
                                        plan.steps.len()
                                    ));
                                } else if new_desc.trim().is_empty() {
                                    app.push_plain("Step description cannot be empty.");
                                } else {
                                    plan.steps[step_num - 1].description =
                                        new_desc.trim().to_string();
                                    app.push_plain(format!("Step {} updated.", step_num));
                                }
                            }
                            PlanSubcommand::Add(after_step, step_text) => {
                                if plan.approved && !plan.paused {
                                    app.push_plain("Cannot add steps while plan is running. Use /plan pause first.");
                                } else if step_text.trim().is_empty() {
                                    app.push_plain("Usage: /plan add <after_step> <description>");
                                } else {
                                    let after_idx = if after_step == 0 {
                                        usize::MAX
                                    } else {
                                        after_step - 1
                                    };
                                    match plan
                                        .insert_step_after(after_idx, step_text.trim().to_string())
                                    {
                                        Ok(()) => {
                                            if after_step == 0 {
                                                app.push_plain(format!("Step added at the beginning. ({} steps total).", plan.steps.len()));
                                            } else {
                                                app.push_plain(format!(
                                                    "Step added after step {}. ({} steps total).",
                                                    after_step,
                                                    plan.steps.len()
                                                ));
                                            }
                                        }
                                        Err(e) => app.push_plain(format!("Error: {}", e)),
                                    }
                                }
                            }
                            PlanSubcommand::Remove(step_num) => {
                                if plan.approved && !plan.paused {
                                    app.push_plain("Cannot remove steps while plan is running. Use /plan pause first.");
                                } else if step_num == 0 || step_num > plan.steps.len() {
                                    app.push_plain(format!(
                                        "Invalid step number. Plan has {} steps (1-{}).",
                                        plan.steps.len(),
                                        plan.steps.len()
                                    ));
                                } else {
                                    match plan.remove_step(step_num - 1) {
                                        Ok(()) => app.push_plain(format!(
                                            "Step {} removed. ({} steps remaining).",
                                            step_num,
                                            plan.steps.len()
                                        )),
                                        Err(e) => app.push_plain(format!("Error: {}", e)),
                                    }
                                }
                            }
                            PlanSubcommand::Replace(plan_text) => {
                                if plan_text.trim().is_empty() {
                                    app.push_plain("Plan text cannot be empty.");
                                } else {
                                    let parsed =
                                        crate::core::plan_state::PlanState::parse_plan(&plan_text);
                                    match parsed {
                                    Some(steps) if steps.len() >= 2 => {
                                        let new_plan = crate::core::plan_state::PlanState::create_plan(steps);
                                        *plan = new_plan;
                                        app.push_plain(format!("Plan replaced ({} steps).", plan.steps.len()));
                                    }
                                    Some(_) => app.push_plain("Plan must have at least 2 steps."),
                                    None => app.push_plain("Could not parse plan text. Use numbered format: 1. Step description"),
                                }
                                }
                            }
                            _ => unreachable!(
                                "PlanSubcommand::Approve/Pause/Resume/Abort are routed to CliOnlyCommand::PlanApprove/Pause/Resume/Abort"
                            ),
                        }
                    }
                    CliOnlyCommand::PlanApprove => {
                        if plan.approved {
                            app.push_plain("Plan is already approved and running.");
                        } else if plan.steps.is_empty() {
                            app.push_plain("Cannot approve an empty plan.");
                        } else {
                            // Validate current_step_index points to a pending step; if not, find first pending
                            let start_index = if plan.current_step_index < plan.steps.len()
                                && plan.steps[plan.current_step_index].status
                                    == crate::core::plan_state::PlanStepStatus::Pending
                            {
                                Some(plan.current_step_index)
                            } else {
                                plan.steps.iter().position(|s| {
                                    s.status == crate::core::plan_state::PlanStepStatus::Pending
                                })
                            };
                            let Some(start_index) = start_index else {
                                app.push_plain(
                                    "No pending step to approve. All steps are complete.",
                                );
                                return Ok(false);
                            };
                            plan.current_step_index = start_index;
                            let steps_len = plan.steps.len();
                            let step_desc = plan.steps[start_index].description.clone();
                            plan.approved = true;
                            plan.steps[start_index].status =
                                crate::core::plan_state::PlanStepStatus::Running;
                            drop(state);
                            {
                                let state_handle = sess.agent_loop_mut().state_handle();
                                let mut state = state_handle.lock().await;
                                state.strict_plan_mode_enabled = false;
                            }
                            sess.agent_loop_mut()
                                .set_mode(crate::core::agent_types::AgentMode::Act);
                            drop(sess);
                            app.push_plain(format!(
                                "Plan approved. Starting from step {}/{}: {}",
                                start_index + 1,
                                steps_len,
                                step_desc
                            ));
                            // Spawn agent to execute the approved plan
                            let prompt = format!(
                                "Execute step {}/{}: {}",
                                start_index + 1,
                                steps_len,
                                step_desc
                            );
                            spawn_agent_task(
                                session,
                                &prompt,
                                agent_busy,
                                agent_done,
                                agent_start_time,
                                agent_task,
                            )
                            .await?;
                            app.agent_busy = true;
                        }
                    }
                    CliOnlyCommand::PlanPause => {
                        if plan.approved && plan.current_step_index < plan.steps.len() {
                            plan.paused = true;
                            app.push_plain("Plan paused. Use /plan resume to continue.");
                        } else {
                            app.push_plain("No active plan to pause.");
                        }
                    }
                    CliOnlyCommand::PlanResume => {
                        if !plan.approved {
                            app.push_plain("Plan is not yet approved. Use /plan approve first.");
                        } else if !plan.paused {
                            app.push_plain("Plan is not paused.");
                        } else if plan.complete {
                            app.push_plain("Plan is already complete.");
                        } else {
                            plan.paused = false;
                            if plan.steps.get(plan.current_step_index).is_some_and(|s| {
                                s.status == crate::core::plan_state::PlanStepStatus::Failed
                            }) {
                                plan.steps[plan.current_step_index].status =
                                    crate::core::plan_state::PlanStepStatus::Running;
                            }
                            let step_num = plan.current_step_index + 1;
                            let step_total = plan.steps.len();
                            let step_desc = plan.steps[plan.current_step_index].description.clone();
                            drop(state);
                            drop(sess);
                            app.push_plain(format!(
                                "Plan resumed at step {}/{}: {}",
                                step_num, step_total, step_desc
                            ));
                            // Spawn agent to resume plan execution
                            let prompt =
                                format!("Execute step {}/{}: {}", step_num, step_total, step_desc);
                            spawn_agent_task(
                                session,
                                &prompt,
                                agent_busy,
                                agent_done,
                                agent_start_time,
                                agent_task,
                            )
                            .await?;
                            app.agent_busy = true;
                        }
                    }
                    CliOnlyCommand::PlanComplete => {
                        if plan.complete {
                            app.push_plain("Plan is already complete.");
                        } else if plan.current_step_index >= plan.steps.len() {
                            app.push_plain("No active step to mark complete.");
                        } else {
                            plan.mark_step(
                                plan.current_step_index,
                                crate::core::plan_state::PlanStepStatus::Done,
                            )
                            .ok();
                            let next = plan.advance();
                            if next.is_none() && plan.is_complete() {
                                plan.complete = true;
                                app.push_plain("All steps marked complete. Plan finished.");
                            } else {
                                app.push_plain(format!(
                                    "Step {} marked complete.",
                                    plan.current_step_index + 1
                                ));
                            }
                        }
                    }
                    CliOnlyCommand::PlanFail => {
                        if plan.complete {
                            app.push_plain("Plan is already complete.");
                        } else if plan.current_step_index >= plan.steps.len() {
                            app.push_plain("No active step to mark as failed.");
                        } else {
                            plan.mark_step(
                                plan.current_step_index,
                                crate::core::plan_state::PlanStepStatus::Failed,
                            )
                            .ok();
                            plan.paused = true;
                            app.push_plain(format!(
                                "Step {}/{} marked as failed. Execution paused. Use /plan resume to retry.",
                                plan.current_step_index + 1,
                                plan.steps.len()
                            ));
                        }
                    }

                    _ => unreachable!(),
                }
            } else {
                app.push_plain("No active plan.");
            }
        }
    }
    Ok(false)
}

/// Main ratatui event loop.
async fn run_main_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    output_rx: &mut mpsc::Receiver<OutputEvent>,
    session: Arc<Mutex<InteractiveSession>>,
    task_id: String,
    agent_busy: Arc<AtomicBool>,
    agent_done: Arc<tokio::sync::Notify>,
    agent_start_time: Arc<Mutex<Option<Instant>>>,
    state_handle: Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    agent_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    queue_handle: Arc<Mutex<Option<crate::core::agent_loop::MessageQueueHandle>>>,
    task_opts: &TaskOptions,
    auto_approve: bool,
) -> anyhow::Result<()> {
    use std::sync::Mutex as StdMutex;
    let last_ctrlc = Arc::new(StdMutex::new(None::<std::time::Instant>));

    loop {
        // 1. Drain channel into app
        drain_output(output_rx, app);

        // 1b. Sync plan state from TaskState to App TUI cache
        {
            if let Some(state_arc) = state_handle.lock().await.as_ref() {
                let state = state_arc.lock().await;
                app.plan_state_cache = state.plan_state.clone();
                if let Some(ref plan) = state.plan_state {
                    let has_failed = plan
                        .steps
                        .iter()
                        .any(|s| s.status == crate::core::plan_state::PlanStepStatus::Failed);
                    app.mode = if plan.complete {
                        "COMPLETE".to_string()
                    } else if has_failed {
                        "FAILED".to_string()
                    } else if plan.paused {
                        "PAUSED".to_string()
                    } else if plan.approved {
                        "ACT".to_string()
                    } else {
                        "PLAN".to_string()
                    };
                }
            }
        }

        // 2. Render
        terminal.draw(|f| app.render(f))?;

        // 3. Poll for events (blocking, 16ms timeout for ~60fps responsiveness)
        let has_event = ratatui::crossterm::event::poll(Duration::from_millis(16))?;
        if has_event {
            match ratatui::crossterm::event::read()? {
                Event::Key(key) => {
                    // Approval prompt: route y/n/a to approval channel
                    if is_approval_prompt_active() {
                        if let Some(result) = approval_result_for_key(&key) {
                            if let Some(sender) = take_approval_sender() {
                                let prompt_lines = app.output_lines.len();
                                let _ = sender.send(result.clone());
                                let drain_start = prompt_lines.min(app.output_lines.len());
                                app.output_lines.drain(drain_start..);

                                if key.code == KeyCode::Char('c')
                                    && key.modifiers.contains(KeyModifiers::CONTROL)
                                {
                                    app.auto_scroll = true;
                                    app.scroll_offset = 0;
                                    cancel_agent(&state_handle, &agent_task, &agent_done).await?;
                                    app.push_plain("^C");
                                    app.agent_busy = false;
                                    continue;
                                }

                                // Echo approval decision
                                app.push_styled(
                                    format!(
                                        "  ↳ {}",
                                        match result {
                                            ApprovalResult::Approved => "approved",
                                            ApprovalResult::Denied => "denied",
                                            ApprovalResult::Always => "always approve",
                                        }
                                    ),
                                    Style::default().fg(theme::ACCENT),
                                );
                                app.auto_scroll = true;
                                app.scroll_offset = 0;
                            }
                            continue;
                        }
                    }

                    // Global Ctrl+C handling
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        let now = std::time::Instant::now();
                        let is_double_tap = {
                            let last = last_ctrlc.lock().unwrap();
                            last.is_some_and(|prev| now.duration_since(prev).as_secs() < 2)
                        };

                        if is_double_tap {
                            // Force exit on second Ctrl+C
                            if app.picker_active {
                                app.picker_active = false;
                                app.picker_results.clear();
                            }
                            return Ok(());
                        }

                        // First Ctrl+C - update timestamp
                        {
                            let mut last = last_ctrlc.lock().unwrap();
                            *last = Some(now);
                        }

                        // Dismiss picker if active
                        if app.picker_active {
                            app.picker_active = false;
                            app.picker_results.clear();
                            continue;
                        }

                        // If agent is busy, cancel it
                        if agent_busy.load(Ordering::Relaxed) {
                            cancel_agent(&state_handle, &agent_task, &agent_done).await?;
                            app.agent_busy = false;
                            app.push_styled("^C cancelled", Style::default().fg(theme::WARNING_FG));
                            app.push_styled(
                                "Press Ctrl+C again to quit.",
                                Style::default().fg(theme::WARNING_FG),
                            );
                            continue;
                        }

                        // Not busy: clear input or hint about quitting
                        if !app.input.lines().join("\n").is_empty() {
                            app.push_plain("^C");
                            app.input = App::new_textarea(Vec::new());
                        }
                        app.push_styled(
                            "Press Ctrl+C again to quit.",
                            Style::default().fg(theme::WARNING_FG),
                        );
                        continue;
                    }

                    if let Some(action) = handle_key_event(key, app, &session, &task_id).await? {
                        match action {
                            Action::Submit(text) => {
                                // Save to command history and reset navigation
                                append_to_history(&text);
                                app.history.reload();

                                // Check for CLI-only slash commands FIRST
                                if let Some(cli_cmd) =
                                    crate::cli::slash_commands::get_cli_only_command(&text)
                                {
                                    // Handle /plan <prompt> specially: clear old plan, enter Plan mode, spawn agent
                                    if let crate::cli::slash_commands::CliOnlyCommand::PlanPrompt(
                                        ref prompt_text,
                                    ) = cli_cmd
                                    {
                                        // Clear old plan state and restore strict plan mode restrictions
                                        {
                                            let state_arc = state_handle.lock().await;
                                            if let Some(sh) = state_arc.as_ref() {
                                                let mut state = sh.lock().await;
                                                state.plan_state = None;
                                                state.strict_plan_mode_enabled = true;
                                            }
                                        }
                                        // Switch agent mode to Plan so write/edit tools are restricted
                                        {
                                            let mut sess = session.lock().await;
                                            sess.agent_loop_mut().set_mode(
                                                crate::core::agent_types::AgentMode::Plan,
                                            );
                                        }
                                        app.push_plain("Entering plan mode...");
                                        app.mode = "PLAN".to_string();
                                        app.update_placeholder();
                                        // Spawn agent with the prompt
                                        if agent_busy.load(Ordering::Relaxed) {
                                            if let Some(qh) = queue_handle.lock().await.as_ref() {
                                                qh.enqueue_text_message(prompt_text.clone()).await;
                                                app.push_plain(
                                                    "Agent is busy. Plan prompt queued.",
                                                );
                                            }
                                        } else {
                                            spawn_agent_task(
                                                &session,
                                                prompt_text,
                                                &agent_busy,
                                                &agent_done,
                                                &agent_start_time,
                                                &agent_task,
                                            )
                                            .await?;
                                            app.agent_busy = true;
                                        }
                                        app.auto_scroll = true;
                                        app.scroll_offset = 0;
                                        terminal.draw(|f| app.render(f))?;
                                        continue;
                                    }

                                    // Local commands execute immediately even when agent is busy
                                    if cli_cmd.is_local_command() {
                                        let should_exit = handle_cli_only_command(
                                            cli_cmd,
                                            &text,
                                            app,
                                            &session,
                                            &task_id,
                                            &agent_busy,
                                            &agent_done,
                                            &agent_start_time,
                                            &agent_task,
                                            &state_handle,
                                            task_opts,
                                            auto_approve,
                                        )
                                        .await?;
                                        if should_exit {
                                            return Ok(());
                                        }
                                        continue;
                                    }

                                    // Agent-required commands: check if agent is busy
                                    if agent_busy.load(Ordering::Relaxed)
                                        && cli_cmd.requires_agent_idle()
                                    {
                                        // Queue the command
                                        if let Some(qh) = queue_handle.lock().await.as_ref() {
                                            qh.enqueue_text_message(text.clone()).await;
                                            let count = qh.queued_message_count().await;
                                            {
                                                let sess = session.lock().await;
                                                let writer = sess.agent_loop().output_writer();
                                                app.push_user_message(&text, writer);
                                            }
                                            app.push_styled(
                                                format!(
                                                    "Command queued ({} in queue): {}",
                                                    count, text
                                                ),
                                                theme::dim_style(),
                                            );
                                        }
                                        continue;
                                    }
                                }

                                // Process model-side slash commands (e.g., /compact, /plan)
                                let processed =
                                    crate::cli::slash_commands::process_slash_command(&text);

                                // If agent is busy, queue the message; otherwise spawn
                                if agent_busy.load(Ordering::Relaxed)
                                    && let Some(qh) = queue_handle.lock().await.as_ref()
                                    && !processed.is_empty()
                                {
                                    // Echo queued message to output pane
                                    {
                                        let sess = session.lock().await;
                                        let writer = sess.agent_loop().output_writer();
                                        app.push_user_message(&processed, writer);
                                    }
                                    qh.enqueue_text_message(processed).await;
                                    let count = qh.queued_message_count().await;
                                    app.push_styled(
                                        format!("Message queued ({} in queue)", count),
                                        theme::dim_style(),
                                    );
                                } else {
                                    spawn_agent_task(
                                        &session,
                                        &processed,
                                        &agent_busy,
                                        &agent_done,
                                        &agent_start_time,
                                        &agent_task,
                                    )
                                    .await?;
                                    app.agent_busy = true;
                                }
                                // Render immediately to show user message before agent starts streaming
                                app.auto_scroll = true;
                                app.scroll_offset = 0;
                                terminal.draw(|f| app.render(f))?;
                            }
                        }
                    }
                }
                Event::Paste(content) => {
                    // Handle paste event with folding for large pastes
                    let folded = app.handle_paste(&content);
                    if folded {
                        app.push_styled(
                            format!(
                                "Large paste folded ({} chars) - will expand on submit",
                                content.len()
                            ),
                            theme::dim_style(),
                        );
                    }
                }
                Event::Resize(_, _) => {
                    // Ratatui handles resize automatically on next draw
                }
                Event::Mouse(mouse_event) => match mouse_event.kind {
                    ratatui::crossterm::event::MouseEventKind::ScrollDown => {
                        app.scroll_offset = app.scroll_offset.saturating_add(3);
                    }
                    ratatui::crossterm::event::MouseEventKind::ScrollUp => {
                        app.auto_scroll = false;
                        app.scroll_offset = app.scroll_offset.saturating_sub(3);
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        // 4. Check agent completion (non-blocking)
        // Always check notification to avoid race condition where agent_busy is already false
        // but app.agent_busy hasn't been updated yet
        if agent_done.notified().now_or_never().is_some() {
            agent_busy.store(false, Ordering::Relaxed);
            app.agent_busy = false;
            // Check if task was cancelled — if so, allow user to exit
            let task_was_cancelled = if let Some(state_arc) = state_handle.lock().await.as_ref() {
                let state = state_arc.lock().await;
                state.is_cancelled
            } else {
                false
            };
            if let Some(start) = agent_start_time.lock().await.take() {
                let elapsed = start.elapsed();
                app.elapsed = Some(elapsed);
                app.push_styled(
                    format!("⏱ Elapsed: {}", format_duration(elapsed)),
                    theme::dim_style(),
                );
                // Turn separator after agent completion
                app.push_turn_separator();
            }
            // If task was cancelled, show message and allow immediate exit via /exit or Ctrl+C
            if task_was_cancelled {
                app.push_styled(
                    "Task cancelled. Type /exit to quit.",
                    Style::default().fg(theme::WARNING_FG),
                );
            }
        }

        // 5. Update elapsed time for status bar
        if app.agent_busy
            && let Some(start) = app.start_time
        {
            app.elapsed = Some(start.elapsed());
        }

        // 6. Tick spinner
        app.tick_spinner();
    }
}

pub async fn run_interactive_shell_inner(
    task_opts: TaskOptions,
    root_opts: RootOnlyOptions,
) -> anyhow::Result<()> {
    // 0. Install panic hook to restore terminal on panic
    crate::terminal::input::install_panic_hook();

    // 1. Initialize ratatui (replaces enter_raw_mode, scroll_region, bracketed paste)
    let mut terminal = if std::env::var("SNED_NO_ALTERNATE_SCREEN").is_ok() {
        ratatui::Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(std::io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(24),
            },
        )?
    } else {
        ratatui::init()
    };

    // Enable bracketed paste mode and mouse capture for proper paste handling and scroll wheel support
    execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;

    crate::core::cancellation::TERMINAL_INITIALIZED
        .store(true, std::sync::atomic::Ordering::Release);
    let _guard = TerminalGuard;
    let mut app = App::new();
    if let Ok(cwd) = std::env::current_dir() {
        app.cwd = cwd.to_string_lossy().to_string();
    }

    // 2. Create output channel (bounded to prevent memory exhaustion during output floods)
    let (output_tx, mut output_rx) = mpsc::channel(4096);
    let output_writer: OutputWriterArc = Arc::new(ChannelOutputWriter::new(output_tx));

    // 3. Build session
    let session = Arc::new(Mutex::new(
        InteractiveSession::build_with_writer(
            task_opts.clone(),
            root_opts.clone(),
            Some(output_writer.clone()),
        )
        .await?,
    ));

    let task_id = {
        let sess = session.lock().await;
        sess.agent_loop.task_id().to_string()
    };

    // Set status bar fields from session info
    {
        let sess = session.lock().await;
        let provider = sess.agent_loop.get_provider();
        let model = provider.get_model();
        app.provider_name = provider.name().to_string();
        app.model_name = sess
            .task_opts
            .model
            .as_deref()
            .unwrap_or(&model.id)
            .to_string();
        app.task_id = task_id.clone();
        app.mode = if sess.task_opts.plan { "PLAN" } else { "ACT" }.to_string();
        app.start_time = Some(Instant::now());
    }

    // 4. Startup banner → app.push_output()
    {
        let sess = session.lock().await;
        if !sess.is_quiet() {
            let startup_info = sess.get_startup_info();
            for line in ansi_to_ratatui_lines(&startup_info) {
                app.push_output(line);
            }
            app.push_styled(
                "type a prompt and press Enter; type /exit to leave",
                theme::dim_style(),
            );
            app.push_styled(
                "type /help for slash commands, @ to search and mention files",
                theme::dim_style(),
            );
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
    let agent_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    // Command history is loaded by App::new() via FileHistory::load()
    // Reload in case history was written by another session
    app.history.reload();

    {
        let sess = session.lock().await;
        let mut qh = queue_handle.lock().await;
        *qh = Some(sess.queue_handle());
        let mut sh = state_handle.lock().await;
        *sh = Some(sess.state_handle());
    }

    // 6. Main loop
    let auto_approve = task_opts.yolo || task_opts.auto_approve_all;
    run_main_loop(
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
        queue_handle,
        &task_opts,
        auto_approve,
    )
    .await
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
    fn test_drain_output_preserves_manual_scroll_without_approval_prompt() {
        use crate::cli::output::OutputEvent;

        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(OutputEvent::plain("line 1")).unwrap();

        let mut app = App::new();
        app.auto_scroll = false;
        app.scroll_offset = 7;

        drain_output(&mut rx, &mut app);

        assert!(!app.auto_scroll);
        assert_eq!(app.scroll_offset, 7);
        assert_eq!(app.output_lines.len(), 1);
    }

    #[test]
    fn test_drain_output_forces_scroll_for_approval_prompt() {
        let (_tx, mut rx) = mpsc::channel(1);

        // Ensure clean state before test
        crate::core::approval::clear_approval_prompt_scroll();
        crate::core::approval::set_approval_prompt_scroll();

        let mut app = App::new();
        app.auto_scroll = false;
        app.scroll_offset = 7;

        drain_output(&mut rx, &mut app);

        assert!(app.auto_scroll);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn test_drain_output_forces_scroll_while_approval_prompt_is_active() {
        let (_tx, mut rx) = mpsc::channel(1);

        // Ensure clean state before test
        crate::core::approval::clear_approval_prompt_scroll();
        crate::core::approval::set_approval_prompt_active(true);

        let mut app = App::new();
        app.auto_scroll = false;
        app.scroll_offset = 7;

        drain_output(&mut rx, &mut app);

        crate::core::approval::set_approval_prompt_active(false);

        assert!(app.auto_scroll);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn test_approval_result_for_key_only_accepts_prompt_shortcuts() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        assert_eq!(
            approval_result_for_key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty())),
            Some(ApprovalResult::Approved)
        );
        assert_eq!(
            approval_result_for_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(ApprovalResult::Denied)
        );
        assert_eq!(
            approval_result_for_key(&KeyEvent::new(KeyCode::Char('/'), KeyModifiers::empty())),
            None
        );
        assert_eq!(
            approval_result_for_key(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty())),
            None
        );
    }

    #[test]
    fn test_shutdown_submit_detection_matches_exit_aliases() {
        assert!(is_shutdown_submit("/exit"));
        assert!(is_shutdown_submit("/quit"));
        assert!(is_shutdown_submit("/q"));
        assert!(!is_shutdown_submit("/clear"));
        assert!(!is_shutdown_submit("hello world"));
    }
}
