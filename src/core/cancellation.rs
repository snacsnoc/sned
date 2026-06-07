//! Task cancellation and abort handling for sned CLI.
//!
//! Ports behavior from `dirac/src/core/task/LifecycleManager.ts` abortTask().

use crate::core::agent_loop::TaskState;
use crate::core::hooks::{HookData, HookInput, HookManager, HookName, TaskCancelData};
use crate::storage::disk;
use crate::storage::state_manager::StateManager;
use ratatui::crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use ratatui::crossterm::execute;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Set to true when ratatui terminal has been initialized (TUI mode).
/// Guards ratatui::restore() calls in the force-exit signal handler
/// so we don't write ANSI escape sequences to stdout in one-shot mode.
pub(crate) static TERMINAL_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Restore the interactive terminal state on shutdown or forced exit.
pub(crate) fn restore_terminal_state() {
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
    ratatui::restore();
    TERMINAL_INITIALIZED.store(false, Ordering::Release);
}

/// Terminate a process group with SIGTERM, wait `grace`, then SIGKILL
/// any survivors. Uses negative PID to signal the entire group, matching
/// how sned spawns child processes (process_group(0)).
///
/// Safety: `pid` must be a process group ID previously registered by sned
/// (via `child.id()` of a spawned command) or stored in
/// `TaskState::running_command_pids`. Each signal is gated by a
/// `kill(pgid, 0)` liveness check to avoid signaling a recycled PGID.
/// Safe to call multiple times and from multiple sites; normalizes
/// SIGTERM→wait→SIGKILL escalation across cancellation, timeout, and
/// subagent paths.
#[cfg(unix)]
pub(crate) async fn terminate_process_group(pid: i32, grace: Duration) {
    use libc::{SIGKILL, SIGTERM};
    let pgid: i32 = -pid;
    if unsafe { libc::kill(pgid, 0) } != 0 {
        return;
    }
    let _ = unsafe { libc::kill(pgid, SIGTERM) };
    tracing::debug!("Sent SIGTERM to PGID -{}", pid);
    tokio::time::sleep(grace).await;
    if unsafe { libc::kill(pgid, 0) } != 0 {
        return;
    }
    let _ = unsafe { libc::kill(pgid, SIGKILL) };
    tracing::debug!("Sent SIGKILL to PGID -{}", pid);
}

/// Handles task cancellation and cleanup.
pub struct CancellationHandler {
    state: Arc<Mutex<TaskState>>,
}

impl CancellationHandler {
    pub fn new(state: Arc<Mutex<TaskState>>) -> Self {
        Self { state }
    }

    /// Aborts the current task.
    ///
    /// Sequence:
    /// 1. Set abort flag on task state
    /// 2. Cancel active hook execution
    /// 3. Kill running background commands (SIGTERM, then SIGKILL)
    /// 4. Execute TaskCancel hook if enabled
    /// 5. Save state
    /// 6. Cleanup resources (file watcher, anchor state, caches)
    pub async fn abort_task(
        &self,
        hook_manager: Option<&HookManager>,
        state_manager: &StateManager,
        task_id: &str,
        anchor_mgr: Option<&crate::core::file_editor::AnchorStateManager>,
    ) -> Result<(), CancellationError> {
        // 1. Set abort flag
        {
            let mut state = self.state.lock().await;
            state.is_cancelled = true;
            state
                .is_cancelled_atomic
                .store(true, std::sync::atomic::Ordering::Release);
        }

        // 2. Cancel active hook execution (if hook manager available)
        if let Some(hook_mgr) = hook_manager
            && let Some(_execution) = hook_mgr.get_active_hook_execution()
        {
            if let Err(e) = hook_mgr.cancel_hook_execution() {
                tracing::warn!("Failed to cancel hook execution: {}", e);
            }
            hook_mgr.clear_active_hook_execution();
        }

        // 3. Kill running background commands
        {
            let state = self.state.lock().await;
            let pids_to_kill = state.running_command_pids.clone();
            drop(state);

            if !pids_to_kill.is_empty() {
                tracing::info!(
                    "Killing {} running command(s) on task cancellation",
                    pids_to_kill.len()
                );
                #[cfg(unix)]
                {
                    for pid in &pids_to_kill {
                        terminate_process_group(*pid, Duration::from_millis(100)).await;
                    }
                }
                #[cfg(not(unix))]
                {
                    tracing::warn!("Command cancellation not implemented on non-Unix platforms");
                }

                // Clear the PID list
                {
                    let mut state = self.state.lock().await;
                    state.running_command_pids.clear();
                }
            }
        }

        // 4. Execute TaskCancel hook
        if let Some(hook_mgr) = hook_manager
            && hook_mgr.should_run_task_cancel_hook()
        {
            let hook_input = HookInput {
                task_id: task_id.to_string(),
                model: None,
                data: HookData::TaskCancel {
                    task_cancel: TaskCancelData {
                        reason: "User cancelled task".to_string(),
                    },
                },
            };
            let _ = hook_mgr.execute_hook(HookName::TaskCancel, &hook_input, None);
            tracing::info!("TaskCancel hook executed");
        }

        // 5. Save state immediately (force save)
        if let Err(e) = state_manager.persist() {
            tracing::warn!("Failed to persist state on cancellation: {}", e);
            return Err(CancellationError::StateError(e.to_string()));
        }
        tracing::info!("State persisted on cancellation");

        // 6. Cleanup resources (file watcher, anchor state, caches)
        {
            let mut state = self.state.lock().await;

            // Stop file watcher to prevent further events
            state.file_context_tracker.stop_watcher();
            tracing::info!("File context watcher stopped");

            // Reset anchor state for this task
            if let Some(anchor_mgr) = anchor_mgr {
                anchor_mgr.reset(Some(task_id));
                tracing::info!("Anchor state reset for task {}", task_id);
            } else {
                tracing::warn!("AnchorStateManager not provided, skipping anchor reset");
            }

            // Clear in-memory file content cache (cross-call coordination within a turn)
            state.file_content_cache.clear();
            tracing::info!("File content cache cleared");
        }

        tracing::info!("Task abort sequence complete");

        Ok(())
    }

    /// Checks if the task has been cancelled.
    pub async fn is_cancelled(&self) -> bool {
        let state = self.state.lock().await;
        state.is_cancelled
    }
}

/// Errors during cancellation.
#[derive(Debug, thiserror::Error)]
pub enum CancellationError {
    #[error("State save failed: {0}")]
    StateError(String),
}

const FORCE_EXIT_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalAction {
    Cancel,
    ForceExit,
}

fn signal_action(last_signal: &mut Option<Instant>, now: Instant) -> SignalAction {
    if last_signal.is_some_and(|previous| now.duration_since(previous) <= FORCE_EXIT_WINDOW) {
        SignalAction::ForceExit
    } else {
        *last_signal = Some(now);
        SignalAction::Cancel
    }
}

/// Resolve the exit code for a force-exit signal.
///
/// In TUI mode, the interactive loop already owns the "double Ctrl+C" quit
/// behavior, so the signal handler should not convert that path into a shell
/// interrupt exit code. Non-TUI invocations keep the conventional signal code.
fn force_exit_code(signal_name: &str, exit_code: i32) -> i32 {
    if signal_name == "Ctrl+C" && TERMINAL_INITIALIZED.load(Ordering::Acquire) {
        crate::exit_codes::EXIT_SUCCESS
    } else {
        exit_code
    }
}

async fn handle_shutdown_signal(
    state: &Arc<Mutex<TaskState>>,
    last_signal: &Arc<Mutex<Option<Instant>>>,
    signal_name: &'static str,
    exit_code: i32,
) {
    let action = {
        let mut last = last_signal.lock().await;
        signal_action(&mut last, Instant::now())
    };

    match action {
        SignalAction::Cancel => {
            let mut state = state.lock().await;
            state.is_cancelled = true;
            state
                .is_cancelled_atomic
                .store(true, std::sync::atomic::Ordering::Release);
            tracing::info!("Received {}, initiating graceful shutdown...", signal_name);
            tracing::info!("Repeat the signal within 2s to force exit");
        }
        SignalAction::ForceExit => {
            tracing::warn!(
                "Received second {} within 2s, forcing exit after atomic writes drain...",
                signal_name
            );
            let drained = disk::wait_for_atomic_writes(FORCE_EXIT_WINDOW).await;
            if !drained {
                tracing::warn!(
                    "Timed out waiting for {} active atomic write(s) before forced exit",
                    disk::active_atomic_write_count()
                );
            }
            let exit = force_exit_code(signal_name, exit_code);
            // Restore terminal state before forced exit to avoid breaking user's shell
            if TERMINAL_INITIALIZED.load(Ordering::Acquire) {
                restore_terminal_state();
            }
            std::process::exit(exit);
        }
    }
}

/// Sets up signal handlers for graceful shutdown.
///
/// First SIGINT/Ctrl+C or SIGTERM requests cooperative cancellation. A repeated
/// signal within two seconds waits briefly for active atomic writes, then exits.
pub async fn setup_ctrl_c_handler(state: Arc<Mutex<TaskState>>) {
    let last_signal = Arc::new(Mutex::new(None));

    {
        let state = state.clone();
        let last_signal = last_signal.clone();
        tokio::spawn(async move {
            loop {
                match tokio::signal::ctrl_c().await {
                    Ok(()) => {
                        handle_shutdown_signal(&state, &last_signal, "Ctrl+C", 130).await;
                    }
                    Err(e) => {
                        tracing::error!("Failed to listen for Ctrl+C: {}", e);
                        break;
                    }
                }
            }
        });
    }

    #[cfg(unix)]
    {
        let state = state.clone();
        let last_signal = last_signal.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};

            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(sigterm) => sigterm,
                Err(e) => {
                    tracing::error!("Failed to listen for SIGTERM: {}", e);
                    return;
                }
            };

            while sigterm.recv().await.is_some() {
                handle_shutdown_signal(&state, &last_signal, "SIGTERM", 143).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TerminalInitializedGuard(bool);

    impl Drop for TerminalInitializedGuard {
        fn drop(&mut self) {
            TERMINAL_INITIALIZED.store(self.0, Ordering::Release);
        }
    }

    #[test]
    fn test_force_exit_code_uses_clean_exit_in_terminal_mode() {
        let _guard = TerminalInitializedGuard(TERMINAL_INITIALIZED.load(Ordering::Acquire));

        TERMINAL_INITIALIZED.store(false, Ordering::Release);
        assert_eq!(force_exit_code("Ctrl+C", 130), 130);

        TERMINAL_INITIALIZED.store(true, Ordering::Release);
        assert_eq!(
            force_exit_code("Ctrl+C", 130),
            crate::exit_codes::EXIT_SUCCESS
        );
        assert_eq!(force_exit_code("SIGTERM", 143), 143);
    }

    #[tokio::test]
    async fn test_cancellation_handler_sets_abort_flag() {
        let state = Arc::new(Mutex::new(TaskState::default()));
        let handler = CancellationHandler::new(state.clone());

        let temp_dir = tempfile::tempdir().unwrap();
        let data_dir = temp_dir.path().join("data");
        let state_dir = data_dir.join("state");
        let settings_dir = data_dir.join("settings");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(&settings_dir).unwrap();
        let old_data_dir = std::env::var_os("SNED_DATA_DIR");
        // SAFETY: this test runs in isolation under the validation command.
        unsafe {
            std::env::set_var("SNED_DATA_DIR", &data_dir);
        }

        let state_manager = crate::storage::state_manager::StateManager::new().unwrap();

        assert!(!handler.is_cancelled().await);

        let result = handler
            .abort_task(None, &state_manager, "test-task", None)
            .await;
        assert!(
            result.is_ok(),
            "abort_task should succeed: {:?}",
            result.err()
        );

        assert!(handler.is_cancelled().await);

        let state = state.lock().await;
        assert!(state.is_cancelled);

        // SAFETY: restore the process environment for later tests.
        unsafe {
            match old_data_dir {
                Some(ref value) => std::env::set_var("SNED_DATA_DIR", value),
                None => std::env::remove_var("SNED_DATA_DIR"),
            }
        }
    }

    #[test]
    fn test_cancellation_error_display() {
        assert_eq!(
            format!("{}", CancellationError::StateError("test".to_string())),
            "State save failed: test"
        );
    }

    #[tokio::test]
    async fn test_abort_task_with_state_persist() {
        let state = Arc::new(Mutex::new(TaskState::default()));
        let handler = CancellationHandler::new(state.clone());

        let temp_dir = tempfile::tempdir().unwrap();
        let data_dir = temp_dir.path().join("data");
        let state_dir = data_dir.join("state");
        let settings_dir = data_dir.join("settings");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(&settings_dir).unwrap();
        let old_data_dir = std::env::var_os("SNED_DATA_DIR");
        // SAFETY: this test runs in isolation under the validation command.
        unsafe {
            std::env::set_var("SNED_DATA_DIR", &data_dir);
        }

        let state_manager = crate::storage::state_manager::StateManager::new().unwrap();

        let result = handler
            .abort_task(None, &state_manager, "test-task", None)
            .await;
        assert!(
            result.is_ok(),
            "abort_task should succeed: {:?}",
            result.err()
        );

        assert!(
            handler.is_cancelled().await,
            "Cancellation flag should be set"
        );

        // SAFETY: restore the process environment for later tests.
        unsafe {
            match old_data_dir {
                Some(ref value) => std::env::set_var("SNED_DATA_DIR", value),
                None => std::env::remove_var("SNED_DATA_DIR"),
            }
        }
    }

    #[test]
    fn test_signal_action_second_signal_within_window_forces_exit() {
        let now = Instant::now();
        let mut last_signal = None;

        assert_eq!(signal_action(&mut last_signal, now), SignalAction::Cancel);
        assert_eq!(
            signal_action(&mut last_signal, now + Duration::from_millis(500)),
            SignalAction::ForceExit
        );
    }

    #[test]
    fn test_signal_action_after_window_starts_new_cancel_cycle() {
        let now = Instant::now();
        let mut last_signal = None;

        assert_eq!(signal_action(&mut last_signal, now), SignalAction::Cancel);
        assert_eq!(
            signal_action(&mut last_signal, now + Duration::from_secs(3)),
            SignalAction::Cancel
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_terminate_process_group_noop_for_dead_pid() {
        terminate_process_group(0x7fffffff, Duration::from_millis(10)).await;
    }
}
