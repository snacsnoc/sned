//! Output abstraction for ratatui migration.
//!
//! This module provides the `OutputEvent` enum and `OutputWriter` trait that
//! allow agent output to be decoupled from the terminal. During the migration,
//! output flows through an `mpsc` channel, allowing the TUI render loop to be
//! the sole writer to the terminal.

use ratatui::text::{Line, Span};
use std::fmt;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

/// An output event that can be rendered by the TUI or forwarded to stderr.
#[derive(Clone, Debug)]
pub enum OutputEvent {
    /// A line of text with optional styling.
    Line(Line<'static>),
    /// Raw ANSI escape sequences (for PTY output, etc.).
    RawAnsi(String),
}

impl OutputEvent {
    /// Create a plain text line.
    pub fn plain(text: impl Into<String>) -> Self {
        OutputEvent::Line(Line::from(text.into()))
    }

    /// Create a styled line with the given style.
    pub fn styled(text: impl Into<String>, style: ratatui::style::Style) -> Self {
        OutputEvent::Line(Line::from(Span::styled(text.into(), style)))
    }

    /// Create an error line (red styling).
    pub fn error(text: impl fmt::Display) -> Self {
        use ratatui::style::{Color, Style};
        OutputEvent::Line(Line::from(Span::styled(
            format!("[sned] ERROR: {}", text),
            Style::default().fg(Color::Red),
        )))
    }

    /// Create a warning line (yellow styling).
    pub fn warning(text: impl fmt::Display) -> Self {
        use ratatui::style::{Color, Style};
        OutputEvent::Line(Line::from(Span::styled(
            format!("[sned] Warning: {}", text),
            Style::default().fg(Color::Yellow),
        )))
    }

    /// Create an info line (dim styling).
    pub fn info(text: impl fmt::Display) -> Self {
        use ratatui::style::{Color, Modifier, Style};
        OutputEvent::Line(Line::from(Span::styled(
            format!("[sned] {}", text),
            Style::default().fg(Color::White).add_modifier(Modifier::DIM),
        )))
    }

    /// Create a styled line with magenta color (for tool calls).
    pub fn tool_call(text: impl Into<String>) -> Self {
        use ratatui::style::{Color, Style};
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(Color::Magenta),
        )))
    }

    /// Create a styled line with cyan color (for model output).
    pub fn model_output(text: impl Into<String>) -> Self {
        use ratatui::style::{Color, Style};
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(Color::Cyan),
        )))
    }
}

/// Trait for writing output events.
///
/// This abstraction allows the same code to write to stderr (during migration)
/// or to a channel (for ratatui rendering).
pub trait OutputWriter: Send + Sync {
    /// Emit an output event.
    fn emit(&self, event: OutputEvent);

    /// Flush any buffered output.
    fn flush(&self);
}

/// Output writer that forwards to stderr.
///
/// Used during Phase 0-1 to keep old code working while new code
/// also writes to the channel.
pub struct StderrOutputWriter;

impl OutputWriter for StderrOutputWriter {
    fn emit(&self, event: OutputEvent) {
        match event {
            OutputEvent::Line(line) => {
                // For now, just render as plain text (colors come in Phase 1)
                eprintln!("{}", line);
            }
            OutputEvent::RawAnsi(s) => {
                eprint!("{}", s);
            }
        }
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Output writer that sends events through an mpsc channel.
///
/// The channel is unbounded; the render loop drains it on each frame tick.
pub struct ChannelOutputWriter {
    tx: mpsc::UnboundedSender<OutputEvent>,
}

impl ChannelOutputWriter {
    /// Create a new channel output writer.
    pub fn new(tx: mpsc::UnboundedSender<OutputEvent>) -> Self {
        Self { tx }
    }
}

impl OutputWriter for ChannelOutputWriter {
    fn emit(&self, event: OutputEvent) {
        let _ = self.tx.send(event);
    }

    fn flush(&self) {
        // Channel is unbuffered; flush is a no-op.
        // The render loop drains the channel on each frame tick.
    }
}

/// Type alias for convenience.
pub type OutputWriterArc = Arc<dyn OutputWriter>;
