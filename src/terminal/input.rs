//! Terminal input handling for sned CLI.
//!
//! Provides panic hook installation to restore terminal state on panic.

use crossterm::terminal::disable_raw_mode;
use std::io::Write;

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
