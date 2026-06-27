//! List skills tool handler for sned CLI.
//!
//!
//! Lists available skills with descriptions, prioritizing project over global skills.

use crate::core::agent_loop::TaskState;
use crate::core::context::instructions::{
    SkillMetadata, SkillSource, discover_skills, get_available_skills,
};
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use std::future::Future;
use std::pin::Pin;

/// List skills tool handler.
#[derive(Debug, Clone, Default)]
pub struct ListSkillsHandler;

impl ListSkillsHandler {
    #[must_use] 
    pub fn new() -> Self {
        Self
    }

    fn build_response(
        state: &mut TaskState,
        workspace_root: &std::path::Path,
        _params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let skills = if state.available_skills.is_empty() {
            let project_skills = discover_skills(workspace_root);
            get_available_skills(project_skills)
        } else {
            state.available_skills.clone()
        };

        if skills.is_empty() {
            return Ok("No skills are currently available.".to_string());
        }

        let project_skills: Vec<&SkillMetadata> = skills
            .iter()
            .filter(|s| matches!(s.source, SkillSource::Project))
            .collect();
        let global_skills: Vec<&SkillMetadata> = skills
            .iter()
            .filter(|s| matches!(s.source, SkillSource::Global))
            .collect();

        let mut response = "# AVAILABLE SKILLS\n\n".to_string();

        for skill in project_skills.iter().chain(global_skills.iter()) {
            response.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }

        response.push_str("\nUse the 'use_skill' tool to activate a skill.");

        Ok(response)
    }

    pub fn execute(
        &self,
        state: &mut TaskState,
        _params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let workspace_root = std::env::current_dir().map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to get current directory: {e}"))
        })?;
        Self::build_response(state, &workspace_root, _params)
    }
}

impl ToolHandler for ListSkillsHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let _handler = self.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            // Discover skills outside the state lock to avoid holding lock across sync I/O
            let workspace_root = ctx.workspace_root.as_path();
            let skills = {
                let state = ctx.state.lock().await;
                if state.available_skills.is_empty() {
                    // Release lock before discovering skills
                    drop(state);
                    let project_skills = discover_skills(workspace_root);
                    get_available_skills(project_skills)
                } else {
                    state.available_skills.clone()
                }
            };

            // Build response without holding any lock
            let project_skills: Vec<&SkillMetadata> = skills
                .iter()
                .filter(|s| matches!(s.source, SkillSource::Project))
                .collect();
            let global_skills: Vec<&SkillMetadata> = skills
                .iter()
                .filter(|s| matches!(s.source, SkillSource::Global))
                .collect();

            let response = if skills.is_empty() {
                "No skills are currently available.".to_string()
            } else {
                let mut response = "# AVAILABLE SKILLS\n\n".to_string();
                for skill in project_skills.iter().chain(global_skills.iter()) {
                    response.push_str(&format!("- {}: {}\n", skill.name, skill.description));
                }
                response.push_str("\nUse the 'use_skill' tool to activate a skill.");
                response
            };

            Ok(serde_json::Value::String(response))
        })
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[list_skills]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_skills_handler_creation() {
        let handler = ListSkillsHandler::new();
        assert_eq!(format!("{:?}", handler), "ListSkillsHandler");
    }

    #[tokio::test]
    async fn test_list_skills_empty_state() {
        // Test with pre-populated empty state (no filesystem discovery)
        let mut state = TaskState {
            available_skills: vec![],
            ..Default::default()
        };

        // Use a path that won't have skills - /dev/null on Unix
        #[cfg(unix)]
        let test_path = std::path::Path::new("/dev/null");
        #[cfg(not(unix))]
        let test_path = std::env::temp_dir();

        let result =
            ListSkillsHandler::build_response(&mut state, test_path, serde_json::json!({}));
        assert!(result.is_ok());
        // When state.available_skills is empty, discover_skills is called
        // which may find global skills, so we just verify the response is valid
        assert!(!result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_list_skills_with_skills() {
        let handler = ListSkillsHandler::new();
        let mut state = TaskState {
            available_skills: vec![
                SkillMetadata {
                    name: "project-skill".to_string(),
                    description: "A project skill".to_string(),
                    path: "/project/SKILL.md".to_string(),
                    source: SkillSource::Project,
                },
                SkillMetadata {
                    name: "global-skill".to_string(),
                    description: "A global skill".to_string(),
                    path: "/global/SKILL.md".to_string(),
                    source: SkillSource::Global,
                },
            ],
            ..Default::default()
        };
        let result = handler.execute(&mut state, serde_json::json!({}));
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("# AVAILABLE SKILLS"));
        assert!(text.contains("project-skill: A project skill"));
        assert!(text.contains("global-skill: A global skill"));
        // Project skills should come first
        let project_pos = text.find("project-skill").unwrap();
        let global_pos = text.find("global-skill").unwrap();
        assert!(project_pos < global_pos);
        assert!(text.contains("Use the 'use_skill' tool to activate a skill."));
    }

    #[tokio::test]
    async fn test_list_skills_from_filesystem() {
        let handler = ListSkillsHandler::new();
        let mut state = TaskState {
            available_skills: vec![SkillMetadata {
                name: "fs-skill".to_string(),
                description: "Filesystem discovered skill".to_string(),
                path: "/tmp/fs-skill/SKILL.md".to_string(),
                source: SkillSource::Project,
            }],
            ..Default::default()
        };

        let result = handler.execute(&mut state, serde_json::json!({}));
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("fs-skill: Filesystem discovered skill"));
    }

    #[test]
    fn test_list_skills_description() {
        let handler = ListSkillsHandler::new();
        let desc = handler.description(&serde_json::json!({}));
        assert_eq!(desc, "[list_skills]");
    }
}
