//! Tool definitions and JSON schema generation for sned CLI.
//!
//! Ports behavior from `dirac/src/core/prompts/system-prompt/spec.ts` and
//! `dirac/src/core/prompts/system-prompt/tools/*.ts`.

use crate::providers::{FunctionDefinition, ToolDefinition};

/// A parameter in a tool schema.
#[derive(Debug, Clone)]
pub struct ToolParameter {
    pub name: &'static str,
    pub required: bool,
    pub param_type: &'static str,
    pub description: &'static str,
    pub items: Option<serde_json::Value>,
    pub extra: Option<serde_json::Value>,
}

impl ToolParameter {
    /// Convert to a JSON schema property.
    #[must_use]
    pub fn to_schema_property(&self) -> serde_json::Value {
        let mut prop = serde_json::json!({
            "type": self.param_type,
            "description": self.description,
        });

        if let Some(items) = &self.items {
            prop["items"] = items.clone();
        }

        if let Some(extra) = &self.extra
            && let Some(obj) = extra.as_object()
        {
            for (key, value) in obj {
                prop[key] = value.clone();
            }
        }

        prop
    }
}

/// Schema for a tool.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Vec<ToolParameter>,
}

impl ToolSchema {
    /// Convert to a provider-native ToolDefinition (OpenAI format).
    #[must_use]
    pub fn to_tool_definition(&self) -> ToolDefinition {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for param in &self.parameters {
            properties.insert(param.name.to_string(), param.to_schema_property());
            if param.required {
                required.push(param.name.to_string());
            }
        }

        let parameters = if properties.is_empty() {
            serde_json::json!({
                "type": "object",
            })
        } else {
            serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            })
        };

        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: self.name.to_string(),
                description: self.description.to_string(),
                parameters,
            },
        }
    }
}

// ============================================================================
// Tool Schemas (ported from TypeScript source)
// ============================================================================

#[must_use]
pub fn read_file_schema() -> ToolSchema {
    ToolSchema {
        name: "read_file",
        description: "Read files/ranges as Word§source lines for edit_file. Copy exactly; prefixes distinguish duplicate occurrences. Above SNED_MAX_FILE_READ_SIZE (default 512KB), a ranged read also returns a complete sha256 revision for memory-bounded line-range editing; its Word§ anchors remain inspection-only.",
        parameters: vec![
            ToolParameter {
                name: "paths",
                required: true,
                param_type: "array",
                description: "Relative file paths.",
                items: Some(serde_json::json!({"type": "string"})),
                extra: None,
            },
            ToolParameter {
                name: "start_line",
                required: false,
                param_type: "integer",
                description: "First line (default 1).",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "end_line",
                required: false,
                param_type: "integer",
                description: "Last line (default EOF).",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn write_to_file_schema() -> ToolSchema {
    ToolSchema {
        name: "write_to_file",
        description: "Create/full-rewrite a file, creating parent directories. Overwrites existing content. For targeted changes use edit_file with read anchors; no shell/script bypass.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: true,
                param_type: "string",
                description: "Path relative to workspace root.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "content",
                required: true,
                param_type: "string",
                description: "Complete file content.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn list_files_schema() -> ToolSchema {
    ToolSchema {
        name: "list_files",
        description: "List files/directories as a tree with sizes and line counts.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: false,
                param_type: "string",
                description: "Directory relative to cwd (default cwd).",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "recursive",
                required: false,
                param_type: "boolean",
                description: "Recurse (default false).",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn search_files_schema() -> ToolSchema {
    ToolSchema {
        name: "search_files",
        description: "Search file contents by regex; returns paths, line numbers and context.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: false,
                param_type: "string",
                description: "Search directory relative to cwd (default cwd).",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "regex",
                required: true,
                param_type: "string",
                description: "Search regex.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "file_pattern",
                required: false,
                param_type: "string",
                description: "Glob pattern to filter files (e.g., '*.rs', '*.ts').",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn edit_file_schema() -> ToolSchema {
    ToolSchema {
        name: "edit_file",
        description: "Edit existing files; does not create files. Each files[] item must contain path and edits. Put anchor, edit_type, text, start_line, end_line, and expected_text inside an edits[] item, never directly alongside path. Use exact anchors from reads or successful edits. Batch independent edits from one snapshot. Untouched tracked anchors remain valid. For oversized files, use the complete sha256 revision from a ranged read with start_line/end_line/expected_text; Sned streams a same-directory atomic replacement. Failures withhold that file; other anchored files may apply. Reread only when recovery metadata requires it. Use write_to_file to create/full-rewrite.",
        parameters: vec![ToolParameter {
            name: "files",
            required: true,
            param_type: "array",
            description: "Files to edit.",
            items: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of an existing file relative to workspace root; create via write_to_file."
                    },
                    "expected_file_hash": {
                        "type": "string",
                        "description": "Optional complete sha256:<digest> from a large ranged read. When present, submit exactly one file and use line-range selectors instead of anchors."
                    },
                    "edits": {
                        "type": "array",
                        "description": "Edits for this file.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "edit_type": {
                                    "type": "string",
                                    "enum": ["replace", "insert_after", "insert_before"],
                                    "description": "Default replace. Inserts preserve anchor; wrap with range replace. Reject adjacent duplicates or insertions repeating anchor."
                                },
                                "anchor": {
                                    "type": "string",
                                    "description": "Start/insertion anchor: copy one complete Word§source line exactly from read_file, get_function, get_file_skeleton, or successful edit output. No newline; never paste a block here. Select a range with anchor and end_anchor, each containing one line."
                                },
                                "end_anchor": {
                                    "type": "string",
                                    "description": "Optional for a single-line replace; required for a range/fingerprint. Inclusive endpoint: copy one exact Word§source line from current read/edit output."
                                },
                                "start_line": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "1-based inclusive start line for revision-checked editing; use only with expected_file_hash."
                                },
                                "end_line": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "1-based inclusive end line. Defaults to start_line; insertions require one line."
                                },
                                "expected_text": {
                                    "type": "string",
                                    "description": "Exact selected logical lines joined with LF. Required with start_line; whitespace is significant."
                                },
                                "content": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Optional exact interior lines for a duplicate-line fingerprint. Use only with replace and end_anchor; include every line between them in exact order and whitespace. Limited to 4096 lines and 1 MiB."
                                },
                                "text": {
                                    "type": "string",
                                    "description": "Replacement source without Word§ prefixes; use \\n for new lines. Use an empty string to delete the inclusive anchor/end_anchor range. In insertions, leading and trailing blank lines count in duplicate checks."
                                }
                            },
                            "required": ["text"],
                            "anyOf": [
                                {"required": ["anchor"]},
                                {"required": ["start_line", "expected_text"]}
                            ]
                        }
                    }
                },
                "required": ["path", "edits"]
            })),
            extra: None,
        }],
    }
}

#[must_use]
pub fn execute_command_schema() -> ToolSchema {
    ToolSchema {
        name: "execute_command",
        description: "Executes CLI commands or scripts. Use commands as a literal JSON array of strings for simple sequences, not a string containing an array. Each commands[] entry runs in a fresh shell, so variables, cd, and other shell state do not persist between entries or tool calls; keep dependent statements in one multiline entry or use script. Use script for complex run-only logic. Use file tools for workspace changes rather than shell redirection, heredocs, or ad-hoc Python/sed rewrites. Provide exactly one of {commands, script}.",
        parameters: vec![
            ToolParameter {
                name: "commands",
                required: false,
                param_type: "array",
                description: "A literal JSON array of CLI command strings to execute in sequence. Each entry runs in a fresh shell; variables, cd, and other shell state do not persist between entries or tool calls. Keep dependent statements in one multiline entry or use script. Do not encode the array as a string.",
                items: Some(serde_json::json!({"type": "string"})),
                extra: None,
            },
            ToolParameter {
                name: "script",
                required: false,
                param_type: "string",
                description: "A script to execute for complex run-only logic or non-shell languages. Use write_to_file or edit_file for workspace file changes.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "language",
                required: false,
                param_type: "string",
                description: "The language of the script (e.g., 'bash', 'python', 'node'). Defaults to 'bash'.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "raw_output",
                required: false,
                param_type: "boolean",
                description: "If true, preserve carriage-return progress output in command results. Defaults to false.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn ask_followup_question_schema() -> ToolSchema {
    ToolSchema {
        name: "ask_followup_question",
        description: "Ask the user for clarification.",
        parameters: vec![ToolParameter {
            name: "question",
            required: true,
            param_type: "string",
            description: "Question.",
            items: None,
            extra: None,
        }],
    }
}

#[must_use]
pub fn attempt_completion_schema() -> ToolSchema {
    ToolSchema {
        name: "attempt_completion",
        description: "Present the final result when the task is complete.",
        parameters: vec![
            ToolParameter {
                name: "result",
                required: true,
                param_type: "string",
                description: "Completion summary.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "command",
                required: false,
                param_type: "string",
                description: "Optional demonstration command.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn plan_mode_respond_schema() -> ToolSchema {
    ToolSchema {
        name: "plan_mode_respond",
        description: "Respond to the user in plan mode. Use this when you need to present a plan or ask for confirmation before proceeding.",
        parameters: vec![
            ToolParameter {
                name: "response",
                required: true,
                param_type: "string",
                description: "Your response to the user.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "needs_more_exploration",
                required: false,
                param_type: "boolean",
                description: "Set to true if you need more exploration before finalizing the plan.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn get_function_schema() -> ToolSchema {
    ToolSchema {
        name: "get_function",
        description: "Get function/method code with editable anchors. Identical snapshots reuse identities; mandatory read_file recovery stays required. Above 5000 source lines, anchors expire after any edit. Oversized files are inspection-only.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: true,
                param_type: "string",
                description: "File path.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "name",
                required: true,
                param_type: "string",
                description: "Function/method name.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn get_file_skeleton_schema() -> ToolSchema {
    ToolSchema {
        name: "get_file_skeleton",
        description: "Get definitions/signatures with anchors for shown lines. Identical snapshots reuse identities; mandatory read_file recovery stays required. Above 5000 source lines, anchors expire after any edit. Oversized files are inspection-only.",
        parameters: vec![ToolParameter {
            name: "path",
            required: true,
            param_type: "string",
            description: "File path.",
            items: None,
            extra: None,
        }],
    }
}

#[must_use]
pub fn find_symbol_references_schema() -> ToolSchema {
    ToolSchema {
        name: "find_symbol_references",
        description: "Find definitions/references with canonical anchors. Supply paths for direct parsing; omit paths to use a ready index.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: false,
                param_type: "string",
                description: "Direct search path; omit only with a ready, enabled index.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "paths",
                required: false,
                param_type: "array",
                description: "Direct paths; merged with indexed hits when enabled.",
                items: Some(serde_json::json!({"type": "string"})),
                extra: None,
            },
            ToolParameter {
                name: "name",
                required: true,
                param_type: "string",
                description: "Symbol name.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn replace_symbol_schema() -> ToolSchema {
    ToolSchema {
        name: "replace_symbol",
        description: "Replace all occurrences of a symbol with a new name across the codebase.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: true,
                param_type: "string",
                description: "Definition file path.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "old_name",
                required: true,
                param_type: "string",
                description: "Current symbol name.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "new_name",
                required: true,
                param_type: "string",
                description: "New symbol name.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn rename_symbol_schema() -> ToolSchema {
    ToolSchema {
        name: "rename_symbol",
        description: "Rename all AST-resolved occurrences in specified files/directories. Prefer for symbol renames.",
        parameters: vec![
            ToolParameter {
                name: "paths",
                required: true,
                param_type: "array",
                description: "Relative file/directory paths.",
                items: Some(serde_json::json!({"type": "string"})),
                extra: None,
            },
            ToolParameter {
                name: "existing_symbol",
                required: true,
                param_type: "string",
                description: "Exact current symbol name.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "new_symbol",
                required: true,
                param_type: "string",
                description: "New symbol name.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn use_subagents_schema() -> ToolSchema {
    ToolSchema {
        name: "use_subagents",
        description: "Run between one and five focused in-process subagents in parallel. Each subagent gets its own prompt and returns a comprehensive research result. Default timeout is 300 seconds. Particularly effective for investigating multiple independent paths simultaneously without consuming your context window.",
        parameters: vec![
            ToolParameter {
                name: "prompt_1",
                required: true,
                param_type: "string",
                description: "First subagent prompt.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "prompt_2",
                required: false,
                param_type: "string",
                description: "Second subagent prompt.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "prompt_3",
                required: false,
                param_type: "string",
                description: "Optional third subagent prompt.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "prompt_4",
                required: false,
                param_type: "string",
                description: "Optional fourth subagent prompt.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "prompt_5",
                required: false,
                param_type: "string",
                description: "Optional fifth subagent prompt.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "timeout",
                required: false,
                param_type: "integer",
                description: "Optional timeout in seconds for each subagent. Defaults to 300 seconds.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "max_turns",
                required: false,
                param_type: "integer",
                description: "Optional maximum number of turns for each subagent.",
                items: None,
                extra: None,
            },
        ],
    }
}

#[must_use]
pub fn use_skill_schema() -> ToolSchema {
    ToolSchema {
        name: "use_skill",
        description: "Load and activate a skill by name. Skills provide specialized instructions for specific tasks. Use this tool ONCE when a user's request matches one of the available skill descriptions shown in the SKILLS section of your system prompt. After activation, follow the skill's instructions directly - do not call use_skill again.",
        parameters: vec![ToolParameter {
            name: "skill_name",
            required: true,
            param_type: "string",
            description: "The name of the skill to activate (must match exactly one of the available skill names).",
            items: None,
            extra: None,
        }],
    }
}

#[must_use]
pub fn list_skills_schema() -> ToolSchema {
    ToolSchema {
        name: "list_skills",
        description: "List all available skills and their descriptions. Use this to discover specialized capabilities when the initial list in the system prompt is truncated.",
        parameters: vec![],
    }
}

#[must_use]
pub fn diagnostics_scan_schema() -> ToolSchema {
    ToolSchema {
        name: "diagnostics_scan",
        description: "Runs diagnostics (linter and syntax checks) on the specified files and returns the results. This is useful for checking if recent changes introduced any errors or for getting a summary of existing problems in specific files.",
        parameters: vec![ToolParameter {
            name: "paths",
            required: true,
            param_type: "array",
            description: "An array of relative paths to the files to scan.",
            items: Some(serde_json::json!({"type": "string"})),
            extra: None,
        }],
    }
}

#[must_use]
pub fn condense_schema() -> ToolSchema {
    ToolSchema {
        name: "condense",
        description: "Create a detailed summary of the conversation so far, which will be used to compact the context window while retaining key information.",
        parameters: vec![ToolParameter {
            name: "context",
            required: true,
            param_type: "string",
            description: "Detailed summary of the conversation so far, including current work, technical concepts, modified files, problems solved, and exact pending next steps. If applicable based on the current task, this should include previous conversation, current work, key technical concepts, relevant files and code, problem solving, and pending tasks.",
            items: None,
            extra: None,
        }],
    }
}

#[must_use]
pub fn web_fetch_schema() -> ToolSchema {
    ToolSchema {
        name: "web_fetch",
        description: "Fetch web pages via HTTP and convert HTML to readable text. Includes SSRF protection and URL validation.",
        parameters: vec![ToolParameter {
            name: "url",
            required: true,
            param_type: "string",
            description: "URL to fetch (http:// or https:// only). Private IPs, localhost, and cloud metadata endpoints are blocked.",
            items: None,
            extra: None,
        }],
    }
}

// ============================================================================
// Active Tool Definitions
// ============================================================================

use super::SnedTool;

/// Returns the tool schema for a given SnedTool variant.
///
/// All SnedTool variants are explicitly matched. The compiler will warn if a
/// new variant is added without a corresponding schema function.
#[must_use]
pub fn get_tool_schema(tool: SnedTool) -> ToolSchema {
    match tool {
        SnedTool::ReadFile => read_file_schema(),
        SnedTool::WriteToFile => write_to_file_schema(),
        SnedTool::ListFiles => list_files_schema(),
        SnedTool::SearchFiles => search_files_schema(),
        SnedTool::EditFile => edit_file_schema(),
        SnedTool::ExecuteCommand => execute_command_schema(),
        SnedTool::AskFollowupQuestion => ask_followup_question_schema(),
        SnedTool::AttemptCompletion => attempt_completion_schema(),
        SnedTool::PlanModeRespond => plan_mode_respond_schema(),
        SnedTool::GetFunction => get_function_schema(),
        SnedTool::GetFileSkeleton => get_file_skeleton_schema(),
        SnedTool::FindSymbolReferences => find_symbol_references_schema(),
        SnedTool::ReplaceSymbol => replace_symbol_schema(),
        SnedTool::RenameSymbol => rename_symbol_schema(),
        SnedTool::UseSubagents => use_subagents_schema(),
        SnedTool::UseSkill => use_skill_schema(),
        SnedTool::ListSkills => list_skills_schema(),
        SnedTool::DiagnosticsScan => diagnostics_scan_schema(),
        SnedTool::Condense => condense_schema(),
        SnedTool::WebFetch => web_fetch_schema(),
    }
}

/// Returns ToolDefinitions for all active (kept) tools.
#[must_use]
pub fn get_active_tool_definitions() -> Vec<ToolDefinition> {
    get_tool_definitions_for_profile(ToolProfile::Full)
}

// ============================================================================
// Tool Profiles — Adaptive tool sets for request shaping
// ============================================================================

/// Adaptive tool profiles that control how many tool schemas are sent to the
/// model. Sending fewer tools on simple prompts reduces input tokens and
/// model-side planning, which directly reduces wall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolProfile {
    DirectAnswer,
    AnswerOnly,
    WriteOnly,
    CoreEdit,
    Validate,
    Symbol,
    Plan,
    Full,
}

impl ToolProfile {
    /// Returns the next larger profile for recovery when a reduced tier
    /// causes the model to produce a text-only non-completion response.
    #[must_use]
    pub fn escalate(self) -> Option<Self> {
        match self {
            Self::DirectAnswer => Some(Self::AnswerOnly),
            Self::AnswerOnly => Some(Self::WriteOnly),
            Self::WriteOnly => Some(Self::CoreEdit),
            Self::CoreEdit | Self::Validate | Self::Symbol | Self::Plan | Self::Full => None,
        }
    }

    #[must_use]
    pub fn tools(self) -> &'static [SnedTool] {
        match self {
            Self::DirectAnswer => &[],
            Self::AnswerOnly => &[SnedTool::AttemptCompletion, SnedTool::AskFollowupQuestion],
            Self::WriteOnly => &[
                SnedTool::WriteToFile,
                SnedTool::AttemptCompletion,
                SnedTool::AskFollowupQuestion,
            ],
            Self::CoreEdit => &[
                SnedTool::ReadFile,
                SnedTool::EditFile,
                SnedTool::WriteToFile,
                SnedTool::ListFiles,
                SnedTool::SearchFiles,
                SnedTool::AttemptCompletion,
                SnedTool::AskFollowupQuestion,
            ],
            Self::Validate => &[
                SnedTool::ReadFile,
                SnedTool::EditFile,
                SnedTool::WriteToFile,
                SnedTool::ListFiles,
                SnedTool::SearchFiles,
                SnedTool::ExecuteCommand,
                SnedTool::DiagnosticsScan,
                SnedTool::AttemptCompletion,
                SnedTool::AskFollowupQuestion,
            ],
            Self::Symbol => &[
                SnedTool::ReadFile,
                SnedTool::EditFile,
                SnedTool::WriteToFile,
                SnedTool::ListFiles,
                SnedTool::SearchFiles,
                SnedTool::GetFunction,
                SnedTool::GetFileSkeleton,
                SnedTool::FindSymbolReferences,
                SnedTool::RenameSymbol,
                SnedTool::ReplaceSymbol,
                SnedTool::AttemptCompletion,
                SnedTool::AskFollowupQuestion,
            ],
            Self::Plan => &[
                SnedTool::ReadFile,
                SnedTool::ListFiles,
                SnedTool::SearchFiles,
                SnedTool::ExecuteCommand,
                SnedTool::AskFollowupQuestion,
                SnedTool::PlanModeRespond,
                SnedTool::GetFunction,
                SnedTool::GetFileSkeleton,
                SnedTool::FindSymbolReferences,
                SnedTool::ReplaceSymbol,
                SnedTool::RenameSymbol,
                SnedTool::UseSubagents,
                SnedTool::UseSkill,
                SnedTool::ListSkills,
                SnedTool::DiagnosticsScan,
                SnedTool::Condense,
                SnedTool::WebFetch,
            ],
            Self::Full => &[
                SnedTool::ReadFile,
                SnedTool::WriteToFile,
                SnedTool::ListFiles,
                SnedTool::SearchFiles,
                SnedTool::EditFile,
                SnedTool::ExecuteCommand,
                SnedTool::AskFollowupQuestion,
                SnedTool::AttemptCompletion,
                SnedTool::PlanModeRespond,
                SnedTool::GetFunction,
                SnedTool::GetFileSkeleton,
                SnedTool::FindSymbolReferences,
                SnedTool::ReplaceSymbol,
                SnedTool::RenameSymbol,
                SnedTool::UseSubagents,
                SnedTool::UseSkill,
                SnedTool::ListSkills,
                SnedTool::DiagnosticsScan,
                SnedTool::Condense,
                SnedTool::WebFetch,
            ],
        }
    }
}

/// Returns ToolDefinitions for a given tool profile.
#[must_use]
pub fn get_tool_definitions_for_profile(profile: ToolProfile) -> Vec<ToolDefinition> {
    profile
        .tools()
        .iter()
        .map(|&t| get_tool_schema(t).to_tool_definition())
        .collect()
}

/// Selects a tool profile based on the user prompt and mode.
///
/// Heuristic classification:
/// - DirectAnswer: obvious answer-only prompts (math, factual, greetings)
/// - WriteOnly: "create/write/generate file(s)" without mention of existing code
/// - CoreEdit: "modify/fix/refactor/patch" existing files
/// - Validate: tasks requesting validation (run tests, lint, check)
/// - Symbol: rename/reference/symbol tasks
/// - Full: anything complex, multi-step, or ambiguous
#[must_use]
pub fn select_tool_profile(prompt: &str, mode: &str) -> ToolProfile {
    let lower = prompt.to_lowercase();

    if mode == "plan" {
        return ToolProfile::Plan;
    }

    let is_answer_only = is_answer_only_prompt(&lower);
    let is_write_only = is_write_only_prompt(&lower);
    let is_edit_task = is_edit_prompt(&lower);
    let is_validate_task = is_validate_prompt(&lower);
    let is_symbol_task = is_symbol_prompt(&lower);

    if is_symbol_task && !is_edit_task && !is_write_only {
        return ToolProfile::Symbol;
    }
    if is_validate_task && !is_write_only {
        return ToolProfile::Validate;
    }
    if is_edit_task && !is_write_only {
        return ToolProfile::CoreEdit;
    }
    if is_write_only && !is_edit_task {
        return ToolProfile::WriteOnly;
    }
    if is_answer_only && !is_write_only && !is_edit_task {
        return ToolProfile::DirectAnswer;
    }

    ToolProfile::Full
}

fn is_answer_only_prompt(lower: &str) -> bool {
    let short = lower.len() < 120;
    let has_file_keywords = lower.contains("file")
        || lower.contains("create")
        || lower.contains("write")
        || lower.contains("generate")
        || lower.contains("implement")
        || lower.contains("build")
        || lower.contains("refactor")
        || lower.contains("fix")
        || lower.contains("debug")
        || lower.contains("patch")
        || lower.contains("modify");

    if !short || has_file_keywords {
        return false;
    }

    let math_like = lower.contains("what is ")
        || lower.contains("calculate")
        || lower.contains("compute")
        || lower.contains(" + ")
        || lower.contains(" - ")
        || lower.contains(" * ")
        || lower.contains(" / ")
        || lower.contains("how many")
        || lower.contains("how much");

    let factual = lower.starts_with("who ")
        || lower.starts_with("what ")
        || lower.starts_with("where ")
        || lower.starts_with("when ")
        || lower.starts_with("why ")
        || lower.starts_with("is ")
        || lower.starts_with("are ")
        || lower.starts_with("does ")
        || lower.starts_with("can ")
        || lower.starts_with("explain ")
        || lower.starts_with("define ")
        || lower.starts_with("describe ")
        || lower.starts_with("tell me ");

    math_like || factual
}

fn is_write_only_prompt(lower: &str) -> bool {
    let has_create = lower.contains("create")
        || lower.contains("write")
        || lower.contains("generate")
        || lower.contains("scaffold");
    let has_file = lower.contains("file")
        || lower.contains(".py")
        || lower.contains(".rs")
        || lower.contains(".ts")
        || lower.contains(".js")
        || lower.contains(".go")
        || lower.contains(".java")
        || lower.contains("source file")
        || lower.contains("named `");

    let has_edit_keywords = lower.contains("modify")
        || lower.contains("edit")
        || lower.contains("fix")
        || lower.contains("refactor")
        || lower.contains("patch")
        || lower.contains("update")
        || lower.contains("change existing")
        || lower.contains("in the existing")
        || lower.contains("read_file")
        || lower.contains("edit_file");

    has_create && has_file && !has_edit_keywords
}

fn is_edit_prompt(lower: &str) -> bool {
    lower.contains("modify")
        || lower.contains("edit ")
        || lower.contains("fix ")
        || lower.contains("refactor")
        || lower.contains("patch")
        || lower.contains("update the")
        || lower.contains("change the")
        || lower.contains("change existing")
        || lower.contains("in the existing")
        || lower.contains("update existing")
}

fn is_validate_prompt(lower: &str) -> bool {
    lower.contains("run test")
        || lower.contains("run the test")
        || lower.contains("lint")
        || lower.contains("check if")
        || lower.contains("validate")
        || lower.contains("verify")
        || lower.contains("diagnostics")
}

fn is_symbol_prompt(lower: &str) -> bool {
    lower.contains("rename ")
        || lower.contains("find all references")
        || lower.contains("find references")
        || lower.contains("symbol reference")
        || lower.contains("replace symbol")
        || lower.contains("rename symbol")
        || lower.contains("find where ")
            && (lower.contains("used") || lower.contains("called") || lower.contains("referenced"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_file_definition() {
        let def = read_file_schema().to_tool_definition();
        assert_eq!(def.function.name, "read_file");
        assert_eq!(def.tool_type, "function");

        let params = def.function.parameters.as_object().unwrap();
        assert!(params.contains_key("properties"));
        assert!(params.contains_key("required"));

        let required = params["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("paths")));
        assert!(!required.contains(&serde_json::json!("start_line")));
    }

    #[test]
    fn test_edit_file_definition() {
        let schema = edit_file_schema();
        assert!(schema.description.contains("Edit existing files"));
        assert!(schema.description.contains("does not create files"));
        assert!(schema.description.contains("write_to_file"));

        let files = schema
            .parameters
            .iter()
            .find(|parameter| parameter.name == "files")
            .expect("edit_file should expose files");
        let path_description = files
            .items
            .as_ref()
            .and_then(|items| items.pointer("/properties/path/description"))
            .and_then(serde_json::Value::as_str)
            .expect("edit_file file entries should describe path");
        assert!(path_description.contains("existing file"));
        assert!(path_description.contains("write_to_file"));

        let def = schema.to_tool_definition();
        assert_eq!(def.function.name, "edit_file");

        let params = def.function.parameters.as_object().unwrap();
        let properties = params["properties"].as_object().unwrap();
        assert!(properties.contains_key("files"));

        let files_param = properties["files"].as_object().unwrap();
        assert_eq!(files_param["type"], "array");
    }

    #[test]
    fn edit_file_schema_describes_optional_single_line_and_fingerprint_fields() {
        let schema = edit_file_schema();
        let files = schema
            .parameters
            .iter()
            .find(|parameter| parameter.name == "files")
            .expect("edit_file should expose files");
        let edit = files
            .items
            .as_ref()
            .and_then(|items| items.pointer("/properties/edits/items"))
            .expect("edit_file should expose edit items");
        let properties = edit["properties"].as_object().expect("edit properties");
        let required = edit["required"].as_array().expect("required fields");

        assert_eq!(properties["content"]["type"], "array");
        assert!(
            properties["end_anchor"]["description"]
                .as_str()
                .is_some_and(
                    |description| description.contains("Optional for a single-line replace")
                )
        );
        assert!(!required.iter().any(|field| field == "edit_type"));
        assert!(!required.iter().any(|field| field == "anchor"));
        assert!(required.iter().any(|field| field == "text"));
        assert_eq!(properties["start_line"]["minimum"], 1);
        assert_eq!(properties["expected_text"]["type"], "string");
        assert_eq!(edit["anyOf"][0]["required"][0], "anchor");
        assert_eq!(edit["anyOf"][1]["required"][0], "start_line");
        assert!(
            schema
                .description
                .contains("from reads or successful edits")
        );
        assert!(
            schema
                .description
                .contains("Untouched tracked anchors remain valid")
        );
        assert!(schema.description.contains("revision from a ranged read"));
        assert!(schema.description.contains("recovery metadata requires it"));
    }

    #[test]
    fn edit_file_schema_keeps_edit_fields_inside_nested_edits_array() {
        let schema = edit_file_schema();
        assert!(
            schema
                .description
                .contains("files[] item must contain path and edits")
        );
        assert!(schema.description.contains(
            "anchor, edit_type, text, start_line, end_line, and expected_text inside an edits[] item"
        ));
        assert!(schema.description.contains("never directly alongside path"));

        let file_items = schema.parameters[0]
            .items
            .as_ref()
            .expect("files should define item schema");
        assert_eq!(file_items["required"], serde_json::json!(["path", "edits"]));

        let file_properties = file_items["properties"]
            .as_object()
            .expect("file items should define properties");
        for misplaced in [
            "anchor",
            "edit_type",
            "text",
            "start_line",
            "end_line",
            "expected_text",
        ] {
            assert!(!file_properties.contains_key(misplaced));
        }

        let edit_properties = file_properties["edits"]["items"]["properties"]
            .as_object()
            .expect("edits should define item properties");
        for nested in [
            "anchor",
            "edit_type",
            "text",
            "start_line",
            "end_line",
            "expected_text",
        ] {
            assert!(edit_properties.contains_key(nested));
        }
    }

    #[test]
    fn test_write_to_file_schema_mentions_workspace_root() {
        let schema = write_to_file_schema();
        let path_param = schema
            .parameters
            .iter()
            .find(|param| param.name == "path")
            .expect("write_to_file should expose a path parameter");

        assert!(
            path_param.description.contains("workspace root"),
            "write_to_file schema should describe paths relative to the workspace root"
        );
    }

    #[test]
    fn test_active_tools_count() {
        let defs = get_active_tool_definitions();
        assert_eq!(defs.len(), 20);
    }

    #[test]
    fn test_condense_is_in_active_tool_definitions() {
        // Regression guard: a model may believe it has no `condense` tool
        // if the tool is silently dropped from the active set. The full
        // profile is what gets sent to OpenAI-compatible providers, so
        // `condense` must be present there.
        let defs = get_active_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(
            names.contains(&"condense"),
            "condense tool must be in active tool definitions so the model can call it; got: {:?}",
            names
        );
    }

    #[test]
    fn test_condense_schema_omits_interactive_approval_parameter() {
        let schema = condense_schema();
        assert_eq!(schema.parameters.len(), 1);
        assert_eq!(schema.parameters[0].name, "context");
        assert!(
            schema
                .parameters
                .iter()
                .all(|parameter| parameter.name != "auto_accept")
        );
    }

    #[test]
    fn test_use_subagents_schema_omits_unsupported_history() {
        let schema = use_subagents_schema();

        assert!(
            schema
                .parameters
                .iter()
                .all(|param| param.name != "include_history")
        );
    }

    #[test]
    fn test_tool_schema_sizes() {
        let defs = get_active_tool_definitions();
        let mut entries: Vec<(String, usize)> = defs
            .iter()
            .map(|d| {
                let json_len = serde_json::to_string(d).unwrap().len();
                (d.function.name.clone(), json_len)
            })
            .collect();
        entries.sort_by_key(|b| std::cmp::Reverse(b.1));
        let total: usize = entries.iter().map(|(_, s)| s).sum();
        eprintln!("\nTool schema sizes (sorted by JSON bytes):");
        for (name, size) in &entries {
            let pct = (*size as f64 / total as f64) * 100.0;
            eprintln!("  {:30} {:6} bytes ({:5.1}%)", name, size, pct);
        }
        eprintln!("  {:30} {:6} bytes", "TOTAL", total);
        let sys_prompt_approx = 1500;
        eprintln!(
            "\n  System prompt (approx):         {:6} bytes",
            sys_prompt_approx
        );
        eprintln!(
            "  Tool schemas as % of prompt:    {:5.1}%",
            (total as f64 / (total + sys_prompt_approx) as f64) * 100.0
        );
    }

    #[test]
    fn test_tool_name_consistency() {
        let defs = get_active_tool_definitions();
        for def in &defs {
            assert!(!def.function.name.is_empty());
            assert!(!def.function.description.is_empty());
            assert_eq!(def.tool_type, "function");
        }
    }

    #[test]
    fn test_tool_profile_tool_counts() {
        assert_eq!(ToolProfile::DirectAnswer.tools().len(), 0);
        assert_eq!(ToolProfile::AnswerOnly.tools().len(), 2);
        assert_eq!(ToolProfile::WriteOnly.tools().len(), 3);
        assert!(ToolProfile::CoreEdit.tools().len() >= 7);
        assert!(ToolProfile::Validate.tools().len() >= 9);
        assert!(ToolProfile::Symbol.tools().len() >= 9);
        assert_eq!(ToolProfile::Plan.tools().len(), 17);
        assert_eq!(ToolProfile::Full.tools().len(), 20);
    }

    #[test]
    fn test_tool_profile_byte_budgets() {
        let direct = get_tool_definitions_for_profile(ToolProfile::DirectAnswer);
        let answer = get_tool_definitions_for_profile(ToolProfile::AnswerOnly);
        let write = get_tool_definitions_for_profile(ToolProfile::WriteOnly);
        let core = get_tool_definitions_for_profile(ToolProfile::CoreEdit);
        let validate = get_tool_definitions_for_profile(ToolProfile::Validate);
        let symbol = get_tool_definitions_for_profile(ToolProfile::Symbol);
        let full = get_tool_definitions_for_profile(ToolProfile::Full);

        let json_bytes = |defs: &Vec<ToolDefinition>| -> usize {
            defs.iter()
                .map(|d| serde_json::to_string(d).unwrap().len())
                .sum()
        };

        let direct_bytes = json_bytes(&direct);
        let answer_bytes = json_bytes(&answer);
        let write_bytes = json_bytes(&write);
        let core_bytes = json_bytes(&core);
        let validate_bytes = json_bytes(&validate);
        let symbol_bytes = json_bytes(&symbol);
        let full_bytes = json_bytes(&full);

        eprintln!("\nTool profile byte budgets:");
        eprintln!(
            "  DirectAnswer:  {} bytes ({} tools)",
            direct_bytes,
            direct.len()
        );
        eprintln!(
            "  AnswerOnly:   {} bytes ({} tools)",
            answer_bytes,
            answer.len()
        );
        eprintln!(
            "  WriteOnly:    {} bytes ({} tools)",
            write_bytes,
            write.len()
        );
        eprintln!(
            "  CoreEdit:     {} bytes ({} tools)",
            core_bytes,
            core.len()
        );
        eprintln!(
            "  Validate:     {} bytes ({} tools)",
            validate_bytes,
            validate.len()
        );
        eprintln!(
            "  Symbol:       {} bytes ({} tools)",
            symbol_bytes,
            symbol.len()
        );
        eprintln!(
            "  Full:         {} bytes ({} tools)",
            full_bytes,
            full.len()
        );

        assert_eq!(direct_bytes, 0, "DirectAnswer should have 0 bytes");
        assert!(
            answer_bytes < 1200,
            "AnswerOnly should be under 1200 bytes: got {}",
            answer_bytes
        );
        assert!(
            write_bytes < 2000,
            "WriteOnly should be under 2000 bytes: got {}",
            write_bytes
        );
        assert!(
            core_bytes < 8000,
            "CoreEdit should be under 8000 bytes: got {}",
            core_bytes
        );
        assert!(
            validate_bytes < 10000,
            "Validate should be under 10000 bytes: got {}",
            validate_bytes
        );
        assert!(
            symbol_bytes < 9000,
            "Symbol should be under 9000 bytes: got {}",
            symbol_bytes
        );
        assert!(
            full_bytes > 10000,
            "Full should be over 10000 bytes: got {}",
            full_bytes
        );

        assert!(direct_bytes < answer_bytes);
        assert!(answer_bytes < write_bytes);
        assert!(write_bytes < core_bytes);
        assert!(core_bytes <= validate_bytes);
        assert!(validate_bytes <= full_bytes);
    }

    #[test]
    fn test_tool_profile_escalation() {
        assert_eq!(
            ToolProfile::DirectAnswer.escalate(),
            Some(ToolProfile::AnswerOnly)
        );
        assert_eq!(
            ToolProfile::AnswerOnly.escalate(),
            Some(ToolProfile::WriteOnly)
        );
        assert_eq!(
            ToolProfile::WriteOnly.escalate(),
            Some(ToolProfile::CoreEdit)
        );
        // Escalation caps at CoreEdit — Validate and Full include execute_command
        assert_eq!(ToolProfile::CoreEdit.escalate(), None);
        assert_eq!(ToolProfile::Validate.escalate(), None);
        assert_eq!(ToolProfile::Full.escalate(), None);
        assert_eq!(ToolProfile::Symbol.escalate(), None);
        assert_eq!(ToolProfile::Plan.escalate(), None);
    }

    #[test]
    fn test_select_tool_profile_direct_answer() {
        assert_eq!(
            select_tool_profile("What is 2 + 2?", "act"),
            ToolProfile::DirectAnswer
        );
        assert_eq!(
            select_tool_profile("how many legs does a dog have?", "act"),
            ToolProfile::DirectAnswer
        );
        assert_eq!(
            select_tool_profile("is Rust memory safe?", "act"),
            ToolProfile::DirectAnswer
        );
    }

    #[test]
    fn test_select_tool_profile_write_only() {
        assert_eq!(
            select_tool_profile("Create a Rust source file named `even_sum.rs`.", "act"),
            ToolProfile::WriteOnly
        );
        assert_eq!(
            select_tool_profile(
                "Create exactly these source files in the current directory:\n- async_user_api.py\n- test_async_user_api.py",
                "act"
            ),
            ToolProfile::WriteOnly
        );
    }

    #[test]
    fn test_select_tool_profile_core_edit() {
        assert_eq!(
            select_tool_profile("Fix the off-by-one error in main.rs", "act"),
            ToolProfile::CoreEdit
        );
        assert_eq!(
            select_tool_profile("Refactor the auth module to use traits", "act"),
            ToolProfile::CoreEdit
        );
        assert_eq!(
            select_tool_profile("Modify the config parser to accept YAML", "act"),
            ToolProfile::CoreEdit
        );
    }

    #[test]
    fn test_select_tool_profile_validate() {
        assert_eq!(
            select_tool_profile("Run the tests and fix any failures", "act"),
            ToolProfile::Validate
        );
        assert_eq!(
            select_tool_profile("Lint the codebase and fix warnings", "act"),
            ToolProfile::Validate
        );
    }

    #[test]
    fn test_select_tool_profile_symbol() {
        assert_eq!(
            select_tool_profile(
                "Rename `get_user` to `fetch_user` across the codebase",
                "act"
            ),
            ToolProfile::Symbol
        );
        assert_eq!(
            select_tool_profile("Find all references to `DatabasePool`", "act"),
            ToolProfile::Symbol
        );
    }

    #[test]
    fn test_select_tool_profile_plan_mode() {
        assert_eq!(
            select_tool_profile("Create a file", "plan"),
            ToolProfile::Plan
        );
    }

    #[test]
    fn test_plan_profile_excludes_attempt_completion() {
        let names = ToolProfile::Plan
            .tools()
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();

        assert!(names.contains(&"plan_mode_respond"));
        assert!(!names.contains(&"attempt_completion"));
    }

    #[test]
    fn test_select_tool_profile_complex_defaults_to_full() {
        assert_eq!(
            select_tool_profile(
                "Create a full web application with authentication, database, and deployment",
                "act"
            ),
            ToolProfile::Full
        );
    }

    #[test]
    fn test_answer_only_rejects_file_keywords() {
        assert_ne!(
            select_tool_profile("What is the create_file function?", "act"),
            ToolProfile::AnswerOnly
        );
        assert_ne!(
            select_tool_profile("Can you write a test for this?", "act"),
            ToolProfile::AnswerOnly
        );
    }
}
