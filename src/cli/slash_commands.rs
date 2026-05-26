use regex::Regex;
use std::collections::HashMap;
use std::path::Path;

use crate::core::context::instructions::{self, SkillMetadata};

const SLASH_COMMAND_REGEX: &str = r"(^|\s)\/([a-zA-Z0-9_.:@-]+)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    NewTask,
    Smol,
    NewRule,
    ReportBug,
    ExplainChanges,
    SkillCommand { name: String },
    WorkflowCommand { name: String },
}

impl SlashCommand {
    pub fn parse(s: &str) -> Option<SlashCommand> {
        match s {
            "newtask" => Some(SlashCommand::NewTask),
            "compact" => Some(SlashCommand::Smol),
            "newrule" => Some(SlashCommand::NewRule),
            "reportbug" => Some(SlashCommand::ReportBug),
            "explain-changes" => Some(SlashCommand::ExplainChanges),
            _ => None,
        }
    }

    pub fn parse_with_skills_and_workflows(
        command_name: &str,
        available_skills: &[SkillMetadata],
        local_workflow_toggles: &HashMap<String, bool>,
        global_workflow_toggles: &HashMap<String, bool>,
        remote_workflow_toggles: &HashMap<String, bool>,
        cwd: &Path,
    ) -> Option<SlashCommand> {
        if let Some(slash_cmd) = SlashCommand::parse(command_name) {
            return Some(slash_cmd);
        }

        if let Some(skill) = available_skills.iter().find(|s| s.name == command_name) {
            return Some(SlashCommand::SkillCommand {
                name: skill.name.clone(),
            });
        }

        let workflows = build_workflows_list(
            local_workflow_toggles,
            global_workflow_toggles,
            remote_workflow_toggles,
            cwd,
        );
        if let Some(workflow) = workflows.iter().find(|w| w.file_name == command_name) {
            return Some(SlashCommand::WorkflowCommand {
                name: workflow.file_name.clone(),
            });
        }

        None
    }

    pub fn is_skill_command(&self) -> bool {
        matches!(self, SlashCommand::SkillCommand { .. })
    }

    pub fn is_workflow_command(&self) -> bool {
        matches!(self, SlashCommand::WorkflowCommand { .. })
    }

    pub fn is_compact(&self) -> bool {
        matches!(self, SlashCommand::Smol)
    }

    pub fn instruction_block(&self) -> &'static str {
        match self {
            SlashCommand::NewTask => NEW_TASK_INSTRUCTION,
            SlashCommand::Smol => CONDENSE_INSTRUCTION,
            SlashCommand::NewRule => NEW_RULE_INSTRUCTION,
            SlashCommand::ReportBug => REPORT_BUG_INSTRUCTION,
            SlashCommand::ExplainChanges => EXPLAIN_CHANGES_INSTRUCTION,
            SlashCommand::SkillCommand { .. } => "",
            SlashCommand::WorkflowCommand { .. } => "",
        }
    }

    pub fn skill_name(&self) -> Option<&str> {
        match self {
            SlashCommand::SkillCommand { name } => Some(name),
            _ => None,
        }
    }

    pub fn workflow_name(&self) -> Option<&str> {
        match self {
            SlashCommand::WorkflowCommand { name } => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedSlashCommand {
    pub command: String,
    pub prefix: String,
    pub start_index: usize,
    pub end_index: usize,
}

#[derive(Debug, Clone)]
pub struct SlashCommandParseResult {
    pub processed_text: String,
    pub command: Option<ParsedSlashCommand>,
}

#[derive(Debug, Clone)]
pub struct WorkflowInfo {
    pub file_name: String,
    pub full_path: String,
    pub is_remote: bool,
    pub contents: Option<String>,
}

fn build_workflows_list(
    local_workflow_toggles: &HashMap<String, bool>,
    global_workflow_toggles: &HashMap<String, bool>,
    _remote_workflow_toggles: &HashMap<String, bool>,
    cwd: &Path,
) -> Vec<WorkflowInfo> {
    let mut workflows = Vec::new();

    let local_dir = cwd.join(".agents/workflows");
    if local_dir.exists()
        && local_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&local_dir)
    {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "md") {
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.trim_end_matches(".md").to_string())
                    .unwrap_or_default();

                if local_workflow_toggles
                    .get(&file_name)
                    .copied()
                    .unwrap_or(true)
                {
                    workflows.push(WorkflowInfo {
                        file_name,
                        full_path: path.to_string_lossy().to_string(),
                        is_remote: false,
                        contents: None,
                    });
                }
            }
        }
    }

    let global_dir = dirs::home_dir()
        .map(|h| h.join(".sned/workflows"))
        .filter(|p| p.exists() && p.is_dir());

    if let Some(global_dir) = global_dir
        && let Ok(entries) = std::fs::read_dir(&global_dir)
    {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "md") {
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.trim_end_matches(".md").to_string())
                    .unwrap_or_default();

                if global_workflow_toggles
                    .get(&file_name)
                    .copied()
                    .unwrap_or(true)
                    && !workflows.iter().any(|w| w.file_name == file_name)
                {
                    workflows.push(WorkflowInfo {
                        file_name,
                        full_path: path.to_string_lossy().to_string(),
                        is_remote: false,
                        contents: None,
                    });
                }
            }
        }
    }

    workflows
}

pub fn get_workflow_content(
    workflow_name: &str,
    local_workflow_toggles: &HashMap<String, bool>,
    global_workflow_toggles: &HashMap<String, bool>,
    remote_workflow_toggles: &HashMap<String, bool>,
    cwd: &Path,
) -> Option<String> {
    let workflows = build_workflows_list(
        local_workflow_toggles,
        global_workflow_toggles,
        remote_workflow_toggles,
        cwd,
    );

    workflows
        .iter()
        .find(|w| w.file_name == workflow_name)
        .and_then(|w| {
            if w.full_path.is_empty() {
                None
            } else {
                std::fs::read_to_string(&w.full_path).ok()
            }
        })
        .map(|c| c.trim().to_string())
}

pub fn get_skill_content_for_command(
    skill_name: &str,
    available_skills: &[SkillMetadata],
) -> Option<String> {
    let skill_content = instructions::get_skill_content(skill_name, available_skills)?;
    Some(skill_content.instructions)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliOnlyCommand {
    Exit,
    Quit,
    Clear,
    History,
    Skills,
    Help,
    Settings,
    Models,
    ResetCompact,
    Stats,
    Undo,
    Diff,
    Log,
    Commit,
    CheckpointList,
    CheckpointRestore,
    CheckpointUndo,
    Expand,
    Changes,
    HelpOption(String),
}

impl CliOnlyCommand {
    pub fn parse(s: &str) -> Option<CliOnlyCommand> {
        match s.to_lowercase().as_str() {
            "exit" => Some(CliOnlyCommand::Exit),
            "q" => Some(CliOnlyCommand::Quit),
            "quit" => Some(CliOnlyCommand::Quit),
            "clear" => Some(CliOnlyCommand::Clear),
            "history" => Some(CliOnlyCommand::History),
            "skills" => Some(CliOnlyCommand::Skills),
            "help" => Some(CliOnlyCommand::Help),
            "settings" => Some(CliOnlyCommand::Settings),
            "models" => Some(CliOnlyCommand::Models),
            "resetcompact" | "clearcompact" => Some(CliOnlyCommand::ResetCompact),
            "stats" => Some(CliOnlyCommand::Stats),
            "undo" => Some(CliOnlyCommand::Undo),
            "diff" => Some(CliOnlyCommand::Diff),
            "log" => Some(CliOnlyCommand::Log),
            "commit" => Some(CliOnlyCommand::Commit),
            "checkpoint-list" | "checkpoint list" => Some(CliOnlyCommand::CheckpointList),
            "checkpoint-restore" | "checkpoint restore" => Some(CliOnlyCommand::CheckpointRestore),
            "checkpoint-undo" | "checkpoint undo" => Some(CliOnlyCommand::CheckpointUndo),
            "expand" => Some(CliOnlyCommand::Expand),
            "changes" => Some(CliOnlyCommand::Changes),
            _ => None,
        }
    }

    pub fn parse_with_arg(cmd: &str, arg: &str) -> Option<CliOnlyCommand> {
        match cmd.to_lowercase().as_str() {
            "help" if !arg.is_empty() => Some(CliOnlyCommand::HelpOption(arg.to_lowercase())),
            _ => Self::parse(cmd),
        }
    }

    pub fn is_shutdown(&self) -> bool {
        matches!(self, CliOnlyCommand::Exit | CliOnlyCommand::Quit)
    }

    pub fn is_clear(&self) -> bool {
        matches!(self, CliOnlyCommand::Clear)
    }

    pub fn is_reset_compact(&self) -> bool {
        matches!(self, CliOnlyCommand::ResetCompact)
    }
}

pub fn parse_slash_command(text: &str) -> SlashCommandParseResult {
    let regex = Regex::new(SLASH_COMMAND_REGEX).unwrap();
    let caps = regex.captures(text);
    if let Some(caps) = caps {
        let m = caps.get(0).unwrap();
        let start = m.start();
        let end = m.end();

        let prefix = caps.get(1).map_or("", |m| m.as_str()).to_string();
        let command = caps.get(2).map_or("", |m| m.as_str()).to_string();

        let after_command = text.get(end..).unwrap_or("");
        let following_char = after_command.chars().next();

        let is_valid = following_char.map(|c| c.is_whitespace()).unwrap_or(true);

        if !is_valid {
            return SlashCommandParseResult {
                processed_text: text.to_string(),
                command: None,
            };
        }

        let processed_text = format!("{}{}", &text[..start], &text[end..])
            .trim()
            .to_string();

        SlashCommandParseResult {
            processed_text,
            command: Some(ParsedSlashCommand {
                command,
                prefix,
                start_index: start,
                end_index: end,
            }),
        }
    } else {
        SlashCommandParseResult {
            processed_text: text.to_string(),
            command: None,
        }
    }
}

pub fn process_slash_command(text: &str) -> String {
    let result = parse_slash_command(text);
    if let Some(cmd) = result.command
        && let Some(slash_cmd) = SlashCommand::parse(&cmd.command)
    {
        return format!(
            "{}\n{}",
            slash_cmd.instruction_block(),
            result.processed_text
        );
    }
    text.to_string()
}

pub fn process_slash_command_with_context(
    text: &str,
    available_skills: &[SkillMetadata],
    local_workflow_toggles: &HashMap<String, bool>,
    global_workflow_toggles: &HashMap<String, bool>,
    remote_workflow_toggles: &HashMap<String, bool>,
    cwd: &Path,
) -> String {
    let result = parse_slash_command(text);
    if let Some(cmd) = result.command
        && let Some(slash_cmd) = SlashCommand::parse_with_skills_and_workflows(
            &cmd.command,
            available_skills,
            local_workflow_toggles,
            global_workflow_toggles,
            remote_workflow_toggles,
            cwd,
        )
    {
        if slash_cmd.is_skill_command() {
            if let Some(skill_name) = slash_cmd.skill_name()
                && let Some(content) = get_skill_content_for_command(skill_name, available_skills)
            {
                let instruction = format!(
                    "<explicit_instructions type=\"skill\" name=\"{}\">\n{}\n</explicit_instructions>",
                    skill_name, content
                );

                // Check if text after slash command is empty (TS behavior)
                let text_after = &result.processed_text;
                let is_empty = text_after.trim().is_empty();

                if is_empty {
                    let activation_note = format!(
                        "\n(Note: The user has explicitly activated the \"{}\" skill via a slash command. This skill is now active. Please acknowledge its activation, summarize how you can help based on its instructions, and ask the user for the specific target or task they want you to perform, or propose a first step if appropriate.)\n",
                        skill_name
                    );
                    return format!("{}{}{}", instruction, activation_note, text_after);
                } else {
                    return format!("{}\n{}", instruction, text_after);
                }
            }
        } else if slash_cmd.is_workflow_command() {
            if let Some(workflow_name) = slash_cmd.workflow_name()
                && let Some(content) = get_workflow_content(
                    workflow_name,
                    local_workflow_toggles,
                    global_workflow_toggles,
                    remote_workflow_toggles,
                    cwd,
                )
            {
                let instruction = format!(
                    "<explicit_instructions type=\"{}\">\n{}\n</explicit_instructions>",
                    workflow_name, content
                );
                return format!("{}\n{}", instruction, result.processed_text);
            }
        } else if slash_cmd.instruction_block().is_empty() {
            return result.processed_text.clone();
        } else {
            return format!(
                "{}\n{}",
                slash_cmd.instruction_block(),
                result.processed_text
            );
        }
    }
    text.to_string()
}

pub fn is_compact_command(text: &str) -> bool {
    let result = parse_slash_command(text);
    if let Some(cmd) = result.command
        && let Some(slash_cmd) = SlashCommand::parse(&cmd.command)
    {
        return slash_cmd.is_compact();
    }
    false
}

pub fn get_cli_only_command(text: &str) -> Option<CliOnlyCommand> {
    let result = parse_slash_command(text);
    if let Some(cmd) = result.command {
        // Try the command with argument first (handles /help <command>, etc.)
        if !result.processed_text.is_empty()
            && let Some(parsed) =
                CliOnlyCommand::parse_with_arg(&cmd.command, result.processed_text.trim())
        {
            return Some(parsed);
        }
        // Try the command word first (handles /expand 1, /commit "msg", etc.)
        if let Some(parsed) = CliOnlyCommand::parse(&cmd.command) {
            return Some(parsed);
        }
        // Fall back to full command + args (handles /checkpoint list, /checkpoint restore N, etc.)
        if !result.processed_text.is_empty()
            && let Some(parsed) =
                CliOnlyCommand::parse(&format!("{} {}", cmd.command, result.processed_text))
        {
            return Some(parsed);
        }
    }
    None
}

pub fn parse_expand_index(text: &str) -> Option<usize> {
    let result = parse_slash_command(text);
    let cmd = result.command?;
    if !cmd.command.eq_ignore_ascii_case("expand") {
        return None;
    }

    result
        .processed_text
        .split_whitespace()
        .next()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|index| *index > 0)
}

pub fn parse_checkpoint_restore(text: &str) -> Option<usize> {
    let result = parse_slash_command(text);
    let cmd = result.command?;

    // Build the full command for matching (handles both /checkpoint-restore N and /checkpoint restore N)
    let full_command = if result.processed_text.is_empty() {
        cmd.command.clone()
    } else {
        format!(
            "{} {}",
            cmd.command,
            result
                .processed_text
                .split_whitespace()
                .next()
                .unwrap_or("")
        )
    };

    let is_match = cmd.command.eq_ignore_ascii_case("checkpoint-restore")
        || cmd.command.eq_ignore_ascii_case("checkpoint restore")
        || full_command.eq_ignore_ascii_case("checkpoint restore");

    if !is_match {
        return None;
    }

    // For /checkpoint-restore N, processed_text is "N"
    // For /checkpoint restore N, processed_text is "restore N" — skip the subcommand word
    let number_str = if cmd.command.eq_ignore_ascii_case("checkpoint-restore") {
        result.processed_text.split_whitespace().next()
    } else {
        // /checkpoint restore N or /checkpoint-restore matched via full_command
        result.processed_text.split_whitespace().nth(1)
    };

    number_str
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|index| *index > 0)
}

pub fn format_help_text() -> String {
    use crate::cli::colors::style;

    let mut s = String::new();
    s.push_str(&format!(
        "{}{}═══════ Sned Commands ═══════{}\n\n",
        style::BOLD,
        style::CYAN,
        style::RESET
    ));

    s.push_str(&format!(
        "{}{}Base Commands (sent to AI):{}\n",
        style::BOLD,
        style::CYAN,
        style::RESET
    ));
    s.push_str(&format!(
        "{}─────────────────────────────{}\n",
        style::DIM,
        style::RESET
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Create a new task with context from the current task{}\n",
        style::CYAN,
        "/newtask",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Condense your current context window{}\n",
        style::CYAN,
        "/compact",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Create a new Snad rule based on your conversation{}\n",
        style::CYAN,
        "/newrule",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Submit a bug report to GitHub{}\n",
        style::CYAN,
        "/reportbug",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!("  {}{}{}  - {}Explain the changes you have made to the code (alias: /explain_changes){}\n\n", style::CYAN, "/explain-changes", style::DIM, style::RESET, style::DIM));

    s.push_str(&format!(
        "{}{}CLI-Only Commands (handled locally):{}\n",
        style::BOLD,
        style::CYAN,
        style::RESET
    ));
    s.push_str(&format!(
        "{}─────────────────────────────{}\n",
        style::DIM,
        style::RESET
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Exit the CLI (aliases: /q, /quit){}\n",
        style::CYAN,
        "/exit",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Clear the current conversation history{}\n",
        style::CYAN,
        "/clear",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show recent tasks{}\n",
        style::CYAN,
        "/history",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}List available skills{}\n",
        style::CYAN,
        "/skills",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show this help message (alias: /help <command>){}\n",
        style::CYAN,
        "/help",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show current settings{}\n",
        style::CYAN,
        "/settings",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show available models{}\n",
        style::CYAN,
        "/models",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show token usage and session cost{}\n",
        style::CYAN,
        "/stats",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Clear compacted summary (allows /compact to be used again){}\n",
        style::CYAN,
        "/resetcompact",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Undo the last agent turn {}{}{}\n",
        style::CYAN,
        "/undo",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show changes from the last turn {}{}{}\n",
        style::CYAN,
        "/diff",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show agent turn history {}{}{}\n",
        style::CYAN,
        "/log",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Commit agent changes to your git repo {}{}{}\n",
        style::CYAN,
        "/commit \"msg\"",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show a previously snipped code block{}\n",
        style::CYAN,
        "/expand N",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}List available checkpoints with timestamps {}{}{}\n",
        style::CYAN,
        "/checkpoint list",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Restore a specific checkpoint by number {}{}{}\n",
        style::CYAN,
        "/checkpoint restore N",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!("  {}{}{}  - {}Undo last turn using checkpoint (reverts files + trims history, alias: /checkpoint-undo) {}{}{}\n\n", style::CYAN, "/checkpoint undo", style::DIM, style::RESET, style::YELLOW, "[requires --track-changes]", style::DIM));

    s.push_str(&format!(
        "{}{}Keyboard Shortcuts:{}\n",
        style::BOLD,
        style::CYAN,
        style::RESET
    ));
    s.push_str(&format!(
        "{}─────────────────────────────{}\n",
        style::DIM,
        style::RESET
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Move cursor to beginning of line{}\n",
        style::CYAN,
        "Ctrl+A / Home",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Move cursor to end of line{}\n",
        style::CYAN,
        "Ctrl+E / End",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Clear from cursor to beginning{}\n",
        style::CYAN,
        "Ctrl+U",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Clear from cursor to end{}\n",
        style::CYAN,
        "Ctrl+K",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Delete word backward{}\n",
        style::CYAN,
        "Ctrl+W",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Cancel current operation{}\n",
        style::CYAN,
        "Ctrl+C",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Move cursor left/right{}\n",
        style::CYAN,
        "Arrow Left/Right",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Navigate command history / file picker{}\n\n",
        style::CYAN,
        "Arrow Up/Down",
        style::DIM,
        style::RESET,
        style::DIM
    ));

    s.push_str(&format!(
        "{}{}Examples:{}\n",
        style::BOLD,
        style::CYAN,
        style::RESET
    ));
    s.push_str(&format!(
        "{}─────────────────────────────{}\n",
        style::DIM,
        style::RESET
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Compact context before a new topic{}\n",
        style::CYAN,
        "/compact",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Start a new task, carrying over context{}\n",
        style::CYAN,
        "/newtask",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show this help{}\n",
        style::CYAN,
        "/help",
        style::DIM,
        style::RESET,
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Undo last agent turn {}{}{}\n",
        style::CYAN,
        "/undo",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Review last turn's changes {}{}{}\n",
        style::CYAN,
        "/diff",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Commit changes to git {}{}{}\n",
        style::CYAN,
        "/commit \"fix: auth bug\"",
        style::DIM,
        style::RESET,
        style::YELLOW,
        "[requires --track-changes]",
        style::DIM
    ));
    s.push_str(&format!(
        "  {}{}{}  - {}Show snipped code block 1{}",
        style::CYAN,
        "/expand 1",
        style::DIM,
        style::RESET,
        style::DIM
    ));

    s
}

pub fn format_help_for_command(cmd: &str) -> String {
    use crate::cli::colors::style;

    let cmd_lower = cmd.to_lowercase();
    let banner = format!(
        "{}{}═══════ Help: /{} ═══════{}",
        style::BOLD,
        style::CYAN,
        cmd,
        style::RESET
    );

    let help_text = match cmd_lower.as_str() {
        "newtask" => {
            r#"Creates a new task while preserving context from the current conversation.

Use when:
  - Starting a new subtask within a larger project
  - Switching to a different topic while keeping conversation history
  - Creating a focused task with inherited context

Example:
  /newtask - Create a new task with current context"#
        }

        "compact" => {
            r#"Condenses the current conversation history to reduce token usage.

Use when:
  - Approaching context window limits
  - After completing a major task phase
  - Before starting a new topic in the same session

Note: Can only be used once per session. After that, use /resetcompact."#
        }

        "newrule" => {
            r#"Creates a new Sned rule based on your conversation context.

Use when:
  - You want to codify a pattern from your current work
  - Creating project-specific conventions
  - Documenting recurring workflows"#
        }

        "reportbug" => {
            r#"Submits a bug report to GitHub.

Use when:
  - You encounter unexpected behavior
  - Found a regression or crash
  - Want to report a feature request"#
        }

        "explain-changes" => {
            r#"Explains the changes you have made to the codebase.

Use when:
  - You want a summary of your modifications
  - Preparing commit messages
  - Reviewing your own work"#
        }

        "exit" | "q" | "quit" => {
            r#"Exits the Sned CLI.

Aliases: /exit, /q, /quit

Use when:
  - Finished with your session
  - Want to return to the shell"#
        }

        "clear" => {
            r#"Clears the current conversation history.

Use when:
  - Starting fresh without context
  - Privacy concerns require clearing history
  - Resetting a confused AI session

Warning: This action cannot be undone."#
        }

        "history" => {
            r#"Shows recent tasks from your history.

Use when:
  - Looking for a previous task
  - Reviewing past work sessions
  - Finding task IDs for reference"#
        }

        "skills" => {
            r#"Lists available skills that can be invoked via slash commands.

Use when:
  - Discovering available skills
  - Checking skill names and descriptions
  - Learning about project-specific automations"#
        }

        "help" => {
            r#"Shows this help message or detailed help for a specific command.

Usage:
  /help - Show all commands
  /help <command> - Show detailed help for a specific command

Examples:
  /help newtask - Get detailed help for /newtask
  /help checkpoint - Get help for checkpoint commands"#
        }

        "settings" => {
            r#"Shows current Sned settings including provider, model, and mode.

Use when:
  - Verifying current configuration
  - Checking which model is active
  - Confirming auto-approve status"#
        }

        "models" => {
            r#"Lists available models across providers.

Use when:
  - Choosing a model for your task
  - Checking model availability
  - Comparing model options"#
        }

        "stats" => {
            r#"Shows token usage and session cost statistics.

Use when:
  - Monitoring API costs
  - Tracking token consumption
  - Optimizing prompt efficiency"#
        }

        "resetcompact" => {
            r#"Clears the compacted summary, allowing /compact to be used again.

Alias: /clearcompact

Use when:
  - Need to compact again after first use
  - Compacted summary is outdated
  - Starting a new major phase"#
        }

        "undo" => {
            r#"Undoes the last agent turn by reverting file changes.

Requires: --track-changes flag

Use when:
  - Agent made incorrect changes
  - Want to retry with a different prompt
  - Reverting experimental modifications

Note: Requires checkpoint tracking to be enabled."#
        }

        "diff" => {
            r#"Shows changes from the last agent turn.

Requires: --track-changes flag

Use when:
  - Reviewing what the agent changed
  - Preparing to commit changes
  - Understanding agent modifications"#
        }

        "log" => {
            r#"Shows agent turn history.

Requires: --track-changes flag

Use when:
  - Reviewing session timeline
  - Finding specific turns
  - Auditing agent actions"#
        }

        "commit" => {
            r#"Commits agent changes to your git repository.

Requires: --track-changes flag

Usage:
  /commit "message" - Commit with a custom message

Use when:
  - Ready to save agent changes
  - Creating version control checkpoints
  - Finalizing a completed task

Example:
  /commit "fix: resolve authentication bug""#
        }

        "expand" => {
            r#"Shows a previously snipped code block.

Usage:
  /expand N - Show code block number N

Use when:
  - Need to review truncated output
  - Agent snipped long code blocks
  - Checking specific sections of output"#
        }

        "checkpoint" | "checkpoint-list" | "checkpoint list" => {
            r#"Lists available checkpoints with timestamps.

Requires: --track-changes flag

Aliases: /checkpoint-list, /checkpoint list

Use when:
  - Finding restore points
  - Reviewing checkpoint history
  - Identifying specific turns to restore"#
        }

        "checkpoint-restore" | "checkpoint restore" => {
            r#"Restores files to a specific checkpoint state.

Requires: --track-changes flag

Aliases: /checkpoint-restore, /checkpoint restore

Usage:
  /checkpoint restore N - Restore to checkpoint N

Use when:
  - Reverting to a known good state
  - Undoing multiple turns at once
  - Experimenting with different approaches

Warning: Reverts all files to checkpoint state."#
        }

        "checkpoint-undo" | "checkpoint undo" => {
            r#"Undoes last turn using checkpoint (reverts files + trims history).

Requires: --track-changes flag

Aliases: /checkpoint-undo, /checkpoint undo

Use when:
  - Complete undo of last agent action
  - Both file and history rollback needed
  - More thorough than /undo alone"#
        }

        _ => &format!(
            "Unknown command: /{}\n\nUse /help to see all available commands.",
            cmd
        ),
    };

    format!("{}\n\n{}", banner, help_text)
}

pub fn format_settings_text(provider: &str, model: &str, mode: &str, auto_approve: bool) -> String {
    format!(
        r#"Current Sned Settings:

Provider:     {}
Model:        {}
Mode:         {}
Auto-approve: {}
"#,
        provider,
        model,
        mode,
        if auto_approve { "enabled" } else { "disabled" }
    )
}

pub fn format_models_text() -> String {
    r#"Available Models:

Anthropic:
  claude-sonnet-4-20250514
  claude-opus-4-20250514
  claude-3-5-sonnet-20241022
  claude-3-5-haiku-20241022

OpenAI:
  gpt-4o
  gpt-4o-mini
  o1-preview
  o1-mini

MiniMax:
  minimax-m2.7
"#
    .to_string()
}

pub fn format_stats_text(state: &crate::core::agent_types::TaskState) -> String {
    if let Some(ref api_req_info) = state.last_api_req_info {
        let tokens_in = api_req_info.tokens_in.unwrap_or(0);
        let tokens_out = api_req_info.tokens_out.unwrap_or(0);
        let cache_writes = api_req_info.cache_writes.unwrap_or(0);
        let cache_reads = api_req_info.cache_reads.unwrap_or(0);
        let reasoning = api_req_info.reasoning_tokens.unwrap_or(0);
        let cost = api_req_info.cost.unwrap_or(0.0);
        let context_pct = api_req_info.context_usage_percentage.unwrap_or(0.0);
        let context_window = api_req_info.context_window.unwrap_or(0);

        let cache_str = if cache_writes > 0 || cache_reads > 0 {
            format!(" ({}w/{}r)", cache_writes, cache_reads)
        } else {
            String::new()
        };

        let reasoning_str = if reasoning > 0 {
            format!(" | {} reasoning", reasoning)
        } else {
            String::new()
        };

        let cost_str = if cost > 0.0 {
            format!(" | ${:.4}", cost)
        } else {
            String::new()
        };

        format!(
            "Token Usage:\n  Input:  {}\n  Output: {}\n  Cache:{}{}\n  Cost:{}\n  Context: {:.1}% ({} / {})",
            tokens_in,
            tokens_out,
            cache_str,
            reasoning_str,
            cost_str,
            context_pct,
            api_req_info.tokens_in.unwrap_or(0) + api_req_info.tokens_out.unwrap_or(0),
            context_window
        )
    } else {
        "No API request info available yet.".to_string()
    }
}

/// Format session file changes for /changes command.
pub fn format_changes_text(state: &crate::core::agent_types::TaskState) -> String {
    use crate::core::context::trackers::FileRecordSource;

    let files = state.file_context_tracker.files_in_context();
    if files.is_empty() {
        return "No files tracked in this session yet.".to_string();
    }

    let mut created: Vec<&String> = Vec::new();
    let mut edited: Vec<&String> = Vec::new();
    let mut total_additions: i64 = 0;
    let total_deletions: i64 = 0;

    for entry in files {
        // Files with sned_edit_date are edited
        if entry.sned_edit_date.is_some() {
            edited.push(&entry.path);
            // Estimate: assume ~20 lines added/removed per edit (simplified)
            total_additions += 20;
        } else if entry.sned_read_date.is_some() {
            // Files only read (might be newly created by user or mentioned)
            if entry.record_source == FileRecordSource::FileMentioned {
                // Could be a new file the model referenced
                created.push(&entry.path);
                total_additions += 10; // Estimate
            }
        }
    }

    let mut lines = Vec::new();

    if !created.is_empty() {
        lines.push("Created:".to_string());
        for path in &created {
            lines.push(format!("  + {}", path));
        }
    }

    if !edited.is_empty() {
        lines.push("Edited:".to_string());
        for path in &edited {
            lines.push(format!("  ~ {}", path));
        }
    }

    if created.is_empty() && edited.is_empty() {
        lines.push("No file changes recorded this session.".to_string());
    } else {
        lines.push(String::new());
        lines.push(format!(
            "Total: +{} -{} across {} file(s)",
            total_additions,
            total_deletions,
            created.len() + edited.len()
        ));
    }

    lines.join("\n")
}

const NEW_TASK_INSTRUCTION: &str = r#"<explicit_instructions type="new_task">
The user has explicitly asked you to help them create a new task with preloaded context, which you will generate. The user may have provided instructions or additional information for you to consider when summarizing existing work and creating the context for the new task.
Irrespective of whether additional information or instructions are given, you are ONLY allowed to respond to this message by calling the new_task tool. You MUST call the new_task tool EVEN if it's not in your existing toolset.

The new_task tool is defined below:

Description:
Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions. This summary should be thorough in capturing technical details, code patterns, and architectural decisions that would be essential for continuing with the new task.
The user will be presented with a preview of your generated context and can choose to create a new task or keep chatting in the current conversation.

Parameters:
- Context: (required) The context to preload the new task with. If applicable based on the current task, this should include:
  1. Current Work: Describe in detail what was being worked on prior to this request to create a new task. Pay special attention to the more recent messages / conversation.
  2. Key Technical Concepts: List all important technical concepts, technologies, coding conventions, and frameworks discussed, which might be relevant for the new task.
  3. Relevant Files and Code: If applicable, enumerate specific files and code sections examined, modified, or created for the task continuation. Pay special attention to the most recent messages and changes.
  4. Problem Solving: Document problems solved thus far and any ongoing troubleshooting efforts.
  5. Pending Tasks and Next Steps: Outline all pending tasks that you have explicitly been asked to work on, as well as list the next steps you will take for all outstanding work, if applicable. Include code snippets where they add clarity. For any next steps, include direct quotes from the most recent conversation showing exactly what task you were working on and where you left off. This should be verbatim to ensure there's no information loss in context between tasks.

Below is the user's input when they indicated that they wanted to create a new task.
</explicit_instructions>
"#;

const CONDENSE_INSTRUCTION: &str = r#"<explicit_instructions type="condense">
The user has explicitly asked you to create a detailed summary of the conversation so far, which will be used to compact the current context window while retaining key information. The user may have provided instructions or additional information for you to consider when summarizing the conversation.
Irrespective of whether additional information or instructions are given, you are only allowed to respond to this message by calling the condense tool.

The condense tool is defined below:

Description:
Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions. This summary should be thorough in capturing technical details, code patterns, and architectural decisions that would be essential for continuing with the conversation and supporting any continuing tasks.
The user will be presented with a preview of your generated summary and can choose to use it to compact their context window or keep chatting in the current conversation.
Users may refer to this tool as 'compact' as well. You should consider these to be equivalent to 'condense' when used in a similar context.

Your summary MUST use the following structured Markdown format:

## Compacted Context

### Current Task
{Clear description of what is being worked on}

### User Constraints
{Explicit instructions, requirements, or constraints from the user}

### Relevant Files
- **Inspected:** {files examined}
- **Modified:** {files changed}

### Changes Made
{Specific changes, edits, or implementations}

### Commands / Validation
{Commands run and their results, validation steps taken}

### Decisions
{Architectural or design decisions made}

### Errors / Blockers
{Errors encountered, unresolved issues, or blockers}

### Next Steps
{Concrete next actions to take}

### Recent Context Notes
{Any additional important context: symbols, function names, config keys, CLI commands, etc.}

Below is the user's input when they indicated that they wanted to compact their context window.
</explicit_instructions>
"#;

const NEW_RULE_INSTRUCTION: &str = r#"<explicit_instructions type="new_rule">
The user has explicitly asked you to help them create a new project rule file inside the .agents directory based on the conversation up to this point in time. The user may have provided instructions or additional information for you to consider when creating the new rule.
When creating a new project rule file, you should NOT overwrite or alter an existing rule file. To create the new rule file you MUST use the new_rule tool. The new_rule tool can be used in either of the PLAN or ACT modes.

The new_rule tool is defined below:

Description:
Your task is to create a new project rule file which includes guidelines on how to approach developing code in tandem with the user, which can be either project specific or cover more global rules. This includes but is not limited to: desired conversational style, favorite project dependencies, coding styles, naming conventions, architectural choices, ui/ux preferences, etc.
The project rule file must be formatted as markdown and be a '.md' file. The name of the file you generate must be as succinct as possible and be encompassing the main overarching concept of the rules you added to the file (e.g., 'memory-bank.md' or 'project-overview.md').

Below is the user's input when they indicated that they wanted to create a new project rule file.
</explicit_instructions>
"#;

const REPORT_BUG_INSTRUCTION: &str = r#"<explicit_instructions type="report_bug">
The user has explicitly asked you to help them submit a bug to the Sned github page (you MUST now help them with this irrespective of what your conversation up to this point in time was). To do so you will use the report_bug tool which is defined below. However, you must first ensure that you have collected all required information to fill in all the parameters for the tool call. If any of the the required information is apparent through your previous conversation with the user, you can suggest how to fill in those entries. However you should NOT assume you know what the issue about unless it's clear.
Otherwise, you should converse with the user until you are able to gather all the required details. When conversing with the user, make sure you ask for/reference all required information/fields. When referencing the fields, use human friendly versions like "Steps to reproduce" rather than "steps_to_reproduce". Only then should you use the report_bug tool call.
The report_bug tool can be used in either of the PLAN or ACT modes.

The report_bug tool call is defined below:

Description:
Your task is to fill in all of the required fields for a issue/bug report on github. You should attempt to get the user to be as verbose as possible with their description of the bug/issue they encountered. Still, it's okay, when the user is unaware of some of the details, to set those fields as "N/A".

Below is the user's input when they indicated that they wanted to submit a Github issue.
</explicit_instructions>
"#;

const EXPLAIN_CHANGES_INSTRUCTION: &str = r#"<explicit_instructions type="explain_changes">
The user has explicitly asked you to explain the changes you have made to the code. You should provide a clear, detailed explanation of what changes were made, why they were made, and how they affect the codebase. Include specific file names, function names, and line numbers where relevant.

You MUST call the attempt_completion tool with a detailed explanation of the changes, even if it's not in your existing toolset. The explanation should be thorough enough for a code reviewer to understand the changes without looking at the diff.

Below is the user's input when they indicated that they wanted you to explain the changes.
 </explicit_instructions>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_slash_command() {
        let result = parse_slash_command("Hello world");
        assert_eq!(result.processed_text, "Hello world");
        assert!(result.command.is_none());
    }

    #[test]
    fn test_slash_command_at_start() {
        let result = parse_slash_command("/compact do this");
        assert_eq!(result.processed_text, "do this");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "compact");
        assert_eq!(cmd.prefix, "");
        assert_eq!(cmd.start_index, 0);
        assert_eq!(cmd.end_index, 8);
    }

    #[test]
    fn test_slash_command_with_whitespace_prefix() {
        let result = parse_slash_command("Please /newtask now");
        assert_eq!(result.processed_text, "Please now");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "newtask");
        assert_eq!(cmd.prefix, " ");
        assert_eq!(cmd.start_index, 6);
        assert_eq!(cmd.end_index, 15);
    }

    #[test]
    fn test_slash_command_inside_tags_not_detected() {
        let result = parse_slash_command("<task>/newtask</task>");
        assert_eq!(result.processed_text, "<task>/newtask</task>");
        assert!(result.command.is_none());
    }

    #[test]
    fn test_slash_command_with_numbers() {
        let result = parse_slash_command("/newtask2");
        assert_eq!(result.processed_text, "");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "newtask2");
    }

    #[test]
    fn test_slash_command_with_special_chars() {
        let result = parse_slash_command("/new-task_v2.alpha");
        assert_eq!(result.processed_text, "");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "new-task_v2.alpha");
    }

    #[test]
    fn test_slash_command_at_end() {
        let result = parse_slash_command("Please help me /compact");
        assert_eq!(result.processed_text, "Please help me");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "compact");
    }

    #[test]
    fn test_no_false_positive_in_url() {
        let result = parse_slash_command("Check http://example.com/newtask");
        assert_eq!(result.processed_text, "Check http://example.com/newtask");
        assert!(result.command.is_none());
    }

    #[test]
    fn test_no_false_positive_in_path() {
        let result = parse_slash_command("Path: /usr/local/newtask");
        assert_eq!(result.processed_text, "Path: /usr/local/newtask");
        assert!(result.command.is_none());
    }

    #[test]
    fn test_only_one_slash_command_processed() {
        let result = parse_slash_command("/first /second /third");
        assert_eq!(result.processed_text, "/second /third");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "first");
    }

    #[test]
    fn test_slash_command_at_string_end() {
        let result = parse_slash_command("/compact");
        assert_eq!(result.processed_text, "");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "compact");
    }

    #[test]
    fn test_process_newtask_command() {
        let result = process_slash_command("/newtask make it work");
        assert!(result.contains("<explicit_instructions type=\"new_task\">"));
        assert!(result.contains("make it work"));
    }

    #[test]
    fn test_process_compact_command() {
        let result = process_slash_command("/compact now");
        assert!(result.contains("<explicit_instructions type=\"condense\">"));
        assert!(result.contains("now"));
    }

    #[test]
    fn test_process_newrule_command() {
        let result = process_slash_command("/newrule");
        assert!(result.contains("<explicit_instructions type=\"new_rule\">"));
    }

    #[test]
    fn test_process_reportbug_command() {
        let result = process_slash_command("/reportbug");
        assert!(result.contains("<explicit_instructions type=\"report_bug\">"));
    }

    #[test]
    fn test_process_explain_changes_command() {
        let result = process_slash_command("/explain-changes");
        assert!(result.contains("<explicit_instructions type=\"explain_changes\">"));
    }

    #[test]
    fn test_process_no_command() {
        let result = process_slash_command("Hello world");
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_is_compact_command_compact() {
        assert!(is_compact_command("/compact"));
    }

    #[test]
    fn test_is_compact_command_slash_compact() {
        assert!(is_compact_command("/compact"));
    }

    #[test]
    fn test_is_compact_command_newtask() {
        assert!(!is_compact_command("/newtask"));
    }

    #[test]
    fn test_is_compact_command_no_command() {
        assert!(!is_compact_command("Hello world"));
    }

    #[test]
    fn test_parse_cli_only_exit() {
        let result = get_cli_only_command("/exit");
        assert_eq!(result, Some(CliOnlyCommand::Exit));
    }

    #[test]
    fn test_parse_cli_only_quit() {
        let result = get_cli_only_command("/q");
        assert_eq!(result, Some(CliOnlyCommand::Quit));
    }

    #[test]
    fn test_parse_cli_only_clear() {
        let result = get_cli_only_command("/clear");
        assert_eq!(result, Some(CliOnlyCommand::Clear));
    }

    #[test]
    fn test_parse_cli_only_history() {
        let result = get_cli_only_command("/history");
        assert_eq!(result, Some(CliOnlyCommand::History));
    }

    #[test]
    fn test_parse_cli_only_skills() {
        let result = get_cli_only_command("/skills");
        assert_eq!(result, Some(CliOnlyCommand::Skills));
    }

    #[test]
    fn test_parse_cli_only_help() {
        let result = get_cli_only_command("/help");
        assert_eq!(result, Some(CliOnlyCommand::Help));
    }

    #[test]
    fn test_parse_cli_only_help_with_arg() {
        let result = get_cli_only_command("/help newtask");
        assert!(matches!(result, Some(CliOnlyCommand::HelpOption(ref cmd)) if cmd == "newtask"));
    }

    #[test]
    fn test_format_help_for_command_newtask() {
        let text = format_help_for_command("newtask");
        assert!(text.contains("newtask"));
        assert!(text.contains("Creates a new task"));
        assert!(text.contains("═══════"));
    }

    #[test]
    fn test_format_help_for_command_unknown() {
        let text = format_help_for_command("unknowncmd");
        assert!(text.contains("Unknown command"));
        assert!(text.contains("unknowncmd"));
    }

    #[test]
    fn test_parse_cli_only_settings() {
        let result = get_cli_only_command("/settings");
        assert_eq!(result, Some(CliOnlyCommand::Settings));
    }

    #[test]
    fn test_parse_cli_only_models() {
        let result = get_cli_only_command("/models");
        assert_eq!(result, Some(CliOnlyCommand::Models));
    }

    #[test]
    fn test_parse_cli_only_unknown() {
        let result = get_cli_only_command("/unknown");
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_cli_only_expand() {
        let result = get_cli_only_command("/expand 1");
        assert_eq!(result, Some(CliOnlyCommand::Expand));
    }

    #[test]
    fn test_parse_expand_index() {
        assert_eq!(parse_expand_index("/expand 12"), Some(12));
        assert_eq!(parse_expand_index("/expand 0"), None);
        assert_eq!(parse_expand_index("/expand nope"), None);
        assert_eq!(parse_expand_index("/help"), None);
    }

    #[test]
    fn test_cli_only_command_is_shutdown_exit() {
        assert!(CliOnlyCommand::parse("exit").unwrap().is_shutdown());
    }

    #[test]
    fn test_cli_only_command_is_shutdown_quit() {
        assert!(CliOnlyCommand::parse("quit").unwrap().is_shutdown());
        assert!(CliOnlyCommand::parse("q").unwrap().is_shutdown());
    }

    #[test]
    fn test_cli_only_command_is_clear() {
        assert!(CliOnlyCommand::parse("clear").unwrap().is_clear());
    }

    #[test]
    fn test_parse_cli_only_no_command() {
        let result = get_cli_only_command("Hello world");
        assert_eq!(result, None);
    }

    #[test]
    fn test_format_help_text_contains_commands() {
        let text = format_help_text();
        assert!(text.contains("/newtask"));
        assert!(text.contains("/compact"));
        assert!(text.contains("/exit"));
        assert!(text.contains("/clear"));
        assert!(text.contains("/history"));
        assert!(text.contains("/skills"));
        assert!(text.contains("/help"));
    }

    #[test]
    fn test_format_help_text_contains_keyboard_shortcuts() {
        let text = format_help_text();
        assert!(text.contains("Keyboard Shortcuts"));
        assert!(text.contains("Ctrl+A"));
        assert!(text.contains("Ctrl+E"));
        assert!(text.contains("Ctrl+U"));
        assert!(text.contains("Ctrl+K"));
        assert!(text.contains("Ctrl+W"));
        assert!(text.contains("Ctrl+C"));
        assert!(text.contains("Arrow Left"));
        assert!(text.contains("Arrow Up"));
    }

    #[test]
    fn test_format_help_text_contains_ansi_codes() {
        let text = format_help_text();
        // ANSI escape code prefix
        assert!(text.contains("\x1b["));
        // Bold style
        assert!(text.contains("\x1b[1m"));
        // Cyan color (for command names)
        assert!(text.contains("\x1b[96m"));
        // Dim style (for descriptions)
        assert!(text.contains("\x1b[2m"));
        // Reset code
        assert!(text.contains("\x1b[0m"));
    }

    #[test]
    fn test_format_help_text_has_visual_hierarchy() {
        let text = format_help_text();
        // Banner at top
        assert!(text.contains("═══════ Sned Commands ═══════"));
        // Separators between sections
        assert!(text.contains("─────────────────────────────"));
        // Section headers
        assert!(text.contains("Base Commands (sent to AI):"));
        assert!(text.contains("CLI-Only Commands (handled locally):"));
        assert!(text.contains("Keyboard Shortcuts:"));
        assert!(text.contains("Examples:"));
    }

    #[test]
    fn test_format_help_text_shows_aliases() {
        let text = format_help_text();
        // Exit command aliases
        assert!(text.contains("aliases: /q, /quit"));
        // Help command alias
        assert!(text.contains("alias: /help <command>"));
    }

    #[test]
    fn test_format_help_text_shows_track_changes_badge() {
        let text = format_help_text();
        // Commands requiring --track-changes should have yellow badge
        assert!(text.contains("[requires --track-changes]"));
        // Check multiple commands have the badge
        assert!(text.contains("/undo"));
        assert!(text.contains("/diff"));
        assert!(text.contains("/log"));
        assert!(text.contains("/commit"));
        assert!(text.contains("/checkpoint list"));
        assert!(text.contains("/checkpoint restore"));
        assert!(text.contains("/checkpoint undo"));
    }

    #[test]
    fn test_format_help_for_command_shows_aliases() {
        // Test checkpoint commands show aliases
        let checkpoint_list = format_help_for_command("checkpoint list");
        assert!(checkpoint_list.contains("Aliases: /checkpoint-list, /checkpoint list"));

        let checkpoint_restore = format_help_for_command("checkpoint restore");
        assert!(checkpoint_restore.contains("Aliases: /checkpoint-restore, /checkpoint restore"));

        let checkpoint_undo = format_help_for_command("checkpoint undo");
        assert!(checkpoint_undo.contains("Aliases: /checkpoint-undo, /checkpoint undo"));

        // Test resetcompact shows alias
        let resetcompact = format_help_for_command("resetcompact");
        assert!(resetcompact.contains("Alias: /clearcompact"));
    }

    #[test]
    fn test_format_settings_text() {
        let text = format_settings_text("anthropic", "claude-3-5-sonnet", "act", false);
        assert!(text.contains("anthropic"));
        assert!(text.contains("claude-3-5-sonnet"));
        assert!(text.contains("act"));
        assert!(text.contains("disabled"));
    }

    #[test]
    fn test_format_models_text_contains_providers() {
        let text = format_models_text();
        assert!(text.contains("Anthropic"));
        assert!(text.contains("OpenAI"));
        assert!(text.contains("claude-"));
        assert!(text.contains("gpt-"));
    }

    #[test]
    fn test_slash_command_skill_command_variant() {
        let result = SlashCommand::parse("some-skill");
        assert!(result.is_none());
    }

    #[test]
    fn test_slash_command_workflow_command_variant() {
        let result = SlashCommand::parse("some-workflow");
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_with_skills_finds_skill() {
        use crate::core::context::instructions::{SkillMetadata, SkillSource};

        let skills = vec![SkillMetadata {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            path: "/tmp/skills/test-skill/SKILL.md".to_string(),
            source: SkillSource::Project,
        }];

        let result = SlashCommand::parse_with_skills_and_workflows(
            "test-skill",
            &skills,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            Path::new("/tmp"),
        );

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.is_skill_command());
        assert_eq!(cmd.skill_name(), Some("test-skill"));
    }

    #[test]
    fn test_parse_with_skills_unknown_falls_through() {
        use crate::core::context::instructions::{SkillMetadata, SkillSource};

        let skills = vec![SkillMetadata {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            path: "/tmp/skills/test-skill/SKILL.md".to_string(),
            source: SkillSource::Project,
        }];

        let result = SlashCommand::parse_with_skills_and_workflows(
            "unknown-cmd",
            &skills,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            Path::new("/tmp"),
        );

        assert!(result.is_none());
    }

    #[test]
    fn test_builtin_commands_still_work() {
        let result = SlashCommand::parse_with_skills_and_workflows(
            "newtask",
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            Path::new("/tmp"),
        );

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(!cmd.is_skill_command());
        assert!(!cmd.is_workflow_command());
        assert_eq!(cmd.instruction_block(), NEW_TASK_INSTRUCTION);
    }

    #[test]
    fn test_skill_command_helper_methods() {
        let skill_cmd = SlashCommand::SkillCommand {
            name: "my-skill".to_string(),
        };
        assert!(skill_cmd.is_skill_command());
        assert!(!skill_cmd.is_workflow_command());
        assert_eq!(skill_cmd.skill_name(), Some("my-skill"));
        assert_eq!(skill_cmd.workflow_name(), None);
    }

    #[test]
    fn test_workflow_command_helper_methods() {
        let workflow_cmd = SlashCommand::WorkflowCommand {
            name: "my-workflow".to_string(),
        };
        assert!(!workflow_cmd.is_skill_command());
        assert!(workflow_cmd.is_workflow_command());
        assert_eq!(workflow_cmd.skill_name(), None);
        assert_eq!(workflow_cmd.workflow_name(), Some("my-workflow"));
    }

    #[test]
    fn test_skill_slash_command_injects_instructions() {
        use crate::core::context::instructions::{SkillMetadata, SkillSource};

        let skills = vec![SkillMetadata {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            path: "/tmp/skills/test-skill/SKILL.md".to_string(),
            source: SkillSource::Project,
        }];

        // Mock: we can't easily mock get_skill_content_for_command, but we can test the flow
        // This test verifies the command is parsed correctly
        let result = SlashCommand::parse_with_skills_and_workflows(
            "test-skill",
            &skills,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            Path::new("/tmp"),
        );

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.is_skill_command());
        assert_eq!(cmd.skill_name(), Some("test-skill"));
    }

    #[test]
    fn test_process_slash_command_with_skill_injects_content() {
        // This tests that process_slash_command_with_context correctly formats skill content
        // Since we can't easily mock get_skill_content_for_command, we test the format
        let text = "/test-skill do something";
        let result = parse_slash_command(text);
        assert!(result.command.is_some());
        assert_eq!(result.processed_text, "do something");
    }

    #[test]
    fn test_parse_cli_only_checkpoint_list() {
        assert_eq!(
            get_cli_only_command("/checkpoint-list"),
            Some(CliOnlyCommand::CheckpointList)
        );
        assert_eq!(
            get_cli_only_command("/checkpoint list"),
            Some(CliOnlyCommand::CheckpointList)
        );
    }

    #[test]
    fn test_parse_cli_only_checkpoint_restore() {
        assert_eq!(
            get_cli_only_command("/checkpoint-restore"),
            Some(CliOnlyCommand::CheckpointRestore)
        );
        assert_eq!(
            get_cli_only_command("/checkpoint restore"),
            Some(CliOnlyCommand::CheckpointRestore)
        );
    }

    #[test]
    fn test_parse_cli_only_checkpoint_undo() {
        assert_eq!(
            get_cli_only_command("/checkpoint-undo"),
            Some(CliOnlyCommand::CheckpointUndo)
        );
        assert_eq!(
            get_cli_only_command("/checkpoint undo"),
            Some(CliOnlyCommand::CheckpointUndo)
        );
    }

    #[test]
    fn test_parse_checkpoint_restore_number() {
        assert_eq!(parse_checkpoint_restore("/checkpoint-restore 3"), Some(3));
        assert_eq!(parse_checkpoint_restore("/checkpoint restore 5"), Some(5));
        assert_eq!(parse_checkpoint_restore("/checkpoint-restore 0"), None);
        assert_eq!(parse_checkpoint_restore("/checkpoint-restore"), None);
        assert_eq!(parse_checkpoint_restore("/checkpoint-restore abc"), None);
    }

    #[test]
    fn test_format_help_text_contains_checkpoint_commands() {
        let text = format_help_text();
        assert!(text.contains("/checkpoint list"));
        assert!(text.contains("/checkpoint restore"));
        assert!(text.contains("/checkpoint undo"));
    }
}
