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
    /// Lock-free cancellation flag shared with long-running tool handlers.
    pub cancellation_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Number of consecutive tool failures observed before this tool call.
    ///
    /// The agent loop owns and updates the counter. Handlers only use this
    /// snapshot to make recovery guidance more specific without maintaining a
    /// competing retry counter.
    pub consecutive_failures: u32,
    /// Per-task path locks prevent reads and writes of the same file from racing.
    file_operation_locks: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    pub approval_manager: Option<Arc<Mutex<ApprovalManager>>>,
    pub workspace_root: PathBuf,
    /// Canonical external directory roots authorized for this tool invocation.
    pub allowed_external_roots: Vec<PathBuf>,
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
            cancellation_flag: None,
            consecutive_failures: 0,
            file_operation_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            approval_manager,
            workspace_root,
            allowed_external_roots: Vec::new(),
            anchor_mgr,
            json_output,
            task_id,
            hook_manager,
            explicitly_approved,
            session_command_scope_approved: false,
            output_writer,
        }
    }

    /// Attach the task's lock-free cancellation flag to this context.
    #[must_use]
    pub fn with_cancellation_flag(
        mut self,
        cancellation_flag: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.cancellation_flag = Some(cancellation_flag);
        self
    }

    /// Attach the agent loop's current consecutive-failure count.
    #[must_use]
    pub fn with_consecutive_failures(mut self, consecutive_failures: u32) -> Self {
        self.consecutive_failures = consecutive_failures;
        self
    }

    /// Acquire all requested file locks in sorted order.
    ///
    /// Sorting prevents two multi-file operations from deadlocking while they
    /// acquire overlapping path sets in different model-supplied orders.
    pub async fn lock_file_paths(
        &self,
        paths: &[PathBuf],
    ) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mut keys: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        keys.sort();
        keys.dedup();

        let locks: Vec<Arc<tokio::sync::Mutex<()>>> = {
            let mut registry = self
                .file_operation_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            keys.into_iter()
                .map(|key| {
                    registry
                        .entry(key)
                        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                        .clone()
                })
                .collect()
        };

        let mut guards = Vec::with_capacity(locks.len());
        for lock in locks {
            guards.push(lock.lock_owned().await);
        }
        guards
    }

    /// Resolve a path under the workspace or an external directory that was
    /// explicitly authorized for this invocation.
    pub fn resolve_path(&self, path: &str) -> Result<PathBuf, ToolError> {
        resolve_authorized_path(&self.workspace_root, &self.allowed_external_roots, path)
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

    if !workspace_root.is_absolute() {
        return Err(ToolError::InvalidInput(format!(
            "Workspace root must be an absolute path: {}",
            workspace_root.display()
        )));
    }

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

/// Resolve a file-tool path under the workspace or an explicitly authorized
/// external directory. Callers that do not hold such an authority must keep
/// using [`resolve_sanitized_path`].
pub fn resolve_authorized_path(
    workspace_root: &std::path::Path,
    external_roots: &[PathBuf],
    path: &str,
) -> Result<PathBuf, ToolError> {
    use std::path::{Component, Path};

    let path_obj = Path::new(path);
    if !path_obj.is_absolute() || path_obj.starts_with(workspace_root) {
        return resolve_sanitized_path(workspace_root, path);
    }

    let mut normalized = PathBuf::new();
    for component in path_obj.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::Prefix(component) => normalized.push(component.as_os_str()),
            Component::RootDir => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }

    let canonical_path = canonicalize_existing_parent(&normalized)?;
    if external_roots
        .iter()
        .any(|root| canonical_path.starts_with(root))
    {
        return Ok(canonical_path);
    }

    Err(ToolError::InvalidInput(format!(
        "External path is not authorized for this session: {}. Ask the user to approve its directory or restart with --allow-dir <directory>.",
        path_obj.display()
    )))
}

fn canonicalize_existing_parent(path: &std::path::Path) -> Result<PathBuf, ToolError> {
    let mut cursor = path.to_path_buf();
    let mut missing_components = Vec::new();
    while !cursor.exists() {
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ToolError::InvalidInput(format!(
                    "Failed to resolve symlink path: {} (target does not exist)",
                    cursor.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ToolError::InvalidInput(format!(
                    "Failed to inspect path component {} ({error})",
                    cursor.display()
                )));
            }
        }
        let component = cursor.file_name().ok_or_else(|| {
            ToolError::InvalidInput(format!("Failed to resolve path: {}", path.display()))
        })?;
        missing_components.push(component.to_os_string());
        cursor = cursor
            .parent()
            .ok_or_else(|| {
                ToolError::InvalidInput(format!("Failed to resolve path: {}", path.display()))
            })?
            .to_path_buf();
    }

    let mut canonical = std::fs::canonicalize(&cursor).map_err(|error| {
        ToolError::InvalidInput(format!(
            "Failed to resolve path: {} ({error})",
            path.display()
        ))
    })?;
    for component in missing_components.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
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
/// Accepts arrays, JSON-stringified arrays of strings, and single strings, so
/// tool handlers work correctly regardless of whether the provider sends
/// `{"paths": ["file.rs"]}` (proper array), `{"paths": "[\"file.rs\"]"}`
/// (a valid array encoded as a string), or `{"paths": "file.rs"}` (scalar
/// from XML-limited providers like MiniMax M2). Also falls back to a singular
/// key (e.g. `"path"` vs `"paths"`). Malformed JSON strings remain scalar
/// values so the handler can return a useful validation error.
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
        if let Ok(serde_json::Value::Array(values)) = serde_json::from_str(s) {
            if values.iter().all(serde_json::Value::is_string) {
                return values
                    .into_iter()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect();
            }
        }
        return vec![s.to_string()];
    }

    params
        .get(singular_key)
        .and_then(|v| v.as_str())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

/// Coerce `execute_command.commands` without reinterpreting shell syntax that
/// also happens to be valid JSON-array text. A stringified command array is
/// accepted only when the first array element immediately follows `[` (for
/// example, `["cargo test", "cargo clippy"]`), while shell commands such as
/// `[ "foo" ]` remain one scalar command.
pub fn coerce_command_array(params: &serde_json::Value) -> Vec<String> {
    if let Some(values) = params.get("commands").and_then(|value| value.as_array()) {
        return values
            .iter()
            .filter_map(|value| value.as_str().map(String::from))
            .collect();
    }

    if let Some(value) = params.get("commands").and_then(|value| value.as_str()) {
        if let Some(values) = parse_unambiguous_stringified_string_array(value) {
            return values;
        }
        return vec![value.to_string()];
    }

    params
        .get("command")
        .and_then(|value| value.as_str())
        .map(|value| vec![value.to_string()])
        .unwrap_or_default()
}

/// Parse a stringified JSON array only when its opening is unambiguous. JSON
/// formatting with spaces after `[` is also common shell test syntax and must
/// not be silently converted into a different command. Whitespace between
/// later array elements remains supported for provider compatibility.
pub fn parse_unambiguous_stringified_string_array(value: &str) -> Option<Vec<String>> {
    let trimmed = value.trim();
    if trimmed != "[]" && !trimmed.starts_with("[\"") {
        return None;
    }
    let parsed = serde_json::from_str::<serde_json::Value>(trimmed).ok()?;
    let array = parsed.as_array()?;
    if !array.iter().all(serde_json::Value::is_string) {
        return None;
    }

    Some(
        array
            .iter()
            .filter_map(|value| value.as_str().map(String::from))
            .collect(),
    )
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
    fn test_resolve_sanitized_path_rejects_relative_workspace_root() {
        let result = resolve_sanitized_path(std::path::Path::new("."), "AGENTS.md");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Workspace root must be an absolute path"));
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
    fn test_resolve_authorized_path_allows_only_granted_external_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let target = external.path().join("generated.sql");
        let external_root = external.path().canonicalize().unwrap();

        let denied = resolve_authorized_path(workspace.path(), &[], target.to_str().unwrap());
        assert!(denied.is_err());

        let resolved = resolve_authorized_path(
            workspace.path(),
            std::slice::from_ref(&external_root),
            target.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(resolved, external_root.join("generated.sql"));

        let sibling = external.path().parent().unwrap().join(format!(
            "{}-sibling",
            external.path().file_name().unwrap().to_string_lossy()
        ));
        assert!(
            resolve_authorized_path(
                workspace.path(),
                std::slice::from_ref(&external_root),
                sibling.join("blocked.sql").to_str().unwrap(),
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_authorized_path_rejects_dangling_intermediate_symlink() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let symlink_path = external.path().join("dangling");
        symlink(external.path().join("not-created"), &symlink_path).unwrap();

        let result = resolve_authorized_path(
            workspace.path(),
            std::slice::from_ref(&external.path().canonicalize().unwrap()),
            symlink_path.join("file.txt").to_str().unwrap(),
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("Failed to resolve symlink path"));
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
    fn test_coerce_string_array_from_stringified_json_array() {
        let params = serde_json::json!({"paths": "[\"src/main.rs\", \"src/lib.rs\"]"});
        let result = coerce_string_array(&params, "paths", "path");
        assert_eq!(result, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn test_coerce_string_array_keeps_malformed_string_as_scalar() {
        let params = serde_json::json!({"paths": "[\"src/main.rs\""});
        let result = coerce_string_array(&params, "paths", "path");
        assert_eq!(result, vec!["[\"src/main.rs\""]);
    }

    #[test]
    fn test_coerce_command_array_preserves_ambiguous_shell_test() {
        let params = serde_json::json!({"commands": "[ \"foo\" ]"});
        assert_eq!(coerce_command_array(&params), vec!["[ \"foo\" ]"]);
    }

    #[test]
    fn test_coerce_command_array_accepts_canonical_stringified_array() {
        let params = serde_json::json!({"commands": "[\"echo one\", \"echo two\"]"});
        assert_eq!(coerce_command_array(&params), vec!["echo one", "echo two"]);
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

    #[tokio::test]
    async fn test_file_operation_locks_serialize_shared_paths() {
        let state = Arc::new(Mutex::new(TaskState::default()));
        let ctx = ToolContext::new(
            state,
            None,
            PathBuf::from("/workspace"),
            AnchorStateManager::new(),
            true,
            "lock-test".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let path = PathBuf::from("/workspace/src/main.rs");
        let guard = ctx.lock_file_paths(std::slice::from_ref(&path)).await;
        let waiting_ctx = ctx.clone();
        let mut waiting = tokio::spawn(async move {
            let _guard = waiting_ctx.lock_file_paths(&[path]).await;
        });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut waiting)
                .await
                .is_err(),
            "a second operation should wait for the shared path lock"
        );
        drop(guard);
        waiting.await.unwrap();
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
