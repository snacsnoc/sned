//! System prompt construction for sned CLI.
//!

use super::instructions::{RuleToggles, SkillMetadata, SkillSource};
use crate::core::tools::SnedTool;
use crate::core::tools::definitions::ToolProfile;

// ============================================================================
// System Prompt Context
// ============================================================================

/// Context used when building the system prompt.
// Prompt feature toggles are orthogonal; splitting them would multiply
// context types for no behavior gain.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct SystemPromptContext {
    pub cwd: Option<String>,
    pub ide: String,
    pub supports_browser_use: bool,
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
    /// Rule toggles used for path-scoped AGENTS.md discovery after startup.
    pub local_agents_rule_toggles: RuleToggles,
    pub enable_parallel_tool_calling: bool,
    pub user_instructions: Option<String>,
    pub sned_rules: Option<String>,
    /// Active model identifier (e.g. "qwen3.6-35b-a3b", "gpt-4o", "qwen/qwen3.6-35b-a3b").
    /// Used for model-specific prompt routing. None = generic behavior.
    pub model_id: Option<String>,
    /// Tool inventory active for this request. Examples are omitted when the
    /// profile is unknown so prompts cannot advertise unavailable tools.
    pub tool_profile: Option<ToolProfile>,
    /// Operating mode (Plan or Act).
    pub agent_mode: Option<crate::core::agent_types::AgentMode>,
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

    fn has_tool(&self, tool: SnedTool) -> bool {
        self.context
            .tool_profile
            .is_some_and(|profile| profile.tools().contains(&tool))
    }

    fn render_template(&self) -> String {
        let skills_section = self.format_skills_section();

        let mut prompt = "You are Sned, a terminal assistant.\n\n".to_string();

        prompt.push_str(
            "PRIME DIRECTIVES\n\
             - Complete the user's task directly.\n\
             - Minimize turns and load only the context you need.\n\
             - Be concise and direct.\n\n",
        );
        prompt.push_str(
            "STRATEGY\n\
             - Before finishing, check the result against what was asked; state what changed and how it was verified.\n\
             - If a required choice is not specified, pick the conservative option and state the assumption.\n\
             - If the task needs a capability this turn's tools do not include, say which one rather than working around it in prose.\n\n",
        );
        prompt.push_str("EXECUTION\n");

        if self.context.enable_parallel_tool_calling {
            prompt.push_str(
                "- Independent tool calls may be batched; edits based on the same snapshot can share one edit_file call.\n",
            );
        }
        if !self.context.enable_parallel_tool_calling {
            prompt.push_str("- Use tools sequentially.\n");
        }

        prompt
            .push_str("- Use subagents or skills only when they clearly improve result quality.\n");
        if self.context.tool_profile == Some(ToolProfile::DirectAnswer) {
            prompt.push_str("- No tools are available for this turn; answer directly in text.\n");
        } else {
            prompt.push_str("- Workspace reads and writes must use tools, not prose.");
            if self.has_tool(SnedTool::ReadFile) {
                prompt.push_str(" Read files with `read_file`.");
            }
            if self.has_tool(SnedTool::WriteToFile) {
                prompt.push_str(" Create or overwrite complete files with `write_to_file`.");
            }
            if self.has_tool(SnedTool::EditFile) {
                prompt.push_str(" Change existing files with `edit_file`.");
            }
            prompt.push('\n');
        }
        if self.context.tool_profile.is_some() {
            prompt.push_str("- Use only tools supplied for this turn; unavailable tools must not be simulated in prose.\n");
        }
        if self.has_tool(SnedTool::ReadFile)
            || self.has_tool(SnedTool::WriteToFile)
            || self.has_tool(SnedTool::EditFile)
        {
            prompt.push_str(
                "- Use relative paths from the working directory for workspace tools. Absolute paths outside the workspace require explicit user approval; request the needed external directory rather than retrying after a denial.\n",
            );
        }
        if self.has_tool(SnedTool::EditFile) {
            prompt.push_str(
                "- For `edit_file`, use top-level `{path, old_text, new_text}` for one unique block replacement; use `files[]` for batches and anchored or revision-checked edits. For anchors, copy the exact anchor prefix and content from read_file; omit displayed line numbers. Include enough context for `old_text` to match once. Independent edits from the same snapshot can share a call; dependent edits use the updated result. Untouched tracked anchors remain valid. For oversized files, use the ranged read's sha256 revision with start_line, end_line, and expected_text.\n\
                 - In an `edit_file` edit, put replacement text in `text`; the optional `content` field is only an exact array of interior lines for duplicate-anchor disambiguation, never a replacement string.\n\
                 - After a stale or unknown anchor error, call `read_file` again before retrying. Correct malformed input, whitespace mismatches, overlaps, and duplicate insertions without rereading unless the returned recovery metadata explicitly requires it.\n",
            );
        }
        if self.has_tool(SnedTool::WriteToFile) {
            prompt.push_str(
                "- Use `write_to_file` when creating a file or when replacing a complete file and you have the complete desired contents.\n",
            );
        }
        if self.has_tool(SnedTool::ExecuteCommand) {
            prompt.push_str(
                "- Sandboxed commands receive a filtered environment; to pass non-sensitive variables, set SNED_ALLOW_ENV=VAR1,VAR2.\n\
                 - For `execute_command`, send `commands` as a JSON array of strings, not a string containing an array; use `script` for complex run-only logic.\n\
                 - Each `commands[]` entry runs in a fresh shell, so variables, `cd`, and other shell state do not persist between entries or tool calls. Keep dependent statements in one multiline entry or use `script`. Preserve `\\n` escapes and quoted delimiters when a JSON command string contains multiline shell text such as a heredoc.\n",
            );
        }
        let is_act = self.context.agent_mode == Some(crate::core::agent_types::AgentMode::Act)
            || (self.context.agent_mode.is_none() && self.has_tool(SnedTool::AttemptCompletion));
        let is_plan = self.context.agent_mode == Some(crate::core::agent_types::AgentMode::Plan)
            || (self.context.agent_mode.is_none() && self.has_tool(SnedTool::PlanModeRespond));

        if is_act && self.has_tool(SnedTool::AttemptCompletion) {
            prompt.push_str(
                "- In ACT mode, complete the task using tools as needed, then provide a final text response or call `attempt_completion`.\n",
            );
        }
        if is_plan && self.has_tool(SnedTool::PlanModeRespond) {
            prompt.push_str(
                "- In PLAN mode, gather needed context and finish with `plan_mode_respond` without modifying files.\n",
            );
        }
        prompt.push_str(
            "SAFETY AND REFUSAL\n\
             - Refuse unsafe or disallowed requests.\n\
             - Do not delete, reset, overwrite user work, or run destructive commands unless the user explicitly requested it and tool approvals allow it.\n\
             - If required information is missing and available tools cannot get it, ask one focused follow-up question.\n",
        );

        prompt.push_str(
            "\nOUTPUT FORMAT\n\
              - Keep responses short and CLI-friendly.\n\
              - If no tools are needed or available, answer directly in text and keep it brief.\n",
        );

        prompt.push_str(&skills_section);

        let custom_instructions = self.custom_instruction_blocks();
        if !custom_instructions.is_empty() {
            prompt.push_str("\n\n# USER'S CUSTOM INSTRUCTIONS\n\n");
            prompt.push_str(
                "Follow these task, user, and project instructions unless they conflict with safety or with the request formats a tool requires.\n",
            );

            for instructions in custom_instructions {
                prompt.push_str(&format!("\n{instructions}\n"));
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
            "When a listed skill clearly matches the task, activate it before acting. Follow the skill's instructions directly once loaded.\n\n",
        );

        // Prioritize Project skills
        let display_skills = self
            .context
            .skills
            .iter()
            .filter(|s| matches!(s.source, SkillSource::Project))
            .chain(
                self.context
                    .skills
                    .iter()
                    .filter(|s| matches!(s.source, SkillSource::Global)),
            )
            .take(10);

        for skill in display_skills {
            section.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }

        if self.context.skills.len() > 10 {
            section.push_str(&format!(
                "\n... and {} more available skills are present in this workspace.\n",
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
            tool_profile: Some(ToolProfile::Full),
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
        assert!(
            prompt.contains("Absolute paths outside the workspace require explicit user approval")
        );
        assert!(prompt.contains("If no tools are needed or available, answer directly in text"));
        assert!(prompt.contains("copy the exact anchor prefix and content"));
        assert!(prompt.contains("Untouched tracked anchors remain valid"));
        assert!(!prompt.lines().any(|line| line.starts_with('\\')));
        assert!(prompt.contains("In ACT mode"));
        assert!(prompt.contains("provide a final text response or call `attempt_completion`"));
        assert!(!prompt.contains("finish with `attempt_completion`"));
        assert!(!prompt.contains("PLAN mode"));
        // Environment info is now provided by context_loader, not in system prompt
        assert!(!prompt.contains("ENVIRONMENT"));
        assert!(!prompt.contains("Current Working Directory:"));
    }

    #[test]
    fn test_system_prompt_plan_mode_guidance() {
        let context = SystemPromptContext {
            tool_profile: Some(ToolProfile::Plan),
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(prompt.contains("In PLAN mode"));
        assert!(!prompt.contains("In ACT mode"));
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

    /// Skills section must not name tools that only appear in the Full/Plan
    /// profile. Telling the model to call `use_skill`/`list_skills` in a
    /// reduced profile (Validate, CoreEdit, etc.) forces hallucination of the
    /// tool name on the next turn.
    #[test]
    fn test_prompt_builder_skills_section_does_not_name_profile_excluded_tools() {
        let context = SystemPromptContext {
            skills: vec![SkillMetadata {
                name: "rust".to_string(),
                description: "Rust programming expertise".to_string(),
                path: ".sned/skills/rust".to_string(),
                source: SkillSource::Project,
            }],
            ..Default::default()
        };

        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(
            !prompt.contains("use_skill"),
            "skills section must not reference the use_skill tool by name: {prompt}"
        );
        assert!(
            !prompt.contains("list_skills"),
            "skills section must not reference the list_skills tool by name: {prompt}"
        );
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
    fn test_validate_profile_prompt_stays_compact() {
        let context = SystemPromptContext {
            enable_parallel_tool_calling: true,
            tool_profile: Some(ToolProfile::Validate),
            model_id: Some("qwen3.5-35b-a3b".to_string()),
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        eprintln!("validate prompt bytes: {}", prompt.len());
        assert!(prompt.contains("same snapshot can share"));
        assert!(prompt.contains("ranged read's sha256 revision"));
        assert!(
            prompt.len() < 8850,
            "validate prompt is {} bytes, budget 8850",
            prompt.len()
        );
    }

    #[test]
    fn test_prompt_builder_does_not_prescribe_file_creation_workflow() {
        let context = SystemPromptContext {
            tool_profile: Some(ToolProfile::Full),
            ..Default::default()
        };
        let builder = PromptBuilder::new(context);
        let prompt = builder.build();

        assert!(!prompt.contains("write a small skeleton first"));
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
        assert!(prompt.contains("Independent tool calls may be batched"));
        assert!(prompt.contains("edits based on the same snapshot"));
    }

    #[test]
    fn test_prompt_builder_sequential_mode_has_sequential_guidance() {
        let context = SystemPromptContext {
            enable_parallel_tool_calling: false,
            tool_profile: Some(ToolProfile::CoreEdit),
            ..Default::default()
        };
        let prompt = PromptBuilder::new(context).build();
        assert!(prompt.contains("Use tools sequentially"));
        assert!(!prompt.contains("Independent tool calls may be batched"));
    }

    #[test]
    fn direct_answer_prompt_does_not_require_a_completion_tool() {
        let prompt = PromptBuilder::new(SystemPromptContext {
            tool_profile: Some(ToolProfile::DirectAnswer),
            ..Default::default()
        })
        .build();
        assert!(prompt.contains("No tools are available"));
        assert!(!prompt.contains("completion tool"));
        assert!(!prompt.contains("attempt_completion"));
    }

    #[test]
    fn strategy_guidance_appears_in_every_profile() {
        for profile in [
            ToolProfile::LeanAct,
            ToolProfile::DirectAnswer,
            ToolProfile::AnswerOnly,
            ToolProfile::WriteOnly,
            ToolProfile::CoreEdit,
            ToolProfile::Validate,
            ToolProfile::Symbol,
            ToolProfile::Plan,
            ToolProfile::Full,
        ] {
            let context = SystemPromptContext {
                tool_profile: Some(profile),
                ..Default::default()
            };
            let prompt = PromptBuilder::new(context).build();
            for line in [
                "check the result against what was asked",
                "pick the conservative option and state the assumption",
                "say which one rather than working around it in prose",
            ] {
                assert!(
                    prompt.contains(line),
                    "{profile:?} prompt must include strategy guidance: {line}"
                );
            }
        }
        let direct = PromptBuilder::new(SystemPromptContext {
            tool_profile: Some(ToolProfile::DirectAnswer),
            ..Default::default()
        })
        .build();
        assert!(direct.contains("No tools are available for this turn"));
    }

    #[test]
    fn generated_prompts_are_neutral_across_models_and_profiles() {
        for model in [None, Some("qwen3.5-27b"), Some("gpt-4o")] {
            for profile in [ToolProfile::Validate, ToolProfile::Full, ToolProfile::Plan] {
                let context = SystemPromptContext {
                    model_id: model.map(str::to_string),
                    tool_profile: Some(profile),
                    ..Default::default()
                };
                let prompt = PromptBuilder::new(context).build();
                for unwanted in [
                    "cargo test",
                    "EXAMPLE TOOL CALLS",
                    "QWEN MODEL GUIDANCE",
                    "baseline status",
                    "CODE GENERATION",
                    "write a small skeleton first",
                ] {
                    assert!(
                        !prompt.contains(unwanted),
                        "unexpected guidance {unwanted:?} in {model:?}/{profile:?}"
                    );
                }
                assert!(prompt.contains("terminal assistant"));
                assert!(prompt.contains("Each `commands[]` entry runs in a fresh shell"));
            }
        }
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
