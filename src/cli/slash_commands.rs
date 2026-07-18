use regex::Regex;

use crate::core::context::instructions::{self, SkillMetadata};

const SLASH_COMMAND_REGEX: &str = r"(^|\s)\/([a-zA-Z0-9_.:@?-]+)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashCommandId {
    Compact,
    NewRule,
    Exit,
    Clear,
    Copy,
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
    Changes,
    Queue,
    Retry,
    Model,
    Plan,
    PlanApprove,
    PlanPause,
    PlanResume,
    PlanAbort,
    PlanComplete,
    PlanFail,
    PlanEdit,
    PlanAdd,
    PlanRemove,
    PlanReplace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandRequirement {
    Always,
    TrackChanges,
    RetryableRequest,
    CompactedSummary,
    PlanExists,
    PlanPending,
    PlanRunning,
    PlanPaused,
    PlanActive,
    PlanEditable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlashCommandAvailability {
    pub track_changes: bool,
    pub has_retryable_request: bool,
    pub has_compacted_summary: bool,
    pub has_plan: bool,
    pub plan_approved: bool,
    pub plan_paused: bool,
    pub plan_complete: bool,
    pub plan_mode: bool,
}

impl SlashCommandAvailability {
    #[must_use]
    pub fn from_task_state(
        state: &crate::core::agent_types::TaskState,
        track_changes: bool,
        plan_mode: bool,
    ) -> Self {
        let plan = state.plan_state.as_ref();
        Self {
            track_changes,
            has_retryable_request: state.retryable_failed_request.is_some(),
            has_compacted_summary: state.compacted_summary.is_some(),
            has_plan: plan.is_some(),
            plan_approved: plan.is_some_and(|plan| plan.approved),
            plan_paused: plan.is_some_and(|plan| plan.paused),
            plan_complete: plan.is_some_and(|plan| plan.complete),
            plan_mode,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SlashCommandSpec {
    id: SlashCommandId,
    name: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    usage: &'static str,
    detail: &'static str,
    category: SlashCommandCategory,
    requires_args: bool,
    requirement: CommandRequirement,
}

const SLASH_COMMAND_SPECS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        id: SlashCommandId::Compact,
        name: "compact",
        aliases: &["c"],
        description: "Condense context to reduce token usage",
        usage: "/compact",
        detail: "Condenses the active model context while preserving the information needed to continue the task.",
        category: SlashCommandCategory::Agent,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::NewRule,
        name: "newrule",
        aliases: &["nr"],
        description: "Draft a rule from the current conversation",
        usage: "/newrule <rule request>",
        detail: "Asks the model to turn the current conversation into a concise project rule.",
        category: SlashCommandCategory::Agent,
        requires_args: true,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Exit,
        name: "exit",
        aliases: &["quit", "q"],
        description: "Exit the interactive shell",
        usage: "/exit",
        detail: "Closes the interactive session after flushing pending local output.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Clear,
        name: "clear",
        aliases: &["cls"],
        description: "Clear the visible display",
        usage: "/clear",
        detail: "Clears the transcript shown in this terminal. Model context and saved conversation history are unchanged.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Copy,
        name: "copy",
        aliases: &[],
        description: "Copy the last completion to the clipboard",
        usage: "/copy",
        detail: "Copies the most recent agent completion as raw Markdown through the terminal clipboard.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::History,
        name: "history",
        aliases: &["h"],
        description: "Show recent command input",
        usage: "/history",
        detail: "Shows the ten most recent entries from interactive command history.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Skills,
        name: "skills",
        aliases: &[],
        description: "List available skills",
        usage: "/skills",
        detail: "Lists discovered skills. Enter a skill as its own slash command to activate it.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Help,
        name: "help",
        aliases: &["?"],
        description: "Search commands and view details",
        usage: "/help [query]",
        detail: "Opens searchable command help. Enter inserts the selected command; Esc closes help.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Settings,
        name: "settings",
        aliases: &[],
        description: "Show current session settings",
        usage: "/settings",
        detail: "Shows the current provider, model, mode, and approval setting.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Models,
        name: "models",
        aliases: &[],
        description: "List configured model examples",
        usage: "/models",
        detail: "Shows provider and model examples accepted by the model switch command.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::ResetCompact,
        name: "resetcompact",
        aliases: &["clearcompact"],
        description: "Clear the active compacted summary",
        usage: "/resetcompact",
        detail: "Removes the saved compacted summary so the uncompacted context path can be used again.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::CompactedSummary,
    },
    SlashCommandSpec {
        id: SlashCommandId::Stats,
        name: "stats",
        aliases: &[],
        description: "Show token usage and session cost",
        usage: "/stats",
        detail: "Shows cumulative token, cache, timing, command, and cost statistics for this session.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Undo,
        name: "undo",
        aliases: &[],
        description: "Undo the last tracked turn",
        usage: "/undo",
        detail: "Restores the most recent tracked checkpoint and removes the corresponding conversation turn.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::TrackChanges,
    },
    SlashCommandSpec {
        id: SlashCommandId::Diff,
        name: "diff",
        aliases: &[],
        description: "Show changes from the last tracked turn",
        usage: "/diff",
        detail: "Shows the shadow-git diff for the most recent tracked turn.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::TrackChanges,
    },
    SlashCommandSpec {
        id: SlashCommandId::Log,
        name: "log",
        aliases: &[],
        description: "Show tracked turn history",
        usage: "/log",
        detail: "Shows recent shadow-git checkpoints created for tracked turns.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::TrackChanges,
    },
    SlashCommandSpec {
        id: SlashCommandId::Commit,
        name: "commit",
        aliases: &[],
        description: "Commit pending tracked changes",
        usage: "/commit <message>",
        detail: "Reviews pending changes and asks for confirmation before committing them to the workspace repository.",
        category: SlashCommandCategory::Local,
        requires_args: true,
        requirement: CommandRequirement::TrackChanges,
    },
    SlashCommandSpec {
        id: SlashCommandId::CheckpointList,
        name: "checkpoint list",
        aliases: &["checkpoint-list"],
        description: "List tracked checkpoints",
        usage: "/checkpoint list",
        detail: "Lists available tracked checkpoints with their numbers and timestamps.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::TrackChanges,
    },
    SlashCommandSpec {
        id: SlashCommandId::CheckpointRestore,
        name: "checkpoint restore",
        aliases: &["checkpoint-restore"],
        description: "Restore a tracked checkpoint",
        usage: "/checkpoint restore <number>",
        detail: "Shows affected files and asks for confirmation before restoring the selected checkpoint.",
        category: SlashCommandCategory::Local,
        requires_args: true,
        requirement: CommandRequirement::TrackChanges,
    },
    SlashCommandSpec {
        id: SlashCommandId::CheckpointUndo,
        name: "checkpoint undo",
        aliases: &["checkpoint-undo"],
        description: "Restore the previous checkpoint",
        usage: "/checkpoint undo",
        detail: "Restores the previous tracked checkpoint and trims the corresponding conversation turn.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::TrackChanges,
    },
    SlashCommandSpec {
        id: SlashCommandId::Changes,
        name: "changes",
        aliases: &[],
        description: "Show files changed in this session",
        usage: "/changes",
        detail: "Summarizes files created or edited during the current session.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Queue,
        name: "queue",
        aliases: &[],
        description: "Show queued messages",
        usage: "/queue",
        detail: "Shows messages waiting for the current agent turn to finish.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Retry,
        name: "retry",
        aliases: &[],
        description: "Retry the last safe failed request",
        usage: "/retry",
        detail: "Replays the last user-authored request that failed before any tool execution.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::RetryableRequest,
    },
    SlashCommandSpec {
        id: SlashCommandId::Model,
        name: "model",
        aliases: &[],
        description: "Switch the active provider and model",
        usage: "/model [provider/model_id]",
        detail: "Opens the model picker without an argument, or switches directly to a provider/model_id pair.",
        category: SlashCommandCategory::Local,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::Plan,
        name: "plan",
        aliases: &[],
        description: "View a plan or start plan mode",
        usage: "/plan [task description]",
        detail: "Shows the current plan without arguments, or starts plan mode for the supplied task description.",
        category: SlashCommandCategory::Plan,
        requires_args: false,
        requirement: CommandRequirement::Always,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanApprove,
        name: "plan approve",
        aliases: &[],
        description: "Approve the pending plan",
        usage: "/plan approve",
        detail: "Approves the current pending plan and starts execution at its first pending step.",
        category: SlashCommandCategory::Plan,
        requires_args: false,
        requirement: CommandRequirement::PlanPending,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanPause,
        name: "plan pause",
        aliases: &[],
        description: "Pause the running plan",
        usage: "/plan pause",
        detail: "Pauses plan progression after the current operation.",
        category: SlashCommandCategory::Plan,
        requires_args: false,
        requirement: CommandRequirement::PlanRunning,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanResume,
        name: "plan resume",
        aliases: &[],
        description: "Resume the paused plan",
        usage: "/plan resume",
        detail: "Resumes execution from the current plan step.",
        category: SlashCommandCategory::Plan,
        requires_args: false,
        requirement: CommandRequirement::PlanPaused,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanAbort,
        name: "plan abort",
        aliases: &[],
        description: "Exit plan mode",
        usage: "/plan abort",
        detail: "Aborts the active plan or exits plan mode while keeping changes already applied.",
        category: SlashCommandCategory::Plan,
        requires_args: false,
        requirement: CommandRequirement::PlanActive,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanComplete,
        name: "plan complete",
        aliases: &[],
        description: "Mark the active plan complete",
        usage: "/plan complete",
        detail: "Marks the active plan complete and exits its execution flow.",
        category: SlashCommandCategory::Plan,
        requires_args: false,
        requirement: CommandRequirement::PlanExists,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanFail,
        name: "plan fail",
        aliases: &[],
        description: "Mark the current plan step failed",
        usage: "/plan fail",
        detail: "Marks the current plan step failed and pauses progression for recovery.",
        category: SlashCommandCategory::Plan,
        requires_args: false,
        requirement: CommandRequirement::PlanExists,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanEdit,
        name: "plan edit",
        aliases: &[],
        description: "Edit a plan step",
        usage: "/plan edit <step> <description>",
        detail: "Changes one step while the plan is pending or paused.",
        category: SlashCommandCategory::Plan,
        requires_args: true,
        requirement: CommandRequirement::PlanEditable,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanAdd,
        name: "plan add",
        aliases: &[],
        description: "Add a plan step",
        usage: "/plan add <after_step> <description>",
        detail: "Adds a step after the selected step, or at the beginning when the index is zero.",
        category: SlashCommandCategory::Plan,
        requires_args: true,
        requirement: CommandRequirement::PlanEditable,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanRemove,
        name: "plan remove",
        aliases: &[],
        description: "Remove a plan step",
        usage: "/plan remove <step>",
        detail: "Removes one step while the plan is pending or paused.",
        category: SlashCommandCategory::Plan,
        requires_args: true,
        requirement: CommandRequirement::PlanEditable,
    },
    SlashCommandSpec {
        id: SlashCommandId::PlanReplace,
        name: "plan replace",
        aliases: &[],
        description: "Replace all plan steps",
        usage: "/plan replace <numbered plan>",
        detail: "Replaces the current plan with a new numbered plan while execution is not running.",
        category: SlashCommandCategory::Plan,
        requires_args: true,
        requirement: CommandRequirement::PlanEditable,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Compact,
    NewRule,
    SkillCommand { name: String },
}

impl SlashCommand {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let spec = command_spec_for_name(s)?;
        match spec.id {
            SlashCommandId::Compact => Some(Self::Compact),
            SlashCommandId::NewRule => Some(Self::NewRule),
            _ => None,
        }
    }

    #[must_use]
    pub fn parse_with_skills(
        command_name: &str,
        available_skills: &[SkillMetadata],
    ) -> Option<Self> {
        if command_spec_for_name(command_name).is_some() {
            return Self::parse(command_name);
        }

        if let Some(skill) = available_skills
            .iter()
            .find(|skill| skill.name.eq_ignore_ascii_case(command_name))
        {
            return Some(Self::SkillCommand {
                name: skill.name.clone(),
            });
        }

        None
    }

    #[must_use] 
    pub fn is_skill_command(&self) -> bool {
        matches!(self, Self::SkillCommand { .. })
    }

    #[must_use] 
    pub fn is_compact(&self) -> bool {
        matches!(self, Self::Compact)
    }

    #[must_use] 
    pub fn instruction_block(&self) -> &'static str {
        match self {
            Self::Compact => CONDENSE_INSTRUCTION,
            Self::NewRule => NEW_RULE_INSTRUCTION,
            Self::SkillCommand { .. } => "",
        }
    }

    #[must_use] 
    pub fn skill_name(&self) -> Option<&str> {
        match self {
            Self::SkillCommand { name } => Some(name),
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

#[must_use] 
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
    Copy,
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
    Changes,
    HelpOption(String),
    Queue,
    Retry,
    Plan(PlanSubcommand),
    PlanPrompt(String),
    PlanApprove,
    PlanPause,
    PlanResume,
    PlanAbort,
    PlanComplete,
    PlanFail,
    ModelSwitch(String), // /model provider/model_id
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanSubcommand {
    /// Show current plan status
    Status,
    /// Edit a step: (step_number, new_description)
    Edit(usize, String),
    /// Add a step after: (after_step_number, description)
    Add(usize, String),
    /// Remove a step: (step_number)
    Remove(usize),
    /// Replace the entire plan: (full_plan_text)
    Replace(String),
    Approve,
    Pause,
    Resume,
    Abort,
    Complete,
    Fail,
}

pub fn parse_plan_subcommand(args: &str) -> Option<PlanSubcommand> {
    let args = args.trim_start();
    if args.is_empty() {
        return Some(PlanSubcommand::Status);
    }

    let mut tokens = args.splitn(2, char::is_whitespace);
    let command = tokens.next().unwrap_or("");
    let remainder = tokens.next().unwrap_or("").trim_start();

    match command {
        "status" => Some(PlanSubcommand::Status),
        "approve" => Some(PlanSubcommand::Approve),
        "pause" => Some(PlanSubcommand::Pause),
        "resume" => Some(PlanSubcommand::Resume),
        "abort" => Some(PlanSubcommand::Abort),
        "complete" => Some(PlanSubcommand::Complete),
        "fail" => Some(PlanSubcommand::Fail),
        "edit" => {
            let mut parts = remainder.splitn(2, char::is_whitespace);
            let step = parts.next().unwrap_or("");
            let description = parts.next().unwrap_or("").trim_start();
            let step = step.parse::<usize>().unwrap_or(0);
            Some(PlanSubcommand::Edit(step, description.to_string()))
        }
        "add" => {
            let mut parts = remainder.splitn(2, char::is_whitespace);
            let after_step = parts.next().unwrap_or("");
            let description = parts.next().unwrap_or("").trim_start();
            let after_step = after_step.parse::<usize>().unwrap_or(0);
            Some(PlanSubcommand::Add(after_step, description.to_string()))
        }
        "remove" => {
            let step = remainder
                .split_whitespace()
                .next()
                .and_then(|raw| raw.parse::<usize>().ok())
                .unwrap_or(0);
            Some(PlanSubcommand::Remove(step))
        }
        "replace" => Some(PlanSubcommand::Replace(remainder.to_string())),
        _ => None,
    }
}

impl CliOnlyCommand {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let matched = match_static_command(s)?;
        cli_command_from_match(&matched)
    }

    /// Parse plan subcommands with arguments.
    /// Called when the command is "plan" and there's additional text.
    #[must_use]
    pub fn parse_plan_with_args(args: &str) -> Option<Self> {
        let args_trimmed = args.trim();
        let subcmd = parse_plan_subcommand(args_trimmed);
        match subcmd {
            Some(PlanSubcommand::Status) => Some(Self::Plan(PlanSubcommand::Status)),
            Some(PlanSubcommand::Edit(step, desc)) => {
                Some(Self::Plan(PlanSubcommand::Edit(step, desc)))
            }
            Some(PlanSubcommand::Add(after, desc)) => {
                Some(Self::Plan(PlanSubcommand::Add(after, desc)))
            }
            Some(PlanSubcommand::Remove(step)) => {
                Some(Self::Plan(PlanSubcommand::Remove(step)))
            }
            Some(PlanSubcommand::Replace(text)) => {
                Some(Self::Plan(PlanSubcommand::Replace(text)))
            }
            Some(PlanSubcommand::Approve) => Some(Self::PlanApprove),
            Some(PlanSubcommand::Pause) => Some(Self::PlanPause),
            Some(PlanSubcommand::Resume) => Some(Self::PlanResume),
            Some(PlanSubcommand::Abort) => Some(Self::PlanAbort),
            Some(PlanSubcommand::Complete) => Some(Self::PlanComplete),
            Some(PlanSubcommand::Fail) => Some(Self::PlanFail),
            None if args_trimmed.is_empty() => Some(Self::Plan(PlanSubcommand::Status)),
            None => Some(Self::PlanPrompt(args_trimmed.to_string())),
        }
    }

    #[must_use]
    pub fn parse_with_arg(cmd: &str, arg: &str) -> Option<Self> {
        let invocation = if arg.trim().is_empty() {
            cmd.to_string()
        } else {
            format!("{} {}", cmd, arg.trim())
        };
        Self::parse(&invocation)
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        matches!(self, Self::Exit | Self::Quit)
    }

    #[must_use]
    pub fn is_clear(&self) -> bool {
        matches!(self, Self::Clear)
    }

    #[must_use]
    pub fn is_reset_compact(&self) -> bool {
        matches!(self, Self::ResetCompact)
    }

    /// Returns true if this command can execute locally without the agent.
    #[must_use]
    pub fn is_local_command(&self) -> bool {
        matches!(
            self,
            Self::Exit
                | Self::Quit
                | Self::Clear
                | Self::Copy
                | Self::History
                | Self::Skills
                | Self::Help
                | Self::HelpOption(_)
                | Self::Settings
                | Self::Models
                | Self::ResetCompact
                | Self::Stats
                | Self::Changes
                | Self::Queue
                | Self::Retry
                | Self::Plan(_)
                | Self::PlanPrompt(_)
                | Self::PlanApprove
                | Self::PlanPause
                | Self::PlanResume
                | Self::PlanAbort
                | Self::PlanComplete
                | Self::PlanFail
                | Self::ModelSwitch(_)
        )
    }

    /// Returns true if this command requires the agent to be idle.
    #[must_use] 
    pub fn requires_agent_idle(&self) -> bool {
        matches!(
            self,
            Self::Undo
                | Self::Diff
                | Self::Log
                | Self::Commit
                | Self::CheckpointList
                | Self::CheckpointRestore
                | Self::CheckpointUndo
        )
    }

    /// Returns true if this is a plan command.
    #[must_use] 
    pub fn is_plan_command(&self) -> bool {
        matches!(
            self,
            Self::Plan(_)
                | Self::PlanPrompt(_)
                | Self::PlanApprove
                | Self::PlanPause
                | Self::PlanResume
                | Self::PlanAbort
                | Self::PlanComplete
                | Self::PlanFail
        )
    }
}

struct StaticCommandMatch<'a> {
    spec: &'a SlashCommandSpec,
    matched_name: &'a str,
    args: &'a str,
}

fn command_spec_for_name(name: &str) -> Option<&'static SlashCommandSpec> {
    SLASH_COMMAND_SPECS.iter().find(|spec| {
        spec.name.eq_ignore_ascii_case(name)
            || spec
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    })
}

fn match_static_command(input: &str) -> Option<StaticCommandMatch<'_>> {
    let input = input.trim();
    SLASH_COMMAND_SPECS
        .iter()
        .flat_map(|spec| {
            std::iter::once(spec.name)
                .chain(spec.aliases.iter().copied())
                .map(move |name| (spec, name))
        })
        .filter_map(|(spec, name)| {
            if input.eq_ignore_ascii_case(name) {
                return Some(StaticCommandMatch {
                    spec,
                    matched_name: name,
                    args: "",
                });
            }

            let prefix = input.get(..name.len())?;
            if prefix.eq_ignore_ascii_case(name)
                && input
                    .get(name.len()..)
                    .is_some_and(|rest| rest.starts_with(char::is_whitespace))
            {
                return Some(StaticCommandMatch {
                    spec,
                    matched_name: name,
                    args: input[name.len()..].trim_start(),
                });
            }
            None
        })
        .max_by_key(|matched| matched.matched_name.len())
}

fn cli_command_from_match(matched: &StaticCommandMatch<'_>) -> Option<CliOnlyCommand> {
    let args = matched.args;
    match matched.spec.id {
        SlashCommandId::Compact | SlashCommandId::NewRule => None,
        SlashCommandId::Exit => {
            if matched.matched_name.eq_ignore_ascii_case("exit") {
                Some(CliOnlyCommand::Exit)
            } else {
                Some(CliOnlyCommand::Quit)
            }
        }
        SlashCommandId::Clear => Some(CliOnlyCommand::Clear),
        SlashCommandId::Copy => Some(CliOnlyCommand::Copy),
        SlashCommandId::History => Some(CliOnlyCommand::History),
        SlashCommandId::Skills => Some(CliOnlyCommand::Skills),
        SlashCommandId::Help => {
            if args.is_empty() {
                Some(CliOnlyCommand::Help)
            } else {
                Some(CliOnlyCommand::HelpOption(args.to_lowercase()))
            }
        }
        SlashCommandId::Settings => Some(CliOnlyCommand::Settings),
        SlashCommandId::Models => Some(CliOnlyCommand::Models),
        SlashCommandId::ResetCompact => Some(CliOnlyCommand::ResetCompact),
        SlashCommandId::Stats => Some(CliOnlyCommand::Stats),
        SlashCommandId::Undo => Some(CliOnlyCommand::Undo),
        SlashCommandId::Diff => Some(CliOnlyCommand::Diff),
        SlashCommandId::Log => Some(CliOnlyCommand::Log),
        SlashCommandId::Commit => Some(CliOnlyCommand::Commit),
        SlashCommandId::CheckpointList => Some(CliOnlyCommand::CheckpointList),
        SlashCommandId::CheckpointRestore => Some(CliOnlyCommand::CheckpointRestore),
        SlashCommandId::CheckpointUndo => Some(CliOnlyCommand::CheckpointUndo),
        SlashCommandId::Changes => Some(CliOnlyCommand::Changes),
        SlashCommandId::Queue => Some(CliOnlyCommand::Queue),
        SlashCommandId::Retry => Some(CliOnlyCommand::Retry),
        SlashCommandId::Model => Some(CliOnlyCommand::ModelSwitch(args.to_string())),
        SlashCommandId::Plan => CliOnlyCommand::parse_plan_with_args(args),
        SlashCommandId::PlanApprove => Some(CliOnlyCommand::PlanApprove),
        SlashCommandId::PlanPause => Some(CliOnlyCommand::PlanPause),
        SlashCommandId::PlanResume => Some(CliOnlyCommand::PlanResume),
        SlashCommandId::PlanAbort => Some(CliOnlyCommand::PlanAbort),
        SlashCommandId::PlanComplete => Some(CliOnlyCommand::PlanComplete),
        SlashCommandId::PlanFail => Some(CliOnlyCommand::PlanFail),
        SlashCommandId::PlanEdit => CliOnlyCommand::parse_plan_with_args(&format!("edit {args}")),
        SlashCommandId::PlanAdd => CliOnlyCommand::parse_plan_with_args(&format!("add {args}")),
        SlashCommandId::PlanRemove => {
            CliOnlyCommand::parse_plan_with_args(&format!("remove {args}"))
        }
        SlashCommandId::PlanReplace => {
            CliOnlyCommand::parse_plan_with_args(&format!("replace {args}"))
        }
    }
}

fn command_is_available(spec: &SlashCommandSpec, availability: SlashCommandAvailability) -> bool {
    match spec.requirement {
        CommandRequirement::Always => true,
        CommandRequirement::TrackChanges => availability.track_changes,
        CommandRequirement::RetryableRequest => availability.has_retryable_request,
        CommandRequirement::CompactedSummary => availability.has_compacted_summary,
        CommandRequirement::PlanExists => availability.has_plan && !availability.plan_complete,
        CommandRequirement::PlanPending => {
            availability.has_plan && !availability.plan_approved && !availability.plan_complete
        }
        CommandRequirement::PlanRunning => {
            availability.has_plan
                && availability.plan_approved
                && !availability.plan_paused
                && !availability.plan_complete
        }
        CommandRequirement::PlanPaused => {
            availability.has_plan
                && availability.plan_approved
                && availability.plan_paused
                && !availability.plan_complete
        }
        CommandRequirement::PlanActive => {
            availability.plan_mode || (availability.has_plan && !availability.plan_complete)
        }
        CommandRequirement::PlanEditable => {
            availability.has_plan
                && !availability.plan_complete
                && (!availability.plan_approved || availability.plan_paused)
        }
    }
}

#[must_use]
pub fn parse_slash_command(text: &str) -> SlashCommandParseResult {
    if !text.starts_with('/') {
        return SlashCommandParseResult {
            processed_text: text.to_string(),
            command: None,
        };
    }

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

        let is_valid = following_char.is_none_or(char::is_whitespace);

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

#[must_use] 
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

#[must_use] 
pub fn process_slash_command_with_context(
    text: &str,
    available_skills: &[SkillMetadata],
) -> String {
    let result = parse_slash_command(text);
    if let Some(cmd) = result.command
        && let Some(slash_cmd) = SlashCommand::parse_with_skills(&cmd.command, available_skills)
    {
        if slash_cmd.is_skill_command() {
            if let Some(skill_name) = slash_cmd.skill_name()
                && let Some(content) = get_skill_content_for_command(skill_name, available_skills)
            {
                let instruction = format!(
                    "<explicit_instructions type=\"skill\" name=\"{skill_name}\">\n{content}\n</explicit_instructions>"
                );

                let text_after = &result.processed_text;
                let is_empty = text_after.trim().is_empty();

                if is_empty {
                    let activation_note = format!(
                        "\n(Note: The user has explicitly activated the \"{skill_name}\" skill via a slash command. This skill is now active. Please acknowledge its activation, summarize how you can help based on its instructions, and ask the user for the specific target or task they want you to perform, or propose a first step if appropriate.)\n"
                    );
                    return format!("{instruction}{activation_note}{text_after}");
                }
                return format!("{instruction}\n{text_after}");
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

#[must_use] 
pub fn is_compact_command(text: &str) -> bool {
    let result = parse_slash_command(text);
    if let Some(cmd) = result.command
        && let Some(slash_cmd) = SlashCommand::parse(&cmd.command)
    {
        return slash_cmd.is_compact();
    }
    false
}

#[must_use]
pub fn get_cli_only_command(text: &str) -> Option<CliOnlyCommand> {
    let result = parse_slash_command(text);
    let cmd = result.command?;
    let invocation = if result.processed_text.is_empty() {
        cmd.command
    } else {
        format!("{} {}", cmd.command, result.processed_text)
    };
    CliOnlyCommand::parse(&invocation)
}

#[must_use]
pub fn parse_checkpoint_restore(text: &str) -> Option<usize> {
    let result = parse_slash_command(text);
    let cmd = result.command?;
    let invocation = if result.processed_text.is_empty() {
        cmd.command
    } else {
        format!("{} {}", cmd.command, result.processed_text)
    };
    let matched = match_static_command(&invocation)?;
    if matched.spec.id != SlashCommandId::CheckpointRestore {
        return None;
    }
    matched
        .args
        .split_whitespace()
        .next()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|index| *index > 0)
}

#[must_use]
pub fn unknown_leading_slash_command(
    text: &str,
    available_skills: &[SkillMetadata],
) -> Option<String> {
    let body = text.strip_prefix('/')?;
    let command_name = body.split_whitespace().next()?;
    if command_name.is_empty()
        || match_static_command(body).is_some()
        || available_skills
            .iter()
            .any(|skill| skill.name.eq_ignore_ascii_case(command_name))
    {
        return None;
    }
    Some(command_name.to_string())
}

// =============================================================================
// Slash command autocomplete
// =============================================================================

/// Category of a slash command entry for grouping in the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandCategory {
    /// Commands injected to the agent (compact, newrule, etc.)
    Agent,
    /// Local CLI-only commands (exit, clear, help, etc.)
    Local,
    /// Plan subcommands (status, approve, pause, etc.)
    Plan,
    /// Skills loaded from available skills
    Skill,
}

/// A single slash command entry for the autocomplete picker.
#[derive(Debug, Clone)]
pub struct SlashCommandEntry {
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub category: SlashCommandCategory,
    pub requires_args: bool,
}

impl SlashCommandEntry {
    #[must_use]
    pub fn usage(&self) -> String {
        command_spec_for_name(&self.name).map_or_else(
            || format!("/{} <request>", self.name),
            |spec| spec.usage.to_string(),
        )
    }

    #[must_use]
    pub fn detail(&self) -> String {
        command_spec_for_name(&self.name)
            .map_or_else(|| self.description.clone(), |spec| spec.detail.to_string())
    }
}

#[must_use]
pub fn build_slash_command_entries(available_skills: &[SkillMetadata]) -> Vec<SlashCommandEntry> {
    build_slash_command_entries_with_availability(available_skills, None)
}

#[must_use]
pub fn build_available_slash_command_entries(
    available_skills: &[SkillMetadata],
    availability: SlashCommandAvailability,
) -> Vec<SlashCommandEntry> {
    build_slash_command_entries_with_availability(available_skills, Some(availability))
}

fn build_slash_command_entries_with_availability(
    available_skills: &[SkillMetadata],
    availability: Option<SlashCommandAvailability>,
) -> Vec<SlashCommandEntry> {
    let mut entries: Vec<SlashCommandEntry> = SLASH_COMMAND_SPECS
        .iter()
        .filter(|spec| availability.is_none_or(|state| command_is_available(spec, state)))
        .map(|spec| SlashCommandEntry {
            name: spec.name.to_string(),
            description: spec.description.to_string(),
            aliases: spec
                .aliases
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
            category: spec.category,
            requires_args: spec.requires_args,
        })
        .collect();

    for skill in available_skills {
        if command_spec_for_name(&skill.name).is_some() {
            continue;
        }
        let desc = if skill.description.is_empty() {
            "Skill".to_string()
        } else {
            skill.description.clone()
        };
        entries.push(SlashCommandEntry {
            name: skill.name.clone(),
            description: desc,
            aliases: vec![],
            category: SlashCommandCategory::Skill,
            requires_args: true,
        });
    }

    entries
}

/// Extract the slash command query from the input text.
///
/// Returns `Some(query)` if the user is currently typing a slash command
/// at the start of the input, `None` otherwise.
#[must_use]
pub fn extract_slash_query(text: &str) -> Option<String> {
    let after = text.strip_prefix('/')?;
    if after.split_whitespace().count() > 1 {
        return None;
    }
    Some(after.split_whitespace().next().unwrap_or("").to_string())
}

/// Replace the active slash query with the selected command name.
///
/// The returned text keeps any suffix after the typed query, but the current
/// `/query` token is replaced with the completed command name.
#[must_use]
pub fn apply_slash_completion(text: &str, command_name: &str) -> Option<(String, usize)> {
    let query = extract_slash_query(text)?;
    let end = 1 + query.len();

    let mut new_text = String::with_capacity(text.len() + command_name.len() + 1 - end);
    new_text.push('/');
    new_text.push_str(command_name);
    new_text.push_str(&text[end..]);

    let cursor_pos = command_name.len() + 1;
    Some((new_text, cursor_pos))
}

/// Filter slash command entries by query string.
///
/// Returns matching entries in priority order:
/// 1. Exact name match
/// 2. Prefix match on name
/// 3. Prefix match on alias
/// 4. Substring match on name or description
#[must_use] 
pub fn filter_slash_commands(entries: &[SlashCommandEntry], query: &str) -> Vec<SlashCommandEntry> {
    if query.is_empty() {
        return entries.to_vec();
    }

    let query_lower = query.to_lowercase();

    // Priority 1: exact name match
    let exact: Vec<SlashCommandEntry> = entries
        .iter()
        .filter(|e| e.name.to_lowercase() == query_lower)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }

    // Priority 2: prefix match on name
    let prefix: Vec<SlashCommandEntry> = entries
        .iter()
        .filter(|e| e.name.to_lowercase().starts_with(&query_lower))
        .cloned()
        .collect();
    if !prefix.is_empty() {
        return prefix;
    }

    // Priority 3: prefix match on any alias
    let alias: Vec<SlashCommandEntry> = entries
        .iter()
        .filter(|e| {
            e.aliases
                .iter()
                .any(|a| a.to_lowercase().starts_with(&query_lower))
        })
        .cloned()
        .collect();
    if !alias.is_empty() {
        return alias;
    }

    // Priority 4: substring match on name or description
    let substring: Vec<SlashCommandEntry> = entries
        .iter()
        .filter(|e| {
            e.name.to_lowercase().contains(&query_lower)
                || e.description.to_lowercase().contains(&query_lower)
        })
        .cloned()
        .collect();

    substring.into_iter().take(10).collect()
}
#[must_use]
pub fn format_help_text() -> String {
    use crate::cli::colors::style;

    let mut lines = vec![format!(
        "{}{}═══════ Sned Commands ═══════{}",
        style::BOLD,
        style::CYAN,
        style::RESET
    )];
    for spec in SLASH_COMMAND_SPECS {
        let aliases = if spec.aliases.is_empty() {
            String::new()
        } else {
            format!(
                " (aliases: {})",
                spec.aliases
                    .iter()
                    .map(|alias| format!("/{alias}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let requirement = if spec.requirement == CommandRequirement::TrackChanges {
            " [requires --track-changes]"
        } else {
            ""
        };
        lines.push(format!(
            "  {}{}{}  - {}{}{}{}",
            style::CYAN,
            spec.usage,
            style::RESET,
            spec.description,
            aliases,
            requirement,
            style::RESET
        ));
    }
    lines.extend([
        String::new(),
        format!(
            "{}{}Keyboard Shortcuts:{}",
            style::BOLD,
            style::CYAN,
            style::RESET
        ),
        "  ↑/↓ - Navigate history and pickers".to_string(),
        "  Tab/Enter - Insert a selected picker item".to_string(),
        "  Esc - Close the active picker".to_string(),
        "  Ctrl+C - Cancel the current operation".to_string(),
        "  Shift+drag - Select text; set SNED_DISABLE_MOUSE=1 for native mouse selection"
            .to_string(),
    ]);
    lines.join("\n")
}

#[must_use]
pub fn format_help_for_command(cmd: &str) -> String {
    use crate::cli::colors::style;

    let command = cmd.trim().trim_start_matches('/');
    let Some(matched) = match_static_command(command) else {
        return format!(
            "{}Unknown command: /{}{}\n\nType /help to browse available commands.",
            style::YELLOW,
            command,
            style::RESET
        );
    };
    let spec = matched.spec;
    let aliases = if spec.aliases.is_empty() {
        String::new()
    } else {
        format!(
            "\nAliases: {}",
            spec.aliases
                .iter()
                .map(|alias| format!("/{alias}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "{}{}Help: /{}{}\n\nUsage: {}\n\n{}\n{}{}",
        style::BOLD,
        style::CYAN,
        spec.name,
        style::RESET,
        spec.usage,
        spec.description,
        spec.detail,
        aliases
    )
}

#[must_use]
pub fn format_settings_text(provider: &str, model: &str, mode: &str, auto_approve: bool) -> String {
    format!(
        r"Current Sned Settings:

Provider:     {}
Model:        {}
Mode:         {}
Auto-approve: {}
",
        provider,
        model,
        mode,
        if auto_approve { "enabled" } else { "disabled" }
    )
}

/// Model picker entry for the /model command.
#[derive(Debug, Clone)]
pub struct ModelPickerEntry {
    pub provider: &'static str,
    pub model_id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// Build the list of available model picker entries.
#[must_use] 
pub fn build_model_picker_entries() -> Vec<ModelPickerEntry> {
    vec![
        ModelPickerEntry {
            provider: "anthropic",
            model_id: "claude-sonnet-4-20250514",
            label: "claude-sonnet-4-20250514",
            description: "Claude Sonnet 4",
        },
        ModelPickerEntry {
            provider: "anthropic",
            model_id: "claude-opus-4-20250514",
            label: "claude-opus-4-20250514",
            description: "Claude Opus 4",
        },
        ModelPickerEntry {
            provider: "anthropic",
            model_id: "claude-3-5-sonnet-20241022",
            label: "claude-3-5-sonnet-20241022",
            description: "Claude 3.5 Sonnet",
        },
        ModelPickerEntry {
            provider: "anthropic",
            model_id: "claude-3-5-haiku-20241022",
            label: "claude-3-5-haiku-20241022",
            description: "Claude 3.5 Haiku",
        },
        ModelPickerEntry {
            provider: "openai",
            model_id: "gpt-4o",
            label: "gpt-4o",
            description: "GPT-4o",
        },
        ModelPickerEntry {
            provider: "openai",
            model_id: "gpt-4o-mini",
            label: "gpt-4o-mini",
            description: "GPT-4o Mini",
        },
        ModelPickerEntry {
            provider: "openai",
            model_id: "o1-preview",
            label: "o1-preview",
            description: "O1 Preview",
        },
        ModelPickerEntry {
            provider: "openai",
            model_id: "o1-mini",
            label: "o1-mini",
            description: "O1 Mini",
        },
        ModelPickerEntry {
            provider: "minimax",
            model_id: "minimax-m2.7",
            label: "minimax-m2.7",
            description: "MiniMax M2.7",
        },
        ModelPickerEntry {
            provider: "gemini",
            model_id: "gemini-3.1-pro-preview",
            label: "gemini-3.1-pro-preview",
            description: "Gemini 3.1 Pro",
        },
        ModelPickerEntry {
            provider: "deepseek",
            model_id: "deepseek-chat",
            label: "deepseek-chat",
            description: "DeepSeek Chat",
        },
        ModelPickerEntry {
            provider: "openrouter",
            model_id: "anthropic/claude-sonnet-4.5",
            label: "anthropic/claude-sonnet-4.5",
            description: "OpenRouter - Claude Sonnet 4.5",
        },
    ]
}

/// Format available models text for /models command.
#[must_use] 
pub fn format_models_text() -> String {
    let entries = build_model_picker_entries();
    let mut s = String::new();
    s.push_str("Available Models:\n");
    let mut current_provider: Option<&str> = None;
    for entry in &entries {
        if Some(entry.provider) != current_provider {
            current_provider = Some(entry.provider);
            s.push_str(&format!("\n{}:\n", to_title_case(entry.provider)));
        }
        s.push_str(&format!("  {}\n", entry.label));
    }
    s
}

fn to_title_case(s: &str) -> String {
    // Special cases for provider names
    match s {
        "openai" => "OpenAI".to_string(),
        "minimax" => "MiniMax".to_string(),
        "deepseek" => "DeepSeek".to_string(),
        "openrouter" => "OpenRouter".to_string(),
        _ => {
            let mut c = s.chars();
            match c.next() {
                None => String::new(),
                Some(first) => {
                    let mut result = first.to_uppercase().collect::<String>();
                    result.extend(c.flat_map(char::to_lowercase));
                    result
                }
            }
        }
    }
}

#[must_use] 
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
            format!(" ({cache_writes}w/{cache_reads}r)")
        } else {
            String::new()
        };

        let reasoning_str = if reasoning > 0 {
            format!(" | {reasoning} reasoning")
        } else {
            String::new()
        };

        let cost_str = if cost > 0.0 {
            format!(" | ${cost:.4}")
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
#[must_use] 
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
            lines.push(format!("  + {path}"));
        }
    }

    if !edited.is_empty() {
        lines.push("Edited:".to_string());
        for path in &edited {
            lines.push(format!("  ~ {path}"));
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

const CONDENSE_INSTRUCTION: &str = r#"<explicit_instructions type="condense">
The user has explicitly asked you to create a detailed summary of the conversation so far, which will be used to compact the current context window while retaining key information. The user may have provided instructions or additional information for you to consider when summarizing the conversation.
Irrespective of whether additional information or instructions are given, you are only allowed to respond to this message by calling the condense tool.
Set the auto_accept parameter to true, because the user explicitly requested compaction and no extra confirmation is needed.

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
    fn test_inline_slash_command_is_plain_input() {
        let text = "Please /compact now";
        let result = parse_slash_command(text);
        assert_eq!(result.processed_text, text);
        assert!(result.command.is_none());
    }

    #[test]
    fn test_slash_command_inside_tags_not_detected() {
        let result = parse_slash_command("<task>/compact</task>");
        assert_eq!(result.processed_text, "<task>/compact</task>");
        assert!(result.command.is_none());
    }

    #[test]
    fn test_slash_command_with_numbers() {
        let result = parse_slash_command("/compact2");
        assert_eq!(result.processed_text, "");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "compact2");
    }

    #[test]
    fn test_slash_command_with_special_chars() {
        let result = parse_slash_command("/new-task_v2.alpha");
        assert_eq!(result.processed_text, "");
        let cmd = result.command.unwrap();
        assert_eq!(cmd.command, "new-task_v2.alpha");
    }

    #[test]
    fn test_slash_command_at_end_is_plain_input() {
        let text = "Please help me /compact";
        let result = parse_slash_command(text);
        assert_eq!(result.processed_text, text);
        assert!(result.command.is_none());
    }

    #[test]
    fn test_no_false_positive_in_url() {
        let result = parse_slash_command("Check http://example.com/compact");
        assert_eq!(result.processed_text, "Check http://example.com/compact");
        assert!(result.command.is_none());
    }

    #[test]
    fn test_no_false_positive_in_path() {
        let result = parse_slash_command("Path: /usr/local/compact");
        assert_eq!(result.processed_text, "Path: /usr/local/compact");
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
    fn test_process_compact_command() {
        let result = process_slash_command("/compact now");
        assert!(result.contains("<explicit_instructions type=\"condense\">"));
        assert!(result.contains("Set the auto_accept parameter to true"));
        assert!(result.contains("now"));
    }

    #[test]
    fn test_process_newrule_command() {
        let result = process_slash_command("/newrule");
        assert!(result.contains("<explicit_instructions type=\"new_rule\">"));
    }

    #[test]
    fn test_process_no_command() {
        let result = process_slash_command("Hello world");
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_unknown_command_remains_plain_input_outside_interactive_rejection() {
        assert_eq!(
            process_slash_command_with_context("/workflow do this", &[]),
            "/workflow do this"
        );
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
    fn test_is_compact_command_unknown() {
        assert!(!is_compact_command("/unknown"));
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
    fn test_parse_cli_only_retry() {
        let result = get_cli_only_command("/retry");
        assert_eq!(result, Some(CliOnlyCommand::Retry));
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
        let result = get_cli_only_command("/help compact");
        assert!(matches!(result, Some(CliOnlyCommand::HelpOption(ref cmd)) if cmd == "compact"));
    }

    #[test]
    fn test_format_help_for_command_compact() {
        let text = format_help_for_command("compact");
        assert!(text.contains("Help: /compact"));
        assert!(text.contains("Usage: /compact"));
        assert!(text.contains("Condenses the active model context"));
    }

    #[test]
    fn test_format_help_for_command_unknown() {
        let text = format_help_for_command("unknowncmd");
        assert!(text.contains("Unknown command"));
        assert!(text.contains("unknowncmd"));
    }

    #[test]
    fn test_clear_help_describes_display_only_semantics() {
        let text = format_help_for_command("clear");
        assert!(text.contains("visible display"));
        assert!(text.contains("Model context and saved conversation history are unchanged"));
        assert!(!text.contains("conversation cleared"));
    }

    #[test]
    fn test_format_help_for_command_retry() {
        let text = format_help_for_command("retry");
        assert!(text.contains("Retry the last safe failed request"));
        assert!(text.contains("before any tool execution"));
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
    fn test_removed_commands_are_not_available() {
        assert!(SlashCommand::parse("explain-changes").is_none());
        assert!(SlashCommand::parse("newtask").is_none());
        assert!(get_cli_only_command("/expand 1").is_none());
        assert!(SlashCommand::parse_with_skills("some-workflow", &[]).is_none());

        let entries = build_slash_command_entries(&[]);
        assert!(entries.iter().all(|entry| {
            !matches!(
                entry.name.as_str(),
                "explain-changes" | "expand" | "newtask"
            )
        }));
    }

    #[test]
    fn test_every_advertised_static_command_parses() {
        for spec in SLASH_COMMAND_SPECS {
            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                let invocation = format!("/{name}");
                let parsed = match spec.id {
                    SlashCommandId::Compact | SlashCommandId::NewRule => {
                        SlashCommand::parse(name).is_some()
                    }
                    _ => get_cli_only_command(&invocation).is_some(),
                };
                assert!(parsed, "advertised command did not parse: {invocation}");
            }
        }
    }

    #[test]
    fn test_static_command_names_and_aliases_are_unique() {
        let mut names = std::collections::HashSet::new();
        for spec in SLASH_COMMAND_SPECS {
            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                assert!(
                    names.insert(name.to_ascii_lowercase()),
                    "duplicate command name or alias: {name}"
                );
            }
        }
    }

    #[test]
    fn test_availability_hides_commands_without_runtime_state() {
        let entries =
            build_available_slash_command_entries(&[], SlashCommandAvailability::default());
        let names: std::collections::HashSet<&str> =
            entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains("plan"));
        assert!(!names.contains("undo"));
        assert!(!names.contains("retry"));
        assert!(!names.contains("resetcompact"));
        assert!(!names.contains("plan approve"));

        let pending_plan = build_available_slash_command_entries(
            &[],
            SlashCommandAvailability {
                track_changes: true,
                has_retryable_request: true,
                has_compacted_summary: true,
                has_plan: true,
                plan_mode: true,
                ..Default::default()
            },
        );
        let pending_names: std::collections::HashSet<&str> = pending_plan
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert!(pending_names.contains("undo"));
        assert!(pending_names.contains("retry"));
        assert!(pending_names.contains("resetcompact"));
        assert!(pending_names.contains("plan approve"));
        assert!(pending_names.contains("plan edit"));
        assert!(!pending_names.contains("plan pause"));
        assert!(!pending_names.contains("plan resume"));
    }

    #[test]
    fn test_static_commands_take_precedence_over_skill_names() {
        use crate::core::context::instructions::{SkillMetadata, SkillSource};

        let skills = vec![SkillMetadata {
            name: "help".to_string(),
            description: "Colliding skill".to_string(),
            path: "/tmp/skills/help/SKILL.md".to_string(),
            source: SkillSource::Project,
        }];
        assert!(SlashCommand::parse_with_skills("help", &skills).is_none());
        let entries = build_slash_command_entries(&skills);
        assert_eq!(
            entries.iter().filter(|entry| entry.name == "help").count(),
            1
        );
    }

    #[test]
    fn test_unknown_detection_only_rejects_leading_commands() {
        assert_eq!(
            unknown_leading_slash_command("/workflow now", &[]),
            Some("workflow".to_string())
        );
        assert!(unknown_leading_slash_command("please use /workflow now", &[]).is_none());
        assert!(unknown_leading_slash_command(" /workflow now", &[]).is_none());
        assert!(unknown_leading_slash_command("/compact", &[]).is_none());
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
    fn test_parse_copy_command() {
        assert_eq!(CliOnlyCommand::parse("copy"), Some(CliOnlyCommand::Copy));
    }

    #[test]
    fn test_parse_cli_only_no_command() {
        let result = get_cli_only_command("Hello world");
        assert_eq!(result, None);
    }

    #[test]
    fn test_format_help_text_contains_commands() {
        let text = format_help_text();
        assert!(!text.contains("/newtask"));
        assert!(text.contains("/compact"));
        assert!(text.contains("/exit"));
        assert!(text.contains("/clear"));
        assert!(text.contains("/copy"));
        assert!(text.contains("/history"));
        assert!(text.contains("/skills"));
        assert!(text.contains("/help"));
        assert!(!text.contains("/explain-changes"));
        assert!(!text.contains("/explain_changes"));
        assert!(!text.contains("/expand"));
    }

    #[test]
    fn test_format_help_text_contains_keyboard_shortcuts() {
        let text = format_help_text();
        assert!(text.contains("Keyboard Shortcuts"));
        assert!(text.contains("Ctrl+C"));
        assert!(text.contains("↑/↓"));
        assert!(text.contains("Enter"));
        assert!(text.contains("Esc"));
        assert!(text.contains("Shift+drag"));
    }

    #[test]
    fn test_format_help_text_contains_ansi_codes() {
        let text = format_help_text();
        assert!(text.contains("\x1b["));
        assert!(text.contains("\x1b[1m"));
        assert!(text.contains("\x1b[96m"));
        assert!(text.contains("\x1b[0m"));
    }

    #[test]
    fn test_format_help_text_is_generated_from_registry() {
        let text = format_help_text();
        assert!(text.contains("═══════ Sned Commands ═══════"));
        for spec in SLASH_COMMAND_SPECS {
            assert!(text.contains(spec.usage), "missing help for {}", spec.name);
        }
    }

    #[test]
    fn test_format_help_text_shows_aliases() {
        let text = format_help_text();
        assert!(text.contains("aliases: /quit, /q"));
        assert!(text.contains("aliases: /?"));
    }

    #[test]
    fn test_format_help_text_shows_track_changes_badge() {
        let text = format_help_text();
        assert!(text.contains("[requires --track-changes]"));
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
        let checkpoint_list = format_help_for_command("checkpoint list");
        assert!(checkpoint_list.contains("Aliases: /checkpoint-list"));

        let checkpoint_restore = format_help_for_command("checkpoint restore");
        assert!(checkpoint_restore.contains("Aliases: /checkpoint-restore"));

        let checkpoint_undo = format_help_for_command("checkpoint undo");
        assert!(checkpoint_undo.contains("Aliases: /checkpoint-undo"));

        let resetcompact = format_help_for_command("resetcompact");
        assert!(resetcompact.contains("Aliases: /clearcompact"));
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
    fn test_parse_with_skills_finds_skill() {
        use crate::core::context::instructions::{SkillMetadata, SkillSource};

        let skills = vec![SkillMetadata {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            path: "/tmp/skills/test-skill/SKILL.md".to_string(),
            source: SkillSource::Project,
        }];

        let result = SlashCommand::parse_with_skills("test-skill", &skills);

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

        let result = SlashCommand::parse_with_skills("unknown-cmd", &skills);

        assert!(result.is_none());
    }

    #[test]
    fn test_builtin_commands_still_work() {
        let result = SlashCommand::parse_with_skills("compact", &[]);

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(!cmd.is_skill_command());
        assert_eq!(cmd.instruction_block(), CONDENSE_INSTRUCTION);
    }

    #[test]
    fn test_skill_command_helper_methods() {
        let skill_cmd = SlashCommand::SkillCommand {
            name: "my-skill".to_string(),
        };
        assert!(skill_cmd.is_skill_command());
        assert_eq!(skill_cmd.skill_name(), Some("my-skill"));
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

        let result = SlashCommand::parse_with_skills("test-skill", &skills);

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.is_skill_command());
        assert_eq!(cmd.skill_name(), Some("test-skill"));
    }

    #[test]
    fn test_process_slash_command_with_skill_injects_content() {
        use crate::core::context::instructions::{SkillMetadata, SkillSource};

        let temp = tempfile::TempDir::new().unwrap();
        let skill_path = temp.path().join("SKILL.md");
        std::fs::write(
            &skill_path,
            "---\nname: test-skill\ndescription: A test skill\n---\nFollow this skill.",
        )
        .unwrap();
        let skills = vec![SkillMetadata {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            path: skill_path.to_string_lossy().into_owned(),
            source: SkillSource::Project,
        }];
        let text = "/test-skill do something";
        let result = process_slash_command_with_context(text, &skills);

        assert!(result.contains("<explicit_instructions type=\"skill\" name=\"test-skill\">"));
        assert!(result.contains("Follow this skill."));
        assert!(result.ends_with("do something"));
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

    // --- Plan Mode Tests (using CliOnlyCommand variants) ---

    #[test]
    fn test_parse_cli_only_plan_prompt() {
        let result = get_cli_only_command("/plan describe the project");
        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(matches!(cmd, CliOnlyCommand::PlanPrompt(ref x) if x == "describe the project"));
    }

    #[test]
    fn test_parse_cli_only_ignores_inline_plan_command() {
        let text = "what about action modes? we have /plan but need auto approval";
        assert!(get_cli_only_command(text).is_none());
        assert_eq!(process_slash_command_with_context(text, &[]), text);
    }

    #[test]
    fn test_parse_cli_only_plan_approve() {
        let result = get_cli_only_command("/plan approve");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::PlanApprove));
    }

    #[test]
    fn test_parse_cli_only_plan_pause() {
        let result = get_cli_only_command("/plan pause");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::PlanPause));
    }

    #[test]
    fn test_parse_cli_only_plan_resume() {
        let result = get_cli_only_command("/plan resume");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::PlanResume));
    }

    #[test]
    fn test_parse_cli_only_plan_abort() {
        let result = get_cli_only_command("/plan abort");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::PlanAbort));
    }

    #[test]
    fn test_parse_cli_only_plan_edit() {
        let result = get_cli_only_command("/plan edit 1 new description");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::Plan(_)));
    }

    #[test]
    fn test_parse_cli_only_plan_edit_preserves_multi_word_description() {
        let result = get_cli_only_command("/plan edit 2 update the README and tests");
        match result {
            Some(CliOnlyCommand::Plan(PlanSubcommand::Edit(step, desc))) => {
                assert_eq!(step, 2);
                assert_eq!(desc, "update the README and tests");
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_parse_cli_only_plan_add() {
        let result = get_cli_only_command("/plan add 0 new step");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::Plan(_)));
    }

    #[test]
    fn test_parse_cli_only_plan_add_preserves_multi_word_description() {
        let result = get_cli_only_command("/plan add 1 add a follow-up step");
        match result {
            Some(CliOnlyCommand::Plan(PlanSubcommand::Add(after, desc))) => {
                assert_eq!(after, 1);
                assert_eq!(desc, "add a follow-up step");
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_parse_cli_only_plan_remove() {
        let result = get_cli_only_command("/plan remove 1");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::Plan(_)));
    }

    #[test]
    fn test_parse_cli_only_plan_replace() {
        let result = get_cli_only_command("/plan replace 1. step one\n2. step two");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::Plan(_)));
    }

    #[test]
    fn test_parse_cli_only_plan_replace_preserves_full_text() {
        let result = get_cli_only_command("/plan replace 1. step one\n2. step two");
        match result {
            Some(CliOnlyCommand::Plan(PlanSubcommand::Replace(text))) => {
                assert_eq!(text, "1. step one\n2. step two");
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_extract_slash_query_empty() {
        assert!(extract_slash_query("").is_none());
    }

    #[test]
    fn test_extract_slash_query_no_slash() {
        assert!(extract_slash_query("hello world").is_none());
    }

    #[test]
    fn test_extract_slash_query_at_start() {
        assert_eq!(extract_slash_query("/compact").unwrap(), "compact");
    }

    #[test]
    fn test_extract_slash_query_after_whitespace_is_ignored() {
        assert!(extract_slash_query("hello /compact").is_none());
    }

    #[test]
    fn test_extract_slash_query_multiple_words_not_detected() {
        assert!(extract_slash_query("/compact do this").is_none());
    }

    #[test]
    fn test_extract_slash_query_with_inline_command_is_ignored() {
        assert!(extract_slash_query("/a /b").is_none());
    }

    #[test]
    fn test_apply_slash_completion_ignores_inline_query() {
        assert!(apply_slash_completion("please /pl", "plan").is_none());
    }

    #[test]
    fn test_apply_slash_completion_preserves_suffix() {
        let (text, cursor) = apply_slash_completion("/pl  ", "plan").unwrap();
        assert_eq!(text, "/plan  ");
        assert_eq!(cursor, "/plan".len());
    }

    #[test]
    fn test_filter_slash_commands_empty_query() {
        let entries = vec![
            SlashCommandEntry {
                name: "exit".to_string(),
                description: "Exit".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Local,
                requires_args: false,
            },
            SlashCommandEntry {
                name: "compact".to_string(),
                description: "Compact".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Agent,
                requires_args: false,
            },
        ];
        let results = filter_slash_commands(&entries, "");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_filter_slash_commands_exact_match() {
        let entries = vec![
            SlashCommandEntry {
                name: "exit".to_string(),
                description: "Exit".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Local,
                requires_args: false,
            },
            SlashCommandEntry {
                name: "compact".to_string(),
                description: "Compact".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Agent,
                requires_args: false,
            },
        ];
        let results = filter_slash_commands(&entries, "exit");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "exit");
    }

    #[test]
    fn test_filter_slash_commands_prefix_match() {
        let entries = vec![
            SlashCommandEntry {
                name: "exit".to_string(),
                description: "Exit".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Local,
                requires_args: false,
            },
            SlashCommandEntry {
                name: "execute".to_string(),
                description: "Execute".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Local,
                requires_args: false,
            },
        ];
        let results = filter_slash_commands(&entries, "ex");
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|e| e.name == "exit"));
        assert!(results.iter().any(|e| e.name == "execute"));
    }

    #[test]
    fn test_filter_slash_commands_alias_match() {
        let entries = vec![SlashCommandEntry {
            name: "exit".to_string(),
            description: "Exit".to_string(),
            aliases: vec!["q".to_string(), "quit".to_string()],
            category: SlashCommandCategory::Local,
            requires_args: false,
        }];
        let results = filter_slash_commands(&entries, "q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "exit");
    }

    #[test]
    fn test_filter_slash_commands_case_insensitive() {
        let entries = vec![SlashCommandEntry {
            name: "compact".to_string(),
            description: "Compact".to_string(),
            aliases: vec![],
            category: SlashCommandCategory::Agent,
            requires_args: false,
        }];
        let results = filter_slash_commands(&entries, "COMPACT");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "compact");
    }

    #[test]
    fn test_filter_slash_commands_no_match() {
        let entries = vec![SlashCommandEntry {
            name: "compact".to_string(),
            description: "Compact".to_string(),
            aliases: vec![],
            category: SlashCommandCategory::Agent,
            requires_args: false,
        }];
        let results = filter_slash_commands(&entries, "zzz");
        assert!(results.is_empty());
    }

    #[test]
    fn test_filter_slash_commands_description_match() {
        let entries = vec![SlashCommandEntry {
            name: "compact".to_string(),
            description: "Condense the current context".to_string(),
            aliases: vec![],
            category: SlashCommandCategory::Agent,
            requires_args: false,
        }];
        let results = filter_slash_commands(&entries, "condense");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "compact");
    }

    #[test]
    fn test_filter_slash_commands_exact_takes_priority() {
        let entries = vec![
            SlashCommandEntry {
                name: "compact".to_string(),
                description: "Compact".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Agent,
                requires_args: false,
            },
            SlashCommandEntry {
                name: "compact-all".to_string(),
                description: "Compact all".to_string(),
                aliases: vec![],
                category: SlashCommandCategory::Agent,
                requires_args: false,
            },
        ];
        let results = filter_slash_commands(&entries, "compact");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "compact");
    }

    #[test]
    fn test_filter_slash_commands_limit_substring() {
        let entries: Vec<SlashCommandEntry> = (0..20)
            .map(|i| SlashCommandEntry {
                name: format!("command{}", i),
                description: format!("Command {}", i),
                aliases: vec![],
                category: SlashCommandCategory::Local,
                requires_args: false,
            })
            .collect();
        // "xyz" doesn't match any name prefix, falls through to substring
        let results = filter_slash_commands(&entries, "xyz");
        assert_eq!(results.len(), 0);

        // "command" matches all 20 as prefix - no limit applied to prefix matches
        let results = filter_slash_commands(&entries, "command");
        assert_eq!(results.len(), 20);

        // Use a query that only matches via description substring
        let entries2: Vec<SlashCommandEntry> = (0..20)
            .map(|i| SlashCommandEntry {
                name: format!("cmd{}", i),
                description: format!("Special command {}", i),
                aliases: vec![],
                category: SlashCommandCategory::Local,
                requires_args: false,
            })
            .collect();
        // "Special" only matches in description (substring), limited to 10
        let results = filter_slash_commands(&entries2, "Special");
        assert_eq!(results.len(), 10);
    }

    #[test]
    fn test_slash_command_category_all_variants() {
        assert_eq!(format!("{:?}", SlashCommandCategory::Agent), "Agent");
        assert_eq!(format!("{:?}", SlashCommandCategory::Local), "Local");
        assert_eq!(format!("{:?}", SlashCommandCategory::Plan), "Plan");
        assert_eq!(format!("{:?}", SlashCommandCategory::Skill), "Skill");
    }

    #[test]
    fn test_parse_model_switch_command() {
        let result = CliOnlyCommand::parse_with_arg("model", "anthropic/claude-sonnet-4");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::ModelSwitch(_)));
    }

    #[test]
    fn test_model_switch_is_local_command() {
        let cmd = CliOnlyCommand::ModelSwitch("openai/gpt-4".to_string());
        assert!(cmd.is_local_command());
    }

    #[test]
    fn test_parse_model_switch_empty_arg() {
        let result = CliOnlyCommand::parse_with_arg("model", "");
        assert!(result.is_some());
        if let Some(CliOnlyCommand::ModelSwitch(s)) = result {
            assert!(s.is_empty());
        }
    }

    #[test]
    fn test_parse_model_switch_no_arg() {
        let result = CliOnlyCommand::parse("model");
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CliOnlyCommand::ModelSwitch(_)));
    }

    #[test]
    fn test_model_picker_entries_contains_providers() {
        let entries = build_model_picker_entries();
        assert!(entries.len() >= 12);

        let providers: Vec<&str> = entries.iter().map(|e| e.provider).collect();
        assert!(providers.contains(&"anthropic"));
        assert!(providers.contains(&"openai"));
        assert!(providers.contains(&"minimax"));
        assert!(providers.contains(&"gemini"));
        assert!(providers.contains(&"deepseek"));
        assert!(providers.contains(&"openrouter"));
    }

    #[test]
    fn test_model_picker_entry_has_model_id() {
        let entries = build_model_picker_entries();
        for entry in &entries {
            assert!(!entry.provider.is_empty(), "provider must not be empty");
            assert!(!entry.model_id.is_empty(), "model_id must not be empty");
            assert!(!entry.label.is_empty(), "label must not be empty");
            assert!(
                !entry.description.is_empty(),
                "description must not be empty"
            );
        }
    }

    #[test]
    fn test_format_models_text_has_all_providers() {
        let text = format_models_text();
        assert!(text.contains("Anthropic"));
        assert!(text.contains("OpenAI"));
        assert!(text.contains("MiniMax"));
        assert!(text.contains("Gemini"));
        assert!(text.contains("DeepSeek"));
        assert!(text.contains("OpenRouter"));
    }

    #[test]
    fn test_format_models_text_has_models() {
        let text = format_models_text();
        assert!(text.contains("claude-"));
        assert!(text.contains("gpt-"));
        assert!(text.contains("minimax-"));
        assert!(text.contains("gemini-"));
        assert!(text.contains("deepseek-"));
    }
}
