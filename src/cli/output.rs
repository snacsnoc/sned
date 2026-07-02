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
    /// A line of text with optional styling (model output).
    Line(Line<'static>),
    /// Replace the most recent streamed model line in-place. Used for
    /// throttled partial-line updates so the TUI can show in-progress
    /// text without appending duplicate transcript lines.
    ModelUpdateLine(Line<'static>),
    /// A line of text with optional styling (tool result, plan status,
    /// heat map, etc.).  The TUI tags these as `ToolOutput` so they
    /// are never popped or re-rendered by `finalize_turn_stream`.
    ToolOutputLine(Line<'static>),
    /// Tool-call header line (e.g. "▶ execute_command").  Tagged
    /// separately from `Line` so render-time grouping can recognise
    /// the start of a tool block.
    ToolHeaderLine(Line<'static>),
    /// Command-execution header (e.g. "Running: <cmd>").
    CommandHeaderLine(Line<'static>),
    /// Command stdout / stderr / tail output line.
    CommandOutputLine(Line<'static>),
    /// Reasoning summary line ("Ɵ ...").
    ReasoningLine(Line<'static>),
    /// User-submitted prompt line ("❯ ..." or multi-line "│ ❯ ...").
    UserPromptLine(Line<'static>),
    /// Raw ANSI escape sequences (for PTY output, etc.).
    RawAnsi(String),
    /// Task completion message rendered as a dedicated Block widget.
    Completion(String),
    /// Error message rendered as a dedicated Block widget with red border.
    ErrorBox(String),
    /// End of a streamed agent turn. The TUI uses this to re-render
    /// the raw streamed lines recorded during the turn as formatted
    /// markdown. The payload is the original (pre-wrap, pre-indent)
    /// markdown text accumulated by the agent loop.
    ///
    /// In non-interactive output paths (e.g. one-shot/JSON), this is a
    /// no-op marker.
    TurnEnd { accumulated_text: String },
    /// A turn indicator line (e.g. "♦"). Emitted separately from
    /// streamed model text so that `finalize_turn_stream` does not
    /// strip it when re-rendering the turn as markdown.
    TurnIndicator(Line<'static>),
}

impl OutputEvent {
    pub fn plain(text: impl Into<String>) -> Self {
        Self::Line(Line::from(text.into()))
    }

    pub fn styled(text: impl Into<String>, style: ratatui::style::Style) -> Self {
        Self::Line(Line::from(Span::styled(text.into(), style)))
    }

    pub fn dim_yellow(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
            text.into(),
            Style::default()
                .fg(theme::WARNING_FG)
                .add_modifier(Modifier::DIM),
        )))
    }

    pub fn dim(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(text.into(), theme::dim_style())))
    }

    pub fn cyan(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::ACCENT),
        )))
    }

    pub fn magenta(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::TOOL_CALL_FG),
        )))
    }

    pub fn error_or_success(text: impl Into<String>, is_error: bool) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
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
        Self::Line(Line::from(Span::styled(text.into(), theme::bold_style())))
    }

    pub fn yellow(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::WARNING_FG),
        )))
    }

    pub fn error(text: impl fmt::Display) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
            format!("[sned] ERROR: {text}"),
            Style::default().fg(theme::ERROR_FG),
        )))
    }

    pub fn error_box(text: impl fmt::Display) -> Self {
        Self::ErrorBox(text.to_string())
    }

    pub fn warning(text: impl fmt::Display) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
            format!("[sned] Warning: {text}"),
            Style::default().fg(theme::WARNING_FG),
        )))
    }

    pub fn info(text: impl fmt::Display) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
            format!("[sned] {text}"),
            Style::default()
                .fg(theme::INFO_FG)
                .add_modifier(Modifier::DIM),
        )))
    }

    pub fn tool_call(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        Self::ToolHeaderLine(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::TOOL_CALL_FG),
        )))
    }

    pub fn model_output(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        Self::Line(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::ACCENT),
        )))
    }

    /// Emit a turn indicator line (e.g. "♦"). This is a separate event
    /// from `Line` so that `finalize_turn_stream` does not strip it
    /// when re-rendering the turn as markdown.
    pub fn turn_indicator(text: impl Into<String>) -> Self {
        use crate::cli::tui::theme;
        Self::TurnIndicator(Line::from(Span::styled(
            text.into(),
            Style::default().fg(theme::ACCENT),
        )))
    }

    /// Emit a tool-result line. The TUI will never pop or re-render
    /// these lines during `finalize_turn_stream`. Tool output uses
    /// DarkGray foreground for visual hierarchy against bright model
    /// text, except for error lines which stay bright red.
    pub fn tool_output_line(text: impl Into<String>, style: ratatui::style::Style) -> Self {
        let is_error = style.fg == Some(crate::cli::tui::theme::ERROR_FG);
        let final_style = if is_error {
            style
        } else if style.fg.is_some() {
            // Already has a foreground color — keep it, just add DIM.
            style.add_modifier(ratatui::style::Modifier::DIM)
        } else {
            // No foreground set — use DarkGray for subtle appearance.
            ratatui::style::Style::default().fg(crate::cli::tui::theme::STATUS_FG)
        };
        Self::ToolOutputLine(Line::from(Span::styled(text.into(), final_style)))
    }

    /// Emit a command-execution header line (e.g. "Running: <cmd>").
    /// Tagged separately so render-time grouping can keep the header
    /// visually anchored to its stdout/stderr block.
    pub fn command_header_line(text: impl Into<String>) -> Self {
        Self::CommandHeaderLine(Line::from(text.into()))
    }

    /// Emit a command stdout / stderr / tail line.  Tagged as a
    /// `CommandOutput` block so consecutive output lines are grouped
    /// without blank separators between them.
    pub fn command_output_line(text: impl Into<String>) -> Self {
        Self::CommandOutputLine(Line::from(text.into()))
    }

    /// Emit a reasoning-summary line (e.g. "Ɵ ...").  Tagged
    /// separately so render-time grouping can recognise the start of a
    /// reasoning block without confusing it with model prose.
    pub fn reasoning_line(text: impl Into<String>) -> Self {
        Self::ReasoningLine(Line::from(text.into()))
    }

    /// Emit a user-prompt line (e.g. "❯ ..." or "│ ❯ ...").  Routed
    /// to the TUI buffer with `BlockKind::UserPrompt` so render-time
    /// grouping gives it a visual boundary above.
    pub fn user_prompt_line(text: impl Into<String>) -> Self {
        Self::UserPromptLine(Line::from(text.into()))
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
            OutputEvent::Line(line)
            | OutputEvent::ModelUpdateLine(line)
            | OutputEvent::ToolOutputLine(line)
            | OutputEvent::ToolHeaderLine(line)
            | OutputEvent::CommandHeaderLine(line)
            | OutputEvent::CommandOutputLine(line)
            | OutputEvent::ReasoningLine(line)
            | OutputEvent::UserPromptLine(line) => {
                eprintln!("{line}");
            }
            OutputEvent::RawAnsi(s) => {
                eprint!("{s}");
            }
            OutputEvent::Completion(result) => {
                eprintln!("\n[sned] Task Completed: {result}");
            }
            OutputEvent::ErrorBox(msg) => {
                if !msg.trim().is_empty() {
                    let width = crate::cli::text_utils::get_terminal_width();
                    let box_str = crate::cli::text_utils::draw_error_box("✗ Error", &msg, width);
                    if crate::cli::colors::stderr_colors_disabled() {
                        eprint!("{box_str}");
                    } else {
                        for line in box_str.lines() {
                            eprintln!(
                                "{}{}{}",
                                crate::cli::colors::style::RED,
                                line,
                                crate::cli::colors::style::RESET
                            );
                        }
                    }
                }
            }
            OutputEvent::TurnEnd { .. } | OutputEvent::TurnIndicator(_) => {}
        }
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Output writer that sends events through an mpsc channel.
///
/// The channel is bounded (default 16384 entries; override with
/// `SNED_OUTPUT_CHANNEL_CAPACITY` in `run_interactive_shell_inner`). When
/// the buffer is full, events are dropped silently and a counter is
/// incremented; the TUI's main loop reads this counter via
/// `dropped_count()` and surfaces a user-visible "⚠ output overflow (N
/// dropped)" warning in the status bar. The render loop drains the channel
/// on each frame tick.
pub struct ChannelOutputWriter {
    tx: mpsc::Sender<OutputEvent>,
    priority_tx: mpsc::UnboundedSender<OutputEvent>,
    priority_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<OutputEvent>>>,
    dropped_count: std::sync::atomic::AtomicU64,
    overflow_signaled: std::sync::atomic::AtomicBool,
}

impl ChannelOutputWriter {
    /// Create a new ChannelOutputWriter with a bounded channel.
    #[must_use]
    pub fn new(tx: mpsc::Sender<OutputEvent>) -> Self {
        let (priority_tx, priority_rx) = mpsc::unbounded_channel();
        Self {
            tx,
            priority_tx,
            priority_rx: std::sync::Mutex::new(Some(priority_rx)),
            dropped_count: std::sync::atomic::AtomicU64::new(0),
            overflow_signaled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn take_priority_rx(&self) -> Option<mpsc::UnboundedReceiver<OutputEvent>> {
        self.priority_rx
            .lock()
            .expect("priority rx mutex poisoned")
            .take()
    }
}

impl OutputWriter for ChannelOutputWriter {
    fn emit(&self, event: OutputEvent) {
        let is_lossy_update = matches!(event, OutputEvent::ModelUpdateLine(_));
        if let Err(err) = self.tx.try_send(event) {
            if is_lossy_update {
                return;
            }

            if self.priority_tx.send(err.into_inner()).is_ok() {
                return;
            }

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
#[must_use]
pub fn timing_enabled() -> bool {
    matches!(
        std::env::var("SNED_TIMING").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Format phase timing diagnostics into printable lines.
#[must_use]
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

    /// Critical non-lossy events must survive a saturated main output
    /// queue by spilling into the reliable priority lane instead of
    /// being dropped.
    #[test]
    fn test_channel_overflow_preserves_approval_prompt_via_priority_lane() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, mut rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);
        let mut priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");

        writer.emit(OutputEvent::plain("line 1"));
        writer.emit(OutputEvent::plain("line 2"));
        writer.emit(OutputEvent::plain("line 3"));

        let approval_prompt = OutputEvent::RawAnsi(
            "\n\x1b[33m🔧 Tool:\x1b[0m \x1b[1mexecute_command\x1b[0m\n\
             Execute this tool? (y/n/always): "
                .to_string(),
        );
        writer.emit(approval_prompt.clone());

        let primary_events: Vec<OutputEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let priority_events: Vec<OutputEvent> =
            std::iter::from_fn(|| priority_rx.try_recv().ok()).collect();

        assert!(
            primary_events
                .iter()
                .chain(priority_events.iter())
                .any(|event| matches!(event, OutputEvent::RawAnsi(text) if text == match &approval_prompt {
                    OutputEvent::RawAnsi(text) => text,
                    _ => unreachable!(),
                })),
            "approval prompt must survive overflow via either output lane"
        );
        assert!(
            !writer.take_overflow_signal(),
            "priority fallback should preserve the prompt without signaling a drop"
        );
        assert_eq!(
            writer.dropped_count(),
            0,
            "priority fallback should avoid durable drops"
        );
    }

    #[test]
    fn test_channel_overflow_ignores_lossy_model_updates() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};
        use ratatui::text::Line;

        let (tx, _rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);

        writer.emit(OutputEvent::plain("line 1"));
        writer.emit(OutputEvent::ModelUpdateLine(Line::from("partial")));

        assert!(
            !writer.take_overflow_signal(),
            "lossy model updates should not trigger the global overflow warning"
        );
        assert_eq!(
            writer.dropped_count(),
            0,
            "lossy model updates should not count as dropped durable output"
        );
    }

    #[test]
    fn test_channel_overflow_still_signals_when_all_receivers_are_gone() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);
        let priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");

        drop(rx);
        drop(priority_rx);

        writer.emit(OutputEvent::plain("dropped"));

        assert!(
            writer.take_overflow_signal(),
            "closed receivers should still surface durable-output loss"
        );
        assert_eq!(
            writer.dropped_count(),
            1,
            "closed receivers should count as one dropped durable event"
        );
    }
}
