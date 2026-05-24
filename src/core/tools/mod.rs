//! Tool registry and inventory for sned CLI.
//!
//! Ports behavior from `dirac/src/shared/tools.ts` and
//! `dirac/src/core/task/tools/ToolExecutorCoordinator.ts`.

pub mod definitions;
pub mod handlers;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::core::agent_loop::TaskState;
use crate::core::approval::ApprovalManager;
use crate::core::file_editor::AnchorStateManager;

/// All available Sned tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnedTool {
    AskFollowupQuestion,
    AttemptCompletion,
    ExecuteCommand,
    ReadFile,
    WriteToFile,
    SearchFiles,
    ListFiles,
    WebFetch,
    NewTask,
    PlanModeRespond,
    Condense,
    SummarizeTask,
    UseSkill,
    ListSkills,
    UseSubagents,
    GetFunction,
    GetFileSkeleton,
    FindSymbolReferences,
    EditFile,
    DiagnosticsScan,
    ReplaceSymbol,
    RenameSymbol,
}

/// Shared approval-oriented grouping for tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCategory {
    ReadOnly,
    ReadFiles,
    EditFiles,
    ExecuteCommand,
    WebFetch,
    Other,
}

impl ToolCategory {
    pub const fn is_read_only(self) -> bool {
        matches!(self, ToolCategory::ReadOnly)
    }
}

impl SnedTool {
    /// Returns the string name of the tool.
    pub fn name(&self) -> &'static str {
        match self {
            SnedTool::AskFollowupQuestion => "ask_followup_question",
            SnedTool::AttemptCompletion => "attempt_completion",
            SnedTool::ExecuteCommand => "execute_command",
            SnedTool::ReadFile => "read_file",
            SnedTool::WriteToFile => "write_to_file",
            SnedTool::SearchFiles => "search_files",
            SnedTool::ListFiles => "list_files",
            SnedTool::WebFetch => "web_fetch",
            SnedTool::NewTask => "new_task",
            SnedTool::PlanModeRespond => "plan_mode_respond",
            SnedTool::Condense => "condense",
            SnedTool::SummarizeTask => "summarize_task",
            SnedTool::UseSkill => "use_skill",
            SnedTool::ListSkills => "list_skills",
            SnedTool::UseSubagents => "use_subagents",
            SnedTool::GetFunction => "get_function",
            SnedTool::GetFileSkeleton => "get_file_skeleton",
            SnedTool::FindSymbolReferences => "find_symbol_references",
            SnedTool::EditFile => "edit_file",
            SnedTool::DiagnosticsScan => "diagnostics_scan",
            SnedTool::ReplaceSymbol => "replace_symbol",
            SnedTool::RenameSymbol => "rename_symbol",
        }
    }

    /// Returns the approval category for this tool.
    pub const fn category(self) -> ToolCategory {
        match self {
            SnedTool::ReadFile
            | SnedTool::GetFunction
            | SnedTool::GetFileSkeleton
            | SnedTool::FindSymbolReferences
            | SnedTool::DiagnosticsScan
            | SnedTool::ListFiles
            | SnedTool::SearchFiles
            | SnedTool::UseSkill => ToolCategory::ReadFiles,

            SnedTool::UseSubagents => ToolCategory::Other,

            SnedTool::WriteToFile
            | SnedTool::EditFile
            | SnedTool::ReplaceSymbol
            | SnedTool::RenameSymbol => ToolCategory::EditFiles,

            SnedTool::ExecuteCommand => ToolCategory::ExecuteCommand,
            SnedTool::WebFetch => ToolCategory::WebFetch,

            SnedTool::ListSkills
            | SnedTool::AttemptCompletion
            | SnedTool::PlanModeRespond
            | SnedTool::AskFollowupQuestion
            | SnedTool::Condense
            | SnedTool::SummarizeTask => ToolCategory::ReadOnly,

            _ => ToolCategory::Other,
        }
    }

    /// Parses a tool name string into a SnedTool.
    pub fn from_name(name: &str) -> Option<SnedTool> {
        match name {
            "ask_followup_question" => Some(SnedTool::AskFollowupQuestion),
            "attempt_completion" => Some(SnedTool::AttemptCompletion),
            "execute_command" => Some(SnedTool::ExecuteCommand),
            "read_file" => Some(SnedTool::ReadFile),
            "write_to_file" => Some(SnedTool::WriteToFile),
            "search_files" => Some(SnedTool::SearchFiles),
            "list_files" => Some(SnedTool::ListFiles),
            "web_fetch" => Some(SnedTool::WebFetch),
            "new_task" => Some(SnedTool::NewTask),
            "plan_mode_respond" => Some(SnedTool::PlanModeRespond),
            "condense" => Some(SnedTool::Condense),
            "summarize_task" => Some(SnedTool::SummarizeTask),
            "use_skill" => Some(SnedTool::UseSkill),
            "list_skills" => Some(SnedTool::ListSkills),
            "use_subagents" => Some(SnedTool::UseSubagents),
            "get_function" => Some(SnedTool::GetFunction),
            "get_file_skeleton" => Some(SnedTool::GetFileSkeleton),
            "find_symbol_references" => Some(SnedTool::FindSymbolReferences),
            "edit_file" => Some(SnedTool::EditFile),
            "diagnostics_scan" => Some(SnedTool::DiagnosticsScan),
            "replace_symbol" => Some(SnedTool::ReplaceSymbol),
            "rename_symbol" => Some(SnedTool::RenameSymbol),
            _ => None,
        }
    }
}

/// Registry of tool handlers.
pub struct ToolRegistry {
    handlers: HashMap<SnedTool, Arc<dyn ToolHandler>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::with_capacity(16),
        }
    }

    /// Register a tool handler.
    pub fn register(&mut self, tool: SnedTool, handler: Arc<dyn ToolHandler>) {
        self.handlers.insert(tool, handler);
    }

    /// Get a handler for a tool.
    pub fn get_handler(&self, tool: &SnedTool) -> Option<Arc<dyn ToolHandler>> {
        self.handlers.get(tool).cloned()
    }

    /// Check if a handler is registered.
    pub fn has_handler(&self, tool: &SnedTool) -> bool {
        self.handlers.contains_key(tool)
    }
}

/// Shared execution context passed to all tool handlers.
#[derive(Clone)]
pub struct ToolContext {
    pub state: Arc<Mutex<TaskState>>,
    pub approval_manager: Option<Arc<Mutex<ApprovalManager>>>,
    pub workspace_root: PathBuf,
    pub anchor_mgr: AnchorStateManager,
    pub json_output: bool,
    pub task_id: String,
    pub hook_manager: Option<Arc<crate::core::hooks::HookManager>>,
    /// When true, skip safety checks because user explicitly approved this execution.
    /// Safety checks still apply for auto-approved tools (from previous "always" selection).
    pub explicitly_approved: bool,
    /// Output writer for decoupled terminal output.
    pub output_writer: crate::cli::output::OutputWriterArc,
}

impl ToolContext {
    pub fn new(
        state: Arc<Mutex<TaskState>>,
        approval_manager: Option<Arc<Mutex<ApprovalManager>>>,
        workspace_root: PathBuf,
        anchor_mgr: AnchorStateManager,
        json_output: bool,
        task_id: String,
        hook_manager: Option<Arc<crate::core::hooks::HookManager>>,
        explicitly_approved: bool,
        output_writer: crate::cli::output::OutputWriterArc,
    ) -> Self {
        Self {
            state,
            approval_manager,
            workspace_root,
            anchor_mgr,
            json_output,
            task_id,
            hook_manager,
            explicitly_approved,
            output_writer,
        }
    }
}

/// Sanitize and resolve a path relative to the workspace root.
///
/// Rejects path traversal attempts (`..` sequences) and absolute paths
/// that escape the workspace. Returns an error for unsafe paths instead
/// of silently proceeding.
///
/// # Security
///
/// This function canonicalizes existing paths to resolve symlinks before
/// validation, preventing TOCTOU attacks where a symlink inside the workspace
/// points outside and is swapped after validation.
pub fn resolve_sanitized_path(
    workspace_root: &std::path::Path,
    path: &str,
) -> Result<std::path::PathBuf, ToolError> {
    use std::path::{Component, Path};

    let path = Path::new(path);

    // Resolve against workspace root, allowing absolute paths within the workspace
    let resolved: std::path::PathBuf = if path.is_absolute() {
        // Accept absolute paths that are within the workspace root
        if path.starts_with(workspace_root) {
            path.to_path_buf()
        } else {
            return Err(ToolError::InvalidInput(format!(
                "Absolute paths outside workspace are not allowed: {} \
                 (workspace root: {})",
                path.display(),
                workspace_root.display()
            )));
        }
    } else {
        workspace_root.join(path)
    };

    // Normalize by stripping `..` and `.` components manually so we can
    // detect traversal without requiring the path to exist.
    let mut normalized = std::path::PathBuf::new();
    for component in resolved.components() {
        match component {
            Component::Normal(c) => normalized.push(c),
            Component::RootDir => {
                // keep root so we stay absolute
                normalized.push(component);
            }
            Component::Prefix(_) => {
                normalized.push(component);
            }
            Component::CurDir => { /* skip */ }
            Component::ParentDir => {
                // Pop first, then check if we're still within workspace.
                // This prevents `/workspace/foo/../../etc/passwd` by detecting
                // when `..` would escape the workspace root.
                normalized.pop();
                if !normalized.starts_with(workspace_root) {
                    return Err(ToolError::InvalidInput(format!(
                        "Path traversal attempt detected: {}",
                        path.display()
                    )));
                }
            }
        }
    }

    // Final check: the normalized path must still start with workspace_root
    if !normalized.starts_with(workspace_root) {
        return Err(ToolError::InvalidInput(format!(
            "Path escapes workspace: {}",
            path.display()
        )));
    }

    let canonical_root =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());

    if normalized.exists() {
        let canonical = std::fs::canonicalize(&normalized).map_err(|e| {
            ToolError::InvalidInput(format!(
                "Failed to resolve path: {} ({})",
                path.display(),
                e
            ))
        })?;

        if !canonical.starts_with(&canonical_root) {
            return Err(ToolError::InvalidInput(format!(
                "Resolved path escapes workspace via symlink: {} -> {}",
                path.display(),
                canonical.display()
            )));
        }

        return Ok(canonical);
    }

    if let Some(parent) = normalized.parent()
        && parent.exists()
    {
        let canonical_parent = std::fs::canonicalize(parent).map_err(|e| {
            ToolError::InvalidInput(format!(
                "Failed to resolve parent path: {} ({})",
                parent.display(),
                e
            ))
        })?;

        if !canonical_parent.starts_with(&canonical_root) {
            return Err(ToolError::InvalidInput(format!(
                "Resolved parent path escapes workspace via symlink: {} -> {}",
                path.display(),
                canonical_parent.display()
            )));
        }

        return Ok(canonical_parent.join(normalized.file_name().unwrap_or_default()));
    }

    Ok(normalized)
}

/// Trait for tool handlers.
#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync {
    /// Execute the tool with the given input.
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError>;

    /// Get a description of what the tool does.
    fn description(&self, params: &serde_json::Value) -> String;
}

/// Errors from tool execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Cancelled")]
    Cancelled,
}

/// Convert a tool result value into plain text for conversation history.
/// Uses compact JSON to minimize token usage in conversation history.
pub fn tool_result_to_text(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text,
        other => serde_json::to_string(&other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Coerce a JSON value to a `Vec<String>`.
///
/// Accepts both arrays and single strings, so tool handlers work correctly
/// regardless of whether the provider sends `{"paths": ["file.rs"]}` (proper
/// array) or `{"paths": "file.rs"}` (scalar from XML-limited providers like
/// MiniMax M2). Also falls back to a singular key (e.g. `"path"` vs `"paths"`).
pub fn coerce_string_array(
    params: &serde_json::Value,
    plural_key: &str,
    singular_key: &str,
) -> Vec<String> {
    if let Some(arr) = params.get(plural_key).and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
    }

    if let Some(s) = params.get(plural_key).and_then(|v| v.as_str()) {
        return vec![s.to_string()];
    }

    params
        .get(singular_key)
        .and_then(|v| v.as_str())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct DummyHandler;

    #[async_trait]
    impl ToolHandler for DummyHandler {
        async fn execute(
            &self,
            _ctx: &ToolContext,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            Ok(params)
        }

        fn description(&self, _params: &serde_json::Value) -> String {
            "dummy".to_string()
        }
    }

    #[test]
    fn test_tool_categories_cover_known_tools() {
        assert_eq!(SnedTool::ReadFile.category(), ToolCategory::ReadFiles);
        assert_eq!(SnedTool::EditFile.category(), ToolCategory::EditFiles);
        assert_eq!(
            SnedTool::ExecuteCommand.category(),
            ToolCategory::ExecuteCommand
        );
        assert_eq!(SnedTool::WebFetch.category(), ToolCategory::WebFetch);
        assert_eq!(
            SnedTool::AttemptCompletion.category(),
            ToolCategory::ReadOnly
        );
        assert_eq!(SnedTool::Condense.category(), ToolCategory::ReadOnly);
    }

    #[test]
    fn test_tool_registry_round_trip() {
        let mut registry = ToolRegistry::new();
        registry.register(SnedTool::Condense, Arc::new(DummyHandler));

        let handler = registry.get_handler(&SnedTool::Condense);
        assert!(handler.is_some());
        assert_eq!(
            handler.unwrap().description(&serde_json::json!({})),
            "dummy"
        );
    }

    #[test]
    fn test_resolve_sanitized_path_rejects_absolute_outside_workspace() {
        let workspace = std::path::Path::new("/tmp/workspace");
        let result = resolve_sanitized_path(workspace, "/etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Absolute paths outside workspace are not allowed"));
    }

    #[test]
    fn test_resolve_sanitized_path_allows_absolute_within_workspace() {
        let workspace = std::path::Path::new("/tmp/workspace");
        let result = resolve_sanitized_path(workspace, "/tmp/workspace/subdir/file.rs");
        assert!(result.is_ok());
        let path = result.unwrap();
        assert_eq!(path, std::path::Path::new("/tmp/workspace/subdir/file.rs"));
    }

    #[test]
    fn test_resolve_sanitized_path_rejects_traversal() {
        let workspace = std::path::Path::new("/tmp/workspace");
        let result = resolve_sanitized_path(workspace, "../etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("traversal") || err.contains("escapes workspace"));
    }

    #[test]
    fn test_resolve_sanitized_path_rejects_nested_traversal() {
        let workspace = std::path::Path::new("/tmp/workspace");
        let result = resolve_sanitized_path(workspace, "foo/bar/../../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_sanitized_path_allows_normal_relative() {
        let workspace = std::path::Path::new("/tmp/workspace");
        let result = resolve_sanitized_path(workspace, "src/main.rs").unwrap();
        assert_eq!(
            result,
            std::path::PathBuf::from("/tmp/workspace/src/main.rs")
        );
    }

    #[test]
    fn test_resolve_sanitized_path_allows_subdir_traversal_within_workspace() {
        let workspace = std::path::Path::new("/tmp/workspace");
        let result = resolve_sanitized_path(workspace, "foo/../bar/baz.rs").unwrap();
        assert_eq!(
            result,
            std::path::PathBuf::from("/tmp/workspace/bar/baz.rs")
        );
    }

    #[test]
    fn test_coerce_string_array_from_array() {
        let params = serde_json::json!({"paths": ["a.rs", "b.rs"]});
        let result = coerce_string_array(&params, "paths", "path");
        assert_eq!(result, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn test_coerce_string_array_from_string() {
        let params = serde_json::json!({"paths": "tetris.c"});
        let result = coerce_string_array(&params, "paths", "path");
        assert_eq!(result, vec!["tetris.c"]);
    }

    #[test]
    fn test_coerce_string_array_fallback_singular() {
        let params = serde_json::json!({"path": "single.rs"});
        let result = coerce_string_array(&params, "paths", "path");
        assert_eq!(result, vec!["single.rs"]);
    }

    #[test]
    fn test_coerce_string_array_empty() {
        let params = serde_json::json!({});
        let result = coerce_string_array(&params, "paths", "path");
        assert!(result.is_empty());
    }
}
