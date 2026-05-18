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
        let current_cwd = self.context.cwd.as_deref().unwrap_or("/");
        let os = std::env::consts::OS;

        let shell_env = std::env::var("SHELL").ok();
        let shell = self
            .context
            .active_shell_path
            .as_deref()
            .or(shell_env.as_deref())
            .unwrap_or("bash");

        let shell_type = self.context.active_shell_type.as_deref().unwrap_or("bash");

        let available_cores = self.context.available_cores.unwrap_or(1);

        let skills_section = self.format_skills_section();

        let mut prompt = "You are Sned, an exceptionally skilled AI agent at solving problems with extensive knowledge in many programming languages, frameworks, design patterns, and best practices.\n\
             \n\
             PRIME DIRECTIVES\n\
             \n\
             1. ACCOMPLISH THE TASK HUMAN GIVES YOU.\n\
             2. MINIMIZE THE NUMBER OF ROUND TRIPS NEEDED TO DO THIS. BATCH TOOL CALLS TOGETHER TO AVOID MULTIPLE ROUND TRIPS.\n\
             3. LOAD INTO CONTEXT ONLY WHAT IS NECESSARY.\n\
             \n\
             TOOL USE\n\
             \n".to_string();

        if self.context.enable_parallel_tool_calling {
            prompt.push_str(" You may use multiple tools in a single response when the operations are independent (e.g., reading several files, searching in parallel). When refactoring a single file, multiple edits to different sections of the file are considered INDEPENDENT operations because we have stable hash anchors. You should batch them into a single response to save roundtrips.\n");
        }

        prompt.push_str(
            "- Prefer tools for communication; avoid redundant text in assistant responses.\n",
        );
        prompt.push_str("- CRITICAL: When writing files, you MUST use the write_to_file tool. NEVER output file contents as text in your response.\n");
        prompt.push_str("- For large files, avoid a single giant write_to_file payload. If the file is too large to author in one response, write a minimal skeleton first, then fill sections incrementally with edit_file.\n");
        prompt.push_str("- CRITICAL: When editing files, you MUST use the edit_file tool. NEVER describe edits in text.\n");
        prompt.push_str("- CRITICAL: When reading files, you MUST use the read_file tool. NEVER ask the user to provide file contents.\n");
        prompt.push_str("\nHASH-ANCHORED EDITS\n\n");
        prompt.push_str(
            "The read_file tool returns lines with hash anchors in the format: Word§line content\n",
        );
        prompt.push_str("- Example: \"Crawler§void draw_game_over() {\"\n");
        prompt.push_str(
            "- The anchor prefix (e.g., \"Crawler§\") is REQUIRED for edit_file to work.\n",
        );
        prompt.push_str("- When using edit_file, you MUST copy the EXACT anchor strings from read_file output.\n");
        prompt.push_str(
            "- WRONG: Using raw source lines like \"void draw_game_over() {\" as anchors.\n",
        );
        prompt.push_str("- WRONG: Modifying the anchor text or omitting the Word§ prefix.\n");
        prompt.push_str("- The anchor word is a hash of the line content - it uniquely identifies that exact line.\n");
        prompt.push_str("- Always read files first to get current anchors before editing.\n");
        prompt.push_str("\nEXAMPLES\n\n");
        prompt.push_str("CORRECT - Writing a file (use the write_to_file tool):\n");
        prompt.push_str("  {\"path\": \"src/main.rs\", \"content\": \"fn main() {}\"}\n\n");
        prompt.push_str("WRONG - Do NOT output code as text:\n");
        prompt.push_str("  \"Here is the code: fn main() { ... }\"\n\n");
        prompt.push_str("CORRECT - Reading a file (use the read_file tool):\n");
        prompt.push_str("  {\"path\": \"src/main.rs\"}\n\n");
        prompt.push_str("WRONG - Do NOT ask user for file contents:\n");
        prompt.push_str("  \"Please provide the contents of src/main.rs\"\n\n");

        prompt.push_str(
            "ACT MODE VS PLAN MODE\n\
             \n\
             In each user message, the environment_details will specify the current mode. There are two modes:\n\
             \n\
             - ACT MODE: In this mode, you have access to all tools EXCEPT the plan_mode_respond tool.\n\
              - In ACT MODE, you use tools to accomplish the user's task. Once you've completed the user's task, you use the attempt_completion tool to present the result of the task to the user.\n\
             - PLAN MODE: In this special mode, you have access to the plan_mode_respond tool.\n\
              - In PLAN MODE, start by getting precise understanding of what the user wants in this task.\n\
              - In PLAN MODE, the goal is to gather information and get context to create a detailed plan for accomplishing the task, which the user will review and approve before they switch you to ACT MODE to implement the solution.\n\
             \n\
             SYSTEM INFO\n\
             \n"
        );

        prompt.push_str(&format!("- Operating System: {}\n", os));
        prompt.push_str(&format!("- Default Shell: {}\n", shell));

        if self.context.active_shell_is_posix {
            prompt.push_str("- You are running in a full-featured shell environment. You have access to standard Unix tools (`grep`, `sed`, `awk`, `find`, `xargs`, etc.).\n");
        } else if os == "windows" {
            prompt.push_str("- You are in a limited Windows shell environment. Standard Unix tools are NOT available. You MUST use PowerShell cmdlets or standard cmd commands.\n");
        }

        if shell_type == "git-bash" {
            prompt.push_str("- Note: Use Git Bash path formatting (e.g., `/c/Users/...`) and account for Windows CRLF line endings.\n");
        }

        if shell_type == "wsl" {
            prompt.push_str("- Note: Windows drives are mounted at `/mnt/c/`.\n");
        }

        prompt.push_str(&format!(
            "- Current Working Directory: {} (this is where all the tools will be executed from)\n",
            current_cwd
        ));
        prompt.push_str(&format!("- Available CPU Cores: {} (Use this value for parallel jobs like 'make -j' instead of 'nproc')\n", available_cores));

        prompt.push_str("\nIMPORTANT PATH RULES\n\n");
        prompt.push_str("When using tools that accept file paths (read_file, write_to_file, edit_file, list_files, etc.), ALWAYS use relative paths from the current working directory.\n");
        prompt.push_str("- CORRECT: path: \"src/main.rs\"\n");
        prompt.push_str("- WRONG: path: \"/Users/easto/project/src/main.rs\"\n");
        prompt.push_str(
            "The current working directory is already set; do NOT use absolute paths.\n\n",
        );

        if self.context.yolo_mode_toggled {
            prompt.push_str("- You are running in fully autonomous mode.\n");
        }

        prompt.push_str("\nOBJECTIVE\n\n");
        prompt.push_str("You accomplish a given task iteratively, breaking it down into clear steps and working through them methodically.\n\n");
        prompt.push_str("1. Analyze the user's task and set clear, achievable goals to accomplish it. Prioritize these goals in a logical order.\n");

        if self.context.enable_parallel_tool_calling {
            prompt.push_str("2. Work through these goals sequentially, utilizing available tools as necessary. You may call multiple independent tools in a single response to work efficiently.\n");
        } else {
            prompt.push_str("2. Work through these goals sequentially, utilizing available tools one at a time as necessary.\n");
        }

        prompt.push_str("3. Once you've completed the user's task, you must use the attempt_completion tool to present the result of the task to the user.\n");

        if self.context.yolo_mode_toggled {
            prompt.push_str("4. You are running in fully autonomous mode. Make sure to keep the CPU usage and RAM use reasonable when using `execute_command`.\n");
        }

        prompt.push_str("\nFEEDBACK\n\n");
        prompt.push_str("When user is providing you with feedback on how you could improve, you can let the user know to report new issue using the '/reportbug' slash command.\n");

        prompt.push_str(&skills_section);

        // Add custom instructions section if any are present
        let has_custom_instructions = self.context.user_instructions.is_some()
            || self.context.sned_rules.is_some()
            || self.context.preferred_language_instructions.is_some()
            || self.context.global_sned_rules_file_instructions.is_some()
            || self.context.local_sned_rules_file_instructions.is_some()
            || self.context.local_cursor_rules_file_instructions.is_some()
            || self.context.local_cursor_rules_dir_instructions.is_some()
            || self
                .context
                .local_windsurf_rules_file_instructions
                .is_some()
            || self.context.local_agents_rules_file_instructions.is_some();

        if has_custom_instructions {
            prompt.push_str("\n\n# USER'S CUSTOM INSTRUCTIONS\n\n");
            prompt.push_str("The following additional instructions are provided by the user.\n");

            if let Some(instructions) = &self.context.user_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(rules) = &self.context.sned_rules {
                prompt.push_str(&format!("\n{}\n", rules));
            }
            if let Some(instructions) = &self.context.sned_ignore_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.global_sned_rules_file_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.local_sned_rules_file_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.preferred_language_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.sned_ignore_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.global_sned_rules_file_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.local_sned_rules_file_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.local_cursor_rules_file_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.local_cursor_rules_dir_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.local_windsurf_rules_file_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
            if let Some(instructions) = &self.context.local_agents_rules_file_instructions {
                prompt.push_str(&format!("\n{}\n", instructions));
            }
        }

        prompt
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
            "You have access to specialized skills. Use the 'use_skill' tool to activate one.\n\n",
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
        assert!(prompt.contains("TOOL USE"));
        assert!(prompt.contains("batch them into a single response"));
        assert!(prompt.contains("Current Working Directory: /tmp/test"));
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

        assert!(prompt.contains("If the file is too large to author in one response"));
        assert!(prompt.contains("write a minimal skeleton first"));
        assert!(prompt.contains("fill sections incrementally with edit_file"));
    }
}
