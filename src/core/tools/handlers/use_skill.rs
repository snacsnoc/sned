//! Use skill tool handler for sned CLI.
//!
//!
//! Activates a skill by name, injecting its instructions into the task context.

use crate::core::agent_loop::TaskState;
use crate::core::context::instructions::{
    SkillMetadata, discover_skills, get_available_skills, get_skill_content, list_supporting_files,
};
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use std::future::Future;
use std::pin::Pin;
use std::path::Path;

/// Use skill tool handler.
#[derive(Debug, Clone, Default)]
pub struct UseSkillHandler;

impl UseSkillHandler {
    #[must_use] 
    pub fn new() -> Self {
        Self
    }

    /// Discover available skills lazily.
    fn discover_available_skills(workspace_root: &Path) -> Vec<SkillMetadata> {
        let project_skills = discover_skills(workspace_root);
        get_available_skills(project_skills)
    }

    fn execute_with_workspace_root(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        workspace_root: &Path,
    ) -> Result<String, ToolError> {
        let skill_name = params
            .get("skill_name")
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                state.consecutive_mistakes += 1;
                tracing::warn!(
                    consecutive_mistakes = state.consecutive_mistakes,
                    "use_skill: missing skill_name parameter"
                );
                ToolError::InvalidInput(
                    "Missing required parameter 'skill_name'. Please provide the name of the skill to activate.".to_string(),
                )
            })?;

        state.consecutive_mistakes = 0;

        let available_skills = if state.available_skills.is_empty() {
            Self::discover_available_skills(workspace_root)
        } else {
            state.available_skills.clone()
        };

        if available_skills.is_empty() {
            return Ok(
                "Error: No skills are available. Skills may be disabled or not configured."
                    .to_string(),
            );
        }

        let skill_content = get_skill_content(skill_name, &available_skills);

        if skill_content.is_none() {
            let available_names: Vec<String> =
                available_skills.iter().map(|s| s.name.clone()).collect();
            return Ok(format!(
                "Error: Skill \"{}\" not found. Available skills: {}",
                skill_name,
                if available_names.is_empty() {
                    "none".to_string()
                } else {
                    available_names.join(", ")
                }
            ));
        }

        let skill_content = skill_content.unwrap();
        let skill_md_path = std::path::Path::new(&skill_content.path);
        let supporting = list_supporting_files(skill_md_path);
        let skill_dir = skill_md_path
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or(".");

        let mut activation_message = format!(
            "# Skill \"{}\" is now active\n\n{}\n\n---\n",
            skill_content.name, skill_content.instructions
        );
        activation_message.push_str(
            "IMPORTANT: The skill is now loaded. Do NOT call use_skill again for this task. Simply follow the instructions above to complete the user's request.\n",
        );

        if !supporting.docs.is_empty() || !supporting.scripts.is_empty() {
            activation_message.push_str(&format!(
                "\nYou may access supporting files in the skill directory: {skill_dir}/\n"
            ));
            if !supporting.docs.is_empty() {
                activation_message.push_str("\nDocumentation available:\n");
                for doc in &supporting.docs {
                    activation_message.push_str(&format!("- {skill_dir}/docs/{doc}\n"));
                }
            }
            if !supporting.scripts.is_empty() {
                activation_message.push_str("\nScripts available (run via execute_command):\n");
                for script in &supporting.scripts {
                    activation_message.push_str(&format!("- {skill_dir}/scripts/{script}\n"));
                }
            }
        } else {
            activation_message.push_str(&format!(
                "\nYou may access other files in the skill directory at: {skill_dir}"
            ));
        }

        Ok(activation_message)
    }

    pub fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let workspace_root = std::env::current_dir().map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to get current directory: {e}"))
        })?;
        self.execute_with_workspace_root(state, params, &workspace_root)
    }

    /// Execute with pre-discovered skills (avoids holding lock across I/O).
    fn execute_with_skills(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        _workspace_root: &Path,
        available_skills: Vec<SkillMetadata>,
    ) -> Result<String, ToolError> {
        let skill_name = params
            .get("skill_name")
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                state.consecutive_mistakes += 1;
                tracing::warn!(
                    consecutive_mistakes = state.consecutive_mistakes,
                    "use_skill: missing skill_name parameter"
                );
                ToolError::InvalidInput(
                    "Missing required parameter 'skill_name'. Please provide the name of the skill to activate.".to_string(),
                )
            })?;

        state.consecutive_mistakes = 0;

        if available_skills.is_empty() {
            return Ok(
                "Error: No skills are available. Skills may be disabled or not configured."
                    .to_string(),
            );
        }

        let skill_content = get_skill_content(skill_name, &available_skills);

        if skill_content.is_none() {
            let available_names: Vec<String> =
                available_skills.iter().map(|s| s.name.clone()).collect();
            return Ok(format!(
                "Error: Skill \"{}\" not found. Available skills: {}",
                skill_name,
                if available_names.is_empty() {
                    "none".to_string()
                } else {
                    available_names.join(", ")
                }
            ));
        }

        let skill_content = skill_content.unwrap();
        let skill_md_path = std::path::Path::new(&skill_content.path);
        let supporting = list_supporting_files(skill_md_path);
        let skill_dir = skill_md_path
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or(".");

        let mut activation_message = format!(
            "# Skill \"{}\" is now active\n\n{}\n\n---\n",
            skill_content.name, skill_content.instructions
        );
        activation_message.push_str(
            "IMPORTANT: The skill is now loaded. Do NOT call use_skill again for this task. Simply follow the instructions above to complete the user's request.\n",
        );

        if !supporting.docs.is_empty() || !supporting.scripts.is_empty() {
            activation_message.push_str(&format!(
                "\nYou may access supporting files in the skill directory: {skill_dir}/\n"
            ));
            if !supporting.docs.is_empty() {
                activation_message.push_str("\nDocumentation available:\n");
                for doc in &supporting.docs {
                    activation_message.push_str(&format!("- {skill_dir}/docs/{doc}\n"));
                }
            }
            if !supporting.scripts.is_empty() {
                activation_message.push_str("\nScripts available (run via execute_command):\n");
                for script in &supporting.scripts {
                    activation_message.push_str(&format!("- {skill_dir}/scripts/{script}\n"));
                }
            }
        } else {
            activation_message.push_str(&format!(
                "\nYou may access other files in the skill directory at: {skill_dir}"
            ));
        }

        Ok(activation_message)
    }
}

impl ToolHandler for UseSkillHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            // Discover skills outside the state lock to avoid holding lock across sync I/O
            let workspace_root = ctx.workspace_root.as_path();
            let skills_to_use = {
                let state = ctx.state.lock().await;
                if state.available_skills.is_empty() {
                    // Release lock before discovering skills
                    drop(state);
                    Self::discover_available_skills(workspace_root)
                } else {
                    state.available_skills.clone()
                }
            };

            // Re-acquire lock only for state mutation
            let mut state = ctx.state.lock().await;
            let result = handler.execute_with_skills(&mut state, params, workspace_root, skills_to_use)?;
            Ok(serde_json::Value::String(result))
        })
    }

    fn description(&self, params: &serde_json::Value) -> String {
        if let Some(skill_name) = params.get("skill_name").and_then(|s| s.as_str()) {
            format!("[use_skill for \"{skill_name}\"]")
        } else {
            "[use_skill]".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_use_skill_handler_creation() {
        let handler = UseSkillHandler::new();
        assert_eq!(format!("{:?}", handler), "UseSkillHandler");
    }

    #[tokio::test]
    async fn test_use_skill_missing_param() {
        let handler = UseSkillHandler::new();
        let mut state = TaskState::default();
        let result = handler.execute(&mut state, serde_json::json!({}));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("skill_name"));
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_use_skill_not_found() {
        let handler = UseSkillHandler::new();
        let mut state = TaskState {
            available_skills: vec![SkillMetadata {
                name: "other-skill".to_string(),
                description: "Another skill".to_string(),
                path: "/tmp/other/SKILL.md".to_string(),
                source: crate::core::context::instructions::SkillSource::Project,
            }],
            ..Default::default()
        };
        let result = handler
            .execute(
                &mut state,
                serde_json::json!({"skill_name": "missing-skill"}),
            );
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Error: Skill \"missing-skill\" not found"));
        assert!(text.contains("other-skill"));
    }

    #[tokio::test]
    async fn test_use_skill_success() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md_path = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md_path,
            "---\nname: test-skill\ndescription: A test skill\n---\n\nThese are the skill instructions.",
        )
        .unwrap();

        // Create supporting files
        let docs_dir = skill_dir.join("docs");
        std::fs::create_dir_all(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# README").unwrap();

        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        std::fs::write(scripts_dir.join("setup.sh"), "#!/bin/sh").unwrap();

        let handler = UseSkillHandler::new();
        let mut state = TaskState {
            available_skills: vec![SkillMetadata {
                name: "test-skill".to_string(),
                description: "A test skill".to_string(),
                path: skill_md_path.to_string_lossy().to_string(),
                source: crate::core::context::instructions::SkillSource::Project,
            }],
            ..Default::default()
        };

        let result = handler
            .execute(&mut state, serde_json::json!({"skill_name": "test-skill"}));
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Skill \"test-skill\" is now active"));
        assert!(text.contains("These are the skill instructions."));
        assert!(text.contains("IMPORTANT: The skill is now loaded"));
        assert!(text.contains("Documentation available:"));
        assert!(text.contains("readme.md"));
        assert!(text.contains("Scripts available"));
        assert!(text.contains("setup.sh"));
    }

    #[tokio::test]
    async fn test_use_skill_no_supporting_files() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("minimal-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md_path = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md_path,
            "---\nname: minimal-skill\ndescription: Minimal skill\n---\n\nMinimal instructions.",
        )
        .unwrap();

        let handler = UseSkillHandler::new();
        let mut state = TaskState {
            available_skills: vec![SkillMetadata {
                name: "minimal-skill".to_string(),
                description: "Minimal skill".to_string(),
                path: skill_md_path.to_string_lossy().to_string(),
                source: crate::core::context::instructions::SkillSource::Project,
            }],
            ..Default::default()
        };

        let result = handler
            .execute(
                &mut state,
                serde_json::json!({"skill_name": "minimal-skill"}),
            );
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Minimal instructions."));
        assert!(text.contains("You may access other files in the skill directory at:"));
    }

    #[test]
    fn test_use_skill_description() {
        let handler = UseSkillHandler::new();
        let desc = handler.description(&serde_json::json!({"skill_name": "my-skill"}));
        assert_eq!(desc, "[use_skill for \"my-skill\"]");

        let desc2 = handler.description(&serde_json::json!({}));
        assert_eq!(desc2, "[use_skill]");
    }

    #[tokio::test]
    async fn test_use_skill_supporting_file_paths_have_correct_separators() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md_path = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md_path,
            "---\nname: test-skill\ndescription: A test skill\n---\n\nThese are the skill instructions.",
        )
        .unwrap();

        let docs_dir = skill_dir.join("docs");
        std::fs::create_dir_all(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("guide.md"), "# Guide").unwrap();

        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        std::fs::write(scripts_dir.join("run.sh"), "#!/bin/sh").unwrap();

        let handler = UseSkillHandler::new();
        let mut state = TaskState {
            available_skills: vec![SkillMetadata {
                name: "test-skill".to_string(),
                description: "A test skill".to_string(),
                path: skill_md_path.to_string_lossy().to_string(),
                source: crate::core::context::instructions::SkillSource::Project,
            }],
            ..Default::default()
        };

        let result = handler
            .execute(&mut state, serde_json::json!({"skill_name": "test-skill"}));
        assert!(result.is_ok());
        let text = result.unwrap();

        assert!(
            text.contains("/docs/guide.md"),
            "Supporting doc path should have correct separator: expected '/docs/guide.md' in output"
        );
        assert!(
            text.contains("/scripts/run.sh"),
            "Supporting script path should have correct separator: expected '/scripts/run.sh' in output"
        );
        assert!(
            !text.contains("skilldocs/"),
            "Should not have malformed path without separator"
        );
        assert!(
            !text.contains("skillscripts/"),
            "Should not have malformed path without separator"
        );
    }
}
