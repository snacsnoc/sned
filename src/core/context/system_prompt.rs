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
    /// Active model identifier (e.g. "qwen3-coder", "gpt-4o", "qwen/qwen3-coder").
    /// Used for model-specific prompt routing. None = generic behavior.
    pub model_id: Option<String>,
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
    #[must_use]
    pub fn new(context: SystemPromptContext) -> Self {
        Self { context }
    }

    /// Builds the system prompt.
    #[must_use]
    pub fn build(&self) -> String {
        let prompt = self.render_template();
        let prompt = self.apply_env_override(prompt);
        self.post_process(prompt)
    }

    /// If `SNED_SYSTEM_MD` points to a readable, non-empty file, prepend
    /// its contents ahead of the generated prompt with a separator.
    /// If unset, unreadable, or empty, the prompt is returned unchanged.
    /// No replace mode in this patch.
    fn apply_env_override(&self, prompt: String) -> String {
        let Ok(path) = std::env::var("SNED_SYSTEM_MD") else {
            return prompt;
        };
        if path.is_empty() {
            return prompt;
        }
        let Ok(custom) = std::fs::read_to_string(&path) else {
            return prompt;
        };
        if custom.trim().is_empty() {
            return prompt;
        }
        format!("{custom}\n\n---\n\n{prompt}")
    }

    fn model_specific_section(&self) -> String {
        match self.context.model_id.as_deref() {
            Some(m) if super::model_detect::is_qwen_model(m) => Self::qwen_section(),
            _ => String::new(),
        }
    }

    fn qwen_section() -> String {
        "\
QWEN MODEL GUIDANCE
- Call a tool explicitly when the task advances by inspecting, editing, running, or searching; do not describe plans in prose instead of acting.
- Tool arguments must be valid JSON that matches the tool's schema; double-check field names and required keys before submitting.
- Execute in small, verifiable steps: re-read a file with read_file before edit_file so the anchor matches the current line exactly.
- When a tool returns an error, read the error text, fix the failing argument, and call the same tool again. Do not guess a corrected result.
"
        .to_string()
    }

    fn render_template(&self) -> String {
        let skills_section = self.format_skills_section();

        let model_section = self.model_specific_section();

        let mut prompt = "You are Sned, a terminal-first coding agent.\n\n".to_string();

        if !model_section.is_empty() {
            prompt.push('\n');
            prompt.push_str(&model_section);
            prompt.push('\n');
        }

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
                prompt.push_str(&format!("\n{instructions}\n"));
            }
        }

        if let Some(examples) =
            super::tool_examples::tool_examples_for_model(self.context.model_id.as_deref())
        {
            prompt.push_str("\n\n");
            prompt.push_str(examples);
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

    #[allow(clippy::unused_self)]
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

    #[test]
    fn test_qwen_model_id_injects_qwen_section() {
        let context = SystemPromptContext {
            model_id: Some("qwen3-coder".to_string()),
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(prompt.contains("QWEN MODEL GUIDANCE"));
        assert!(prompt.contains("Call a tool explicitly"));
    }

    #[test]
    fn test_qwen_model_id_injects_tool_examples() {
        let context = SystemPromptContext {
            model_id: Some("qwen3-coder".to_string()),
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(prompt.contains("EXAMPLE TOOL CALLS (Qwen)"));
        assert!(prompt.contains("tool=read_file"));
    }

    #[test]
    fn test_qwen_routed_model_id_injects() {
        let context = SystemPromptContext {
            model_id: Some("qwen/qwen3-coder".to_string()),
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(prompt.contains("QWEN MODEL GUIDANCE"));
        assert!(prompt.contains("EXAMPLE TOOL CALLS (Qwen)"));
    }

    #[test]
    fn test_non_qwen_model_id_no_section() {
        let context = SystemPromptContext {
            model_id: Some("gpt-4o".to_string()),
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(!prompt.contains("QWEN MODEL GUIDANCE"));
        assert!(!prompt.contains("EXAMPLE TOOL CALLS"));
    }

    #[test]
    fn test_model_id_none_no_section_no_examples() {
        let context = SystemPromptContext::default();
        let prompt = PromptBuilder::new(context).build();
        assert!(!prompt.contains("QWEN MODEL GUIDANCE"));
        assert!(!prompt.contains("EXAMPLE TOOL CALLS"));
    }

    #[test]
    fn test_non_qwen_prompt_byte_identical_to_pre_patch() {
        // Critical invariant: non-Qwen prompts must be byte-identical
        // whether model_id is None or Some(non-Qwen).
        let ctx_none = SystemPromptContext {
            cwd: Some("/tmp/test".to_string()),
            ide: "vscode".to_string(),
            enable_parallel_tool_calling: true,
            ..Default::default()
        };
        let ctx_gpt = SystemPromptContext {
            cwd: Some("/tmp/test".to_string()),
            ide: "vscode".to_string(),
            enable_parallel_tool_calling: true,
            model_id: Some("gpt-4o".to_string()),
            ..Default::default()
        };
        let prompt_none = PromptBuilder::new(ctx_none).build();
        let prompt_gpt = PromptBuilder::new(ctx_gpt).build();
        assert_eq!(prompt_none, prompt_gpt);
    }

    /// Process-global mutex to serialize env var tests. `cargo test` runs
    /// tests in parallel by default, and process-global env vars are
    /// shared across threads. Without this guard, one test reading
    /// `SNED_SYSTEM_MD` can observe another test's value mid-flight.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Helper for env var tests: set env var for the duration of the
    /// test, restore on drop. Holds `ENV_TEST_LOCK` while alive so
    /// tests that read/write `SNED_SYSTEM_MD` cannot interleave.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var(key).ok();
            // SAFETY: env var mutation is serialized via ENV_TEST_LOCK.
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key,
                prev,
                _lock: lock,
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: env var mutation is serialized via ENV_TEST_LOCK.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn test_sned_system_md_prepends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("custom.md");
        std::fs::write(&path, "CUSTOM_HEADER").expect("write");
        let _g = EnvGuard::set("SNED_SYSTEM_MD", path.to_str().unwrap());

        let context = SystemPromptContext::default();
        let prompt = PromptBuilder::new(context).build();

        assert!(prompt.starts_with("CUSTOM_HEADER"));
        assert!(prompt.contains("\n\n---\n\n"));
        // Generated prompt must appear after the separator.
        assert!(prompt.contains("You are Sned"));
        let sep_idx = prompt.find("---").expect("separator present");
        let sned_idx = prompt.find("You are Sned").expect("sned prompt present");
        assert!(sned_idx > sep_idx);
    }

    #[test]
    fn test_sned_system_md_unset_no_change() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: env var mutation is serialized via ENV_TEST_LOCK.
        unsafe {
            std::env::remove_var("SNED_SYSTEM_MD");
        }
        let ctx_unset = SystemPromptContext::default();
        let prompt_unset = PromptBuilder::new(ctx_unset).build();
        // Compare against a fresh builder (also with env unset).
        let prompt_again = PromptBuilder::new(SystemPromptContext::default()).build();
        assert_eq!(prompt_unset, prompt_again);
    }

    #[test]
    fn test_sned_system_md_missing_file_no_change() {
        let _g = EnvGuard::set("SNED_SYSTEM_MD", "/nonexistent/path/to/file.md");
        let ctx = SystemPromptContext::default();
        let prompt_with_missing = PromptBuilder::new(ctx).build();
        drop(_g);
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: env var mutation is serialized via ENV_TEST_LOCK.
        unsafe {
            std::env::remove_var("SNED_SYSTEM_MD");
        }
        let prompt_baseline = PromptBuilder::new(SystemPromptContext::default()).build();
        assert_eq!(prompt_with_missing, prompt_baseline);
    }

    #[test]
    fn test_sned_system_md_empty_file_no_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.md");
        std::fs::write(&path, "").expect("write");
        let _g = EnvGuard::set("SNED_SYSTEM_MD", path.to_str().unwrap());
        let ctx = SystemPromptContext::default();
        let prompt_with_empty = PromptBuilder::new(ctx).build();
        drop(_g);
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: env var mutation is serialized via ENV_TEST_LOCK.
        unsafe {
            std::env::remove_var("SNED_SYSTEM_MD");
        }
        let prompt_baseline = PromptBuilder::new(SystemPromptContext::default()).build();
        assert_eq!(prompt_with_empty, prompt_baseline);
    }
}
