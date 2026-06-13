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
    EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
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
        crate::core::cancellation::restore_terminal_state();
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

fn build_user_message_content(
    clean_prompt: String,
    image_paths: Vec<String>,
    model_info: crate::providers::ModelInfo,
    output_writer: &OutputWriterArc,
    show_image_warnings: bool,
) -> crate::providers::MessageContent {
    let supports_images = model_info.supports_images.unwrap_or(false);
    let image_blocks = if !image_paths.is_empty() && !supports_images {
        if show_image_warnings {
            output_writer.emit(OutputEvent::warning(format!(
                "Model '{}' does not support images. Ignoring {} image(s).",
                model_info.name.as_deref().unwrap_or("unknown"),
                image_paths.len()
            )));
        }
        Vec::new()
    } else {
        crate::cli::image_input::load_images_to_content_blocks(&image_paths)
    };

    if image_blocks.is_empty() {
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
    }
}

fn strip_active_slash_command(text: &str) -> Option<String> {
    let query = crate::cli::slash_commands::extract_slash_query(text)?;
    let start = text.rfind(&format!("/{}", query))?;
    Some(format!(
        "{}{}",
        &text[..start],
        &text[start + query.len() + 1..]
    ))
}

pub struct InteractiveSession {
    agent_loop: Arc<tokio::sync::Mutex<crate::core::agent_loop::AgentLoop>>,
    hook_manager: Arc<crate::core::hooks::HookManager>,
    state_manager: Arc<crate::storage::state_manager::StateManager>,
    task_opts: TaskOptions,
    root_opts: RootOnlyOptions,
}

impl InteractiveSession {
    /// Get a reference to the underlying AgentLoop.
    pub async fn agent_loop(
        &self,
    ) -> tokio::sync::MutexGuard<'_, crate::core::agent_loop::AgentLoop> {
        self.agent_loop.lock().await
    }

    /// Get a mutable reference to the underlying AgentLoop.
    pub async fn agent_loop_mut(
        &self,
    ) -> tokio::sync::MutexGuard<'_, crate::core::agent_loop::AgentLoop> {
        self.agent_loop.lock().await
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
            .with_checkpoint_manager(components.checkpoint_mgr)
            .with_yolo(task_opts.yolo);

        let agent_loop = Arc::new(tokio::sync::Mutex::new(agent_loop));
        crate::core::cancellation::setup_ctrl_c_handler(agent_loop.lock().await.state_handle())
            .await;

        Ok(Self {
            agent_loop,
            hook_manager: components.hook_manager,
            state_manager: components.state_manager,
            task_opts,
            root_opts,
        })
    }

    async fn queue_handle(&self) -> crate::core::agent_loop::MessageQueueHandle {
        self.agent_loop.lock().await.message_queue_handle()
    }

    /// Get the message queue handle for checking queued messages.
    pub async fn message_queue_handle(
        &self,
    ) -> Option<crate::core::agent_loop::MessageQueueHandle> {
        Some(self.agent_loop.lock().await.message_queue_handle())
    }

    async fn retryable_failed_request(&self) -> Option<crate::providers::StorageMessage> {
        let state_handle = self.agent_loop.lock().await.state_handle();
        state_handle.lock().await.retryable_failed_request.clone()
    }

    async fn prepend_retryable_failed_request(
        &self,
        message: crate::providers::StorageMessage,
    ) -> bool {
        if let crate::providers::MessageContent::Text(text) = message.content {
            self.queue_handle().await.prepend_text_message(text).await;
            true
        } else {
            false
        }
    }

    async fn state_handle(&self) -> Arc<tokio::sync::Mutex<crate::core::agent_types::TaskState>> {
        self.agent_loop.lock().await.state_handle()
    }

    async fn clear_compacted_summary(&self) -> bool {
        self.agent_loop.lock().await.clear_compacted_summary().await
    }

    /// Get startup info line showing provider, model, task ID, mode, and context window.
    pub async fn get_startup_info(&self) -> String {
        use crate::core::context::context_window::get_context_window_info;

        let guard = self.agent_loop.lock().await;
        let provider = guard.get_provider();
        let provider_name = provider.name();
        let model = provider.get_model();
        let model_name = self.task_opts.model.as_deref().unwrap_or(&model.id);
        let task_id = guard.task_id();
        let mode = if self.task_opts.plan { "PLAN" } else { "ACT" };
        let context_info = get_context_window_info(provider.as_ref());
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

        crate::cli::colors::print_horizontal_rule_writer(writer);
    }

    pub async fn run(&self, prompt: Option<String>) -> anyhow::Result<()> {
        tracing::debug!(target: "sned::session", "InteractiveSession::run() called, prompt={}", prompt.as_ref().map(|s| format!("{} chars", s.len())).unwrap_or("None".to_string()));
        let agent = self.agent_loop.clone();
        let state_manager = self.state_manager.clone();

        let mut initial_messages = Vec::new();

        let is_resuming = self.root_opts.continue_task || self.root_opts.task_id.is_some();
        if is_resuming {
            let loaded = agent.lock().await.load_conversation_history().await;
            agent.lock().await.load_file_context_tracker().await;

            // Fire TaskResume hook after loading state
            let _ = self.hook_manager.task_resume(agent.lock().await.task_id());

            if loaded && !self.task_opts.json {
                let agent_lock = agent.lock().await;
                Self::print_resume_summary(&agent_lock, agent_lock.output_writer()).await;
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

            let (model_info, output_writer) = {
                let agent_lock = agent.lock().await;
                (
                    agent_lock.get_provider().get_model().info,
                    Arc::clone(agent_lock.output_writer()),
                )
            };
            let user_content = build_user_message_content(
                clean_prompt,
                all_image_paths,
                model_info,
                &output_writer,
                !self.task_opts.json,
            );

            initial_messages.push(crate::providers::StorageMessage {
                id: None,
                role: crate::providers::MessageRole::User,
                content: user_content,
                model_info: None,
                metrics: None,
                ts: Some(chrono::Utc::now().timestamp_millis() as u64),
            });
        }

        let run_result = {
            let mut loop_guard = agent.lock().await;
            loop_guard.reset_cancellation().await;
            loop_guard
                .run(initial_messages, state_manager)
                .await
                .map_err(|e| anyhow::anyhow!("Agent error: {}", e))
        };

        // Always export on exit, even if the agent errored out.
        // This ensures the conversation is saved for debugging failed runs.
        if let Some(export_path) = self.task_opts.export.clone() {
            let output_writer = {
                let agent_lock = agent.lock().await;
                Arc::clone(agent_lock.output_writer())
            };
            let export_result = export_agent_conversation(&agent, &export_path).await;
            report_conversation_export(&output_writer, self.task_opts.json, &export_result, true);
        }

        run_result?;
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
    let mut saw_output = false;
    while let Ok(event) = rx.try_recv() {
        saw_output = true;
        match event {
            OutputEvent::Line(line) => {
                // Record the line index for turn-end markdown re-render
                // BEFORE pushing, so the index points at the line being
                // added.
                app.push_stream_line(line);
            }
            OutputEvent::RawAnsi(s) => {
                // Raw ANSI events (code blocks) are already styled. We
                // do NOT record them in turn_stream_line_indices, so
                // turn-end replacement only re-renders the plain
                // streamed text and leaves ANSI code blocks untouched.
                let lines = ansi_to_ratatui_lines(&s);
                for line in lines {
                    app.push_output(line);
                }
            }
            OutputEvent::Completion(result) => {
                app.clear_completion_lines();
                for line in
                    crate::cli::markdown::render_completion_markdown("🚀 Task Completed: ", &result)
                {
                    app.push_completion_line(line);
                }
            }
            OutputEvent::TurnEnd { accumulated_text } => {
                app.finalize_turn_stream(&accumulated_text);
            }
            OutputEvent::TurnIndicator(line) => {
                // Store the indicator separately from streamed lines so
                // finalize_turn_stream can re-insert it at the top of
                // the markdown block instead of stripping it.
                app.push_turn_indicator(line);
            }
        }
    }
    if crate::core::approval::take_approval_prompt_scroll()
        || crate::core::approval::take_followup_prompt_scroll()
    {
        app.pin_approval_bottom();
    } else if crate::core::approval::is_approval_prompt_active() {
        app.pin_approval_bottom();
    } else if crate::core::approval::is_any_followup_question_active() {
        // Any interactive prompt that blocks progress must keep its input line
        // visible until the user responds, regardless of prior manual scroll.
        app.pin_approval_bottom();
    } else if app.is_approval_pinned() {
        app.clear_approval_pin();
    }

    if saw_output {
        app.clamp_to_content();
    }
}

fn sync_scroll_viewport(terminal: &ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let terminal_size = terminal.size()?;
    let terminal_height = terminal_size.height;
    let content_height = terminal_height.saturating_sub(6) as usize;
    app.set_content_width(terminal_size.width as usize);
    app.set_content_height(content_height);
    app.clamp_to_content();
    app.has_resized = false;
    Ok(())
}

/// Drain the output channel, force the scroll to the bottom, sync the
/// viewport, and re-render. Used immediately after `push_user_message` so
/// the user's just-submitted text is visible before the agent starts
/// streaming its response.
fn drain_and_render_user_submit(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    output_rx: &mut mpsc::Receiver<OutputEvent>,
) -> anyhow::Result<()> {
    drain_output(output_rx, app);
    app.force_bottom();
    sync_scroll_viewport(terminal, app)?;
    terminal.draw(|f| app.render(f))?;
    Ok(())
}

/// Test-only: drain the output channel into the app buffer without any
/// terminal-side rendering. Exposed `pub(crate)` so the TUI tests in
/// `cli::tui::app` can verify the emit → drain pipeline against a real
/// `ChannelOutputWriter` without standing up a full `ratatui::DefaultTerminal`.
#[cfg(test)]
pub(crate) fn drain_output_for_test(
    rx: &mut mpsc::Receiver<OutputEvent>,
    app: &mut App,
) {
    drain_output(rx, app);
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
    output_writer: OutputWriterArc,
) -> anyhow::Result<()> {
    let session_clone = Arc::clone(session);
    let prompt = prompt.to_string();
    let output_writer = Arc::clone(&output_writer);
    let agent_loop = {
        let sess = session_clone.lock().await;
        sess.agent_loop.clone()
    };
    let initial_message = {
        let processed_prompt = crate::cli::slash_commands::process_slash_command(&prompt);
        let (clean_prompt, parsed_image_paths) =
            crate::cli::image_input::parse_images_from_input(&processed_prompt);
        let model_info = agent_loop.lock().await.get_provider().get_model().info;
        let user_content = build_user_message_content(
            clean_prompt,
            parsed_image_paths,
            model_info,
            &output_writer,
            true,
        );
        crate::providers::StorageMessage {
            id: None,
            role: crate::providers::MessageRole::User,
            content: user_content,
            model_info: None,
            metrics: None,
            ts: Some(chrono::Utc::now().timestamp_millis() as u64),
        }
    };

    spawn_agent_task_from_message(
        session,
        initial_message,
        agent_busy,
        agent_done,
        agent_start_time,
        agent_task,
        output_writer,
    )
    .await
}

async fn spawn_agent_task_from_message(
    session: &Arc<Mutex<InteractiveSession>>,
    initial_message: crate::providers::StorageMessage,
    agent_busy: &Arc<AtomicBool>,
    agent_done: &Arc<tokio::sync::Notify>,
    agent_start_time: &Arc<Mutex<Option<Instant>>>,
    agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    output_writer: OutputWriterArc,
) -> anyhow::Result<()> {
    agent_busy.store(true, Ordering::Relaxed);
    *agent_start_time.lock().await = Some(Instant::now());

    let session_clone = Arc::clone(session);
    let agent_busy_clone = Arc::clone(agent_busy);
    let agent_done_clone = Arc::clone(agent_done);

    let handle = tokio::spawn(async move {
        let sess = session_clone.lock().await;
        let agent_loop = sess.agent_loop.clone();
        let state_manager = sess.state_manager.clone();
        let output_writer = Arc::clone(&output_writer);
        drop(sess);
        let initial_messages = vec![initial_message];

        let result = {
            let mut agent = agent_loop.lock().await;
            agent.reset_cancellation().await;
            agent.run(initial_messages, state_manager).await
        };
        let retry_available = {
            let state_handle = agent_loop.lock().await.state_handle();
            state_handle.lock().await.retryable_failed_request.is_some()
        };
        drop(agent_loop);

        if let Err(e) = result {
            output_writer.emit(OutputEvent::warning(format!("Agent task failed: {}", e)));
            if retry_available {
                output_writer.emit(OutputEvent::warning(
                    "Retry available: type /retry to resend the last failed request verbatim.",
                ));
            }
            tracing::error!("Agent task failed: {}", e);
        }

        agent_busy_clone.store(false, Ordering::Relaxed);
        agent_done_clone.notify_one();
    });

    *agent_task.lock().await = Some(handle);
    Ok(())
}

/// Cancel running agent task.
///
/// Uses the same graceful shutdown sequence as CancellationHandler::abort_task:
/// SIGTERM → 100ms wait → SIGKILL. This gives running commands a chance to
/// clean up (flush output, close files, etc.) before being force-killed.
///
/// `task.abort()` cancels the entire spawned future — including the
/// epilogue that would normally reset `agent_busy` and notify `agent_done`
/// — so without an explicit reset the atomic would stay `true` after
/// Ctrl+C. The next `/plan`/message submission would then be enqueued
/// forever, since the queue is consumed by the agent's `run()` loop,
/// which is no longer running.
async fn cancel_agent(
    state_handle: &Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    _agent_done: &Arc<tokio::sync::Notify>,
    agent_busy: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    if let Some(sh) = state_handle.lock().await.as_ref() {
        let mut state = sh.lock().await;
        state.is_cancelled = true;
        state.is_cancelled_atomic.store(true, Ordering::Release);

        #[cfg(unix)]
        {
            let pids = state.running_command_pids.clone();
            for pid in &pids {
                crate::core::cancellation::terminate_process_group(
                    *pid,
                    Duration::from_millis(100),
                )
                .await;
            }
        }

        state.running_command_pids.clear();
    }

    if let Some(task) = agent_task.lock().await.take() {
        task.abort();
    }

    // Reset unconditionally: covers both abort (epilogue never runs) and
    // natural completion (epilogue may have run before abort, setting the
    // same value). Without this, a Ctrl+C during a busy agent leaves the
    // atomic stuck at `true` and any subsequent prompt is enqueued.
    agent_busy.store(false, Ordering::Relaxed);

    Ok(())
}

/// Handle key events in ratatui loop (non-Ctrl+C keys).
async fn handle_key_event(
    key: KeyEvent,
    app: &mut App,
    output_writer: &OutputWriterArc,
    state_handle: &Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    task_id: &str,
) -> anyhow::Result<Option<Action>> {
    use crate::core::approval::{is_followup_question_active, take_followup_sender};

    fn accept_slash_completion(app: &mut App) -> bool {
        if app.slash_command_results.is_empty() {
            return false;
        }

        let text = app.input.lines().join("\n");
        let selected = app.slash_command_results[app.slash_command_selected]
            .name
            .clone();

        if let Some((new_text, cursor_pos)) =
            crate::cli::slash_commands::apply_slash_completion(&text, &selected)
        {
            app.input = App::new_textarea(vec![new_text.clone()]);
            app.input
                .move_cursor(tui_textarea::CursorMove::Jump(0, cursor_pos as u16));
            app.slash_command_active = false;
            app.slash_command_results.clear();
            app.slash_command_selected = 0;
            // Record the post-completion text so the post-text-input
            // re-evaluation keeps the picker dismissed while the user is
            // still browsing the completed command (e.g. arrow keys).
            // The picker re-opens once the user starts a new query
            // (separator, typed character, or backspace).
            app.slash_command_completed_text = Some(new_text);
            return true;
        }

        false
    }

    // Tab or Enter with active model picker -> insert model spec into textarea
    if app.model_picker_active
        && !app.model_picker_results.is_empty()
        && (key.code == KeyCode::Tab
            || (key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT)))
    {
        let entry = &app.model_picker_results[app.model_picker_selected];
        let model_spec = format!("{}/{}", entry.provider, entry.model_id);
        app.input = App::new_textarea(vec![model_spec]);
        app.input.move_cursor(tui_textarea::CursorMove::End);
        app.model_picker_active = false;
        app.model_picker_results.clear();
        app.model_picker_selected = 0;
        return Ok(None);
    }

    // Tab or Enter with active file picker -> insert selection (must come before Enter handler)
    if app.picker_active
        && !app.picker_results.is_empty()
        && (key.code == KeyCode::Tab || key.code == KeyCode::Enter)
    {
        let text = app.input.lines().join("\n");
        let mq = crate::core::file_search::extract_mention_query(&text);
        if mq.in_mention_mode {
            let result = &app.picker_results[app.picker_index];
            let (new_text, cursor_pos) = crate::core::file_search::insert_mention(
                &text,
                mq.at_index as usize,
                &result.path,
                result.file_type,
            );
            app.input = App::new_textarea(vec![new_text]);
            app.input
                .move_cursor(tui_textarea::CursorMove::Jump(0, cursor_pos as u16));
            app.picker_active = false;
            app.picker_results.clear();
            app.mention_search_active = false;
            app.mention_search_query.clear();
            return Ok(None);
        }
        // Picker active but mention mode lost — dismiss picker and fall through
        app.picker_active = false;
        app.picker_results.clear();
        app.mention_search_active = false;
        app.mention_search_query.clear();
        // Fall through to normal Enter/Tab handling
    }

    // Slash command mode - Tab/Enter accept the current entry into the input box.
    if app.slash_command_active && key.code == KeyCode::Tab {
        if accept_slash_completion(app) {
            return Ok(None);
        }
        return Ok(None);
    }
    if app.slash_command_active
        && key.code == KeyCode::Enter
        && !key.modifiers.contains(KeyModifiers::SHIFT)
    {
        let text = app.input.lines().join("\n");
        let current_query = crate::cli::slash_commands::extract_slash_query(&text);
        let selected = app
            .slash_command_results
            .get(app.slash_command_selected)
            .map(|entry| entry.name.as_str());

        if let (Some(query), Some(selected)) = (current_query.as_deref(), selected)
            && query != selected
            && accept_slash_completion(app)
        {
            return Ok(None);
        }
        // Fall through so Enter can submit a non-autocomplete slash command.
    }

    // Enter key - intercept before passing to textarea
    if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
        // Check for followup question (used by /undo, /commit, /checkpoint-restore)
        if is_followup_question_active(task_id) {
            if let Some(sender) = take_followup_sender(task_id) {
                let text = app.get_input_with_expanded_pastes();
                app.push_user_message(&text, output_writer);
                if sender.send(text).is_err() {
                    app.push_styled(
                        "Response discarded - prompt closed.",
                        Style::default().fg(theme::WARNING_FG),
                    );
                }
                app.input = App::new_textarea(Vec::new());
                // Same early-return issue: slash mode evaluation at end of
                // handle_key_event() never runs for followup entries.
                app.slash_command_active = false;
                app.slash_command_results.clear();
                app.slash_command_selected = 0;
                app.slash_command_completed_text = None;
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
                // Same early-return issue as main submit: slash mode evaluation
                // at end of handle_key_event() never runs.
                app.slash_command_active = false;
                app.slash_command_results.clear();
                app.slash_command_selected = 0;
                app.slash_command_completed_text = None;
                return Ok(Some(Action::Submit(text)));
            }

            if is_approval_prompt_active() {
                app.push_styled(
                    "Approval pending. Type y, n, or a first.",
                    Style::default().fg(theme::WARNING_FG),
                );
                return Ok(None);
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
            app.push_user_message(&text, output_writer);
            // Clear textarea and paste tracking
            app.input = App::new_textarea(Vec::new());
            app.clear_pastes();
            // Clear slash command picker state — Enter handler returns early so
            // the slash mode evaluation at the end of handle_key_event() never
            // runs for Enter submissions, leaving slash_command_active=true.
            app.slash_command_active = false;
            app.slash_command_results.clear();
            app.slash_command_selected = 0;
            app.slash_command_completed_text = None;
            // Submit to agent
            return Ok(Some(Action::Submit(text)));
        }
        return Ok(None);
    }

    // PageUp/PageDown for scrolling
    if key.code == KeyCode::PageUp {
        app.scroll_pages(-1);
        return Ok(None);
    }
    if key.code == KeyCode::PageDown {
        app.scroll_pages(1);
        return Ok(None);
    }

    // Handle pending clear confirmation
    if app.pending_clear.is_some() {
        if key.code == KeyCode::Char('y')
            || key.code == KeyCode::Char('Y')
            || key.code == KeyCode::Enter
        {
            app.clear_output();
            app.force_bottom();
            let trigger = app.pending_clear.take().unwrap();
            if let Some(sh) = state_handle.lock().await.as_ref() {
                let mut state = sh.lock().await;
                state.last_injected_plan_state_hash = None;
            }
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
            app.scroll_lines(-1);
            return Ok(None);
        }
        if key.code == KeyCode::Down {
            app.scroll_lines(1);
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

    // Up/Down for model picker navigation (when model picker is active)
    if app.model_picker_active && !app.model_picker_results.is_empty() {
        if key.code == KeyCode::Up {
            app.model_picker_selected = app.model_picker_selected.saturating_sub(1);
            return Ok(None);
        }
        if key.code == KeyCode::Down {
            app.model_picker_selected =
                (app.model_picker_selected + 1).min(app.model_picker_results.len() - 1);
            return Ok(None);
        }
    }

    // Escape key - dismiss model picker
    if key.code == KeyCode::Esc && app.model_picker_active {
        app.model_picker_active = false;
        app.model_picker_results.clear();
        app.model_picker_selected = 0;
        return Ok(None);
    }

    // Escape key - dismiss picker or clear input mode
    if key.code == KeyCode::Esc && app.picker_active {
        app.picker_active = false;
        app.picker_results.clear();
        app.mention_search_active = false;
        app.mention_search_query.clear();
        return Ok(None);
    }

    // Escape key - dismiss slash command picker
    if key.code == KeyCode::Esc && app.slash_command_active {
        app.slash_command_active = false;
        app.slash_command_results.clear();
        app.slash_command_selected = 0;
        let text = app.input.lines().join("\n");
        if let Some(new_text) = strip_active_slash_command(&text) {
            app.input = App::new_textarea(vec![new_text]);
        }
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

    // Check for @ mention mode - show file picker overlay (debounced)
    let input_text = app.input.lines().join("\n");
    let mq = crate::core::file_search::extract_mention_query(&input_text);
    if mq.in_mention_mode && !app.cwd.is_empty() {
        let query = mq.query;
        if !app.mention_search_active {
            // First entry into mention mode — search immediately
            app.mention_search_active = true;
            app.mention_search_query = query.clone();
            let results =
                crate::core::file_search::search_workspace_files(&query, &app.cwd, 10).await;
            app.picker_active = true;
            app.picker_results = results;
            app.picker_index = 0;
            // Push deadline far forward so the main loop doesn't re-search immediately
            app.mention_search_deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(3600);
        } else if query != app.mention_search_query {
            // Query changed — reset debounce timer, keep stale results visible
            app.mention_search_query = query;
            app.mention_search_deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(150);
        }
    } else {
        app.picker_active = false;
        app.picker_results.clear();
        app.mention_search_active = false;
        app.mention_search_query.clear();
    }

    // Check for slash command mode activation / update
    //
    // The completed-text guard prevents the picker from re-opening after
    // a Tab/Enter completion: as long as the input still matches the
    // completed text, the user is just browsing (arrow keys, escape,
    // etc.) and the picker should stay hidden. The picker re-opens as
    // soon as the user starts a new query — a typed character, a
    // separator (space), or a backspace.
    if let Some(query) = crate::cli::slash_commands::extract_slash_query(&input_text) {
        let still_completed = app
            .slash_command_completed_text
            .as_deref()
            .is_some_and(|completed| completed == input_text);
        if still_completed {
            // Picker stays dismissed; clear the completed marker so the
            // next genuinely-new input can re-enable the picker.
            app.slash_command_completed_text = None;
        } else if !app.slash_command_active {
            app.slash_command_active = true;
            app.slash_command_selected = 0;
            app.slash_command_results = crate::cli::slash_commands::filter_slash_commands(
                &app.slash_command_all_entries,
                &query,
            );
            app.slash_command_completed_text = None;
        } else {
            app.slash_command_results = crate::cli::slash_commands::filter_slash_commands(
                &app.slash_command_all_entries,
                &query,
            );
            app.slash_command_completed_text = None;
        }
    } else if app.slash_command_active {
        app.slash_command_active = false;
        app.slash_command_results.clear();
        app.slash_command_selected = 0;
        app.slash_command_completed_text = None;
    } else {
        // Input is no longer a slash command — drop the completed marker.
        app.slash_command_completed_text = None;
    }

    Ok(None)
}

/// Handle CLI-only slash commands, routing output to the App buffer.
/// Returns `true` if the caller should exit the main loop (for /exit, /quit).
async fn handle_cli_only_command(
    cli_cmd: crate::cli::slash_commands::CliOnlyCommand,
    text: &str,
    app: &mut App,
    output_writer: &OutputWriterArc,
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
        CliOnlyCommand::ModelSwitch(model_spec) => {
            if model_spec.is_empty() {
                // No argument: show model picker
                app.model_picker_results = crate::cli::slash_commands::build_model_picker_entries();
                app.model_picker_selected = 0;
                app.model_picker_active = true;
                return Ok(false);
            }

            let parts: Vec<&str> = model_spec.splitn(2, '/').collect();
            if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                app.push_plain(
                    "Usage: /model provider/model_id\nExample: /model anthropic/claude-sonnet-4",
                );
                return Ok(false);
            }

            let provider_name = parts[0];
            let model_id = parts[1];

            let mut temp_opts = task_opts.clone();
            temp_opts.provider = Some(provider_name.to_string());
            temp_opts.model = Some(model_id.to_string());

            match crate::cli::create_provider(&temp_opts) {
                Ok(new_provider) => {
                    let sess = session.lock().await;
                    sess.agent_loop_mut().await.set_provider(new_provider);
                    app.push_plain(format!("Model switched to {}/{}", provider_name, model_id));
                }
                Err(e) => {
                    app.push_plain(format!("Failed to create provider: {}", e));
                }
            }
            return Ok(false);
        }
        CliOnlyCommand::Models => {
            let models_text = crate::cli::slash_commands::format_models_text();
            for line in ansi_to_ratatui_lines(&models_text) {
                app.push_output(line);
            }
        }
        CliOnlyCommand::ResetCompact => {
            let sess = session.lock().await;
            if sess.clear_compacted_summary().await {
                let sh = sess.agent_loop().await.state_handle();
                {
                    let mut state = sh.lock().await;
                    state.last_injected_plan_state_hash = None;
                }
                app.push_plain("Compacted summary cleared. You can now use /compact again.");
            } else {
                app.push_plain("No compacted summary to clear.");
            }
        }
        CliOnlyCommand::Stats => {
            let sess = session.lock().await;
            let sh = sess.agent_loop().await.state_handle();
            let state = sh.lock().await;
            let stats = format_stats_text(&state);
            app.push_plain(stats);
        }
        CliOnlyCommand::Changes => {
            let sess = session.lock().await;
            let sh = sess.agent_loop().await.state_handle();
            let state = sh.lock().await;
            let changes = format_changes_text(&state);
            app.push_plain(changes);
        }
        CliOnlyCommand::Queue => {
            let sess = session.lock().await;
            if let Some(qh) = sess.message_queue_handle().await {
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
        CliOnlyCommand::Retry => {
            let retry_message = {
                let sess = session.lock().await;
                sess.retryable_failed_request().await
            };

            let Some(retry_message) = retry_message else {
                app.push_plain("No safe failed request is available to retry.");
                return Ok(false);
            };

            if agent_busy.load(Ordering::Relaxed) {
                let sess = session.lock().await;
                if !sess.prepend_retryable_failed_request(retry_message).await {
                    app.push_plain(
                        "The last failed request includes non-text content. Wait for idle, then run /retry again.",
                    );
                    return Ok(false);
                }
                let count = sess.queue_handle().await.queued_message_count().await;
                app.push_styled(
                    format!("Retry queued to run next ({} in queue).", count),
                    theme::dim_style(),
                );
            } else {
                spawn_agent_task_from_message(
                    session,
                    retry_message,
                    agent_busy,
                    agent_done,
                    agent_start_time,
                    agent_task,
                    output_writer.clone(),
                )
                .await?;
                app.agent_busy = true;
                app.push_styled("Retrying last failed request verbatim.", theme::dim_style());
            }
            app.force_bottom();
            return Ok(false);
        }
        CliOnlyCommand::Undo | CliOnlyCommand::CheckpointUndo => {
            let sess = session.lock().await;
            let agent_guard = sess.agent_loop().await;
            let checkpoint_mgr = agent_guard
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
                    let removed = sess.agent_loop().await.remove_last_turn().await;
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
            let agent_guard = sess.agent_loop().await;
            let checkpoint_mgr = agent_guard
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
            let agent_guard = sess.agent_loop().await;
            let checkpoint_mgr = agent_guard
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
                let sh = sess.agent_loop().await.state_handle();
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
            let sess = session.lock().await;
            let sh = sess.agent_loop().await.state_handle();
            let mut state = sh.lock().await;
            // Plan mode can be entered via `/plan <prompt>` (or `--plan`)
            // before any plan_state is created, so the abort path must
            // check the agent mode rather than only plan_state.is_some().
            // Otherwise the user gets stuck in plan mode when the model
            // answers a follow-up question without calling
            // `plan_mode_respond`, leaving no plan to approve.
            let had_plan = state.plan_state.is_some();
            state.plan_state = None;
            state.last_injected_plan_state_hash = None;
            state.strict_plan_mode_enabled = true;
            drop(state);
            sess.agent_loop_mut()
                .await
                .set_mode(crate::core::agent_types::AgentMode::Act);
            app.mode = "ACT".to_string();
            app.update_placeholder();
            if had_plan {
                app.push_plain("Plan aborted. Already-applied changes are kept.");
            } else {
                app.push_plain("Exited plan mode. Ready for act mode.");
            }
        }
        CliOnlyCommand::Plan(_)
        | CliOnlyCommand::PlanApprove
        | CliOnlyCommand::PlanPause
        | CliOnlyCommand::PlanResume
        | CliOnlyCommand::PlanComplete
        | CliOnlyCommand::PlanFail => {
            use crate::cli::slash_commands::PlanSubcommand;
            let sess = session.lock().await;
            let sh = sess.agent_loop().await.state_handle();
            let mut state = sh.lock().await;
            if let Some(plan) = &mut state.plan_state {
                match cli_cmd {
                    CliOnlyCommand::Plan(cmd) => match cmd {
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
                                plan.steps[step_num - 1].description = new_desc.trim().to_string();
                                app.push_plain(format!("Step {} updated.", step_num));
                            }
                        }
                        PlanSubcommand::Add(after_step, step_text) => {
                            if plan.approved && !plan.paused {
                                app.push_plain("Cannot add steps while plan is running. Use /plan pause first.");
                            } else if step_text.trim().is_empty() {
                                app.push_plain("Usage: /plan add <after_step> <description>");
                            } else {
                                if after_step == 0 {
                                    match plan
                                        .insert_step_at_beginning(step_text.trim().to_string())
                                    {
                                        Ok(()) => {
                                            app.push_plain(format!(
                                                "Step added at the beginning. ({} steps total).",
                                                plan.steps.len()
                                            ));
                                        }
                                        Err(e) => app.push_plain(format!("Error: {}", e)),
                                    }
                                } else {
                                    let after_idx = after_step - 1;
                                    match plan
                                        .insert_step_after(after_idx, step_text.trim().to_string())
                                    {
                                        Ok(()) => {
                                            app.push_plain(format!(
                                                "Step added after step {}. ({} steps total).",
                                                after_step,
                                                plan.steps.len()
                                            ));
                                        }
                                        Err(e) => app.push_plain(format!("Error: {}", e)),
                                    }
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
                            if plan.approved && !plan.paused && !plan.complete {
                                app.push_plain(
                                        "Cannot replace plan while plan is running. Use /plan pause first.",
                                    );
                            } else if plan_text.trim().is_empty() {
                                app.push_plain("Plan text cannot be empty.");
                            } else {
                                let parsed =
                                    crate::core::plan_state::PlanState::parse_plan(&plan_text);
                                match parsed {
                                        Some(steps) if !steps.is_empty() => {
                                            let new_plan = crate::core::plan_state::PlanState::create_plan(steps);
                                            let plan_len = {
                                                *plan = new_plan;
                                                plan.steps.len()
                                            };
                                            state.last_injected_plan_state_hash = None;
                                            app.push_plain(format!(
                                                "Plan replaced ({} steps).",
                                                plan_len
                                            ));
                                        }
                                        Some(_) => app.push_plain("Plan must have at least 1 step."),
                                        None => app.push_plain("Could not parse plan text. Use numbered format: 1. Step description"),
                                    }
                            }
                        }
                        _ => unreachable!(
                            "PlanSubcommand::Approve/Pause/Resume/Abort are routed to CliOnlyCommand::PlanApprove/Pause/Resume/Abort"
                        ),
                    },
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
                                if plan.is_complete() {
                                    plan.complete = true;
                                    app.push_plain("All steps are complete.");
                                } else if plan.steps.iter().all(|s| {
                                    s.status == crate::core::plan_state::PlanStepStatus::Failed
                                }) {
                                    app.push_plain("No pending step to approve. All steps failed.");
                                } else if plan.steps.iter().any(|s| {
                                    s.status == crate::core::plan_state::PlanStepStatus::Failed
                                }) {
                                    app.push_plain(
                                        "No pending step to approve. Plan contains failed steps.",
                                    );
                                } else {
                                    app.push_plain("No pending step to approve.");
                                }
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
                                let state_handle = sess.agent_loop_mut().await.state_handle();
                                let mut state = state_handle.lock().await;
                                state.strict_plan_mode_enabled = false;
                            }
                            sess.agent_loop_mut()
                                .await
                                .set_mode(crate::core::agent_types::AgentMode::Act);
                            app.mode = "ACT".to_string();
                            app.update_placeholder();
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
                                output_writer.clone(),
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
                            let Some(current_step) = plan.steps.get(plan.current_step_index) else {
                                app.push_plain(format!(
                                    "Cannot resume plan: current step {} is out of range (1-{}).",
                                    plan.current_step_index + 1,
                                    plan.steps.len()
                                ));
                                return Ok(false);
                            };
                            let current_step_failed = current_step.status
                                == crate::core::plan_state::PlanStepStatus::Failed;
                            let step_desc = current_step.description.clone();
                            let step_num = plan.current_step_index + 1;
                            let step_total = plan.steps.len();
                            plan.paused = false;
                            if current_step_failed {
                                plan.steps[plan.current_step_index].status =
                                    crate::core::plan_state::PlanStepStatus::Running;
                            }
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
                                output_writer.clone(),
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

fn serialize_conversation_export<T: serde::Serialize>(history: &T) -> Result<String, String> {
    serde_json::to_string_pretty(history)
        .map(|json| crate::cli::redact::redact_secrets(&json).into_owned())
        .map_err(|e| format!("Failed to serialize conversation: {}", e))
}

fn write_conversation_export(export_path: &str, export_data: &str) -> Result<String, String> {
    crate::storage::disk::atomic_write_file(export_path, export_data)
        .map_err(|e| format!("Failed to write export file: {}", e))?;
    Ok(format!(
        "Conversation exported to: {} (secrets redacted)",
        export_path
    ))
}

fn report_conversation_export(
    output_writer: &OutputWriterArc,
    json_output: bool,
    result: &Result<String, String>,
    announce_success: bool,
) {
    if json_output {
        return;
    }

    match result {
        Ok(message) if announce_success => {
            output_writer.emit(OutputEvent::info(message));
        }
        Err(message) => {
            output_writer.emit(OutputEvent::warning(message));
        }
        Ok(_) => {}
    }
}

/// Export the current conversation history to the given path.
async fn export_conversation(
    session: &Arc<Mutex<InteractiveSession>>,
    export_path: &str,
) -> Result<String, String> {
    let history = session
        .lock()
        .await
        .agent_loop()
        .await
        .get_conversation_history()
        .await;
    let export_data = serialize_conversation_export(&history)?;
    write_conversation_export(export_path, &export_data)
}

async fn export_agent_conversation(
    agent: &Arc<Mutex<crate::core::agent_loop::AgentLoop>>,
    export_path: &str,
) -> Result<String, String> {
    let history = agent.lock().await.get_conversation_history().await;
    let export_data = serialize_conversation_export(&history)?;
    write_conversation_export(export_path, &export_data)
}

/// Main ratatui event loop.
async fn run_main_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    output_rx: &mut mpsc::Receiver<OutputEvent>,
    output_writer: OutputWriterArc,
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

    let timing_enabled = crate::cli::output::timing_enabled();

    struct TimingSummary {
        enabled: bool,
        session_start_time: Option<std::time::Instant>,
        request_sent_time: Option<std::time::Instant>,
        first_provider_chunk_time: Option<std::time::Instant>,
        first_reasoning_chunk_time: Option<std::time::Instant>,
        first_displayable_text_time: Option<std::time::Instant>,
        first_output_emit_time: Option<std::time::Instant>,
        first_render_time: Option<std::time::Instant>,
        draw_total_us: u64,
        draw_count: u64,
        drain_total_us: u64,
        drain_count: u64,
        output_lines_peak: usize,
    }

    impl Drop for TimingSummary {
        fn drop(&mut self) {
            if !self.enabled {
                return;
            }

            eprintln!(
                "[timing] draw: total={}us count={} avg={}us",
                self.draw_total_us,
                self.draw_count,
                self.draw_total_us.saturating_div(self.draw_count),
            );
            eprintln!(
                "[timing] drain: total={}us count={} avg={}us",
                self.drain_total_us,
                self.drain_count,
                self.drain_total_us.saturating_div(self.drain_count),
            );
            eprintln!("[timing] output_lines_peak={}", self.output_lines_peak);

            if let Some(session_start) = self.session_start_time {
                for line in crate::cli::output::format_timing_phases(
                    session_start,
                    self.request_sent_time,
                    self.first_provider_chunk_time,
                    self.first_reasoning_chunk_time,
                    self.first_displayable_text_time,
                    self.first_output_emit_time,
                    self.first_render_time,
                ) {
                    eprintln!("{}", line);
                }
            }
        }
    }

    const BUSY_POLL_INTERVAL: Duration = Duration::from_millis(1);
    const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(1);
    let last_ctrlc = Arc::new(StdMutex::new(None::<std::time::Instant>));
    let mut timing = TimingSummary {
        enabled: timing_enabled,
        session_start_time: app.start_time,
        request_sent_time: None,
        first_provider_chunk_time: None,
        first_reasoning_chunk_time: None,
        first_displayable_text_time: None,
        first_output_emit_time: None,
        first_render_time: None,
        draw_total_us: 0,
        draw_count: 0,
        drain_total_us: 0,
        drain_count: 0,
        output_lines_peak: 0,
    };

    loop {
        // 1. Drain channel into app
        {
            let t = std::time::Instant::now();
            drain_output(output_rx, app);
            // Surface channel overflow to the user. The writer silently
            // drops events on a full channel (src/cli/output.rs:198-213);
            // if the dropped event was the approval prompt, the user
            // cannot see it and must blindly hit "y". The status bar
            // shows a warning when overflow is detected.
            if output_writer.take_overflow_signal() {
                app.output_overflow = true;
                app.output_overflow_count = output_writer.dropped_count();
                app.needs_redraw = true;
            }
            let us = t.elapsed().as_micros() as u64;
            timing.drain_total_us += us;
            timing.drain_count += 1;
        }

        // Track output lines peak
        let len = app.output_lines.len();
        if len > timing.output_lines_peak {
            timing.output_lines_peak = len;
        }

        // 1b. Sync plan state from TaskState to App TUI cache
        {
            if let Ok(state_arc) = state_handle.try_lock()
                && let Some(inner_arc) = state_arc.as_ref()
                && let Ok(state) = inner_arc.try_lock()
            {
                if timing_enabled {
                    if timing.session_start_time.is_none() {
                        timing.session_start_time = state.session_start_time;
                    }
                    if timing.request_sent_time.is_none() {
                        timing.request_sent_time = state.request_sent_time;
                    }
                    if timing.first_provider_chunk_time.is_none() {
                        timing.first_provider_chunk_time = state.first_provider_chunk_time;
                    }
                    if timing.first_reasoning_chunk_time.is_none() {
                        timing.first_reasoning_chunk_time = state.first_reasoning_chunk_time;
                    }
                    if timing.first_displayable_text_time.is_none() {
                        timing.first_displayable_text_time = state.first_displayable_text_time;
                    }
                    if timing.first_output_emit_time.is_none() {
                        timing.first_output_emit_time = state.first_output_emit_time;
                    }
                }

                app.reasoning_active = state.reasoning_active;
                app.context_pct = state
                    .last_api_req_info
                    .as_ref()
                    .and_then(|info| info.context_usage_percentage);
                let plan_changed = app.sync_plan_state_cache(state.plan_state.as_ref());
                if plan_changed {
                    app.needs_redraw = true;
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
        }

        if app.has_resized {
            sync_scroll_viewport(terminal, app)?;
        }

        // 2. Render (skip if nothing changed)
        if app.needs_redraw || app.has_resized {
            {
                let t = std::time::Instant::now();
                terminal.draw(|f| app.render(f))?;
                timing.draw_total_us += t.elapsed().as_micros() as u64;
                timing.draw_count += 1;
                if timing_enabled
                    && timing.first_render_time.is_none()
                    && timing.first_output_emit_time.is_some()
                {
                    timing.first_render_time = Some(std::time::Instant::now());
                }
            }
            app.needs_redraw = false;
        }

        // 3. Poll for events. Busy-state redraw does not need 60 FPS; a slower
        // cadence keeps the TUI responsive without wasting most cycles on the spinner.
        let poll_interval = if app.agent_busy {
            BUSY_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        let has_event = ratatui::crossterm::event::poll(poll_interval)?;
        if has_event {
            match ratatui::crossterm::event::read()? {
                Event::Key(key) => {
                    app.needs_redraw = true;
                    // Approval prompt: route y/n/a to approval channel
                    if is_approval_prompt_active()
                        && let Some(result) = approval_result_for_key(&key)
                    {
                        if let Some(sender) = take_approval_sender() {
                            let prompt_lines = app.output_lines.len();
                            let _ = sender.send(result.clone());
                            app.drain_output_from(prompt_lines);

                            if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                app.force_bottom();
                                cancel_agent(&state_handle, &agent_task, &agent_done, &agent_busy)
                                    .await?;
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
                            app.clear_approval_pin();
                            app.force_bottom();
                        }
                        continue;
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
                                app.mention_search_active = false;
                                app.mention_search_query.clear();
                            }
                            // Always export on exit, even if the agent errored out.
                            if let Some(ref export_path) = task_opts.export {
                                let export_result =
                                    export_conversation(&session, export_path).await;
                                report_conversation_export(
                                    &output_writer,
                                    task_opts.json,
                                    &export_result,
                                    true,
                                );
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
                            app.mention_search_active = false;
                            app.mention_search_query.clear();
                            continue;
                        }

                        // If agent is busy, cancel it
                        if agent_busy.load(Ordering::Relaxed) {
                            cancel_agent(&state_handle, &agent_task, &agent_done, &agent_busy)
                                .await?;
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
                            app.clear_pastes();
                        }
                        app.push_styled(
                            "Press Ctrl+C again to quit.",
                            Style::default().fg(theme::WARNING_FG),
                        );
                        continue;
                    }

                    if let Some(action) =
                        handle_key_event(key, app, &output_writer, &state_handle, &task_id).await?
                    {
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
                                                state.last_injected_plan_state_hash = None;
                                                state.strict_plan_mode_enabled = true;
                                            }
                                        }
                                        // Switch agent mode to Plan so write/edit tools are restricted
                                        {
                                            let sess = session.lock().await;
                                            sess.agent_loop_mut().await.set_mode(
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
                                                output_writer.clone(),
                                            )
                                            .await?;
                                            app.agent_busy = true;
                                        }
                                        // Drain the channel, snap to bottom, and
                                        // re-render so the just-submitted prompt is
                                        // visible before the agent starts streaming.
                                        drain_and_render_user_submit(
                                            terminal,
                                            app,
                                            output_rx,
                                        )?;
                                        continue;
                                    }

                                    // Local commands execute immediately even when agent is busy
                                    if cli_cmd.is_local_command() {
                                        let should_exit = handle_cli_only_command(
                                            cli_cmd,
                                            &text,
                                            app,
                                            &output_writer,
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
                                            // Always export on exit, even if the agent errored out.
                                            if let Some(ref export_path) = task_opts.export {
                                                let export_result =
                                                    export_conversation(&session, export_path)
                                                        .await;
                                                report_conversation_export(
                                                    &output_writer,
                                                    task_opts.json,
                                                    &export_result,
                                                    true,
                                                );
                                            }
                                            return Ok(());
                                        }
                                        continue;
                                    }

                                    // Block slash commands while approval prompt is active
                                    if is_approval_prompt_active() {
                                        app.push_styled(
                                        "Blocked: cannot process commands while approval is pending.",
                                        theme::status_style(),
                                    );
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
                                            // Message already echoed by handle_key_event
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
                                    // Message already echoed by handle_key_event; just enqueue
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
                                        output_writer.clone(),
                                    )
                                    .await?;
                                    app.agent_busy = true;
                                }
                                // Drain the channel, snap to bottom, and
                                // re-render so the just-submitted prompt is
                                // visible before the agent starts streaming.
                                drain_and_render_user_submit(
                                    terminal,
                                    app,
                                    output_rx,
                                )?;
                            }
                        }
                    }
                }
                Event::Paste(content) => {
                    app.needs_redraw = true;
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
                    app.has_resized = true;
                    app.needs_redraw = true;
                    // Ratatui handles resize automatically on next draw
                }
                Event::Mouse(mouse_event) => {
                    app.needs_redraw = true;
                    match mouse_event.kind {
                        ratatui::crossterm::event::MouseEventKind::ScrollDown => {
                            app.scroll_lines(3);
                        }
                        ratatui::crossterm::event::MouseEventKind::ScrollUp => {
                            app.scroll_lines(-3);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // 3b. Fire debounced mention search if timer expired
        if app.mention_search_active
            && !app.mention_search_query.is_empty()
            && std::time::Instant::now() >= app.mention_search_deadline
        {
            let query = app.mention_search_query.clone();
            let cwd = app.cwd.clone();
            let results = crate::core::file_search::search_workspace_files(&query, &cwd, 10).await;
            app.picker_active = true;
            app.picker_results = results;
            app.picker_index = 0;
            app.needs_redraw = true;
            // Push deadline far forward so we don't re-search until query changes
            app.mention_search_deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(3600);
        }

        // 4. Check agent completion (non-blocking)
        // Always check notification to avoid race condition where agent_busy is already false
        // but app.agent_busy hasn't been updated yet
        let agent_completed = agent_done.notified().now_or_never().is_some();
        if agent_completed {
            agent_busy.store(false, Ordering::Relaxed);
            app.agent_busy = false;
            app.needs_redraw = true;
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

        // Export conversation after each completed turn when --export is set.
        if agent_completed
            && let Some(export_path) = task_opts.export.clone()
        {
            let export_result = export_conversation(&session, &export_path).await;
            report_conversation_export(&output_writer, task_opts.json, &export_result, false);
        }

        // 5. Update elapsed time for status bar
        if app.agent_busy
            && let Some(start) = agent_start_time.lock().await.as_ref()
        {
            let new_elapsed = start.elapsed();
            let new_secs = new_elapsed.as_secs();
            let old_secs = app.elapsed.map(|e| e.as_secs()).unwrap_or(u64::MAX);
            app.elapsed = Some(new_elapsed);
            if new_secs != old_secs {
                app.needs_redraw = true;
            }
        }

        // 6. Tick spinner
        if app.tick_spinner() {
            app.needs_redraw = true;
        }
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
    let (output_tx, mut output_rx) = mpsc::channel(8192);
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
        sess.agent_loop().await.task_id().to_string()
    };

    // Set status bar fields from session info
    {
        let sess = session.lock().await;
        let agent_guard = sess.agent_loop().await;
        let provider = agent_guard.get_provider();
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
            let startup_info = sess.get_startup_info().await;
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
        *qh = Some(sess.queue_handle().await);
        let mut sh = state_handle.lock().await;
        *sh = Some(sess.state_handle().await);

        let _cwd = app.cwd.clone();
        let agent_loop = sess.agent_loop().await.state_handle();
        let skills = {
            let state = agent_loop.lock().await;
            state.available_skills.clone()
        };
        let local_toggles: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        let global_toggles: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        let remote_toggles: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();

        let entries = crate::cli::slash_commands::build_slash_command_entries(
            &skills,
            &local_toggles,
            &global_toggles,
            &remote_toggles,
        );
        app.slash_command_all_entries = entries;
    }

    // 6. Main loop
    let auto_approve = task_opts.yolo || task_opts.auto_approve_all;
    run_main_loop(
        &mut terminal,
        &mut app,
        &mut output_rx,
        output_writer,
        session,
        task_id.clone(),
        agent_busy,
        agent_done,
        agent_start_time,
        state_handle,
        agent_task,
        queue_handle,
        &task_opts,
        auto_approve,
    )
    .await?;
    // In JSON mode stdout is reserved for structured events, so route
    // the session ID to stderr to keep stdout parseable as JSONL.
    if task_opts.json {
        eprintln!("Session: {task_id}");
    } else {
        println!("Session: {task_id}");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::ser::{Error as _, Serialize, Serializer};

    fn reset_prompt_state() {
        crate::core::approval::clear_approval_prompt_scroll();
        crate::core::approval::set_approval_prompt_active(false);
        crate::core::approval::clear_followup_prompt_scroll();
        crate::core::approval::set_followup_question_active("test-task", false);
    }

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
    fn test_drain_output_resets_to_auto_on_new_output() {
        use crate::cli::output::OutputEvent;
        use crate::cli::tui::app::ScrollMode;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(OutputEvent::plain("new line")).unwrap();

        let mut app = App::new();
        app.set_content_height(5);
        app.set_content_width(80);
        for index in 0..20 {
            app.push_plain(format!("line {}", index));
        }
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 7;

        drain_output(&mut rx, &mut app);

        assert_eq!(app.scroll_mode, ScrollMode::Auto);
        assert_eq!(app.scroll_offset, 0);

        reset_prompt_state();
    }

    #[test]
    fn test_drain_output_reenables_auto_follow_at_bottom() {
        use crate::cli::output::OutputEvent;
        use crate::cli::tui::app::ScrollMode;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(OutputEvent::plain("new streamed line"))
            .unwrap();

        let mut app = App::new();
        app.set_content_height(5);
        app.set_content_width(80);
        for index in 0..20 {
            app.push_plain(format!("line {}", index));
        }
        app.scroll_mode = ScrollMode::Manual;
        // 20 lines + 1 new = 21 lines, content_height=5 → max_offset=16.
        // At exact bottom (offset=16), distance_from_bottom==0 → snaps to Auto.
        app.scroll_offset = 16;

        drain_output(&mut rx, &mut app);

        assert_eq!(app.scroll_mode, ScrollMode::Auto);

        reset_prompt_state();
    }

    #[test]
    fn test_drain_output_forces_scroll_for_approval_prompt() {
        use crate::cli::tui::app::ScrollMode;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (_tx, mut rx) = mpsc::channel(1);

        // Ensure clean state before test
        crate::core::approval::clear_approval_prompt_scroll();
        crate::core::approval::set_approval_prompt_scroll();

        let mut app = App::new();
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 7;

        drain_output(&mut rx, &mut app);

        assert_eq!(app.scroll_mode, ScrollMode::ApprovalPinned);
        assert_eq!(app.scroll_offset, 0);

        reset_prompt_state();
    }

    #[test]
    fn test_drain_output_forces_scroll_while_approval_prompt_is_active() {
        use crate::cli::tui::app::ScrollMode;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (_tx, mut rx) = mpsc::channel(1);

        // Ensure clean state before test
        crate::core::approval::clear_approval_prompt_scroll();
        crate::core::approval::set_approval_prompt_active(true);

        let mut app = App::new();
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 7;

        drain_output(&mut rx, &mut app);

        crate::core::approval::set_approval_prompt_active(false);

        assert_eq!(app.scroll_mode, ScrollMode::ApprovalPinned);
        assert_eq!(app.scroll_offset, 0);

        reset_prompt_state();
    }

    #[test]
    fn test_drain_output_forces_scroll_for_followup_prompt() {
        use crate::cli::tui::app::ScrollMode;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (_tx, mut rx) = mpsc::channel(1);

        crate::core::approval::clear_followup_prompt_scroll();
        crate::core::approval::set_followup_prompt_scroll();

        let mut app = App::new();
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 7;

        drain_output(&mut rx, &mut app);

        assert_eq!(app.scroll_mode, ScrollMode::ApprovalPinned);
        assert_eq!(app.scroll_offset, 0);

        reset_prompt_state();
    }

    #[test]
    fn test_drain_output_forces_scroll_while_followup_prompt_is_active() {
        use crate::cli::tui::app::ScrollMode;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (_tx, mut rx) = mpsc::channel(1);

        crate::core::approval::clear_followup_prompt_scroll();
        crate::core::approval::set_followup_question_active("test-task", true);

        let mut app = App::new();
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 7;

        drain_output(&mut rx, &mut app);

        crate::core::approval::set_followup_question_active("test-task", false);
        crate::core::approval::clear_followup_prompt_scroll();

        assert_eq!(app.scroll_mode, ScrollMode::ApprovalPinned);
        assert_eq!(app.scroll_offset, 0);

        reset_prompt_state();
    }

    #[test]
    fn test_drain_output_clears_approval_pin_when_prompt_resolves() {
        use crate::cli::tui::app::ScrollMode;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (_tx, mut rx) = mpsc::channel(1);
        crate::core::approval::clear_approval_prompt_scroll();
        crate::core::approval::set_approval_prompt_active(false);

        let mut app = App::new();
        app.pin_approval_bottom();

        drain_output(&mut rx, &mut app);

        assert_eq!(app.scroll_mode, ScrollMode::Auto);
        assert_eq!(app.scroll_offset, 0);

        reset_prompt_state();
    }

    #[test]
    fn test_drain_output_replaces_previous_completion_box() {
        use crate::cli::output::OutputEvent;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (tx, mut rx) = mpsc::channel(4);
        tx.try_send(OutputEvent::Completion("first completion".to_string()))
            .unwrap();

        let mut app = App::new();
        drain_output(&mut rx, &mut app);

        let first_rendered = app
            .completion_lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            first_rendered.contains("first completion"),
            "expected first completion to render, got: {first_rendered}"
        );

        tx.try_send(OutputEvent::Completion("second completion".to_string()))
            .unwrap();
        drain_output(&mut rx, &mut app);

        let second_rendered = app
            .completion_lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !second_rendered.contains("first completion"),
            "previous completion box should be replaced, got: {second_rendered}"
        );
        assert!(
            second_rendered.contains("second completion"),
            "expected second completion to replace the first, got: {second_rendered}"
        );

        reset_prompt_state();
    }

    /// End-to-end test for the markdown re-render fix. Streamed model
    /// text arrives as raw `OutputEvent::Line` events. When
    /// `OutputEvent::TurnEnd` arrives, `drain_output` should swap the
    /// streamed raw lines for the markdown-rendered version of the
    /// original accumulated text.
    #[test]
    fn test_drain_output_rerenders_streamed_text_as_markdown_on_turn_end() {
        use crate::cli::output::OutputEvent;

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (tx, mut rx) = mpsc::channel(8);
        // Simulate the agent loop streaming three lines that are
        // fragments of the original markdown "**bold** text\n\nmore".
        tx.try_send(OutputEvent::model_output("  **bold"))
            .unwrap();
        tx.try_send(OutputEvent::model_output("  text"))
            .unwrap();
        tx.try_send(OutputEvent::model_output("  more"))
            .unwrap();
        // The agent loop emits TurnEnd with the raw markdown text
        // when the turn finishes.
        tx.try_send(OutputEvent::TurnEnd {
            accumulated_text: "**bold** text\n\nmore".to_string(),
        })
        .unwrap();

        let mut app = App::new();
        drain_output(&mut rx, &mut app);

        let rendered: Vec<String> = app
            .output_lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        // The raw streamed lines should be gone.
        assert!(
            !rendered.iter().any(|s| s.starts_with("  **bold")),
            "raw streamed lines should be replaced: {:?}",
            rendered
        );
        assert!(
            !rendered.iter().any(|s| s == "  text"),
            "raw streamed lines should be replaced: {:?}",
            rendered
        );
        // No 🚀 banner (that's for completion, not agent text).
        assert!(
            !rendered.iter().any(|s| s.contains("🚀")),
            "agent-text re-render must not include the completion banner: {:?}",
            rendered
        );
        // The markdown content should be present.
        let joined = rendered.join("\n");
        assert!(
            joined.contains("bold"),
            "rendered content should contain 'bold': {}",
            joined
        );
        assert!(
            joined.contains("text"),
            "rendered content should contain 'text': {}",
            joined
        );
        assert!(
            joined.contains("more"),
            "rendered content should contain 'more': {}",
            joined
        );

        // The recorded indices buffer is cleared after finalize.
        assert!(app.turn_stream_line_indices.is_empty());

        reset_prompt_state();
    }

    struct NullOutputWriter;

    impl crate::cli::output::OutputWriter for NullOutputWriter {
        fn emit(&self, _event: crate::cli::output::OutputEvent) {}

        fn flush(&self) {}
    }

    #[test]
    fn test_build_user_message_content_includes_images() {
        use std::sync::Arc;

        let tmp_path = std::env::temp_dir().join(format!(
            "sned-interactive-image-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&tmp_path, b"not-a-real-png-but-valid-by-extension").unwrap();

        let writer: OutputWriterArc = Arc::new(NullOutputWriter);
        let model_info = crate::providers::ModelInfo {
            name: Some("image-model".to_string()),
            supports_images: Some(true),
            ..Default::default()
        };

        let content = build_user_message_content(
            "hello".to_string(),
            vec![tmp_path.to_string_lossy().into_owned()],
            model_info,
            &writer,
            true,
        );

        std::fs::remove_file(&tmp_path).unwrap();

        let crate::providers::MessageContent::UserBlocks(blocks) = content else {
            panic!("Expected UserBlocks");
        };
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            crate::providers::UserContentBlock::Text(text) => assert_eq!(text.text, "hello"),
            _ => panic!("Expected text block first"),
        }
        match &blocks[1] {
            crate::providers::UserContentBlock::Image(image) => match &image.source {
                crate::providers::ImageSource::Base64 { media_type, data } => {
                    assert_eq!(media_type, "image/png");
                    assert!(!data.is_empty());
                }
                _ => panic!("Expected base64 image source"),
            },
            _ => panic!("Expected image block second"),
        }
    }

    #[test]
    fn test_build_user_message_content_warns_when_images_unsupported() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Default)]
        struct CapturingWriter {
            events: StdMutex<Vec<crate::cli::output::OutputEvent>>,
        }

        impl crate::cli::output::OutputWriter for CapturingWriter {
            fn emit(&self, event: crate::cli::output::OutputEvent) {
                self.events.lock().unwrap().push(event);
            }

            fn flush(&self) {}
        }

        let tmp_path = std::env::temp_dir().join(format!(
            "sned-interactive-image-unsupported-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&tmp_path, b"still-valid-by-extension").unwrap();

        let writer = Arc::new(CapturingWriter::default());
        let model_info = crate::providers::ModelInfo {
            name: Some("text-only".to_string()),
            supports_images: Some(false),
            ..Default::default()
        };
        let writer_arc: OutputWriterArc = writer.clone();

        let content = build_user_message_content(
            "hello".to_string(),
            vec![tmp_path.to_string_lossy().into_owned()],
            model_info,
            &writer_arc,
            true,
        );

        std::fs::remove_file(&tmp_path).unwrap();

        assert!(matches!(
            content,
            crate::providers::MessageContent::Text(text) if text == "hello"
        ));
        let events = writer.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let crate::cli::output::OutputEvent::Line(line) = &events[0] else {
            panic!("Expected warning line");
        };
        let rendered = line.to_string();
        assert!(rendered.contains("does not support images"));
        assert!(rendered.contains("Ignoring 1 image(s)"));
    }

    #[test]
    fn test_serialize_conversation_export_reports_failure() {
        struct FailingSerialize;

        impl Serialize for FailingSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                Err(S::Error::custom("boom"))
            }
        }

        let result = serialize_conversation_export(&FailingSerialize);
        let err = result.expect_err("serialization should fail");
        assert!(err.contains("Failed to serialize conversation"));
        assert!(err.contains("boom"));
    }

    #[test]
    fn test_write_conversation_export_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let result = write_conversation_export(dir.path().to_str().unwrap(), "[]");
        let err = result.expect_err("writing to a directory should fail");
        assert!(err.contains("Failed to write export file"));
    }

    #[test]
    fn test_report_conversation_export_emits_warning_for_failure() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Default)]
        struct CapturingWriter {
            events: StdMutex<Vec<crate::cli::output::OutputEvent>>,
        }

        impl crate::cli::output::OutputWriter for CapturingWriter {
            fn emit(&self, event: crate::cli::output::OutputEvent) {
                self.events.lock().unwrap().push(event);
            }

            fn flush(&self) {}
        }

        let writer = Arc::new(CapturingWriter::default());
        let writer_arc: OutputWriterArc = writer.clone();
        let result = Err("Failed to write export file: boom".to_string());
        report_conversation_export(&writer_arc, false, &result, true);

        let events = writer.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let crate::cli::output::OutputEvent::Line(line) = &events[0] else {
            panic!("Expected warning line");
        };
        let rendered = line.to_string();
        assert!(rendered.contains("Warning"));
        assert!(rendered.contains("Failed to write export file"));
    }

    #[test]
    fn test_report_conversation_export_suppresses_turn_success() {
        use std::sync::{Arc, Mutex as StdMutex};

        #[derive(Default)]
        struct CapturingWriter {
            events: StdMutex<Vec<crate::cli::output::OutputEvent>>,
        }

        impl crate::cli::output::OutputWriter for CapturingWriter {
            fn emit(&self, event: crate::cli::output::OutputEvent) {
                self.events.lock().unwrap().push(event);
            }

            fn flush(&self) {}
        }

        let writer = Arc::new(CapturingWriter::default());
        let writer_arc: OutputWriterArc = writer.clone();
        let result = Ok("Conversation exported to: /tmp/out.json (secrets redacted)".to_string());
        report_conversation_export(&writer_arc, false, &result, false);

        let events = writer.events.lock().unwrap();
        assert!(
            events.is_empty(),
            "per-turn export should stay silent on success"
        );
    }

    #[test]
    fn test_strip_active_slash_command_uses_last_command_position() {
        let text = "foo/bar /baz";
        assert_eq!(
            strip_active_slash_command(text).as_deref(),
            Some("foo/bar ")
        );
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

    fn slash_completion_test_app() -> App {
        let mut app = App::new();
        app.input = App::new_textarea(vec!["/pl".to_string()]);
        app.slash_command_active = true;
        app.slash_command_results = vec![crate::cli::slash_commands::SlashCommandEntry {
            name: "plan".to_string(),
            description: "View or manage the current plan".to_string(),
            aliases: vec![],
            category: crate::cli::slash_commands::SlashCommandCategory::Plan,
            requires_args: false,
        }];
        app.slash_command_selected = 0;
        app
    }

    /// Reproduces the user-reported bug: tab completion for `/plan` does
    /// not dismiss the popup when the completed input is still a valid
    /// slash command. The fix should keep the popup hidden until the user
    /// starts a new query (separator, character, or movement).
    #[tokio::test]
    async fn test_slash_completion_dismissed_until_new_query() -> anyhow::Result<()> {
        use crate::cli::slash_commands::{SlashCommandCategory, SlashCommandEntry};
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (tx, _rx) = mpsc::channel(4);
        let output_writer: OutputWriterArc = Arc::new(ChannelOutputWriter::new(tx));
        let state_handle = Arc::new(Mutex::new(None));

        // Set up: input `/pl`, picker active with multiple matches.
        let mut app = App::new();
        app.input = App::new_textarea(vec!["/pl".to_string()]);
        app.slash_command_active = true;
        app.slash_command_results = vec![
            SlashCommandEntry {
                name: "plan".to_string(),
                description: "View or manage the current plan".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Plan,
                requires_args: false,
            },
            SlashCommandEntry {
                name: "plan-prompt".to_string(),
                description: "Prompt with plan".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Plan,
                requires_args: false,
            },
        ];
        app.slash_command_selected = 0;
        app.slash_command_all_entries = app.slash_command_results.clone();

        // Press Tab to accept "plan"
        let action = handle_key_event(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &mut app,
            &output_writer,
            &state_handle,
            "task-1",
        )
        .await?;
        assert!(action.is_none());
        assert_eq!(app.input.lines().join("\n"), "/plan");
        assert!(
            !app.slash_command_active,
            "picker must be hidden immediately after Tab"
        );

        // After Tab, the picker should stay hidden until the user starts
        // a new query. Simulate the next key event being a no-op for the
        // picker (an arrow key). The picker should remain hidden.
        let action = handle_key_event(
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
            &mut app,
            &output_writer,
            &state_handle,
            "task-1",
        )
        .await?;
        assert!(action.is_none());
        assert_eq!(app.input.lines().join("\n"), "/plan");
        assert!(
            !app.slash_command_active,
            "picker must stay hidden when user navigates within the completed input"
        );

        // Once the user types a real new character, the picker should
        // re-open with refreshed results.
        let action = handle_key_event(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
            &mut app,
            &output_writer,
            &state_handle,
            "task-1",
        )
        .await?;
        assert!(action.is_none());
        assert_eq!(app.input.lines().join("\n"), "/plan ");
        assert!(
            app.slash_command_active,
            "picker must re-open when user types a separator (space) after a completed command"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_key_event_accepts_slash_completion_with_enter() -> anyhow::Result<()> {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (tx, _rx) = mpsc::channel(4);
        let output_writer: OutputWriterArc = Arc::new(ChannelOutputWriter::new(tx));
        let state_handle = Arc::new(Mutex::new(None));
        let mut app = slash_completion_test_app();

        let action = handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut app,
            &output_writer,
            &state_handle,
            "task-1",
        )
        .await?;

        assert!(action.is_none());
        assert_eq!(app.input.lines().join("\n"), "/plan");
        assert!(!app.slash_command_active);
        assert!(app.slash_command_results.is_empty());
        assert_eq!(app.slash_command_selected, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_key_event_accepts_slash_completion_with_tab() -> anyhow::Result<()> {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (tx, _rx) = mpsc::channel(4);
        let output_writer: OutputWriterArc = Arc::new(ChannelOutputWriter::new(tx));
        let state_handle = Arc::new(Mutex::new(None));
        let mut app = slash_completion_test_app();

        let action = handle_key_event(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &mut app,
            &output_writer,
            &state_handle,
            "task-1",
        )
        .await?;

        assert!(action.is_none());
        assert_eq!(app.input.lines().join("\n"), "/plan");
        assert!(!app.slash_command_active);
        assert!(app.slash_command_results.is_empty());
        assert_eq!(app.slash_command_selected, 0);

        // After Tab, render the app and assert the slash command overlay
        // is NOT in the buffer. This is the user-visible bug:
        // "Tab completion for /plan doesn't disappear the popup box."
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");
        let buffer = terminal.backend().buffer().clone();
        let mut found_overlay = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol().contains("Slash Commands") {
                    found_overlay = true;
                    break;
                }
            }
            if found_overlay {
                break;
            }
        }
        assert!(
            !found_overlay,
            "slash command overlay must not appear in the rendered buffer after Tab completion"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_key_event_blocks_normal_submit_while_approval_is_active()
    -> anyhow::Result<()> {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (tx, _rx) = mpsc::channel(4);
        let output_writer: OutputWriterArc = Arc::new(ChannelOutputWriter::new(tx));
        let state_handle = Arc::new(Mutex::new(None));
        let mut app = App::new();
        app.input = App::new_textarea(vec!["hello".to_string()]);

        crate::core::approval::set_approval_prompt_active(true);
        let action = handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut app,
            &output_writer,
            &state_handle,
            "task-1",
        )
        .await?;
        crate::core::approval::set_approval_prompt_active(false);

        assert!(action.is_none());
        assert_eq!(app.input.lines().join("\n"), "hello");
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Approval pending. Type y, n, or a first.")
        }));

        reset_prompt_state();

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_key_event_allows_shutdown_submit_during_approval() -> anyhow::Result<()> {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let _lock = crate::core::approval::approval_test_guard();
        reset_prompt_state();

        let (tx, _rx) = mpsc::channel(4);
        let output_writer: OutputWriterArc = Arc::new(ChannelOutputWriter::new(tx));
        let state_handle = Arc::new(Mutex::new(None));
        let mut app = App::new();
        app.input = App::new_textarea(vec!["/quit".to_string()]);

        crate::core::approval::set_approval_prompt_active(true);
        let action = handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut app,
            &output_writer,
            &state_handle,
            "task-1",
        )
        .await?;
        crate::core::approval::set_approval_prompt_active(false);

        assert!(matches!(action, Some(Action::Submit(text)) if text == "/quit"));
        assert!(app.input.lines().join("\n").is_empty());

        reset_prompt_state();

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_key_event_allows_shutdown_submit_while_agent_busy() -> anyhow::Result<()> {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (tx, _rx) = mpsc::channel(4);
        let output_writer: OutputWriterArc = Arc::new(ChannelOutputWriter::new(tx));
        let state_handle = Arc::new(Mutex::new(None));
        let mut app = App::new();
        app.agent_busy = true;
        app.input = App::new_textarea(vec!["/exit".to_string()]);

        let action = handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut app,
            &output_writer,
            &state_handle,
            "task-1",
        )
        .await?;

        assert!(matches!(action, Some(Action::Submit(text)) if text == "/exit"));
        assert!(app.input.lines().join("\n").is_empty());

        Ok(())
    }

    fn retry_test_task_opts() -> TaskOptions {
        TaskOptions {
            act: false,
            plan: false,
            yolo: false,
            auto_approve_all: false,
            timeout: None,
            model: None,
            provider: Some("mock".to_string()),
            base_url: None,
            api_key: None,
            verbose: false,
            cwd: None,
            config: None,
            thinking: None,
            reasoning_effort: None,
            max_consecutive_mistakes: None,
            json: false,
            double_check_completion: false,
            auto_condense: true,
            no_token_display: false,
            subagents: false,
            is_subagent: false,
            user_agent: None,
            hooks_dir: None,
            export: None,
            image: vec![],
            track_changes: false,
            max_context_turns: None,
            max_tokens: None,
            debug: false,
        }
    }

    #[tokio::test]
    async fn test_retry_command_reports_when_no_failed_request_exists() -> anyhow::Result<()> {
        use crate::cli::slash_commands::CliOnlyCommand;

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(
                retry_test_task_opts(),
                RootOnlyOptions {
                    task_id: None,
                    continue_task: false,
                },
                None,
            )
            .await?,
        ));

        let mut app = App::new();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::Retry,
            "/retry",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &retry_test_task_opts(),
            false,
        )
        .await?;

        assert!(!should_exit);
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("No safe failed request is available to retry")
        }));
        Ok(())
    }

    #[tokio::test]
    async fn test_retry_command_queues_failed_request_to_run_next_when_busy() -> anyhow::Result<()>
    {
        use crate::cli::slash_commands::CliOnlyCommand;

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(
                retry_test_task_opts(),
                RootOnlyOptions {
                    task_id: None,
                    continue_task: false,
                },
                None,
            )
            .await?,
        ));
        {
            let state_handle = {
                let sess = session.lock().await;
                sess.agent_loop().await.state_handle()
            };
            let mut state = state_handle.lock().await;
            state.retryable_failed_request = Some(crate::providers::StorageMessage {
                id: None,
                role: crate::providers::MessageRole::User,
                content: crate::providers::MessageContent::Text("retry me".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            });
        }
        {
            let sess = session.lock().await;
            sess.queue_handle()
                .await
                .enqueue_text_message("older queued message".to_string())
                .await;
        }

        let mut app = App::new();
        let agent_busy = Arc::new(AtomicBool::new(true));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::Retry,
            "/retry",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &retry_test_task_opts(),
            false,
        )
        .await?;

        assert!(!should_exit);
        let queued = {
            let sess = session.lock().await;
            sess.queue_handle().await.peek_queued_messages(2).await
        };
        assert_eq!(
            queued,
            vec!["retry me".to_string(), "older queued message".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_plan_replace_is_rejected_while_plan_is_running() -> anyhow::Result<()> {
        use crate::cli::slash_commands::{CliOnlyCommand, PlanSubcommand};
        use crate::core::plan_state::{PlanState, PlanStepStatus};

        let task_opts = TaskOptions {
            act: false,
            plan: true,
            yolo: false,
            auto_approve_all: false,
            timeout: None,
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            base_url: None,
            api_key: Some("test-key".to_string()),
            verbose: false,
            cwd: None,
            config: None,
            thinking: None,
            reasoning_effort: None,
            max_consecutive_mistakes: None,
            json: false,
            double_check_completion: false,
            auto_condense: true,
            no_token_display: false,
            subagents: false,
            is_subagent: false,
            user_agent: None,
            hooks_dir: None,
            export: None,
            image: vec![],
            track_changes: false,
            max_context_turns: None,
            max_tokens: None,
            debug: false,
        };
        let root_opts = RootOnlyOptions {
            task_id: None,
            continue_task: false,
        };

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(task_opts.clone(), root_opts, None).await?,
        ));
        let state_handle = {
            let sess = session.lock().await;
            sess.agent_loop().await.state_handle()
        };
        {
            let mut state = state_handle.lock().await;
            let mut plan =
                PlanState::create_plan(vec!["Initial step".to_string(), "Second step".to_string()]);
            plan.approved = true;
            plan.current_step_index = 0;
            plan.steps[0].status = PlanStepStatus::Running;
            state.plan_state = Some(plan);
        }

        let mut app = App::new();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::Plan(PlanSubcommand::Replace(
                "1. Replaced step one\n2. Replaced step two".to_string(),
            )),
            "/plan replace 1. Replaced step one\n2. Replaced step two",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &task_opts,
            false,
        )
        .await?;

        assert!(!should_exit);
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Cannot replace plan while plan is running")
        }));

        let state = state_handle.lock().await;
        let plan = state.plan_state.as_ref().expect("plan should remain");
        assert!(plan.approved);
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.current_step_index, 0);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Running);
        assert_eq!(plan.steps[0].description, "Initial step");
        assert_eq!(plan.steps[1].description, "Second step");
        Ok(())
    }

    #[tokio::test]
    async fn test_plan_approve_transitions_to_act_and_starts_execution() -> anyhow::Result<()> {
        use crate::cli::slash_commands::CliOnlyCommand;
        use crate::core::agent_types::AgentMode;
        use crate::core::plan_state::{PlanState, PlanStepStatus};

        let task_opts = TaskOptions {
            act: false,
            plan: true,
            yolo: false,
            auto_approve_all: false,
            timeout: None,
            model: None,
            provider: Some("mock".to_string()),
            base_url: None,
            api_key: None,
            verbose: false,
            cwd: None,
            config: None,
            thinking: None,
            reasoning_effort: None,
            max_consecutive_mistakes: None,
            json: false,
            double_check_completion: false,
            auto_condense: true,
            no_token_display: false,
            subagents: false,
            is_subagent: false,
            user_agent: None,
            hooks_dir: None,
            export: None,
            image: vec![],
            track_changes: false,
            max_context_turns: None,
            max_tokens: None,
            debug: false,
        };
        let root_opts = RootOnlyOptions {
            task_id: None,
            continue_task: false,
        };

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(task_opts.clone(), root_opts, None).await?,
        ));
        let state_handle = {
            let sess = session.lock().await;
            sess.agent_loop().await.state_handle()
        };
        {
            let mut state = state_handle.lock().await;
            state.strict_plan_mode_enabled = true;
            let plan =
                PlanState::create_plan(vec!["First step".to_string(), "Second step".to_string()]);
            state.plan_state = Some(plan);
        }

        let mut app = App::new();
        app.mode = "PLAN".to_string();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::PlanApprove,
            "/plan approve",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &task_opts,
            false,
        )
        .await?;

        assert!(!should_exit);
        assert_eq!(app.mode, "ACT");
        assert!(app.agent_busy);
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Plan approved. Starting from step 1/2: First step")
        }));

        {
            let sess = session.lock().await;
            assert_eq!(sess.agent_loop().await.mode(), AgentMode::Act);
        }

        let state = state_handle.lock().await;
        assert!(!state.strict_plan_mode_enabled);
        let plan = state.plan_state.as_ref().expect("plan should exist");
        assert!(plan.approved);
        assert_eq!(plan.current_step_index, 0);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Running);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Pending);

        if let Some(task) = agent_task.lock().await.take() {
            task.abort();
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_plan_pause_marks_running_plan_paused() -> anyhow::Result<()> {
        use crate::cli::slash_commands::CliOnlyCommand;
        use crate::core::plan_state::{PlanState, PlanStepStatus};

        let task_opts = TaskOptions {
            act: false,
            plan: true,
            yolo: false,
            auto_approve_all: false,
            timeout: None,
            model: None,
            provider: Some("mock".to_string()),
            base_url: None,
            api_key: None,
            verbose: false,
            cwd: None,
            config: None,
            thinking: None,
            reasoning_effort: None,
            max_consecutive_mistakes: None,
            json: false,
            double_check_completion: false,
            auto_condense: true,
            no_token_display: false,
            subagents: false,
            is_subagent: false,
            user_agent: None,
            hooks_dir: None,
            export: None,
            image: vec![],
            track_changes: false,
            max_context_turns: None,
            max_tokens: None,
            debug: false,
        };
        let root_opts = RootOnlyOptions {
            task_id: None,
            continue_task: false,
        };

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(task_opts.clone(), root_opts, None).await?,
        ));
        let state_handle = {
            let sess = session.lock().await;
            sess.agent_loop().await.state_handle()
        };
        {
            let mut state = state_handle.lock().await;
            let mut plan =
                PlanState::create_plan(vec!["Initial step".to_string(), "Second step".to_string()]);
            plan.approved = true;
            plan.current_step_index = 0;
            plan.steps[0].status = PlanStepStatus::Running;
            state.strict_plan_mode_enabled = false;
            state.plan_state = Some(plan);
        }

        let mut app = App::new();
        app.mode = "ACT".to_string();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::PlanPause,
            "/plan pause",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &task_opts,
            false,
        )
        .await?;

        assert!(!should_exit);
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Plan paused. Use /plan resume to continue.")
        }));
        assert_eq!(app.mode, "ACT");

        let state = state_handle.lock().await;
        let plan = state.plan_state.as_ref().expect("plan should remain");
        assert!(plan.approved);
        assert!(plan.paused);
        assert_eq!(plan.current_step_index, 0);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Running);

        Ok(())
    }

    #[tokio::test]
    async fn test_plan_resume_unpauses_failed_step_and_starts_execution() -> anyhow::Result<()> {
        use crate::cli::slash_commands::CliOnlyCommand;
        use crate::core::plan_state::{PlanState, PlanStepStatus};

        let task_opts = TaskOptions {
            act: false,
            plan: true,
            yolo: false,
            auto_approve_all: false,
            timeout: None,
            model: None,
            provider: Some("mock".to_string()),
            base_url: None,
            api_key: None,
            verbose: false,
            cwd: None,
            config: None,
            thinking: None,
            reasoning_effort: None,
            max_consecutive_mistakes: None,
            json: false,
            double_check_completion: false,
            auto_condense: true,
            no_token_display: false,
            subagents: false,
            is_subagent: false,
            user_agent: None,
            hooks_dir: None,
            export: None,
            image: vec![],
            track_changes: false,
            max_context_turns: None,
            max_tokens: None,
            debug: false,
        };
        let root_opts = RootOnlyOptions {
            task_id: None,
            continue_task: false,
        };

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(task_opts.clone(), root_opts, None).await?,
        ));
        let state_handle = {
            let sess = session.lock().await;
            sess.agent_loop().await.state_handle()
        };
        {
            let mut state = state_handle.lock().await;
            let mut plan =
                PlanState::create_plan(vec!["Initial step".to_string(), "Second step".to_string()]);
            plan.approved = true;
            plan.paused = true;
            plan.current_step_index = 0;
            plan.steps[0].status = PlanStepStatus::Failed;
            state.strict_plan_mode_enabled = false;
            state.plan_state = Some(plan);
        }

        let mut app = App::new();
        app.mode = "ACT".to_string();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::PlanResume,
            "/plan resume",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &task_opts,
            false,
        )
        .await?;

        assert!(!should_exit);
        assert!(app.agent_busy);
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Plan resumed at step 1/2: Initial step")
        }));
        assert_eq!(app.mode, "ACT");

        {
            let state = state_handle.lock().await;
            let plan = state.plan_state.as_ref().expect("plan should remain");
            assert!(plan.approved);
            assert!(!plan.paused);
            assert_eq!(plan.current_step_index, 0);
            assert_eq!(plan.steps[0].status, PlanStepStatus::Running);
            assert_eq!(plan.steps[1].status, PlanStepStatus::Pending);
        }

        if let Some(task) = agent_task.lock().await.take() {
            task.abort();
        }
        agent_busy.store(false, Ordering::Relaxed);

        Ok(())
    }

    #[tokio::test]
    async fn test_plan_abort_clears_paused_plan() -> anyhow::Result<()> {
        use crate::cli::slash_commands::CliOnlyCommand;
        use crate::core::plan_state::{PlanState, PlanStepStatus};

        let task_opts = TaskOptions {
            act: false,
            plan: true,
            yolo: false,
            auto_approve_all: false,
            timeout: None,
            model: None,
            provider: Some("mock".to_string()),
            base_url: None,
            api_key: None,
            verbose: false,
            cwd: None,
            config: None,
            thinking: None,
            reasoning_effort: None,
            max_consecutive_mistakes: None,
            json: false,
            double_check_completion: false,
            auto_condense: true,
            no_token_display: false,
            subagents: false,
            is_subagent: false,
            user_agent: None,
            hooks_dir: None,
            export: None,
            image: vec![],
            track_changes: false,
            max_context_turns: None,
            max_tokens: None,
            debug: false,
        };
        let root_opts = RootOnlyOptions {
            task_id: None,
            continue_task: false,
        };

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(task_opts.clone(), root_opts, None).await?,
        ));
        let state_handle = {
            let sess = session.lock().await;
            sess.agent_loop().await.state_handle()
        };
        {
            let mut state = state_handle.lock().await;
            let mut plan =
                PlanState::create_plan(vec!["Initial step".to_string(), "Second step".to_string()]);
            plan.approved = true;
            plan.paused = true;
            plan.current_step_index = 0;
            plan.steps[0].status = PlanStepStatus::Failed;
            state.strict_plan_mode_enabled = false;
            state.plan_state = Some(plan);
            state.last_injected_plan_state_hash = Some(12345);
        }

        let mut app = App::new();
        app.mode = "PLAN".to_string();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::PlanAbort,
            "/plan abort",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &task_opts,
            false,
        )
        .await?;

        assert!(!should_exit);
        assert_eq!(app.mode, "ACT");
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Plan aborted. Already-applied changes are kept.")
        }));

        let state = state_handle.lock().await;
        assert!(state.plan_state.is_none());
        assert!(state.strict_plan_mode_enabled);
        assert_eq!(state.last_injected_plan_state_hash, None);

        Ok(())
    }

    /// Regression test for the user-reported bug: when the user enters
    /// plan mode via `/plan <prompt>` (or `--plan`) and the model
    /// answers the follow-up question without calling
    /// `plan_mode_respond`, no plan_state is ever created. `/plan abort`
    /// used to be a no-op in that case ("No active plan to abort."),
    /// leaving the user stuck in plan mode with no way to switch back
    /// to act mode. The fix checks the agent mode rather than only
    /// plan_state.is_some() and always transitions to Act.
    #[tokio::test]
    async fn test_plan_abort_exits_plan_mode_without_plan_state() -> anyhow::Result<()> {
        use crate::cli::slash_commands::CliOnlyCommand;

        let task_opts = TaskOptions {
            act: false,
            plan: true,
            yolo: false,
            auto_approve_all: false,
            timeout: None,
            model: None,
            provider: Some("mock".to_string()),
            base_url: None,
            api_key: None,
            verbose: false,
            cwd: None,
            config: None,
            thinking: None,
            reasoning_effort: None,
            max_consecutive_mistakes: None,
            json: false,
            double_check_completion: false,
            auto_condense: true,
            no_token_display: false,
            subagents: false,
            is_subagent: false,
            user_agent: None,
            hooks_dir: None,
            export: None,
            image: vec![],
            track_changes: false,
            max_context_turns: None,
            max_tokens: None,
            debug: false,
        };
        let root_opts = RootOnlyOptions {
            task_id: None,
            continue_task: false,
        };

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(task_opts.clone(), root_opts, None).await?,
        ));
        let state_handle = {
            let sess = session.lock().await;
            sess.agent_loop().await.state_handle()
        };
        {
            let mut state = state_handle.lock().await;
            // No plan_state — the model answered a follow-up question
            // without ever calling plan_mode_respond. The agent is
            // still in Plan mode from the original /plan <prompt>.
            state.plan_state = None;
            state.strict_plan_mode_enabled = true;
        }
        {
            let sess = session.lock().await;
            sess.agent_loop_mut()
                .await
                .set_mode(crate::core::agent_types::AgentMode::Plan);
        }

        let mut app = App::new();
        app.mode = "PLAN".to_string();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::PlanAbort,
            "/plan abort",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &task_opts,
            false,
        )
        .await?;

        assert!(!should_exit);
        assert_eq!(
            app.mode, "ACT",
            "/plan abort must transition the user out of plan mode even when no plan_state was created"
        );
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Exited plan mode")
        }));
        let state = state_handle.lock().await;
        assert!(state.plan_state.is_none());
        assert!(state.strict_plan_mode_enabled);
        let sess = session.lock().await;
        assert_eq!(
            sess.agent_loop().await.mode(),
            crate::core::agent_types::AgentMode::Act,
            "agent loop must also be switched out of Plan mode"
        );

        Ok(())
    }

    /// Regression test for the `--plan` flag entry path: when the user
    /// starts a task with `TaskOptions { plan: true, .. }` and the model
    /// answers a follow-up question without calling `plan_mode_respond`,
    /// `/plan abort` must still exit plan mode. Unlike
    /// `test_plan_abort_exits_plan_mode_without_plan_state`, this test
    /// does NOT explicitly call `set_mode(Plan)` after building the
    /// session — it relies on the flag-driven `build_task_components`
    /// initialization in `src/cli/mod.rs:1107` to set Plan mode. This
    /// guards against a future change to that initialization silently
    /// breaking the abort path for the `--plan` flag.
    #[tokio::test]
    async fn test_plan_abort_exits_plan_mode_from_flag_entry() -> anyhow::Result<()> {
        use crate::cli::slash_commands::CliOnlyCommand;

        let task_opts = TaskOptions {
            act: false,
            plan: true,
            yolo: false,
            auto_approve_all: false,
            timeout: None,
            model: None,
            provider: Some("mock".to_string()),
            base_url: None,
            api_key: None,
            verbose: false,
            cwd: None,
            config: None,
            thinking: None,
            reasoning_effort: None,
            max_consecutive_mistakes: None,
            json: false,
            double_check_completion: false,
            auto_condense: true,
            no_token_display: false,
            subagents: false,
            is_subagent: false,
            user_agent: None,
            hooks_dir: None,
            export: None,
            image: vec![],
            track_changes: false,
            max_context_turns: None,
            max_tokens: None,
            debug: false,
        };
        let root_opts = RootOnlyOptions {
            task_id: None,
            continue_task: false,
        };

        let session = Arc::new(Mutex::new(
            InteractiveSession::build_with_writer(task_opts.clone(), root_opts, None).await?,
        ));
        let state_handle = {
            let sess = session.lock().await;
            sess.agent_loop().await.state_handle()
        };

        // Pre-conditions: the --plan flag must put the agent into Plan
        // mode without any plan_state being created.
        {
            let sess = session.lock().await;
            assert_eq!(
                sess.agent_loop().await.mode(),
                crate::core::agent_types::AgentMode::Plan,
                "TaskOptions {{ plan: true, .. }} must initialize the agent in Plan mode"
            );
        }
        let state = state_handle.lock().await;
        assert!(
            state.plan_state.is_none(),
            "no plan_state should exist at session start"
        );
        drop(state);

        let mut app = App::new();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_start_time = Arc::new(Mutex::new(None));
        let agent_task = Arc::new(Mutex::new(None));
        let state_handle_slot = Arc::new(Mutex::new(None));
        let output_writer: OutputWriterArc = Arc::new(crate::cli::output::StderrOutputWriter);
        let task_id = {
            let sess = session.lock().await;
            sess.agent_loop().await.task_id().to_string()
        };

        let should_exit = handle_cli_only_command(
            CliOnlyCommand::PlanAbort,
            "/plan abort",
            &mut app,
            &output_writer,
            &session,
            &task_id,
            &agent_busy,
            &agent_done,
            &agent_start_time,
            &agent_task,
            &state_handle_slot,
            &task_opts,
            false,
        )
        .await?;

        assert!(!should_exit);
        assert_eq!(
            app.mode, "ACT",
            "/plan abort must transition out of plan mode entered via the --plan flag"
        );
        assert!(app.output_lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Exited plan mode")
        }));
        let state = state_handle.lock().await;
        assert!(state.plan_state.is_none());
        assert!(state.strict_plan_mode_enabled);
        let sess = session.lock().await;
        assert_eq!(
            sess.agent_loop().await.mode(),
            crate::core::agent_types::AgentMode::Act,
            "agent loop must also be switched out of Plan mode after --plan flag abort"
        );

        Ok(())
    }

    /// Regression test for the "Ctrl+C then /plan stalls" bug.
    ///
    /// `cancel_agent` aborts the spawned task, but `task.abort()` also
    /// cancels the task's epilogue — the code that resets `agent_busy`
    /// to `false` and notifies `agent_done`. Without an explicit reset
    /// in `cancel_agent`, the atomic would stay `true` after Ctrl+C and
    /// the next message submission would be enqueued (because the
    /// enqueue-vs-spawn branch sees `agent_busy == true`) into a queue
    /// that nothing consumes (the agent's `run()` loop is gone).
    #[tokio::test]
    async fn test_cancel_agent_resets_busy_atomic_after_abort() {
        use std::time::Duration;
        use tokio::task::JoinHandle;

        // Stuck-agent stand-in: a 60-second sleep. Abort will cancel it
        // before the sleep returns, so the epilogue (which doesn't
        // exist for this stand-in anyway) never runs.
        let task_slot: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        *task_slot.lock().await = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }));

        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_busy = Arc::new(AtomicBool::new(true));
        // None state_handle skips the `running_command_pids` block.
        let state_handle: Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>> =
            Arc::new(Mutex::new(None));

        cancel_agent(&state_handle, &task_slot, &agent_done, &agent_busy)
            .await
            .unwrap();

        assert!(
            !agent_busy.load(Ordering::Relaxed),
            "agent_busy atomic must be reset to false after cancel_agent, \
             otherwise subsequent prompts are enqueued forever"
        );
    }

    #[tokio::test]
    async fn test_cancel_agent_returns_without_waiting_for_agent_done() {
        use std::time::Duration;
        use tokio::task::JoinHandle;

        let task_slot: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        *task_slot.lock().await = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }));

        let agent_done = Arc::new(tokio::sync::Notify::new());
        let agent_busy = Arc::new(AtomicBool::new(true));
        let state_handle: Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>> =
            Arc::new(Mutex::new(None));

        tokio::time::timeout(
            Duration::from_millis(200),
            cancel_agent(&state_handle, &task_slot, &agent_done, &agent_busy),
        )
        .await
        .expect("cancel_agent should not wait for agent_done notification")
        .unwrap();

        assert!(!agent_busy.load(Ordering::Relaxed));
    }
}
