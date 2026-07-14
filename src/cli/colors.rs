//! Terminal color/styling utilities with NO_COLOR and TTY detection.
//!
//! Respects the NO_COLOR environment variable (https://no-color.org/)
//! and disables ANSI codes when the target stream is not a TTY.

use std::io::IsTerminal;

fn env_colors_disabled() -> bool {
    std::env::var("NO_COLOR").is_ok() || std::env::var("TERM").is_ok_and(|t| t == "dumb")
}

// Re-evaluate on every call so tests that mutate NO_COLOR mid-process take
// effect immediately. The cost is one std::env::var + one is_terminal() per
// call, which is negligible for human-scale output.
#[must_use]
pub fn stdout_colors_disabled() -> bool {
    env_colors_disabled() || !std::io::stdout().is_terminal()
}

#[must_use]
pub fn stderr_colors_disabled() -> bool {
    env_colors_disabled() || !std::io::stderr().is_terminal()
}

/// ANSI color/style codes.
pub mod style {
    /// Reset all styles
    pub const RESET: &str = "\x1b[0m";
    /// Bold text
    pub const BOLD: &str = "\x1b[1m";
    /// Dim text
    pub const DIM: &str = "\x1b[2m";
    /// Italic text
    pub const ITALIC: &str = "\x1b[3m";
    /// Underlined text
    pub const UNDERLINE: &str = "\x1b[4m";
    /// Strikethrough text
    pub const STRIKETHROUGH: &str = "\x1b[9m";

    /// Bright cyan (for model output)
    pub const CYAN: &str = "\x1b[96m";
    /// Bright green (for prompts)
    pub const GREEN: &str = "\x1b[92m";
    /// Bright yellow (for warnings)
    pub const YELLOW: &str = "\x1b[93m";
    /// Bright red (for errors)
    pub const RED: &str = "\x1b[91m";
    /// Bright magenta (for tool calls)
    pub const MAGENTA: &str = "\x1b[95m";
    /// Bright blue (for info)
    pub const BLUE: &str = "\x1b[94m";
    /// Gray/dim (for reasoning)
    pub const GRAY: &str = "\x1b[90m";
    /// White
    pub const WHITE: &str = "\x1b[97m";
}

/// Wrap text with ANSI codes for stdout, respecting NO_COLOR and TTY status.
#[must_use]
pub fn colorize(text: &str, code: &str) -> String {
    if stdout_colors_disabled() {
        text.to_string()
    } else {
        format!("{}{}{}", code, text, style::RESET)
    }
}

/// Wrap text with ANSI codes for stderr, respecting NO_COLOR and TTY status.
#[must_use]
pub fn colorize_stderr(text: &str, code: &str) -> String {
    if stderr_colors_disabled() {
        text.to_string()
    } else {
        format!("{}{}{}", code, text, style::RESET)
    }
}

/// Format a tool name with color for prompts (stdout).
#[must_use]
pub fn tool_name(name: &str) -> String {
    colorize(name, style::BOLD)
}

/// Format an error message consistently (for stderr output).
#[must_use]
pub fn error(text: &str) -> String {
    if stderr_colors_disabled() {
        format!("[sned] ERROR: {text}")
    } else {
        format!(
            "{}[sned]{} {}ERROR:{} {}",
            style::DIM,
            style::RESET,
            style::RED,
            style::RESET,
            text
        )
    }
}

/// Format the interactive prompt (stdout).
#[must_use]
pub fn prompt(text: &str) -> String {
    colorize(text, style::GREEN)
}

/// Print an error message to stderr.
pub fn eprint_error(text: &str) {
    eprintln!("{}", error(text));
}

/// Format a tool result success indicator.
#[must_use]
pub fn tool_success(tool_name: &str) -> String {
    if stdout_colors_disabled() {
        format!("✓ {tool_name}")
    } else {
        format!(
            "{}✓{} {}{}{}",
            style::GREEN,
            style::RESET,
            style::BOLD,
            tool_name,
            style::RESET
        )
    }
}

/// Format a tool result failure indicator.
#[must_use]
pub fn tool_failure(tool_name: &str) -> String {
    if stdout_colors_disabled() {
        format!("✗ {tool_name}")
    } else {
        format!(
            "{}✗{} {}{}{}",
            style::RED,
            style::RESET,
            style::BOLD,
            tool_name,
            style::RESET
        )
    }
}

/// Format a section header (for grouping related output).
#[must_use]
pub fn section_header(text: &str) -> String {
    if stdout_colors_disabled() {
        format!("═══ {text}")
    } else {
        format!(
            "{}═══{} {}{}{}",
            style::DIM,
            style::RESET,
            style::BOLD,
            text,
            style::RESET
        )
    }
}

/// Format a diff addition line.
#[must_use]
pub fn diff_addition(text: &str) -> String {
    if stdout_colors_disabled() {
        format!("+ {text}")
    } else {
        format!("{}+ {}{}", style::GREEN, text, style::RESET)
    }
}

/// Format a diff removal line.
#[must_use]
pub fn diff_removal(text: &str) -> String {
    if stdout_colors_disabled() {
        format!("- {text}")
    } else {
        format!("{}- {}{}", style::RED, text, style::RESET)
    }
}

/// Format a diff context line.
#[must_use]
pub fn diff_context(text: &str) -> String {
    if stdout_colors_disabled() {
        format!("  {text}")
    } else {
        format!("{}  {}{}", style::DIM, text, style::RESET)
    }
}

/// Format a file path for display.
#[must_use]
pub fn file_path(path: &str) -> String {
    if stdout_colors_disabled() {
        path.to_string()
    } else {
        format!("{}{}{}", style::CYAN, path, style::RESET)
    }
}

/// Print a horizontal rule via the output writer.
pub fn print_horizontal_rule_writer(writer: &crate::cli::output::OutputWriterArc) {
    use crate::cli::output::OutputEvent;
    if stderr_colors_disabled() {
        writer.emit(OutputEvent::RawAnsi(
            "────────────────────────────────────────\n".to_string(),
        ));
    } else {
        writer.emit(OutputEvent::RawAnsi(format!(
            "{}────────────────────────────────────────{}\n",
            style::DIM,
            style::RESET
        )));
    }
}

/// Format reasoning/thinking text with dim styling.
#[must_use]
pub fn reasoning(text: &str) -> String {
    if stdout_colors_disabled() {
        text.to_string()
    } else {
        format!("{}{}{}", style::DIM, text, style::RESET)
    }
}

/// Format a status badge (e.g., [PLAN], [ACT]) for stdout.
#[must_use]
pub fn badge(text: &str) -> String {
    if stdout_colors_disabled() {
        format!("[{text}]")
    } else {
        format!("{}[{}{}]{}", style::DIM, style::BOLD, text, style::RESET)
    }
}

/// Format a status badge (e.g., [PLAN], [ACT]) for stderr output.
#[must_use]
pub fn badge_stderr(text: &str) -> String {
    if stderr_colors_disabled() {
        format!("[{text}]")
    } else {
        format!("{}[{}{}]{}", style::DIM, style::BOLD, text, style::RESET)
    }
}

/// Spinner frames for progress indication.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Get the current spinner frame based on an index.
#[must_use]
pub fn spinner_frame(index: usize) -> char {
    SPINNER_FRAMES[index % SPINNER_FRAMES.len()]
}

/// Format a wait/loading message.
#[must_use]
pub fn waiting(message: &str) -> String {
    if stdout_colors_disabled() {
        format!("⏳ {message}")
    } else {
        format!(
            "{}⏳{} {}{}",
            style::YELLOW,
            style::RESET,
            message,
            style::DIM
        )
    }
}

/// Wrap a file path in OSC 8 hyperlink escape sequence.
///
/// Terminals that support OSC 8 will render it as a clickable link.
/// Unsupported terminals will display the path as plain text.
/// Respects NO_COLOR and TTY detection.
#[must_use]
pub fn hyperlink_path(path: &str) -> String {
    if stderr_colors_disabled() {
        return path.to_string();
    }

    let abs_path = if path.starts_with('/') {
        path.to_string()
    } else if let Some(rest) = path.strip_prefix('~') {
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        dirs::home_dir().map_or_else(
            || path.to_string(),
            |h| h.join(rest).to_string_lossy().into_owned(),
        )
    } else {
        std::env::current_dir().map_or_else(
            |_| path.to_string(),
            |d| d.join(path).to_string_lossy().into_owned(),
        )
    };

    format!("\x1b]8;;file://{abs_path}\x1b\\{path}\x1b]8;;\x1b\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    #[test]
    fn test_colorize_returns_plain_when_no_color_set() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        if std::env::var("NO_COLOR").is_ok() || std::env::var("TERM").as_deref() == Ok("dumb") {
            let result = colorize("hello", style::RED);
            assert_eq!(
                result, "hello",
                "should be plain when NO_COLOR or TERM=dumb"
            );
        }
    }

    #[test]
    fn test_colorize_contains_ansi_when_tty() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        if std::env::var("NO_COLOR").is_err()
            && std::env::var("TERM").as_deref() != Ok("dumb")
            && std::io::stdout().is_terminal()
        {
            let result = colorize("hello", style::RED);
            assert!(
                result.contains("\x1b["),
                "should contain ANSI codes when stdout is a TTY and NO_COLOR is unset"
            );
            assert!(result.contains("hello"));
        }
    }

    #[test]
    fn test_colorize_stderr_returns_plain_when_no_color_set() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        if std::env::var("NO_COLOR").is_ok() || std::env::var("TERM").as_deref() == Ok("dumb") {
            let result = colorize_stderr("hello", style::RED);
            assert_eq!(
                result, "hello",
                "should be plain when NO_COLOR or TERM=dumb"
            );
        }
    }

    #[test]
    fn test_env_colors_disabled_respects_no_color() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let original = std::env::var("NO_COLOR").ok();
        // SAFETY: single-threaded test; env mutation guarded by sequential execution
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }
        assert!(env_colors_disabled());
        // SAFETY: single-threaded test; restoring env after assertion
        unsafe {
            match original {
                Some(v) => std::env::set_var("NO_COLOR", v),
                None => std::env::remove_var("NO_COLOR"),
            }
        }
    }

    #[test]
    fn test_env_colors_disabled_respects_term_dumb() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let original = std::env::var("TERM").ok();
        // SAFETY: single-threaded test; env mutation guarded by sequential execution
        unsafe {
            std::env::set_var("TERM", "dumb");
        }
        assert!(env_colors_disabled());
        // SAFETY: single-threaded test; restoring env after assertion
        unsafe {
            match original {
                Some(v) => std::env::set_var("TERM", v),
                None => std::env::remove_var("TERM"),
            }
        }
    }

    #[test]
    fn test_badge_stderr_uses_stderr_tty_check() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        // badge_stderr should check stderr TTY status, not stdout
        if std::env::var("NO_COLOR").is_err()
            && std::env::var("TERM").as_deref() != Ok("dumb")
            && std::io::stderr().is_terminal()
        {
            let result = badge_stderr("ACT");
            assert!(
                result.contains("\x1b["),
                "badge_stderr should contain ANSI when stderr is TTY"
            );
            assert!(result.contains("ACT"));
        }
    }

    #[test]
    fn test_error_format() {
        let plain = error("fail");
        assert!(plain.contains("ERROR"));
        assert!(plain.contains("fail"));
    }
}
