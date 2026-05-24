//! Task cancellation and abort handling for sned CLI.
//!
//! Ports behavior from `dirac/src/core/task/LifecycleManager.ts` abortTask().

use crate::core::agent_loop::TaskState;
use crate::core::hooks::{HookData, HookInput, HookManager, HookName, TaskCancelData};
use crate::storage::disk;
use crate::storage::state_manager::StateManager;
#[cfg(unix)]
use libc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

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
                    use libc;

                    // Send SIGTERM to all processes first (graceful shutdown)
                    for pid in &pids_to_kill {
                        // Check liveness first to avoid signaling recycled PIDs
                        if unsafe { libc::kill(*pid, 0) } == 0 {
                            let _ = unsafe { libc::kill(*pid, libc::SIGTERM) };
                            tracing::debug!("Sent SIGTERM to PID {}", pid);
                        }
                    }

                    // Single global wait for all processes to handle SIGTERM
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                    // Then SIGKILL any remaining processes
                    for pid in &pids_to_kill {
                        if unsafe { libc::kill(*pid, 0) } == 0 {
                            let _ = unsafe { libc::kill(*pid, libc::SIGKILL) };
                            tracing::debug!("Sent SIGKILL to PID {}", pid);
                        }
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
            // Restore terminal state before forced exit to avoid breaking user's shell
            ratatui::restore();
            std::process::exit(exit_code);
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

        let state_manager = crate::storage::state_manager::StateManager::new().unwrap();

        assert!(!handler.is_cancelled().await);

        let result = handler
            .abort_task(None, &state_manager, "test-task", None)
            .await;
        assert!(result.is_ok());

        assert!(handler.is_cancelled().await);

        let state = state.lock().await;
        assert!(state.is_cancelled);
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

        let state_manager = crate::storage::state_manager::StateManager::new().unwrap();

        let result = handler
            .abort_task(None, &state_manager, "test-task", None)
            .await;
        assert!(result.is_ok(), "abort_task should succeed");

        assert!(
            handler.is_cancelled().await,
            "Cancellation flag should be set"
        );
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
}
