//! Output abstraction for agent output routing.
//!
//! This module provides the `OutputEvent` enum and `OutputWriter` trait that
//! decouple agent output from the terminal. In interactive mode, output flows
//! through an `mpsc` channel to the ratatui render loop. In one-shot/piped
//! mode, output goes directly to stderr.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::fmt;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
pub struct TurnEndTiming {
    pub(crate) provider_completed_at: Option<Instant>,
    pub(crate) first_output_at: Option<Instant>,
    pub(crate) emitted_at: Instant,
}

/// Bounded latency histogram used by timing reports. Buckets are in
/// microseconds so reports can describe frame-cost tails without retaining a
/// sample for every redraw.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct TimingHistogram {
    #[serde(rename = "lt_1ms")]
    lt_1ms: u64,
    #[serde(rename = "1_2ms")]
    one_to_2ms: u64,
    #[serde(rename = "2_4ms")]
    two_to_4ms: u64,
    #[serde(rename = "4_8ms")]
    four_to_8ms: u64,
    #[serde(rename = "8_16ms")]
    eight_to_16ms: u64,
    #[serde(rename = "16_32ms")]
    sixteen_to_32ms: u64,
    #[serde(rename = "32_64ms")]
    thirty_two_to_64ms: u64,
    #[serde(rename = "gte_64ms")]
    gte_64ms: u64,
}

impl TimingHistogram {
    pub(crate) fn record(&mut self, elapsed_us: u64) {
        match elapsed_us {
            0..1_000 => self.lt_1ms += 1,
            1_000..2_000 => self.one_to_2ms += 1,
            2_000..4_000 => self.two_to_4ms += 1,
            4_000..8_000 => self.four_to_8ms += 1,
            8_000..16_000 => self.eight_to_16ms += 1,
            16_000..32_000 => self.sixteen_to_32ms += 1,
            32_000..64_000 => self.thirty_two_to_64ms += 1,
            _ => self.gte_64ms += 1,
        }
    }
}

static TUI_TIMING_SINK_ACTIVE: AtomicBool = AtomicBool::new(false);

fn timing_stderr_allowed() -> bool {
    if TUI_TIMING_SINK_ACTIVE.load(Ordering::Acquire) {
        return false;
    }

    !std::io::stderr().is_terminal()
        || matches!(
            std::env::var("SNED_TIMING_STDERR").ok().as_deref(),
            Some("1") | Some("true") | Some("yes")
        )
}

fn timing_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SNED_TIMING_FILE")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path));
    }

    TUI_TIMING_SINK_ACTIVE
        .load(Ordering::Acquire)
        .then(|| crate::storage::disk::get_data_dir().join("logs/sned-timing.jsonl"))
}

fn report_timing_write_error(error: &std::io::Error, kind: &str) {
    if TUI_TIMING_SINK_ACTIVE.load(Ordering::Acquire) {
        // The alternate screen owns stderr while the TUI is active. Do not
        // route diagnostics there when the auxiliary timing sink fails.
        let _ = (error, kind);
    } else {
        tracing::warn!(%error, kind, "failed to write timing output");
    }
}

pub(crate) struct TuiTimingSinkGuard;

impl Drop for TuiTimingSinkGuard {
    fn drop(&mut self) {
        TUI_TIMING_SINK_ACTIVE.store(false, Ordering::Release);
    }
}

pub(crate) fn enter_tui_timing_sink() -> TuiTimingSinkGuard {
    TUI_TIMING_SINK_ACTIVE.store(true, Ordering::Release);
    TuiTimingSinkGuard
}

fn append_timing_line(path: &str, line: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

pub(crate) fn emit_timing_text(line: &str) {
    if !timing_enabled() {
        return;
    }
    if let Some(path) = timing_file_path() {
        if let Err(error) = append_timing_line(&path.to_string_lossy(), line) {
            report_timing_write_error(&error, "text");
        }
    } else if timing_stderr_allowed() {
        eprintln!("{line}");
    }
}

/// Emits a machine-readable timing record without model content, prompts, or
/// credentials.
pub(crate) fn emit_timing_record(record: &impl serde::Serialize) {
    if !timing_enabled() {
        return;
    }
    match serde_json::to_string(record) {
        Ok(record) => {
            if let Some(path) = timing_file_path() {
                if let Err(error) = append_timing_line(&path.to_string_lossy(), &record) {
                    report_timing_write_error(&error, "record");
                }
            } else if timing_stderr_allowed() {
                eprintln!("[timing-json] {record}");
            }
        }
        Err(error) => tracing::warn!(%error, "failed to serialize timing record"),
    }
}

/// A benchmark runner supplies this so independent Sned processes can be
/// joined without relying on prompts, task ids, or terminal output ordering.
pub(crate) fn timing_run_id() -> Option<String> {
    std::env::var("SNED_TIMING_RUN_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct ProviderTimingRecord {
    #[serde(rename = "type")]
    pub(crate) record_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<String>,
    pub(crate) session_id: String,
    pub(crate) turn: u32,
    pub(crate) attempt: usize,
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
    pub(crate) stream: bool,
    pub(crate) request_to_headers_us: u64,
    pub(crate) headers_to_first_byte_us: Option<u64>,
    pub(crate) request_to_first_chunk_us: Option<u64>,
    pub(crate) first_chunk_to_displayable_text_us: Option<u64>,
    pub(crate) displayable_text_to_output_us: Option<u64>,
    pub(crate) stream_total_us: u64,
    pub(crate) raw_sse_frames: u64,
    pub(crate) decoded_chunks: u64,
    pub(crate) text_chunks: u64,
    pub(crate) reasoning_chunks: u64,
    pub(crate) empty_sse_frames: u64,
    pub(crate) max_inter_raw_byte_gap_us: u64,
    pub(crate) max_inter_decoded_chunk_gap_us: u64,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct TuiTimingRecord {
    #[serde(rename = "type")]
    pub(crate) record_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<String>,
    pub(crate) session_id: String,
    pub(crate) turn: u32,
    pub(crate) first_output_to_render_us: Option<u64>,
    pub(crate) provider_stream_complete_to_turn_end_emit_us: Option<u64>,
    pub(crate) turn_end_emit_to_dequeue_us: Option<u64>,
    pub(crate) turn_end_dequeue_to_render_start_us: u64,
    pub(crate) events_drained_before_turn_end: u64,
    pub(crate) transcript_lines_at_turn_end: usize,
    pub(crate) turn_render_worker_us: u64,
    pub(crate) turn_render_syntax_highlight_us: u64,
    pub(crate) turn_render_apply_us: u64,
    pub(crate) layout_rebuild_us: u64,
    pub(crate) layout_rebuild_count: u64,
    pub(crate) drain_peak_us: u64,
    pub(crate) draw_peak_us: u64,
    pub(crate) drain_histogram: TimingHistogram,
    pub(crate) draw_histogram: TimingHistogram,
    /// True when this turn completed in a frame shared with a newer turn.
    /// Its render work was coalesced, so frame metrics belong to that newer
    /// generation rather than being duplicated here.
    pub(crate) frame_coalesced: bool,
    pub(crate) main_queue_peak: usize,
    pub(crate) main_queue_backlog_peak: usize,
    pub(crate) main_queue_backlog_cycles: u64,
    pub(crate) priority_queue_peak: usize,
    pub(crate) approval_queue_peak: usize,
    pub(crate) dropped_events: u64,
}

/// A bounded progress sample distinguishes a large fixed backlog from drain
/// throughput that degrades as the visible transcript grows.
#[derive(Debug, serde::Serialize)]
pub(crate) struct TuiDrainProgressRecord {
    #[serde(rename = "type")]
    pub(crate) record_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<String>,
    pub(crate) session_id: String,
    pub(crate) turn: u32,
    pub(crate) events_drained: u64,
    pub(crate) main_queue_depth: usize,
    pub(crate) priority_queue_depth: usize,
    pub(crate) transcript_lines: usize,
}

#[derive(Clone, Default)]
pub(crate) struct ReasoningMailbox {
    pending: Arc<std::sync::Mutex<Option<(String, u64)>>>,
    received_chunks: Arc<std::sync::atomic::AtomicU64>,
}

const MAX_REASONING_SNAPSHOT_BYTES: usize = 64 * 1024;

impl ReasoningMailbox {
    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Option<(String, u64)>> {
        match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                *guard = None;
                self.pending.clear_poison();
                guard
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn append(&self, chunk: String) {
        self.append_with_sequence(chunk, 0);
    }

    fn append_with_sequence(&self, chunk: String, sequence: u64) {
        self.received_chunks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if chunk.is_empty() {
            return;
        }

        let mut chunk = chunk;
        Self::retain_tail(&mut chunk);
        let mut pending = self.lock_pending();
        match pending.as_mut() {
            Some((existing, seq)) => {
                existing.push_str(&chunk);
                Self::retain_tail(existing);
                if sequence != 0 {
                    *seq = sequence;
                }
            }
            None => *pending = Some((chunk, sequence)),
        }
    }

    fn retain_tail(value: &mut String) {
        if value.len() <= MAX_REASONING_SNAPSHOT_BYTES {
            return;
        }
        let minimum_start = value.len() - MAX_REASONING_SNAPSHOT_BYTES;
        let start = value
            .char_indices()
            .find_map(|(index, _)| (index >= minimum_start).then_some(index))
            .unwrap_or(value.len());
        value.drain(..start);
    }

    #[cfg(test)]
    pub(crate) fn take(&self) -> Option<String> {
        self.take_with_sequence().map(|(text, _)| text)
    }

    pub(crate) fn take_with_sequence(&self) -> Option<(String, u64)> {
        self.lock_pending().take()
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.lock_pending().is_some()
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.lock_pending()
            .as_ref()
            .map_or(0, |(text, _)| text.len())
    }

    pub(crate) fn received_chunks(&self) -> u64 {
        self.received_chunks
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// An output event paired with a monotonic sequence number for cross-lane ordering.
#[derive(Clone, Debug)]
pub struct SequencedOutputEvent {
    pub sequence: u64,
    pub event: OutputEvent,
}

impl SequencedOutputEvent {
    #[must_use]
    pub fn new(sequence: u64, event: OutputEvent) -> Self {
        Self { sequence, event }
    }
}

impl From<OutputEvent> for SequencedOutputEvent {
    fn from(event: OutputEvent) -> Self {
        Self { sequence: 0, event }
    }
}

impl std::ops::Deref for SequencedOutputEvent {
    type Target = OutputEvent;
    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

impl std::ops::DerefMut for SequencedOutputEvent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.event
    }
}

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
    /// A queued user message is about to begin a new agent turn. The TUI uses
    /// the remaining count to stop suppressing terminal panels from prior turns.
    QueuedMessageStarted { remaining: usize },
    /// Local slash-command echo rendered like a user prompt without starting a new turn.
    LocalCommandEcho(Line<'static>),
    /// Raw ANSI escape sequences (for PTY output, etc.).
    RawAnsi(String),
    /// Task completion message retained in the interactive transcript.
    Completion(String),
    /// Error message shown at the interactive transcript tail until the next prompt.
    ErrorBox(String),
    /// End of a streamed agent turn. The TUI uses this to re-render
    /// the raw streamed lines recorded during the turn as formatted
    /// markdown. The payload is the original (pre-wrap, pre-indent)
    /// markdown text accumulated by the agent loop.
    ///
    /// In non-interactive output paths (e.g. one-shot/JSON), this is a
    /// no-op marker.
    TurnEnd {
        accumulated_text: String,
        timing: Option<TurnEndTiming>,
    },
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

    /// Returns true when this event represents a transient state snapshot
    /// (`ModelUpdateLine`) whose latest value supersedes prior ones. Lossy
    /// events may be dropped without losing user-visible content; every
    /// other event is finalized and must keep its content + order.
    pub fn is_lossy(&self) -> bool {
        matches!(self, Self::ModelUpdateLine(_))
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

    pub fn queued_message_started(remaining: usize) -> Self {
        Self::QueuedMessageStarted { remaining }
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

    /// Flush any deferred finalized events that were preserved on overflow.
    /// Returns `(moved, remaining)`: how many events were re-inserted into
    /// the main channel and how many are still waiting. The drain loop uses
    /// this to pull waiting finalized output back before post-priority
    /// control events render, preserving emission order across lanes.
    /// Default: nothing to flush.
    fn flush_deferred_finalized_events(&self) -> (usize, usize) {
        (0, 0)
    }

    /// Returns whether any finalized events are waiting in the deferred
    /// queue. Used by the drain loop to decide whether post-priority
    /// control events must wait for the next frame. Default: none.
    fn has_deferred_finalized_events(&self) -> bool {
        false
    }

    /// Returns the sequence number of the oldest event waiting in the
    /// deferred finalized queue, if any.
    fn oldest_deferred_sequence(&self) -> Option<u64> {
        None
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
            | OutputEvent::UserPromptLine(line)
            | OutputEvent::LocalCommandEcho(line) => {
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
            | OutputEvent::QueuedMessageStarted { .. }
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
    reasoning: std::sync::atomic::AtomicU64,
    approval_prompt: std::sync::atomic::AtomicU64,
    other: std::sync::atomic::AtomicU64,
}

impl DropCounters {
    fn total(&self) -> u64 {
        self.model_text.load(std::sync::atomic::Ordering::Relaxed)
            + self.tool_output.load(std::sync::atomic::Ordering::Relaxed)
            + self.reasoning.load(std::sync::atomic::Ordering::Relaxed)
            + self
                .approval_prompt
                .load(std::sync::atomic::Ordering::Relaxed)
            + self.other.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn format_summary(&self) -> String {
        let m = self.model_text.load(std::sync::atomic::Ordering::Relaxed);
        let t = self.tool_output.load(std::sync::atomic::Ordering::Relaxed);
        let r = self.reasoning.load(std::sync::atomic::Ordering::Relaxed);
        let a = self
            .approval_prompt
            .load(std::sync::atomic::Ordering::Relaxed);
        let o = self.other.load(std::sync::atomic::Ordering::Relaxed);
        let mut parts = Vec::new();
        if m > 0 {
            parts.push(format!("{m} model"));
        }
        if t > 0 {
            parts.push(format!("{t} tools"));
        }
        if r > 0 {
            parts.push(format!("{r} reasoning"));
        }
        if a > 0 {
            parts.push(format!("{a} approvals"));
        }
        if o > 0 {
            parts.push(format!("{o} other"));
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
/// The main channel is bounded (default 262144 entries; override with
/// `SNED_OUTPUT_CHANNEL_CAPACITY` in `run_interactive_shell_inner`). Approval
/// requests and critical overflow events use separate control lanes so the
/// drain loop can service approvals first and bound critical work separately.
///
/// Finalized transcript output keeps one ordering domain: the bounded main
/// channel plus a bounded deferred FIFO. Events that arrive while the main
/// channel is full wait in the deferred queue (in emission order) and are
/// re-inserted at the main-channel tail by `flush_deferred_finalized_events`,
/// which runs before every drain pass, before every non-lossy `emit`, and
/// inside the drain pass before post-priority control events apply.
/// Flushing before appending guarantees a freed slot always goes to the
/// oldest waiting event first, so finalized output cannot be reordered by
/// lane overtaking. Control events stay on the priority lane so the UI
/// remains responsive. The **delivery contract** is explicit and bounded:
///
/// - `ModelUpdateLine` snapshots are droppable in any quantity; they are
///   coalesced by the TUI's streaming layout and the user-visible
///   transcript only ever shows the last snapshot.
/// - Finalized events (`Line`, `ToolHeaderLine`, `ToolOutputLine`,
///   `CommandHeaderLine`, `CommandOutputLine`, `RawAnsi`,
///   `LocalCommandEcho`, `TurnIndicator`) are retained without loss while the
///   deferred queue stays within `MAX_DEFERRED_FINALIZED_EVENTS` and
///   `MAX_DEFERRED_FINALIZED_BYTES` (defaults: 65 536 entries / 64 MiB).
///   Those bounds are sized to absorb every realistic model stream, tool
///   flood, and command flood in a single turn. The byte bound is an
///   estimate based on rendered text length, not an exact heap-memory bound.
/// - When the deferred queue is saturated and a new finalized event
///   arrives, the **oldest waiter is evicted** to make room. This is the
///   documented bounded-loss overflow policy: the eviction is counted per
///   category by `record_dropped_event`, which sets the overflow signal so
///   the TUI shows a visible `⚠ N dropped` banner with a per-category
///   summary. Loss is therefore never silent. This is bounded finalized-
///   output retention with visible loss on saturation, not a lossless
///   guarantee.
/// - A single finalized event larger than `MAX_DEFERRED_FINALIZED_BYTES`
///   is rejected outright as a counted, signaled bounded failure, so the
///   deferred queue can never exceed its advertised bound by one event.
///   The rejection is checked before any eviction, so a single oversized
///   arrival cannot discard valid older output.
///
/// This is the single contract. Tests, comments, and acceptance claims all
/// refer to it: see `test_deferred_finalized_events_are_bounded_and_drop_earliest`,
/// `test_deferred_finalized_events_are_bounded_by_bytes`, and
/// `test_deferred_finalized_rejects_single_oversized_event`.
pub struct ChannelOutputWriter {
    tx: mpsc::Sender<SequencedOutputEvent>,
    reasoning_mailbox: ReasoningMailbox,
    approval_tx: mpsc::UnboundedSender<SequencedOutputEvent>,
    approval_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<SequencedOutputEvent>>>,
    priority_tx: mpsc::UnboundedSender<SequencedOutputEvent>,
    priority_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<SequencedOutputEvent>>>,
    approval_attached: std::sync::atomic::AtomicBool,
    priority_attached: std::sync::atomic::AtomicBool,
    inner: std::sync::Mutex<ChannelOutputWriterInner>,
    /// Maximum finalized events retained while waiting for main queue drain.
    /// Kept bounded to avoid unbounded memory growth.
    max_deferred_finalized_events: usize,
    /// Maximum estimated payload bytes retained in the deferred queue.
    /// Bounds memory even when individual events are large.
    max_deferred_finalized_bytes: usize,
    drop_counters: DropCounters,
    overflow_signaled: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct ChannelOutputWriterInner {
    next_sequence: u64,
    deferred_finalized_events: DeferredFinalizedQueue,
}

/// Bounded FIFO for finalized events that overflowed the main channel.
///
/// Overflow policy (explicit, bounded-loss): when either bound is reached,
/// the oldest waiting event is evicted to make room for the newest arrival.
/// Every eviction is counted by category via `record_dropped_event`, which
/// also sets the overflow signal so the TUI shows a visible `⚠ N dropped`
/// indicator with a per-category summary. Loss is therefore never silent:
/// this implements the "fail visibly and preserve the diagnostic" option.
/// Evicting the oldest (rather than rejecting the newest) keeps the most
/// recent transcript context adjacent to the live stream. A single event
/// larger than the byte bound is rejected before any eviction, so it cannot
/// discard valid older output.
#[derive(Default)]
struct DeferredFinalizedQueue {
    events: VecDeque<SizedOutputEvent>,
    bytes: usize,
}

struct SizedOutputEvent {
    event: SequencedOutputEvent,
    bytes: usize,
}

impl ChannelOutputWriter {
    /// Create a new ChannelOutputWriter with a bounded channel.
    /// Bounded number of finalized events retained while the main output
    /// queue is full. Sized to absorb a long sustained backpressure burst
    /// (provider stream + tool flood + command flood in one turn) without
    /// evicting any finalized events. Finalized events beyond this bound
    /// fall back to the explicit overflow policy (counted + visible banner).
    pub(crate) const MAX_DEFERRED_FINALIZED_EVENTS: usize = 65_536;
    /// Bounded payload bytes retained in the deferred finalized queue.
    /// Caps memory when individual finalized events are large.
    pub(crate) const MAX_DEFERRED_FINALIZED_BYTES: usize = 64 * 1024 * 1024;

    #[must_use]
    pub(crate) fn with_deferred_capacity(
        tx: mpsc::Sender<SequencedOutputEvent>,
        max_deferred_finalized_events: usize,
    ) -> Self {
        Self::with_deferred_capacity_and_bytes(
            tx,
            max_deferred_finalized_events,
            Self::MAX_DEFERRED_FINALIZED_BYTES,
        )
    }

    #[must_use]
    pub(crate) fn with_deferred_capacity_and_bytes(
        tx: mpsc::Sender<SequencedOutputEvent>,
        max_deferred_finalized_events: usize,
        max_deferred_finalized_bytes: usize,
    ) -> Self {
        let (approval_tx, approval_rx) = mpsc::unbounded_channel();
        let (priority_tx, priority_rx) = mpsc::unbounded_channel();
        Self {
            tx,
            reasoning_mailbox: ReasoningMailbox::default(),
            approval_tx,
            approval_rx: std::sync::Mutex::new(Some(approval_rx)),
            priority_tx,
            priority_rx: std::sync::Mutex::new(Some(priority_rx)),
            approval_attached: std::sync::atomic::AtomicBool::new(false),
            priority_attached: std::sync::atomic::AtomicBool::new(false),
            inner: std::sync::Mutex::new(ChannelOutputWriterInner {
                next_sequence: 1,
                deferred_finalized_events: DeferredFinalizedQueue::default(),
            }),
            max_deferred_finalized_events: max_deferred_finalized_events.max(1),
            max_deferred_finalized_bytes: max_deferred_finalized_bytes.max(1),
            drop_counters: DropCounters::default(),
            overflow_signaled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn new(tx: mpsc::Sender<SequencedOutputEvent>) -> Self {
        Self::with_deferred_capacity(tx, Self::MAX_DEFERRED_FINALIZED_EVENTS)
    }

    #[must_use]
    pub(crate) fn reasoning_mailbox(&self) -> ReasoningMailbox {
        self.reasoning_mailbox.clone()
    }

    #[must_use]
    pub fn take_approval_rx(&self) -> Option<mpsc::UnboundedReceiver<SequencedOutputEvent>> {
        let receiver = self
            .approval_rx
            .lock()
            .expect("approval rx mutex poisoned")
            .take();
        if receiver.is_some() {
            self.approval_attached
                .store(true, std::sync::atomic::Ordering::Release);
        }
        receiver
    }

    #[must_use]
    pub fn take_priority_rx(&self) -> Option<mpsc::UnboundedReceiver<SequencedOutputEvent>> {
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
            OutputEvent::Line(_) | OutputEvent::ModelUpdateLine(_) => {
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
            OutputEvent::ReasoningChunk(_) => {
                counters
                    .reasoning
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

    fn flush_reasoning_snapshot_locked(&self, inner: &mut ChannelOutputWriterInner) {
        let Some((snapshot, _)) = self.reasoning_mailbox.take_with_sequence() else {
            return;
        };
        let sequence = inner.next_sequence;
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        let sequenced = SequencedOutputEvent::new(sequence, OutputEvent::ReasoningChunk(snapshot));
        if let Err(error) = self.tx.try_send(sequenced) {
            self.record_dropped_event(&error.into_inner().event);
        }
    }

    /// Estimated heap payload of an event, used only to bound the deferred
    /// finalized queue. Exact accounting is unnecessary; the goal is to cap
    /// memory when deferred events are individually large.
    fn estimated_event_bytes(event: &OutputEvent) -> usize {
        match event {
            OutputEvent::Line(line)
            | OutputEvent::ModelUpdateLine(line)
            | OutputEvent::ToolOutputLine(line)
            | OutputEvent::ToolHeaderLine(line)
            | OutputEvent::CommandHeaderLine(line)
            | OutputEvent::CommandOutputLine(line)
            | OutputEvent::UserPromptLine(line)
            | OutputEvent::LocalCommandEcho(line)
            | OutputEvent::TurnIndicator(line) => line.to_string().len(),
            OutputEvent::ReasoningChunk(chunk)
            | OutputEvent::RawAnsi(chunk)
            | OutputEvent::Completion(chunk)
            | OutputEvent::ErrorBox(chunk) => chunk.len(),
            OutputEvent::TurnEnd {
                accumulated_text, ..
            } => accumulated_text.len(),
            OutputEvent::QueuedMessageStarted { .. } | OutputEvent::ApprovalFinished { .. } => {
                std::mem::size_of::<usize>()
            }
            OutputEvent::ApprovalRequested(_) => 1024,
        }
    }

    fn push_deferred_finalized_event_inner(
        &self,
        inner: &mut ChannelOutputWriterInner,
        event: SequencedOutputEvent,
    ) {
        let bytes = Self::estimated_event_bytes(&event.event);
        // A single event larger than the whole byte bound can never fit no
        // matter how much is evicted. Reject it before evicting any valid
        // older output, so a single oversized arrival cannot discard
        // already-queued finalized events.
        if bytes > self.max_deferred_finalized_bytes {
            self.record_dropped_event(&event.event);
            return;
        }
        let queue = &mut inner.deferred_finalized_events;
        while !queue.events.is_empty()
            && (queue.events.len() >= self.max_deferred_finalized_events
                || queue.bytes.saturating_add(bytes) > self.max_deferred_finalized_bytes)
        {
            if let Some(dropped) = queue.events.pop_front() {
                queue.bytes = queue.bytes.saturating_sub(dropped.bytes);
                self.record_dropped_event(&dropped.event.event);
            }
        }
        queue.bytes = queue.bytes.saturating_add(bytes);
        queue.events.push_back(SizedOutputEvent { event, bytes });
    }

    /// Flush any deferred finalized events back into the main channel.
    /// Returns `(moved, remaining)`: how many events were re-inserted and
    /// how many are still waiting. The drain loop uses `remaining` to keep
    /// pulling before post-priority control events render, preserving
    /// emission order across lanes.
    fn flush_deferred_finalized_events_inner(
        &self,
        inner: &mut ChannelOutputWriterInner,
    ) -> (usize, usize) {
        let mut moved = 0usize;
        let queue = &mut inner.deferred_finalized_events;
        while let Some(pending) = queue.events.pop_front() {
            queue.bytes = queue.bytes.saturating_sub(pending.bytes);
            match self.tx.try_send(pending.event) {
                Ok(()) => {
                    moved = moved.saturating_add(1);
                }
                Err(mpsc::error::TrySendError::Full(event)) => {
                    let bytes = Self::estimated_event_bytes(&event.event);
                    queue.bytes = queue.bytes.saturating_add(bytes);
                    queue.events.push_front(SizedOutputEvent { event, bytes });
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(event)) => {
                    // The TUI is gone; count the rest as lost so the
                    // diagnostic reflects what never rendered.
                    self.record_dropped_event(&event.event);
                    while let Some(pending) = queue.events.pop_front() {
                        queue.bytes = queue.bytes.saturating_sub(pending.bytes);
                        self.record_dropped_event(&pending.event.event);
                    }
                    break;
                }
            }
        }
        (moved, queue.events.len())
    }

    #[cfg(test)]
    pub(crate) fn deferred_queue_for_test(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .deferred_finalized_events
            .events
            .len()
    }

    #[cfg(test)]
    pub(crate) fn deferred_bytes_for_test(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .deferred_finalized_events
            .bytes
    }
}

impl OutputWriter for ChannelOutputWriter {
    fn emit(&self, event: OutputEvent) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());

        if let OutputEvent::ReasoningChunk(chunk) = event {
            let sequence = inner.next_sequence;
            inner.next_sequence = inner.next_sequence.saturating_add(1);
            self.reasoning_mailbox.append_with_sequence(chunk, sequence);
            return;
        }

        self.flush_reasoning_snapshot_locked(&mut inner);

        let sequence = inner.next_sequence;
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        let sequenced = SequencedOutputEvent::new(sequence, event);

        if matches!(
            &sequenced.event,
            OutputEvent::ApprovalRequested(_) | OutputEvent::ApprovalFinished { .. }
        ) {
            // Approval bypasses transcript backlog so the panel is rendered
            // before the user can accidentally act on an unseen request.
            if !self
                .approval_attached
                .load(std::sync::atomic::Ordering::Acquire)
            {
                self.fail_control_delivery(
                    sequenced.event,
                    "interactive approval receiver is not attached",
                );
                return;
            }
            if let Err(err) = self.approval_tx.send(sequenced) {
                self.fail_control_delivery(err.0.event, "interactive approval receiver is closed");
            }
            return;
        }

        let is_lossy_update = sequenced.event.is_lossy();
        if !is_lossy_update {
            // Re-insert any waiting finalized events before appending: a
            // slot freed by the drain loop must go to the oldest waiter
            // first. Without this, a newer event could take the freed slot
            // while an older event still waits in the deferred queue,
            // reordering the transcript. Lossy snapshots skip this; they
            // are droppable.
            self.flush_deferred_finalized_events_inner(&mut inner);
        }
        // This path must remain non-blocking. A slow TUI is handled as an
        // explicit delivery policy below (lossy updates are counted; critical
        // events use the priority lane), rather than stalling the agent or a
        // tool producer behind the UI.
        if let Err(err) = self.tx.try_send(sequenced) {
            if is_lossy_update {
                let dropped = err.into_inner();
                self.record_dropped_event(&dropped.event);
                return;
            }

            // Main channel is full. Classify the event to determine
            // whether it must be preserved (critical) or can be safely
            // dropped (non-critical).
            let dropped = err.into_inner();
            let is_critical = matches!(
                &dropped.event,
                OutputEvent::TurnEnd { .. }
                    | OutputEvent::Completion(_)
                    | OutputEvent::ErrorBox(_)
                    | OutputEvent::QueuedMessageStarted { .. }
                    | OutputEvent::UserPromptLine(_)
            );

            if is_critical {
                // Critical events survive channel saturation: send them
                // into the unbounded critical lane so the TUI always sees
                // them even when the main queue is backed up.
                if let Err(err) = self.priority_tx.send(dropped) {
                    self.record_dropped_event(&err.0.event);
                }
                return;
            }

            // Finalized, non-lossy, non-critical event: queue it in the
            // bounded deferred FIFO. The drain loop re-inserts these in
            // emission order before applying post-priority control events.
            // Beyond the bound the oldest waiter is evicted as a counted,
            // signaled bounded failure (see `DeferredFinalizedQueue`).
            self.push_deferred_finalized_event_inner(&mut inner, dropped);
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

    fn flush_deferred_finalized_events(&self) -> (usize, usize) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        self.flush_deferred_finalized_events_inner(&mut inner)
    }

    fn has_deferred_finalized_events(&self) -> bool {
        !self
            .inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .deferred_finalized_events
            .events
            .is_empty()
    }

    fn oldest_deferred_sequence(&self) -> Option<u64> {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .deferred_finalized_events
            .events
            .front()
            .map(|e| e.event.sequence)
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
    format_timing_phases_inner(
        session_start,
        request_sent,
        first_provider_chunk,
        first_reasoning_chunk,
        first_displayable_text,
        first_output_emit,
        first_render,
        None,
    )
}

#[must_use]
pub(crate) fn format_timing_phases_with_retries(
    session_start: Instant,
    request_sent: Option<Instant>,
    first_provider_chunk: Option<Instant>,
    first_reasoning_chunk: Option<Instant>,
    first_displayable_text: Option<Instant>,
    first_output_emit: Option<Instant>,
    first_render: Option<Instant>,
    retry_info: Option<(usize, std::time::Duration)>,
) -> Vec<String> {
    format_timing_phases_inner(
        session_start,
        request_sent,
        first_provider_chunk,
        first_reasoning_chunk,
        first_displayable_text,
        first_output_emit,
        first_render,
        retry_info,
    )
}

fn format_timing_phases_inner(
    session_start: Instant,
    request_sent: Option<Instant>,
    first_provider_chunk: Option<Instant>,
    first_reasoning_chunk: Option<Instant>,
    first_displayable_text: Option<Instant>,
    first_output_emit: Option<Instant>,
    first_render: Option<Instant>,
    retry_info: Option<(usize, std::time::Duration)>,
) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(first_output_emit) = first_output_emit {
        lines.push(format!(
            "[timing] session_to_first_output_us={}",
            first_output_emit.duration_since(session_start).as_micros()
        ));
    }

    if let Some((stream_attempts, preoutput_retry_elapsed)) = retry_info {
        lines.push(format!("[timing] stream_attempts={stream_attempts}"));
        lines.push(format!(
            "[timing] preoutput_retry_elapsed_us={}",
            preoutput_retry_elapsed.as_micros()
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
    use super::{
        ProviderTimingRecord, TimingHistogram, TuiTimingRecord, format_timing_phases,
        format_timing_phases_with_retries, non_interactive_approval_message,
    };
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

        let lines = format_timing_phases_with_retries(
            start,
            Some(request),
            Some(chunk),
            Some(reasoning),
            Some(displayable),
            Some(output),
            Some(render),
            Some((2, Duration::from_millis(350))),
        );

        assert_eq!(lines[0], "[timing] session_to_first_output_us=400000");
        assert_eq!(lines[1], "[timing] stream_attempts=2");
        assert_eq!(lines[2], "[timing] preoutput_retry_elapsed_us=350000");
        assert_eq!(lines[3], "[timing] session_to_request_us=100000");
        assert_eq!(lines[4], "[timing] request_to_first_chunk_us=250000");
        assert_eq!(
            lines[5],
            "[timing] first_chunk_to_first_reasoning_chunk_us=12000"
        );
        assert_eq!(
            lines[6],
            "[timing] first_chunk_to_first_displayable_text_us=25000"
        );
        assert_eq!(
            lines[7],
            "[timing] first_displayable_text_to_first_output_us=25000"
        );
        assert_eq!(lines[8], "[timing] first_chunk_to_first_output_us=50000");
        assert_eq!(lines[9], "[timing] first_output_to_first_render_us=16000");
    }

    #[test]
    fn test_format_timing_phases_omits_unknown_retry_metrics() {
        let start = Instant::now();
        let request_sent = Some(start + Duration::from_millis(100));
        let lines = format_timing_phases_with_retries(
            start,
            request_sent,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let compatibility_lines =
            format_timing_phases(start, request_sent, None, None, None, None, None);

        for lines in [&lines, &compatibility_lines] {
            assert!(
                !lines
                    .iter()
                    .any(|line| line.starts_with("[timing] stream_attempts="))
            );
            assert!(
                !lines
                    .iter()
                    .any(|line| line.starts_with("[timing] preoutput_retry_elapsed_us="))
            );
        }
    }

    #[test]
    fn test_machine_readable_timing_records_exclude_model_content() {
        let mut histogram = TimingHistogram::default();
        histogram.record(3_000);
        let provider = ProviderTimingRecord {
            record_type: "sned_timing_provider_attempt",
            run_id: Some("run-fixture".to_string()),
            session_id: "task-123".to_string(),
            turn: 1,
            attempt: 1,
            provider: "openai".to_string(),
            model: Some("fixture-model".to_string()),
            stream: true,
            request_to_headers_us: 100,
            headers_to_first_byte_us: Some(200),
            request_to_first_chunk_us: Some(300),
            first_chunk_to_displayable_text_us: Some(400),
            displayable_text_to_output_us: Some(500),
            stream_total_us: 600,
            raw_sse_frames: 2,
            decoded_chunks: 2,
            text_chunks: 1,
            reasoning_chunks: 0,
            empty_sse_frames: 0,
            max_inter_raw_byte_gap_us: 50,
            max_inter_decoded_chunk_gap_us: 60,
        };
        let tui = TuiTimingRecord {
            record_type: "sned_timing_tui_turn",
            run_id: Some("run-fixture".to_string()),
            session_id: "task-123".to_string(),
            turn: 1,
            first_output_to_render_us: Some(16_000),
            provider_stream_complete_to_turn_end_emit_us: Some(100),
            turn_end_emit_to_dequeue_us: Some(200),
            turn_end_dequeue_to_render_start_us: 25,
            events_drained_before_turn_end: 4,
            transcript_lines_at_turn_end: 3,
            turn_render_worker_us: 300,
            turn_render_syntax_highlight_us: 50,
            turn_render_apply_us: 25,
            layout_rebuild_us: 10,
            layout_rebuild_count: 1,
            drain_peak_us: 100,
            draw_peak_us: 200,
            drain_histogram: histogram.clone(),
            draw_histogram: histogram,
            frame_coalesced: false,
            main_queue_peak: 3,
            main_queue_backlog_peak: 2,
            main_queue_backlog_cycles: 1,
            priority_queue_peak: 0,
            approval_queue_peak: 0,
            dropped_events: 0,
        };

        let provider = serde_json::to_value(provider).expect("provider timing serializes");
        let tui = serde_json::to_value(tui).expect("TUI timing serializes");

        assert_eq!(provider["type"], "sned_timing_provider_attempt");
        assert_eq!(provider["run_id"], "run-fixture");
        assert_eq!(provider["request_to_headers_us"], 100);
        assert_eq!(tui["type"], "sned_timing_tui_turn");
        assert_eq!(tui["draw_histogram"]["2_4ms"], 1);
        let serialized = format!("{provider}{tui}");
        assert!(!serialized.contains("Authorization"));
        assert!(!serialized.contains("prompt"));
        assert!(!serialized.contains("response"));
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

    #[test]
    fn test_channel_overflow_preserves_approval_prompt_via_approval_lane() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let writer = ChannelOutputWriter::new(tx);
        let mut approval_rx = writer
            .take_approval_rx()
            .expect("approval receiver should be available");
        let _priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");

        writer.emit(OutputEvent::plain("line 1"));
        let (request, response_rx) = crate::core::approval::approval_request_for_test(
            50,
            "Approval required · execute_command",
            "🔧 Tool: execute_command\nExecute this tool?",
        );
        writer.emit(OutputEvent::ApprovalRequested(request));

        let event = approval_rx
            .try_recv()
            .expect("approval should bypass the saturated transcript queue");
        let OutputEvent::ApprovalRequested(request) = event.event else {
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
    fn test_channel_overflow_coalesces_reasoning_chunks_in_bounded_mailbox() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let writer = ChannelOutputWriter::new(tx);

        writer.emit(OutputEvent::plain("line 1"));
        writer.emit(OutputEvent::reasoning_chunk("first\n"));
        writer.emit(OutputEvent::reasoning_chunk("second"));

        assert_eq!(
            writer.reasoning_mailbox().take().as_deref(),
            Some("first\nsecond")
        );
        assert_eq!(writer.dropped_count(), 0);
        assert!(!writer.take_overflow_signal());
    }

    #[test]
    fn test_reasoning_snapshot_is_queued_before_following_output() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let writer = ChannelOutputWriter::new(tx);

        writer.emit(OutputEvent::reasoning_chunk("thinking"));
        writer.emit(OutputEvent::plain("answer"));

        assert!(matches!(
            rx.try_recv().unwrap().event,
            OutputEvent::ReasoningChunk(chunk) if chunk == "thinking"
        ));
        assert!(matches!(
            rx.try_recv().unwrap().event,
            OutputEvent::Line(line) if line.to_string() == "answer"
        ));
        assert!(!writer.reasoning_mailbox().is_pending());
    }

    #[test]
    fn test_reasoning_snapshot_has_a_bounded_tail() {
        use super::{MAX_REASONING_SNAPSHOT_BYTES, ReasoningMailbox};

        let mailbox = ReasoningMailbox::default();
        mailbox.append("a".repeat(MAX_REASONING_SNAPSHOT_BYTES + 1024));
        mailbox.append("終".repeat(1024));

        let snapshot = mailbox.take().expect("reasoning snapshot should exist");
        assert!(snapshot.len() <= MAX_REASONING_SNAPSHOT_BYTES);
        assert!(snapshot.ends_with(&"終".repeat(1024)));
        assert_eq!(mailbox.received_chunks(), 2);
    }

    #[test]
    fn test_reasoning_mailbox_counts_bursts_without_growing_unbounded() {
        use super::{MAX_REASONING_SNAPSHOT_BYTES, ReasoningMailbox};

        let mailbox = ReasoningMailbox::default();
        for _ in 0..100_000 {
            mailbox.append("reasoning chunk\n".to_string());
        }

        assert_eq!(mailbox.received_chunks(), 100_000);
        assert!(mailbox.pending_len() <= MAX_REASONING_SNAPSHOT_BYTES);
    }

    #[test]
    fn test_reasoning_mailbox_recovers_from_poisoned_lock() {
        use super::ReasoningMailbox;

        let mailbox = ReasoningMailbox::default();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = mailbox.pending.lock().unwrap();
            *guard = Some(("partial".to_string(), 0));
            panic!("simulate a panic while holding the mailbox lock");
        }));

        mailbox.append("still usable".to_string());
        assert_eq!(mailbox.take().as_deref(), Some("still usable"));
    }

    #[test]
    fn test_dropped_reasoning_snapshot_is_counted_when_main_queue_is_full() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let writer = ChannelOutputWriter::new(tx);

        writer.emit(OutputEvent::plain("line 1"));
        writer.emit(OutputEvent::reasoning_chunk("thinking"));
        writer.emit(OutputEvent::plain("line 2"));

        // "line 2" is now a finalized Line that goes to the bounded
        // deferred queue instead of being counted as a drop. Only the
        // transient reasoning snapshot is dropped and counted.
        assert_eq!(writer.dropped_count(), 1);
        assert_eq!(writer.drop_summary(), "1 reasoning");
        assert!(writer.take_overflow_signal());
    }

    #[test]
    fn test_deferred_finalized_rejects_single_oversized_event() {
        // P2 regression: a single finalized event larger than the byte
        // bound can never fit no matter how much is evicted. It must be
        // rejected as a counted, signaled bounded failure rather than
        // being inserted and exceeding the advertised byte bound.
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        // 8-byte payload budget with room for smaller events.
        let writer = ChannelOutputWriter::with_deferred_capacity_and_bytes(tx, 1_024, 8);

        writer.emit(OutputEvent::plain("main"));
        let oversized = "X".repeat(100);
        writer.emit(OutputEvent::plain(oversized));
        assert_eq!(
            writer.deferred_queue_for_test(),
            0,
            "oversized event must not be inserted into the deferred queue"
        );
        assert_eq!(
            writer.deferred_bytes_for_test(),
            0,
            "oversized event must not exceed the byte bound by sitting in the deferred queue"
        );
        assert_eq!(writer.dropped_count(), 1);
        assert_eq!(writer.drop_summary(), "1 model");
        assert!(writer.take_overflow_signal());

        // The main channel is untouched: the original line still drains.
        assert!(matches!(
            rx.try_recv().expect("main queue should still hold the first line").event,
            OutputEvent::Line(line) if line.to_string() == "main"
        ));
        let (moved, remaining) = writer.flush_deferred_finalized_events();
        assert_eq!(moved, 0);
        assert_eq!(remaining, 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_oversized_arrival_preserves_existing_deferred_event() {
        // P1 regression: when the deferred queue already holds a valid
        // finalized event and a new oversized event arrives, the existing
        // event must survive. The oversized event is rejected before any
        // eviction, so it cannot evict valid older output.
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        // 8-byte payload budget: "existing" (8 bytes) fits, "oversized" (100) does not.
        let writer = ChannelOutputWriter::with_deferred_capacity_and_bytes(tx, 1_024, 8);

        // Fill the main channel so the first event goes to the deferred queue.
        writer.emit(OutputEvent::plain("main"));
        // This event overflows into the deferred queue (8 bytes, fits the bound).
        writer.emit(OutputEvent::plain("existing"));
        assert_eq!(
            writer.deferred_queue_for_test(),
            1,
            "existing event must be in the deferred queue"
        );
        assert_eq!(
            writer.deferred_bytes_for_test(),
            8,
            "existing event must account for its 8 bytes"
        );

        // Now an oversized event arrives. It must be rejected without
        // evicting the existing event.
        let oversized = "X".repeat(100);
        writer.emit(OutputEvent::plain(oversized));

        assert_eq!(
            writer.deferred_queue_for_test(),
            1,
            "existing event must survive the oversized arrival"
        );
        assert_eq!(
            writer.deferred_bytes_for_test(),
            8,
            "byte count must be unchanged after oversized rejection"
        );
        // Only the oversized event is counted as dropped.
        assert_eq!(writer.dropped_count(), 1);
        assert_eq!(writer.drop_summary(), "1 model");
        assert!(writer.take_overflow_signal());

        // The existing event is still intact and flushable.
        // Drain the main channel first to free a slot.
        let _ = rx.try_recv();
        let (moved, remaining) = writer.flush_deferred_finalized_events();
        assert_eq!(moved, 1);
        assert_eq!(remaining, 0);
        assert!(matches!(
            rx.try_recv().expect("existing event should flush to main").event,
            OutputEvent::Line(line) if line.to_string() == "existing"
        ));
    }

    #[test]
    fn test_approval_request_fails_closed_without_attached_ui() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
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

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let writer = ChannelOutputWriter::new(tx);
        let approval_rx = writer
            .take_approval_rx()
            .expect("approval receiver should be available");
        drop(approval_rx);
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

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let writer = ChannelOutputWriter::new(tx);
        let priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");

        writer.emit(OutputEvent::plain("line 1"));
        drop(priority_rx);
        writer.emit(OutputEvent::TurnEnd {
            accumulated_text: "done".to_string(),
            timing: None,
        });
        writer.emit(OutputEvent::Completion("done".to_string()));
        writer.emit(OutputEvent::ErrorBox("failed".to_string()));
        writer.emit(OutputEvent::ReasoningChunk("thinking".to_string()));

        assert_eq!(writer.dropped_count(), 3);
        assert_eq!(writer.drop_summary(), "3 other");
        assert!(writer.take_overflow_signal());
        assert_eq!(
            writer.reasoning_mailbox().take().as_deref(),
            Some("thinking")
        );
    }

    #[test]
    fn test_channel_overflow_counts_lossy_model_updates() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};
        use ratatui::text::Line;

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let writer = ChannelOutputWriter::new(tx);

        writer.emit(OutputEvent::plain("line 1"));
        writer.emit(OutputEvent::ModelUpdateLine(Line::from("partial")));

        assert!(writer.take_overflow_signal());
        assert_eq!(writer.dropped_count(), 1);
        assert_eq!(writer.drop_summary(), "1 model");
    }

    #[test]
    fn test_channel_overflow_still_signals_when_all_receivers_are_gone() {
        use super::{ChannelOutputWriter, OutputEvent, OutputWriter};

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let writer = ChannelOutputWriter::new(tx);
        let priority_rx = writer
            .take_priority_rx()
            .expect("priority receiver should be available");

        drop(rx);
        drop(priority_rx);

        writer.emit(OutputEvent::plain("dropped"));
        // Closed receivers are only discovered once the deferred flush
        // attempts the send; the deferred queue holds finalized output
        // until then.
        let _ = writer.flush_deferred_finalized_events();

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
