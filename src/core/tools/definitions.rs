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

pub fn read_file_schema() -> ToolSchema {
    ToolSchema {
        name: "read_file",
        description: "Reads the complete contents of one or more files at the specified paths. Automatically extracts raw text from PDF and DOCX files. Returns hash-anchored lines (format: Word§line content) that you MUST use with the edit_file tool. IMPORTANT: Copy anchor strings EXACTLY as shown in the output (e.g., \"Crawler§void draw_game_over() {\"). You can also specify a line range to read only a specific part of the file(s). Examples: { paths: [\"src/main.ts\", \"package.json\"] }, { paths: [\"src/main.ts\"] }, { paths: [\"src/main.ts\"], start_line: 10, end_line: 50 }. Consider using surgical tools like get_file_skeleton or get_function over this.",
        parameters: vec![
            ToolParameter {
                name: "paths",
                required: true,
                param_type: "array",
                description: "An array of relative paths to the source files.",
                items: Some(serde_json::json!({"type": "string"})),
                extra: None,
            },
            ToolParameter {
                name: "start_line",
                required: false,
                param_type: "integer",
                description: "Optional. If not supplied, output will start from line 1.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "end_line",
                required: false,
                param_type: "integer",
                description: "Optional. If not supplied, the output will go until the last line.",
                items: None,
                extra: None,
            },
        ],
    }
}

pub fn write_to_file_schema() -> ToolSchema {
    ToolSchema {
        name: "write_to_file",
        description: "Write content to a file at the specified path. If the file exists, it will be overwritten. Creates parent directories if needed.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: true,
                param_type: "string",
                description: "The path of the file to write (relative to the workspace root).",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "content",
                required: true,
                param_type: "string",
                description: "The content to write to the file.",
                items: None,
                extra: None,
            },
        ],
    }
}

pub fn list_files_schema() -> ToolSchema {
    ToolSchema {
        name: "list_files",
        description: "Lists files and directories in the specified path. Returns a formatted tree-like listing with file sizes and line counts.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: false,
                param_type: "string",
                description: "The path to list (relative to current working directory). Defaults to current directory.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "recursive",
                required: false,
                param_type: "boolean",
                description: "Whether to list files recursively. Defaults to false.",
                items: None,
                extra: None,
            },
        ],
    }
}

pub fn search_files_schema() -> ToolSchema {
    ToolSchema {
        name: "search_files",
        description: "Search for files matching a regex pattern. Returns file paths with line numbers and match context.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: false,
                param_type: "string",
                description: "The directory to search in (relative to current working directory). Defaults to current directory.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "regex",
                required: true,
                param_type: "string",
                description: "The regular expression pattern to search for.",
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

pub fn edit_file_schema() -> ToolSchema {
    ToolSchema {
        name: "edit_file",
        description: "Edit one or more files by replacing, inserting after, or inserting before specific lines. CRITICAL: You MUST read files first using read_file to get the current hash-anchored lines. Use the EXACT anchor strings from read_file output (format: Word§line content). Each file contains an array of edits. EDIT TYPES: 1. replace (default): Replaces an inclusive range of lines from anchor to end_anchor. If end_anchor is omitted, defaults to anchor (single-line replace). 2. insert_after: Inserts the provided text immediately after the line specified by anchor. 3. insert_before: Inserts the provided text immediately before the line specified by anchor.",
        parameters: vec![ToolParameter {
            name: "files",
            required: true,
            param_type: "array",
            description: "An array of file objects to edit.",
            items: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The path of the file to edit (relative to the current working directory)."
                    },
                    "edits": {
                        "type": "array",
                        "description": "An array of edit objects to apply to the file.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "edit_type": {
                                    "type": "string",
                                    "enum": ["replace", "insert_after", "insert_before"],
                                    "description": "The type of edit to perform. Defaults to 'replace'."
                                },
                                "anchor": {
                                    "type": "string",
                                    "description": "Anchor for the start of the edit or the insertion point. MUST be copied exactly from read_file output (format: Word§line content). Example: \"Crawler§void draw_game_over() {\". Must be a single line only, no newline char."
                                },
                                "end_anchor": {
                                    "type": "string",
                                    "description": "Anchor for the end of the edit (required for 'replace'). MUST be copied exactly from read_file output (format: Word§line content). Example: \"Crawler§void draw_game_over() {\". Must be a single line only, no newline char."
                                },
                                "text": {
                                    "type": "string",
                                    "description": "The new text content for the edit. Use \\n for new lines."
                                }
                            },
                            "required": ["edit_type", "anchor", "text"]
                        }
                    }
                },
                "required": ["path", "edits"]
            })),
            extra: None,
        }],
    }
}

pub fn execute_command_schema() -> ToolSchema {
    ToolSchema {
        name: "execute_command",
        description: "Executes CLI commands or scripts. Use 'commands' for simple sequences of shell operations. Use 'script' for complex multi-line logic. Provide exactly one of {commands, script}.",
        parameters: vec![
            ToolParameter {
                name: "commands",
                required: false,
                param_type: "array",
                description: "An array of CLI commands to execute in sequence.",
                items: Some(serde_json::json!({"type": "string"})),
                extra: None,
            },
            ToolParameter {
                name: "script",
                required: false,
                param_type: "string",
                description: "A script to execute. Use this for complex multi-line logic or non-shell languages.",
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
                description: "If true, return raw output without stripping progress bar artifacts. Defaults to false.",
                items: None,
                extra: None,
            },
        ],
    }
}

pub fn ask_followup_question_schema() -> ToolSchema {
    ToolSchema {
        name: "ask_followup_question",
        description: "Ask the user a question to clarify their request or get additional information.",
        parameters: vec![ToolParameter {
            name: "question",
            required: true,
            param_type: "string",
            description: "The question to ask the user.",
            items: None,
            extra: None,
        }],
    }
}

pub fn attempt_completion_schema() -> ToolSchema {
    ToolSchema {
        name: "attempt_completion",
        description: "Present the final result of the task to the user. Use this when you have completed the user's request.",
        parameters: vec![
            ToolParameter {
                name: "result",
                required: true,
                param_type: "string",
                description: "A summary of what was accomplished.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "command",
                required: false,
                param_type: "string",
                description: "Optional CLI command to demonstrate the result.",
                items: None,
                extra: None,
            },
        ],
    }
}

pub fn plan_mode_respond_schema() -> ToolSchema {
    ToolSchema {
        name: "plan_mode_respond",
        description: "Respond to the user in plan mode. Use this when you need to present a plan or ask for confirmation before proceeding.",
        parameters: vec![ToolParameter {
            name: "response",
            required: true,
            param_type: "string",
            description: "Your response to the user.",
            items: None,
            extra: None,
        }],
    }
}

pub fn get_function_schema() -> ToolSchema {
    ToolSchema {
        name: "get_function",
        description: "Get the implementation of a specific function or method from a file.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: true,
                param_type: "string",
                description: "The path of the file containing the function.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "name",
                required: true,
                param_type: "string",
                description: "The name of the function or method to retrieve.",
                items: None,
                extra: None,
            },
        ],
    }
}

pub fn get_file_skeleton_schema() -> ToolSchema {
    ToolSchema {
        name: "get_file_skeleton",
        description: "Get a structural skeleton of a file showing classes, functions, and their signatures without full implementations.",
        parameters: vec![ToolParameter {
            name: "path",
            required: true,
            param_type: "string",
            description: "The path of the file to analyze.",
            items: None,
            extra: None,
        }],
    }
}

pub fn find_symbol_references_schema() -> ToolSchema {
    ToolSchema {
        name: "find_symbol_references",
        description: "Find all references to a symbol (function, class, variable) across the codebase.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: true,
                param_type: "string",
                description: "The path of the file containing the symbol definition.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "name",
                required: true,
                param_type: "string",
                description: "The name of the symbol to find references for.",
                items: None,
                extra: None,
            },
        ],
    }
}

pub fn replace_symbol_schema() -> ToolSchema {
    ToolSchema {
        name: "replace_symbol",
        description: "Replace all occurrences of a symbol with a new name across the codebase.",
        parameters: vec![
            ToolParameter {
                name: "path",
                required: true,
                param_type: "string",
                description: "The path of the file containing the symbol definition.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "old_name",
                required: true,
                param_type: "string",
                description: "The current name of the symbol.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "new_name",
                required: true,
                param_type: "string",
                description: "The new name for the symbol.",
                items: None,
                extra: None,
            },
        ],
    }
}

pub fn rename_symbol_schema() -> ToolSchema {
    ToolSchema {
        name: "rename_symbol",
        description: "Renames ALL occurrences of a symbol (function, class, method, or variable) inside the specified files or directories. This tool can identify precise symbols using a language's AST and is more accurate than a simple search-and-replace because it understands the language structure. For renaming tasks, strongly prefer this as the first pass.",
        parameters: vec![
            ToolParameter {
                name: "paths",
                required: true,
                param_type: "array",
                description: "An array of relative paths to the directories or files to perform the rename in.",
                items: Some(serde_json::json!({"type": "string"})),
                extra: None,
            },
            ToolParameter {
                name: "existing_symbol",
                required: true,
                param_type: "string",
                description: "The exact name of the symbol to be renamed.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "new_symbol",
                required: true,
                param_type: "string",
                description: "The new name for the symbol.",
                items: None,
                extra: None,
            },
        ],
    }
}

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
            ToolParameter {
                name: "include_history",
                required: false,
                param_type: "boolean",
                description: "Optional boolean to include the main task's conversation history. This benefits from context caching and provides more context, but consumes context window space.",
                items: None,
                extra: None,
            },
        ],
    }
}

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

pub fn list_skills_schema() -> ToolSchema {
    ToolSchema {
        name: "list_skills",
        description: "List all available skills and their descriptions. Use this to discover specialized capabilities when the initial list in the system prompt is truncated.",
        parameters: vec![],
    }
}

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

pub fn summarize_task_schema() -> ToolSchema {
    ToolSchema {
        name: "summarize_task",
        description: "Summarize the task to free up context window space.",
        parameters: vec![
            ToolParameter {
                name: "context",
                required: true,
                param_type: "string",
                description: "Detailed summary of the conversation so far, including current work, technical concepts, modified files, problems solved, and exact pending next steps.",
                items: None,
                extra: None,
            },
            ToolParameter {
                name: "required_files",
                required: false,
                param_type: "array",
                description: "List of relative paths to the most important files needed to continue the task.",
                items: Some(serde_json::json!({"type": "string"})),
                extra: None,
            },
        ],
    }
}

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

pub fn new_task_schema() -> ToolSchema {
    ToolSchema {
        name: "new_task",
        description: "Creates a new task with preloaded context from the current conversation.",
        parameters: vec![ToolParameter {
            name: "context",
            required: true,
            param_type: "string",
            description: "Detailed summary of the conversation so far, including current work, technical concepts, modified files, problems solved, and exact pending next steps.",
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
#[allow(unreachable_patterns)]
pub fn get_tool_schema(tool: SnedTool) -> Option<ToolSchema> {
    match tool {
        // All variants are explicitly matched
        SnedTool::ReadFile => Some(read_file_schema()),
        SnedTool::WriteToFile => Some(write_to_file_schema()),
        SnedTool::ListFiles => Some(list_files_schema()),
        SnedTool::SearchFiles => Some(search_files_schema()),
        SnedTool::EditFile => Some(edit_file_schema()),
        SnedTool::ExecuteCommand => Some(execute_command_schema()),
        SnedTool::AskFollowupQuestion => Some(ask_followup_question_schema()),
        SnedTool::AttemptCompletion => Some(attempt_completion_schema()),
        SnedTool::PlanModeRespond => Some(plan_mode_respond_schema()),
        SnedTool::GetFunction => Some(get_function_schema()),
        SnedTool::GetFileSkeleton => Some(get_file_skeleton_schema()),
        SnedTool::FindSymbolReferences => Some(find_symbol_references_schema()),
        SnedTool::ReplaceSymbol => Some(replace_symbol_schema()),
        SnedTool::RenameSymbol => Some(rename_symbol_schema()),
        SnedTool::UseSubagents => Some(use_subagents_schema()),
        SnedTool::UseSkill => Some(use_skill_schema()),
        SnedTool::ListSkills => Some(list_skills_schema()),
        SnedTool::DiagnosticsScan => Some(diagnostics_scan_schema()),
        SnedTool::SummarizeTask => Some(summarize_task_schema()),
        SnedTool::Condense => Some(condense_schema()),
        SnedTool::WebFetch => Some(web_fetch_schema()),
        SnedTool::NewTask => Some(new_task_schema()),
        _ => None,
    }
}

/// Returns ToolDefinitions for all active (kept) tools.
pub fn get_active_tool_definitions() -> Vec<ToolDefinition> {
    let active_tools = [
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
        SnedTool::SummarizeTask,
        SnedTool::Condense,
        SnedTool::WebFetch,
        SnedTool::NewTask,
    ];

    active_tools
        .iter()
        .filter_map(|&t| get_tool_schema(t).map(|s| s.to_tool_definition()))
        .collect()
}

/// Returns ToolDefinitions for read-only tools only.
pub fn get_read_only_tool_definitions() -> Vec<ToolDefinition> {
    let read_only_tools = [
        SnedTool::ReadFile,
        SnedTool::ListFiles,
        SnedTool::SearchFiles,
        SnedTool::AskFollowupQuestion,
        SnedTool::GetFunction,
        SnedTool::GetFileSkeleton,
        SnedTool::FindSymbolReferences,
        SnedTool::ListSkills,
        SnedTool::DiagnosticsScan,
    ];

    read_only_tools
        .iter()
        .filter_map(|&t| get_tool_schema(t).map(|s| s.to_tool_definition()))
        .collect()
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
        let def = edit_file_schema().to_tool_definition();
        assert_eq!(def.function.name, "edit_file");

        let params = def.function.parameters.as_object().unwrap();
        let properties = params["properties"].as_object().unwrap();
        assert!(properties.contains_key("files"));

        let files_param = properties["files"].as_object().unwrap();
        assert_eq!(files_param["type"], "array");
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
        assert_eq!(defs.len(), 22);
    }

    #[test]
    fn test_read_only_tools_count() {
        let defs = get_read_only_tool_definitions();
        assert_eq!(defs.len(), 9);
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
    fn test_all_tools_have_schemas() {
        let active = [
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
            SnedTool::SummarizeTask,
            SnedTool::Condense,
            SnedTool::WebFetch,
        ];

        for tool in &active {
            assert!(
                get_tool_schema(*tool).is_some(),
                "Tool {:?} should have a schema",
                tool
            );
        }
    }
}
