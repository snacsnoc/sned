//! Animated spinner for progress indication during long-running operations.
//!
//! Runs on a background tokio task, writes frames to stderr via `\r` overwrites.
//! Respects NO_COLOR and TTY detection via `cli::colors`.

use std::io::IsTerminal;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Animated spinner for progress indication during long-running operations.
///
/// Runs on a background tokio::task, writes frames to stderr via `\r` overwrites.
/// Supports pause/resume for coordinating with other stderr output (e.g., live command output).
pub struct Spinner {
    cancel: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
    enabled: bool,
}

impl Spinner {
    pub fn start(label: &str) -> Self {
        if !io::stderr().is_terminal() {
            return Self::disabled();
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        let stopped_clone = stopped.clone();
        let paused_clone = paused.clone();
        let label = label.to_string();
        let start = Instant::now();

        let handle = tokio::spawn(async move {
            let mut frame_idx = 0usize;
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(80));

            loop {
                if cancel_clone.load(Ordering::Relaxed) {
                    break;
                }
                interval.tick().await;
                if cancel_clone.load(Ordering::Relaxed) {
                    break;
                }

                // Check if paused - if so, clear line and wait without writing frames
                if paused_clone.load(Ordering::Relaxed) {
                    // Clear the spinner line while paused
                    eprint!("\r\x1b[K");
                    let _ = io::stderr().flush();
                    // Wait while paused, checking for cancel
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        if cancel_clone.load(Ordering::Relaxed) {
                            return;
                        }
                        if !paused_clone.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }

                let frame = SPINNER_FRAMES[frame_idx % SPINNER_FRAMES.len()];
                let elapsed = start.elapsed();
                let elapsed_str = if elapsed.as_secs() >= 60 {
                    format!("{}m {}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
                } else {
                    format!("{}ms", elapsed.as_millis())
                };
                let label_with_time = format!("{} ({})", label, elapsed_str);
                let styled_label = crate::cli::colors::colorize_stderr(
                    &label_with_time,
                    &format!(
                        "{}{}",
                        crate::cli::colors::style::BOLD,
                        crate::cli::colors::style::YELLOW
                    ),
                );
                eprint!("\r{} {} ", frame, styled_label);
                let _ = io::stderr().flush();
                frame_idx += 1;
            }

            // Only clear if not already stopped externally
            if !stopped_clone.load(Ordering::Relaxed) {
                eprint!("\r\x1b[K");
                let _ = io::stderr().flush();
            }
        });

        Self {
            cancel,
            stopped,
            paused,
            handle: Some(handle),
            enabled: true,
        }
    }

    fn disabled() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(true)),
            stopped: Arc::new(AtomicBool::new(true)),
            paused: Arc::new(AtomicBool::new(true)),
            handle: None,
            enabled: false,
        }
    }

    pub fn stop(mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if self.enabled {
            eprint!("\r\x1b[K");
            let _ = io::stderr().flush();
        }
    }

    pub fn stop_with_message(self, message: &str) {
        self.stop();
        if !message.is_empty() {
            eprintln!("\r{}", message);
        }
    }

    /// Pause the spinner animation. Clears the spinner line and suspends frame updates.
    /// Use this before writing other output to stderr to prevent cursor misalignment.
    pub fn pause(&self) {
        if self.enabled {
            self.paused.store(true, Ordering::Relaxed);
            // Clear the spinner line immediately
            eprint!("\r\x1b[K");
            let _ = io::stderr().flush();
        }
    }

    /// Resume the spinner animation after a pause.
    pub fn resume(&self) {
        if self.enabled {
            self.paused.store(false, Ordering::Relaxed);
        }
    }

    /// Returns true if the spinner is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Returns a clone of the internal pause flag.
    /// Use this to coordinate spinner pause/resume from outside the Spinner.
    pub fn pause_flag(&self) -> Arc<AtomicBool> {
        self.paused.clone()
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if self.enabled {
            eprint!("\r\x1b[K");
            let _ = io::stderr().flush();
        }
    }
}

/// Return a human-friendly spinner label for a given tool name.
pub fn tool_spinner_label(tool_name: &str) -> &'static str {
    match tool_name {
        "read_file" => "Reading file...",
        "write_to_file" => "Writing file...",
        "edit_file" => "Editing file...",
        "execute_command" | "execute_script" => "Running command...",
        "search_files" => "Searching files...",
        "list_files" => "Listing files...",
        "web_fetch" => "Fetching URL...",
        "diagnostics_scan" => "Running diagnostics...",
        "find_symbol_references" => "Finding references...",
        "replace_symbol" | "rename_symbol" => "Refactoring...",
        "get_file_skeleton" => "Extracting skeleton...",
        "attempt_completion" => "Completing...",
        "ask_followup_question" => "Asking...",
        "condense" => "Condensing context...",
        "new_task" => "Creating task...",
        "use_skill" => "Running skill...",
        "use_subagents" => "Dispatching agents...",
        "summarize_task" => "Summarizing...",
        _ => "Working...",
    }
}

/// Return a descriptive spinner label when multiple tools run in parallel.
pub fn multi_tool_label(tool_names: &[String]) -> String {
    if tool_names.is_empty() {
        return "Working...".to_string();
    }

    let unique: Vec<&str> = {
        let mut seen = std::collections::HashSet::new();
        tool_names
            .iter()
            .map(|s| s.as_str())
            .filter(|n| seen.insert(*n))
            .collect()
    };

    match unique.len() {
        1 => tool_spinner_label(unique[0]).to_string(),
        2..=3 => {
            let labels: Vec<&str> = unique
                .iter()
                .map(|n| tool_spinner_label(n).trim_end_matches('.'))
                .collect();
            format!("{}...", labels.join(", "))
        }
        _ => format!("Running {} tools...", unique.len()),
    }
}
