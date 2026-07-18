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
    /// Raw streamed reasoning text, styled and assembled by the renderer.
    ReasoningChunk(String),
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
    /// A dedicated event prevents approval visibility from depending on
    /// transcript ordering or `RawAnsi` classification timing.
    ApprovalRequested(crate::core::approval::ApprovalRequest),
    /// Prompt identity prevents an old timeout from clearing a newer panel.
    ApprovalFinished { id: u64 },
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

    pub fn reasoning_chunk(text: impl Into<String>) -> Self {
        Self::ReasoningChunk(text.into())
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

    /// Human-readable summary of per-category drop counts.
    /// Default: "none" (no drops).
    fn drop_summary(&self) -> String {
        "none".to_string()
    }
}

/// Output writer that forwards to stderr.
///
/// Used during Phase 0-1 to keep old code working while new code
/// also writes to the channel.
pub struct StderrOutputWriter;

fn non_interactive_approval_message(title: &str) -> String {
    format!(
        "✗ {title}: non-interactive mode cannot accept approval. Re-run interactively or with --yolo to allow this action."
    )
}

impl OutputWriter for StderrOutputWriter {
    fn emit(&self, event: OutputEvent) {
        match event {
            OutputEvent::Line(line)
            | OutputEvent::ModelUpdateLine(line)
            | OutputEvent::ToolOutputLine(line)
            | OutputEvent::ToolHeaderLine(line)
            | OutputEvent::CommandHeaderLine(line)
            | OutputEvent::CommandOutputLine(line)
            | OutputEvent::UserPromptLine(line) => {
                eprintln!("{line}");
            }
            OutputEvent::ReasoningChunk(chunk) => {
                for segment in chunk.split_inclusive('\n') {
                    eprint!("  Ɵ {segment}");
                }
                if !chunk.ends_with('\n') {
                    eprintln!();
                }
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
            OutputEvent::ApprovalRequested(request) => {
                eprintln!("{}", non_interactive_approval_message(request.title()));
                let _ = std::io::stderr().flush();
                request.fail("interactive approval UI is unavailable");
            }
            OutputEvent::TurnEnd { .. }
            | OutputEvent::TurnIndicator(_)
            | OutputEvent::ApprovalFinished { .. } => {}
        }
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Per-category drop counters for overflow diagnostics.
/// Tracking by category lets the TUI surface what was lost
/// (model text, tool results, approval prompts, etc.).
#[derive(Default)]
struct DropCounters {
    model_text: std::sync::atomic::AtomicU64,
    tool_output: std::sync::atomic::AtomicU64,
    approval_prompt: std::sync::atomic::AtomicU64,
    other: std::sync::atomic::AtomicU64,
}

impl DropCounters {
    fn total(&self) -> u64 {
        self.model_text.load(std::sync::atomic::Ordering::Relaxed)
            + self.tool_output.load(std::sync::atomic::Ordering::Relaxed)
            + self
                .approval_prompt
                .load(std::sync::atomic::Ordering::Relaxed)
            + self.other.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn format_summary(&self) -> String {
        let m = self.model_text.load(std::sync::atomic::Ordering::Relaxed);
        let t = self.tool_output.load(std::sync::atomic::Ordering::Relaxed);
        let a = self
            .approval_prompt
            .load(std::sync::atomic::Ordering::Relaxed);
        let o = self.other.load(std::sync::atomic::Ordering::Relaxed);
        let mut parts = Vec::new();
        if m > 0 {
            parts.push(format!("{} model", m));
        }
        if t > 0 {
            parts.push(format!("{} tools", t));
        }
        if a > 0 {
            parts.push(format!("{} approvals", a));
        }
        if o > 0 {
            parts.push(format!("{} other", o));
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Output writer that sends events through an mpsc channel.
///
/// The channel is bounded (default 262144 entries; override with
/// `SNED_OUTPUT_CHANNEL_CAPACITY` in `run_interactive_shell_inner`). When
/// the buffer is full, events spill to the priority lane. If the priority
/// lane is also unreachable (receiver dropped), events are durably dropped
/// and per-category counters are incremented.
pub struct ChannelOutputWriter {
    tx: mpsc::Sender<OutputEvent>,
    priority_tx: mpsc::UnboundedSender<OutputEvent>,
    priority_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<OutputEvent>>>,
    priority_attached: std::sync::atomic::AtomicBool,
    drop_counters: DropCounters,
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
            priority_attached: std::sync::atomic::AtomicBool::new(false),
            drop_counters: DropCounters::default(),
            overflow_signaled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn take_priority_rx(&self) -> Option<mpsc::UnboundedReceiver<OutputEvent>> {
        let receiver = self
            .priority_rx
            .lock()
            .expect("priority rx mutex poisoned")
            .take();
        if receiver.is_some() {
            self.priority_attached
                .store(true, std::sync::atomic::Ordering::Release);
        }
        receiver
    }

    fn fail_control_delivery(&self, event: OutputEvent, reason: &'static str) {
        if let OutputEvent::ApprovalRequested(request) = event {
            self.drop_counters
                .approval_prompt
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.overflow_signaled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            request.fail(reason);
        }
    }

    fn record_dropped_event(&self, event: &OutputEvent) {
        let counters = &self.drop_counters;
        match event {
            OutputEvent::Line(_) => {
                counters
                    .model_text
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            OutputEvent::ToolOutputLine(_)
            | OutputEvent::ToolHeaderLine(_)
            | OutputEvent::CommandHeaderLine(_)
            | OutputEvent::CommandOutputLine(_) => {
                counters
                    .tool_output
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {
                counters
                    .other
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.overflow_signaled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let total = counters.total();
        if total > 0 && total.is_multiple_of(100) {
            tracing::warn!(
                dropped = total,
                summary = counters.format_summary(),
                "Output channel full; TUI render loop is falling behind. \
                 {} events dropped so far ({}).",
                total,
                counters.format_summary()
            );
        }
    }
}

impl OutputWriter for ChannelOutputWriter {
    fn emit(&self, event: OutputEvent) {
        if matches!(
            &event,
            OutputEvent::ApprovalRequested(_) | OutputEvent::ApprovalFinished { .. }
        ) {
            // Approval bypasses transcript backlog so the panel is rendered
            // before the user can accidentally act on an unseen request.
            if !self
                .priority_attached
                .load(std::sync::atomic::Ordering::Acquire)
            {
                self.fail_control_delivery(event, "interactive approval receiver is not attached");
                return;
            }
            if let Err(err) = self.priority_tx.send(event) {
                self.fail_control_delivery(err.0, "interactive approval receiver is closed");
            }
            return;
        }

        let is_lossy_update = matches!(event, OutputEvent::ModelUpdateLine(_));
        if let Err(err) = self.tx.try_send(event) {
            if is_lossy_update {
                return;
            }

            // Main channel is full. Classify the event to determine
            // whether it must be preserved (critical) or can be safely
            // dropped (non-critical).
            let dropped = err.into_inner();
            let is_critical = matches!(
                &dropped,
                OutputEvent::TurnEnd { .. }
                    | OutputEvent::Completion(_)
                    | OutputEvent::ErrorBox(_)
                    | OutputEvent::ReasoningChunk(_)
            );

            if is_critical {
                // Critical events survive channel saturation: send them
                // into the unbounded priority lane so the TUI always sees
                // them even when the main queue is backed up.
                if let Err(err) = self.priority_tx.send(dropped) {
                    self.record_dropped_event(&err.0);
                }
                return;
            }

            self.record_dropped_event(&dropped);
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
        self.drop_counters.total()
    }

    fn drop_summary(&self) -> String {
        self.drop_counters.format_summary()
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
    use super::{format_timing_phases, non_interactive_approval_message};
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

    #[test]
    fn test_non_interactive_approval_message_omits_prompt_details() {
        let message = non_interactive_approval_message("Approval required · web_fetch");

        assert_eq!(
            message,
            "✗ Approval required · web_fetch: non-interactive mode cannot accept approval. Re-run interactively or with --yolo to allow this action."
        );
        assert!(!message.contains("Execute this tool?"));
    }

    /// Critical non-lossy events must survive a saturated main output
    /// queue by spilling into the reliable priority lane instead of
    /// being dropped.
    #[test]
    fn test_channel_overflow_preserves_approval_prompt_via_priority_lane() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);
        let mut priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");

        writer.emit(OutputEvent::plain("line 1"));
        let (request, response_rx) = crate::core::approval::approval_request_for_test(
            50,
            "Approval required · execute_command",
            "🔧 Tool: execute_command\nExecute this tool?",
        );
        writer.emit(OutputEvent::ApprovalRequested(request));

        let event = priority_rx
            .try_recv()
            .expect("approval should bypass the saturated transcript queue");
        let OutputEvent::ApprovalRequested(request) = event else {
            panic!("expected approval request in control lane");
        };
        assert!(request.details().contains("Execute this tool?"));
        assert_eq!(request.choices().len(), 3);
        assert!(request.respond(crate::core::approval::ApprovalResult::Denied));
        assert!(matches!(
            response_rx.try_recv(),
            Ok(crate::core::approval::ApprovalResponse::Decision(
                crate::core::approval::ApprovalResult::Denied
            ))
        ));
        assert_eq!(writer.dropped_count(), 0);
        assert!(!writer.take_overflow_signal());
    }

    #[test]
    fn test_channel_overflow_preserves_reasoning_chunks_via_priority_lane() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);
        let mut priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");

        writer.emit(OutputEvent::plain("line 1"));
        writer.emit(OutputEvent::reasoning_chunk("first\n"));
        writer.emit(OutputEvent::reasoning_chunk("second"));

        let mut reasoning = Vec::new();
        while let Ok(event) = priority_rx.try_recv() {
            match event {
                OutputEvent::ReasoningChunk(chunk) => reasoning.push(chunk),
                other => panic!("expected reasoning chunk, got {other:?}"),
            }
        }
        assert_eq!(reasoning, ["first\n", "second"]);
        assert_eq!(writer.dropped_count(), 0);
        assert!(!writer.take_overflow_signal());
    }

    #[test]
    fn test_approval_request_fails_closed_without_attached_ui() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);
        let (request, response_rx) = crate::core::approval::approval_request_for_test(
            51,
            "Approval required · edit_file",
            "Approve edit?",
        );

        writer.emit(OutputEvent::ApprovalRequested(request));

        assert!(matches!(
            response_rx.try_recv(),
            Ok(crate::core::approval::ApprovalResponse::Unavailable(reason))
                if reason.contains("not attached")
        ));
        assert_eq!(writer.dropped_count(), 1);
        assert!(writer.take_overflow_signal());
    }

    #[test]
    fn test_approval_request_fails_closed_after_ui_disconnects() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);
        let priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");
        drop(priority_rx);
        let (request, response_rx) = crate::core::approval::approval_request_for_test(
            52,
            "Approval required · execute_command",
            "Execute command?",
        );

        writer.emit(OutputEvent::ApprovalRequested(request));

        assert!(matches!(
            response_rx.try_recv(),
            Ok(crate::core::approval::ApprovalResponse::Unavailable(reason))
                if reason.contains("closed")
        ));
        assert_eq!(writer.dropped_count(), 1);
        assert!(writer.take_overflow_signal());
    }

    #[test]
    fn test_channel_overflow_counts_critical_events_after_priority_disconnect() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel::<OutputEvent>(1);
        let writer = ChannelOutputWriter::new(tx);
        let priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");

        writer.emit(OutputEvent::plain("line 1"));
        drop(priority_rx);
        writer.emit(OutputEvent::TurnEnd {
            accumulated_text: "done".to_string(),
        });
        writer.emit(OutputEvent::Completion("done".to_string()));
        writer.emit(OutputEvent::ErrorBox("failed".to_string()));
        writer.emit(OutputEvent::ReasoningChunk("thinking".to_string()));

        assert_eq!(writer.dropped_count(), 4);
        assert_eq!(writer.drop_summary(), "4 other");
        assert!(writer.take_overflow_signal());
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
