//! Interactive shell implementation.
//!
//! Extracted from `cli/mod.rs` — handles raw mode, terminal rendering,
//! file picker, input queuing, and agent lifecycle.

use crate::cli::{RootOnlyOptions, TaskOptions};
use crate::cli::output::{OutputEvent, OutputWriterArc, ChannelOutputWriter};
use crate::cli::tui::{App, ansi_to_ratatui_lines};
use crate::cli::tui::history::append_to_history;
use crate::core::approval::is_approval_prompt_active;
use futures::FutureExt;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};

/// RAII guard that restores ratatui terminal state on drop.
/// Prevents terminal from being left in alternate screen on early returns or errors.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

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
    async fn print_resume_summary(agent: &crate::core::agent_loop::AgentLoop, writer: &crate::cli::output::OutputWriterArc) {
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
                    agent.output_writer().emit(crate::cli::output::OutputEvent::warning(format!(
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
    _session: &Arc<Mutex<InteractiveSession>>,
    task_id: &str,
    agent_busy: &AtomicBool,
    agent_done: &Arc<tokio::sync::Notify>,
    _agent_start_time: &Arc<Mutex<Option<Instant>>>,
    state_handle: &Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
) -> anyhow::Result<Option<Action>> {
    use crate::core::approval::{is_followup_question_active, take_followup_sender};
    
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
            // Clear input with visual feedback
            app.push_plain("^C");
            app.input = tui_textarea::TextArea::new(Vec::new());
            return Ok(None);
        }
    }
    
    // Enter key - intercept before passing to textarea
    if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
        // Check for followup question (used by /undo, /commit, /checkpoint-restore)
        if is_followup_question_active(task_id) {
            if let Some(sender) = take_followup_sender(task_id) {
                let text = app.input.lines().join("");
                let _ = sender.send(text);
                app.input = tui_textarea::TextArea::new(Vec::new());
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
            // Clear textarea
            app.input = tui_textarea::TextArea::new(Vec::new());
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
    
    // Up/Down for command history navigation
    if key.code == KeyCode::Up && app.input.cursor().0 == 0 && app.input.cursor().1 == 0 {
        if let Some(entry) = app.history.navigate_up() {
            app.input = tui_textarea::TextArea::new(vec![entry.to_string()]);
        }
        return Ok(None);
    }
    if key.code == KeyCode::Down && app.history.is_navigating() {
        if let Some(entry) = app.history.navigate_down() {
            app.input = tui_textarea::TextArea::new(vec![entry.to_string()]);
        } else {
            app.input = tui_textarea::TextArea::new(Vec::new());
        }
        return Ok(None);
    }
    
    // Up/Down for file picker navigation (when picker is active)
    
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
    
    // Tab key with active file picker -> insert selection
    if key.code == KeyCode::Tab && app.picker_active && !app.picker_results.is_empty() {
        let result = &app.picker_results[app.picker_index];
        let text = app.input.lines().join("");
        let mq = crate::core::file_search::extract_mention_query(&text);
        if mq.in_mention_mode {
            let new_text = crate::core::file_search::insert_mention(&text, mq.at_index as usize, &result.path);
            app.input = tui_textarea::TextArea::new(vec![new_text]);
            app.picker_active = false;
            app.picker_results.clear();
        }
        return Ok(None);
    }
    
    // All other keys go to textarea
    use tui_textarea::Input;
    app.input.input(Input::from(key));
    
    // Check for @ mention mode - show file picker overlay
    let input_text = app.input.lines().join("");
    let mq = crate::core::file_search::extract_mention_query(&input_text);
    if mq.in_mention_mode && !app.cwd.is_empty() {
        let query = mq.query.clone();
        let cwd = app.cwd.clone();
        let results = crate::core::file_search::search_workspace_files(
            &query, &cwd, 10
        ).await;
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
    agent_busy: &AtomicBool,
    _agent_start_time: &Arc<Mutex<Option<Instant>>>,
    _agent_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    _state_handle: &Arc<Mutex<Option<Arc<Mutex<crate::core::agent_types::TaskState>>>>>,
    task_opts: &TaskOptions,
    auto_approve: bool,
) -> anyhow::Result<bool> {
    use crate::cli::slash_commands::{CliOnlyCommand, format_help_text, format_settings_text, format_stats_text, format_changes_text, format_help_for_command};
    
    if agent_busy.load(Ordering::Relaxed) {
        let busy_cmds = [CliOnlyCommand::Exit, CliOnlyCommand::Quit, CliOnlyCommand::Clear, CliOnlyCommand::History];
        if !busy_cmds.contains(&cli_cmd) {
            app.push_styled("Agent is busy. Wait for it to finish before running this command.",
                Style::default().fg(Color::Yellow));
            return Ok(false);
        }
    }
    
    match cli_cmd {
        CliOnlyCommand::Exit | CliOnlyCommand::Quit => {
            return Ok(true);
        }
        CliOnlyCommand::Clear => {
            app.output_lines.clear();
            app.push_plain("Conversation cleared.");
        }
        CliOnlyCommand::History => {
            let last_n: Vec<String> = app.history.entries().iter().rev().take(10).cloned().collect();
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
        CliOnlyCommand::Undo | CliOnlyCommand::CheckpointUndo => {
            let sess = session.lock().await;
            let checkpoint_mgr = sess.agent_loop().checkpoint_manager();

            if checkpoint_mgr.is_none() {
                app.push_plain("Checkpoint manager is not initialized.");
                return Ok(false);
            }

            let checkpoint_mgr = checkpoint_mgr.unwrap();
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
                checkpoint_mgr.get_changed_files(&most_recent.hash, Some(current)).await.unwrap_or_else(|_| vec![])
            } else {
                vec![]
            };

            if !changed_files.is_empty() {
                app.push_styled("/undo will revert the following files to the previous checkpoint:",
                    Style::default().fg(Color::Yellow));
                for f in &changed_files {
                    app.push_plain(format!("  - {}", f));
                }
                app.push_plain("Continue? (y to cancel, Enter to confirm): ");

                let (sender, receiver) = std::sync::mpsc::channel();
                crate::core::approval::set_followup_question_active(task_id, true);
                crate::core::approval::set_followup_sender(task_id, sender);

                let response_result = tokio::task::spawn_blocking(move || {
                    receiver.recv_timeout(std::time::Duration::from_secs(30))
                }).await;

                crate::core::approval::clear_followup_sender(task_id);
                crate::core::approval::set_followup_question_active(task_id, false);

                let confirm = match response_result {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) | Err(_) => String::new(),
                };

                if !confirm.trim().is_empty() && confirm.trim().to_lowercase() == "y" {
                    app.push_plain("Undo cancelled.");
                    return Ok(false);
                }
            }

            match checkpoint_mgr.restore_checkpoint(&most_recent.hash).await {
                Ok(()) => {
                    app.push_plain(format!("Restored to checkpoint {} — {} file(s) reverted",
                        most_recent.number, changed_files.len()));
                    if !changed_files.is_empty() {
                        app.push_plain("\nReverted files:");
                        for f in &changed_files {
                            app.push_plain(format!("  - {}", f));
                        }
                    }
                    let removed = sess.agent_loop().remove_last_turn().await;
                    if removed > 0 {
                        app.push_plain(format!("Removed {} message(s) from conversation history.", removed));
                    }
                }
                Err(e) => {
                    app.push_plain(format!("Undo failed: {}", e));
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
                            app.push_plain(format!("Failed to get diff: {}", e));
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
                            app.push_plain(format!("Failed to get log: {}", e));
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
                                    app.push_styled("Changes to commit:", Style::default().fg(Color::Cyan));
                                    for line in ansi_to_ratatui_lines(&diff) {
                                        app.push_output(line);
                                    }
                                    app.push_plain("Commit to your git repo? (y/n): ");

                                    let (sender, receiver) = std::sync::mpsc::channel();
                                    crate::core::approval::set_followup_question_active(task_id, true);
                                    crate::core::approval::set_followup_sender(task_id, sender);

                                    let response_result = tokio::task::spawn_blocking(move || {
                                        receiver.recv_timeout(std::time::Duration::from_secs(30))
                                    }).await;

                                    crate::core::approval::clear_followup_sender(task_id);
                                    crate::core::approval::set_followup_question_active(task_id, false);

                                    let confirm = match response_result {
                                        Ok(Ok(r)) => r,
                                        Ok(Err(_)) | Err(_) => String::new(),
                                    };

                                    if confirm.trim().is_empty() || confirm.trim().to_lowercase() == "y" {
                                        match crate::core::shadow_git::commit_to_real_git(&workspace_root, &msg) {
                                            Ok(files) => {
                                                app.push_plain(format!("Committed {} file(s) to your git repo.", files.len()));
                                            }
                                            Err(e) => {
                                                app.push_plain(format!("Commit failed: {}", e));
                                            }
                                        }
                                    } else {
                                        app.push_plain("Commit cancelled.");
                                    }
                                }
                            }
                            Err(e) => {
                                app.push_plain(format!("Failed to get diff: {}", e));
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
            let checkpoint_mgr = sess.agent_loop().checkpoint_manager();

            if checkpoint_mgr.is_none() {
                app.push_plain("Checkpoint manager is not initialized.");
                return Ok(false);
            }

            let checkpoint_mgr = checkpoint_mgr.unwrap();
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
            let checkpoint_mgr = sess.agent_loop().checkpoint_manager();

            if checkpoint_mgr.is_none() {
                app.push_plain("Checkpoint manager is not initialized.");
                return Ok(false);
            }

            let checkpoint_mgr = checkpoint_mgr.unwrap();
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
                }).await;

                crate::core::approval::clear_followup_sender(task_id);
                crate::core::approval::set_followup_question_active(task_id, false);

                let input = match response_result {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) | Err(_) => String::new(),
                };
                input.trim().parse::<usize>().unwrap_or(0)
            };

            if num == 0 || num > checkpoints.len() {
                app.push_plain(format!("Invalid checkpoint number. Available: 1-{}", checkpoints.len()));
                return Ok(false);
            }

            if let Some(checkpoint) = checkpoints.get(num - 1) {
                let current_hash = checkpoint_mgr.last_checkpoint()
                    .map(|h| h.as_str()).unwrap_or("HEAD");
                match checkpoint_mgr.get_changed_files(&checkpoint.hash, Some(current_hash)).await {
                    Ok(changed_files) => {
                        if !changed_files.is_empty() {
                            app.push_styled("Files that will be restored:", Style::default().fg(Color::Yellow));
                            for file in &changed_files {
                                app.push_plain(format!("  - {}", file));
                            }
                            app.push_plain("Continue? (y to cancel, Enter to confirm): ");

                            let (sender, receiver) = std::sync::mpsc::channel();
                            crate::core::approval::set_followup_question_active(task_id, true);
                            crate::core::approval::set_followup_sender(task_id, sender);

                            let response_result = tokio::task::spawn_blocking(move || {
                                receiver.recv_timeout(std::time::Duration::from_secs(30))
                            }).await;

                            crate::core::approval::clear_followup_sender(task_id);
                            crate::core::approval::set_followup_question_active(task_id, false);

                            let confirm = match response_result {
                                Ok(Ok(r)) => r,
                                Ok(Err(_)) | Err(_) => String::new(),
                            };

                            if !confirm.trim().is_empty() && confirm.trim().to_lowercase() == "y" {
                                app.push_plain("Restore cancelled.");
                                return Ok(false);
                            }
                        }
                    }
                    Err(e) => {
                        app.push_plain(format!("Warning: Could not determine changed files: {}", e));
                    }
                }

                match checkpoint_mgr.restore_by_number(num).await {
                    Ok(()) => {
                        app.push_plain(format!("Checkpoint {} ({}) restored successfully.", num, checkpoint.hash));
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
                if let Some(block) = state.snipped_code_blocks.iter().find(|block| block.index == index) {
                    if block.language.is_empty() {
                        app.push_plain("```");
                    } else {
                        app.push_plain(format!("```{}", block.language));
                    }
                    let highlighted = crate::cli::syntax_highlight::highlight_code(&block.code, &block.language);
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
    }
    Ok(false)
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
    queue_handle: Arc<Mutex<Option<crate::core::agent_loop::MessageQueueHandle>>>,
    task_opts: &TaskOptions,
    auto_approve: bool,
) -> anyhow::Result<()> {
    loop {
        // 1. Drain channel into app
        drain_output(output_rx, app);
        
        // 2. Render
        terminal.draw(|f| app.render(f))?;
        
        // 3. Poll for events (blocking, 50ms timeout)
        // Always poll to provide delay; skip stdin read while approval is active to avoid fd race
        let has_event = ratatui::crossterm::event::poll(Duration::from_millis(50))?;
        if has_event && !is_approval_prompt_active() {
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
                                // Save to command history and reset navigation
                                append_to_history(&text);
                                app.history.reload();
                                
                                // Check for CLI-only slash commands FIRST
                                if let Some(cli_cmd) = crate::cli::slash_commands::get_cli_only_command(&text) {
                                    let should_exit = handle_cli_only_command(
                                        cli_cmd, &text, app, &session, &task_id,
                                        &agent_busy, &agent_start_time, &agent_task,
                                        &state_handle, task_opts, auto_approve,
                                    ).await?;
                                    if should_exit {
                                        return Ok(());
                                    }
                                    continue;
                                }
                                
                                // Process model-side slash commands (e.g., /compact, /plan)
                                let processed = crate::cli::slash_commands::process_slash_command(&text);
                                
                                // If agent is busy, queue the message; otherwise spawn
                                if agent_busy.load(Ordering::Relaxed) {
                                    if let Some(qh) = queue_handle.lock().await.as_ref() {
                                        if !processed.is_empty() {
                                            qh.enqueue_text_message(processed).await;
                                            let count = qh.queued_message_count().await;
                                            app.push_styled(
                                                format!("Message queued ({} in queue)", count),
                                                Style::default().add_modifier(Modifier::DIM),
                                            );
                                        }
                                    }
                                } else {
                                    spawn_agent_task(
                                        &session, &processed,
                                        &agent_busy, &agent_done,
                                        &agent_start_time, &agent_task,
                                    ).await?;
                                    app.agent_busy = true;
                                }
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
                app.agent_busy = false;
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
    crate::core::cancellation::TERMINAL_INITIALIZED.store(true, std::sync::atomic::Ordering::Release);
    let _guard = TerminalGuard;
    let mut app = App::new();
    if let Ok(cwd) = std::env::current_dir() {
        app.cwd = cwd.to_string_lossy().to_string();
    }
    
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
            for line in ansi_to_ratatui_lines(&startup_info) {
                app.push_output(line);
            }
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
        queue_handle,
        &task_opts,
        auto_approve,
    ).await;
    
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
}
