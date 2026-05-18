//! Terminal module for sned CLI.
//!
//! Provides terminal input handling, UI components, PTY-backed command execution,
//! and VT rendering for stripping ANSI escape sequences.

pub mod input;
pub mod picker;

#[cfg(unix)]
pub mod command_pty;
pub mod vt_renderer;

// Re-export the main types for convenience.
pub use input::{
    ArrowDirection, InputParser, RawModeGuard, TerminalEvent, enter_raw_mode, install_panic_hook,
    setup_sigwinch_handler,
};

#[cfg(unix)]
pub use command_pty::{CommandOutput, PtyError, run_command_in_pty};
pub use vt_renderer::{VtRenderer, strip_progress_artifacts};
