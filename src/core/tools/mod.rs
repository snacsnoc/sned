//! Tool registry and inventory for sned CLI.
//!
//! Ports behavior from `dirac/src/shared/tools.ts` and
//! `dirac/src/core/task/tools/ToolExecutorCoordinator.ts`.

pub mod definitions;
pub mod handlers;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
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
    PlanModeRespond,
    Condense,
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
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

impl SnedTool {
    /// Returns the string name of the tool.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::AskFollowupQuestion => "ask_followup_question",
            Self::AttemptCompletion => "attempt_completion",
            Self::ExecuteCommand => "execute_command",
            Self::ReadFile => "read_file",
            Self::WriteToFile => "write_to_file",
            Self::SearchFiles => "search_files",
            Self::ListFiles => "list_files",
            Self::WebFetch => "web_fetch",
            Self::PlanModeRespond => "plan_mode_respond",
            Self::Condense => "condense",
            Self::UseSkill => "use_skill",
            Self::ListSkills => "list_skills",
            Self::UseSubagents => "use_subagents",
            Self::GetFunction => "get_function",
            Self::GetFileSkeleton => "get_file_skeleton",
            Self::FindSymbolReferences => "find_symbol_references",
            Self::EditFile => "edit_file",
            Self::DiagnosticsScan => "diagnostics_scan",
            Self::ReplaceSymbol => "replace_symbol",
            Self::RenameSymbol => "rename_symbol",
        }
    }

    /// Returns the approval category for this tool.
    #[must_use]
    pub const fn category(self) -> ToolCategory {
        match self {
            Self::ReadFile
            | Self::GetFunction
            | Self::GetFileSkeleton
            | Self::FindSymbolReferences
            | Self::DiagnosticsScan
            | Self::ListFiles
            | Self::SearchFiles
            | Self::UseSkill => ToolCategory::ReadFiles,

            Self::UseSubagents => ToolCategory::Other,

            Self::WriteToFile | Self::EditFile | Self::ReplaceSymbol | Self::RenameSymbol => {
                ToolCategory::EditFiles
            }

            Self::ExecuteCommand => ToolCategory::ExecuteCommand,
            Self::WebFetch => ToolCategory::WebFetch,

            Self::ListSkills
            | Self::AttemptCompletion
            | Self::PlanModeRespond
            | Self::AskFollowupQuestion
            | Self::Condense => ToolCategory::ReadOnly,
        }
    }

    /// Parses a tool name string into a SnedTool.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "ask_followup_question" => Some(Self::AskFollowupQuestion),
            "attempt_completion" => Some(Self::AttemptCompletion),
            "execute_command" => Some(Self::ExecuteCommand),
            "read_file" => Some(Self::ReadFile),
            "write_to_file" => Some(Self::WriteToFile),
            "search_files" => Some(Self::SearchFiles),
            "list_files" => Some(Self::ListFiles),
            "web_fetch" => Some(Self::WebFetch),
            "plan_mode_respond" => Some(Self::PlanModeRespond),
            "condense" => Some(Self::Condense),
            "use_skill" => Some(Self::UseSkill),
            "list_skills" => Some(Self::ListSkills),
            "use_subagents" => Some(Self::UseSubagents),
            "get_function" => Some(Self::GetFunction),
            "get_file_skeleton" => Some(Self::GetFileSkeleton),
            "find_symbol_references" => Some(Self::FindSymbolReferences),
            "edit_file" => Some(Self::EditFile),
            "diagnostics_scan" => Some(Self::DiagnosticsScan),
            "replace_symbol" => Some(Self::ReplaceSymbol),
            "rename_symbol" => Some(Self::RenameSymbol),
            _ => None,
        }
    }
}

/// Registry of tool handlers.
pub struct ToolRegistry {
    handlers: HashMap<SnedTool, Arc<dyn ToolHandler + Send + Sync>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            handlers: HashMap::with_capacity(16),
        }
    }

    /// Register a tool handler.
    pub fn register(&mut self, tool: SnedTool, handler: Arc<dyn ToolHandler + Send + Sync>) {
        self.handlers.insert(tool, handler);
    }

    /// Get a handler for a tool.
    #[must_use]
    pub fn get_handler(&self, tool: &SnedTool) -> Option<Arc<dyn ToolHandler + Send + Sync>> {
        self.handlers.get(tool).cloned()
    }

    /// Check if a handler is registered.
    #[must_use]
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
    /// When true, the command matched a reusable approval scope for this session.
    /// The execute-command handler still applies structural safety checks.
    pub session_command_scope_approved: bool,
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
            session_command_scope_approved: false,
            output_writer,
        }
    }
}

/// Internal next-step guidance for runtime recovery handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRequiredNextStep {
    AskUser,
    ReadFile,
    NarrowRead,
}

/// Internal failure classes for tool/runtime recovery handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolFailureClass {
    ApprovalDenied,
    PermissionDenied,
    AnchorInvalid,
    RangeInsufficient,
    RootListingFailed,
}

/// Internal failure metadata carried with tool errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailureMetadata {
    pub class: ToolFailureClass,
    pub affected_paths: Vec<String>,
    pub required_next_step: Option<ToolRequiredNextStep>,
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

    let mut current = workspace_root.to_path_buf();
    let suffix = normalized
        .strip_prefix(workspace_root)
        .unwrap_or(&normalized);

    for component in suffix.components() {
        current.push(component.as_os_str());

        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                if !meta.file_type().is_symlink() {
                    continue;
                }
                let canonical = std::fs::canonicalize(&current).map_err(|e| {
                    ToolError::InvalidInput(format!(
                        "Failed to resolve symlink path: {} ({})",
                        current.display(),
                        e
                    ))
                })?;

                if !canonical.starts_with(&canonical_root) {
                    return Err(ToolError::InvalidInput(format!(
                        "Resolved parent path escapes workspace via symlink: {} -> {}",
                        path.display(),
                        canonical.display()
                    )));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ToolError::InvalidInput(format!(
                    "Failed to inspect path component {} ({})",
                    current.display(),
                    e
                )));
            }
        }
    }

    Ok(normalized)
}

/// Trait for tool handlers.
pub trait ToolHandler: Send + Sync {
    /// Execute the tool with the given input.
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>>;

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
    #[error("Invalid input: {0}")]
    InvalidInputWithMetadata(String, ToolFailureMetadata),
    #[error("Execution failed: {0}")]
    ExecutionFailedWithMetadata(String, ToolFailureMetadata),
}

impl ToolError {
    #[must_use]
    pub fn metadata(&self) -> Option<&ToolFailureMetadata> {
        match self {
            Self::InvalidInputWithMetadata(_, metadata)
            | Self::ExecutionFailedWithMetadata(_, metadata) => Some(metadata),
            Self::InvalidInput(_) | Self::ExecutionFailed(_) => None,
        }
    }
}

/// Convert a tool result value into plain text for conversation history.
/// Uses compact JSON to minimize token usage in conversation history.
#[must_use]
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
    struct DummyHandler;

    impl ToolHandler for DummyHandler {
        fn execute(
            &self,
            _ctx: &ToolContext,
            params: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>>
        {
            Box::pin(async move { Ok(params) })
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
    fn test_resolve_sanitized_path_rejects_sibling_prefix_path() {
        let workspace = std::path::Path::new("/tmp/workspace");
        let result = resolve_sanitized_path(workspace, "/tmp/workspace2/file.rs");
        assert!(result.is_err());
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

    #[cfg(unix)]
    #[test]
    fn test_resolve_sanitized_path_rejects_nested_missing_path_through_symlink() {
        use std::os::unix::fs::symlink;
        let workspace_root = tempfile::tempdir().unwrap();
        let outside_root = tempfile::tempdir().unwrap();
        let symlink_path = workspace_root.path().join("linked");

        symlink(outside_root.path(), &symlink_path).unwrap();

        let result = resolve_sanitized_path(workspace_root.path(), "linked/nested/file.rs");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("symlink") || err.contains("escapes workspace"));
    }
}
