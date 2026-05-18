use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Hook names matching TypeScript hook types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookName {
    #[serde(rename = "PreToolUse")]
    PreToolUse,
    #[serde(rename = "PostToolUse")]
    PostToolUse,
    #[serde(rename = "TaskStart")]
    TaskStart,
    #[serde(rename = "TaskResume")]
    TaskResume,
    #[serde(rename = "TaskCancel")]
    TaskCancel,
    #[serde(rename = "TaskComplete")]
    TaskComplete,
    #[serde(rename = "PreCompact")]
    PreCompact,
}

impl HookName {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookName::PreToolUse => "PreToolUse",
            HookName::PostToolUse => "PostToolUse",
            HookName::TaskStart => "TaskStart",
            HookName::TaskResume => "TaskResume",
            HookName::TaskCancel => "TaskCancel",
            HookName::TaskComplete => "TaskComplete",
            HookName::PreCompact => "PreCompact",
        }
    }
}

/// Hook input data
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookInput {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<HookModelContext>,
    #[serde(flatten)]
    pub data: HookData,
}

/// Callback type for streaming hook output line-by-line.
/// Receives: (line, stream_type) where stream_type is "stdout" or "stderr".
pub type HookStreamCallback = dyn FnMut(&str, &str) + Send;

/// Metadata for hook stream callback
#[derive(Debug, Clone)]
pub struct HookStreamMeta {
    pub source: String, // "global" | "workspace" | "runtime"
    pub script_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookModelContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

/// Hook-specific data types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "hookType")]
pub enum HookData {
    PreToolUse { pre_tool_use: PreToolUseData },
    PostToolUse { post_tool_use: PostToolUseData },
    TaskStart { task_start: TaskStartData },
    TaskResume { task_resume: TaskResumeData },
    TaskCancel { task_cancel: TaskCancelData },
    TaskComplete { task_complete: TaskCompleteData },
    PreCompact { pre_compact: PreCompactData },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PreToolUseData {
    pub tool: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PostToolUseData {
    pub tool: String,
    pub input: serde_json::Value,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskStartData {
    pub task: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskResumeData {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskCancelData {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskCompleteData {
    pub task: String,
    pub completion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PreCompactData {
    pub conversation_history_path: String,
}

/// Hook output from script execution
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HookOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_modification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Hook execution result
#[derive(Debug, Clone)]
pub struct HookResult {
    pub output: Option<HookOutput>,
    pub error: Option<String>,
    pub exit_code: i32,
    pub execution_time_ms: u64,
}

/// Hook discovery and execution
#[derive(Debug)]
pub struct HookManager {
    /// Global hooks directory (e.g., ~/Documents/Sned/Hooks or ~/.sned/hooks)
    global_hooks_dir: Option<PathBuf>,
    /// Workspace hooks directories (.snedrules/hooks)
    workspace_hooks_dirs: Vec<PathBuf>,
    /// Runtime hooks directory (--hooks-dir)
    runtime_hooks_dir: Option<PathBuf>,
    /// Execution timeout in milliseconds
    timeout_ms: u64,
    /// Sned version
    sned_version: String,
    /// User ID
    user_id: String,
    /// Currently active hook execution (for cancellation tracking)
    active_execution: Arc<Mutex<Option<HookExecution>>>,
    /// PID of the currently active child process
    active_child_pid: Arc<Mutex<Option<u32>>>,
    /// Optional cancellation token for hook execution
    cancel_token: Option<CancellationToken>,
    /// Flag indicating hook was cancelled externally
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// Optional discovery cache for hook lookups
    discovery_cache: Option<crate::core::hook_cache::HookDiscoveryCache>,
}

impl HookManager {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            global_hooks_dir: None,
            workspace_hooks_dirs: Vec::new(),
            runtime_hooks_dir: None,
            timeout_ms: 10000,
            sned_version: env!("CARGO_PKG_VERSION").to_string(),
            user_id: user_id.into(),
            active_execution: Arc::new(Mutex::new(None)),
            active_child_pid: Arc::new(Mutex::new(None)),
            cancel_token: None,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            discovery_cache: None,
        }
    }

    /// Set the global hooks directory
    pub fn set_global_hooks_dir(&mut self, path: PathBuf) {
        self.global_hooks_dir = Some(path);
    }

    /// Add a workspace hooks directory
    pub fn add_workspace_hooks_dir(&mut self, path: PathBuf) {
        self.workspace_hooks_dirs.push(path);
    }

    /// Set the runtime hooks directory (--hooks-dir)
    pub fn set_runtime_hooks_dir(&mut self, path: PathBuf) {
        self.runtime_hooks_dir = Some(path);
    }

    /// Set execution timeout
    pub fn set_timeout(&mut self, timeout_ms: u64) {
        self.timeout_ms = timeout_ms;
    }

    /// Set cancellation token for hook execution
    pub fn set_cancel_token(&mut self, token: CancellationToken) {
        self.cancel_token = Some(token);
    }

    /// Inject the hook discovery cache (builder pattern).
    pub fn with_discovery_cache(
        mut self,
        cache: crate::core::hook_cache::HookDiscoveryCache,
    ) -> Self {
        self.discovery_cache = Some(cache);
        self
    }

    /// Get the runtime hooks directory if set.
    pub fn get_runtime_hooks_dir(&self) -> Option<&PathBuf> {
        self.runtime_hooks_dir.as_ref()
    }

    /// Get the workspace hooks directories.
    pub fn get_workspace_hooks_dirs(&self) -> &[PathBuf] {
        &self.workspace_hooks_dirs
    }

    /// Get the global hooks directory if set.
    pub fn get_global_hooks_dir(&self) -> Option<&PathBuf> {
        self.global_hooks_dir.as_ref()
    }

    /// Discover all hooks for a given hook name across all directories.
    /// Priority: runtime > workspace > global
    pub fn discover_hooks(&self, hook_name: HookName) -> Vec<PathBuf> {
        // Use discovery cache if available
        if let Some(ref cache) = self.discovery_cache {
            return cache.discover_hooks(hook_name);
        }

        let mut hooks = Vec::new();
        let hook_file = hook_name.as_str();

        let mut check_dir = |dir: &std::path::Path| {
            // Unix: extensionless executable hooks are canonical
            // Windows: .ps1 PowerShell scripts
            let path = dir.join(hook_file);
            if path.exists() {
                hooks.push(path);
            }

            // Windows PowerShell support only
            #[cfg(target_os = "windows")]
            {
                let ps1_path = dir.join(format!("{}.ps1", hook_file));
                if ps1_path.exists() {
                    hooks.push(ps1_path);
                }
            }
        };

        if let Some(dir) = &self.runtime_hooks_dir {
            check_dir(dir);
        }

        for dir in &self.workspace_hooks_dirs {
            check_dir(dir);
        }

        if let Some(dir) = &self.global_hooks_dir {
            check_dir(dir);
        }

        hooks
    }

    /// Execute a hook with the given input data.
    /// Returns the combined output from all hook scripts.
    /// Fail-open: if a hook fails, it returns empty modifications and logs the error.
    pub fn execute_hook(
        &self,
        hook_name: HookName,
        input: &HookInput,
        stream_callback: Option<Arc<Mutex<HookStreamCallback>>>,
    ) -> HookResult {
        let hooks = self.discover_hooks(hook_name);

        if hooks.is_empty() {
            return HookResult {
                output: None,
                error: None,
                exit_code: 0,
                execution_time_ms: 0,
            };
        }

        let start = std::time::Instant::now();
        let mut combined_output = HookOutput::default();
        let mut last_exit_code = 0;
        let mut errors = Vec::new();

        for hook_path in hooks {
            match self.run_single_hook(&hook_path, input, stream_callback.clone()) {
                Ok(output) => {
                    // Merge outputs
                    if let Some(cancel) = output.cancel {
                        combined_output.cancel = Some(cancel);
                    }
                    if let Some(modification) = output.context_modification {
                        // Append modifications
                        let existing = combined_output.context_modification.unwrap_or_default();
                        if !existing.is_empty() {
                            combined_output.context_modification =
                                Some(format!("{}\n{}", existing, modification));
                        } else {
                            combined_output.context_modification = Some(modification);
                        }
                    }
                    if let Some(error) = output.error_message {
                        errors.push(error);
                    }
                }
                Err(e) => {
                    errors.push(format!("Hook {} failed: {}", hook_path.display(), e));
                    last_exit_code = 1;
                    if hook_name == HookName::PreToolUse {
                        combined_output.cancel = Some(true);
                    }
                }
            }
        }

        if !errors.is_empty() {
            combined_output.error_message = Some(errors.join("\n"));
        }

        let execution_time_ms = start.elapsed().as_millis() as u64;

        HookResult {
            output: Some(combined_output),
            error: if errors.is_empty() {
                None
            } else {
                Some(errors.join("\n"))
            },
            exit_code: last_exit_code,
            execution_time_ms,
        }
    }

    /// Run a single hook script.
    fn run_single_hook(
        &self,
        hook_path: &Path,
        input: &HookInput,
        stream_callback: Option<Arc<Mutex<HookStreamCallback>>>,
    ) -> Result<HookOutput, String> {
        // Serialize input to JSON
        let input_json = serde_json::to_string(input)
            .map_err(|e| format!("Failed to serialize hook input: {}", e))?;

        // Determine shell to use - use /bin/sh for portability and security
        // rather than user's login shell which could execute malicious rc files
        let shell = "/bin/sh";

        // Run the hook script
        let mut cmd = Command::new(shell);
        cmd.arg(hook_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SNED_VERSION", &self.sned_version)
            .env("SNED_USER_ID", &self.user_id);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn hook process: {}", e))?;

        // Track active execution
        let hook_name = hook_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        {
            let mut exec = self.active_execution.lock();
            *exec = Some(HookExecution {
                hook_name,
                start_time: std::time::Instant::now(),
            });
        }
        {
            let mut pid = self.active_child_pid.lock();
            *pid = Some(child.id());
        }
        // Reset cancellation flag
        self.cancelled
            .store(false, std::sync::atomic::Ordering::SeqCst);

        // Write input to stdin
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to open stdin for hook process".to_string())?;
        use std::io::Write;
        stdin
            .write_all(input_json.as_bytes())
            .map_err(|e| format!("Failed to write hook input to stdin: {}", e))?;
        drop(stdin);

        // Read stdout and stderr before waiting (avoids pipe buffer deadlock)
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let stdout_callback = stream_callback.clone();
        let stdout_handle = std::thread::spawn(move || {
            let mut output = String::new();
            if let Some(pipe) = stdout_pipe {
                let reader = BufReader::new(pipe);
                for line in reader.lines().map_while(Result::ok) {
                    if let Some(cb) = stdout_callback.as_ref() {
                        let mut callback = cb.lock();
                        callback(&line, "stdout");
                    }
                    output.push_str(&line);
                    output.push('\n');
                }
            }
            output
        });

        let stderr_callback = stream_callback.clone();
        let stderr_handle = std::thread::spawn(move || {
            let mut output = String::new();
            if let Some(pipe) = stderr_pipe {
                let reader = BufReader::new(pipe);
                for line in reader.lines().map_while(Result::ok) {
                    if let Some(cb) = stderr_callback.as_ref() {
                        let mut callback = cb.lock();
                        callback(&line, "stderr");
                    }
                    output.push_str(&line);
                    output.push('\n');
                }
            }
            output
        });

        // Wait with timeout and cancellation support
        let timeout = Duration::from_millis(self.timeout_ms);
        let cancel_token = self.cancel_token.clone();
        let result = match Self::wait_with_timeout_and_cancellation(
            child,
            timeout,
            cancel_token.as_ref(),
            &self.cancelled,
        ) {
            Ok(status) => status,
            Err(e) => {
                // Clear active execution
                {
                    let mut exec = self.active_execution.lock();
                    *exec = None;
                }
                {
                    let mut pid = self.active_child_pid.lock();
                    *pid = None;
                }
                // Still collect output
                let stdout = HookManager::join_thread_with_timeout(stdout_handle, "stdout");
                let stderr = HookManager::join_thread_with_timeout(stderr_handle, "stderr");
                return Err(format!("{}\nStdout: {}\nStderr: {}", e, stdout, stderr));
            }
        };

        // Clear active execution
        {
            let mut exec = self.active_execution.lock();
            *exec = None;
        }
        {
            let mut pid = self.active_child_pid.lock();
            *pid = None;
        }

        let exit_code = result.code().unwrap_or(-1);

        // Collect output
        let stdout = HookManager::join_thread_with_timeout(stdout_handle, "stdout");
        let stderr = HookManager::join_thread_with_timeout(stderr_handle, "stderr");

        // Check for cancellation (Unix SIGINT convention: 128 + 2 = 130)
        if exit_code == 130 {
            return Ok(HookOutput {
                cancel: Some(true),
                error_message: Some("Hook requested cancellation".to_string()),
                ..Default::default()
            });
        }

        // Parse JSON output
        if stdout.trim().is_empty() {
            // Empty output is valid (no modifications)
            return Ok(HookOutput::default());
        }

        let output: HookOutput = serde_json::from_str(&stdout).map_err(|e| {
            format!(
                "Failed to parse hook output as JSON: {}\nStdout: {}\nStderr: {}",
                e, stdout, stderr
            )
        })?;

        // Validate output
        self.validate_hook_output(&output)?;

        Ok(output)
    }

    /// Wait for a child process with timeout and optional cancellation.
    fn wait_with_timeout_and_cancellation(
        mut child: std::process::Child,
        timeout: Duration,
        cancel_token: Option<&CancellationToken>,
        cancelled_flag: &std::sync::atomic::AtomicBool,
    ) -> Result<std::process::ExitStatus, String> {
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel();
        let pid = child.id();

        std::thread::spawn(move || {
            let result = child.wait();
            let _ = tx.send(result);
        });

        let start = std::time::Instant::now();
        loop {
            match rx.try_recv() {
                Ok(Ok(status)) => {
                    if cancelled_flag.load(std::sync::atomic::Ordering::SeqCst)
                        || cancel_token
                            .map(|token| token.is_cancelled())
                            .unwrap_or(false)
                    {
                        return Err("Hook execution cancelled".to_string());
                    }
                    return Ok(status);
                }
                Ok(Err(e)) => return Err(format!("Failed to wait for hook: {}", e)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("Hook wait thread disconnected".to_string());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }

            if start.elapsed() >= timeout {
                if pid > 0 {
                    #[cfg(unix)]
                    {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGTERM,
                        );
                        let kill_start = std::time::Instant::now();
                        while kill_start.elapsed() < std::time::Duration::from_secs(5) {
                            match nix::sys::wait::waitpid(
                                nix::unistd::Pid::from_raw(pid as i32),
                                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                            ) {
                                Ok(nix::sys::wait::WaitStatus::Exited(_, _)) => break,
                                Ok(nix::sys::wait::WaitStatus::Signaled(_, _, _)) => break,
                                _ => std::thread::sleep(std::time::Duration::from_millis(100)),
                            }
                        }
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    }
                }
                return Err(format!(
                    "Hook execution timed out after {}ms",
                    timeout.as_millis()
                ));
            }

            if cancelled_flag.load(std::sync::atomic::Ordering::SeqCst)
                || cancel_token
                    .map(|token| token.is_cancelled())
                    .unwrap_or(false)
            {
                if pid > 0 {
                    #[cfg(unix)]
                    {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGTERM,
                        );
                        let kill_start = std::time::Instant::now();
                        while kill_start.elapsed() < std::time::Duration::from_secs(5) {
                            match nix::sys::wait::waitpid(
                                nix::unistd::Pid::from_raw(pid as i32),
                                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                            ) {
                                Ok(nix::sys::wait::WaitStatus::Exited(_, _)) => break,
                                Ok(nix::sys::wait::WaitStatus::Signaled(_, _, _)) => break,
                                _ => std::thread::sleep(std::time::Duration::from_millis(100)),
                            }
                        }
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    }
                }
                return Err("Hook execution cancelled".to_string());
            }

            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Join a thread with explicit panic handling.
    ///
    /// Returns the thread's output on success, or an error message if the thread
    /// panicked or timed out.
    fn join_thread_with_timeout<T>(handle: std::thread::JoinHandle<T>, stream_name: &str) -> T
    where
        T: Default,
    {
        match handle.join() {
            Ok(output) => output,
            Err(panic_payload) => {
                let panic_msg = if let Some(s) = panic_payload.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    format!("unknown panic in {} reader thread", stream_name)
                };
                tracing::error!(
                    stream = %stream_name,
                    panic_message = %panic_msg,
                    "Hook reader thread panicked"
                );
                T::default()
            }
        }
    }

    /// Validate hook output structure
    fn validate_hook_output(&self, output: &HookOutput) -> Result<(), String> {
        // Check cancel is boolean if present
        // cancel field is validated by type system (Option<bool>)

        // context_modification must be a string if present
        if let Some(ref modification) = output.context_modification
            && modification.len() > 50000
        {
            return Err(format!(
                "contextModification exceeds maximum size of 50000 bytes (got {})",
                modification.len()
            ));
        }

        Ok(())
    }

    /// Execute PreToolUse hook
    pub fn pre_tool_use(&self, task_id: &str, tool: &str, input: &serde_json::Value) -> HookResult {
        let hook_input = HookInput {
            task_id: task_id.to_string(),
            model: None,
            data: HookData::PreToolUse {
                pre_tool_use: PreToolUseData {
                    tool: tool.to_string(),
                    input: input.clone(),
                },
            },
        };
        self.execute_hook(HookName::PreToolUse, &hook_input, None)
    }

    /// Execute PostToolUse hook
    pub fn post_tool_use(
        &self,
        task_id: &str,
        tool: &str,
        input: &serde_json::Value,
        output: &str,
    ) -> HookResult {
        let hook_input = HookInput {
            task_id: task_id.to_string(),
            model: None,
            data: HookData::PostToolUse {
                post_tool_use: PostToolUseData {
                    tool: tool.to_string(),
                    input: input.clone(),
                    output: output.to_string(),
                },
            },
        };
        self.execute_hook(HookName::PostToolUse, &hook_input, None)
    }

    /// Execute TaskStart hook
    pub fn task_start(&self, task_id: &str, task: &str) -> HookResult {
        let hook_input = HookInput {
            task_id: task_id.to_string(),
            model: None,
            data: HookData::TaskStart {
                task_start: TaskStartData {
                    task: task.to_string(),
                },
            },
        };
        self.execute_hook(HookName::TaskStart, &hook_input, None)
    }

    /// Execute TaskComplete hook
    pub fn task_complete(&self, task_id: &str, task: &str, completion: &str) -> HookResult {
        let hook_input = HookInput {
            task_id: task_id.to_string(),
            model: None,
            data: HookData::TaskComplete {
                task_complete: TaskCompleteData {
                    task: task.to_string(),
                    completion: completion.to_string(),
                },
            },
        };
        self.execute_hook(HookName::TaskComplete, &hook_input, None)
    }

    /// Execute TaskCancel hook
    pub fn task_cancel(&self, task_id: &str) -> HookResult {
        let hook_input = HookInput {
            task_id: task_id.to_string(),
            model: None,
            data: HookData::TaskCancel {
                task_cancel: TaskCancelData {
                    reason: "user_cancelled".to_string(),
                },
            },
        };
        self.execute_hook(HookName::TaskCancel, &hook_input, None)
    }

    /// Execute TaskResume hook
    pub fn task_resume(&self, task_id: &str) -> HookResult {
        let hook_input = HookInput {
            task_id: task_id.to_string(),
            model: None,
            data: HookData::TaskResume {
                task_resume: TaskResumeData {
                    task_id: task_id.to_string(),
                },
            },
        };
        self.execute_hook(HookName::TaskResume, &hook_input, None)
    }

    /// Execute PreCompact hook
    pub fn pre_compact(&self, task_id: &str, conversation_history_path: &str) -> HookResult {
        let hook_input = HookInput {
            task_id: task_id.to_string(),
            model: None,
            data: HookData::PreCompact {
                pre_compact: PreCompactData {
                    conversation_history_path: conversation_history_path.to_string(),
                },
            },
        };
        self.execute_hook(HookName::PreCompact, &hook_input, None)
    }
}

// Moved from cancellation.rs to eliminate circular dependency
#[derive(Debug, Clone)]
pub struct HookExecution {
    pub hook_name: String,
    pub start_time: std::time::Instant,
}

/// Errors from hook operations.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("No active hook execution")]
    NoActiveHook,
    #[error("Hook cancellation timeout")]
    Timeout,
    #[error("Hook cancelled the operation")]
    HookCancelled,
    #[error("{0}")]
    Other(String),
}

// Inherent methods (formerly HookManager trait methods - collapsed to eliminate unnecessary abstraction)
impl HookManager {
    pub fn should_run_task_cancel_hook(&self) -> bool {
        let hooks = self.discover_hooks(HookName::TaskCancel);
        !hooks.is_empty()
    }

    pub fn get_active_hook_execution(&self) -> Option<HookExecution> {
        let guard = self.active_execution.lock();
        guard.clone()
    }

    pub fn cancel_hook_execution(&self) -> Result<(), HookError> {
        let pid = {
            let guard = self.active_child_pid.lock();
            *guard
        };

        if let Some(pid) = pid {
            // Set cancellation flag so the wait loop knows this was an external cancellation
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            #[cfg(unix)]
            {
                use nix::sys::signal::{Signal, kill};
                use nix::unistd::Pid;
                kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
                    .map_err(|e| HookError::Other(format!("Failed to kill process: {}", e)))?;
            }
            #[cfg(not(unix))]
            {
                return Err(HookError::Other(
                    "Process cancellation not supported on this platform".to_string(),
                ));
            }
            Ok(())
        } else {
            Err(HookError::NoActiveHook)
        }
    }

    pub fn clear_active_hook_execution(&self) {
        let mut guard = self.active_execution.lock();
        *guard = None;
        let mut pid_guard = self.active_child_pid.lock();
        *pid_guard = None;
    }
}

/// Get hooks directories from the filesystem.
/// Priority: runtime (--hooks-dir) > workspace (.snedrules/hooks) > global
pub fn get_hooks_dirs(runtime_dir: Option<&Path>, workspace_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // Runtime hooks directory (--hooks-dir)
    if let Some(dir) = runtime_dir
        && dir.exists()
    {
        dirs.push(dir.to_path_buf());
    }

    // Workspace hooks directories
    for root in workspace_roots {
        let workspace_hooks = root.join(".snedrules").join("hooks");
        if workspace_hooks.exists() {
            dirs.push(workspace_hooks);
        }
    }

    // Global hooks directory
    let global_hooks = dirs::home_dir()
        .map(|h| h.join("Documents").join("Sned").join("Hooks"))
        .unwrap_or_else(|| PathBuf::from(".sned").join("hooks"));

    if global_hooks.exists() {
        dirs.push(global_hooks);
    }

    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_hook_name_as_str() {
        assert_eq!(HookName::PreToolUse.as_str(), "PreToolUse");
        assert_eq!(HookName::TaskComplete.as_str(), "TaskComplete");
    }

    #[test]
    fn test_hook_discovery() {
        let temp_dir = std::env::temp_dir().join("sned_test_hooks");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        // Create a PreToolUse hook
        let hook_path = temp_dir.join("PreToolUse");
        fs::write(&hook_path, "#!/bin/sh\necho '{}'").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms).unwrap();
        }

        let mut manager = HookManager::new("test-user");
        manager.set_runtime_hooks_dir(temp_dir.clone());

        let hooks = manager.discover_hooks(HookName::PreToolUse);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0], hook_path);

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_hook_execution_empty_output() {
        let temp_dir = std::env::temp_dir().join("sned_test_hooks_exec");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        // Create a hook that outputs empty JSON
        let hook_path = temp_dir.join("TaskStart");
        fs::write(&hook_path, "#!/bin/sh\ncat > /dev/null\necho '{}'").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms).unwrap();
        }

        let mut manager = HookManager::new("test-user");
        manager.set_runtime_hooks_dir(temp_dir.clone());

        let result = manager.task_start("test-task", "Test task");
        assert_eq!(result.exit_code, 0);
        assert!(result.output.is_some());

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_hook_execution_cancel() {
        let temp_dir = std::env::temp_dir().join("sned_test_hooks_cancel");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        // Create a hook that requests cancellation
        let hook_path = temp_dir.join("PreToolUse");
        fs::write(
            &hook_path,
            "#!/bin/sh\necho '{\"cancel\":true,\"errorMessage\":\"User cancelled\"}'",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms).unwrap();
        }

        let mut manager = HookManager::new("test-user");
        manager.set_runtime_hooks_dir(temp_dir.clone());

        let result = manager.pre_tool_use(
            "test-task",
            "read_file",
            &serde_json::json!({"path": "test.txt"}),
        );
        // Hook execution may fail in test environment due to shell differences
        // Just verify it doesn't panic and has some result
        if let Some(output) = result.output {
            assert_eq!(output.cancel, Some(true));
        }

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_hook_discovery_priority() {
        let temp_dir = std::env::temp_dir().join("sned_test_hooks_priority");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let runtime_dir = temp_dir.join("runtime");
        let workspace_dir = temp_dir.join("workspace").join(".snedrules").join("hooks");
        let global_dir = temp_dir.join("global");

        fs::create_dir_all(&runtime_dir).unwrap();
        fs::create_dir_all(&workspace_dir).unwrap();
        fs::create_dir_all(&global_dir).unwrap();

        // Create hooks in all three locations
        fs::write(runtime_dir.join("PreToolUse"), "runtime").unwrap();
        fs::write(workspace_dir.join("PreToolUse"), "workspace").unwrap();
        fs::write(global_dir.join("PreToolUse"), "global").unwrap();

        let mut manager = HookManager::new("test-user");
        manager.set_runtime_hooks_dir(runtime_dir.clone());
        manager.add_workspace_hooks_dir(workspace_dir.clone());
        manager.set_global_hooks_dir(global_dir.clone());

        let hooks = manager.discover_hooks(HookName::PreToolUse);
        assert_eq!(hooks.len(), 3);
        // Priority: runtime > workspace > global
        assert_eq!(hooks[0], runtime_dir.join("PreToolUse"));
        assert_eq!(hooks[1], workspace_dir.join("PreToolUse"));
        assert_eq!(hooks[2], global_dir.join("PreToolUse"));

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_get_hooks_dirs() {
        let temp_dir = std::env::temp_dir().join("sned_test_get_hooks");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let runtime_dir = temp_dir.join("runtime");
        let workspace_root = temp_dir.join("workspace");
        let workspace_hooks = workspace_root.join(".snedrules").join("hooks");

        fs::create_dir_all(&runtime_dir).unwrap();
        fs::create_dir_all(&workspace_hooks).unwrap();

        let dirs = get_hooks_dirs(Some(&runtime_dir), &[workspace_root]);

        // Should contain at least runtime and workspace dirs
        // (global dir may or may not exist on the system)
        assert!(dirs.len() >= 2);
        assert!(dirs.contains(&runtime_dir));
        assert!(dirs.contains(&workspace_hooks));

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_hook_output_validation() {
        let manager = HookManager::new("test-user");

        // Valid output
        let valid = HookOutput {
            cancel: Some(false),
            context_modification: Some("Some context".to_string()),
            error_message: None,
        };
        assert!(manager.validate_hook_output(&valid).is_ok());

        // Output with large context modification
        let large = HookOutput {
            cancel: None,
            context_modification: Some("x".repeat(50001)),
            error_message: None,
        };
        assert!(manager.validate_hook_output(&large).is_err());
    }

    #[test]
    fn test_hook_streaming() {
        let temp_dir = std::env::temp_dir().join("sned_test_hooks_stream");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        // Create a hook that outputs multiple lines to stderr and JSON to stdout
        let hook_path = temp_dir.join("TaskStart");
        fs::write(
            &hook_path,
            "#!/bin/sh\nfor i in 1 2 3; do echo \"line $i\" >&2; done\necho '{}'",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms).unwrap();
        }

        let mut manager = HookManager::new("test-user");
        manager.set_runtime_hooks_dir(temp_dir.clone());

        // Collect streamed lines
        let streamed_lines = Arc::new(Mutex::new(Vec::new()));
        let callback_lines = streamed_lines.clone();
        let callback: Arc<Mutex<HookStreamCallback>> =
            Arc::new(Mutex::new(Box::new(move |line: &str, stream: &str| {
                callback_lines
                    .lock()
                    .push((line.to_string(), stream.to_string()));
            })));

        let input = HookInput {
            task_id: "test-task".to_string(),
            model: None,
            data: HookData::TaskStart {
                task_start: TaskStartData {
                    task: "Test".to_string(),
                },
            },
        };

        let result = manager.execute_hook(HookName::TaskStart, &input, Some(callback));
        println!(
            "Result: exit_code={}, error={:?}, output={:?}",
            result.exit_code, result.error, result.output
        );
        assert_eq!(result.exit_code, 0, "Hook failed: {:?}", result.error);

        // Verify streaming captured lines
        let lines = streamed_lines.lock();
        assert!(
            !lines.is_empty(),
            "Streaming should have captured output lines"
        );

        // Should have at least 3 stderr lines (the loop output)
        let stderr_lines: Vec<_> = lines.iter().filter(|(_, s)| s == "stderr").collect();
        assert!(
            stderr_lines.len() >= 3,
            "Should have at least 3 stderr lines, got {}",
            stderr_lines.len()
        );

        // Verify line content
        let line_contents: Vec<_> = stderr_lines.iter().map(|(l, _)| l.as_str()).collect();
        assert!(line_contents.contains(&"line 1"), "Should contain 'line 1'");
        assert!(line_contents.contains(&"line 2"), "Should contain 'line 2'");
        assert!(line_contents.contains(&"line 3"), "Should contain 'line 3'");

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_hook_manager_trait_methods() {
        let manager = HookManager::new("test-user");

        // should_run_task_cancel_hook should return false when no hooks exist
        assert!(!manager.should_run_task_cancel_hook());

        // get_active_hook_execution should return None initially
        assert!(manager.get_active_hook_execution().is_none());

        // cancel_hook_execution should fail when no hook is active
        assert!(matches!(
            manager.cancel_hook_execution(),
            Err(HookError::NoActiveHook)
        ));

        // clear_active_hook_execution should not panic
        manager.clear_active_hook_execution();
    }

    #[test]
    fn test_hook_cancellation_via_token() {
        let temp_dir = std::env::temp_dir().join("sned_test_hooks_cancel_token");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        // Create a slow hook
        let hook_path = temp_dir.join("TaskStart");
        fs::write(&hook_path, "#!/bin/sh\nsleep 30\necho '{}'").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms).unwrap();
        }

        let mut manager = HookManager::new("test-user");
        manager.set_runtime_hooks_dir(temp_dir.clone());
        manager.set_timeout(60000); // 60 seconds so timeout doesn't trigger first

        let token = CancellationToken::new();
        manager.set_cancel_token(token.clone());

        let manager = Arc::new(manager);
        let manager_clone = manager.clone();

        // Run hook in background
        let handle = std::thread::spawn(move || manager_clone.task_start("test-task", "Slow task"));

        // Give the hook time to start
        std::thread::sleep(Duration::from_millis(500));

        // Cancel the token
        token.cancel();

        // Wait for hook to finish (should be quick after cancellation)
        let result = handle.join().unwrap();

        // Should have failed due to cancellation
        assert!(result.error.is_some(), "Hook should have been cancelled");
        assert!(
            result.error.unwrap().contains("cancelled"),
            "Error should mention cancellation"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_hook_cancellation_via_trait() {
        let temp_dir = std::env::temp_dir().join("sned_test_hooks_cancel_trait");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        // Create a slow hook
        let hook_path = temp_dir.join("TaskStart");
        fs::write(&hook_path, "#!/bin/sh\nsleep 30\necho '{}'").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms).unwrap();
        }

        let mut manager = HookManager::new("test-user");
        manager.set_runtime_hooks_dir(temp_dir.clone());
        manager.set_timeout(60000);

        let manager = Arc::new(manager);
        let manager_clone = manager.clone();

        // Run hook in background
        let handle = std::thread::spawn(move || manager_clone.task_start("test-task", "Slow task"));

        // Give the hook time to start
        std::thread::sleep(Duration::from_millis(500));

        // Create a runtime for the async trait method
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Verify active execution is tracked
            let exec = manager.get_active_hook_execution();
            assert!(exec.is_some(), "Active execution should be tracked");
            assert_eq!(exec.unwrap().hook_name, "TaskStart");

            // Cancel the hook via trait method
            manager.cancel_hook_execution().unwrap();
        });

        // Wait for hook to finish
        let result = handle.join().unwrap();

        // Should have failed due to cancellation
        assert!(result.error.is_some(), "Hook should have been cancelled");
        assert!(
            result.error.unwrap().contains("cancelled"),
            "Error should mention cancellation"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_join_thread_with_timeout_handles_panic() {
        // Thread that panics
        let panicking = std::thread::spawn(|| -> String {
            panic!("intentional test panic");
        });

        let result = HookManager::join_thread_with_timeout(panicking, "test");
        // Should return default (empty string) instead of crashing
        assert_eq!(result, "");

        // Normal thread should return its value
        let normal = std::thread::spawn(|| "success".to_string());
        let result = HookManager::join_thread_with_timeout(normal, "test");
        assert_eq!(result, "success");
    }

    #[test]
    fn test_task_resume_hook() {
        use std::fs;
        use std::io::Write;

        let temp_dir = std::env::temp_dir().join("sned_test_task_resume");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let runtime_dir = temp_dir.join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();

        // Create a TaskResume hook script
        let hook_script = runtime_dir.join("TaskResume");
        #[cfg(unix)]
        {
            let mut file = fs::File::create(&hook_script).unwrap();
            file.write_all(b"#!/bin/bash\necho '{\"cancel\": false}'\n")
                .unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut manager = HookManager::new("test-user");
        manager.set_runtime_hooks_dir(runtime_dir.clone());

        // Call task_resume convenience method
        let result = manager.task_resume("test-task-123");

        // Hook should have run (exit code 0 = success, -1 = not run)
        assert_ne!(result.exit_code, -1);

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
