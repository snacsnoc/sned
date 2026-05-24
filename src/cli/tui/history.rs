//! File-backed command history for the TUI input.
//!
//! Manages loading, navigating, and persisting command history
//! to the sned data directory.

use std::path::PathBuf;

fn get_history_file_path() -> PathBuf {
    crate::storage::disk::get_sned_dir().join("cli_history")
}

fn max_history_lines() -> usize {
    std::env::var("SNED_HISTORY_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(10_000)
}

/// Load command history from the history file.
/// Returns lines with most recent last.
pub(crate) fn load_command_history() -> Vec<String> {
    let history_path = get_history_file_path();
    if !history_path.exists() {
        return Vec::new();
    }

    match std::fs::read_to_string(&history_path) {
        Ok(content) => {
            let mut lines: Vec<String> = content
                .lines()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .collect();

            let max = max_history_lines();
            if lines.len() > max {
                lines = lines[lines.len() - max..].to_vec();
            }

            lines
        }
        Err(_) => Vec::new(),
    }
}

/// Append a command to the history file.
/// Creates the file if it doesn't exist.
/// Trims the file to the configured max if it grows too large.
pub(crate) fn append_to_history(command: &str) {
    if command.trim().is_empty() {
        return;
    }

    let history_path = get_history_file_path();
    let default_path = PathBuf::from(".");
    let dir = history_path.parent().unwrap_or(&default_path);

    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("Warning: Failed to create history directory: {}", e);
        return;
    }

    let mut history = load_command_history();

    history.push(command.to_string());

    let max = max_history_lines();
    if history.len() > max {
        history = history[history.len() - max..].to_vec();
    }

    let content = history.join("\n") + "\n";
    if let Err(e) = crate::storage::disk::atomic_write_file(&history_path, &content) {
        eprintln!("Warning: Failed to save command history: {}", e);
    }
}

/// File-backed command history with navigation state.
///
/// Holds the loaded history list and a navigation index used by
/// Up/Down arrow keys. The navigation index tracks position
/// relative to the most-recent entry (0 = most recent, -1 = not navigating).
pub struct FileHistory {
    entries: Vec<String>,
    index: isize,
}

impl FileHistory {
    /// Load command history from disk.
    pub fn load() -> Self {
        Self {
            entries: load_command_history(),
            index: -1,
        }
    }

    /// Navigate backward in history (Up arrow).
    /// Returns the entry to display, if any.
    pub fn navigate_up(&mut self) -> Option<&str> {
        if self.entries.is_empty() || self.index >= self.entries.len() as isize - 1 {
            return None;
        }
        self.index += 1;
        let idx = (self.entries.len() as isize - 1 - self.index) as usize;
        self.entries.get(idx).map(|s| s.as_str())
    }

    /// Navigate forward in history (Down arrow).
    /// Returns the entry to display, or None to clear the input.
    pub fn navigate_down(&mut self) -> Option<&str> {
        if self.entries.is_empty() || self.index < 0 {
            return None;
        }
        if self.index > 0 {
            self.index -= 1;
            let idx = (self.entries.len() as isize - 1 - self.index) as usize;
            self.entries.get(idx).map(|s| s.as_str())
        } else {
            self.index = -1;
            None
        }
    }

    /// Reset navigation state (e.g., after submitting a command).
    pub fn reset(&mut self) {
        self.index = -1;
    }

    /// Whether the user is currently navigating history (index >= 0).
    pub fn is_navigating(&self) -> bool {
        self.index >= 0
    }

    /// Reload history from disk and reset navigation.
    pub fn reload(&mut self) {
        self.entries = load_command_history();
        self.index = -1;
    }

    /// Get a reference to the underlying entries (for `/history` command).
    pub fn entries(&self) -> &[String] {
        &self.entries
    }
}
