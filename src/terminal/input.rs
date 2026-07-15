//! Terminal input handling for sned CLI.
//!
//! Provides panic hook installation to restore terminal state on panic.

use crossterm::terminal::disable_raw_mode;
use ratatui::crossterm::Command;
#[cfg(windows)]
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use std::fmt;
use std::io::Write;

/// Captures clicks and wheel input without claiming drag motion, so terminals
/// can keep their native text-selection behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnableSnedMouseCapture;

impl Command for EnableSnedMouseCapture {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1000h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        EnableMouseCapture.execute_winapi()
    }
}

/// Clears every mouse mode Sned enables before restoring the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisableSnedMouseCapture;

impl Command for DisableSnedMouseCapture {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1006l\x1b[?1000l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        DisableMouseCapture.execute_winapi()
    }
}

/// Install a panic hook that restores terminal state before printing the panic.
///
/// This prevents the terminal from staying in raw mode after a panic,
/// which would make the shell unusable.
pub fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore terminal state using raw ANSI sequences (no allocation)
        let _ = disable_raw_mode();
        // Use raw ANSI escapes to avoid allocation in panic handler
        let _ = std::io::stderr().write_all(b"\x1b[?1049l"); // leave alternate screen
        let _ = std::io::stderr().write_all(b"\x1b[?25h"); // show cursor
        let _ = std::io::stderr().write_all(b"\x1b[0m"); // reset colors
        let _ = std::io::stderr().flush();
        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sned_mouse_capture_avoids_drag_tracking() {
        let mut output = String::new();
        EnableSnedMouseCapture.write_ansi(&mut output).unwrap();

        assert_eq!(output, "\x1b[?1000h\x1b[?1006h");
    }

    #[test]
    fn sned_mouse_capture_teardown_matches_enabled_modes() {
        let mut output = String::new();
        DisableSnedMouseCapture.write_ansi(&mut output).unwrap();

        assert_eq!(output, "\x1b[?1006l\x1b[?1000l");
    }
}
