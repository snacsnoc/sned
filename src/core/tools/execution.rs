//! Tool-batch execution mechanics for the agent loop.
//!
//! The agent loop owns turn-level decisions (mistake accounting, plan and
//! completion handling, history persistence); this module owns how a batch of
//! prepared calls becomes ordered outcomes: preflight, approval, scheduling,
//! and pairing results back to provider order.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use futures::future::FutureExt;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use super::definitions::{ToolProfile, get_tool_definitions_for_profile};
use super::{
    SnedTool, ToolContext, ToolError, ToolFailureClass, ToolFailureMetadata, ToolHandler,
    ToolPublicationOutcome, ToolRegistry, ToolRequiredNextStep, coerce_command_array,
    coerce_string_array, resolve_sanitized_path, tool_result_to_text,
};
use crate::cli::output::OutputWriterArc;
use crate::core::agent_types::{AgentConfig, AgentMode, DeniedToolAction, TaskState};
use crate::core::approval::ApprovalManager;
use crate::core::hooks::HookManager;
use crate::providers::{ApiStreamToolCall, MAX_TOOL_ARGUMENT_SIZE, StorageMessage};
use crate::storage::task_storage::TaskStorage;

/// Default concurrency limit for parallel non-grouped tool execution.
/// Prevents I/O contention when many tools run simultaneously.
pub(crate) const DEFAULT_TOOL_CONCURRENCY: usize = 12;

#[derive(Debug, Clone)]
pub(crate) struct ToolExecutionOutput {
    pub(crate) text: String,
    pub(crate) metadata: Option<ToolFailureMetadata>,
    pub(crate) is_error: bool,
    pub(crate) hook_context: Vec<String>,
    pub(crate) publication_outcomes: Vec<ToolPublicationOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileActionPath {
    pub(crate) normalized: String,
    pub(crate) display: String,
}

pub(crate) struct PreparedToolCall {
    pub(crate) tool_call: ApiStreamToolCall,
    pub(crate) tool_id: String,
    pub(crate) tool_name: String,
    pub(crate) parsed_args: Result<serde_json::Value, String>,
}

pub(crate) struct ToolTask {
    tool_id: String,
    tool_name: String,
    immediate: Option<ToolExecutionOutput>,
    task: Option<futures::future::BoxFuture<'static, ToolExecutionOutput>>,
    edit_file_paths: Vec<FileActionPath>,
    tool_params: serde_json::Value,
}

/// Everything batch execution needs beyond the prepared calls. Resolved by
/// the caller so this module never reaches into agent-loop internals.
pub(crate) struct ToolBatchInputs {
    pub(crate) mode: AgentMode,
    pub(crate) state: Arc<Mutex<TaskState>>,
    pub(crate) tool_profile: Option<ToolProfile>,
    pub(crate) registry: Option<Arc<ToolRegistry>>,
    pub(crate) approval_manager: Option<Arc<Mutex<ApprovalManager>>>,
    pub(crate) output_writer: OutputWriterArc,
    pub(crate) interactive_mode: bool,
    pub(crate) tool_context: Arc<ToolContext>,
    pub(crate) hook_manager: Option<Arc<HookManager>>,
    pub(crate) config: AgentConfig,
    pub(crate) conversation_history: Arc<Mutex<Vec<StorageMessage>>>,
    pub(crate) task_storage: Option<Arc<TaskStorage>>,
    pub(crate) scoped_rules_added: bool,
    pub(crate) parallel_enabled: bool,
    pub(crate) cancelled: Arc<AtomicBool>,
}

pub(crate) struct OrderedToolOutcome {
    pub(crate) tool_id: String,
    pub(crate) tool_name: String,
    pub(crate) tool_params: serde_json::Value,
    pub(crate) edit_file_paths: Vec<FileActionPath>,
    pub(crate) output: ToolExecutionOutput,
    pub(crate) executed: bool,
}

pub(crate) struct ToolBatchOutput {
    pub(crate) outcomes: Vec<OrderedToolOutcome>,
    pub(crate) tools_called: bool,
    pub(crate) tool_failure_count: usize,
}

impl ToolExecutionOutput {
    pub(crate) fn error(text: String, metadata: Option<ToolFailureMetadata>) -> Self {
        Self {
            text,
            metadata,
            is_error: true,
            hook_context: Vec::new(),
            publication_outcomes: Vec::new(),
        }
    }

    pub(crate) fn success_with_hook_context(text: String, hook_context: Vec<String>) -> Self {
        Self {
            text,
            metadata: None,
            is_error: false,
            hook_context,
            publication_outcomes: Vec::new(),
        }
    }

    fn error_with_hook_context(
        text: String,
        metadata: Option<ToolFailureMetadata>,
        hook_context: Vec<String>,
        publication_outcomes: Vec<ToolPublicationOutcome>,
    ) -> Self {
        Self {
            text,
            metadata,
            is_error: true,
            hook_context,
            publication_outcomes,
        }
    }

    fn cancelled_before_start() -> Self {
        Self::error(
            "Tool execution skipped: task was cancelled before the tool started".to_string(),
            None,
        )
    }
}

/// Resolve a prepared but unstarted tool future. Cancellation observed here
/// must not begin new work; an already-started future runs to completion so
/// a file transaction is never dropped mid-application.
async fn run_tool_unless_cancelled(
    cancelled: Arc<AtomicBool>,
    task: futures::future::BoxFuture<'static, ToolExecutionOutput>,
) -> ToolExecutionOutput {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        ToolExecutionOutput::cancelled_before_start()
    } else {
        task.await
    }
}

pub(crate) fn parse_tool_arguments(
    tool_name: &str,
    tool_id: &str,
    raw_arguments: Option<&String>,
) -> Result<serde_json::Value, String> {
    let Some(raw) = raw_arguments else {
        return Ok(serde_json::json!({}));
    };
    // Oversized arguments are never executable, no matter how valid the
    // JSON looks after repair. Fail the call; the stream assembly below
    // keeps draining so the protocol stays in sync.
    if raw.len() > MAX_TOOL_ARGUMENT_SIZE {
        let preview: String = raw.chars().take(200).collect();
        error!(
            tool_name = %tool_name,
            tool_id = %tool_id,
            args_len = raw.len(),
            args_preview = %preview,
            "tool call arguments exceed size limit; call will not execute"
        );
        return Err(format!(
            "Tool '{tool_name}' arguments exceed the {MAX_TOOL_ARGUMENT_SIZE}-byte limit ({} bytes, id: {tool_id}) and were not executed. Please retry with smaller arguments. Rejected argument prefix: {preview}",
            raw.len()
        ));
    }
    // Treat empty string as no arguments (some providers send empty string instead of "{}")
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(mut parsed) => {
            if let Some(parse_error) = crate::providers::tool_arguments_error(&parsed) {
                return Err(format!(
                    "Tool '{tool_name}' arguments could not be repaired as JSON (id: {tool_id}): {parse_error} Please retry the same tool call with valid JSON arguments."
                ));
            }
            let normalized = match tool_name {
                "execute_command" => {
                    if let Some(raw) = parsed
                        .get("commands")
                        .and_then(serde_json::Value::as_str)
                        .filter(|raw| raw.trim().starts_with("[\""))
                    {
                        let commands = super::parse_unambiguous_stringified_string_array(raw)
                            .or_else(|| super::parse_relaxed_stringified_string_array(raw))
                            .ok_or_else(|| format!(
                                "Tool '{tool_name}' arguments contain an ambiguous stringified 'commands' array (id: {tool_id}). Re-issue the tool call with a literal JSON array of command strings."
                            ))?;
                        parsed["commands"] = serde_json::json!(commands);
                        true
                    } else {
                        false
                    }
                }
                "edit_file" => super::handlers::edit_file::EditFileHandler::normalize_stringified_files_param(&mut parsed)
                    .map_err(|error| format!("Tool '{tool_name}' arguments could not be normalized (id: {tool_id}): {error}"))?,
                _ => false,
            };
            if normalized {
                tracing::debug!(tool_name = %tool_name, tool_id = %tool_id, "normalized compatibility tool arguments");
            }
            Ok(parsed)
        }
        Err(err) => {
            let preview: String = raw.chars().take(200).collect();
            error!(
                tool_name = %tool_name,
                tool_id = %tool_id,
                error = %err,
                args_len = raw.len(),
                args_preview = %preview,
                "failed to parse tool call arguments JSON"
            );
            Err(format!(
                "Tool '{tool_name}' arguments were invalid JSON and could not be parsed (id: {tool_id}): {err}. Please retry with valid JSON arguments."
            ))
        }
    }
}

pub(crate) fn prepare_tool_calls(
    tool_call_order: &[String],
    tool_calls_map: &mut HashMap<String, ApiStreamToolCall>,
) -> Vec<PreparedToolCall> {
    let mut prepared = Vec::with_capacity(tool_call_order.len());

    for key in tool_call_order {
        let Some(tool_call) = tool_calls_map.get_mut(key) else {
            error!(
                "Tool call order mismatch: key '{}' not found in tool_calls_map. \
                 This indicates a stream parsing bug.",
                key
            );
            continue;
        };

        if tool_call
            .function
            .id
            .as_ref()
            .is_none_or(std::string::String::is_empty)
        {
            let generated = ulid::Ulid::new().to_string();
            tool_call.function.id = Some(generated);
        }

        let mut tool_call_clone = tool_call.clone();
        let tool_id = tool_call_clone.function.id.take().unwrap_or_else(|| {
            error!("Tool call ID is None after initialization, generating fallback");
            ulid::Ulid::new().to_string()
        });
        let tool_name = tool_call_clone.function.name.take().unwrap_or_else(|| {
            warn!("Tool call missing name, using 'unknown_tool'");
            "unknown_tool".to_string()
        });
        let parsed_args =
            parse_tool_arguments(&tool_name, &tool_id, tool_call.function.arguments.as_ref());

        prepared.push(PreparedToolCall {
            tool_call: tool_call_clone,
            tool_id,
            tool_name,
            parsed_args,
        });
    }

    prepared
}

pub(crate) fn is_plan_mode_restricted(tool: SnedTool) -> bool {
    matches!(tool, SnedTool::WriteToFile | SnedTool::EditFile)
}

/// Extract the first action path from tool params for per-path approval.
///
/// Each tool extracts paths differently:
/// - ReadFile/SearchFiles/ListFiles: `params.paths` (string or string[])
/// - WriteToFile: `params.path` (single string)
/// - EditFile: `params.files[0].path`
/// - ReplaceSymbol: `params.path` or `params.replacements[0].path`
/// - RenameSymbol: `params.paths[0]`
/// - GetFileSkeleton/FindSymbolReferences/DiagnosticsScan: `params.path`
pub(crate) fn extract_action_path(tool: SnedTool, params: &serde_json::Value) -> Vec<String> {
    match tool {
        SnedTool::ReadFile
        | SnedTool::GetFileSkeleton
        | SnedTool::FindSymbolReferences
        | SnedTool::DiagnosticsScan
        | SnedTool::RenameSymbol => coerce_string_array(params, "paths", "path"),
        SnedTool::WriteToFile
        | SnedTool::SearchFiles
        | SnedTool::ListFiles
        | SnedTool::GetFunction => params
            .get("path")
            .and_then(|p| p.as_str())
            .map(|s| vec![String::from(s)])
            .unwrap_or_default(),
        SnedTool::EditFile => {
            super::handlers::edit_file::EditFileHandler::requested_paths_for_locking(params)
        }
        SnedTool::ReplaceSymbol => {
            if let Some(s) = params.get("path").and_then(|p| p.as_str()) {
                vec![String::from(s)]
            } else {
                params
                    .get("replacements")
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|r| r.get("path"))
                            .filter_map(|p| p.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default()
            }
        }
        _ => vec![],
    }
}

pub(crate) fn is_mutating_file_tool(tool_name: &str) -> bool {
    matches!(
        SnedTool::from_name(tool_name),
        Some(
            SnedTool::WriteToFile
                | SnedTool::EditFile
                | SnedTool::ReplaceSymbol
                | SnedTool::RenameSymbol
        )
    )
}

/// Whether a parallel-scheduled call may run inside a read-only batch.
/// Only tools categorized as read-only qualify; unknown names fail
/// closed to barrier so an unrecognized mutation cannot slip into a
/// concurrent batch.
pub(crate) fn tool_is_schedulable_read(tool_name: &str) -> bool {
    SnedTool::from_name(tool_name).is_some_and(|tool| {
        matches!(
            tool.category(),
            super::ToolCategory::ReadOnly | super::ToolCategory::ReadFiles
        )
    })
}

pub(crate) fn external_action_directories(
    tool: SnedTool,
    workspace_root: &std::path::Path,
    action_paths: &[String],
) -> Vec<PathBuf> {
    if !matches!(
        tool.category(),
        super::ToolCategory::ReadFiles | super::ToolCategory::EditFiles
    ) {
        return Vec::new();
    }

    let mut directories = action_paths
        .iter()
        .filter(|path| {
            let path = std::path::Path::new(path);
            path.is_absolute() && !path.starts_with(workspace_root)
        })
        .filter_map(|path| crate::core::approval::external_directory_for_path(path))
        .collect::<Vec<_>>();
    directories.sort();
    directories.dedup();
    directories
}

fn canonicalize_tool_params(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let ordered: std::collections::BTreeMap<_, _> = map
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize_tool_params(value)))
                .collect();
            serde_json::Value::Object(ordered.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_tool_params).collect())
        }
        other => other.clone(),
    }
}

pub(crate) fn tool_params_fingerprint(params: &serde_json::Value) -> String {
    serde_json::to_string(&canonicalize_tool_params(params)).unwrap_or_else(|_| params.to_string())
}

pub(crate) fn extract_file_action_path(
    tool_name: &str,
    params: &serde_json::Value,
    workspace_root: &std::path::Path,
) -> Vec<FileActionPath> {
    let requested_paths = match tool_name {
        "edit_file" => {
            let Some(files) = params.get("files").and_then(|files| files.as_array()) else {
                return vec![];
            };
            let fallback = params.get("path").and_then(|path| path.as_str());
            let use_fallback = fallback.is_some()
                && !files.is_empty()
                && files
                    .iter()
                    .all(|file| file.get("path").is_none() && file.get("edits").is_some());

            files
                .iter()
                .filter_map(|file| {
                    if use_fallback {
                        return fallback.map(String::from);
                    }
                    match file.get("path") {
                        Some(path) => path.as_str().map(String::from),
                        None => file
                            .get("edits")
                            .and_then(|edits| edits.as_array())
                            .and_then(|edits| edits.first())
                            .and_then(|edit| edit.get("path"))
                            .and_then(|path| path.as_str())
                            .map(String::from),
                    }
                })
                .collect()
        }
        "write_to_file" => params
            .get("path")
            .and_then(|path| path.as_str())
            .map(|path| vec![String::from(path)])
            .unwrap_or_default(),
        _ => return vec![],
    };

    let mut seen = std::collections::HashSet::with_capacity(requested_paths.len());
    requested_paths
        .into_iter()
        .filter_map(|display| {
            let normalized = resolve_sanitized_path(workspace_root, &display)
                .ok()?
                .to_string_lossy()
                .into_owned();
            if !seen.insert(normalized.clone()) {
                return None;
            }
            Some(FileActionPath {
                normalized,
                display,
            })
        })
        .collect()
}

/// Static version of execute_tool_with_hooks for parallel execution.
/// Takes ownership of shared resources to avoid borrowing issues across async boundaries.
pub(crate) async fn execute_tool_with_hooks_internal(
    config: &AgentConfig,
    hook_manager: Option<Arc<HookManager>>,
    tool_context: Arc<ToolContext>,
    tool_name: &str,
    tool_params: &serde_json::Value,
    handler: Arc<dyn ToolHandler>,
    task_storage: Option<Arc<TaskStorage>>,
    conversation_history: Arc<Mutex<Vec<StorageMessage>>>,
) -> ToolExecutionOutput {
    let mut params_for_execution = tool_params.clone();
    let mut hook_context = Vec::new();
    if let Some(ref hook_mgr) = hook_manager {
        let pre_result = hook_mgr.pre_tool_use(&config.task_id, tool_name, tool_params);
        if let Some(error) = pre_result.error.as_deref() {
            warn!(tool = tool_name, error, "PreToolUse hook reported an error");
        }
        if let Some(output) = pre_result.output {
            if let Some(error) = output.error_message.as_deref() {
                warn!(tool = tool_name, error, "PreToolUse hook returned an error");
            }
            if output.cancel == Some(true) {
                return ToolExecutionOutput::error(
                    format!("Tool '{tool_name}' was cancelled by PreToolUse hook."),
                    None,
                );
            }
            if let Some(modification) = output.context_modification {
                info!("[PreToolUse hook] {}", modification);
                hook_context.push(format!("[Hook context from PreToolUse]: {modification}"));
            }
        }
    }

    if tool_name == "condense" {
        let history = conversation_history.lock().await;
        let history = match serde_json::to_value(&*history) {
            Ok(history) => history,
            Err(error) => {
                return ToolExecutionOutput::error_with_hook_context(
                    format!("Failed to prepare conversation history for condense: {error}"),
                    None,
                    hook_context,
                    Vec::new(),
                );
            }
        };
        if let Some(params) = params_for_execution.as_object_mut() {
            params.insert("history".to_string(), history);
        }
    }

    let execute_future = handler.execute(&tool_context, params_for_execution);
    let execution_result = if !matches!(
        tool_name,
        "edit_file" | "write_to_file" | "replace_symbol" | "rename_symbol"
    ) && let Some(cancellation_flag) =
        tool_context.cancellation_flag.clone()
    {
        tokio::select! {
            result = execute_future => result,
            () = async {
                while !cancellation_flag.load(std::sync::atomic::Ordering::Acquire) {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            } => Err(ToolError::ExecutionFailed(
                "Tool cancelled by user".to_string(),
            )),
        }
    } else {
        execute_future.await
    };

    match execution_result {
        Ok(res) => {
            let res_text = tool_result_to_text(res);

            // Persist compacted summary immediately if condense tool was used
            let summary = if tool_name == "condense" {
                tool_context.state.lock().await.compacted_summary.clone()
            } else {
                None
            };
            if let Some(summary) = summary
                && let Some(storage) = task_storage
                && let Err(e) = storage.write_compacted_summary_async(&summary).await
            {
                error!("Failed to persist compacted summary immediately: {}", e);
            }

            if let Some(ref hook_mgr) = hook_manager {
                let post_result =
                    hook_mgr.post_tool_use(&config.task_id, tool_name, tool_params, &res_text);
                if let Some(post_output) = post_result.output
                    && let Some(modification) = post_output.context_modification
                {
                    info!("[PostToolUse hook] {}", modification);
                    hook_context.push(format!("[Hook context from PostToolUse]: {modification}"));
                }
            }
            ToolExecutionOutput::success_with_hook_context(res_text, hook_context)
        }
        Err(e) => ToolExecutionOutput::error_with_hook_context(
            format!("Error: {e}"),
            e.metadata().cloned(),
            hook_context,
            e.publication_outcomes().unwrap_or(&[]).to_vec(),
        ),
    }
}

// Phase 1: Pre-process all tools (check plan mode, approval, resolve handlers)
// This is done sequentially since approval may require user interaction
async fn prepare_tool_tasks(
    prepared_calls: &[PreparedToolCall],
    inputs: &ToolBatchInputs,
) -> Vec<ToolTask> {
    let mut tool_tasks: Vec<ToolTask> = Vec::with_capacity(prepared_calls.len());

    for prepared in prepared_calls {
        let tool_name = prepared.tool_name.clone();
        tracing::debug!(tool = %tool_name, "preparing tool execution");

        // Skip tool calls with empty names (malformed provider response)
        if tool_name.is_empty() {
            tracing::warn!("received tool call with empty name, skipping");
            continue;
        }

        // Read-warning decay runs after execution, not here: a
        // denied or malformed call must not erase warning history
        // for files it never touched.
        let tool_id = prepared.tool_id.clone();
        let tool_params = match &prepared.parsed_args {
            Ok(params) => params.clone(),
            Err(parse_error) => {
                tool_tasks.push(ToolTask {
                    tool_id,
                    tool_name,
                    immediate: Some(ToolExecutionOutput::error(parse_error.clone(), None)),
                    task: None,
                    edit_file_paths: vec![],
                    tool_params: serde_json::Value::Null,
                });
                continue;
            }
        };

        // A write/edit generated without the applicable nested rules
        // must not run under an incomplete prompt. The rules have now
        // been loaded, so return a retryable tool result and let the
        // next provider request make the informed decision.
        if inputs.scoped_rules_added && is_mutating_file_tool(&tool_name) {
            tracing::debug!(
                tool = %tool_name,
                "deferred mutating file tool until scoped AGENTS.md rules are visible"
            );
            tool_tasks.push(ToolTask {
                tool_id,
                tool_name,
                immediate: Some(ToolExecutionOutput::error(
                    "Scoped AGENTS.md rules were loaded for this path. Retry the file operation so the updated instructions are applied.".to_string(),
                    None,
                )),
                task: None,
                edit_file_paths: vec![],
                tool_params,
            });
            continue;
        }

        let immediate_output = if let Some(tool) = SnedTool::from_name(&tool_name) {
            // Reject tools that are not in the active profile so the model
            // cannot call tools its current profile has filtered out.
            let profile_denied = inputs
                .tool_profile
                .is_some_and(|p| !p.tools().contains(&tool));

            // Check plan mode restrictions
            let is_restricted = if inputs.mode == AgentMode::Plan {
                tracing::debug!(tool = %tool_name, "checking plan-mode restriction");
                let state = inputs.state.lock().await;
                state.strict_plan_mode_enabled && is_plan_mode_restricted(tool)
            } else {
                false
            };

            if profile_denied {
                ToolExecutionOutput::error(
                    format!(
                        "Tool '{tool_name}' is not available in the current tool profile. Use one of the tools listed for this turn."
                    ),
                    None,
                )
            } else if is_restricted {
                ToolExecutionOutput::error(
                    format!(
                        "Tool '{tool_name}' is not available in PLAN MODE. This tool is restricted to ACT MODE for file modifications. Only use tools available for PLAN MODE when in that mode."
                    ),
                    None,
                )
            } else if let Some(handler) = inputs
                .registry
                .as_ref()
                .expect("AgentLoopDeps: registry not initialized. Call with_tools() before run().")
                .get_handler(&tool)
            {
                // Check approval with per-path resolution (ported from autoApprove.ts:126-180)
                //
                // Key semantics matching TypeScript source:
                //   shouldAutoApprove = isYolo || (isSafe && autoApproveEnabled)
                // Safety gates auto-approval, NEVER post-approval execution.
                // Once the user approves at the prompt, the command always runs.
                // For execute_command: if auto-approved but command is unsafe,
                // force a prompt so the user can review.
                let action_paths = extract_action_path(tool, &tool_params);
                let external_directories = external_action_directories(
                    tool,
                    &inputs.tool_context.workspace_root,
                    &action_paths,
                );
                let params_fingerprint = tool_params_fingerprint(&tool_params);
                tracing::debug!(tool = %tool_name, "checking prior tool denial");
                let previously_denied = {
                    let state = inputs.state.lock().await;
                    state
                        .is_denied_tool_action(&tool_name, &params_fingerprint)
                        .is_some()
                };
                if previously_denied {
                    ToolExecutionOutput::error(
                        format!(
                            "Tool '{tool_name}' was already denied for this exact request. Ask the user before retrying the same action."
                        ),
                        Some(ToolFailureMetadata {
                            class: ToolFailureClass::ApprovalDenied,
                            affected_paths: action_paths.clone(),
                            required_next_step: Some(ToolRequiredNextStep::AskUser),
                        }),
                    )
                } else {
                    match resolve_preflight(
                        inputs,
                        tool,
                        &tool_name,
                        &tool_params,
                        handler,
                        action_paths,
                        external_directories,
                        params_fingerprint,
                    )
                    .await
                    {
                        PreflightDecision::Denied(output) => output,
                        PreflightDecision::Execute {
                            task,
                            edit_file_paths,
                            tool_params,
                        } => {
                            tool_tasks.push(ToolTask {
                                tool_id,
                                tool_name,
                                immediate: None,
                                task: Some(task),
                                edit_file_paths,
                                tool_params,
                            });
                            continue;
                        }
                    }
                }
            } else {
                tracing::warn!(tool = %tool_name, "tool handler not implemented");
                ToolExecutionOutput::error(
                    format!("Tool execution for '{tool_name}' not yet implemented"),
                    None,
                )
            }
        } else {
            tracing::warn!(tool = %tool_name, "unknown tool requested");
            // Surface only the tools in the active profile so the model
            // does not hallucinate names from tools it cannot actually call.
            let active_profile = inputs.tool_profile.unwrap_or(ToolProfile::Full);
            let available = get_tool_definitions_for_profile(active_profile)
                .iter()
                .map(|t| t.function.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            ToolExecutionOutput::error(
                format!("Unknown tool: '{tool_name}'. Available tools: {available}"),
                None,
            )
        };

        tool_tasks.push(ToolTask {
            tool_id,
            tool_name,
            immediate: Some(immediate_output),
            task: None,
            edit_file_paths: vec![],
            tool_params,
        });
    }

    tool_tasks
}

enum PreflightDecision {
    Denied(ToolExecutionOutput),
    Execute {
        task: futures::future::BoxFuture<'static, ToolExecutionOutput>,
        edit_file_paths: Vec<FileActionPath>,
        tool_params: serde_json::Value,
    },
}

#[allow(clippy::too_many_arguments)]
async fn resolve_preflight(
    inputs: &ToolBatchInputs,
    tool: SnedTool,
    tool_name: &str,
    tool_params: &serde_json::Value,
    handler: Arc<dyn ToolHandler>,
    action_paths: Vec<String>,
    external_directories: Vec<PathBuf>,
    params_fingerprint: String,
) -> PreflightDecision {
    let mut user_prompted = false;
    let mut session_command_scope_approved = false;
    let mut allowed_external_roots = Vec::new();
    let command_scopes = (tool_name == "execute_command")
        .then(|| crate::core::approval::command_approval_scopes(tool_params))
        .flatten();
    let approval_result = if let Some(ref approval_mgr) = inputs.approval_manager {
        tracing::debug!(tool = %tool_name, "waiting for approval manager");
        let mgr = approval_mgr.lock().await;
        tracing::debug!(tool = %tool_name, "acquired approval manager");
        allowed_external_roots =
            mgr.external_directory_grants_for(tool.category(), &external_directories);
        let external_needs_prompt = !external_directories.is_empty()
            && !mgr.external_directories_are_granted(tool.category(), &external_directories);
        // Check if any action paths require prompting
        let needs_prompt = if external_needs_prompt {
            true
        } else if action_paths.is_empty() {
            if tool_name == "execute_command" {
                session_command_scope_approved = command_scopes
                    .as_ref()
                    .is_some_and(|scopes| mgr.command_scopes_are_approved(scopes));
                !session_command_scope_approved
                    && mgr.should_prompt(tool, Some(params_fingerprint.as_str()))
            } else {
                mgr.should_prompt(tool, None)
            }
        } else {
            // Has paths: check per-path approval
            action_paths
                .iter()
                .any(|p| mgr.should_prompt_with_path(tool, Some(p.as_str())))
        };
        if needs_prompt {
            drop(mgr); // Drop lock before async call
            user_prompted = true;
            let approval = if external_needs_prompt {
                crate::core::approval::prompt_for_external_directory_approval_async(
                    tool_name,
                    tool_params,
                    external_directories.clone(),
                    inputs.output_writer.clone(),
                    Some(inputs.tool_context.workspace_root.clone()),
                )
                .await
            } else {
                crate::core::approval::prompt_for_approval_async_in_workspace(
                    tool_name,
                    tool_params,
                    inputs.output_writer.clone(),
                    Some(inputs.tool_context.workspace_root.clone()),
                )
                .await
            };
            match approval {
                Ok(crate::core::approval::ApprovalResult::Denied) => {
                    let mut state = inputs.state.lock().await;
                    let is_subagent = state.is_subagent_execution;
                    state.record_denied_tool_action(DeniedToolAction {
                        tool_name: tool_name.to_string(),
                        action_paths: action_paths.clone(),
                        params_fingerprint: params_fingerprint.clone(),
                    });
                    Some(ToolExecutionOutput::error(
                        crate::core::approval::format_denial_message_with_context(
                            tool_name,
                            is_subagent,
                        ),
                        Some(ToolFailureMetadata {
                            class: ToolFailureClass::ApprovalDenied,
                            affected_paths: action_paths.clone(),
                            required_next_step: Some(ToolRequiredNextStep::AskUser),
                        }),
                    ))
                }
                Ok(crate::core::approval::ApprovalResult::Always) => {
                    if let Some(ref am) = inputs.approval_manager {
                        let mut mgr = am.lock().await;
                        if tool_name == "execute_command" {
                            mgr.auto_approve_command(
                                &params_fingerprint,
                                command_scopes.as_deref(),
                            );
                        } else {
                            mgr.auto_approve(tool, None);
                        }
                    }
                    None // Proceed to execute
                }
                Ok(crate::core::approval::ApprovalResult::AllowExternalDirectory) => {
                    if let Some(ref am) = inputs.approval_manager {
                        let mut mgr = am.lock().await;
                        if let Some(error) = external_directories.iter().find_map(|directory| {
                            mgr.grant_external_directory(directory, tool.category())
                                .err()
                        }) {
                            Some(ToolExecutionOutput::error(
                                format!("Could not authorize external directory: {error}"),
                                None,
                            ))
                        } else {
                            allowed_external_roots = mgr.external_directory_grants_for(
                                tool.category(),
                                &external_directories,
                            );
                            None
                        }
                    } else {
                        Some(ToolExecutionOutput::error(
                            "External directory approval is unavailable for this task".to_string(),
                            None,
                        ))
                    }
                }
                Ok(crate::core::approval::ApprovalResult::Approved) => {
                    allowed_external_roots = external_directories.clone();
                    None // Proceed to execute
                }
                Err(e) => Some(ToolExecutionOutput::error(
                    crate::core::approval::format_approval_error(Some(tool_name), &e),
                    None,
                )),
            }
        } else if tool_name == "execute_command" {
            // Auto-approved path for execute_command: check command
            // safety before auto-approving. If the command is
            // unsafe, prompt the user instead (matching TS:
            // shouldAutoApprove = isSafe && autoApproveEnabled).
            let commands = coerce_command_array(tool_params);
            let script = tool_params.get("script").and_then(|s| s.as_str());
            let yolo = mgr.is_yolo_mode();
            let user_safe = mgr.get_user_safe_commands().clone();
            let checker = crate::core::approval::CommandSafetyChecker::new()
                .with_yolo(yolo)
                .with_user_safe_commands(user_safe);
            let any_unsafe = if session_command_scope_approved {
                commands.iter().any(|cmd| {
                    !cmd.is_empty() && checker.is_structurally_safe_for_scope(cmd).is_err()
                }) || script.is_some_and(|s| checker.is_structurally_safe_for_scope(s).is_err())
            } else {
                commands
                    .iter()
                    .any(|cmd| !cmd.is_empty() && checker.is_safe(cmd).is_err())
                    || script.is_some_and(|s| checker.is_safe(s).is_err())
            };
            if any_unsafe {
                // In non-interactive mode, deny unsafe commands directly
                // (no TUI available to prompt the user).
                if !inputs.interactive_mode {
                    let mut state = inputs.state.lock().await;
                    let is_subagent = state.is_subagent_execution;
                    state.record_denied_tool_action(DeniedToolAction {
                        tool_name: tool_name.to_string(),
                        action_paths: action_paths.clone(),
                        params_fingerprint: params_fingerprint.clone(),
                    });
                    Some(ToolExecutionOutput::error(
                        crate::core::approval::format_denial_message_with_context(
                            tool_name,
                            is_subagent,
                        ),
                        Some(ToolFailureMetadata {
                            class: ToolFailureClass::ApprovalDenied,
                            affected_paths: action_paths.clone(),
                            required_next_step: Some(ToolRequiredNextStep::AskUser),
                        }),
                    ))
                } else {
                    drop(mgr);
                    user_prompted = true;
                    match crate::core::approval::prompt_for_approval_async_in_workspace(
                        tool_name,
                        tool_params,
                        inputs.output_writer.clone(),
                        Some(inputs.tool_context.workspace_root.clone()),
                    )
                    .await
                    {
                        Ok(crate::core::approval::ApprovalResult::Denied) => {
                            let mut state = inputs.state.lock().await;
                            let is_subagent = state.is_subagent_execution;
                            state.record_denied_tool_action(DeniedToolAction {
                                tool_name: tool_name.to_string(),
                                action_paths: action_paths.clone(),
                                params_fingerprint: params_fingerprint.clone(),
                            });
                            Some(ToolExecutionOutput::error(
                                crate::core::approval::format_denial_message_with_context(
                                    tool_name,
                                    is_subagent,
                                ),
                                Some(ToolFailureMetadata {
                                    class: ToolFailureClass::ApprovalDenied,
                                    affected_paths: action_paths.clone(),
                                    required_next_step: Some(ToolRequiredNextStep::AskUser),
                                }),
                            ))
                        }
                        Ok(crate::core::approval::ApprovalResult::Always) => {
                            if let Some(ref am) = inputs.approval_manager {
                                let mut mgr = am.lock().await;
                                mgr.auto_approve_command(
                                    &params_fingerprint,
                                    command_scopes.as_deref(),
                                );
                            }
                            None
                        }
                        Ok(crate::core::approval::ApprovalResult::AllowExternalDirectory) => {
                            Some(ToolExecutionOutput::error(
                                "External directory access does not apply to execute_command"
                                    .to_string(),
                                None,
                            ))
                        }
                        Ok(crate::core::approval::ApprovalResult::Approved) => None,
                        Err(e) => Some(ToolExecutionOutput::error(
                            crate::core::approval::format_approval_error(Some(tool_name), &e),
                            None,
                        )),
                    }
                }
            } else {
                None // Safe command, auto-approve proceeds
            }
        } else {
            None // No approval needed
        }
    } else {
        None // No approval manager configured
    };

    if let Some(denied_text) = approval_result {
        tracing::debug!(tool = %tool_name, "tool execution denied by approval");
        return PreflightDecision::Denied(denied_text);
    }

    // Prompt approval already performed the safety review that the
    // handler otherwise applies to an auto-approved command.
    let mut tool_context = (*inputs.tool_context).clone();
    tool_context.explicitly_approved = user_prompted;
    tool_context.allowed_external_roots = allowed_external_roots;
    tool_context.session_command_scope_approved = session_command_scope_approved;
    let tool_context = Arc::new(tool_context);
    let hook_manager = inputs.hook_manager.clone();
    let config = inputs.config.clone();
    let task_storage = inputs.task_storage.clone();
    let edit_file_paths = if tool_name == "edit_file" || tool_name == "write_to_file" {
        extract_file_action_path(tool_name, tool_params, &inputs.tool_context.workspace_root)
    } else {
        vec![]
    };

    // Condense reads the current history while the tool runs.
    let conversation_history = inputs.conversation_history.clone();

    let tool_name_owned = tool_name.to_string();
    let tool_params_owned = tool_params.clone();
    let tool_params_for_task = tool_params_owned.clone();
    let task = async move {
        let params_text = tool_params_owned.to_string();
        tracing::debug!(
            tool = %tool_name_owned,
            params_len = params_text.len(),
            params_preview = %&params_text[..params_text.floor_char_boundary(params_text.len().min(1024))],
            "executing tool"
        );
        let result = execute_tool_with_hooks_internal(
            &config,
            hook_manager,
            tool_context,
            &tool_name_owned,
            &tool_params_owned,
            handler,
            task_storage,
            conversation_history,
        )
        .await;
        tracing::debug!(
            tool = %tool_name_owned,
            result_len = result.text.len(),
            "tool execution complete"
        );
        result
    }
    .boxed();
    PreflightDecision::Execute {
        task,
        edit_file_paths,
        tool_params: tool_params_for_task,
    }
}

pub(crate) async fn execute_tool_batch(
    prepared_calls: &[PreparedToolCall],
    inputs: ToolBatchInputs,
) -> ToolBatchOutput {
    let mut tool_tasks = prepare_tool_tasks(prepared_calls, &inputs).await;
    {
        let mut state = inputs.state.lock().await;
        let batch = tool_tasks.len() as u32;
        state.turn_tool_calls = state.turn_tool_calls.saturating_add(batch);
        state.cumulative_tool_calls = state.cumulative_tool_calls.saturating_add(batch);
    }
    let mut result_map: HashMap<usize, ToolExecutionOutput> =
        HashMap::with_capacity(tool_tasks.len());
    if !inputs.parallel_enabled {
        for (i, task) in tool_tasks.iter_mut().enumerate() {
            if let Some(future) = task.task.take() {
                result_map.insert(
                    i,
                    run_tool_unless_cancelled(inputs.cancelled.clone(), future).await,
                );
            }
        }
    }

    // Mutating and unknown-effect tools run as barriers in provider
    // order; read-only tools between two barriers form one bounded
    // batch. Path-overlap grouping cannot order a write against a
    // later validation command or symbol edit, so every barrier
    // drains the pending reads before it starts.
    if inputs.parallel_enabled {
        use futures::{FutureExt, StreamExt};
        type IndexedToolFuture = futures::future::BoxFuture<'static, (usize, ToolExecutionOutput)>;
        let mut read_batch: Vec<IndexedToolFuture> = Vec::new();
        for (i, task) in tool_tasks.iter_mut().enumerate() {
            let Some(future) = task.task.take() else {
                continue;
            };
            if tool_is_schedulable_read(&task.tool_name) {
                let gated = run_tool_unless_cancelled(inputs.cancelled.clone(), future).boxed();
                read_batch.push(
                    async move {
                        let result = gated.await;
                        (i, result)
                    }
                    .boxed(),
                );
                continue;
            }
            if !read_batch.is_empty() {
                let batch = std::mem::take(&mut read_batch);
                for (j, result) in futures::stream::iter(batch)
                    .buffered(DEFAULT_TOOL_CONCURRENCY)
                    .collect::<Vec<_>>()
                    .await
                {
                    result_map.insert(j, result);
                }
            }
            result_map.insert(
                i,
                run_tool_unless_cancelled(inputs.cancelled.clone(), future).await,
            );
        }
        if !read_batch.is_empty() {
            for (j, result) in futures::stream::iter(read_batch)
                .buffered(DEFAULT_TOOL_CONCURRENCY)
                .collect::<Vec<_>>()
                .await
            {
                result_map.insert(j, result);
            }
        }
    }

    let (outcomes, tools_called, tool_failure_count) =
        collect_ordered_outcomes(tool_tasks, result_map);
    ToolBatchOutput {
        outcomes,
        tools_called,
        tool_failure_count,
    }
}

// One outcome per prepared call, immediate or executed: preflight
// failures (denied, malformed, unknown, deferred) participate
// before any statistics, completion, or plan decision.
fn collect_ordered_outcomes(
    tool_tasks: Vec<ToolTask>,
    mut result_map: HashMap<usize, ToolExecutionOutput>,
) -> (Vec<OrderedToolOutcome>, bool, usize) {
    let mut unified_failure_count = 0usize;
    let mut unified_called = false;
    for (index, task) in tool_tasks.iter().enumerate() {
        if let Some(output) = task.immediate.as_ref() {
            unified_called = true;
            unified_failure_count += usize::from(output.is_error);
        } else if let Some(output) = result_map.get(&index) {
            unified_called = true;
            unified_failure_count += usize::from(output.is_error);
        } else {
            // A prepared call with no outcome at all: fail closed,
            // matching the fabricated execution error paired below.
            unified_called = true;
            unified_failure_count += 1;
        }
    }

    // Phase 3: Collect results in order for ONE StorageMessage.
    let mut execution_results_iter = (0..tool_tasks.len()).filter_map(|i| result_map.remove(&i));
    let outcomes = tool_tasks
        .into_iter()
        .map(|task| {
            let executed = task.immediate.is_none();
            let output = task.immediate.unwrap_or_else(|| {
                execution_results_iter.next().unwrap_or_else(|| {
                    ToolExecutionOutput::error("Tool execution failed".to_string(), None)
                })
            });
            OrderedToolOutcome {
                tool_id: task.tool_id,
                tool_name: task.tool_name,
                tool_params: task.tool_params,
                edit_file_paths: task.edit_file_paths,
                output,
                executed,
            }
        })
        .collect();
    (outcomes, unified_called, unified_failure_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn immediate_task(id: &str, output: ToolExecutionOutput) -> ToolTask {
        ToolTask {
            tool_id: id.to_string(),
            tool_name: format!("{id}_tool"),
            immediate: Some(output),
            task: None,
            edit_file_paths: vec![],
            tool_params: serde_json::Value::Null,
        }
    }

    #[test]
    fn ordered_outcomes_pair_immediate_executed_and_failed_closed() {
        let tasks = vec![
            immediate_task(
                "call_1",
                ToolExecutionOutput::error("denied".to_string(), None),
            ),
            ToolTask {
                tool_id: "call_2".to_string(),
                tool_name: "read_file".to_string(),
                immediate: None,
                task: None,
                edit_file_paths: vec![],
                tool_params: serde_json::json!({"path": "a.rs"}),
            },
            ToolTask {
                tool_id: "call_3".to_string(),
                tool_name: "read_file".to_string(),
                immediate: None,
                task: None,
                edit_file_paths: vec![],
                tool_params: serde_json::Value::Null,
            },
        ];
        let mut result_map = HashMap::new();
        result_map.insert(
            1usize,
            ToolExecutionOutput::success_with_hook_context("contents".to_string(), vec![]),
        );

        let (outcomes, tools_called, tool_failure_count) =
            collect_ordered_outcomes(tasks, result_map);

        assert!(tools_called);
        // Denied immediate plus the outcome-less call both fail.
        assert_eq!(tool_failure_count, 2);
        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0].tool_id, "call_1");
        assert!(!outcomes[0].executed);
        assert!(outcomes[0].output.is_error);
        assert_eq!(outcomes[1].tool_id, "call_2");
        assert!(outcomes[1].executed);
        assert!(!outcomes[1].output.is_error);
        assert_eq!(outcomes[1].output.text, "contents");
        // A prepared call with no outcome at all fails closed, not dropped.
        assert_eq!(outcomes[2].tool_id, "call_3");
        assert!(outcomes[2].output.is_error);
    }
}
