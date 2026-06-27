//! File-backed command history for the TUI input.
//!
//! Manages loading, navigating, and persisting command history
//! to the sned data directory.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
                .map(std::string::ToString::to_string)
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

fn append_command_to_history(history_path: &Path, command: &str) -> io::Result<()> {
    if command.trim().is_empty() {
        return Ok(());
    }

    let dir = history_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path)?;
    writeln!(file, "{command}")?;
    Ok(())
}

/// Append a command to the history file.
/// Creates the file if it doesn't exist.
pub(crate) fn append_to_history(command: &str) -> io::Result<()> {
    let history_path = get_history_file_path();
    append_command_to_history(&history_path, command)
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
    #[must_use] 
    pub fn load() -> Self {
        Self {
            entries: load_command_history(),
            index: -1,
        }
    }

    /// Record a submitted command in the in-memory history list.
    pub fn push(&mut self, command: String) {
        if command.trim().is_empty() {
            return;
        }

        self.entries.push(command);
        let max = max_history_lines();
        if self.entries.len() > max {
            let keep_from = self.entries.len() - max;
            self.entries.drain(..keep_from);
        }
        self.index = -1;
    }

    /// Navigate backward in history (Up arrow).
    /// Returns the entry to display, if any.
    pub fn navigate_up(&mut self) -> Option<&str> {
        if self.entries.is_empty() || self.index >= self.entries.len() as isize - 1 {
            return None;
        }
        self.index += 1;
        let idx = (self.entries.len() as isize - 1 - self.index) as usize;
        self.entries.get(idx).map(std::string::String::as_str)
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
            self.entries.get(idx).map(std::string::String::as_str)
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
    #[must_use] 
    pub fn is_navigating(&self) -> bool {
        self.index >= 0
    }

    /// Reload history from disk and reset navigation.
    pub fn reload(&mut self) {
        self.entries = load_command_history();
        self.index = -1;
    }

    /// Get a reference to the underlying entries (for `/history` command).
    #[must_use] 
    pub fn entries(&self) -> &[String] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_append_command_to_history_appends_lines() {
        let temp_dir = tempfile::tempdir().unwrap();
        let history_path = temp_dir.path().join("cli_history");

        append_command_to_history(&history_path, "first").unwrap();
        append_command_to_history(&history_path, "second").unwrap();

        let content = std::fs::read_to_string(&history_path).unwrap();
        assert_eq!(content, "first\nsecond\n");
    }

    #[test]
    fn test_file_history_push_appends_in_memory_and_resets_navigation() {
        let mut history = FileHistory {
            entries: vec!["first".to_string()],
            index: 3,
        };

        history.push("second".to_string());

        assert_eq!(
            history.entries().to_vec(),
            vec!["first".to_string(), "second".to_string()]
        );
        assert!(!history.is_navigating());
    }
}
