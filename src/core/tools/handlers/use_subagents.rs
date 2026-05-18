//! Use subagents tool handler for sned CLI.
//!
//!
//! Runs 1-5 focused subagents in parallel, each with its own prompt.
//! Each subagent gets a configured timeout (default 300s) and optional max turns.

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

const MAX_SUBAGENT_PROMPTS: usize = 5;
const DEFAULT_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub tool_calls: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_write_tokens: u32,
    pub cache_read_tokens: u32,
    pub total_cost: f64,
    pub context_tokens: u32,
    pub context_window: u32,
    pub context_usage_pct: f64,
}

impl Default for SubagentResult {
    fn default() -> Self {
        Self {
            status: "failed".to_string(),
            result: None,
            error: Some("No result".to_string()),
            tool_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
            total_cost: 0.0,
            context_tokens: 0,
            context_window: 0,
            context_usage_pct: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UseSubagentsHandler;

impl UseSubagentsHandler {
    pub fn new() -> Self {
        Self
    }

    fn parse_prompts(params: &serde_json::Value) -> Vec<String> {
        let mut prompts = Vec::new();
        for i in 1..=MAX_SUBAGENT_PROMPTS {
            let key = format!("prompt_{}", i);
            if let Some(p) = params.get(&key).and_then(|v| v.as_str()) {
                let trimmed = p.trim();
                if !trimmed.is_empty() {
                    prompts.push(trimmed.to_string());
                }
            }
        }
        prompts
    }

    fn parse_timeout(params: &serde_json::Value) -> u64 {
        params
            .get("timeout")
            .and_then(|v| v.as_i64())
            .map(|v| v.max(1) as u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
    }

    fn parse_max_turns(params: &serde_json::Value) -> Option<u32> {
        params
            .get("max_turns")
            .and_then(|v| v.as_i64())
            .filter(|&v| v > 0)
            .map(|v| v as u32)
    }

    fn parse_include_history(params: &serde_json::Value) -> bool {
        match params.get("include_history").and_then(|v| v.as_bool()) {
            Some(v) => v,
            None => {
                if let Some(s) = params.get("include_history").and_then(|v| v.as_str()) {
                    s == "true" || s == "1"
                } else {
                    false
                }
            }
        }
    }

    async fn run_subagent(
        prompt: &str,
        timeout_secs: u64,
        max_turns: Option<u32>,
        _include_history: bool,
        cwd: &Path,
    ) -> SubagentResult {
        let mut cmd = Command::new("sned");
        cmd.arg("task");
        cmd.arg("--prompt");
        cmd.arg(prompt);
        cmd.arg("--is-subagent"); // Mark as subagent to prevent recursion
        cmd.current_dir(cwd);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());

        if let Some(turns) = max_turns {
            cmd.arg("--max-turns");
            cmd.arg(turns.to_string());
        }

        let child_result = timeout(Duration::from_secs(timeout_secs), async {
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            let mut stdout = String::new();
            let mut stderr = String::new();

            if let Some(mut stdout_fd) = child.stdout.take() {
                let _ = stdout_fd.read_to_string(&mut stdout).await;
            }
            if let Some(mut stderr_fd) = child.stderr.take() {
                let _ = stderr_fd.read_to_string(&mut stderr).await;
            }

            let status = child.wait().await.map_err(|e| e.to_string())?;
            Ok((status.success(), stdout, stderr))
        })
        .await;

        match child_result {
            Ok(Ok((success, stdout, stderr))) => {
                if success {
                    SubagentResult {
                        status: "completed".to_string(),
                        result: Some(stdout.trim().to_string()),
                        error: None,
                        ..Default::default()
                    }
                } else {
                    SubagentResult {
                        status: "failed".to_string(),
                        result: None,
                        error: Some(stderr.trim().to_string()),
                        ..Default::default()
                    }
                }
            }
            Ok(Err(e)) => SubagentResult {
                status: "failed".to_string(),
                error: Some(e),
                ..Default::default()
            },
            Err(_) => SubagentResult {
                status: "failed".to_string(),
                error: Some(format!("Subagent timed out after {} seconds", timeout_secs)),
                ..Default::default()
            },
        }
    }

    async fn execute_with_workspace_root(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        workspace_root: &Path,
    ) -> Result<String, ToolError> {
        // Prevent subagent recursion (matches TypeScript SubagentToolHandler.ts:96-98)
        if state.is_subagent_execution {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "use_subagents: subagent recursion detected"
            );
            return Err(ToolError::ExecutionFailed(
                "Subagents cannot spawn other subagents.".to_string(),
            ));
        }

        let subagents_enabled = state.subagents_enabled;
        if !subagents_enabled {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "use_subagents: subagents are disabled"
            );
            return Err(ToolError::ExecutionFailed(
                "Subagents are disabled. Enable them in Settings > Features to use this tool."
                    .to_string(),
            ));
        }

        let prompts = Self::parse_prompts(&params);
        if prompts.is_empty() {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "use_subagents: no prompts provided"
            );
            return Err(ToolError::InvalidInput(
                "Missing required parameter: at least one prompt (prompt_1) must be provided."
                    .to_string(),
            ));
        }

        if prompts.len() > MAX_SUBAGENT_PROMPTS {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                prompt_count = prompts.len(),
                max_allowed = MAX_SUBAGENT_PROMPTS,
                "use_subagents: too many prompts"
            );
            return Err(ToolError::InvalidInput(format!(
                "Too many subagent prompts provided ({}). Maximum is {}.",
                prompts.len(),
                MAX_SUBAGENT_PROMPTS
            )));
        }

        // Check if the JSON has more than MAX_SUBAGENT_PROMPTS prompt keys (before filtering empty ones)
        let mut prompt_count_in_json = 0;
        for i in 1..=(MAX_SUBAGENT_PROMPTS + 1) {
            let key = format!("prompt_{}", i);
            if params.get(&key).is_some() {
                prompt_count_in_json += 1;
            }
        }
        if prompt_count_in_json > MAX_SUBAGENT_PROMPTS {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                prompt_count = prompt_count_in_json,
                max_allowed = MAX_SUBAGENT_PROMPTS,
                "use_subagents: too many prompts in JSON"
            );
            return Err(ToolError::InvalidInput(format!(
                "Too many subagent prompts provided ({}). Maximum is {}.",
                prompt_count_in_json, MAX_SUBAGENT_PROMPTS
            )));
        }

        let timeout_secs = Self::parse_timeout(&params);
        let max_turns = Self::parse_max_turns(&params);
        let include_history = Self::parse_include_history(&params);

        if timeout_secs == 0 {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "use_subagents: timeout is zero"
            );
            return Err(ToolError::InvalidInput(
                "timeout must be a positive number.".to_string(),
            ));
        }

        if let Some(0) = max_turns {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "use_subagents: max_turns is zero"
            );
            return Err(ToolError::InvalidInput(
                "max_turns must be a positive number.".to_string(),
            ));
        }

        state.consecutive_mistakes = 0;

        let cwd = workspace_root.to_path_buf();

        eprintln!(
            "{}",
            crate::cli::colors::info(&format!(
                "Running {} subagent(s) in parallel...",
                prompts.len()
            ))
        );

        let mut handles = Vec::new();
        for (i, prompt) in prompts.iter().enumerate() {
            let prompt_clone = prompt.clone();
            let cwd_clone = cwd.clone();

            handles.push(tokio::spawn(async move {
                let result = Self::run_subagent(
                    &prompt_clone,
                    timeout_secs,
                    max_turns,
                    include_history,
                    cwd_clone.as_path(),
                )
                .await;
                (i, result)
            }));
        }

        let mut results: Vec<(usize, SubagentResult)> = Vec::new();
        for handle in handles {
            match handle.await {
                Ok((idx, result)) => results.push((idx, result)),
                Err(e) => {
                    results.push((
                        results.len(),
                        SubagentResult {
                            status: "failed".to_string(),
                            error: Some(format!("Join error: {}", e)),
                            ..Default::default()
                        },
                    ));
                }
            }
        }

        results.sort_by_key(|(i, _)| *i);

        let mut successes = 0usize;
        let mut failures = 0usize;
        let mut total_tool_calls = 0u32;
        let mut _total_input_tokens = 0u32;
        let mut _total_output_tokens = 0u32;
        let mut total_cache_writes = 0u32;
        let mut total_cache_reads = 0u32;
        let mut max_context_tokens = 0u32;
        let mut max_context_window = 0u32;
        let mut max_context_pct = 0.0f64;

        let mut summary_lines = vec![format!("Subagent results:")];
        if timeout_secs != DEFAULT_TIMEOUT_SECS {
            summary_lines.push(format!("Timeout: {}s", timeout_secs));
        }
        if let Some(turns) = max_turns {
            summary_lines.push(format!("Max turns: {}", turns));
        }
        summary_lines.push(format!("Total: {}", results.len()));
        summary_lines.push(String::new());

        for (i, result) in &results {
            let label = format!("[{}]", i + 1);
            match result.status.as_str() {
                "completed" => {
                    successes += 1;
                    if let Some(ref res) = result.result {
                        let excerpt = if res.len() > 200 {
                            let end = res.floor_char_boundary(200);
                            format!("{}...", &res[..end])
                        } else {
                            res.clone()
                        };
                        summary_lines.push(format!("{} SUCCEEDED\n{}", label, excerpt));
                    } else {
                        summary_lines.push(format!("{} SUCCEEDED (no output)", label));
                    }
                }
                _ => {
                    failures += 1;
                    if let Some(ref err) = result.error {
                        let excerpt = if err.len() > 200 {
                            let end = err.floor_char_boundary(200);
                            format!("{}...", &err[..end])
                        } else {
                            err.clone()
                        };
                        summary_lines.push(format!("{} FAILED\n{}", label, excerpt));
                    } else {
                        summary_lines.push(format!("{} FAILED", label));
                    }
                }
            }

            total_tool_calls += result.tool_calls;
            _total_input_tokens += result.input_tokens;
            _total_output_tokens += result.output_tokens;
            total_cache_writes += result.cache_write_tokens;
            total_cache_reads += result.cache_read_tokens;
            max_context_tokens = max_context_tokens.max(result.context_tokens);
            max_context_window = max_context_window.max(result.context_window);
            max_context_pct = max_context_pct.max(result.context_usage_pct);
        }

        summary_lines.push(format!("Succeeded: {}", successes));
        summary_lines.push(format!("Failed: {}", failures));
        summary_lines.push(format!("Tool calls: {}", total_tool_calls));
        if max_context_window > 0 {
            summary_lines.push(format!(
                "Peak context usage: {} / {} ({:.1}%)",
                max_context_tokens, max_context_window, max_context_pct
            ));
        }
        summary_lines.push(format!(
            "Cache: {} reads, {} writes",
            total_cache_reads, total_cache_writes
        ));

        let summary = summary_lines.join("\n");
        eprintln!(
            "{}",
            crate::cli::colors::info(&format!(
                "Subagent batch complete: {} succeeded, {} failed",
                successes, failures
            ))
        );

        Ok(summary)
    }

    pub async fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let workspace_root = std::env::current_dir()
            .ok()
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        self.execute_with_workspace_root(state, params, workspace_root.as_path())
            .await
    }
}

#[async_trait]
impl ToolHandler for UseSubagentsHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        // Subagents are never auto-approved - require explicit user approval (matches TypeScript behavior)
        if !ctx.explicitly_approved {
            let mut state = ctx.state.lock().await;
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "use_subagents: requires explicit user approval"
            );
            return Err(ToolError::ExecutionFailed(
                "Subagent execution requires explicit user approval. Please approve the request."
                    .to_string(),
            ));
        }

        let mut state = ctx.state.lock().await;
        self.execute_with_workspace_root(&mut state, params, ctx.workspace_root.as_path())
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let prompts = Self::parse_prompts(params);
        if prompts.is_empty() {
            "[use_subagents]".to_string()
        } else {
            let count = prompts.len();
            if count == 1 {
                "[use_subagents: 1 prompt]".to_string()
            } else {
                format!("[use_subagents: {} prompts]", count)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_prompts() {
        let params = serde_json::json!({
            "prompt_1": "Fix the bug",
            "prompt_2": "Write tests",
            "prompt_3": "",
            "prompt_4": "  ",
        });
        let prompts = UseSubagentsHandler::parse_prompts(&params);
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0], "Fix the bug");
        assert_eq!(prompts[1], "Write tests");
    }

    #[test]
    fn test_parse_prompts_none() {
        let params = serde_json::json!({});
        let prompts = UseSubagentsHandler::parse_prompts(&params);
        assert!(prompts.is_empty());
    }

    #[test]
    fn test_parse_timeout_default() {
        let params = serde_json::json!({});
        assert_eq!(
            UseSubagentsHandler::parse_timeout(&params),
            DEFAULT_TIMEOUT_SECS
        );
    }

    #[test]
    fn test_parse_timeout_custom() {
        let params = serde_json::json!({"timeout": 120});
        assert_eq!(UseSubagentsHandler::parse_timeout(&params), 120);
    }

    #[test]
    fn test_parse_timeout_negative() {
        let params = serde_json::json!({"timeout": -5});
        assert_eq!(UseSubagentsHandler::parse_timeout(&params), 1);
    }

    #[test]
    fn test_parse_max_turns() {
        let params = serde_json::json!({"max_turns": 10});
        assert_eq!(UseSubagentsHandler::parse_max_turns(&params), Some(10));
    }

    #[test]
    fn test_parse_max_turns_zero() {
        let params = serde_json::json!({"max_turns": 0});
        assert_eq!(UseSubagentsHandler::parse_max_turns(&params), None);
    }

    #[test]
    fn test_parse_include_history_bool() {
        let params = serde_json::json!({"include_history": true});
        assert!(UseSubagentsHandler::parse_include_history(&params));
    }

    #[test]
    fn test_parse_include_history_false() {
        let params = serde_json::json!({"include_history": false});
        assert!(!UseSubagentsHandler::parse_include_history(&params));
    }

    #[test]
    fn test_parse_include_history_string() {
        let params = serde_json::json!({"include_history": "true"});
        assert!(UseSubagentsHandler::parse_include_history(&params));
    }

    #[tokio::test]
    async fn test_handler_disabled() {
        let handler = UseSubagentsHandler::new();
        let mut state = TaskState {
            subagents_enabled: false,
            ..Default::default()
        };
        let result = handler
            .execute(&mut state, serde_json::json!({"prompt_1": "Test"}))
            .await;
        assert!(result.is_err());
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_handler_missing_prompts() {
        let handler = UseSubagentsHandler::new();
        let mut state = TaskState {
            subagents_enabled: true,
            ..Default::default()
        };
        let result = handler.execute(&mut state, serde_json::json!({})).await;
        assert!(result.is_err());
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_handler_too_many_prompts() {
        let handler = UseSubagentsHandler::new();
        let mut state = TaskState {
            subagents_enabled: true,
            ..Default::default()
        };
        let params = serde_json::json!({
            "prompt_1": "One",
            "prompt_2": "Two",
            "prompt_3": "Three",
            "prompt_4": "Four",
            "prompt_5": "Five",
            "prompt_6": "Six",
        });
        let result = handler.execute(&mut state, params).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_description() {
        let handler = UseSubagentsHandler::new();
        let desc = handler.description(&serde_json::json!({
            "prompt_1": "First",
            "prompt_2": "Second"
        }));
        assert_eq!(desc, "[use_subagents: 2 prompts]");

        let desc2 = handler.description(&serde_json::json!({}));
        assert_eq!(desc2, "[use_subagents]");

        let desc3 = handler.description(&serde_json::json!({"prompt_1": "Only one"}));
        assert_eq!(desc3, "[use_subagents: 1 prompt]");
    }

    #[tokio::test]
    async fn test_handler_requires_explicit_approval() {
        use crate::core::file_editor::AnchorStateManager;
        use crate::core::tools::ToolHandler;
        use std::sync::Arc;

        let handler = UseSubagentsHandler::new();
        let state = Arc::new(tokio::sync::Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();

        // Create context WITHOUT explicit approval (explicitly_approved = false)
        let ctx = crate::core::tools::ToolContext::new(
            state.clone(),
            None, // no approval manager
            std::env::current_dir().unwrap(),
            anchor_mgr,
            false, // json_output
            "test-task".to_string(),
            None,  // no hook manager
            false, // explicitly_approved = false
        );

        let params = serde_json::json!({"prompt_1": "Test subagent"});
        let result = ToolHandler::execute(&handler, &ctx, params).await;

        // Should fail because explicitly_approved is false
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));

        // Verify consecutive_mistakes was incremented
        let state_guard = state.lock().await;
        assert_eq!(state_guard.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_handler_prevents_recursion() {
        let handler = UseSubagentsHandler::new();
        let mut state = TaskState {
            subagents_enabled: true,
            is_subagent_execution: true, // Mark as subagent
            ..Default::default()
        };

        let params = serde_json::json!({"prompt_1": "Test subagent"});
        let result = handler.execute(&mut state, params).await;

        // Should fail because this is already a subagent execution
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        assert!(err.to_string().contains("cannot spawn other subagents"));

        // Verify consecutive_mistakes was incremented
        assert_eq!(state.consecutive_mistakes, 1);
    }
}
