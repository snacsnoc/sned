//! Output abstraction for agent output routing.
//!
//! This module provides the `OutputEvent` enum and `OutputWriter` trait that
//! decouple agent output from the terminal. In interactive mode, output flows
//! through an `mpsc` channel to the ratatui render loop. In one-shot/piped
//! mode, output goes directly to stderr.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::fmt;
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// An output event that can be rendered by the TUI or forwarded to stderr.
#[derive(Clone, Debug)]
pub enum OutputEvent {
    /// A line of text with optional styling.
    Line(Line<'static>),
    /// Raw ANSI escape sequences (for PTY output, etc.).
    RawAnsi(String),
    /// Task completion message rendered as a dedicated Block widget.
    Completion(String),
}

impl OutputEvent {
    pub fn plain(text: impl Into<String>) -> Self {
        OutputEvent::Line(Line::from(text.into()))
    }

    pub fn styled(text: impl Into<String>, style: ratatui::style::Style) -> Self {
        OutputEvent::Line(Line::from(Span::styled(text.into(), style)))
    }

    pub fn dim_yellow(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default()
                .fg(theme::WARNING_FG)
                .add_modifier(Modifier::DIM),
        )))
    }

    pub fn dim(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(text.into(), theme::dim_style())))
    }

    pub fn cyan(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::ACCENT),
        )))
    }

    pub fn magenta(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::TOOL_CALL_FG),
        )))
    }

    pub fn error_or_success(text: impl Into<String>, is_error: bool) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(if is_error {
                theme::ERROR_FG
            } else {
                theme::PROMPT_FG
            }),
        )))
    }

    pub fn bold(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(text.into(), theme::bold_style())))
    }

    pub fn yellow(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::WARNING_FG),
        )))
    }

    pub fn error(text: impl fmt::Display) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            format!("[sned] ERROR: {}", text),
            Style::default().fg(theme::ERROR_FG),
        )))
    }

    pub fn warning(text: impl fmt::Display) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            format!("[sned] Warning: {}", text),
            Style::default().fg(theme::WARNING_FG),
        )))
    }

    pub fn info(text: impl fmt::Display) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            format!("[sned] {}", text),
            Style::default()
                .fg(theme::INFO_FG)
                .add_modifier(Modifier::DIM),
        )))
    }

    pub fn tool_call(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::TOOL_CALL_FG),
        )))
    }

    pub fn model_output(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        OutputEvent::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::ACCENT),
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

    /// Returns true once if any event was dropped due to overflow since
    /// the last call, then resets the signal. Default: never overflows.
    /// Used by the TUI main loop to surface a user-visible warning when
    /// the render loop falls behind and events (including approval
    /// prompts) are lost.
    fn take_overflow_signal(&self) -> bool {
        false
    }

    /// Total number of events dropped due to overflow. Default: zero.
    fn dropped_count(&self) -> u64 {
        0
    }
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
            OutputEvent::Completion(result) => {
                eprintln!("\n[sned] Task Completed: {}", result);
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
    tx: mpsc::Sender<OutputEvent>,
    dropped_count: std::sync::atomic::AtomicU64,
    overflow_signaled: std::sync::atomic::AtomicBool,
}

impl ChannelOutputWriter {
    /// Create a new ChannelOutputWriter with a bounded channel.
    pub fn new(tx: mpsc::Sender<OutputEvent>) -> Self {
        Self {
            tx,
            dropped_count: std::sync::atomic::AtomicU64::new(0),
            overflow_signaled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Peek at the overflow signal without consuming it.
    pub fn has_overflow(&self) -> bool {
        self.overflow_signaled
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl OutputWriter for ChannelOutputWriter {
    fn emit(&self, event: OutputEvent) {
        if self.tx.try_send(event).is_err() {
            let count = self
                .dropped_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Set the overflow signal on every drop so the TUI can
            // surface a user-visible indicator. This is critical for
            // approval prompts: if the prompt emit is dropped, the user
            // will not see it and must blindly hit "y".
            self.overflow_signaled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if count.is_multiple_of(100) {
                tracing::warn!(
                    dropped = count + 1,
                    "Output channel full; TUI render loop is falling behind. \
                     {} events dropped so far.",
                    count + 1
                );
            }
        }
    }

    fn flush(&self) {
        // Channel is unbuffered; flush is a no-op.
        // The render loop drains the channel on each frame tick.
    }

    fn take_overflow_signal(&self) -> bool {
        self.overflow_signaled
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    fn dropped_count(&self) -> u64 {
        self.dropped_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Type alias for convenience.
pub type OutputWriterArc = Arc<dyn OutputWriter>;

/// Returns whether diagnostic timing output is enabled.
///
/// Set `SNED_TIMING=1` to enable phase timing logs.
pub fn timing_enabled() -> bool {
    matches!(
        std::env::var("SNED_TIMING").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

/// Format phase timing diagnostics into printable lines.
pub fn format_timing_phases(
    session_start: Instant,
    request_sent: Option<Instant>,
    first_provider_chunk: Option<Instant>,
    first_reasoning_chunk: Option<Instant>,
    first_displayable_text: Option<Instant>,
    first_output_emit: Option<Instant>,
    first_render: Option<Instant>,
) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(first_output_emit) = first_output_emit {
        lines.push(format!(
            "[timing] first_token_us={}",
            first_output_emit.duration_since(session_start).as_micros()
        ));
    }

    if let Some(request_sent) = request_sent {
        lines.push(format!(
            "[timing] session_to_request_us={}",
            request_sent.duration_since(session_start).as_micros()
        ));

        if let Some(first_provider_chunk) = first_provider_chunk {
            lines.push(format!(
                "[timing] request_to_first_chunk_us={}",
                first_provider_chunk
                    .duration_since(request_sent)
                    .as_micros()
            ));

            if let Some(first_reasoning_chunk) = first_reasoning_chunk {
                lines.push(format!(
                    "[timing] first_chunk_to_first_reasoning_chunk_us={}",
                    first_reasoning_chunk
                        .duration_since(first_provider_chunk)
                        .as_micros()
                ));
            }

            if let Some(first_displayable_text) = first_displayable_text {
                lines.push(format!(
                    "[timing] first_chunk_to_first_displayable_text_us={}",
                    first_displayable_text
                        .duration_since(first_provider_chunk)
                        .as_micros()
                ));

                if let Some(first_output_emit) = first_output_emit {
                    lines.push(format!(
                        "[timing] first_displayable_text_to_first_output_us={}",
                        first_output_emit
                            .duration_since(first_displayable_text)
                            .as_micros()
                    ));
                }
            }

            if let Some(first_output_emit) = first_output_emit {
                lines.push(format!(
                    "[timing] first_chunk_to_first_output_us={}",
                    first_output_emit
                        .duration_since(first_provider_chunk)
                        .as_micros()
                ));

                if let Some(first_render) = first_render {
                    lines.push(format!(
                        "[timing] first_output_to_first_render_us={}",
                        first_render.duration_since(first_output_emit).as_micros()
                    ));
                }
            }
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::format_timing_phases;
    use std::time::{Duration, Instant};

    #[test]
    fn test_format_timing_phases_includes_all_known_phases() {
        let start = Instant::now();
        let request = start + Duration::from_millis(100);
        let chunk = request + Duration::from_millis(250);
        let reasoning = chunk + Duration::from_millis(12);
        let displayable = chunk + Duration::from_millis(25);
        let output = displayable + Duration::from_millis(25);
        let render = output + Duration::from_millis(16);

        let lines = format_timing_phases(
            start,
            Some(request),
            Some(chunk),
            Some(reasoning),
            Some(displayable),
            Some(output),
            Some(render),
        );

        assert_eq!(lines[0], "[timing] first_token_us=400000");
        assert_eq!(lines[1], "[timing] session_to_request_us=100000");
        assert_eq!(lines[2], "[timing] request_to_first_chunk_us=250000");
        assert_eq!(
            lines[3],
            "[timing] first_chunk_to_first_reasoning_chunk_us=12000"
        );
        assert_eq!(
            lines[4],
            "[timing] first_chunk_to_first_displayable_text_us=25000"
        );
        assert_eq!(
            lines[5],
            "[timing] first_displayable_text_to_first_output_us=25000"
        );
        assert_eq!(lines[6], "[timing] first_chunk_to_first_output_us=50000");
        assert_eq!(lines[7], "[timing] first_output_to_first_render_us=16000");
    }

    /// Regression test for the silent channel-overflow bug. The user
    /// reported that the approval prompt was not visible in the TUI
    /// viewport; the root cause was `try_send` silently dropping events
    /// when the 8192-capacity mpsc channel flooded during tool-result
    /// bursts. This test fills the channel past capacity, drops an
    /// approval-prompt RawAnsi event, and asserts that:
    /// 1. The dropped event does NOT land in the receiver.
    /// 2. The overflow signal is set.
    /// 3. `dropped_count()` reflects the lost event.
    /// The TUI main loop uses these signals to surface a user-visible
    /// warning in the status bar (src/cli/tui/app.rs output_overflow
    /// field) so the user knows output (including approval prompts) may
    /// be missing.
    #[test]
    fn test_channel_overflow_signals_dropped_approval_prompt() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        // Tiny channel capacity (1) so we can force overflow without
        // emitting thousands of events. The approval prompt is the
        // event that MUST be dropped to reproduce the user's bug.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);

        // Fill the channel: 1 event slots into the buffer, the rest
        // queue in the sender's pending queue up to capacity.
        writer.emit(OutputEvent::plain("line 1"));
        writer.emit(OutputEvent::plain("line 2"));
        writer.emit(OutputEvent::plain("line 3"));

        // The approval prompt — the critical event — must be dropped
        // when the channel is full and the receiver is not draining.
        let approval_prompt = OutputEvent::RawAnsi(
            "\n\x1b[33m🔧 Tool:\x1b[0m \x1b[1mexecute_command\x1b[0m\n\
             Execute this tool? (y/n/always): "
                .to_string(),
        );
        writer.emit(approval_prompt);

        // Drain whatever made it through (line 1, possibly line 2/3 if
        // the sender's queue absorbed them, but NOT the prompt).
        while rx.try_recv().is_ok() {}

        // The approval prompt must not have been delivered.
        // (We can't directly assert "not received" because the sender's
        // bounded queue may have absorbed the prior events; the
        // critical assertion is that the overflow signal fired and
        // dropped_count > 0.)

        // Overflow signal must be set so the TUI can surface it.
        assert!(
            writer.take_overflow_signal(),
            "take_overflow_signal must return true when events were dropped"
        );

        // dropped_count must reflect the lost event.
        assert!(
            writer.dropped_count() > 0,
            "dropped_count must be > 0 after channel overflow, got: {}",
            writer.dropped_count()
        );

        // Second call must return false (signal is edge-triggered).
        assert!(
            !writer.take_overflow_signal(),
            "take_overflow_signal must be edge-triggered (false after consume)"
        );
    }
}
