//! Terminal input handling for sned CLI.
//!
//! Ports raw-mode entry/exit, keyboard input parsing, and signal handling
//! from `dirac/cli/src/index.ts` and `dirac/cli/src/constants/keyboard.ts`.
//!
//! ## Design
//!
//! - `RawModeGuard` enters raw mode on creation and restores it on drop,
//!   matching Ink's behaviour.
//! - `TerminalEvent` enumerates the key events that terminals need.
//! - `InputParser` converts raw byte sequences into `TerminalEvent`s.
//!   It replicates the exact escape-sequence tables from the TypeScript
//!   source so that Home / End / Backspace / Delete / Option-arrows work
//!   across Terminal.app, iTerm2, Ghostty, and Linux consoles.
//! - `setup_sigterm_handler` installs a SIGTERM handler that restores the
//!   terminal before exiting. Ctrl+C is handled via TerminalEvent::Ctrl('c').

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, Write};
use tokio::sync::mpsc;

/// --------------------------------------------------------------------------
/// Raw mode
/// --------------------------------------------------------------------------
/// Enter raw mode and return a guard that restores it on drop.
///
/// Corresponds to Ink's implicit `process.stdin.setRawMode(true)` on mount.
pub fn enter_raw_mode() -> io::Result<RawModeGuard> {
    enable_raw_mode()?;
    Ok(RawModeGuard { active: true })
}

/// RAII guard that restores canonical terminal mode when dropped.
///
/// The guard can also be consumed with `restore()` to disable raw mode
/// early without dropping.
pub struct RawModeGuard {
    active: bool,
}

impl RawModeGuard {
    /// Explicitly restore canonical mode.
    pub fn restore(mut self) {
        self._restore();
    }

    pub(crate) fn _restore(&mut self) {
        if self.active {
            // Best-effort restore – never panic on cleanup.
            let _ = disable_raw_mode();
            self.active = false;
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self._restore();
    }
}
/// --------------------------------------------------------------------------
/// Terminal events (the vocabulary terminals and the UI consume)
/// --------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Printable character or whitespace.
    Char(char),
    /// Return / Enter.
    Return,
    /// Tab.
    Tab,
    /// Backspace (delete character left of cursor).
    Backspace,
    /// Delete (delete character under cursor).
    Delete,
    /// Escape.
    Escape,
    /// Arrow keys.
    Arrow(ArrowDirection),
    /// Home key.
    Home,
    /// End key.
    End,
    /// Page up / down.
    PageUp,
    PageDown,
    /// Ctrl+letter (a–z).
    Ctrl(char),
    /// Option / Alt + arrow.
    OptionArrow(ArrowDirection),
    /// Option / Alt + character.
    OptionChar(char),
    /// Paste event containing a string.
    Paste(String),
    /// Terminal resized.
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Mouse tracking sequences (ignored by the parser).
    Mouse,
    /// Unrecognised escape sequence.
    Unknown(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowDirection {
    Up,
    Down,
    Left,
    Right,
}

/// --------------------------------------------------------------------------
/// Input parser
/// --------------------------------------------------------------------------
/// State machine that turns a byte stream into `TerminalEvent`s.
///
/// The parser is deliberately synchronous and testable: feed it bytes
use std::collections::VecDeque;

/// with `feed()` and pull events with `next_event()`.  This makes it
/// easy to unit-test without touching the real TTY.
pub struct InputParser {
    buf: VecDeque<u8>,
    paste_mode: bool,
    paste_buffer: Vec<u8>,
}

/// Maximum paste size to prevent memory exhaustion (200KB default, configurable via SNED_MAX_PASTE_SIZE)
const MAX_PASTE_SIZE_DEFAULT: usize = 200_000;

/// Get the maximum paste size from environment or default.
/// Uses OnceLock for lazy initialization without global mutable state.
fn max_paste_size() -> usize {
    use std::sync::OnceLock;
    static MAX_SIZE: OnceLock<usize> = OnceLock::new();
    *MAX_SIZE.get_or_init(|| {
        std::env::var("SNED_MAX_PASTE_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(MAX_PASTE_SIZE_DEFAULT)
    })
}

impl Default for InputParser {
    fn default() -> Self {
        Self::new()
    }
}

impl InputParser {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            paste_mode: false,
            paste_buffer: Vec::new(),
        }
    }

    /// Append raw bytes (e.g. from `std::io::stdin`).
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
    }

    /// Try to decode the next event from the buffered bytes.
    ///
    /// Returns `Some(event)` when a complete sequence has been parsed,
    /// `None` when more bytes are needed.
    pub fn next_event(&mut self) -> Option<TerminalEvent> {
        if self.buf.is_empty() {
            return None;
        }

        // In paste mode, accumulate bytes and check for end marker
        if self.paste_mode {
            // Check for paste end marker at the start of input buffer
            let is_paste_end =
                self.buf.len() >= 6 && self.buf.iter().take(6).eq(b"\x1b[201~".iter());
            if is_paste_end {
                self.paste_mode = false;
                self.buf.drain(..6);
                let paste_content = String::from_utf8_lossy(&self.paste_buffer).to_string();
                self.paste_buffer.clear();
                return Some(TerminalEvent::Paste(paste_content));
            }
            // Check if paste_buffer ends with partial end marker and input buffer completes it
            // This handles the case where \x1b[201~ is split across multiple stdin reads
            let paste_end_prefix = b"\x1b[201~";
            for prefix_len in 1..6 {
                if self.paste_buffer.len() >= prefix_len
                    && &self.paste_buffer[self.paste_buffer.len() - prefix_len..]
                        == &paste_end_prefix[..prefix_len]
                    && self.buf.len() >= 6 - prefix_len
                    && self.buf.iter().take(6 - prefix_len).eq(
                        paste_end_prefix[prefix_len..].iter()
                    )
                {
                    // Found split end marker
                    self.paste_buffer.truncate(self.paste_buffer.len() - prefix_len);
                    self.buf.drain(..6 - prefix_len);
                    self.paste_mode = false;
                    let paste_content =
                        String::from_utf8_lossy(&self.paste_buffer).to_string();
                    self.paste_buffer.clear();
                    return Some(TerminalEvent::Paste(paste_content));
                }
            }
            // Enforce maximum paste size to prevent memory exhaustion
            if self.paste_buffer.len() >= max_paste_size() {
                // Truncate paste - user pasted too much
                self.paste_mode = false;
                self.paste_buffer.clear();
                // Skip remaining paste content (O(n) but only happens once on truncation)
                while self.buf.len() >= 6 && !self.buf.iter().take(6).eq(b"\x1b[201~".iter()) {
                    self.buf.pop_front();
                }
                if self.buf.len() >= 6 && self.buf.iter().take(6).eq(b"\x1b[201~".iter()) {
                    self.buf.drain(..6);
                }
                return Some(TerminalEvent::Paste(
                    format!("[paste truncated - exceeded {} byte limit]", max_paste_size())
                ));
            }
            // Buffer content during paste (O(1) with VecDeque)
            // Bytes are buffered one at a time, checking for end marker after each
            if let Some(byte) = self.buf.pop_front() {
                self.paste_buffer.push(byte);
            }
            // Keep processing to find paste end
            if self.buf.is_empty() {
                return None;
            }
            return self.next_event();
        }

        // Fast path: single-byte control characters.
        let is_escape = !self.buf.is_empty() && *self.buf.front().unwrap() == 0x1b;
        if self.buf.len() == 1 || !is_escape {
            return self.parse_single_byte();
        }

        // Escape-sequence path.
        self.parse_escape_sequence()
    }

    /// Drain all complete events from the buffer.
    pub fn drain_events(&mut self) -> Vec<TerminalEvent> {
        let mut out = Vec::new();
        // Keep processing while we have bytes
        while !self.buf.is_empty() {
            if let Some(ev) = self.next_event() {
                out.push(ev);
            } else {
                // next_event returned None but we still have bytes
                // In paste mode, this is normal (buffering content)
                // Outside paste mode, this means incomplete sequence - break
                if !self.paste_mode {
                    break;
                }
                // In paste mode with bytes remaining, keep trying
                // (the paste end marker should be in the buffer)
            }
        }
        out
    }

    fn parse_single_byte(&mut self) -> Option<TerminalEvent> {
        let b = self.buf.front()?;

        let ev = match *b {
            b'\r' | b'\n' => TerminalEvent::Return,
            b'\t' => TerminalEvent::Tab,
            0x7f => TerminalEvent::Backspace, // DEL
            0x08 => TerminalEvent::Backspace, // Ctrl+H
            0x1b => {
                // Lone ESC – more bytes may follow, so don't consume yet
                // unless this is truly the only byte.
                if self.buf.len() == 1 {
                    TerminalEvent::Escape
                } else {
                    return None;
                }
            }
            0x00..=0x1f => {
                // Ctrl+letter  (0x01 = Ctrl+A, 0x1a = Ctrl+Z)
                let letter = (*b + b'a' - 1) as char;
                TerminalEvent::Ctrl(letter)
            }
            byte => {
                if let Some(c) = char::from_u32(byte as u32) {
                    TerminalEvent::Char(c)
                } else {
                    TerminalEvent::Unknown(vec![byte])
                }
            }
        };
        self.buf.pop_front();
        Some(ev)
    }

    fn parse_escape_sequence(&mut self) -> Option<TerminalEvent> {
        // We know buf starts with 0x1b.
        let seq = &self.buf;

        // Minimum length for any meaningful ESC sequence is 2.
        if seq.len() < 2 {
            return None;
        }

        // ESC Backspace / ESC Ctrl+H – must be checked before Option+char
        // because DEL (0x7f) and BS (0x08) are not '[' or 'O'.
        if seq[1] == 0x7f || seq[1] == 0x08 {
            self.buf.drain(..2);
            return Some(TerminalEvent::Backspace);
        }

        // Bracketed paste mode detection - paste start
        if seq[1] == b'[' && seq.len() >= 6 && seq.iter().take(6).eq(b"\x1b[200~".iter()) {
            self.paste_mode = true;
            self.paste_buffer.clear();
            self.buf.drain(..6);
            return None; // Wait for paste content
        }

        // Option+char (Meta): ESC <char>
        if seq[1] != b'[' && seq[1] != b'O' {
            let ch = seq[1] as char;
            let ev = if ch == 'b' {
                TerminalEvent::OptionArrow(ArrowDirection::Left)
            } else if ch == 'f' {
                TerminalEvent::OptionArrow(ArrowDirection::Right)
            } else {
                TerminalEvent::OptionChar(ch)
            };
            self.buf.drain(..2);
            return Some(ev);
        }

        // CSI sequences: ESC [ ...
        if seq[1] == b'[' {
            return self.parse_csi();
        }

        // SS3 sequences: ESC O ...
        if seq[1] == b'O' && seq.len() >= 3 {
            let ev = match seq[2] {
                b'H' => TerminalEvent::Home,
                b'F' => TerminalEvent::End,
                b'A' => TerminalEvent::Arrow(ArrowDirection::Up),
                b'B' => TerminalEvent::Arrow(ArrowDirection::Down),
                b'C' => TerminalEvent::Arrow(ArrowDirection::Right),
                b'D' => TerminalEvent::Arrow(ArrowDirection::Left),
                _ => TerminalEvent::Unknown(self.buf.iter().take(3).copied().collect()),
            };
            self.buf.drain(..3);
            return Some(ev);
        }

        // Unrecognised – consume the ESC to avoid getting stuck.
        self.buf.pop_front();
        Some(TerminalEvent::Escape)
    }

    fn parse_csi(&mut self) -> Option<TerminalEvent> {
        // Find the final byte (0x40–0x7E), starting after ESC [.
        let end = self
            .buf
            .iter()
            .enumerate()
            .position(|(i, &b)| i >= 2 && (0x40..=0x7E).contains(&b))?;
        let payload: Vec<u8> = self.buf.iter().take(end + 1).copied().collect();
        let consumed = end + 1;

        let ev = if payload == b"\x1b[A" {
            TerminalEvent::Arrow(ArrowDirection::Up)
        } else if payload == b"\x1b[B" {
            TerminalEvent::Arrow(ArrowDirection::Down)
        } else if payload == b"\x1b[C" {
            TerminalEvent::Arrow(ArrowDirection::Right)
        } else if payload == b"\x1b[D" {
            TerminalEvent::Arrow(ArrowDirection::Left)
        } else if payload == b"\x1b[H" || payload == b"\x1b[1~" || payload == b"\x1b[7~" {
            TerminalEvent::Home
        } else if payload == b"\x1b[F" || payload == b"\x1b[4~" || payload == b"\x1b[8~" {
            TerminalEvent::End
        } else if payload == b"\x1b[3~" {
            TerminalEvent::Delete
        } else if payload == b"\x1b[5~" {
            TerminalEvent::PageUp
        } else if payload == b"\x1b[6~" {
            TerminalEvent::PageDown
        } else if payload == b"\x1b[1;3C" {
            TerminalEvent::OptionArrow(ArrowDirection::Right)
        } else if payload == b"\x1b[1;3D" {
            TerminalEvent::OptionArrow(ArrowDirection::Left)
        } else if payload.starts_with(b"\x1b[<") {
            // Mouse tracking (SGR 1006)
            TerminalEvent::Mouse
        } else {
            TerminalEvent::Unknown(payload)
        };

        self.buf.drain(..consumed);
        Some(ev)
    }
}

/// --------------------------------------------------------------------------
/// Signal handling
/// --------------------------------------------------------------------------
/// Install SIGTERM signal handler for graceful shutdown.
///
/// Mirrors `setupSignalHandlers()` in `dirac/cli/src/index.ts`:
/// - SIGTERM triggers graceful shutdown with immediate exit on second signal.
/// - Terminal mode is always restored before exiting.
///
/// Note: Ctrl+C is handled via TerminalEvent::Ctrl('c') in the interactive
/// shell's main event loop (raw mode captures 0x03 byte). This handler is
/// only for SIGTERM.
///
/// # Usage
///
/// Call this once at application startup.
pub fn setup_sigterm_handler() {
    // SIGTERM (Unix only)
    #[cfg(unix)]
    {
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to install SIGTERM handler: {}", e);
                    return;
                }
            };
            #[allow(clippy::never_loop)]
            loop {
                sigterm.recv().await;
                // First SIGTERM – disable raw mode and exit
                let _ = disable_raw_mode();
                // Use raw ANSI escapes to avoid potential blocking
                let _ = std::io::stderr().write_all(b"\x1b[?25h");
                let _ = std::io::stderr().write_all(b"\x1b[0m");
                let _ = std::io::stderr().flush();
                std::process::exit(crate::exit_codes::EXIT_INTERRUPTED);
            }
        });
    }
}

/// Install a SIGWINCH handler that sends resize events through a channel.
///
/// On Unix, when the terminal window is resized, this handler reads the new
/// dimensions via crossterm and emits a `TerminalEvent::Resize`.
#[cfg(unix)]
pub fn setup_sigwinch_handler(tx: mpsc::Sender<TerminalEvent>) -> Result<(), io::Error> {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigwinch = match signal(SignalKind::window_change()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to install SIGWINCH handler: {}", e);
                return;
            }
        };
        loop {
            sigwinch.recv().await;
            match crossterm::terminal::size() {
                Ok((cols, rows)) => {
                    let _ = tx.send(TerminalEvent::Resize { cols, rows }).await;
                }
                Err(e) => {
                    tracing::warn!("Failed to read terminal size on SIGWINCH: {}", e);
                }
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
pub fn setup_sigwinch_handler(_tx: mpsc::Sender<TerminalEvent>) -> Result<(), io::Error> {
    // SIGWINCH is not available on non-Unix platforms.
    Ok(())
}

/// --------------------------------------------------------------------------
/// Panic hook – restore terminal on panic
/// --------------------------------------------------------------------------
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
        let _ = std::io::stderr().write_all(b"\x1b[?25h"); // show cursor
        let _ = std::io::stderr().write_all(b"\x1b[0m"); // reset colors
        let _ = std::io::stderr().flush();
        original(info);
    }));
}

/// --------------------------------------------------------------------------
/// Convenience: stdin reader
/// --------------------------------------------------------------------------
/// Read raw bytes from stdin and parse them into events.
///
/// This is a blocking convenience for non-async contexts.  In the real
/// application the async event loop will read from stdin directly.

/// --------------------------------------------------------------------------
/// Tests
/// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Raw mode guard ----

    #[test]
    fn test_raw_mode_guard_drop_restores() {
        // This test cannot actually enter raw mode (it would break the
        // test runner's terminal), so we just verify the guard compiles
        // and its Drop impl calls disable_raw_mode (a no-op when not in
        // raw mode).
        let guard = RawModeGuard { active: true };
        drop(guard);
    }

    #[test]
    fn test_raw_mode_guard_restore() {
        let guard = RawModeGuard { active: true };
        guard.restore();
    }

    // ---- Single-byte controls ----

    #[test]
    fn test_parse_return() {
        let mut p = InputParser::new();
        p.feed(b"\r");
        assert_eq!(p.next_event(), Some(TerminalEvent::Return));
        assert_eq!(p.next_event(), None);
    }

    #[test]
    fn test_parse_tab() {
        let mut p = InputParser::new();
        p.feed(b"\t");
        assert_eq!(p.next_event(), Some(TerminalEvent::Tab));
    }

    #[test]
    fn test_parse_backspace_del() {
        let mut p = InputParser::new();
        p.feed(b"\x7f");
        assert_eq!(p.next_event(), Some(TerminalEvent::Backspace));
    }

    #[test]
    fn test_parse_backspace_ctrl_h() {
        let mut p = InputParser::new();
        p.feed(b"\x08");
        assert_eq!(p.next_event(), Some(TerminalEvent::Backspace));
    }

    #[test]
    fn test_parse_ctrl_shortcuts() {
        let mut p = InputParser::new();
        p.feed(b"\x01"); // Ctrl+A
        assert_eq!(p.next_event(), Some(TerminalEvent::Ctrl('a')));

        p.feed(b"\x05"); // Ctrl+E
        assert_eq!(p.next_event(), Some(TerminalEvent::Ctrl('e')));

        p.feed(b"\x15"); // Ctrl+U
        assert_eq!(p.next_event(), Some(TerminalEvent::Ctrl('u')));

        p.feed(b"\x0b"); // Ctrl+K
        assert_eq!(p.next_event(), Some(TerminalEvent::Ctrl('k')));

        p.feed(b"\x17"); // Ctrl+W
        assert_eq!(p.next_event(), Some(TerminalEvent::Ctrl('w')));
    }

    #[test]
    fn test_parse_printable_chars() {
        let mut p = InputParser::new();
        p.feed(b"abc");
        assert_eq!(p.next_event(), Some(TerminalEvent::Char('a')));
        assert_eq!(p.next_event(), Some(TerminalEvent::Char('b')));
        assert_eq!(p.next_event(), Some(TerminalEvent::Char('c')));
        assert_eq!(p.next_event(), None);
    }

    // ---- Arrow keys ----

    #[test]
    fn test_parse_arrows() {
        let mut p = InputParser::new();
        p.feed(b"\x1b[A\x1b[B\x1b[C\x1b[D");
        assert_eq!(
            p.next_event(),
            Some(TerminalEvent::Arrow(ArrowDirection::Up))
        );
        assert_eq!(
            p.next_event(),
            Some(TerminalEvent::Arrow(ArrowDirection::Down))
        );
        assert_eq!(
            p.next_event(),
            Some(TerminalEvent::Arrow(ArrowDirection::Right))
        );
        assert_eq!(
            p.next_event(),
            Some(TerminalEvent::Arrow(ArrowDirection::Left))
        );
    }

    // ---- Home / End ----

    #[test]
    fn test_parse_home_sequences() {
        for seq in [b"\x1b[H".as_slice(), b"\x1b[1~", b"\x1bOH", b"\x1b[7~"] {
            let mut p = InputParser::new();
            p.feed(seq);
            assert_eq!(
                p.next_event(),
                Some(TerminalEvent::Home),
                "Failed for sequence {:?}",
                seq
            );
        }
    }

    #[test]
    fn test_parse_end_sequences() {
        for seq in [b"\x1b[F".as_slice(), b"\x1b[4~", b"\x1bOF", b"\x1b[8~"] {
            let mut p = InputParser::new();
            p.feed(seq);
            assert_eq!(
                p.next_event(),
                Some(TerminalEvent::End),
                "Failed for sequence {:?}",
                seq
            );
        }
    }

    // ---- Backspace / Delete ----

    #[test]
    fn test_parse_delete_sequence() {
        let mut p = InputParser::new();
        p.feed(b"\x1b[3~");
        assert_eq!(p.next_event(), Some(TerminalEvent::Delete));
    }

    #[test]
    fn test_parse_esc_backspace() {
        let mut p = InputParser::new();
        p.feed(b"\x1b\x7f");
        assert_eq!(p.next_event(), Some(TerminalEvent::Backspace));
    }

    #[test]
    fn test_parse_esc_ctrl_h() {
        let mut p = InputParser::new();
        p.feed(b"\x1b\x08");
        assert_eq!(p.next_event(), Some(TerminalEvent::Backspace));
    }

    // ---- Option+arrow ----

    #[test]
    fn test_parse_option_left() {
        for seq in [b"\x1bb".as_slice(), b"\x1b[1;3D"] {
            let mut p = InputParser::new();
            p.feed(seq);
            assert_eq!(
                p.next_event(),
                Some(TerminalEvent::OptionArrow(ArrowDirection::Left)),
                "Failed for sequence {:?}",
                seq
            );
        }
    }

    #[test]
    fn test_parse_option_right() {
        for seq in [b"\x1bf".as_slice(), b"\x1b[1;3C"] {
            let mut p = InputParser::new();
            p.feed(seq);
            assert_eq!(
                p.next_event(),
                Some(TerminalEvent::OptionArrow(ArrowDirection::Right)),
                "Failed for sequence {:?}",
                seq
            );
        }
    }

    // ---- Page up / down ----

    #[test]
    fn test_parse_page_up_down() {
        let mut p = InputParser::new();
        p.feed(b"\x1b[5~\x1b[6~");
        assert_eq!(p.next_event(), Some(TerminalEvent::PageUp));
        assert_eq!(p.next_event(), Some(TerminalEvent::PageDown));
    }

    // ---- Mouse (ignored) ----

    #[test]
    fn test_parse_mouse_sequence() {
        let mut p = InputParser::new();
        p.feed(b"\x1b[<0;1;1M");
        assert_eq!(p.next_event(), Some(TerminalEvent::Mouse));
    }

    // ---- Unknown sequences ----

    #[test]
    fn test_parse_unknown_csi() {
        let mut p = InputParser::new();
        p.feed(b"\x1b[99~");
        assert!(matches!(p.next_event(), Some(TerminalEvent::Unknown(_))));
    }

    // ---- Escape alone ----

    #[test]
    fn test_parse_lone_escape() {
        let mut p = InputParser::new();
        p.feed(b"\x1b");
        assert_eq!(p.next_event(), Some(TerminalEvent::Escape));
    }

    // ---- Drain events ----

    #[test]
    fn test_drain_events() {
        let mut p = InputParser::new();
        p.feed(b"abc\x1b[A\x7f");
        let evs = p.drain_events();
        assert_eq!(evs.len(), 5);
        assert_eq!(evs[0], TerminalEvent::Char('a'));
        assert_eq!(evs[4], TerminalEvent::Backspace);
    }

    // ---- Partial sequences ----

    #[test]
    fn test_partial_sequence_waits() {
        let mut p = InputParser::new();
        p.feed(b"\x1b[");
        assert_eq!(p.next_event(), None); // need more bytes (incomplete CSI)
        p.feed(b"H");
        assert_eq!(p.next_event(), Some(TerminalEvent::Home));
    }

    // ---- Bracketed paste mode ----

    #[test]
    fn test_bracketed_paste_single_line() {
        let mut p = InputParser::new();
        // Paste start + content + paste end
        p.feed(b"\x1b[200~Hello World\x1b[201~");
        let evs = p.drain_events();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], TerminalEvent::Paste(ref s) if s == "Hello World"));
    }

    #[test]
    fn test_bracketed_paste_multiline() {
        let mut p = InputParser::new();
        // Paste multi-line content - should be a single Paste event
        p.feed(b"\x1b[200~Line 1\nLine 2\nLine 3\x1b[201~");
        let evs = p.drain_events();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], TerminalEvent::Paste(ref s) if s.contains('\n')));
    }

    #[test]
    fn test_bracketed_paste_with_newlines_not_return_events() {
        let mut p = InputParser::new();
        // Verify newlines in paste don't generate Return events
        p.feed(b"\x1b[200~Line 1\nLine 2\x1b[201~");
        let evs = p.drain_events();
        assert_eq!(evs.len(), 1);
        assert!(!evs.iter().any(|e| matches!(e, TerminalEvent::Return)));
    }

    #[test]
    fn test_normal_newlines_still_work() {
        let mut p = InputParser::new();
        // Outside paste mode, newlines should still generate Return events
        p.feed(b"test\ninput\r");
        let evs = p.drain_events();
        assert!(evs.iter().any(|e| matches!(e, TerminalEvent::Return)));
    }

    #[test]
    fn test_paste_mode_state_tracking() {
        let mut p = InputParser::new();
        assert!(!p.paste_mode);

        // Start paste
        p.feed(b"\x1b[200~");
        p.drain_events();
        assert!(p.paste_mode);

        // End paste
        p.feed(b"text\x1b[201~");
        let evs = p.drain_events();
        assert!(!p.paste_mode);
        assert!(matches!(evs[0], TerminalEvent::Paste(_)));
    }

    #[test]
    fn test_paste_end_marker_split_across_reads() {
        // Simulate end marker split across two stdin reads
        // The parser buffers bytes one at a time, so split markers are handled correctly
        let mut p = InputParser::new();

        // Start paste with content
        p.feed(b"\x1b[200~test content");
        p.drain_events();
        assert!(p.paste_mode);

        // Feed ESC (first byte of end marker) - gets buffered
        p.feed(b"\x1b");
        assert_eq!(p.next_event(), None); // Still waiting for more bytes

        // Feed rest of marker - should complete and emit Paste
        p.feed(b"[201~");
        let evs = p.drain_events();
        assert!(!p.paste_mode);
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], TerminalEvent::Paste(ref s) if s == "test content"));
    }

    #[test]
    fn test_paste_truncation_with_env_var() {
        // Set a small limit for testing
        unsafe {
            std::env::set_var("SNED_MAX_PASTE_SIZE", "50");
        }

        let mut p = InputParser::new();
        // Start paste with content exceeding limit
        let large_content = "x".repeat(100);
        p.feed(b"\x1b[200~");
        p.feed(large_content.as_bytes());
        p.feed(b"\x1b[201~");

        let evs = p.drain_events();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], TerminalEvent::Paste(ref s) if s.contains("truncated")));

        unsafe {
            std::env::remove_var("SNED_MAX_PASTE_SIZE");
        }
    }
}
