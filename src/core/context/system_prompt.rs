//! System prompt construction for sned CLI.
//!

use super::instructions::{SkillMetadata, SkillSource};

// ============================================================================
// System Prompt Context
// ============================================================================

/// Context used when building the system prompt.
///
#[derive(Debug, Clone, Default)]
pub struct SystemPromptContext {
    pub cwd: Option<String>,
    pub ide: String,
    pub supports_browser_use: bool,
    pub yolo_mode_toggled: bool,
    pub sned_web_tools_enabled: bool,
    pub provider_info: ProviderInfo,
    pub preferred_language_instructions: Option<String>,
    pub sned_ignore_instructions: Option<String>,
    pub global_sned_rules_file_instructions: Option<String>,
    pub local_sned_rules_file_instructions: Option<String>,
    pub local_cursor_rules_file_instructions: Option<String>,
    pub local_cursor_rules_dir_instructions: Option<String>,
    pub local_windsurf_rules_file_instructions: Option<String>,
    pub local_agents_rules_file_instructions: Option<String>,
    pub enable_parallel_tool_calling: bool,
    pub user_instructions: Option<String>,
    pub sned_rules: Option<String>,
    pub active_shell_type: Option<String>,
    pub active_shell_path: Option<String>,
    pub active_shell_is_posix: bool,
    pub available_cores: Option<u32>,
    pub skills: Vec<SkillMetadata>,
    pub runtime_placeholders: Option<std::collections::HashMap<String, String>>,
}

/// Provider information for system prompt.
#[derive(Debug, Clone, Default)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
}

// ============================================================================
// Prompt Builder
// ============================================================================

/// Builds the system prompt from a template and context.
///
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    context: SystemPromptContext,
}

impl PromptBuilder {
    pub fn new(context: SystemPromptContext) -> Self {
        Self { context }
    }

    /// Builds the system prompt.
    pub fn build(&self) -> String {
        let prompt = self.render_template();
        self.post_process(prompt)
    }

    fn render_template(&self) -> String {
        let skills_section = self.format_skills_section();

        let mut prompt = "You are Sned, a terminal-first coding agent.\n\n".to_string();

        prompt.push_str(
            "PRIME DIRECTIVES\n\
             - Complete the user's task directly.\n\
             - Minimize turns and load only the context you need.\n\
             - Be concise, deterministic, and finish with the mode-appropriate completion tool.\n\n",
        );
        prompt.push_str("EXECUTION\n");

        if self.context.enable_parallel_tool_calling {
            prompt.push_str(
                "- Batch independent work in one response when it is truly independent.\n",
            );
            prompt.push_str(
                "- Multiple edits to different sections of one file are independent when based on current hash anchors; batch them to save roundtrips.\n",
            );
        }
        if !self.context.enable_parallel_tool_calling {
            prompt.push_str("- Use tools sequentially.\n");
        }

        prompt.push_str(
            "- Use subagents or skills only when they clearly improve result quality.\n\
             - Workspace reads and writes must use tools, not prose: read files with `read_file`; create or overwrite files with `write_to_file`; change existing files with `edit_file` or AST-aware tools.\n\
             - Use relative paths from the working directory for workspace tools. Do not pass absolute paths or paths outside the workspace to file tools.\n\
             - For `edit_file`, read the current file first and copy the exact `Word§line content` anchors from tool output, including the prefix.\n\
             - For large generated files, write a small skeleton first and fill sections with `edit_file` instead of sending one huge payload.\n\
             - In ACT mode, perform the task and finish with `attempt_completion`; in PLAN mode, gather needed context and respond with `plan_mode_respond` without modifying files.\n\
             - Keep validation focused, but run the checks needed for code/workspace changes or required by project instructions.\n\
             - Avoid planning text, broad validation, and extra file reads unless they are necessary, cheap, or user-requested.\n\n\
             SAFETY AND REFUSAL\n\
             - Refuse unsafe or disallowed requests.\n\
             - Do not delete, reset, overwrite user work, expose secrets, or run destructive commands unless the user explicitly requested it and tool approvals allow it.\n\
             - If required information is missing and available tools cannot get it, ask one focused follow-up question.\n",
        );

        if self.context.yolo_mode_toggled {
            prompt.push_str(
                "- You are running in fully autonomous mode. Keep CPU and RAM usage reasonable when using `execute_command`.\n",
            );
        }

        prompt.push_str(
            "\nOUTPUT FORMAT\n\
              - Keep responses short and CLI-friendly.\n\
              - Use tools instead of long prose whenever possible.\n\
              - If no tools are needed or available, answer directly in text and keep it brief.\n\
              - When the task is complete, return the result through the required completion tool.\n\n\
              CODE GENERATION\n\
              - Only create the files and abstractions explicitly requested.\n\
              - Do not add docstrings unless requested.\n\
              - Do not add sync wrappers for async code unless requested.\n\
              - Do not add main()/CLI entry points unless requested.\n\
              - Do not add extra helpers, config objects, or custom error types unless needed to satisfy the prompt.\n\
              - Prefer standard library solutions over hand-rolled utilities.\n\
              - Match the requested shape exactly: if asked for a function, write a function, not a framework.\n",
        );

        prompt.push_str(&skills_section);

        let custom_instructions = self.custom_instruction_blocks();
        if !custom_instructions.is_empty() {
            prompt.push_str("\n\n# USER'S CUSTOM INSTRUCTIONS\n\n");
            prompt.push_str(
                "Follow these task, user, and project instructions unless they conflict with safety or required tool protocol.\n",
            );

            for instructions in custom_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
        }

        prompt
    }

    fn custom_instruction_blocks(&self) -> Vec<&str> {
        [
            self.context.user_instructions.as_deref(),
            self.context.sned_rules.as_deref(),
            self.context.sned_ignore_instructions.as_deref(),
            self.context.global_sned_rules_file_instructions.as_deref(),
            self.context.local_sned_rules_file_instructions.as_deref(),
            self.context.preferred_language_instructions.as_deref(),
            self.context.local_cursor_rules_file_instructions.as_deref(),
            self.context.local_cursor_rules_dir_instructions.as_deref(),
            self.context
                .local_windsurf_rules_file_instructions
                .as_deref(),
            self.context.local_agents_rules_file_instructions.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn post_process(&self, prompt: String) -> String {
        let mut result = prompt;

        // Remove multiple consecutive empty lines
        result = result.replace("\n\n\n", "\n\n");
        result = result.replace("\n\n\n", "\n\n");

        // Trim
        result = result.trim().to_string();

        // Remove trailing ==== after trim
        result = result.trim_end_matches("====").to_string();

        // Remove empty sections between separators
        result = result.replace("\n====\n\n====\n", "\n====\n");
        result = result.replace("====\n\n====\n", "====\n");

        // Remove empty section headers
        result = result.replace("\n##\n", "\n");
        result = result.replace("\n##\r\n", "\n");

        // Clean up any multiple empty lines created by header removal
        result = result.replace("\n\n\n", "\n\n");

        // Final trim
        result.trim().to_string()
    }

    fn format_skills_section(&self) -> String {
        if self.context.skills.is_empty() {
            return String::new();
        }

        let mut section = "\n\n# AVAILABLE SKILLS\n".to_string();
        section.push_str(
            "Use `use_skill` once only when a listed skill clearly matches the task.\n\n",
        );

        // Prioritize Project skills
        let project_skills: Vec<&SkillMetadata> = self
            .context
            .skills
            .iter()
            .filter(|s| matches!(s.source, SkillSource::Project))
            .collect();
        let global_skills: Vec<&SkillMetadata> = self
            .context
            .skills
            .iter()
            .filter(|s| matches!(s.source, SkillSource::Global))
            .collect();

        let display_skills: Vec<&SkillMetadata> = project_skills
            .into_iter()
            .chain(global_skills)
            .take(10)
            .collect();

        for skill in display_skills {
            section.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }

        if self.context.skills.len() > 10 {
            section.push_str(&format!(
                "\n... and {} more. Use the 'list_skills' tool to see the full list.\n",
                self.context.skills.len() - 10
            ));
        }

        section
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_builder_basic() {
        let context = SystemPromptContext {
            cwd: Some("/tmp/test".to_string()),
            ide: "vscode".to_string(),
            enable_parallel_tool_calling: true,
            ..Default::default()
        };

        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(prompt.contains("You are Sned"));
        assert!(prompt.contains("PRIME DIRECTIVES"));
        assert!(prompt.contains("EXECUTION"));
        assert!(
            prompt
                .contains("Use subagents or skills only when they clearly improve result quality")
        );
        assert!(prompt.contains("Workspace reads and writes must use tools"));
        assert!(prompt.contains("Do not pass absolute paths or paths outside the workspace"));
        assert!(prompt.contains("If no tools are needed or available, answer directly in text"));
        assert!(prompt.contains("Word§line content"));
        assert!(prompt.contains("In ACT mode"));
        assert!(prompt.contains("PLAN mode"));
        // Environment info is now provided by context_loader, not in system prompt
        assert!(!prompt.contains("ENVIRONMENT"));
        assert!(!prompt.contains("Current Working Directory:"));
    }

    #[test]
    fn test_prompt_builder_with_skills() {
        let context = SystemPromptContext {
            skills: vec![
                SkillMetadata {
                    name: "rust".to_string(),
                    description: "Rust programming expertise".to_string(),
                    path: ".sned/skills/rust".to_string(),
                    source: SkillSource::Project,
                },
                SkillMetadata {
                    name: "python".to_string(),
                    description: "Python programming expertise".to_string(),
                    path: "/global/skills/python".to_string(),
                    source: SkillSource::Global,
                },
            ],
            ..Default::default()
        };

        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(prompt.contains("AVAILABLE SKILLS"));
        assert!(prompt.contains("rust: Rust programming expertise"));
        assert!(prompt.contains("python: Python programming expertise"));
    }

    #[test]
    fn test_prompt_builder_with_custom_instructions() {
        let context = SystemPromptContext {
            user_instructions: Some("Always use TypeScript.".to_string()),
            sned_rules: Some("Follow the style guide.".to_string()),
            ..Default::default()
        };

        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(prompt.contains("USER'S CUSTOM INSTRUCTIONS"));
        assert!(prompt.contains("Always use TypeScript."));
        assert!(prompt.contains("Follow the style guide."));
    }

    #[test]
    fn test_prompt_builder_does_not_duplicate_custom_instruction_sources() {
        let context = SystemPromptContext {
            user_instructions: Some("MARKER_ALPHA_USER".to_string()),
            sned_rules: Some("MARKER_BRAVO_RULE".to_string()),
            sned_ignore_instructions: Some("MARKER_CHARLIE_IGNORE".to_string()),
            global_sned_rules_file_instructions: Some("MARKER_DELTA_GLOBAL".to_string()),
            local_sned_rules_file_instructions: Some("MARKER_ECHO_LOCAL".to_string()),
            preferred_language_instructions: Some("MARKER_FOXTROT_LANGUAGE".to_string()),
            local_cursor_rules_file_instructions: Some("MARKER_GOLF_CURSOR_FILE".to_string()),
            local_cursor_rules_dir_instructions: Some("MARKER_HOTEL_CURSOR_DIR".to_string()),
            local_windsurf_rules_file_instructions: Some("MARKER_INDIA_WINDSURF".to_string()),
            local_agents_rules_file_instructions: Some("MARKER_JULIET_AGENTS".to_string()),
            ..Default::default()
        };

        let prompt = PromptBuilder::new(context).build();

        for marker in [
            "MARKER_ALPHA_USER",
            "MARKER_BRAVO_RULE",
            "MARKER_CHARLIE_IGNORE",
            "MARKER_DELTA_GLOBAL",
            "MARKER_ECHO_LOCAL",
            "MARKER_FOXTROT_LANGUAGE",
            "MARKER_GOLF_CURSOR_FILE",
            "MARKER_HOTEL_CURSOR_DIR",
            "MARKER_INDIA_WINDSURF",
            "MARKER_JULIET_AGENTS",
        ] {
            assert_eq!(
                prompt.matches(marker).count(),
                1,
                "expected {marker} to be emitted once"
            );
        }
    }

    #[test]
    fn test_prompt_builder_yolo_mode() {
        let context = SystemPromptContext {
            yolo_mode_toggled: true,
            ..Default::default()
        };

        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(prompt.contains("fully autonomous mode"));
    }

    #[test]
    fn test_prompt_builder_post_processing() {
        let context = SystemPromptContext::default();
        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        // Should not have trailing whitespace
        assert!(!prompt.ends_with('\n'));
        // Should not have multiple consecutive empty lines
        assert!(!prompt.contains("\n\n\n"));
    }

    #[test]
    fn test_prompt_builder_with_rules() {
        let context = SystemPromptContext {
            local_agents_rules_file_instructions: Some(
                "# AGENTS.md Rules\n\nBe helpful.".to_string(),
            ),
            local_cursor_rules_file_instructions: Some("# Cursor Rules\n\nUse Rust.".to_string()),
            local_windsurf_rules_file_instructions: Some(
                "# Windsurf Rules\n\nFormat code.".to_string(),
            ),
            ..Default::default()
        };

        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(prompt.contains("USER'S CUSTOM INSTRUCTIONS"));
        assert!(prompt.contains("Be helpful."));
        assert!(prompt.contains("Use Rust."));
        assert!(prompt.contains("Format code."));
    }

    #[test]
    fn test_prompt_builder_with_cursor_dir_rules() {
        let context = SystemPromptContext {
            local_cursor_rules_dir_instructions: Some(
                "# Cursor Rules Directory\n\nRule 1\n\nRule 2".to_string(),
            ),
            ..Default::default()
        };

        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(prompt.contains("USER'S CUSTOM INSTRUCTIONS"));
        assert!(prompt.contains("Rule 1"));
        assert!(prompt.contains("Rule 2"));
    }

    #[test]
    fn test_prompt_builder_no_rules_no_custom_instructions_section() {
        let context = SystemPromptContext {
            ..Default::default()
        };

        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        // Should not have custom instructions section when no rules/instructions exist
        assert!(!prompt.contains("USER'S CUSTOM INSTRUCTIONS"));
    }

    #[test]
    fn test_prompt_builder_large_file_guidance() {
        let context = SystemPromptContext::default();
        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(prompt.contains("write a small skeleton first"));
        assert!(prompt.contains("fill sections with `edit_file`"));
    }

    #[test]
    fn test_prompt_builder_omits_environment_context() {
        let context = SystemPromptContext {
            cwd: Some("/home/user/project".to_string()),
            active_shell_type: Some("zsh".to_string()),
            active_shell_path: Some("/bin/zsh".to_string()),
            available_cores: Some(8),
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(!prompt.contains("ENVIRONMENT"));
        assert!(!prompt.contains("/home/user/project"));
        assert!(!prompt.contains("/bin/zsh"));
        assert!(!prompt.contains("Available cores"));
    }

    #[test]
    fn test_prompt_builder_parallel_mode_has_batching_guidance() {
        let context = SystemPromptContext {
            enable_parallel_tool_calling: true,
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(prompt.contains("Batch independent work in one response"));
        assert!(prompt.contains("Multiple edits to different sections of one file"));
    }

    #[test]
    fn test_prompt_builder_sequential_mode_has_sequential_guidance() {
        let context = SystemPromptContext {
            enable_parallel_tool_calling: false,
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(prompt.contains("Use tools sequentially"));
        assert!(!prompt.contains("Batch independent tool calls"));
    }
}
