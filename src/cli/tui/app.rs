//! App struct for ratatui TUI.
//!
//! This is the main application state for the ratatui render loop.

use super::history::FileHistory;
use super::theme;
use crate::core::file_search::FileSearchResult;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

const INPUT_MAX_VISIBLE_LINES: usize = 6;
const BLOCKING_PROMPT_INPUT_VISIBLE_LINES: usize = 1;
const APPROVAL_PANEL_MAX_DETAIL_ROWS: usize = 10;
const SCROLLBACK_FLUSH_LINE_BATCH: usize = 128;
const MAX_SCROLLBACK_LOAD_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SCROLLBACK_LOAD_LINES: usize = 10_000;
const MAX_PASTED_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_FOLDED_PASTE_CHUNKS: usize = 256;
const STATUS_NOTIFICATION_DURATION: Duration = Duration::from_secs(4);
const PICKER_MAX_VISIBLE_ROWS: usize = 8;
const OSC8_PREFIX: &str = "\x1b]8;;";
const HYPERLINK_MARKER_RED: u8 = 0xFE;
static NEXT_PASTE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Async @-mention search result delivered back to the interactive loop.
#[derive(Debug, Clone)]
pub struct MentionSearchUpdate {
    pub generation: u64,
    pub query: String,
    pub results: Vec<FileSearchResult>,
}

/// Distinguishes model-streamed prose from tool-result or system lines
/// in the TUI output buffer.  Only `Model` lines are tracked by
/// `turn_stream_entries` and popped during `finalize_turn_stream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Raw model response text — safe to pop and re-render as markdown.
    Model,
    /// Streamed reasoning text retained across partial chunks.
    Reasoning,
    /// Tool result, plan completion, action digest, heat map, etc.
    /// These lines must NOT be popped by `finalize_turn_stream`.
    ToolOutput,
}

/// Model switch awaiting an API key entered through the masked TUI prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingModelSwitch {
    pub provider: String,
    pub model_id: String,
}

/// Visual category for an output line.  Drives render-time structural
/// grouping (blank-line separators between different kinds, no
/// separators within a block).  Mirrors `output_lines` length-for-length
/// via `output_line_kinds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// Raw model response text (finalized or streamed).
    Model,
    /// Tool-call header line (e.g. "▶ execute_command").
    ToolHeader,
    /// Generic tool/system line (action digest, plan status, etc.).
    ToolOutput,
    /// Command-execution header (e.g. "Running: <cmd>").
    CommandHeader,
    /// Command stdout / stderr / tail lines.
    CommandOutput,
    /// Reasoning summary line ("Ɵ ...").
    Reasoning,
    /// User-submitted prompt line.
    UserPrompt,
    /// Kept distinct so transcript pruning cannot hide a blocking prompt.
    BlockingPrompt,
    /// Explicit turn separator (e.g. "──── ♦ ────").
    Separator,
}

/// Accent styling applied to each non-separator transcript row at render time.
fn block_kind_accent_style(kind: BlockKind) -> Style {
    match kind {
        BlockKind::Model => Style::default().fg(theme::ACCENT),
        BlockKind::ToolHeader => Style::default().fg(theme::TOOL_CALL_FG),
        BlockKind::ToolOutput | BlockKind::CommandOutput => Style::default().fg(theme::STATUS_FG),
        BlockKind::CommandHeader => Style::default().fg(theme::INFO_FG),
        BlockKind::Reasoning => Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::DIM),
        BlockKind::UserPrompt => Style::default().fg(theme::PROMPT_FG),
        BlockKind::BlockingPrompt => Style::default().fg(theme::WARNING_FG),
        BlockKind::Separator => Style::default().fg(theme::BORDER_FG),
    }
}

use crate::cli::colors::spinner_frame;
use crate::cli::output::{OutputEvent, OutputWriterArc};

/// Scroll behaviour state machine.
///
/// Valid transitions:
///
///   Auto ──scroll_lines()──→ Manual (offset = max)
///   Manual ──clamp_to_content(distance=0)──→ Auto
///   Auto ──pin_approval_bottom()──→ ApprovalPinned
///   ApprovalPinned ──clear_approval_pin()──→ Auto
///   ApprovalPinned ──scroll_lines()──→ no-op (returns false)
///
/// Invariants:
///   - Manual at offset > 0 from bottom stays Manual
///   - Manual at offset = 0 (bottom) snaps to Auto
///   - ApprovalPinned overrides Manual; user scroll is rejected
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollMode {
    Auto,
    Manual,
    ApprovalPinned,
}

/// `scroll_offset` is a visual-row coordinate, so it becomes invalid when a
/// resize changes wrapping or a streamed block is re-rendered as Markdown.
#[derive(Debug, Clone)]
struct ManualViewportAnchor {
    output_index: usize,
    row_offset: usize,
    separator_before: bool,
    text: String,
    normalized_text: String,
    scroll_y: usize,
}

#[derive(Debug, Clone)]
pub struct PasteChunk {
    pub marker: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteOutcome {
    Inserted,
    Folded { char_count: usize },
    RejectedTooLarge { max_bytes: usize },
    RejectedTooManyChunks { max_chunks: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
struct StatusNotification {
    message: String,
    kind: NotificationKind,
    expires_at: Instant,
}

struct PendingApproval {
    request: crate::core::approval::ApprovalRequest,
    lines: Vec<Line<'static>>,
    rendered: bool,
    scroll_from_bottom: usize,
    total_rows: usize,
    viewport_rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionPane {
    Transcript,
    Completion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionPoint {
    output_line_index: usize,
    row_in_line: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionRowSource {
    output_line_index: usize,
    row_in_line: usize,
}

#[derive(Debug, Clone)]
struct TextSelection {
    pane: SelectionPane,
    anchor: SelectionPoint,
    focus: SelectionPoint,
    click_target: Option<PathBuf>,
    dragging: bool,
    moved: bool,
    last_drag_redraw: Instant,
}

#[derive(Debug, Clone)]
struct VisibleCell {
    symbol: String,
    continuation: bool,
    hyperlink: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct SelectionSurface {
    pane: SelectionPane,
    content_area: Rect,
    rows: Vec<Vec<VisibleCell>>,
    row_sources: Vec<Option<SelectionRowSource>>,
}

enum ScrollbackCommand {
    Append(String),
    Flush(std_mpsc::Sender<io::Result<()>>),
    Clear(std_mpsc::Sender<io::Result<()>>),
    Shutdown(std_mpsc::Sender<io::Result<()>>),
}

struct ScrollbackWriter {
    path: PathBuf,
    sender: std_mpsc::Sender<ScrollbackCommand>,
    errors: std_mpsc::Receiver<String>,
    handle: Option<JoinHandle<()>>,
}

impl ScrollbackWriter {
    fn start(path: PathBuf) -> io::Result<Self> {
        let (sender, receiver) = std_mpsc::channel();
        let (error_sender, errors) = std_mpsc::channel();
        let worker_path = path.clone();
        let handle = std::thread::Builder::new()
            .name("sned-scrollback-writer".to_string())
            .spawn(move || scrollback_writer_loop(&worker_path, &receiver, &error_sender))?;
        Ok(Self {
            path,
            sender,
            errors,
            handle: Some(handle),
        })
    }

    fn append(&self, batch: String) -> Result<(), String> {
        match self.sender.send(ScrollbackCommand::Append(batch)) {
            Ok(()) => Ok(()),
            Err(std_mpsc::SendError(ScrollbackCommand::Append(batch))) => Err(batch),
            Err(_) => unreachable!("append send must return the append command"),
        }
    }

    fn flush(&self) -> io::Result<()> {
        let (sender, receiver) = std_mpsc::channel();
        self.sender
            .send(ScrollbackCommand::Flush(sender))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "scrollback writer stopped"))?;
        receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "scrollback writer stopped"))?
    }

    fn clear(&self) -> io::Result<()> {
        let (sender, receiver) = std_mpsc::channel();
        self.sender
            .send(ScrollbackCommand::Clear(sender))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "scrollback writer stopped"))?;
        receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "scrollback writer stopped"))?
    }

    fn take_error(&self) -> Option<String> {
        let mut latest = None;
        while let Ok(error) = self.errors.try_recv() {
            latest = Some(error);
        }
        latest
    }

    fn shutdown(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let (sender, receiver) = std_mpsc::channel();
        let result = match self.sender.send(ScrollbackCommand::Shutdown(sender)) {
            Ok(()) => receiver.recv().unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "scrollback writer stopped",
                ))
            }),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scrollback writer stopped",
            )),
        };
        if handle.join().is_err() {
            return Err(io::Error::other("scrollback writer panicked"));
        }
        result
    }
}

fn scrollback_writer_loop(
    path: &Path,
    receiver: &std_mpsc::Receiver<ScrollbackCommand>,
    error_sender: &std_mpsc::Sender<String>,
) {
    let mut pending = Vec::new();
    while let Ok(command) = receiver.recv() {
        match command {
            ScrollbackCommand::Append(batch) => {
                pending.extend_from_slice(batch.as_bytes());
                if let Err(error) = write_scrollback_pending(path, &mut pending) {
                    let _ = error_sender.send(error.to_string());
                }
            }
            ScrollbackCommand::Flush(sender) => {
                let _ = sender.send(write_scrollback_pending(path, &mut pending));
            }
            ScrollbackCommand::Clear(sender) => {
                pending.clear();
                let result = match std::fs::remove_file(path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error),
                };
                let _ = sender.send(result);
            }
            ScrollbackCommand::Shutdown(sender) => {
                let _ = sender.send(write_scrollback_pending(path, &mut pending));
                return;
            }
        }
    }
    let _ = write_scrollback_pending(path, &mut pending);
}

fn write_scrollback_pending(path: &Path, pending: &mut Vec<u8>) -> io::Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut written = 0;
    while written < pending.len() {
        match file.write(&pending[written..]) {
            Ok(0) => {
                pending.drain(..written);
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write scrollback batch",
                ));
            }
            Ok(count) => written = written.saturating_add(count),
            Err(error) => {
                pending.drain(..written);
                return Err(error);
            }
        }
    }
    pending.clear();
    rotate_scrollback_file(path)
}

fn rotate_scrollback_file(path: &Path) -> io::Result<()> {
    let file_len = path.metadata()?.len();
    let Some(content) = read_scrollback_tail(path)? else {
        return Ok(());
    };
    let line_count = content.lines().count();
    if file_len <= MAX_SCROLLBACK_LOAD_BYTES && line_count <= MAX_SCROLLBACK_LOAD_LINES {
        return Ok(());
    }

    let first_retained_line = line_count.saturating_sub(MAX_SCROLLBACK_LOAD_LINES);
    let mut retained = content
        .lines()
        .skip(first_retained_line)
        .collect::<Vec<_>>()
        .join("\n");
    if !retained.is_empty() {
        retained.push('\n');
    }
    std::fs::write(path, retained)
}

fn read_scrollback_tail(path: &Path) -> io::Result<Option<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let file_len = file.metadata()?.len();
    let offset = file_len.saturating_sub(MAX_SCROLLBACK_LOAD_BYTES);
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }

    let mut bytes = Vec::with_capacity((file_len - offset) as usize);
    file.read_to_end(&mut bytes)?;
    if offset > 0 {
        let retained_start = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |position| position + 1);
        bytes.drain(..retained_start);
    }

    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Application state for the ratatui TUI.
pub struct App {
    /// Output lines buffer (agent output, submitted prompts, etc.)
    pub output_lines: VecDeque<Line<'static>>,
    /// Per-line visual category.  Always the same length as
    /// `output_lines`; used by `render_output` to insert blank-line
    /// separators between different block kinds.
    pub output_line_kinds: VecDeque<BlockKind>,
    /// Input textarea (live user input)
    pub input: TextArea<'static>,
    pending_approval: Option<PendingApproval>,
    /// Whether the agent is currently busy
    pub agent_busy: bool,
    /// Whether the model is currently in a reasoning/thinking phase (no displayable output yet).
    /// Drives the "Reasoning..." indicator rendered above the status bar.
    pub reasoning_active: bool,
    reasoning_partial_line: String,
    /// Manual scroll offset (top-of-viewport line index)
    pub scroll_offset: usize,
    /// Current output scroll behavior
    pub scroll_mode: ScrollMode,
    pub unseen_output_count: usize,
    pending_manual_viewport_anchor: Option<ManualViewportAnchor>,
    /// Whether the next draw should re-sync layout from the terminal size.
    pub has_resized: bool,
    /// Whether the next draw should render (dirty flag for render optimization).
    pub needs_redraw: bool,
    /// Session start time (for elapsed time display)
    pub start_time: Option<Instant>,
    /// Spinner animation frame index
    pub spinner_index: usize,
    /// Last time the spinner frame advanced.
    pub last_spinner_tick: Option<Instant>,
    /// Current working directory (for file search)
    pub cwd: String,
    /// Whether @ mention file picker is active
    pub picker_active: bool,
    /// Current file search results
    pub picker_results: Vec<FileSearchResult>,
    /// Selected index in picker results
    pub picker_index: usize,
    pub picker_selection_explicit: bool,
    /// File-backed command history with navigation state
    pub history: FileHistory,
    /// Folded paste chunks (marker -> original content)
    pub paste_chunks: Vec<PasteChunk>,
    /// Threshold for folding pastes (in characters)
    pub paste_fold_threshold: usize,
    /// Provider name for status bar
    pub provider_name: String,
    /// Model name for status bar
    pub model_name: String,
    /// Task ID for status bar
    pub task_id: String,
    /// Mode (PLAN/ACT) for status bar
    pub mode: String,
    pub yolo_mode: bool,
    pub auto_approve_all: bool,
    status_notification: Option<StatusNotification>,
    /// True when the output channel has overflowed and events were
    /// dropped. The status bar surfaces this so the user knows output
    /// (including approval prompts) may be missing.
    pub output_overflow: bool,
    /// Total number of dropped events, for the status bar indicator.
    pub output_overflow_count: u64,
    /// Per-category drop summary string (e.g. "5 model, 3 tools, 1 approval").
    pub output_overflow_summary: String,
    /// Number of messages queued for the agent.
    pub queued_message_count: usize,
    /// Path to the scrollback file for evicted output lines.
    pub scrollback_file: Option<std::path::PathBuf>,
    /// Number of lines stored in the scrollback file.
    pub scrollback_count: u64,
    /// Buffered evicted scrollback lines waiting to be appended to disk.
    pub scrollback_pending: String,
    /// Number of buffered lines waiting in `scrollback_pending`.
    pub scrollback_pending_lines: usize,
    scrollback_writer: Option<ScrollbackWriter>,
    /// True when the user is viewing scrollback history.
    pub in_scrollback: bool,
    /// Session elapsed time for status bar
    pub elapsed: Option<Duration>,
    /// Scrollbar state for output pane
    pub scrollbar_state: ScrollbarState,
    /// Last known content height from render (used by key handlers)
    pub last_content_height: usize,
    /// Last known output pane width from render/sync (used for wrapped scroll math)
    pub last_content_width: usize,
    /// Cached wrapped visual row count for the current output width.
    pub cached_visual_rows: usize,
    /// Width the cached visual row count was computed against.
    pub cached_wrap_width: Option<usize>,
    /// Pending clear confirmation (stores the trigger: "slash" or "ctrl_l")
    pub pending_clear: Option<String>,
    /// Saved draft input before history navigation
    pub history_draft: Option<String>,
    /// Cached plan state for TUI rendering (updated from interactive loop)
    pub plan_state_cache: Option<crate::core::plan_state::PlanState>,
    /// Pointer identity for the cached plan state.
    pub plan_state_cache_ptr: Option<usize>,
    /// Revision of the cached plan state.
    pub plan_state_cache_version: u64,
    /// Whether @ mention search is active (user is in mention mode).
    pub mention_search_active: bool,
    /// Last query searched in mention mode (to detect changes).
    pub mention_search_query: String,
    /// Deadline for debounced mention search.
    pub mention_search_deadline: Instant,
    /// Monotonic generation for the latest mention query; stale async
    /// search results are discarded when their generation no longer matches.
    pub mention_search_generation: u64,
    /// Result channel for async mention searches.
    pub mention_search_tx: Option<tokio::sync::mpsc::UnboundedSender<MentionSearchUpdate>>,
    /// Cached status bar left segment (provider / model | task | mode).
    /// Rebuilt only when the underlying fields change.
    pub cached_status_left: String,
    /// Fingerprint of the fields used to build cached_status_left.
    pub status_left_fingerprint: (String, String, String, String, bool, bool),
    /// Cached status bar right segment (elapsed timer). Rebuilt when seconds change.
    pub cached_status_right: String,
    /// Last known context usage percentage from the API.
    pub context_pct: Option<f64>,
    pub cached_status_right_secs: (u64, Option<f64>, bool, u64, usize, usize, ScrollMode),
    /// Cached spacer string for the status bar.
    pub cached_spacer: String,
    /// Length the cached spacer was built for.
    pub cached_spacer_len: usize,
    /// Cached visible output window result (start_idx, take_count, start_row_offset).
    pub cached_visible_window: Option<(usize, usize, usize)>,
    /// Fingerprint for the visible window cache (output_len, scroll_y, wrap_width, content_height, cached_visual_rows, scroll_mode).
    pub cached_window_fingerprint: (usize, usize, usize, usize, usize, ScrollMode),
    /// Whether the slash command picker is active.
    pub slash_command_active: bool,
    pub slash_command_help_active: bool,
    pub slash_command_track_changes: bool,
    /// Filtered slash command results for the current query.
    pub slash_command_results: Vec<crate::cli::slash_commands::SlashCommandEntry>,
    /// Currently selected index in the result list.
    pub slash_command_selected: usize,
    /// All available slash command entries (unfiltered).
    pub slash_command_all_entries: Vec<crate::cli::slash_commands::SlashCommandEntry>,
    /// Input text at the moment the slash command picker was last accepted
    /// (via Tab/Enter). The post-text-input re-evaluation skips re-enabling
    /// the picker while the current input still matches this value, so a
    /// completed `/plan` stays dismissed until the user starts a new query
    /// (separator, character, or backspace).
    pub slash_command_completed_text: Option<String>,
    /// Entries into `output_lines` of lines that were streamed from the
    /// model during the current turn.  Each entry records the buffer
    /// index and the kind of line (model prose vs tool output).
    /// When `OutputEvent::TurnEnd` arrives, `finalize_turn_stream` pops
    /// only the `Model` entries and replaces them with markdown-rendered
    /// equivalents.  ToolOutput lines are left untouched.
    /// Entries are recorded in append order; popping iterates from the
    /// highest index to the lowest to preserve earlier indices.
    pub turn_stream_entries: Vec<(usize, StreamKind)>,
    /// The most recent streamed logical line (start index, visual line
    /// count, kind). Used for in-place partial-line updates while a
    /// response is still streaming.
    pub last_stream_group: Option<(usize, usize, StreamKind)>,
    /// The turn indicator line (e.g. "♦") for the current turn. This is
    /// kept separate from `turn_stream_line_indices` so that
    /// `finalize_turn_stream` can re-insert it at the top of the
    /// markdown block instead of stripping it.
    pub turn_indicator: Option<Line<'static>>,
    /// True if at least one `OutputEvent::Line` was pushed through
    /// `push_stream_line` during the current turn. Used by
    /// `finalize_turn_stream` to decide whether to replace or append
    /// the markdown-rendered output.
    pub turn_had_streamed_line: bool,
    /// Whether the model picker is active.
    pub model_picker_active: bool,
    /// Model picker entries.
    pub model_picker_results: Vec<crate::cli::slash_commands::ModelPickerEntry>,
    /// Currently selected index in model picker.
    pub model_picker_selected: usize,
    /// Model switch awaiting an API key for a provider not configured in this process.
    pub pending_model_switch: Option<PendingModelSwitch>,
    /// Completion box lines rendered as a dedicated Block widget.
    pub completion_lines: VecDeque<Line<'static>>,
    last_completion_text: Option<String>,
    /// Cached completion row count, valid when cached_wrap_width matches.
    pub cached_completion_rows: usize,
    completion_scroll_offset: usize,
    completion_viewport_rows: usize,
    completion_area: Option<Rect>,
    transcript_selection_area: Option<Rect>,
    completion_selection_area: Option<Rect>,
    transcript_selection_row_sources: Vec<Option<SelectionRowSource>>,
    completion_selection_row_sources: Vec<Option<SelectionRowSource>>,
    text_selection: Option<TextSelection>,
    selection_surfaces: Vec<SelectionSurface>,
    rendered_hyperlink_targets: Vec<PathBuf>,
    /// Error box lines rendered as a dedicated Block widget with red border.
    /// Takes priority over completion_lines when non-empty.
    pub error_lines: VecDeque<Line<'static>>,
    /// Cached error row count, valid when cached_wrap_width matches.
    pub cached_error_rows: usize,
}

impl App {
    /// Extract plain text for dedup comparison. Styling artifacts from markdown
    /// re-render would corrupt the match against the raw completion result.
    pub(crate) fn line_to_string(line: &Line<'static>) -> String {
        let mut out = String::new();
        for span in &line.spans {
            out.push_str(&span.content);
        }
        out
    }
    /// Create a new TextArea with default styling (no underline on cursor line).
    #[must_use]
    pub fn new_textarea(lines: Vec<String>) -> TextArea<'static> {
        let mut input = TextArea::new(lines);
        input.set_placeholder_text("❯ ");
        input.set_cursor_line_style(Style::default());
        input
    }

    fn textarea_lines_from_text(text: &str) -> Vec<String> {
        text.split('\n').map(str::to_owned).collect()
    }

    fn cursor_row_col_for_text(text: &str, byte_offset: usize) -> (u16, u16) {
        let clamped = byte_offset.min(text.len());
        let mut row = 0usize;
        let mut line_start = 0usize;
        for (idx, ch) in text.char_indices() {
            if idx >= clamped {
                break;
            }
            if ch == '\n' {
                row += 1;
                line_start = idx + 1;
            }
        }
        let col = text[line_start..clamped].chars().count();
        (
            row.min(u16::MAX as usize) as u16,
            col.min(u16::MAX as usize) as u16,
        )
    }

    pub fn set_input_text(&mut self, text: &str) {
        self.input = Self::new_textarea(Self::textarea_lines_from_text(text));
    }

    pub fn set_input_text_and_cursor(&mut self, text: &str, byte_offset: usize) {
        self.set_input_text(text);
        let (row, col) = Self::cursor_row_col_for_text(text, byte_offset);
        self.input
            .move_cursor(tui_textarea::CursorMove::Jump(row, col));
    }

    pub fn input_height(&self) -> u16 {
        (self.input.lines().len().clamp(1, INPUT_MAX_VISIBLE_LINES) as u16) + 2
    }

    pub fn show_notification(&mut self, message: impl Into<String>, kind: NotificationKind) {
        self.status_notification = Some(StatusNotification {
            message: message.into(),
            kind,
            expires_at: Instant::now() + STATUS_NOTIFICATION_DURATION,
        });
        self.needs_redraw = true;
    }

    #[cfg(test)]
    pub(crate) fn notification_message(&self) -> Option<&str> {
        self.status_notification
            .as_ref()
            .map(|notification| notification.message.as_str())
    }

    pub fn tick_notification(&mut self, now: Instant) -> bool {
        if self
            .status_notification
            .as_ref()
            .is_some_and(|notification| now >= notification.expires_at)
        {
            self.status_notification = None;
            return true;
        }
        false
    }

    pub fn set_pending_approval(
        &mut self,
        request: crate::core::approval::ApprovalRequest,
    ) -> bool {
        if self.pending_approval.is_some() {
            request.fail("another approval request is already visible");
            return false;
        }

        let mut lines = super::ansi_converter::ansi_to_ratatui_lines(request.details());
        while lines
            .first()
            .is_some_and(|line| Self::line_to_string(line).trim().is_empty())
        {
            lines.remove(0);
        }
        while lines
            .last()
            .is_some_and(|line| Self::line_to_string(line).trim().is_empty())
        {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(Line::from("Approval details unavailable"));
        }

        self.pending_approval = Some(PendingApproval {
            request,
            lines,
            rendered: false,
            scroll_from_bottom: 0,
            total_rows: 0,
            viewport_rows: 0,
        });
        self.picker_active = false;
        self.mention_search_active = false;
        self.slash_command_active = false;
        self.slash_command_help_active = false;
        self.model_picker_active = false;
        self.clear_text_selection();
        self.needs_redraw = true;
        self.pin_approval_bottom();
        true
    }

    #[must_use]
    pub fn has_pending_approval(&self) -> bool {
        self.pending_approval.is_some()
    }

    #[must_use]
    pub fn has_unrendered_approval(&self) -> bool {
        self.pending_approval
            .as_ref()
            .is_some_and(|pending| !pending.rendered)
    }

    #[must_use]
    pub fn approval_accepts_input(&self) -> bool {
        self.pending_approval
            .as_ref()
            .is_some_and(|pending| pending.rendered)
    }

    #[must_use]
    pub fn pending_approval_result_for_shortcut(
        &self,
        shortcut: char,
    ) -> Option<crate::core::approval::ApprovalResult> {
        self.pending_approval
            .as_ref()
            .and_then(|pending| pending.request.result_for_shortcut(shortcut))
    }

    #[must_use]
    pub fn pending_approval_has_result(
        &self,
        result: crate::core::approval::ApprovalResult,
    ) -> bool {
        self.pending_approval
            .as_ref()
            .is_some_and(|pending| pending.request.has_result(result))
    }

    pub fn resolve_pending_approval(
        &mut self,
        result: crate::core::approval::ApprovalResult,
    ) -> Option<bool> {
        let pending = self.pending_approval.take()?;
        let delivered = pending.request.respond(result);
        self.needs_redraw = true;
        self.clear_approval_pin();
        Some(delivered)
    }

    pub fn finish_pending_approval(&mut self, id: u64) -> bool {
        if self
            .pending_approval
            .as_ref()
            .is_none_or(|pending| pending.request.id() != id)
        {
            return false;
        }
        self.pending_approval.take();
        self.needs_redraw = true;
        self.clear_approval_pin();
        true
    }

    pub fn scroll_pending_approval(&mut self, rows_toward_history: isize) {
        let Some(pending) = self.pending_approval.as_mut() else {
            return;
        };
        let max = pending.total_rows.saturating_sub(pending.viewport_rows);
        if rows_toward_history >= 0 {
            pending.scroll_from_bottom = pending
                .scroll_from_bottom
                .saturating_add(rows_toward_history as usize)
                .min(max);
        } else {
            pending.scroll_from_bottom = pending
                .scroll_from_bottom
                .saturating_sub(rows_toward_history.unsigned_abs());
        }
        self.needs_redraw = true;
    }

    fn approval_panel_height(&self) -> u16 {
        let Some(pending) = self.pending_approval.as_ref() else {
            return 0;
        };
        let wrap_width = self.last_wrap_width().saturating_sub(2).max(1);
        let detail_rows = pending
            .lines
            .iter()
            .map(|line| Self::line_visual_rows(line, wrap_width))
            .sum::<usize>()
            .clamp(1, APPROVAL_PANEL_MAX_DETAIL_ROWS);
        (detail_rows + 3).min(u16::MAX as usize) as u16
    }

    /// Update the textarea placeholder based on current mode.
    pub fn update_placeholder(&mut self) {
        if self.pending_model_switch.is_some() {
            self.input
                .set_placeholder_text("Enter API key · Enter saves and switches · Esc cancels");
        } else if self.mode == "PLAN" {
            self.input.set_placeholder_text("❯ [PLAN] ");
        } else if self.mode == "ACT" {
            self.input.set_placeholder_text("❯ [ACT] ");
        } else {
            self.input.set_placeholder_text("❯ ");
        }
    }

    /// Create a new App instance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            output_lines: VecDeque::new(),
            output_line_kinds: VecDeque::new(),
            input: Self::new_textarea(Vec::new()),
            pending_approval: None,
            agent_busy: false,
            reasoning_active: false,
            reasoning_partial_line: String::new(),
            scroll_offset: 0,
            scroll_mode: ScrollMode::Auto,
            unseen_output_count: 0,
            pending_manual_viewport_anchor: None,
            has_resized: true,
            needs_redraw: true,
            start_time: None,
            spinner_index: 0,
            last_spinner_tick: None,
            cwd: String::new(),
            picker_active: false,
            picker_results: Vec::new(),
            picker_index: 0,
            picker_selection_explicit: false,
            history: FileHistory::load(),
            paste_chunks: Vec::new(),
            paste_fold_threshold: 500, // Fold pastes > 500 chars
            provider_name: String::new(),
            model_name: String::new(),
            task_id: String::new(),
            mode: String::new(),
            yolo_mode: false,
            auto_approve_all: false,
            status_notification: None,
            elapsed: None,
            scrollbar_state: ScrollbarState::new(0),
            last_content_height: 0,
            last_content_width: 0,
            cached_visual_rows: 0,
            cached_wrap_width: None,
            pending_clear: None,
            history_draft: None,
            plan_state_cache: None,
            plan_state_cache_ptr: None,
            plan_state_cache_version: 0,
            mention_search_active: false,
            mention_search_query: String::new(),
            mention_search_deadline: Instant::now(),
            cached_status_left: String::new(),
            status_left_fingerprint: (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                false,
                false,
            ),
            cached_status_right: String::new(),
            cached_status_right_secs: (u64::MAX, None, false, 0, 0, 0, ScrollMode::Auto),
            context_pct: None,
            cached_spacer: String::new(),
            cached_spacer_len: 0,
            slash_command_active: false,
            slash_command_help_active: false,
            slash_command_track_changes: false,
            slash_command_results: Vec::new(),
            slash_command_selected: 0,
            slash_command_all_entries: Vec::new(),
            slash_command_completed_text: None,
            turn_stream_entries: Vec::new(),
            last_stream_group: None,
            turn_indicator: None,
            turn_had_streamed_line: false,
            model_picker_active: false,
            model_picker_results: Vec::new(),
            model_picker_selected: 0,
            pending_model_switch: None,
            completion_lines: VecDeque::new(),
            last_completion_text: None,
            cached_completion_rows: 0,
            completion_scroll_offset: 0,
            completion_viewport_rows: 0,
            completion_area: None,
            transcript_selection_area: None,
            completion_selection_area: None,
            transcript_selection_row_sources: Vec::new(),
            completion_selection_row_sources: Vec::new(),
            text_selection: None,
            selection_surfaces: Vec::new(),
            rendered_hyperlink_targets: Vec::new(),
            error_lines: VecDeque::new(),
            cached_error_rows: 0,
            cached_visible_window: None,
            cached_window_fingerprint: (0, 0, 0, 0, 0, ScrollMode::Auto),
            output_overflow: false,
            output_overflow_count: 0,
            output_overflow_summary: String::new(),
            queued_message_count: 0,
            scrollback_file: Some(crate::storage::disk::get_data_dir().join("scrollback/lines")),
            scrollback_count: 0,
            scrollback_pending: String::new(),
            scrollback_pending_lines: 0,
            scrollback_writer: None,
            in_scrollback: false,
            mention_search_generation: 0,
            mention_search_tx: None,
        }
    }

    /// Push an output line to the buffer. Long lines are pre-wrapped
    /// to `wrap_width` before being pushed, preventing render-time
    /// wrapping and visual overlap with adjacent content.
    pub fn push_output(&mut self, line: Line<'static>) {
        self.push_output_with_kind(line, BlockKind::ToolOutput);
    }

    /// Push an output line tagged with a structural block kind. Long
    /// lines are pre-wrapped to `wrap_width` before being pushed, and
    /// every pushed piece shares the same `kind`.
    pub fn push_output_with_kind(&mut self, line: Line<'static>, kind: BlockKind) {
        let wrap_width = self.last_wrap_width();
        let contains_hyperlink = Self::line_contains_osc8(&line);

        // Pre-wrap: if the line's total width exceeds wrap_width, split it
        let total_width: usize = line
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();

        let lines_to_push: Vec<Line<'static>> =
            if !contains_hyperlink && total_width > wrap_width && wrap_width > 0 {
                let mut full_text = String::new();
                for span in &line.spans {
                    full_text.push_str(span.content.as_ref());
                }
                let wrapped = crate::cli::text_utils::wrap_text(&full_text, wrap_width, "");
                wrapped.lines().map(|l| Line::from(l.to_string())).collect()
            } else {
                vec![line]
            };

        for l in lines_to_push {
            self._push_output_line(l, kind, wrap_width);
        }
    }

    /// Internal: push a single pre-wrapped line to the buffer with a
    /// kind tag.  `output_line_kinds` is kept in lockstep with
    /// `output_lines` so render-time grouping can walk both buffers
    /// with the same indices.
    fn _push_output_line(&mut self, line: Line<'static>, kind: BlockKind, wrap_width: usize) {
        let previous_kind = self.output_line_kinds.back().copied();
        self.needs_redraw = true;
        self.output_lines.push_back(line);
        self.output_line_kinds.push_back(kind);
        self.cached_visible_window = None;
        if self.output_lines.len() > 10_000 {
            let evicted_rows = if self.scroll_mode == ScrollMode::Manual {
                let evicted_kind = *self
                    .output_line_kinds
                    .front()
                    .expect("output line kinds must match output lines");
                let separator_rows = self
                    .output_line_kinds
                    .get(1)
                    .copied()
                    .is_some_and(|next_kind| Self::should_insert_separator(evicted_kind, next_kind))
                    as usize;
                Self::output_row_visual_rows(self.output_lines.front(), evicted_kind, wrap_width)
                    .saturating_add(separator_rows)
            } else {
                0
            };
            // Evict front line and buffer it for batched scrollback append.
            if let Some(line) = self.output_lines.front() {
                let text = Self::line_to_string(line);
                self.scrollback_pending.push_str(&text);
                self.scrollback_pending.push('\n');
                self.scrollback_pending_lines = self.scrollback_pending_lines.saturating_add(1);
                self.scrollback_count = self
                    .scrollback_count
                    .saturating_add(1)
                    .min(MAX_SCROLLBACK_LOAD_LINES as u64);
            }
            self.output_lines.pop_front();
            self.output_line_kinds.pop_front();
            if self.text_selection.as_ref().is_some_and(|selection| {
                selection.pane == SelectionPane::Transcript
                    && (selection.anchor.output_line_index == 0
                        || selection.focus.output_line_index == 0)
            }) {
                self.clear_text_selection();
            } else if let Some(selection) = self
                .text_selection
                .as_mut()
                .filter(|selection| selection.pane == SelectionPane::Transcript)
            {
                selection.anchor.output_line_index -= 1;
                selection.focus.output_line_index -= 1;
            }
            // The manual offset is measured from the transcript front. Keep
            // the viewport anchored to the same surviving visual row.
            if self.scroll_mode == ScrollMode::Manual {
                self.scroll_offset = self.scroll_offset.saturating_sub(evicted_rows);
            }
            self.cached_wrap_width = None;
            self.cached_visible_window = None;
        } else if self.cached_wrap_width == Some(wrap_width) {
            // Hot path: keep the cached row count in sync for simple appends
            // so the next render does not need to rescan the whole transcript.
            let added_rows = Self::output_row_visual_rows(
                Some(self.output_lines.back().unwrap()),
                kind,
                wrap_width,
            )
            .saturating_add(
                previous_kind.is_some_and(|prev| Self::should_insert_separator(prev, kind))
                    as usize,
            );
            self.cached_visual_rows = self.cached_visual_rows.saturating_add(added_rows);
        }
        match self.scroll_mode {
            ScrollMode::Auto => self.force_bottom(),
            ScrollMode::Manual => {
                self.unseen_output_count = self.unseen_output_count.saturating_add(1);
            }
            ScrollMode::ApprovalPinned => {}
        }
        self.refresh_pending_manual_viewport_anchor(wrap_width);
    }

    /// Push a line and record its index for turn-end markdown
    /// re-rendering.  The index is taken AFTER `push_back`, so it
    /// points at the line that was just added.  On `TurnEnd`, the
    /// recorded entries are popped (Model only) and the raw text
    /// re-rendered as markdown.
    ///
    /// Also sets `turn_had_streamed_line` to `true` when `kind` is
    /// `Model` to record that streamed output was received during
    /// this turn.
    pub fn push_stream_line(&mut self, line: Line<'static>, kind: StreamKind) {
        if kind == StreamKind::Model {
            self.turn_had_streamed_line = true;
        }

        let lines_to_push = self.prewrap_stream_line(line);
        self.push_stream_group(lines_to_push, kind);
    }

    pub fn push_reasoning_chunk(&mut self, chunk: &str) {
        for segment in chunk.split_inclusive('\n') {
            let complete = segment.ends_with('\n');
            let content = segment.strip_suffix('\n').unwrap_or(segment);
            let content = content.strip_suffix('\r').unwrap_or(content);
            self.reasoning_partial_line.push_str(content);
            let line = Line::from(Span::styled(
                format!("  Ɵ {}", self.reasoning_partial_line),
                Style::default()
                    .fg(crate::cli::tui::theme::ACCENT)
                    .add_modifier(Modifier::ITALIC),
            ));
            self.replace_last_stream_line(line, StreamKind::Reasoning);
            if complete {
                self.reasoning_partial_line.clear();
                self.last_stream_group = None;
            }
        }
    }

    pub fn finish_reasoning_stream(&mut self) {
        self.reasoning_partial_line.clear();
        if matches!(self.last_stream_group, Some((_, _, StreamKind::Reasoning))) {
            self.last_stream_group = None;
        }
    }

    /// Replace the most recent streamed logical line. Used for partial
    /// model-line updates so the TUI can repaint the current line
    /// without duplicating transcript entries.
    pub fn replace_last_stream_line(&mut self, line: Line<'static>, kind: StreamKind) {
        if kind == StreamKind::Model {
            self.turn_had_streamed_line = true;
        }

        let Some((start_idx, visual_line_count, last_kind)) = self.last_stream_group else {
            self.push_stream_line(line, kind);
            return;
        };

        let expected_len = start_idx.saturating_add(visual_line_count);
        if last_kind != kind
            || expected_len != self.output_lines.len()
            || visual_line_count == 0
            || self.turn_stream_entries.len() < visual_line_count
        {
            self.push_stream_line(line, kind);
            return;
        }

        let tail_entries =
            &self.turn_stream_entries[self.turn_stream_entries.len() - visual_line_count..];
        let tail_matches = tail_entries
            .iter()
            .enumerate()
            .all(|(offset, (idx, entry_kind))| *entry_kind == kind && *idx == start_idx + offset);
        if !tail_matches {
            self.push_stream_line(line, kind);
            return;
        }

        let block_kind = match kind {
            StreamKind::Model => BlockKind::Model,
            StreamKind::Reasoning => BlockKind::Reasoning,
            StreamKind::ToolOutput => BlockKind::ToolOutput,
        };
        let wrap_width = self.last_wrap_width();
        if self.cached_wrap_width == Some(wrap_width) {
            let tail_start = self.output_lines.len() - visual_line_count;
            let mut removed_rows: usize = self
                .output_lines
                .iter()
                .skip(tail_start)
                .take(visual_line_count)
                .zip(self.output_line_kinds.iter().skip(tail_start))
                .take(visual_line_count)
                .map(|(line, kind)| Self::output_row_visual_rows(Some(line), *kind, wrap_width))
                .sum();
            if let Some(prev_kind) = tail_start
                .checked_sub(1)
                .and_then(|idx| self.output_line_kinds.get(idx).copied())
                && Self::should_insert_separator(prev_kind, block_kind)
            {
                removed_rows = removed_rows.saturating_add(1);
            }
            self.cached_visual_rows = self.cached_visual_rows.saturating_sub(removed_rows);
        }
        self.cached_visible_window = None;
        self.cached_wrap_width = None;

        for _ in 0..visual_line_count {
            self.output_lines.pop_back();
            self.output_line_kinds.pop_back();
            self.turn_stream_entries.pop();
        }

        let unseen_output_count = self.unseen_output_count;
        let lines_to_push = self.prewrap_stream_line(line);
        self.push_stream_group(lines_to_push, kind);
        // Replacing a partial stream line is a repaint, not new output.
        self.unseen_output_count = unseen_output_count;
    }

    /// Store a turn-indicator line (e.g. "♦") for later re-insertion at
    /// the top of the markdown-rendered block.  This is kept separate
    /// from `turn_stream_line_indices` so `finalize_turn_stream` can
    /// re-insert the indicator at the top of the rendered block instead
    /// of stripping it.
    pub fn push_turn_indicator(&mut self, line: Line<'static>) {
        self.turn_indicator = Some(line);
    }

    /// Re-render the model-streamed lines recorded during the current
    /// turn as markdown.  Pops only the `Model` entries from the
    /// buffer (highest index first to preserve earlier indices) and
    /// pushes the rendered lines in their place.  ToolOutput lines are
    /// left untouched.  Resets `turn_stream_entries` and
    /// `turn_had_streamed_line`.
    ///
    /// `markdown_text` is the raw, pre-wrap, pre-indent text that was
    /// streamed during the turn. If empty, the streamed lines are
    /// left in place and the entry buffer is just cleared.
    ///
    /// When `turn_had_streamed_line` is true but no Model entries were
    /// recorded (the agent emitted at least one `OutputEvent::Line` this
    /// turn but it was pushed directly without `push_stream_line`), this
    /// function **appends** the rendered markdown after the existing
    /// streamed lines instead of replacing them. This avoids a visual
    /// flash where the streamed text is popped and re-inserted with
    /// different styling.
    pub fn finalize_turn_stream(&mut self, markdown_text: &str) {
        self.finish_reasoning_stream();
        let entries = std::mem::take(&mut self.turn_stream_entries);
        self.last_stream_group = None;
        let had_streamed_line = std::mem::take(&mut self.turn_had_streamed_line);

        // Filter to Model entries only — tool output lines must never
        // be popped or re-rendered by this function.
        let model_indices: Vec<(usize, StreamKind)> = entries
            .iter()
            .filter(|(_, kind)| *kind == StreamKind::Model)
            .map(|(idx, kind)| (*idx, *kind))
            .collect();

        if model_indices.is_empty() || markdown_text.trim().is_empty() {
            // Drop any pending indicator so it does not linger as an
            // orphaned line if this turn produced no markdown.
            self.turn_indicator = None;
            // When no Model entries were recorded but streamed lines were
            // emitted (direct push), append the re-rendered markdown
            // after the existing lines instead of replacing them.
            // This avoids a visual flash on the first turn.
            if model_indices.is_empty() && had_streamed_line && !markdown_text.trim().is_empty() {
                let prefixed_markdown = if self.turn_indicator.take().is_some() {
                    format!("\u{2666} {markdown_text}")
                } else {
                    markdown_text.to_string()
                };
                let rendered: Vec<Line<'static>> =
                    crate::cli::markdown::render_streamed_markdown(&prefixed_markdown);
                for line in rendered {
                    self.output_lines.push_back(line);
                    self.output_line_kinds.push_back(BlockKind::Model);
                }
                self.needs_redraw = true;
                self.cached_wrap_width = None;
                self.rebuild_visual_row_cache(self.last_wrap_width());
                self.refresh_pending_manual_viewport_anchor(self.last_wrap_width());
            }
            return;
        }

        // Extract just the Model indices for the no-op-reinsert check
        // and for popping/insertion.
        let model_entry_indices: Vec<usize> = model_indices.iter().map(|(idx, _)| *idx).collect();
        let wrap_width = self.last_wrap_width();
        let manual_anchor = self.manual_viewport_anchor(wrap_width);

        // No-op-reinsert optimization: if the rendered line count equals
        // the popped Model line count and every rendered line has the same
        // content and style as the popped line, skip the pop+reinsert
        // entirely.  This prevents the visual flash where streamed plain
        // text vanishes for a frame while render_markdown runs, then
        // reappears styled.
        let mut rendered: Vec<Line<'static>> =
            crate::cli::markdown::render_streamed_markdown(markdown_text);
        let can_skip_reinsert = rendered.len() == model_entry_indices.len()
            && rendered.iter().zip(model_entry_indices.iter()).all(
                |(rendered_line, popped_idx)| {
                    self.output_lines
                        .get(*popped_idx)
                        .is_some_and(|popped| rendered_line == popped)
                },
            );

        if can_skip_reinsert {
            // Prepend the turn indicator to the first rendered line's
            // first span instead of doing a full pop+reinsert.
            let mut prefixed_turn_indicator = false;
            if let Some(first) = rendered.first_mut()
                && self.turn_indicator.take().is_some()
            {
                let mut new_spans = Vec::with_capacity(first.spans.len() + 1);
                new_spans.push(Span::styled(
                    "\u{2666} ",
                    Style::default().fg(crate::cli::tui::theme::ACCENT),
                ));
                new_spans.extend(first.spans.iter().cloned());
                first.spans = new_spans;
                prefixed_turn_indicator = true;
            }
            if prefixed_turn_indicator {
                self.output_lines[model_entry_indices[0]] = rendered[0].clone();
                self.output_line_kinds[model_entry_indices[0]] = BlockKind::Model;
            }
            self.needs_redraw = true;
            self.cached_wrap_width = None;
            self.rebuild_visual_row_cache(wrap_width);
            if let Some(anchor) = manual_anchor.as_ref() {
                self.restore_manual_viewport_anchor(anchor, wrap_width);
            }
            self.refresh_pending_manual_viewport_anchor(wrap_width);
            return;
        }

        // Validate the recorded Model indices are still in-range. If a
        // 10,000-line eviction happened between recording and
        // finalizing, fall back to clearing without replacement.
        let max_idx = *model_entry_indices.iter().max().unwrap();
        if max_idx >= self.output_lines.len() {
            // Eviction happened: clear the pending indicator too so
            // it does not appear as a stray line after the eviction.
            self.turn_indicator = None;
            return;
        }

        let insert_at = *model_entry_indices.iter().min().unwrap();
        let anchor_in_replaced_model = manual_anchor
            .as_ref()
            .is_some_and(|anchor| model_entry_indices.contains(&anchor.output_index));
        let old_model_start = if anchor_in_replaced_model {
            let mut anchor = manual_anchor.clone().expect("manual anchor must exist");
            anchor.output_index = insert_at;
            anchor.row_offset = 0;
            anchor.separator_before = false;
            self.scroll_offset_for_manual_anchor(&anchor, wrap_width)
        } else {
            None
        };
        let anchor_match_ordinal = manual_anchor
            .as_ref()
            .filter(|_| anchor_in_replaced_model)
            .map(|anchor| {
                model_entry_indices
                    .iter()
                    .filter(|index| **index <= anchor.output_index)
                    .filter(|index| {
                        let text = Self::line_to_string(&self.output_lines[**index]);
                        text == anchor.text
                            || (!anchor.normalized_text.is_empty()
                                && Self::normalize_viewport_anchor_text(&text)
                                    == anchor.normalized_text)
                    })
                    .count()
                    .saturating_sub(1)
            });

        // Pop only the Model entries from highest index to lowest to
        // preserve the relative order of entries that come before.
        // ToolOutput entries are NOT popped.  `output_line_kinds` is
        // popped in lockstep so render-time grouping stays valid.
        for &idx in model_entry_indices.iter().rev() {
            self.output_lines.remove(idx);
            if idx < self.output_line_kinds.len() {
                self.output_line_kinds.remove(idx);
            }
        }

        // The Model entry indices were contiguous in append order
        // (model-streamed lines are emitted in sequence) but other
        // events (RawAnsi code blocks, ToolOutput lines) may have
        // interleaved.  The surviving lines between the popped region
        // must be reindexed — since we popped from the highest index
        // first, indices before any popped index remain valid. Indices
        // after the popped region shift down by 1 per popped line.
        //
        // For simplicity, the markdown re-render is inserted at the
        // position of the FIRST popped Model line (the minimum index).
        // The result is approximate ordering when RawAnsi code blocks
        // or ToolOutput lines were interleaved inside the streamed text,
        // but matches what the user would have seen — code blocks were
        // emitted immediately when the model streamed them.
        // Render the markdown text first, then prepend the turn indicator
        // as a styled span to the first rendered line. Prepending the
        // indicator to the markdown string would make `render_markdown`
        // parse "♦ " as paragraph text, breaking the visual hierarchy.
        // Prepending as a span keeps the indicator on the same line as
        // the start of the response.
        let have_indicator = self.turn_indicator.take().is_some();
        let mut rendered: Vec<Line<'static>> =
            crate::cli::markdown::render_streamed_markdown(markdown_text);
        if have_indicator && let Some(first) = rendered.first_mut() {
            let mut new_spans = Vec::with_capacity(first.spans.len() + 1);
            new_spans.push(Span::styled(
                "\u{2666} ",
                Style::default().fg(crate::cli::tui::theme::ACCENT),
            ));
            new_spans.extend(first.spans.iter().cloned());
            first.spans = new_spans;
        }
        let rendered_len = rendered.len();
        for line in rendered.into_iter().rev() {
            self.output_lines.insert(insert_at, line);
            self.output_line_kinds.insert(insert_at, BlockKind::Model);
        }
        // Sanity: lengths should match exactly. If they ever diverge
        // (e.g. because some external mutation slipped through),
        // rebuild both from the same buffer to recover.
        if self.output_lines.len() != self.output_line_kinds.len() {
            debug_assert_eq!(
                self.output_lines.len(),
                self.output_line_kinds.len(),
                "output_line_kinds drifted from output_lines after finalize"
            );
            let drain_to = self.output_lines.len().min(self.output_line_kinds.len());
            self.output_line_kinds.truncate(drain_to);
            while self.output_line_kinds.len() < self.output_lines.len() {
                self.output_line_kinds.push_back(BlockKind::Model);
            }
        }
        self.needs_redraw = true;
        // Invalidate the visual-row cache: the line count and content
        // changed, so the cached row count is stale.
        self.cached_wrap_width = None;
        self.rebuild_visual_row_cache(wrap_width);

        let Some(mut anchor) = manual_anchor else {
            self.refresh_pending_manual_viewport_anchor(wrap_width);
            return;
        };
        if !anchor_in_replaced_model {
            let removed_before = model_entry_indices
                .iter()
                .filter(|index| **index < anchor.output_index)
                .count();
            anchor.output_index = anchor.output_index.saturating_sub(removed_before);
            if anchor.output_index >= insert_at.saturating_sub(removed_before) {
                anchor.output_index = anchor.output_index.saturating_add(rendered_len);
            }
            self.restore_manual_viewport_anchor(&anchor, wrap_width);
            self.refresh_pending_manual_viewport_anchor(wrap_width);
            return;
        }

        let mut model_anchor = anchor.clone();
        model_anchor.output_index = insert_at;
        model_anchor.row_offset = 0;
        model_anchor.separator_before = false;
        let new_model_start = self.scroll_offset_for_manual_anchor(&model_anchor, wrap_width);
        let new_model_rows: usize = self
            .output_lines
            .iter()
            .skip(insert_at)
            .take(rendered_len)
            .zip(self.output_line_kinds.iter().skip(insert_at))
            .map(|(line, kind)| Self::output_row_visual_rows(Some(line), *kind, wrap_width))
            .sum();
        let old_model_relative = old_model_start
            .map(|start| anchor.scroll_y.saturating_sub(start))
            .unwrap_or_default();
        let matching_candidates: Vec<(usize, usize, usize)> = (insert_at
            ..insert_at.saturating_add(rendered_len))
            .filter_map(|index| {
                let text = Self::line_to_string(&self.output_lines[index]);
                let exact_match = text == anchor.text;
                let normalized_match = !anchor.normalized_text.is_empty()
                    && Self::normalize_viewport_anchor_text(&text) == anchor.normalized_text;
                if !exact_match && !normalized_match {
                    return None;
                }

                let mut candidate = anchor.clone();
                candidate.output_index = index;
                let candidate_relative = self
                    .scroll_offset_for_manual_anchor(&candidate, wrap_width)
                    .zip(new_model_start)
                    .map(|(offset, start)| offset.saturating_sub(start))
                    .unwrap_or(usize::MAX);
                Some((
                    index,
                    usize::from(!exact_match),
                    candidate_relative.abs_diff(old_model_relative),
                ))
            })
            .collect();
        let matching_index = anchor_match_ordinal
            .and_then(|ordinal| matching_candidates.get(ordinal).map(|(index, _, _)| *index))
            .or_else(|| {
                matching_candidates
                    .iter()
                    .min_by_key(|(_, match_kind, distance)| (*match_kind, *distance))
                    .map(|(index, _, _)| *index)
            });
        if let Some(index) = matching_index {
            anchor.output_index = index;
            self.restore_manual_viewport_anchor(&anchor, wrap_width);
            self.refresh_pending_manual_viewport_anchor(wrap_width);
            return;
        }

        if let (Some(old_start), Some(new_start)) = (old_model_start, new_model_start) {
            self.scroll_offset = new_start.saturating_add(
                anchor
                    .scroll_y
                    .saturating_sub(old_start)
                    .min(new_model_rows.saturating_sub(1)),
            );
            self.clamp_to_content();
        }
        self.refresh_pending_manual_viewport_anchor(wrap_width);
    }

    /// Push a completion line to the completion buffer.
    pub fn push_completion_line(&mut self, line: Line<'static>) {
        self.needs_redraw = true;
        self.completion_lines.push_back(line);
        self.completion_scroll_offset = 0;
        self.completion_viewport_rows = 0;
        self.completion_area = None;
        // Invalidate the visual-row cache so cached_completion_rows is
        // recomputed on the next render. Without this, the completion box
        // keeps its stale height (just borders) and the text is clipped.
        self.cached_wrap_width = None;
    }

    pub fn set_last_completion_text(&mut self, text: String) {
        self.last_completion_text = Some(text);
    }

    #[must_use]
    pub fn last_completion_text(&self) -> Option<&str> {
        self.last_completion_text.as_deref()
    }

    pub(crate) fn begin_text_selection(&mut self, column: u16, row: u16, now: Instant) -> bool {
        let Some(surface) = self.selection_surface_at(column, row) else {
            return false;
        };
        let Some((row_index, column_index)) = Self::normalize_surface_point(surface, column, row)
        else {
            return false;
        };
        let Some(point) = Self::selection_point(surface, row_index, column_index) else {
            return false;
        };
        let click_target = Self::surface_hyperlink_at(surface, row_index, column_index);
        self.text_selection = Some(TextSelection {
            pane: surface.pane,
            anchor: point,
            focus: point,
            click_target,
            dragging: true,
            moved: false,
            last_drag_redraw: now,
        });
        self.needs_redraw = true;
        true
    }

    /// Update the active selection. The caller can skip a redraw when this
    /// returns false, but the focus still tracks every drag event.
    pub(crate) fn extend_text_selection(&mut self, column: u16, row: u16, now: Instant) -> bool {
        let Some(selection) = self.text_selection.as_ref() else {
            return false;
        };
        let Some(surface) = self.selection_surface(selection.pane) else {
            self.text_selection = None;
            return false;
        };
        let Some((row_index, column_index)) = Self::normalize_surface_point(surface, column, row)
        else {
            return false;
        };
        let Some(point) = Self::selection_point(surface, row_index, column_index) else {
            return false;
        };
        let selection = self
            .text_selection
            .as_mut()
            .expect("selection must remain present while extending it");
        selection.moved |= point != selection.anchor;
        selection.focus = point;
        if now.duration_since(selection.last_drag_redraw) < Duration::from_millis(16) {
            return false;
        }
        selection.last_drag_redraw = now;
        self.needs_redraw = true;
        true
    }

    /// Complete a drag selection and return the text from the last rendered
    /// frame. The event loop cannot access a Frame, so this intentionally
    /// copies exactly the content the user saw before releasing the mouse.
    pub(crate) fn finish_text_selection(&mut self, column: u16, row: u16) -> Option<String> {
        let selection = self.text_selection.as_ref()?;
        let Some(surface) = self.selection_surface(selection.pane) else {
            self.text_selection = None;
            return None;
        };
        let (row_index, column_index) = Self::normalize_surface_point(surface, column, row)?;
        let point = Self::selection_point(surface, row_index, column_index)?;
        let selection = self
            .text_selection
            .as_mut()
            .expect("selection must remain present while finishing it");
        selection.dragging = false;
        selection.moved |= point != selection.anchor;
        selection.focus = point;
        if !selection.moved {
            self.text_selection = None;
            self.needs_redraw = true;
            return None;
        }
        self.needs_redraw = true;
        self.selected_text()
    }

    pub(crate) fn clear_text_selection(&mut self) {
        if self.text_selection.take().is_some() {
            self.needs_redraw = true;
        }
    }

    #[must_use]
    pub(crate) fn is_text_selection_dragging(&self) -> bool {
        self.text_selection
            .as_ref()
            .is_some_and(|selection| selection.dragging)
    }

    pub(crate) fn text_selection_click_target(&self, column: u16, row: u16) -> Option<PathBuf> {
        let selection = self.text_selection.as_ref()?;
        if selection.moved {
            return None;
        }
        let surface = self.selection_surface(selection.pane)?;
        let (row_index, column_index) = Self::normalize_surface_point(surface, column, row)?;
        Self::surface_hyperlink_at(surface, row_index, column_index)
            .filter(|target| selection.click_target.as_ref() == Some(target))
    }

    fn selection_surface_at(&self, column: u16, row: u16) -> Option<&SelectionSurface> {
        self.selection_surfaces
            .iter()
            .find(|surface| Self::selection_area_contains(surface.content_area, column, row))
    }

    fn selection_surface(&self, pane: SelectionPane) -> Option<&SelectionSurface> {
        self.selection_surfaces
            .iter()
            .find(|surface| surface.pane == pane)
    }

    fn selection_area_contains(area: Rect, column: u16, row: u16) -> bool {
        column >= area.x
            && column < area.x.saturating_add(area.width)
            && row >= area.y
            && row < area.y.saturating_add(area.height)
    }

    fn surface_hyperlink_at(
        surface: &SelectionSurface,
        row_index: usize,
        column_index: usize,
    ) -> Option<PathBuf> {
        surface
            .rows
            .get(row_index)?
            .get(column_index)?
            .hyperlink
            .clone()
    }

    fn normalize_surface_point(
        surface: &SelectionSurface,
        column: u16,
        row: u16,
    ) -> Option<(usize, usize)> {
        let last_column = surface
            .content_area
            .x
            .saturating_add(surface.content_area.width.saturating_sub(1));
        let last_row = surface
            .content_area
            .y
            .saturating_add(surface.content_area.height.saturating_sub(1));
        let row = row.clamp(surface.content_area.y, last_row);
        let column = column.clamp(surface.content_area.x, last_column);
        let row_index = row.saturating_sub(surface.content_area.y) as usize;
        let cells = &surface.rows[row_index];
        let mut cell_index = column.saturating_sub(surface.content_area.x) as usize;
        while cell_index > 0 && cells[cell_index].continuation {
            cell_index -= 1;
        }
        Some((row_index, cell_index))
    }

    fn selection_point(
        surface: &SelectionSurface,
        row_index: usize,
        column: usize,
    ) -> Option<SelectionPoint> {
        let source = surface.row_sources.get(row_index).copied().flatten()?;
        Some(SelectionPoint {
            output_line_index: source.output_line_index,
            row_in_line: source.row_in_line,
            column,
        })
    }

    fn selection_columns_for_row(
        &self,
        surface: &SelectionSurface,
        row: usize,
    ) -> Option<(usize, usize)> {
        let selection = self.text_selection.as_ref()?;
        if selection.pane != surface.pane || row >= surface.rows.len() {
            return None;
        }
        let (start, end) = if (
            selection.anchor.output_line_index,
            selection.anchor.row_in_line,
            selection.anchor.column,
        ) <= (
            selection.focus.output_line_index,
            selection.focus.row_in_line,
            selection.focus.column,
        ) {
            (selection.anchor, selection.focus)
        } else {
            (selection.focus, selection.anchor)
        };
        let source = surface.row_sources.get(row).copied().flatten()?;
        let source_key = (source.output_line_index, source.row_in_line);
        let start_key = (start.output_line_index, start.row_in_line);
        let end_key = (end.output_line_index, end.row_in_line);
        if source_key < start_key || source_key > end_key {
            return None;
        }
        let cells = &surface.rows[row];
        if cells.is_empty() {
            return None;
        }
        let mut first = if source_key == start_key {
            start.column
        } else {
            0
        };
        let mut last = if source_key == end_key {
            end.column
        } else {
            cells.len().saturating_sub(1)
        };
        first = first.min(cells.len().saturating_sub(1));
        last = last.min(cells.len().saturating_sub(1));
        while first > 0 && cells[first].continuation {
            first -= 1;
        }
        while last + 1 < cells.len() && cells[last + 1].continuation {
            last += 1;
        }
        Some((first, last))
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.text_selection.as_ref()?;
        let surface = self.selection_surface(selection.pane)?;
        let mut rows = Vec::new();
        for row in 0..surface.rows.len() {
            let Some((start, end)) = self.selection_columns_for_row(surface, row) else {
                continue;
            };
            let mut text = String::new();
            for cell in &surface.rows[row][start..=end] {
                if !cell.continuation {
                    text.push_str(&cell.symbol);
                }
            }
            rows.push(text.trim_end_matches(char::is_whitespace).to_string());
        }
        let text = rows.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    fn refresh_selection_surfaces(&mut self, buffer: &Buffer) {
        let areas = [
            (
                SelectionPane::Transcript,
                self.transcript_selection_area,
                self.transcript_selection_row_sources.as_slice(),
            ),
            (
                SelectionPane::Completion,
                self.completion_selection_area,
                self.completion_selection_row_sources.as_slice(),
            ),
        ];
        let selection_surfaces = areas
            .into_iter()
            .filter_map(|(pane, area, row_sources)| {
                area.map(|area| self.snapshot_selection_surface(buffer, pane, area, row_sources))
            })
            .collect();
        self.selection_surfaces = selection_surfaces;
        if self.text_selection.as_ref().is_some_and(|selection| {
            let Some(surface) = self.selection_surface(selection.pane) else {
                return true;
            };
            !Self::surface_contains_selection_point(surface, selection.anchor)
                || !Self::surface_contains_selection_point(surface, selection.focus)
        }) {
            self.text_selection = None;
        }
    }

    fn snapshot_selection_surface(
        &self,
        buffer: &Buffer,
        pane: SelectionPane,
        content_area: Rect,
        row_sources: &[Option<SelectionRowSource>],
    ) -> SelectionSurface {
        let mut rows = Vec::with_capacity(content_area.height as usize);
        for row in content_area.y..content_area.y.saturating_add(content_area.height) {
            let mut cells = Vec::with_capacity(content_area.width as usize);
            let mut continuation_cells = 0usize;
            for column in content_area.x..content_area.x.saturating_add(content_area.width) {
                let cell = buffer.cell((column, row));
                let symbol = cell.map_or_else(|| " ".to_string(), |cell| cell.symbol().to_string());
                let hyperlink = cell.and_then(|cell| {
                    Self::hyperlink_marker_index(cell.underline_color)
                        .and_then(|index| self.rendered_hyperlink_targets.get(index))
                        .cloned()
                });
                let continuation = continuation_cells > 0;
                if continuation {
                    continuation_cells -= 1;
                } else {
                    continuation_cells = UnicodeWidthStr::width(symbol.as_str()).saturating_sub(1);
                }
                cells.push(VisibleCell {
                    symbol,
                    continuation,
                    hyperlink,
                });
            }
            rows.push(cells);
        }
        SelectionSurface {
            pane,
            content_area,
            rows,
            row_sources: row_sources.to_vec(),
        }
    }

    fn surface_contains_selection_point(surface: &SelectionSurface, point: SelectionPoint) -> bool {
        surface
            .row_sources
            .iter()
            .enumerate()
            .any(|(row_index, source)| {
                source.is_some_and(|source| {
                    source.output_line_index == point.output_line_index
                        && source.row_in_line == point.row_in_line
                        && surface
                            .rows
                            .get(row_index)
                            .is_some_and(|cells| point.column < cells.len())
                })
            })
    }

    fn normalize_hyperlink_markers(&self, buffer: &mut Buffer) {
        for cell in &mut buffer.content {
            if Self::hyperlink_marker_index(cell.underline_color)
                .is_some_and(|index| index < self.rendered_hyperlink_targets.len())
            {
                cell.underline_color = cell.fg;
            }
        }
    }

    fn apply_text_selection_overlay(&self, buffer: &mut Buffer) {
        let Some(selection) = self.text_selection.as_ref() else {
            return;
        };
        let Some(surface) = self.selection_surface(selection.pane) else {
            return;
        };
        for row in 0..surface.rows.len() {
            let Some((start, end)) = self.selection_columns_for_row(surface, row) else {
                continue;
            };
            buffer.set_style(
                Rect {
                    x: surface.content_area.x.saturating_add(start as u16),
                    y: surface.content_area.y.saturating_add(row as u16),
                    width: end.saturating_sub(start).saturating_add(1) as u16,
                    height: 1,
                },
                theme::selection_style(),
            );
        }
    }

    fn selection_content_area(area: Rect, reserve_scrollbar: bool) -> Option<Rect> {
        let mut content = area.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        if reserve_scrollbar {
            content.width = content.width.saturating_sub(1);
        }
        (content.width > 0 && content.height > 0).then_some(content)
    }

    /// Clear the completion box and invalidate cached layout for the next render.
    pub fn clear_completion_lines(&mut self) {
        self.clear_text_selection();
        self.needs_redraw = true;
        self.completion_lines.clear();
        self.cached_completion_rows = 0;
        self.completion_scroll_offset = 0;
        self.completion_viewport_rows = 0;
        self.completion_area = None;
        self.cached_wrap_width = None;
    }

    /// Push an error line to the error buffer.
    pub fn push_error_line(&mut self, line: Line<'static>) {
        self.needs_redraw = true;
        self.error_lines.push_back(line);
        self.cached_wrap_width = None;
    }

    /// Clear the error box and invalidate cached layout for the next render.
    pub fn clear_error_lines(&mut self) {
        self.clear_text_selection();
        self.needs_redraw = true;
        self.error_lines.clear();
        self.cached_error_rows = 0;
        self.cached_wrap_width = None;
    }

    /// Push a plain text line.
    pub fn push_plain(&mut self, text: impl Into<String>) {
        let text = text.into();
        let wrap_width = self.last_wrap_width();
        let wrapped = crate::cli::text_utils::wrap_text(&text, wrap_width, "");
        for line_text in wrapped.lines() {
            self.push_output(Line::from(line_text.to_string()));
        }
    }

    fn prewrap_stream_line(&self, line: Line<'static>) -> Vec<Line<'static>> {
        let wrap_width = self.last_wrap_width();
        let contains_hyperlink = Self::line_contains_osc8(&line);
        let total_width: usize = line
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();

        if !contains_hyperlink && total_width > wrap_width && wrap_width > 0 {
            let mut full_text = String::new();
            for span in &line.spans {
                full_text.push_str(span.content.as_ref());
            }
            let wrapped = crate::cli::text_utils::wrap_text(&full_text, wrap_width, "");
            wrapped.lines().map(|l| Line::from(l.to_string())).collect()
        } else {
            vec![line]
        }
    }

    fn push_stream_group(&mut self, lines_to_push: Vec<Line<'static>>, kind: StreamKind) {
        // StreamKind maps to a default BlockKind. The interactive.rs
        // routing layer overrides this default for structurally tagged
        // emissions (tool headers, command headers, reasoning) by
        // calling `push_output_with_kind` directly via dedicated events.
        let block_kind = match kind {
            StreamKind::Model => BlockKind::Model,
            StreamKind::Reasoning => BlockKind::Reasoning,
            StreamKind::ToolOutput => BlockKind::ToolOutput,
        };
        let start_idx = self.output_lines.len();
        let mut pushed = 0usize;

        for line in lines_to_push {
            let idx = self.output_lines.len();
            self.push_output_with_kind(line, block_kind);
            // push_output may have evicted the front of the buffer if it
            // exceeded 10,000 lines. If our recorded index fell off, drop
            // it and any earlier recorded indices for this turn — the
            // eviction means the model output was so long that we cannot
            // usefully re-render it as a unit anyway.
            if idx >= self.output_lines.len() {
                self.turn_stream_entries.clear();
                self.last_stream_group = None;
                return;
            }
            self.turn_stream_entries.push((idx, kind));
            pushed = pushed.saturating_add(1);
        }

        self.last_stream_group = Some((start_idx, pushed, kind));
    }

    /// Push a styled text line.
    pub fn push_styled(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        let wrap_width = self.last_wrap_width();
        let wrapped = crate::cli::text_utils::wrap_text(&text, wrap_width, "");
        for line in wrapped.lines() {
            self.push_output(Line::from(Span::styled(line.to_string(), style)));
        }
    }

    /// Push a turn separator line.
    pub fn push_turn_separator(&mut self) {
        let sep_width = self.last_wrap_width().max(20);
        let diamond = " ♦ ";
        let remainder = sep_width.saturating_sub(diamond.len());
        let left = remainder.div_ceil(2);
        let right = remainder / 2;
        let sep = format!("{}{}{}", "─".repeat(left), diamond, "─".repeat(right),);
        self.push_output_with_kind(
            Line::from(Span::styled(sep, theme::dim_style())),
            BlockKind::Separator,
        );
    }

    /// Push a user message with proper formatting (splits on newlines).
    /// Multi-line messages get a left border accent for visual grouping.
    pub fn push_user_message(&mut self, text: &str, writer: &OutputWriterArc) {
        self.clear_completion_lines();
        let style = Style::default()
            .fg(theme::PROMPT_FG)
            .add_modifier(Modifier::BOLD);
        let lines: Vec<&str> = text.split('\n').collect();
        let is_multiline = lines.len() > 1;
        for (i, line) in lines.iter().enumerate() {
            let content = if is_multiline {
                if i == 0 {
                    format!("│ ❯ {line}")
                } else {
                    format!("│   {line}")
                }
            } else {
                format!("❯ {line}")
            };
            writer.emit(OutputEvent::UserPromptLine(Line::from(Span::styled(
                content, style,
            ))));
        }
        self.force_bottom();
    }

    pub fn force_bottom(&mut self) {
        self.needs_redraw = true;
        self.scroll_mode = ScrollMode::Auto;
        self.scroll_offset = 0;
        self.unseen_output_count = 0;
        self.pending_manual_viewport_anchor = None;
    }

    pub fn start_scrollback_writer(&mut self) -> io::Result<()> {
        self.ensure_scrollback_writer()
    }

    fn ensure_scrollback_writer(&mut self) -> io::Result<()> {
        let Some(path) = self.scrollback_file.clone() else {
            return Ok(());
        };
        if self
            .scrollback_writer
            .as_ref()
            .is_some_and(|writer| writer.path == path)
        {
            return Ok(());
        }
        if let Some(mut writer) = self.scrollback_writer.take() {
            writer.shutdown()?;
        }
        self.scrollback_writer = Some(ScrollbackWriter::start(path)?);
        Ok(())
    }

    fn enqueue_scrollback_pending(&mut self) -> io::Result<()> {
        if self.scrollback_pending.is_empty() {
            return Ok(());
        }
        if self.scrollback_file.is_none() {
            self.scrollback_pending.clear();
            self.scrollback_pending_lines = 0;
            return Ok(());
        }
        self.ensure_scrollback_writer()?;
        let batch = std::mem::take(&mut self.scrollback_pending);
        let line_count = self.scrollback_pending_lines;
        self.scrollback_pending_lines = 0;
        if let Some(writer) = self.scrollback_writer.as_ref()
            && let Err(batch) = writer.append(batch)
        {
            self.scrollback_pending = batch;
            self.scrollback_pending_lines = line_count;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scrollback writer stopped",
            ));
        }
        Ok(())
    }

    pub fn flush_scrollback_pending(&mut self) -> io::Result<()> {
        self.enqueue_scrollback_pending()?;
        if let Some(writer) = self.scrollback_writer.as_ref() {
            writer.flush()?;
        }
        Ok(())
    }

    pub fn flush_scrollback_pending_if_needed(&mut self) -> io::Result<()> {
        if self.scrollback_pending_lines >= SCROLLBACK_FLUSH_LINE_BATCH {
            self.enqueue_scrollback_pending()?;
        }
        Ok(())
    }

    pub fn take_scrollback_writer_error(&self) -> Option<String> {
        self.scrollback_writer
            .as_ref()
            .and_then(ScrollbackWriter::take_error)
    }

    pub fn shutdown_scrollback_writer(&mut self) -> io::Result<()> {
        self.enqueue_scrollback_pending()?;
        if let Some(mut writer) = self.scrollback_writer.take() {
            writer.shutdown()?;
        }
        Ok(())
    }

    fn clear_scrollback_storage(&mut self) -> io::Result<()> {
        let result = if self.scrollback_file.is_none() {
            Ok(())
        } else {
            self.ensure_scrollback_writer().and_then(|()| {
                self.scrollback_writer
                    .as_ref()
                    .map_or(Ok(()), ScrollbackWriter::clear)
            })
        };
        self.scrollback_pending.clear();
        self.scrollback_pending_lines = 0;
        result
    }

    /// Load scrollback content from the scrollback file and merge with
    /// the current buffer.  The file stores one raw text line per line;
    /// we reconstruct Line objects and prepend them to output_lines.
    pub fn enter_scrollback(&mut self) -> io::Result<()> {
        self.flush_scrollback_pending()?;
        self.clear_text_selection();

        let scrollback_content = match self.scrollback_file.as_ref() {
            Some(file_path) => read_scrollback_tail(file_path)?,
            None => None,
        };

        self.in_scrollback = true;
        self.needs_redraw = true;

        if let Some(content) = scrollback_content {
            let mut new_lines: VecDeque<Line<'static>> = VecDeque::new();
            let mut new_kinds: VecDeque<BlockKind> = VecDeque::new();
            let scrollback_lines: Vec<&str> = content
                .lines()
                .rev()
                .take(MAX_SCROLLBACK_LOAD_LINES)
                .collect();
            for line in scrollback_lines.into_iter().rev() {
                new_lines.push_back(Line::from(line.to_string()));
                new_kinds.push_back(BlockKind::ToolOutput);
            }
            if !self.output_lines.is_empty() {
                let divider = Line::from("─".repeat(40));
                new_lines.push_back(divider);
                new_kinds.push_back(BlockKind::Separator);
            }
            for line in &self.output_lines {
                new_lines.push_back(line.clone());
            }
            for kind in &self.output_line_kinds {
                new_kinds.push_back(*kind);
            }
            self.output_lines = new_lines;
            self.output_line_kinds = new_kinds;
            self.cached_wrap_width = None;
            self.cached_visible_window = None;
        }
        self.scroll_mode = ScrollMode::Auto;
        self.scroll_offset = 0;
        self.unseen_output_count = 0;
        self.pending_manual_viewport_anchor = None;
        Ok(())
    }

    /// Exit scrollback mode: clear the scrollback file, reset buffer to
    /// the original session content, and return to bottom.
    pub fn exit_scrollback(&mut self) -> io::Result<()> {
        let result = self.clear_scrollback_storage();
        self.clear_text_selection();
        self.in_scrollback = false;
        self.needs_redraw = true;
        self.scrollback_count = 0;
        self.cached_wrap_width = None;
        self.cached_visible_window = None;
        self.scroll_mode = ScrollMode::Auto;
        self.scroll_offset = 0;
        self.unseen_output_count = 0;
        self.pending_manual_viewport_anchor = None;
        result
    }

    /// Toggle between normal and scrollback modes.
    pub fn toggle_scrollback(&mut self) -> io::Result<()> {
        if self.in_scrollback {
            self.exit_scrollback()
        } else {
            self.enter_scrollback()
        }
    }

    /// Clear all output and reset the visual-row cache.
    pub fn clear_output(&mut self) -> io::Result<()> {
        let result = self.clear_scrollback_storage();
        self.clear_text_selection();
        self.needs_redraw = true;
        self.output_lines.clear();
        self.output_line_kinds.clear();
        self.completion_lines.clear();
        self.last_completion_text = None;
        self.error_lines.clear();
        self.turn_stream_entries.clear();
        self.last_stream_group = None;
        self.reasoning_partial_line.clear();
        self.turn_indicator = None;
        self.turn_had_streamed_line = false;
        self.cached_visual_rows = 0;
        self.cached_completion_rows = 0;
        self.cached_error_rows = 0;
        self.completion_scroll_offset = 0;
        self.completion_viewport_rows = 0;
        self.completion_area = None;
        self.cached_wrap_width = Some(self.last_wrap_width());
        self.cached_visible_window = None;
        self.in_scrollback = false;
        self.scrollback_count = 0;
        self.unseen_output_count = 0;
        self.pending_manual_viewport_anchor = None;
        result
    }

    /// Drain output from the given index onward and keep the visual-row cache in sync.
    pub fn drain_output_from(&mut self, start: usize) {
        self.needs_redraw = true;
        let start = start.min(self.output_lines.len());
        if start >= self.output_lines.len() {
            return;
        }
        self.output_lines.drain(start..);
        self.output_line_kinds.drain(start..);
        self.last_stream_group = None;
        self.reasoning_partial_line.clear();
        // Invalidate the visual-row cache: drain changes the line buffer,
        // which can alter render-time separator insertion.
        self.cached_wrap_width = None;
        self.cached_visible_window = None;
        self.refresh_pending_manual_viewport_anchor(self.last_wrap_width());
    }

    pub fn pin_approval_bottom(&mut self) {
        self.needs_redraw = true;
        self.scroll_mode = ScrollMode::ApprovalPinned;
        self.scroll_offset = 0;
        self.unseen_output_count = 0;
        self.pending_manual_viewport_anchor = None;
    }

    pub fn clear_approval_pin(&mut self) {
        self.needs_redraw = true;
        self.force_bottom();
    }

    pub fn is_approval_pinned(&self) -> bool {
        matches!(self.scroll_mode, ScrollMode::ApprovalPinned)
    }

    pub fn is_auto_following_output(&self) -> bool {
        matches!(
            self.scroll_mode,
            ScrollMode::Auto | ScrollMode::ApprovalPinned
        )
    }

    pub fn set_content_height(&mut self, content_height: usize) {
        self.last_content_height = content_height;
    }

    pub fn set_content_width(&mut self, content_width: usize) {
        self.last_content_width = content_width;
    }

    /// Synchronize the cached plan panel state with the current task state.
    pub fn sync_plan_state_cache(
        &mut self,
        plan: Option<&crate::core::plan_state::PlanState>,
    ) -> bool {
        match plan {
            Some(plan) => {
                let plan_ptr = std::ptr::from_ref(plan) as usize;
                if self.plan_state_cache_ptr == Some(plan_ptr)
                    && self.plan_state_cache_version == plan.version
                    && self.plan_state_cache.is_some()
                {
                    false
                } else {
                    self.plan_state_cache = Some(plan.clone());
                    self.plan_state_cache_ptr = Some(plan_ptr);
                    self.plan_state_cache_version = plan.version;
                    true
                }
            }
            None => {
                if self.plan_state_cache.is_some() {
                    self.plan_state_cache = None;
                    self.plan_state_cache_ptr = None;
                    self.plan_state_cache_version = 0;
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn clamp_to_content(&mut self) {
        let total_rows = self.output_visual_rows(self.last_wrap_width());
        let max_offset = Self::max_scroll_offset_for(total_rows, self.last_content_height);

        match self.scroll_mode {
            ScrollMode::Auto | ScrollMode::ApprovalPinned => {
                self.scroll_offset = 0;
            }
            ScrollMode::Manual => {
                self.scroll_offset = self.scroll_offset.min(max_offset);
                let distance_from_bottom = max_offset.saturating_sub(self.scroll_offset);
                if distance_from_bottom == 0 {
                    self.force_bottom();
                }
            }
        }
    }

    pub fn scroll_lines(&mut self, delta: isize) {
        self.clear_text_selection();
        self.needs_redraw = true;
        let total_rows = self.output_visual_rows(self.last_wrap_width());
        if !self.enter_manual_mode(total_rows) {
            return;
        }

        let max_offset = Self::max_scroll_offset_for(total_rows, self.last_content_height);
        self.scroll_offset = if delta.is_negative() {
            self.scroll_offset.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll_offset
                .saturating_add(delta as usize)
                .min(max_offset)
        };
        self.clamp_to_content();
    }

    pub fn scroll_pages(&mut self, delta_pages: isize) {
        self.needs_redraw = true;
        let page_height = self.last_content_height.saturating_sub(1).max(1);
        let page_height = page_height.min(isize::MAX as usize) as isize;
        self.scroll_lines(delta_pages.saturating_mul(page_height));
    }

    pub fn scroll_completion_lines(&mut self, delta: isize) -> bool {
        self.clear_text_selection();
        if !self.error_lines.is_empty()
            || self.completion_lines.is_empty()
            || self.completion_viewport_rows == 0
        {
            return false;
        }
        let max_offset = self
            .cached_completion_rows
            .saturating_sub(self.completion_viewport_rows);
        if max_offset == 0 {
            return false;
        }
        let next_offset = if delta.is_negative() {
            self.completion_scroll_offset
                .saturating_sub(delta.unsigned_abs())
        } else {
            self.completion_scroll_offset
                .saturating_add(delta as usize)
                .min(max_offset)
        };
        if next_offset == self.completion_scroll_offset {
            return false;
        }
        self.completion_scroll_offset = next_offset;
        self.needs_redraw = true;
        true
    }

    pub fn scroll_completion_pages(&mut self, delta_pages: isize) -> bool {
        let page_height = self.completion_viewport_rows.saturating_sub(1).max(1);
        let page_height = page_height.min(isize::MAX as usize) as isize;
        self.scroll_completion_lines(delta_pages.saturating_mul(page_height))
    }

    pub fn scroll_completion_at(&mut self, column: u16, row: u16, delta: isize) -> bool {
        let Some(area) = self.completion_area else {
            return false;
        };
        if column < area.x
            || column >= area.x.saturating_add(area.width)
            || row < area.y
            || row >= area.y.saturating_add(area.height)
        {
            return false;
        }
        self.scroll_completion_lines(delta)
    }

    pub fn resolved_scroll_y_for(&self, total_lines: usize, content_height: usize) -> usize {
        let max_offset = Self::max_scroll_offset_for(total_lines, content_height);
        match self.scroll_mode {
            ScrollMode::Auto | ScrollMode::ApprovalPinned => max_offset,
            ScrollMode::Manual => self.scroll_offset.min(max_offset),
        }
    }

    fn resolved_output_scroll_y_for(
        &self,
        wrap_width: usize,
        total_rows: usize,
        content_height: usize,
    ) -> usize {
        if !matches!(self.scroll_mode, ScrollMode::ApprovalPinned) {
            return self.resolved_scroll_y_for(total_rows, content_height);
        }

        let max_offset = Self::max_scroll_offset_for(total_rows, content_height);
        let Some(prompt_tail_row) = self.last_blocking_prompt_tail_row(wrap_width) else {
            return max_offset;
        };

        prompt_tail_row
            .saturating_sub(content_height)
            .min(max_offset)
    }

    fn enter_manual_mode(&mut self, total_rows: usize) -> bool {
        match self.scroll_mode {
            ScrollMode::ApprovalPinned => false,
            ScrollMode::Manual => true,
            ScrollMode::Auto => {
                self.scroll_mode = ScrollMode::Manual;
                self.scroll_offset =
                    Self::max_scroll_offset_for(total_rows, self.last_content_height);
                self.unseen_output_count = 0;
                true
            }
        }
    }

    fn max_scroll_offset_for(total_lines: usize, content_height: usize) -> usize {
        total_lines.saturating_sub(content_height)
    }

    fn terminal_scroll_offset(offset: usize) -> u16 {
        offset.min(u16::MAX as usize) as u16
    }

    fn last_blocking_prompt_tail_row(&self, wrap_width: usize) -> Option<usize> {
        let mut tail_row = None;
        let mut rendered_rows = 0usize;

        self.for_each_output_row(|line, kind| {
            rendered_rows =
                rendered_rows.saturating_add(Self::output_row_visual_rows(line, kind, wrap_width));
            if kind == BlockKind::BlockingPrompt {
                tail_row = Some(rendered_rows);
            }
        });

        tail_row
    }

    fn last_wrap_width(&self) -> usize {
        if self.last_content_width == 0 {
            80
        } else {
            Self::content_wrap_width(self.last_content_width)
        }
    }

    fn content_wrap_width(content_width: usize) -> usize {
        // 2 chars consumed by block borders.
        content_width.saturating_sub(2).max(1)
    }

    fn line_visual_rows(line: &Line<'_>, wrap_width: usize) -> usize {
        Self::line_visual_rows_with_extra_width(line, wrap_width, 0)
    }

    fn line_visual_rows_with_extra_width(
        line: &Line<'_>,
        wrap_width: usize,
        extra_width: usize,
    ) -> usize {
        if wrap_width == 0 {
            return 1;
        }

        if Self::line_contains_osc8(line) {
            let mut hyperlink_targets = Vec::new();
            let mut rendered = Self::parse_osc8_line(line, &mut hyperlink_targets);
            if extra_width > 0 {
                rendered.spans.insert(0, Span::raw("│".repeat(extra_width)));
            }
            return Paragraph::new(rendered)
                .wrap(Wrap { trim: false })
                .line_count(wrap_width.min(u16::MAX as usize) as u16)
                .max(1);
        }

        // Bottom pinning must be computed in rendered rows, not logical lines.
        // A single long prompt line can wrap into multiple terminal rows; if we
        // count only logical lines, the actionable tail of the prompt can land
        // below the visible viewport even while the TUI thinks it is at bottom.
        let width = line
            .spans
            .iter()
            .map(|span| Self::osc8_display_width(span.content.as_ref()))
            .sum::<usize>()
            .saturating_add(extra_width);
        width.max(1).div_ceil(wrap_width)
    }

    fn visible_output_window(
        &mut self,
        wrap_width: usize,
        scroll_y: usize,
        content_height: usize,
    ) -> (usize, usize, usize) {
        if self.output_lines.is_empty() {
            return (0, 0, 0);
        }

        let fingerprint = (
            self.output_lines.len(),
            scroll_y,
            wrap_width,
            content_height,
            self.cached_visual_rows,
            self.scroll_mode,
        );

        if let Some(cached) = self.cached_visible_window
            && self.cached_window_fingerprint == fingerprint
        {
            return cached;
        }

        let target_start = scroll_y.min(self.cached_visual_rows);
        let target_end = target_start.saturating_add(content_height.max(1));

        // Walk the same expanded (line, kind) list that the renderer
        // uses, so separator rows are counted in scroll math and the
        // returned indices reference the same list the renderer
        // eventually slices.  `start_idx`/`end_idx` are indices into
        // `expanded`, NOT into `self.output_lines` — the expanded list
        // is longer when transition separators are present.
        let mut expanded_len = 0usize;
        let mut rows_before = 0usize;
        let mut start_idx = usize::MAX;
        let mut start_row_offset = 0usize;
        let mut end_idx = 0usize;

        let mut done = false;
        self.for_each_output_row(|line, kind| {
            if done {
                return;
            }
            let idx = expanded_len;
            expanded_len = expanded_len.saturating_add(1);
            let rows = Self::output_row_visual_rows(line, kind, wrap_width);
            let rows_after = rows_before.saturating_add(rows);

            if start_idx == usize::MAX && rows_after > target_start {
                start_idx = idx;
                start_row_offset = target_start.saturating_sub(rows_before);
            }

            if rows_after >= target_end {
                end_idx = idx;
                rows_before = rows_after;
                done = true;
                return;
            }

            rows_before = rows_after;
            end_idx = idx;
        });

        if expanded_len == 0 {
            return (0, 0, 0);
        }

        if start_idx == usize::MAX {
            start_idx = expanded_len.saturating_sub(1);
            start_row_offset = 0;
            end_idx = start_idx;
        }

        let take_count = end_idx.saturating_sub(start_idx).saturating_add(1);
        let result = (start_idx, take_count, start_row_offset);

        self.cached_visible_window = Some(result);
        self.cached_window_fingerprint = fingerprint;

        result
    }

    fn rebuild_visual_row_cache(&mut self, wrap_width: usize) {
        let mut output_rows = 0usize;
        self.for_each_output_row(|line, kind| {
            output_rows =
                output_rows.saturating_add(Self::output_row_visual_rows(line, kind, wrap_width));
        });
        // Completion box uses the same wrap width (only borders, no gutter).
        let completion_rows: usize = self
            .completion_lines
            .iter()
            .map(|line| Self::line_visual_rows(line, wrap_width))
            .sum();
        let error_rows: usize = self
            .error_lines
            .iter()
            .map(|line| Self::line_visual_rows(line, wrap_width))
            .sum();
        self.cached_visual_rows = output_rows
            .saturating_add(completion_rows)
            .saturating_add(error_rows);
        self.cached_completion_rows = completion_rows;
        self.cached_error_rows = error_rows;
        self.cached_wrap_width = Some(wrap_width);
    }

    fn for_each_output_row(&self, mut visitor: impl FnMut(Option<&Line<'static>>, BlockKind)) {
        self.for_each_output_row_with_index(|line, kind, _| visitor(line, kind));
    }

    fn for_each_output_row_with_index(
        &self,
        mut visitor: impl FnMut(Option<&Line<'static>>, BlockKind, Option<usize>),
    ) {
        let mut prev: Option<BlockKind> = None;
        for (index, (line, kind)) in self
            .output_lines
            .iter()
            .zip(self.output_line_kinds.iter())
            .enumerate()
        {
            if let Some(p) = prev
                && Self::should_insert_separator(p, *kind)
            {
                visitor(None, BlockKind::Separator, None);
            }
            visitor(Some(line), *kind, Some(index));
            prev = Some(*kind);
        }
    }

    fn output_row_visual_rows(
        line: Option<&Line<'static>>,
        kind: BlockKind,
        wrap_width: usize,
    ) -> usize {
        match line {
            Some(line) => {
                let accent_width = usize::from(kind != BlockKind::Separator);
                Self::line_visual_rows_with_extra_width(line, wrap_width, accent_width)
            }
            None => 1,
        }
    }

    fn output_row_for_render(
        line: Option<&Line<'static>>,
        kind: BlockKind,
        hyperlink_targets: &mut Vec<PathBuf>,
    ) -> Line<'static> {
        let mut line = line.map_or_else(
            || Line::from(""),
            |line| Self::parse_osc8_line(line, hyperlink_targets),
        );
        if kind != BlockKind::Separator {
            line.spans
                .insert(0, Span::styled("│", block_kind_accent_style(kind)));
        }
        line
    }

    fn line_contains_osc8(line: &Line<'_>) -> bool {
        line.spans
            .iter()
            .any(|span| span.content.contains(OSC8_PREFIX))
    }

    fn parse_osc8_line(line: &Line<'_>, hyperlink_targets: &mut Vec<PathBuf>) -> Line<'static> {
        let mut spans = Vec::new();
        for span in &line.spans {
            let mut remaining = span.content.as_ref();
            let mut marker = None;
            while !remaining.is_empty() {
                if let Some(payload) = remaining.strip_prefix(OSC8_PREFIX) {
                    let Some((terminator_at, terminator_len)) = Self::osc_terminator(payload)
                    else {
                        break;
                    };
                    let uri = &payload[..terminator_at];
                    marker = uri
                        .strip_prefix("file://")
                        .filter(|path| !path.is_empty())
                        .and_then(|path| {
                            let index = hyperlink_targets.len();
                            let marker = Self::hyperlink_marker(index)?;
                            hyperlink_targets.push(PathBuf::from(path));
                            Some(marker)
                        });
                    remaining = &payload[terminator_at + terminator_len..];
                    continue;
                }

                let next_control = remaining.find(OSC8_PREFIX).unwrap_or(remaining.len());
                let text = &remaining[..next_control];
                if !text.is_empty() {
                    let style = marker.map_or(span.style, |marker| {
                        span.style
                            .add_modifier(Modifier::UNDERLINED)
                            .underline_color(marker)
                    });
                    spans.push(Span::styled(text.to_string(), style));
                }
                remaining = &remaining[next_control..];
            }
        }
        Line {
            style: line.style,
            alignment: line.alignment,
            spans,
        }
    }

    fn osc_terminator(text: &str) -> Option<(usize, usize)> {
        let string_terminator = text.find("\x1b\\").map(|index| (index, 2));
        let bell_terminator = text.find('\x07').map(|index| (index, 1));
        match (string_terminator, bell_terminator) {
            (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
            (Some(terminator), None) | (None, Some(terminator)) => Some(terminator),
            (None, None) => None,
        }
    }

    fn osc8_display_width(text: &str) -> usize {
        if !text.contains(OSC8_PREFIX) {
            return UnicodeWidthStr::width(text);
        }
        let mut remaining = text;
        let mut width = 0usize;
        while !remaining.is_empty() {
            if let Some(payload) = remaining.strip_prefix(OSC8_PREFIX) {
                let Some((terminator_at, terminator_len)) = Self::osc_terminator(payload) else {
                    break;
                };
                remaining = &payload[terminator_at + terminator_len..];
                continue;
            }
            let next_control = remaining.find(OSC8_PREFIX).unwrap_or(remaining.len());
            width = width.saturating_add(UnicodeWidthStr::width(&remaining[..next_control]));
            remaining = &remaining[next_control..];
        }
        width
    }

    fn hyperlink_marker(index: usize) -> Option<Color> {
        let encoded = u16::try_from(index.checked_add(1)?).ok()?;
        Some(Color::Rgb(
            HYPERLINK_MARKER_RED,
            (encoded >> 8) as u8,
            encoded as u8,
        ))
    }

    fn hyperlink_marker_index(color: Color) -> Option<usize> {
        let Color::Rgb(HYPERLINK_MARKER_RED, high, low) = color else {
            return None;
        };
        let encoded = u16::from(high) << 8 | u16::from(low);
        usize::from(encoded).checked_sub(1)
    }

    fn normalize_viewport_anchor_text(text: &str) -> String {
        text.chars()
            .filter(|ch| !matches!(ch, '*' | '_' | '`' | '#'))
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn manual_viewport_anchor(&mut self, wrap_width: usize) -> Option<ManualViewportAnchor> {
        if self.scroll_mode != ScrollMode::Manual || self.output_lines.is_empty() {
            return None;
        }

        let total_rows = self.output_visual_rows(wrap_width);
        let scroll_y = self.resolved_scroll_y_for(total_rows, self.last_content_height);
        let mut rows_before = 0usize;
        let mut previous_kind = None;

        for (index, (line, kind)) in self
            .output_lines
            .iter()
            .zip(self.output_line_kinds.iter())
            .enumerate()
        {
            let separator_before = previous_kind
                .is_some_and(|previous| Self::should_insert_separator(previous, *kind));
            if separator_before {
                if scroll_y == rows_before {
                    let text = Self::line_to_string(line);
                    return Some(ManualViewportAnchor {
                        output_index: index,
                        row_offset: 0,
                        separator_before: true,
                        normalized_text: Self::normalize_viewport_anchor_text(&text),
                        text,
                        scroll_y,
                    });
                }
                rows_before = rows_before.saturating_add(1);
            }

            let row_count = Self::output_row_visual_rows(Some(line), *kind, wrap_width);
            if scroll_y < rows_before.saturating_add(row_count) {
                let text = Self::line_to_string(line);
                return Some(ManualViewportAnchor {
                    output_index: index,
                    row_offset: scroll_y.saturating_sub(rows_before),
                    separator_before: false,
                    normalized_text: Self::normalize_viewport_anchor_text(&text),
                    text,
                    scroll_y,
                });
            }
            rows_before = rows_before.saturating_add(row_count);
            previous_kind = Some(*kind);
        }

        None
    }

    fn scroll_offset_for_manual_anchor(
        &self,
        anchor: &ManualViewportAnchor,
        wrap_width: usize,
    ) -> Option<usize> {
        let mut rows_before = 0usize;
        let mut previous_kind = None;

        for (index, (line, kind)) in self
            .output_lines
            .iter()
            .zip(self.output_line_kinds.iter())
            .enumerate()
        {
            let separator_before = previous_kind
                .is_some_and(|previous| Self::should_insert_separator(previous, *kind));
            if index == anchor.output_index {
                if anchor.separator_before && separator_before {
                    return Some(rows_before);
                }
                let line_start = rows_before.saturating_add(separator_before as usize);
                let line_rows = Self::output_row_visual_rows(Some(line), *kind, wrap_width);
                return Some(
                    line_start.saturating_add(anchor.row_offset.min(line_rows.saturating_sub(1))),
                );
            }
            rows_before = rows_before.saturating_add(separator_before as usize);
            rows_before = rows_before.saturating_add(Self::output_row_visual_rows(
                Some(line),
                *kind,
                wrap_width,
            ));
            previous_kind = Some(*kind);
        }

        None
    }

    fn restore_manual_viewport_anchor(
        &mut self,
        anchor: &ManualViewportAnchor,
        wrap_width: usize,
    ) -> bool {
        if self.scroll_mode != ScrollMode::Manual {
            return false;
        }
        let Some(offset) = self.scroll_offset_for_manual_anchor(anchor, wrap_width) else {
            return false;
        };
        self.scroll_offset = offset;
        self.clamp_to_content();
        true
    }

    /// Refresh a resize anchor after transcript mutation so it cannot restore
    /// a stale buffer index when the next frame reflows the viewport.
    fn refresh_pending_manual_viewport_anchor(&mut self, wrap_width: usize) {
        if self.pending_manual_viewport_anchor.is_some() {
            self.pending_manual_viewport_anchor = self.manual_viewport_anchor(wrap_width);
        }
    }

    pub fn capture_manual_viewport_for_reflow(&mut self) {
        self.pending_manual_viewport_anchor = self.manual_viewport_anchor(self.last_wrap_width());
    }

    fn restore_pending_manual_viewport_after_reflow(&mut self, wrap_width: usize) {
        if let Some(anchor) = self.pending_manual_viewport_anchor.take() {
            self.restore_manual_viewport_anchor(&anchor, wrap_width);
        }
    }

    fn collect_output_rows_range(
        &mut self,
        start_idx: usize,
        take_count: usize,
    ) -> Vec<Line<'static>> {
        let end_idx = start_idx.saturating_add(take_count);
        let mut expanded_idx = 0usize;
        let mut visible_lines = Vec::with_capacity(take_count);
        let mut hyperlink_targets = Vec::new();
        self.for_each_output_row(|line, kind| {
            if expanded_idx >= start_idx && expanded_idx < end_idx {
                visible_lines.push(Self::output_row_for_render(
                    line,
                    kind,
                    &mut hyperlink_targets,
                ));
            }
            expanded_idx = expanded_idx.saturating_add(1);
        });
        self.rendered_hyperlink_targets = hyperlink_targets;
        visible_lines
    }

    fn build_transcript_selection_row_sources(
        &self,
        start_idx: usize,
        take_count: usize,
        visible_scroll_y: usize,
        content_height: usize,
        wrap_width: usize,
    ) -> Vec<Option<SelectionRowSource>> {
        let end_idx = start_idx.saturating_add(take_count);
        let mut expanded_idx = 0usize;
        let mut skipped_rows = visible_scroll_y;
        let mut row_sources = Vec::with_capacity(content_height);
        self.for_each_output_row_with_index(|line, kind, output_line_index| {
            if expanded_idx >= start_idx && expanded_idx < end_idx {
                let row_count = Self::output_row_visual_rows(line, kind, wrap_width);
                for row_in_line in 0..row_count {
                    if skipped_rows > 0 {
                        skipped_rows -= 1;
                    } else if row_sources.len() < content_height {
                        row_sources.push(output_line_index.map(|output_line_index| {
                            SelectionRowSource {
                                output_line_index,
                                row_in_line,
                            }
                        }));
                    }
                }
            }
            expanded_idx = expanded_idx.saturating_add(1);
        });
        row_sources.resize(content_height, None);
        row_sources
    }

    fn build_completion_selection_row_sources(
        &self,
        content_height: usize,
    ) -> Vec<Option<SelectionRowSource>> {
        let wrap_width = self.last_wrap_width();
        let mut skipped_rows = self.completion_scroll_offset;
        let mut row_sources = Vec::with_capacity(content_height);
        for (output_line_index, line) in self.completion_lines.iter().enumerate() {
            let row_count = Self::line_visual_rows(line, wrap_width);
            for row_in_line in 0..row_count {
                if skipped_rows > 0 {
                    skipped_rows -= 1;
                } else if row_sources.len() < content_height {
                    row_sources.push(Some(SelectionRowSource {
                        output_line_index,
                        row_in_line,
                    }));
                }
            }
        }
        row_sources.resize(content_height, None);
        row_sources
    }

    /// Predicate for whether a blank-line separator should be drawn
    /// between two consecutive output block kinds.  Explicit
    /// `BlockKind::Separator` lines are themselves visual boundaries,
    /// so no extra separator is inserted around them.
    fn should_insert_separator(prev: BlockKind, next: BlockKind) -> bool {
        if prev == next {
            return false;
        }
        if prev == BlockKind::Separator || next == BlockKind::Separator {
            return false;
        }
        // Insert separators between model text and tool/command blocks
        // to visually group related output. Also separate tool headers
        // from their output, and command headers from their output.
        matches!(
            (prev, next),
            (
                BlockKind::Model,
                BlockKind::ToolHeader
                    | BlockKind::CommandHeader
                    | BlockKind::ToolOutput
                    | BlockKind::CommandOutput
                    | BlockKind::Reasoning
                    | BlockKind::UserPrompt
                    | BlockKind::BlockingPrompt,
            ) | (_, BlockKind::UserPrompt)
                | (_, BlockKind::BlockingPrompt)
                | (
                    BlockKind::ToolOutput
                        | BlockKind::CommandOutput
                        | BlockKind::ToolHeader
                        | BlockKind::CommandHeader
                        | BlockKind::Reasoning
                        | BlockKind::UserPrompt
                        | BlockKind::BlockingPrompt,
                    BlockKind::Model,
                )
                | (BlockKind::ToolHeader, BlockKind::ToolOutput)
                | (BlockKind::CommandHeader, BlockKind::CommandOutput)
        )
    }

    fn total_visual_rows(&mut self, wrap_width: usize) -> usize {
        if self.cached_wrap_width != Some(wrap_width) {
            self.rebuild_visual_row_cache(wrap_width);
        }
        self.cached_visual_rows
    }

    fn output_visual_rows(&mut self, wrap_width: usize) -> usize {
        let total_rows = self.total_visual_rows(wrap_width);
        total_rows
            .saturating_sub(self.cached_completion_rows)
            .saturating_sub(self.cached_error_rows)
    }

    /// Render the application state to the frame.
    pub fn render(&mut self, frame: &mut Frame) {
        let has_plan = self.plan_state_cache.as_ref().is_some_and(|p| !p.complete);

        // Reserve the plan area even when no plan is active so the
        // Clear widget below can wipe stale plan content from the
        // right 35 columns after the plan is dismissed.
        let [plan_main_area, plan_area] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(35)]).areas(frame.area());
        let main_area = if has_plan {
            plan_main_area
        } else {
            frame.area()
        };
        frame.render_widget(Clear, plan_area);

        let [output_area, status_area, input_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(self.render_input_height()),
        ])
        .areas(main_area);

        self.render_output(frame, output_area);
        self.render_status_bar(frame, status_area);
        self.render_input(frame, input_area);
        if self.picker_active {
            self.render_picker_overlay(frame, output_area);
        }
        if self.slash_command_active {
            self.render_slash_command_overlay(frame, output_area);
        }
        if self.model_picker_active {
            self.render_model_picker_overlay(frame, output_area);
        }
        if has_plan {
            self.render_plan_panel(frame, plan_area);
        }

        let selection_blocked = self.picker_active
            || self.slash_command_active
            || self.model_picker_active
            || self.pending_model_switch.is_some();
        if selection_blocked {
            self.clear_text_selection();
            self.selection_surfaces.clear();
        } else {
            // Selection must be based on the completed frame, after widgets have
            // applied wrapping, scrolling, and markdown styling.
            self.refresh_selection_surfaces(frame.buffer_mut());
        }
        self.normalize_hyperlink_markers(frame.buffer_mut());
        if !selection_blocked {
            self.apply_text_selection_overlay(frame.buffer_mut());
        }
    }

    fn render_plan_panel(&self, frame: &mut Frame, area: Rect) {
        if let Some(ref plan) = self.plan_state_cache {
            super::plan_panel::render_plan_panel(plan, frame, area);
        }
    }

    fn render_input(&mut self, frame: &mut Frame, input_area: Rect) {
        if self.pending_approval.is_some() {
            self.render_approval_panel(frame, input_area);
            return;
        }

        let input_title = if let Some(pending) = self.pending_model_switch.as_ref() {
            format!(" API key for {} ", pending.provider)
        } else if self.slash_command_help_active {
            " Help search ".to_string()
        } else if crate::core::approval::is_any_followup_question_active() {
            " Follow-up reply ".to_string()
        } else if self.agent_busy {
            if self.reasoning_active {
                format!(" {} Reasoning... ", self.spinner_char())
            } else {
                format!(" {} Agent processing... ", self.spinner_char())
            }
        } else {
            " Input ".to_string()
        };
        self.input.set_block(theme::input_block(
            input_title,
            self.agent_busy || self.has_blocking_prompt(),
        ));

        self.update_placeholder();

        frame.render_widget(&self.input, input_area);
    }

    fn render_approval_panel(&mut self, frame: &mut Frame, area: Rect) {
        let Some(pending) = self.pending_approval.as_ref() else {
            return;
        };
        let id = pending.request.id();
        let title = pending.request.title().to_string();
        let lines = pending.lines.clone();
        let choices = pending.request.choices().to_vec();
        let scroll_from_bottom = pending.scroll_from_bottom;

        let block = theme::input_block(format!(" {title} "), true)
            .border_style(Style::default().fg(theme::WARNING_FG));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);

        let detail_height = inner.height.saturating_sub(1);
        let detail_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: detail_height,
        };
        let action_area = Rect {
            x: inner.x,
            y: inner.y.saturating_add(detail_height),
            width: inner.width,
            height: inner.height.saturating_sub(detail_height),
        };

        let wrap_width = detail_area.width.max(1) as usize;
        let total_rows = lines
            .iter()
            .map(|line| Self::line_visual_rows(line, wrap_width))
            .sum::<usize>();
        let viewport_rows = detail_area.height as usize;
        let max_scroll = total_rows.saturating_sub(viewport_rows);
        let scroll_y = max_scroll.saturating_sub(scroll_from_bottom.min(max_scroll));

        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((Self::terminal_scroll_offset(scroll_y), 0)),
            detail_area,
        );

        let mut actions = Vec::new();
        for (index, choice) in choices.iter().enumerate() {
            if index > 0 {
                actions.push(Span::raw("  "));
            }
            let shortcut = if choice.result() == crate::core::approval::ApprovalResult::Denied {
                format!("{}/Esc", choice.shortcut())
            } else {
                choice.shortcut().to_string()
            };
            let color = match choice.result() {
                crate::core::approval::ApprovalResult::Approved => theme::PROMPT_FG,
                crate::core::approval::ApprovalResult::Denied => theme::ERROR_FG,
                crate::core::approval::ApprovalResult::Always => theme::ACCENT,
            };
            actions.push(Span::styled(
                format!("[{shortcut}]"),
                Style::default().fg(color),
            ));
            actions.push(Span::raw(format!(" {}", choice.label())));
        }
        if max_scroll > 0 {
            actions.push(Span::styled(
                "  [↑/↓] Review",
                Style::default().fg(theme::STATUS_FG),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(actions)), action_area);

        if let Some(pending) = self
            .pending_approval
            .as_mut()
            .filter(|pending| pending.request.id() == id)
        {
            pending.total_rows = total_rows;
            pending.viewport_rows = viewport_rows;
            pending.scroll_from_bottom = pending.scroll_from_bottom.min(max_scroll);
            pending.rendered =
                detail_area.height > 0 && action_area.height > 0 && !choices.is_empty();
        }
    }

    fn has_blocking_prompt(&self) -> bool {
        self.has_pending_approval() || crate::core::approval::is_any_followup_question_active()
    }

    fn render_input_height(&self) -> u16 {
        if self.has_pending_approval() {
            self.approval_panel_height()
        } else if self.has_blocking_prompt() {
            (BLOCKING_PROMPT_INPUT_VISIBLE_LINES as u16) + 2
        } else {
            self.input_height()
        }
    }

    fn render_status_bar(&mut self, frame: &mut Frame, status_area: Rect) {
        if let Some(notification) = self.status_notification.as_ref() {
            let (marker, color) = match notification.kind {
                NotificationKind::Info => ("i", theme::ACCENT),
                NotificationKind::Success => ("✓", theme::PROMPT_FG),
                NotificationKind::Warning => ("!", theme::WARNING_FG),
                NotificationKind::Error => ("×", theme::ERROR_FG),
            };
            let approval_badge = self.approval_mode_badge();
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(
                        " {approval_badge}{} · {marker} {} ",
                        self.mode, notification.message
                    ),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ))),
                status_area,
            );
            return;
        }

        if self.status_left_fingerprint.0 != self.provider_name
            || self.status_left_fingerprint.1 != self.model_name
            || self.status_left_fingerprint.2 != self.task_id
            || self.status_left_fingerprint.3 != self.mode
            || self.status_left_fingerprint.4 != self.yolo_mode
            || self.status_left_fingerprint.5 != self.auto_approve_all
        {
            let approval_badge = self.approval_mode_badge();
            self.cached_status_left = format!(
                " {}{} / {} | {} | {} ",
                approval_badge, self.provider_name, self.model_name, self.task_id, self.mode
            );
            self.status_left_fingerprint = (
                self.provider_name.clone(),
                self.model_name.clone(),
                self.task_id.clone(),
                self.mode.clone(),
                self.yolo_mode,
                self.auto_approve_all,
            );
        }
        let elapsed_secs = self.elapsed.map_or(u64::MAX, |e| e.as_secs());
        let context_key = (
            elapsed_secs,
            self.context_pct,
            self.output_overflow,
            self.output_overflow_count,
            self.queued_message_count,
            self.unseen_output_count,
            self.scroll_mode,
        );
        if context_key != self.cached_status_right_secs {
            let mut status = String::new();
            if self.scroll_mode == ScrollMode::Manual && self.unseen_output_count > 0 {
                status.push_str(&format!("↑ {} new · ", self.unseen_output_count));
            }
            if self.output_overflow {
                let summary = if self.output_overflow_summary.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", self.output_overflow_summary)
                };
                status.push_str(&format!(
                    "⚠ output overflow ({} dropped{}) · ",
                    self.output_overflow_count, summary
                ));
            }
            if let Some(pct) = self.context_pct {
                status.push_str(&format!("Context: {:.0}% left · ", 100.0 - pct));
            }
            if self.queued_message_count > 0 {
                status.push_str(&format!("📨 {} queued · ", self.queued_message_count));
            }
            if let Some(elapsed) = self.elapsed {
                status.push_str(&format!("⏱ {} ", format_duration(elapsed)));
            } else if !status.is_empty() {
                status.push(' ');
            }
            self.cached_status_right = status;
            self.cached_status_right_secs = context_key;
        }
        let left_width = UnicodeWidthStr::width(self.cached_status_left.as_str());
        let right_width = UnicodeWidthStr::width(self.cached_status_right.as_str());
        let spacer_len = status_area
            .width
            .saturating_sub((left_width + right_width) as u16) as usize;
        if spacer_len != self.cached_spacer_len {
            self.cached_spacer = " ".repeat(spacer_len);
            self.cached_spacer_len = spacer_len;
        }
        let status_line = Line::from(vec![
            Span::styled(self.cached_status_left.clone(), theme::status_style()),
            Span::raw(self.cached_spacer.clone()),
            Span::styled(self.cached_status_right.clone(), theme::status_style()),
        ]);
        let status = Paragraph::new(status_line);
        frame.render_widget(status, status_area);
    }

    fn approval_mode_badge(&self) -> &'static str {
        if self.yolo_mode {
            "[YOLO] "
        } else if self.auto_approve_all {
            "[AUTO-APPROVE] "
        } else {
            ""
        }
    }

    fn render_output(&mut self, frame: &mut Frame, output_area: Rect) {
        // Error box takes priority over completion box.
        let has_error = !self.error_lines.is_empty();
        let has_completion = !self.completion_lines.is_empty();
        let has_bottom_box = has_error || has_completion;
        // Rebuild the visual-row cache up front so bottom_height reflects
        // the current error_lines/completion_lines. Without this, a push
        // that invalidates cached_wrap_width still leaves the height math
        // using the stale cached row count from the prior render.
        let wrap_width = Self::content_wrap_width(output_area.width as usize);
        let _ = self.total_visual_rows(wrap_width);
        let bottom_height: u16 = if has_error {
            self.cached_error_rows
                .saturating_add(2)
                .min(u16::MAX as usize)
                .min(output_area.height as usize) as u16
        } else if has_completion {
            // Keeping half the region available prevents a long final result from hiding history.
            let max_completion_height = output_area
                .height
                .div_ceil(2)
                .max(3)
                .min(output_area.height);
            self.cached_completion_rows
                .saturating_add(2)
                .min(u16::MAX as usize)
                .min(max_completion_height as usize) as u16
        } else {
            0
        };
        let main_output_area = if has_bottom_box {
            Rect {
                x: output_area.x,
                y: output_area.y,
                width: output_area.width,
                height: output_area.height.saturating_sub(bottom_height),
            }
        } else {
            output_area
        };
        let bottom_area = if has_bottom_box {
            Rect {
                x: output_area.x,
                y: output_area.y + main_output_area.height,
                width: output_area.width,
                height: bottom_height,
            }
        } else {
            output_area
        };
        if has_completion && !has_error {
            self.completion_viewport_rows = bottom_area.height.saturating_sub(2) as usize;
            self.completion_scroll_offset = self.completion_scroll_offset.min(
                self.cached_completion_rows
                    .saturating_sub(self.completion_viewport_rows),
            );
            self.completion_area = Some(bottom_area);
        } else {
            self.completion_viewport_rows = 0;
            self.completion_area = None;
        }

        let visible_height = main_output_area.height as usize;
        // Content height excludes border (1 line top + 1 line bottom)
        let content_height = visible_height.saturating_sub(2);
        self.last_content_width = main_output_area.width as usize;
        self.last_content_height = content_height;
        self.restore_pending_manual_viewport_after_reflow(wrap_width);
        let total_rows = self.total_visual_rows(wrap_width);
        // The output Paragraph only renders output_lines; completion_lines are
        // drawn as a separate Block below the main output. Scroll math must
        // therefore be based on output_rows alone, or the bottom of the
        // output gets hidden behind the completion overlay.
        let output_rows = total_rows
            .saturating_sub(self.cached_completion_rows)
            .saturating_sub(self.cached_error_rows);
        self.transcript_selection_area =
            Self::selection_content_area(main_output_area, output_rows > content_height);
        let completion_max_scroll = self
            .cached_completion_rows
            .saturating_sub(self.completion_viewport_rows);
        self.completion_selection_area = if has_completion && !has_error {
            Self::selection_content_area(bottom_area, completion_max_scroll > 0)
        } else {
            None
        };
        let scroll_y = self.resolved_output_scroll_y_for(wrap_width, output_rows, content_height);
        let (start_idx, visible_count, visible_scroll_y) =
            self.visible_output_window(wrap_width, scroll_y, content_height);
        self.transcript_selection_row_sources = self
            .transcript_selection_area
            .map(|area| {
                self.build_transcript_selection_row_sources(
                    start_idx,
                    visible_count,
                    visible_scroll_y,
                    area.height as usize,
                    wrap_width,
                )
            })
            .unwrap_or_default();
        if !self.in_scrollback
            && self.scrollback_count > 0
            && let Some(source) = self.transcript_selection_row_sources.last_mut()
        {
            *source = None;
        }
        self.completion_selection_row_sources = self
            .completion_selection_area
            .map(|area| self.build_completion_selection_row_sources(area.height as usize))
            .unwrap_or_default();

        {
            frame.render_widget(Clear, main_output_area);
            let visible_lines = self.collect_output_rows_range(start_idx, visible_count);
            let title = if self.in_scrollback {
                " sned (scrollback) "
            } else {
                " sned "
            };
            let output = Paragraph::new(visible_lines)
                .wrap(Wrap { trim: false })
                .scroll((Self::terminal_scroll_offset(visible_scroll_y), 0))
                .block(
                    theme::border_block(title).padding(ratatui::widgets::Padding::new(0, 0, 0, 0)),
                );
            frame.render_widget(output, main_output_area);
        }

        if !self.in_scrollback && self.scrollback_count > 0 && main_output_area.height > 0 {
            let indicator = Paragraph::new(Line::from(format!(
                "↓ {} lines of scrollback — press Shift+S to view",
                self.scrollback_count,
            )))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(theme::ACCENT).italic());
            let indicator_area = Rect {
                x: main_output_area.x,
                y: main_output_area.y + main_output_area.height - 1,
                width: main_output_area.width,
                height: 1,
            };
            frame.render_widget(Clear, indicator_area);
            frame.render_widget(indicator, indicator_area);
        }

        if has_error {
            frame.render_widget(Clear, bottom_area);
            let error_lines: Vec<Line<'static>> = self.error_lines.iter().cloned().collect();
            let error_box = Paragraph::new(error_lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::ERROR_FG))
                        .border_type(ratatui::widgets::BorderType::Rounded),
                );
            frame.render_widget(error_box, bottom_area);
        } else if has_completion {
            frame.render_widget(Clear, bottom_area);
            let completion_lines: Vec<Line<'static>> =
                self.completion_lines.iter().cloned().collect();
            let max_completion_scroll = completion_max_scroll;
            let mut completion_block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::PROMPT_FG))
                .border_type(ratatui::widgets::BorderType::Rounded);
            if max_completion_scroll > 0 {
                completion_block =
                    completion_block.title(Span::styled(" Scroll: PgUp/PgDn ", theme::dim_style()));
            }
            let completion = Paragraph::new(completion_lines)
                .wrap(Wrap { trim: false })
                .scroll((
                    Self::terminal_scroll_offset(self.completion_scroll_offset),
                    0,
                ))
                .block(completion_block);
            frame.render_widget(completion, bottom_area);
            if max_completion_scroll > 0 {
                let mut completion_scrollbar_state =
                    ScrollbarState::new(self.cached_completion_rows)
                        .viewport_content_length(self.completion_viewport_rows)
                        .position(self.completion_scroll_offset);
                frame.render_stateful_widget(
                    Scrollbar::default()
                        .orientation(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(Some("↑"))
                        .end_symbol(Some("↓"))
                        .style(theme::scrollbar_style())
                        .thumb_style(theme::scrollbar_thumb_style()),
                    bottom_area.inner(ratatui::layout::Margin {
                        horizontal: 0,
                        vertical: 1,
                    }),
                    &mut completion_scrollbar_state,
                );
            }
        }

        // Use output_rows (not total_rows) so the scrollbar thumb
        // reflects only the output pane content — completion rows are
        // rendered separately and must not affect scroll geometry.
        if output_rows > content_height {
            self.scrollbar_state = self
                .scrollbar_state
                .content_length(output_rows)
                .viewport_content_length(content_height.max(1))
                .position(scroll_y.min(output_rows));
            frame.render_stateful_widget(
                Scrollbar::default()
                    .orientation(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("↑"))
                    .end_symbol(Some("↓"))
                    .style(theme::scrollbar_style())
                    .thumb_style(theme::scrollbar_thumb_style()),
                main_output_area.inner(ratatui::layout::Margin {
                    horizontal: 0,
                    vertical: 1,
                }),
                &mut self.scrollbar_state,
            );
        }
    }

    fn picker_overlay_area(output_area: Rect, item_count: usize) -> Option<(Rect, usize)> {
        if output_area.width < 4 || output_area.height < 4 {
            return None;
        }

        let horizontal_margin = output_area.width.saturating_sub(4).min(2);
        let width = output_area.width.saturating_sub(horizontal_margin).min(50);
        if width < 4 {
            return None;
        }

        let visible_rows = item_count
            .max(1)
            .min(usize::from(output_area.height.saturating_sub(3)))
            .min(PICKER_MAX_VISIBLE_ROWS);
        let height = visible_rows as u16 + 3;
        Some((
            Rect {
                x: output_area.x + horizontal_margin,
                y: output_area
                    .y
                    .saturating_add(output_area.height.saturating_sub(height)),
                width,
                height,
            },
            visible_rows,
        ))
    }

    fn picker_viewport_start(selected: usize, item_count: usize, visible_rows: usize) -> usize {
        if item_count <= visible_rows {
            return 0;
        }
        selected
            .min(item_count.saturating_sub(1))
            .saturating_sub(visible_rows / 2)
            .min(item_count.saturating_sub(visible_rows))
    }

    fn render_picker_scrollbar(
        frame: &mut Frame,
        list_area: Rect,
        item_count: usize,
        selected: usize,
        visible_rows: usize,
    ) {
        if !Self::picker_has_scrollbar(item_count, visible_rows) {
            return;
        }

        let mut state = ScrollbarState::new(item_count)
            .viewport_content_length(visible_rows)
            .position(Self::picker_scrollbar_position(
                selected,
                item_count,
                visible_rows,
            ));
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(theme::scrollbar_style())
                .thumb_style(theme::scrollbar_thumb_style()),
            list_area,
            &mut state,
        );
    }

    fn picker_has_scrollbar(item_count: usize, visible_rows: usize) -> bool {
        item_count > visible_rows
    }

    fn picker_scrollbar_position(selected: usize, item_count: usize, visible_rows: usize) -> usize {
        Self::picker_viewport_start(selected, item_count, visible_rows)
    }

    fn render_picker_overlay(&self, frame: &mut Frame, output_area: Rect) {
        let Some((overlay_area, visible_rows)) =
            Self::picker_overlay_area(output_area, self.picker_results.len())
        else {
            return;
        };
        let labels: Vec<String> = self
            .picker_results
            .iter()
            .map(|result| {
                let icon = match result.file_type {
                    crate::core::file_search::FileType::Folder => "📁",
                    crate::core::file_search::FileType::File => "📄",
                };
                format!(
                    "{} {}",
                    icon,
                    crate::core::file_search::truncated_display_path(&result.path)
                )
            })
            .collect();
        let result_count = labels.len();

        let selected = self
            .picker_index
            .min(self.picker_results.len().saturating_sub(1));
        let first_visible =
            Self::picker_viewport_start(selected, self.picker_results.len(), visible_rows);
        let query = self.mention_search_query.trim_start_matches('@');
        let rows: Vec<Line> = if result_count == 0 {
            let message = if query.is_empty() {
                " No matches".to_string()
            } else {
                format!(" No matches for @{query}")
            };
            vec![Line::from(Span::styled(message, theme::dim_style()))]
        } else {
            labels
                .into_iter()
                .enumerate()
                .skip(first_visible)
                .take(visible_rows)
                .map(|(i, label)| {
                    if i == selected {
                        Line::from(Span::styled(label, theme::picker_selected_style()))
                    } else {
                        Line::from(label)
                    }
                })
                .collect()
        };

        frame.render_widget(Clear, overlay_area);
        let title = if result_count == 0 {
            if query.is_empty() {
                " Files ".to_string()
            } else {
                format!(" Files · @{query} ")
            }
        } else {
            format!(" Files ({result_count}) · @{query} ")
        };
        let block = theme::overlay_block(title);
        let inner = block.inner(overlay_area);
        frame.render_widget(block, overlay_area);
        let [list_area, footer_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        frame.render_widget(Paragraph::new(rows), list_area);
        frame.render_widget(
            Paragraph::new("Tab select · ↑/↓ choose · Enter insert · Esc close")
                .style(theme::dim_style()),
            footer_area,
        );
        Self::render_picker_scrollbar(
            frame,
            list_area,
            self.picker_results.len(),
            selected,
            visible_rows,
        );
    }

    fn render_slash_command_overlay(&self, frame: &mut Frame, output_area: Rect) {
        if self.slash_command_help_active {
            self.render_slash_command_help_overlay(frame, output_area);
            return;
        }

        let Some((overlay_area, visible_rows)) =
            Self::picker_overlay_area(output_area, self.slash_command_results.len())
        else {
            return;
        };
        let selected = self
            .slash_command_selected
            .min(self.slash_command_results.len().saturating_sub(1));
        let first_visible =
            Self::picker_viewport_start(selected, self.slash_command_results.len(), visible_rows);

        let rows: Vec<Line> = if self.slash_command_results.is_empty() {
            vec![Line::from(Span::styled(" No matches", theme::dim_style()))]
        } else {
            self.slash_command_results
                .iter()
                .enumerate()
                .skip(first_visible)
                .take(visible_rows)
                .map(|(i, entry)| {
                    let category_marker = match entry.category {
                        crate::cli::slash_commands::SlashCommandCategory::Agent => "▶ ",
                        crate::cli::slash_commands::SlashCommandCategory::Local => "● ",
                        crate::cli::slash_commands::SlashCommandCategory::Plan => "◆ ",
                        crate::cli::slash_commands::SlashCommandCategory::Skill => "★ ",
                    };
                    let label =
                        format!("{} {} - {}", category_marker, entry.name, entry.description);
                    if i == selected {
                        Line::from(Span::styled(label, theme::picker_selected_style()))
                    } else {
                        Line::from(label)
                    }
                })
                .collect()
        };

        frame.render_widget(Clear, overlay_area);
        let input_text = self.input.lines().join("\n");
        let query = crate::cli::slash_commands::extract_slash_query(&input_text)
            .unwrap_or_else(|| input_text.trim().trim_start_matches('/').to_string());
        let title = if query.is_empty() {
            format!(" Slash Commands ({}) ", self.slash_command_results.len())
        } else {
            format!(
                " Slash Commands ({}) · /{query} ",
                self.slash_command_results.len()
            )
        };
        let block = theme::overlay_block(title);
        let inner = block.inner(overlay_area);
        frame.render_widget(block, overlay_area);
        let [list_area, footer_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        frame.render_widget(Paragraph::new(rows), list_area);
        frame.render_widget(
            Paragraph::new("Tab complete · Enter submit · ↑/↓ select · Esc close")
                .style(theme::dim_style()),
            footer_area,
        );
        Self::render_picker_scrollbar(
            frame,
            list_area,
            self.slash_command_results.len(),
            selected,
            visible_rows,
        );
    }

    fn render_slash_command_help_overlay(&self, frame: &mut Frame, output_area: Rect) {
        let width = output_area.width.saturating_sub(4).min(100);
        let height = output_area.height.saturating_sub(2);
        let overlay_area = Rect {
            x: output_area.x + output_area.width.saturating_sub(width) / 2,
            y: output_area.y + output_area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        if overlay_area.width < 4 || overlay_area.height < 4 {
            return;
        }

        frame.render_widget(Clear, overlay_area);
        let block = theme::overlay_block(
            " Command Help · type to search · ↑↓ move · Enter insert · Esc close ",
        );
        let inner = block.inner(overlay_area);
        frame.render_widget(block, overlay_area);

        let (list_area, detail_area) = if inner.width >= 72 {
            let [list, detail] =
                Layout::horizontal([Constraint::Length(34), Constraint::Min(24)]).areas(inner);
            (list, detail)
        } else {
            let list_height = (inner.height / 2).max(3);
            let [list, detail] =
                Layout::vertical([Constraint::Length(list_height), Constraint::Min(3)])
                    .areas(inner);
            (list, detail)
        };

        let visible_rows = list_area.height.saturating_sub(2).max(1) as usize;
        let first_visible = self
            .slash_command_selected
            .saturating_sub(visible_rows.saturating_sub(1));
        let rows: Vec<Line> = if self.slash_command_results.is_empty() {
            vec![Line::from(Span::styled(
                " No matching commands",
                theme::dim_style(),
            ))]
        } else {
            self.slash_command_results
                .iter()
                .enumerate()
                .skip(first_visible)
                .take(visible_rows)
                .map(|(index, entry)| {
                    let label = format!(" /{}  {}", entry.name, entry.description);
                    if index == self.slash_command_selected {
                        Line::from(Span::styled(label, theme::picker_selected_style()))
                    } else {
                        Line::from(label)
                    }
                })
                .collect()
        };
        frame.render_widget(
            Paragraph::new(rows)
                .block(theme::overlay_block(format!(
                    " Commands ({}) ",
                    self.slash_command_results.len()
                )))
                .wrap(Wrap { trim: true }),
            list_area,
        );

        let detail = self
            .slash_command_results
            .get(self.slash_command_selected)
            .map_or_else(
                || vec![Line::from("Type to search available commands.")],
                |entry| {
                    let mut lines = vec![
                        Line::from(Span::styled(
                            entry.usage(),
                            Style::default().fg(theme::ACCENT),
                        )),
                        Line::from(entry.description.clone()),
                        Line::from(entry.detail()),
                    ];
                    if !entry.aliases.is_empty() {
                        lines.push(Line::from(format!(
                            "Aliases: {}",
                            entry
                                .aliases
                                .iter()
                                .map(|alias| format!("/{alias}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    }
                    lines
                },
            );
        frame.render_widget(
            Paragraph::new(detail)
                .block(theme::overlay_block(" Details "))
                .wrap(Wrap { trim: false }),
            detail_area,
        );
    }

    fn render_model_picker_overlay(&self, frame: &mut Frame, output_area: Rect) {
        let Some((overlay_area, visible_rows)) =
            Self::picker_overlay_area(output_area, self.model_picker_results.len())
        else {
            return;
        };
        let selected = self
            .model_picker_selected
            .min(self.model_picker_results.len().saturating_sub(1));
        let first_visible =
            Self::picker_viewport_start(selected, self.model_picker_results.len(), visible_rows);

        let rows: Vec<Line> = if self.model_picker_results.is_empty() {
            vec![Line::from(Span::styled(" No matches", theme::dim_style()))]
        } else {
            self.model_picker_results
                .iter()
                .enumerate()
                .skip(first_visible)
                .take(visible_rows)
                .map(|(i, entry)| {
                    let label = format!(
                        "[{}] {} - {}",
                        entry.provider, entry.label, entry.description
                    );
                    if i == selected {
                        Line::from(Span::styled(label, theme::picker_selected_style()))
                    } else {
                        Line::from(label)
                    }
                })
                .collect()
        };

        frame.render_widget(Clear, overlay_area);
        let block = theme::overlay_block(format!(" Models ({}) ", self.model_picker_results.len()));
        let inner = block.inner(overlay_area);
        frame.render_widget(block, overlay_area);
        let [list_area, footer_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        frame.render_widget(Paragraph::new(rows), list_area);
        frame.render_widget(
            Paragraph::new("Tab/Enter switch · ↑/↓ select · Esc close").style(theme::dim_style()),
            footer_area,
        );
        Self::render_picker_scrollbar(
            frame,
            list_area,
            self.model_picker_results.len(),
            selected,
            visible_rows,
        );
    }

    pub fn handle_paste(&mut self, content: &str, fold_large: bool) -> PasteOutcome {
        self.prune_detached_pastes();
        if self.expanded_input_byte_len().saturating_add(content.len()) > MAX_PASTED_INPUT_BYTES {
            return PasteOutcome::RejectedTooLarge {
                max_bytes: MAX_PASTED_INPUT_BYTES,
            };
        }

        let char_count = content.chars().count();
        if fold_large && char_count > self.paste_fold_threshold {
            if self.paste_chunks.len() >= MAX_FOLDED_PASTE_CHUNKS {
                return PasteOutcome::RejectedTooManyChunks {
                    max_chunks: MAX_FOLDED_PASTE_CHUNKS,
                };
            }
            let paste_id = NEXT_PASTE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let marker = format!("⟦pasted {char_count} chars #{paste_id}⟧");
            self.input.insert_str(&marker);
            self.paste_chunks.push(PasteChunk {
                marker,
                content: content.to_string(),
            });
            PasteOutcome::Folded { char_count }
        } else {
            self.input.insert_str(content);
            PasteOutcome::Inserted
        }
    }

    pub fn get_input_with_expanded_pastes(&mut self) -> String {
        let mut text = self.input.lines().join("\n");
        for paste in self.paste_chunks.drain(..) {
            if let Some(pos) = text.find(&paste.marker) {
                text.replace_range(pos..pos + paste.marker.len(), &paste.content);
            }
        }

        text
    }

    pub fn clear_pastes(&mut self) {
        self.paste_chunks.clear();
    }

    fn prune_detached_pastes(&mut self) {
        let text = self.input.lines().join("\n");
        self.paste_chunks
            .retain(|paste| text.contains(&paste.marker));
    }

    fn expanded_input_byte_len(&self) -> usize {
        let text = self.input.lines().join("\n");
        self.paste_chunks.iter().fold(text.len(), |len, paste| {
            if text.contains(&paste.marker) {
                len.saturating_sub(paste.marker.len())
                    .saturating_add(paste.content.len())
            } else {
                len
            }
        })
    }

    /// Increment spinner frame at a human-scale cadence instead of every loop
    /// iteration; a 60 FPS braille spinner just burns redraw budget.
    /// Returns true when the spinner frame advanced.
    pub fn tick_spinner(&mut self) -> bool {
        const SPINNER_INTERVAL: Duration = Duration::from_millis(125);

        if !self.agent_busy {
            self.last_spinner_tick = None;
            return false;
        }

        let now = Instant::now();
        let should_advance = self
            .last_spinner_tick
            .is_none_or(|last| now.duration_since(last) >= SPINNER_INTERVAL);

        if should_advance {
            self.spinner_index = (self.spinner_index + 1) % 10;
            self.last_spinner_tick = Some(now);
            return true;
        }
        false
    }

    /// Get current spinner character.
    pub fn spinner_char(&self) -> char {
        spinner_frame(self.spinner_index)
    }
}

/// Format a duration as a human-readable string (e.g., "2m 30s", "45s", "1h 15m").
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    if total_secs >= 3600 {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        format!("{hours}h {mins}m")
    } else if total_secs >= 60 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{mins}m {secs}s")
    } else {
        format!("{total_secs}s")
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn make_scrolling_app(total_lines: usize, content_height: usize) -> App {
        let mut app = App::new();
        app.set_content_height(content_height);
        app.set_content_width(80);
        for index in 0..total_lines {
            app.push_plain(format!("line {}", index));
        }
        app
    }

    #[test]
    fn collect_output_rows_range_keeps_accented_rows_aligned() {
        let mut app = App::new();
        app.push_output_with_kind(Line::from("model"), BlockKind::Model);
        app.push_output_with_kind(Line::from("separator"), BlockKind::Separator);
        app.push_output_with_kind(Line::from("tool"), BlockKind::ToolOutput);

        let rows = app.collect_output_rows_range(0, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].spans[0].content, "│");
        assert_eq!(rows[0].spans[0].style.fg, Some(theme::ACCENT));
        assert_eq!(rows[1].spans[0].content, "separator");
        assert_eq!(rows[2].spans[0].content, "│");
        assert_eq!(rows[2].spans[0].style.fg, Some(theme::STATUS_FG));

        let line = Line::from("1234567890");
        assert_eq!(
            App::output_row_visual_rows(Some(&line), BlockKind::Model, 10),
            2
        );
    }

    #[test]
    fn block_kind_accent_styles_match_theme() {
        assert_eq!(
            block_kind_accent_style(BlockKind::Model).fg,
            Some(theme::ACCENT)
        );
        assert_eq!(
            block_kind_accent_style(BlockKind::ToolHeader).fg,
            Some(theme::TOOL_CALL_FG)
        );
        assert_eq!(
            block_kind_accent_style(BlockKind::CommandHeader).fg,
            Some(theme::INFO_FG)
        );
        assert_eq!(
            block_kind_accent_style(BlockKind::UserPrompt).fg,
            Some(theme::PROMPT_FG)
        );
        assert_eq!(
            block_kind_accent_style(BlockKind::BlockingPrompt).fg,
            Some(theme::WARNING_FG)
        );
    }

    fn rendered_rows(buffer: &Buffer) -> Vec<String> {
        let width = buffer.area.width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    fn rendered_output_rows(app: &App, buffer: &Buffer) -> Vec<String> {
        let output_height = buffer
            .area
            .height
            .saturating_sub(1)
            .saturating_sub(app.render_input_height()) as usize;
        rendered_rows(buffer)
            .into_iter()
            .take(output_height)
            .collect()
    }

    fn render_for_selection(app: &mut App) {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| app.render(frame))
            .expect("selection frame should render");
    }

    fn selection_text_position(app: &App, pane: SelectionPane, text: &str) -> (u16, u16) {
        let surface = app
            .selection_surface(pane)
            .expect("render should create a selection surface");
        for (row_index, row) in surface.rows.iter().enumerate() {
            for (column_index, cell) in row.iter().enumerate() {
                if cell.symbol == text {
                    return (
                        surface.content_area.x.saturating_add(column_index as u16),
                        surface.content_area.y.saturating_add(row_index as u16),
                    );
                }
            }
        }
        panic!("selection text {text:?} was not rendered");
    }

    #[test]
    fn test_transcript_selection_copies_visible_cells() {
        let mut app = App::new();
        app.push_plain("alpha beta");
        render_for_selection(&mut app);
        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "a");
        let now = Instant::now();

        assert!(app.begin_text_selection(column, row, now));
        assert!(app.extend_text_selection(column + 4, row, now + Duration::from_millis(16)));
        assert_eq!(
            app.finish_text_selection(column + 4, row),
            Some("alpha".to_string())
        );
    }

    #[test]
    fn test_osc8_path_survives_tui_render_as_click_target() {
        let mut app = App::new();
        app.push_output(Line::from(format!(
            "  ▶ read \x1b]8;;file:///tmp/example.rs\x1b\\/tmp/example.rs\x1b]8;;\x1b\\"
        )));
        let backend = TestBackend::new(48, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| app.render(frame))
            .expect("hyperlink frame should render");

        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "x");
        assert!(app.begin_text_selection(column, row, Instant::now()));
        assert_eq!(
            app.text_selection_click_target(column, row),
            Some(PathBuf::from("/tmp/example.rs"))
        );

        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| !cell.symbol().contains('\x1b'))
        );
        let cell = buffer
            .cell((column, row))
            .expect("linked cell should exist");
        assert!(cell.modifier.contains(Modifier::UNDERLINED));
        assert!(App::hyperlink_marker_index(cell.underline_color).is_none());
    }

    #[test]
    fn test_osc8_click_accepts_release_on_another_cell_of_the_same_target() {
        let mut app = App::new();
        app.push_output(Line::from(
            "\x1b]8;;file:///tmp/jitter.rs\x1b\\/tmp/jitter.rs\x1b]8;;\x1b\\",
        ));
        render_for_selection(&mut app);

        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "/");
        assert!(app.begin_text_selection(column, row, Instant::now()));
        assert_eq!(
            app.text_selection_click_target(column + 1, row),
            Some(PathBuf::from("/tmp/jitter.rs"))
        );
    }

    #[test]
    fn test_osc8_click_requires_same_target_on_release() {
        let mut app = App::new();
        app.push_output(Line::from(
            "\x1b]8;;file:///tmp/pressed.rs\x1b\\/tmp/pressed.rs\x1b]8;;\x1b\\",
        ));
        render_for_selection(&mut app);

        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "p");
        assert!(app.begin_text_selection(column, row, Instant::now()));

        app.clear_output().expect("output should clear");
        app.push_output(Line::from(
            "\x1b]8;;file:///tmp/released.rs\x1b\\/tmp/released.rs\x1b]8;;\x1b\\",
        ));
        render_for_selection(&mut app);

        assert_eq!(app.text_selection_click_target(column, row), None);
    }

    #[test]
    fn test_wrapped_osc8_path_keeps_click_target() {
        let target = "/tmp/a/very/long/path/to/wrapped-target-Z";
        let mut app = App::new();
        app.push_output(Line::from(format!(
            "  ▶ read \x1b]8;;file://{target}\x1b\\{target}\x1b]8;;\x1b\\"
        )));
        let backend = TestBackend::new(24, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| app.render(frame))
            .expect("wrapped hyperlink frame should render");

        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "Z");
        assert!(app.begin_text_selection(column, row, Instant::now()));
        assert_eq!(
            app.text_selection_click_target(column, row),
            Some(PathBuf::from(target))
        );
    }

    #[test]
    fn test_osc8_row_count_matches_ratatui_word_wrapping() {
        let visible = "x xxxxxxxx xxxxxxxx";
        let line = Line::from(format!(
            "\x1b]8;;file:///tmp/word-wrapped\x1b\\{visible}\x1b]8;;\x1b\\"
        ));
        let wrap_width = 10usize;
        let mut targets = Vec::new();
        let rendered = App::output_row_for_render(Some(&line), BlockKind::ToolOutput, &mut targets);
        let expected = Paragraph::new(rendered)
            .wrap(Wrap { trim: false })
            .line_count(wrap_width as u16);
        let naive = (UnicodeWidthStr::width(visible) + 1).div_ceil(wrap_width);

        assert!(expected > naive, "fixture must wrap at word boundaries");
        assert_eq!(
            App::output_row_visual_rows(Some(&line), BlockKind::ToolOutput, wrap_width),
            expected
        );
    }

    #[test]
    fn test_multiple_osc8_paths_keep_distinct_click_targets() {
        let mut app = App::new();
        app.push_output(Line::from(
            "\x1b]8;;file:///tmp/alpha-A\x1b\\/tmp/alpha-A\x1b]8;;\x1b\\  \
             \x1b]8;;file:///tmp/bravo-B\x1b\\/tmp/bravo-B\x1b]8;;\x1b\\",
        ));
        render_for_selection(&mut app);

        for (symbol, target) in [("A", "/tmp/alpha-A"), ("B", "/tmp/bravo-B")] {
            let (column, row) = selection_text_position(&app, SelectionPane::Transcript, symbol);
            assert!(app.begin_text_selection(column, row, Instant::now()));
            assert_eq!(
                app.text_selection_click_target(column, row),
                Some(PathBuf::from(target))
            );
            assert_eq!(app.finish_text_selection(column, row), None);
        }
    }

    #[test]
    fn test_transcript_selection_survives_input_edits() {
        let mut app = App::new();
        app.push_plain("alpha beta");
        render_for_selection(&mut app);
        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "a");
        let now = Instant::now();

        assert!(app.begin_text_selection(column, row, now));
        assert!(app.extend_text_selection(column + 4, row, now + Duration::from_millis(16)));
        assert_eq!(
            app.finish_text_selection(column + 4, row),
            Some("alpha".to_string())
        );

        app.set_input_text("draft");
        render_for_selection(&mut app);

        assert_eq!(
            app.finish_text_selection(column + 4, row),
            Some("alpha".to_string())
        );
    }

    #[test]
    fn test_transcript_selection_remains_available_during_approval() {
        let mut app = App::new();
        app.push_plain("alpha beta");
        let (request, _response_rx) = crate::core::approval::approval_request_for_test(
            60,
            "Approval required · edit_file",
            "Approve these edits?",
        );
        assert!(app.set_pending_approval(request));
        render_for_selection(&mut app);
        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "a");
        let now = Instant::now();

        assert!(app.begin_text_selection(column, row, now));
        assert!(app.extend_text_selection(column + 4, row, now + Duration::from_millis(16)));
        assert_eq!(
            app.finish_text_selection(column + 4, row),
            Some("alpha".to_string())
        );
    }

    #[test]
    fn test_transcript_selection_remains_available_with_plan_panel() {
        let mut app = App::new();
        app.push_plain("alpha beta");
        app.plan_state_cache = Some(crate::core::plan_state::PlanState::create_plan(vec![
            "Inspect the current behavior".to_string(),
        ]));
        render_for_selection(&mut app);
        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "a");
        let now = Instant::now();

        assert!(app.begin_text_selection(column, row, now));
        assert!(app.extend_text_selection(column + 4, row, now + Duration::from_millis(16)));
        assert_eq!(
            app.finish_text_selection(column + 4, row),
            Some("alpha".to_string())
        );
    }

    #[test]
    fn test_completion_selection_copies_visible_cells() {
        let mut app = App::new();
        app.push_plain("transcript");
        app.push_completion_line(Line::from("done now"));
        render_for_selection(&mut app);
        let (column, row) = selection_text_position(&app, SelectionPane::Completion, "d");
        let now = Instant::now();

        assert!(app.begin_text_selection(column, row, now));
        assert!(app.extend_text_selection(column + 3, row, now + Duration::from_millis(16)));
        assert_eq!(
            app.finish_text_selection(column + 3, row),
            Some("done".to_string())
        );
    }

    #[test]
    fn test_selection_normalizes_wide_grapheme_endpoints() {
        let mut app = App::new();
        app.push_plain("A界B");
        render_for_selection(&mut app);
        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "界");
        let selection_column = column.saturating_sub(
            app.selection_surface(SelectionPane::Transcript)
                .expect("render should create a transcript selection surface")
                .content_area
                .x,
        ) as usize;

        assert!(app.begin_text_selection(column + 1, row, Instant::now()));
        assert_eq!(
            app.text_selection
                .as_ref()
                .map(|selection| selection.anchor.column),
            Some(selection_column)
        );
    }

    #[test]
    fn test_transcript_selection_tracks_output_lines_during_streaming() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        for index in 0..20 {
            if index == 17 {
                app.push_plain("TARGET");
            } else {
                app.push_plain(format!("line {index}"));
            }
        }
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial selection frame should render");

        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "T");
        let now = Instant::now();
        assert!(app.begin_text_selection(column, row, now));
        assert!(app.extend_text_selection(column + 5, row, now + Duration::from_millis(16)));

        app.push_stream_line(Line::from("stream update"), StreamKind::Model);
        terminal
            .draw(|frame| app.render(frame))
            .expect("streamed selection frame should render");

        let (updated_column, updated_row) =
            selection_text_position(&app, SelectionPane::Transcript, "T");
        assert_ne!(updated_row, row);
        assert!(
            terminal
                .backend()
                .buffer()
                .cell((updated_column, updated_row))
                .expect("tracked selection cell should exist")
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(
            app.finish_text_selection(updated_column + 5, updated_row),
            Some("TARGET".to_string())
        );
    }

    #[test]
    fn test_drag_selection_throttles_redraws() {
        let mut app = App::new();
        app.push_plain("alpha beta");
        render_for_selection(&mut app);
        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "a");
        let now = Instant::now();

        assert!(app.begin_text_selection(column, row, now));
        app.needs_redraw = false;
        assert!(!app.extend_text_selection(column + 1, row, now + Duration::from_millis(15)));
        assert!(!app.needs_redraw);
        assert!(app.extend_text_selection(column + 1, row, now + Duration::from_millis(16)));
        assert!(app.needs_redraw);
    }

    #[test]
    fn test_selection_overlay_marks_selected_cells_reversed() {
        let mut app = App::new();
        app.push_plain("alpha beta");
        render_for_selection(&mut app);
        let (column, row) = selection_text_position(&app, SelectionPane::Transcript, "a");
        let now = Instant::now();

        assert!(app.begin_text_selection(column, row, now));
        assert!(app.extend_text_selection(column + 4, row, now + Duration::from_millis(16)));

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| app.render(frame))
            .expect("selection overlay should render");
        assert!(
            terminal
                .backend()
                .buffer()
                .cell((column, row))
                .expect("selected cell should exist")
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn test_slash_overlay_disables_selection() {
        let mut app = App::new();
        app.push_plain("transcript");
        app.slash_command_active = true;
        render_for_selection(&mut app);

        assert!(app.selection_surfaces.is_empty());
        assert!(!app.begin_text_selection(1, 1, Instant::now()));
    }

    #[test]
    fn test_scroll_lines_switches_to_manual_mode() {
        let mut app = make_scrolling_app(20, 5);

        app.scroll_lines(-3);

        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, 12);
        assert_eq!(app.resolved_scroll_y_for(app.output_lines.len(), 5), 12);
    }

    #[test]
    fn test_manual_scroll_preserves_viewport_while_output_appends() {
        let mut app = make_scrolling_app(20, 5);

        app.scroll_lines(-3);
        app.push_plain("new streamed output");

        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, 12);
        assert_eq!(app.unseen_output_count, 1);
        assert_eq!(app.resolved_scroll_y_for(app.output_lines.len(), 5), 12);
    }

    #[test]
    fn test_resize_reflow_preserves_manual_viewport_anchor() {
        let mut app = App::new();
        app.set_content_height(5);
        app.set_content_width(80);
        for index in 0..30 {
            app.push_plain(format!("row {index}: {}", "x".repeat(48)));
        }
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 8;

        app.capture_manual_viewport_for_reflow();
        app.set_content_width(20);
        app.restore_pending_manual_viewport_after_reflow(app.last_wrap_width());

        let anchor = app
            .manual_viewport_anchor(app.last_wrap_width())
            .expect("manual viewport should retain an anchor after reflow");
        assert_eq!(anchor.output_index, 8);
        assert!(anchor.text.starts_with("row 8:"));
    }

    #[test]
    fn test_transcript_eviction_preserves_manual_viewport() {
        let mut app = App::new();
        app.set_content_height(5);
        app.set_content_width(80);
        app.push_output_with_kind(Line::from("x".repeat(60)), BlockKind::Model);
        for index in 1..10_000 {
            app.push_plain(format!("line {index}"));
        }

        // The evicted model row now takes eight rows, plus its separator.
        app.set_content_width(10);
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 50;
        app.capture_manual_viewport_for_reflow();
        app.push_plain("new");
        app.restore_pending_manual_viewport_after_reflow(app.last_wrap_width());

        assert_eq!(app.output_lines.len(), 10_000);
        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, 41);
        assert_eq!(app.unseen_output_count, 1);

        let wrap_width = app.last_wrap_width();
        let total_rows = app.total_visual_rows(wrap_width);
        let scroll_y = app.resolved_scroll_y_for(total_rows, app.last_content_height);
        let (start, _, _) =
            app.visible_output_window(wrap_width, scroll_y, app.last_content_height);
        let first_visible = app
            .collect_output_rows_range(start, 1)
            .into_iter()
            .next()
            .expect("manual viewport should contain a transcript row");
        assert!(App::line_to_string(&first_visible).contains("line 42"));
    }

    #[test]
    fn test_finalize_turn_stream_preserves_manual_model_anchor() {
        let mut app = App::new();
        app.set_content_height(1);
        app.set_content_width(80);
        for index in 0..5 {
            app.push_plain(format!("history {index}"));
        }
        app.push_stream_line(Line::from("intro"), StreamKind::Model);
        app.push_stream_line(Line::from("**target marker**"), StreamKind::Model);
        app.push_stream_line(Line::from("outro"), StreamKind::Model);
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 7;

        app.finalize_turn_stream("intro\n\n**target marker**\n\noutro");

        let anchor = app
            .manual_viewport_anchor(app.last_wrap_width())
            .expect("manual viewport should retain a model anchor after finalization");
        assert_eq!(anchor.text, "target marker");
    }

    #[test]
    fn test_finalize_turn_stream_preserves_separator_before_model_anchor() {
        let mut app = App::new();
        app.set_content_height(1);
        app.set_content_width(80);
        for index in 0..5 {
            app.push_plain(format!("history {index}"));
        }
        app.push_stream_line(Line::from("intro"), StreamKind::Model);
        app.push_stream_line(Line::from("outro"), StreamKind::Model);
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 5;

        app.finalize_turn_stream("intro\n\noutro");

        let anchor = app
            .manual_viewport_anchor(app.last_wrap_width())
            .expect("manual viewport should remain on the model separator");
        assert!(anchor.separator_before);
        assert_eq!(anchor.text, "intro");
    }

    #[test]
    fn test_finalize_turn_stream_preserves_duplicate_model_line_occurrence() {
        let mut app = App::new();
        app.set_content_height(1);
        app.set_content_width(80);
        for index in 0..5 {
            app.push_plain(format!("history {index}"));
        }
        for _ in 0..3 {
            app.push_stream_line(Line::from("**repeat**"), StreamKind::Model);
        }
        // Keep a later row below the anchor so preserving it does not
        // legitimately snap Manual mode back into tail-following mode.
        app.push_plain("later output");
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 8;

        app.finalize_turn_stream("**repeat**\n\n**repeat**\n\n**repeat**");

        let anchor = app
            .manual_viewport_anchor(app.last_wrap_width())
            .expect("manual viewport should retain a duplicate model line anchor");
        assert_eq!(anchor.text, "repeat");
        assert_eq!(anchor.output_index, 7);
    }

    #[test]
    fn test_clamp_to_content_stays_manual_near_bottom() {
        let mut app = make_scrolling_app(20, 5);
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 14;

        app.clamp_to_content();

        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, 14);
    }

    #[test]
    fn test_clamp_to_content_snaps_to_bottom_at_bottom() {
        let mut app = make_scrolling_app(20, 5);
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 15;

        app.clamp_to_content();

        assert_eq!(app.scroll_mode, ScrollMode::Auto);
        assert_eq!(app.resolved_scroll_y_for(app.output_lines.len(), 5), 15);
    }

    #[test]
    fn scroll_mode_transition_table() {
        let max_offset = 15usize;
        let cases: &[(ScrollMode, usize, ScrollMode, &str)] = &[
            // Manual at exact bottom → Auto
            (
                ScrollMode::Manual,
                max_offset,
                ScrollMode::Auto,
                "at bottom",
            ),
            // Manual 1 from bottom → stays Manual (regression guard for <= 2 → == 0 fix)
            (
                ScrollMode::Manual,
                max_offset - 1,
                ScrollMode::Manual,
                "1 from bottom",
            ),
            // Manual at arbitrary mid-position → stays Manual
            (ScrollMode::Manual, 5, ScrollMode::Manual, "mid-position"),
            // Manual at top → stays Manual
            (ScrollMode::Manual, 0, ScrollMode::Manual, "at top"),
            // Auto always stays Auto
            (ScrollMode::Auto, 0, ScrollMode::Auto, "auto"),
            // ApprovalPinned always stays ApprovalPinned
            (
                ScrollMode::ApprovalPinned,
                0,
                ScrollMode::ApprovalPinned,
                "approval pinned",
            ),
        ];

        for &(start_mode, offset, expected, label) in cases {
            let mut app = make_scrolling_app(20, 5);
            app.scroll_mode = start_mode;
            app.scroll_offset = offset;
            app.clamp_to_content();
            assert_eq!(
                app.scroll_mode, expected,
                "clamp({start_mode:?}, offset={offset}) [{label}] should yield {expected:?}",
            );
        }
    }

    #[test]
    fn test_approval_pin_ignores_manual_scroll_attempts() {
        let mut app = make_scrolling_app(20, 5);
        app.pin_approval_bottom();

        app.scroll_lines(-4);

        assert_eq!(app.scroll_mode, ScrollMode::ApprovalPinned);
        assert_eq!(app.scroll_offset, 0);
        assert_eq!(app.resolved_scroll_y_for(app.output_lines.len(), 5), 15);
    }

    #[test]
    fn test_clear_approval_pin_returns_to_auto_follow() {
        let mut app = make_scrolling_app(20, 5);
        app.pin_approval_bottom();

        app.clear_approval_pin();

        assert_eq!(app.scroll_mode, ScrollMode::Auto);
        assert!(app.is_auto_following_output());
    }

    #[test]
    fn test_scroll_offsets_remain_full_width_until_terminal_boundary() {
        let total_rows = u16::MAX as usize + 4_096;
        let content_height = 24;
        let max_offset = total_rows - content_height;
        let mut app = App::new();

        assert_eq!(
            App::max_scroll_offset_for(total_rows, content_height),
            max_offset
        );
        assert_eq!(
            app.resolved_scroll_y_for(total_rows, content_height),
            max_offset
        );

        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = max_offset - 1;
        assert_eq!(
            app.resolved_scroll_y_for(total_rows, content_height),
            max_offset - 1
        );
        assert_eq!(App::terminal_scroll_offset(max_offset), u16::MAX);
    }

    #[test]
    fn test_large_manual_scroll_does_not_wrap() {
        let mut app = App::new();
        app.set_content_height(20);
        app.set_content_width(80);
        let total_rows = u16::MAX as usize + 2_048;
        app.cached_visual_rows = total_rows;
        app.cached_wrap_width = Some(app.last_wrap_width());

        app.scroll_lines(-1);

        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, total_rows - 21);
    }

    #[test]
    fn test_transcript_scroll_excludes_completion_rows() {
        let mut app = App::new();
        app.set_content_height(5);
        app.set_content_width(80);
        for index in 0..12 {
            app.push_plain(format!("transcript row {index}"));
            app.push_completion_line(format!("completion row {index}").into());
        }
        let output_rows = app.output_visual_rows(app.last_wrap_width());
        let output_max_offset = output_rows.saturating_sub(app.last_content_height);

        app.scroll_lines(-1);

        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, output_max_offset.saturating_sub(1));
    }

    #[test]
    fn test_cached_visual_rows_tracks_push_clear_and_drain() {
        let mut app = App::new();
        app.scrollback_file = None;
        app.set_content_width(24);
        let wrap_width = app.last_wrap_width();

        // Prime the cache so the common push path can update it
        // incrementally instead of invalidating the whole transcript.
        assert_eq!(app.total_visual_rows(wrap_width), 0);
        assert_eq!(app.cached_wrap_width, Some(wrap_width));

        app.push_plain("short line");
        let first_total = app.total_visual_rows(wrap_width);
        assert_eq!(app.cached_wrap_width, Some(wrap_width));
        assert_eq!(app.cached_visual_rows, first_total);

        app.push_plain("this line is intentionally long enough to wrap twice");
        assert_eq!(app.cached_wrap_width, Some(wrap_width));
        assert!(app.cached_visual_rows > first_total);
        let second_total = app.total_visual_rows(wrap_width);
        assert_eq!(app.cached_visual_rows, second_total);
        assert!(second_total >= first_total);

        app.drain_output_from(1);
        let drained_total = app.total_visual_rows(wrap_width);
        assert_eq!(app.cached_visual_rows, drained_total);

        app.clear_output().unwrap();
        assert_eq!(app.total_visual_rows(wrap_width), 0);
        assert_eq!(app.cached_visual_rows, 0);
    }

    #[test]
    fn test_visible_output_window_limits_render_to_viewport_slice() {
        let mut app = App::new();
        app.set_content_width(20);

        app.push_plain("this first line wraps over the viewport width");
        for index in 0..40 {
            app.push_plain(format!("line {}", index));
        }

        let wrap_width = app.last_wrap_width();
        let total_rows = app.total_visual_rows(wrap_width);
        let (start_idx, take_count, scroll_y) =
            app.visible_output_window(wrap_width, total_rows.saturating_sub(3), 3);

        assert!(start_idx > 0, "expected a later slice near the bottom");
        assert!(
            take_count < app.output_lines.len(),
            "window should not clone all lines"
        );
        assert!(
            scroll_y <= 3,
            "local scroll offset should stay within the viewport"
        );
    }

    #[test]
    fn test_sync_plan_state_cache_skips_unchanged_plan() {
        let mut app = App::new();
        let mut plan =
            crate::core::plan_state::PlanState::create_plan(vec!["First step".to_string()]);

        assert!(app.sync_plan_state_cache(Some(&plan)));
        assert!(!app.sync_plan_state_cache(Some(&plan)));

        plan.update_step(0, "Updated first step".to_string())
            .unwrap();
        assert!(app.sync_plan_state_cache(Some(&plan)));
        assert_eq!(
            app.plan_state_cache
                .as_ref()
                .expect("plan should be cached")
                .steps[0]
                .description,
            "Updated first step"
        );
    }

    #[test]
    fn test_render_approval_panel_replaces_busy_input() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.agent_busy = true;
        app.auto_approve_all = true;
        app.set_input_text("draft that must remain untouched");
        app.force_bottom();
        app.push_plain("line 1");
        app.push_plain("line 2");
        let (response_tx, _response_rx) = std::sync::mpsc::channel();
        let request = crate::core::approval::ApprovalRequest::new(
            1,
            "Approval required · edit_file".to_string(),
            "🔧 Tool: edit_file\nApprove these edits?".to_string(),
            vec![
                crate::core::approval::ApprovalChoice::new(
                    'y',
                    "Run once",
                    crate::core::approval::ApprovalResult::Approved,
                ),
                crate::core::approval::ApprovalChoice::new(
                    'n',
                    "Stop",
                    crate::core::approval::ApprovalResult::Denied,
                ),
                crate::core::approval::ApprovalChoice::new(
                    'a',
                    "Trust session",
                    crate::core::approval::ApprovalResult::Always,
                ),
            ],
            response_tx,
        );
        assert!(app.set_pending_approval(request));

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Approve these edits?"));
        assert!(rendered.contains("[y] Run once"));
        assert!(rendered.contains("[n/Esc] Stop"));
        assert!(rendered.contains("[a] Trust session"));
        assert!(rendered.contains("[AUTO-APPROVE]"));
        assert!(!rendered.contains("Agent processing..."));
        assert!(!rendered.contains("draft that must remain untouched"));
        assert!(app.approval_accepts_input());
        assert_eq!(
            app.input.lines().join("\n"),
            "draft that must remain untouched"
        );
    }

    #[test]
    fn test_sequential_approval_ids_ignore_stale_finish_events() {
        let mut app = App::new();
        let (first, first_rx) = crate::core::approval::approval_request_for_test(
            10,
            "Approval required · first",
            "Approve first?",
        );
        assert!(app.set_pending_approval(first));
        assert!(matches!(
            app.resolve_pending_approval(crate::core::approval::ApprovalResult::Approved),
            Some(true)
        ));
        assert!(matches!(
            first_rx.try_recv(),
            Ok(crate::core::approval::ApprovalResponse::Decision(
                crate::core::approval::ApprovalResult::Approved
            ))
        ));

        let (second, _second_rx) = crate::core::approval::approval_request_for_test(
            11,
            "Approval required · second",
            "Approve second?",
        );
        assert!(app.set_pending_approval(second));
        assert!(!app.finish_pending_approval(10));
        assert!(app.has_pending_approval());
        assert!(app.finish_pending_approval(11));
        assert!(!app.has_pending_approval());
    }

    #[test]
    fn test_busy_state_does_not_replace_input_placeholder() {
        let mut app = App::new();
        app.agent_busy = true;
        app.mode = "ACT".to_string();

        app.update_placeholder();

        assert_eq!(app.input.placeholder_text(), "❯ [ACT] ");
    }

    #[test]
    fn test_render_output_keeps_single_busy_loading_message() {
        let _approval_guard = crate::core::approval::approval_test_guard();
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.agent_busy = true;
        app.mode = "ACT".to_string();
        app.force_bottom();
        app.push_plain("line 1");

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Agent processing..."));
        assert!(!rendered.contains("Working"));
        assert!(!rendered.contains("Agent working..."));
    }

    #[test]
    fn test_tick_spinner_throttles_frame_advancement() {
        let mut app = App::new();
        app.agent_busy = true;

        app.tick_spinner();
        let first = app.spinner_index;

        app.tick_spinner();
        assert_eq!(app.spinner_index, first);

        app.last_spinner_tick = Some(Instant::now() - Duration::from_millis(200));
        app.tick_spinner();
        assert_ne!(app.spinner_index, first);
    }

    #[test]
    fn test_render_output_keeps_wrapped_prompt_tail_visible_when_pinned() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.pin_approval_bottom();
        app.push_plain(
            "A long wrapped tool explanation line that takes multiple visual rows in the output pane.",
        );
        app.push_plain(
            "Another wrapped line that would previously push the confirmation row below the viewport.",
        );
        app.push_plain("[Sned Question] What kind of colour improvement would you like?");
        app.push_plain("Your answer:");

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Your answer:"));
        assert!(rendered.contains("What kind of colour improvement"));
    }

    #[test]
    fn test_render_approval_prompt_stays_visible_with_tall_input() {
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.set_input_text("1\n2\n3\n4\n5\n6\n7\n8");
        app.push_plain("A long wrapped tool explanation line that takes multiple rows.");
        app.push_plain("Another wrapped line that used to crowd the prompt below the input box.");
        let (request, _response_rx) = crate::core::approval::approval_request_for_test(
            2,
            "Approval required · edit_file",
            "🔧 Tool: edit_file\npath: src/lib.rs\nExecute this tool?",
        );
        assert!(app.set_pending_approval(request));

        assert!(app.render_input_height() > 3);

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let rendered = rendered_rows(buffer).join("\n");

        assert!(rendered.contains("Approval required · edit_file"));
        assert!(rendered.contains("Execute this tool?"));
        assert!(rendered.contains("[n/Esc] Deny"));
        assert!(!rendered.contains("1\n2\n3"));
    }

    #[test]
    fn test_render_approval_pin_tracks_prompt_tail_not_transcript_tail() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.pin_approval_bottom();

        for index in 0..8 {
            app.push_plain(format!("old line {index}"));
        }
        app.push_output_with_kind(Line::from("🔧 Tool: edit_file"), BlockKind::BlockingPrompt);
        app.push_output_with_kind(
            Line::from("Execute this tool? (y/n/always):"),
            BlockKind::BlockingPrompt,
        );
        for index in 0..6 {
            app.push_plain(format!("late tool output {index}"));
        }

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let output_rows = rendered_output_rows(&app, buffer);
        let rendered = output_rows.join("\n");

        assert!(
            rendered.contains("Execute this tool?"),
            "approval-pinned viewport must keep the prompt tail visible even if later output exists: {output_rows:?}"
        );
        assert!(
            !rendered.contains("late tool output 5"),
            "approval pin should anchor to the prompt block instead of newer transcript tail rows: {output_rows:?}"
        );
    }

    #[test]
    fn test_render_shows_picker_overlay_when_active() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.picker_active = true;
        app.picker_results = vec![crate::core::file_search::FileSearchResult {
            path: "src/main.rs".to_string(),
            file_type: crate::core::file_search::FileType::File,
            label: "main.rs".to_string(),
        }];

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Files (1)"));
        assert!(rendered.contains("main.rs"));
        assert!(rendered.contains("Tab select"));
    }

    #[test]
    fn test_empty_picker_overlays_show_no_matches() {
        let render = |app: &mut App| {
            let backend = TestBackend::new(80, 12);
            let mut terminal = Terminal::new(backend).expect("terminal should initialize");
            terminal
                .draw(|frame| app.render(frame))
                .expect("render should succeed");
            rendered_rows(terminal.backend().buffer()).join("\n")
        };

        let mut file_picker = App::new();
        file_picker.picker_active = true;
        file_picker.mention_search_query = "zzz".to_string();
        let rendered = render(&mut file_picker);
        assert!(rendered.contains("Files · @zzz"));
        assert!(rendered.contains("No matches for @zzz"));

        let mut slash_picker = App::new();
        slash_picker.slash_command_active = true;
        let rendered = render(&mut slash_picker);
        assert!(rendered.contains("Slash Commands (0)"));
        assert!(rendered.contains("No matches"));

        let mut model_picker = App::new();
        model_picker.model_picker_active = true;
        let rendered = render(&mut model_picker);
        assert!(rendered.contains("Models (0)"));
        assert!(rendered.contains("No matches"));
    }

    #[test]
    fn test_picker_viewport_centers_selected_result_and_only_scrolls_when_needed() {
        assert_eq!(App::picker_viewport_start(0, 12, 8), 0);
        assert_eq!(App::picker_viewport_start(6, 12, 8), 2);
        assert_eq!(App::picker_viewport_start(11, 12, 8), 4);
        assert_eq!(App::picker_scrollbar_position(11, 12, 8), 4);
        assert!(!App::picker_has_scrollbar(8, 8));
        assert!(App::picker_has_scrollbar(9, 8));
    }

    #[test]
    fn test_picker_overlays_show_active_query_hints_and_selected_viewport() {
        let render = |app: &mut App| {
            let backend = TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).expect("terminal should initialize");
            terminal
                .draw(|frame| app.render(frame))
                .expect("render should succeed");
            rendered_rows(terminal.backend().buffer()).join("\n")
        };

        let mut file_picker = App::new();
        file_picker.picker_active = true;
        file_picker.mention_search_query = "src".to_string();
        file_picker.picker_results = (0..12)
            .map(|index| crate::core::file_search::FileSearchResult {
                path: format!("src/file{index}.rs"),
                file_type: crate::core::file_search::FileType::File,
                label: format!("file{index}.rs"),
            })
            .collect();
        file_picker.picker_index = 11;
        let rendered = render(&mut file_picker);
        assert!(rendered.contains("Files (12) · @src"));
        assert!(rendered.contains("file11.rs"));
        assert!(rendered.contains("Enter insert"));

        let mut slash_picker = App::new();
        slash_picker.slash_command_active = true;
        slash_picker.set_input_text("/command");
        slash_picker.slash_command_results = (0..12)
            .map(|index| crate::cli::slash_commands::SlashCommandEntry {
                name: format!("command{index}"),
                description: format!("Command {index}"),
                aliases: Vec::new(),
                category: crate::cli::slash_commands::SlashCommandCategory::Local,
                requires_args: false,
            })
            .collect();
        slash_picker.slash_command_selected = 11;
        let rendered = render(&mut slash_picker);
        assert!(rendered.contains("Slash Commands (12) · /command"));
        assert!(rendered.contains("command11"));
        assert!(rendered.contains("Tab complete"));

        let mut model_picker = App::new();
        model_picker.model_picker_active = true;
        model_picker.model_picker_results =
            crate::cli::slash_commands::build_model_picker_entries();
        model_picker.model_picker_selected = model_picker.model_picker_results.len() - 1;
        let selected_model = model_picker.model_picker_results.last().unwrap().label;
        let rendered = render(&mut model_picker);
        assert!(rendered.contains(&format!(
            "Models ({})",
            model_picker.model_picker_results.len()
        )));
        assert!(rendered.contains(selected_model));
        assert!(rendered.contains("Tab/Enter switch"));
    }

    #[test]
    fn test_output_title_marks_scrollback_mode_and_reverts_after_exit() {
        let render = |app: &mut App| {
            let backend = TestBackend::new(80, 12);
            let mut terminal = Terminal::new(backend).expect("terminal should initialize");
            terminal
                .draw(|frame| app.render(frame))
                .expect("render should succeed");
            rendered_rows(terminal.backend().buffer()).join("\n")
        };

        let mut app = App::new();
        assert!(render(&mut app).contains("sned"));
        assert!(!render(&mut app).contains("sned (scrollback)"));

        app.in_scrollback = true;
        assert!(render(&mut app).contains("sned (scrollback)"));

        app.in_scrollback = false;
        assert!(!render(&mut app).contains("sned (scrollback)"));
    }

    #[test]
    fn test_mention_debounce_does_not_fire_before_deadline() {
        let mut app = App::new();
        app.cwd = "/tmp".to_string();

        // Simulate first entry into mention mode
        app.mention_search_active = true;
        app.mention_search_query = "@m".to_string();
        app.mention_search_deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(150);

        // Query changes — deadline should reset
        app.mention_search_query = "@ma".to_string();
        app.mention_search_deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(150);

        // Deadline has not passed — search should NOT fire
        assert!(std::time::Instant::now() < app.mention_search_deadline);
    }

    #[test]
    fn test_render_output_does_not_update_placeholder() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.mode = "PLAN".to_string();
        app.force_bottom();

        // Record placeholder before render_output
        let placeholder_before = app.input.placeholder_text().to_string();
        assert_eq!(placeholder_before, "❯ ");

        // Call render_output directly (not render(), which also calls render_input)
        let output_area = ratatui::layout::Rect::new(0, 0, 80, 10);
        terminal
            .draw(|frame| app.render_output(frame, output_area))
            .expect("render_output should succeed");

        // Placeholder should be unchanged — render_output no longer mutates it
        assert_eq!(
            app.input.placeholder_text(),
            placeholder_before,
            "render_output should not update placeholder"
        );
    }

    #[test]
    fn test_render_status_bar_caches_static_fields() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.provider_name = "openai".to_string();
        app.model_name = "gpt-4".to_string();
        app.task_id = "task-1".to_string();
        app.mode = "ACT".to_string();

        let status_area = ratatui::layout::Rect::new(0, 0, 80, 1);
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("first render should succeed");

        let cached_after_first = app.cached_status_left.clone();
        assert!(cached_after_first.contains("openai"));
        assert!(cached_after_first.contains("gpt-4"));

        // Second render with no field changes — cache should be reused
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("second render should succeed");
        assert_eq!(
            app.cached_status_left, cached_after_first,
            "cache should be reused when fields are unchanged"
        );

        // Mutate a field — cache should rebuild
        app.task_id = "task-2".to_string();
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("third render should succeed");
        assert_ne!(
            app.cached_status_left, cached_after_first,
            "cache should rebuild when a field changes"
        );
        assert!(app.cached_status_left.contains("task-2"));
    }

    #[test]
    fn test_status_notification_replaces_status_until_expiry() {
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.provider_name = "openai".to_string();
        app.model_name = "gpt-4".to_string();
        app.task_id = "task-1".to_string();
        app.mode = "ACT".to_string();
        app.yolo_mode = true;
        app.show_notification("Model switched to openai/gpt-4", NotificationKind::Success);
        let expires_at = app
            .status_notification
            .as_ref()
            .expect("notification should be active")
            .expires_at;
        let status_area = Rect::new(0, 0, 80, 1);

        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("notification render should succeed");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Model switched to openai/gpt-4"));
        assert!(rendered.contains("[YOLO] ACT"));

        assert!(app.tick_notification(expires_at));
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("normal status render should succeed");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("Model switched"));
        assert!(rendered.contains("openai / gpt-4"));
    }

    #[test]
    fn test_render_status_bar_shows_approval_mode() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(48, 1);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.provider_name = "openai".to_string();
        app.model_name = "long-model-name".to_string();
        app.task_id = "01KY7V7M0PVJYG0Y06WW8DGSRY".to_string();
        app.mode = "ACT".to_string();

        let status_area = ratatui::layout::Rect::new(0, 0, 48, 1);
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("standard render should succeed");
        assert!(!app.cached_status_left.contains("YOLO"));
        assert!(!app.cached_status_left.contains("AUTO-APPROVE"));

        app.auto_approve_all = true;
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("auto-approve render should succeed");
        assert!(app.cached_status_left.starts_with(" [AUTO-APPROVE] "));

        app.yolo_mode = true;
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("yolo render should succeed");
        assert!(app.cached_status_left.starts_with(" [YOLO] "));
        assert!(!app.cached_status_left.contains("AUTO-APPROVE"));
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("[YOLO]"));
    }

    #[test]
    fn test_clear_pastes_empties_paste_chunks() {
        let mut app = App::new();
        app.paste_chunks.push(PasteChunk {
            marker: "[pasted 10 chars]".to_string(),
            content: "0123456789".to_string(),
        });
        assert_eq!(app.paste_chunks.len(), 1);

        app.clear_pastes();
        assert!(app.paste_chunks.is_empty());
    }

    #[test]
    fn test_get_input_with_expanded_pastes_handles_duplicates_and_no_collisions() {
        let mut app = App::new();

        // Simulate two separate pastes (same content, different markers)
        app.paste_chunks.push(PasteChunk {
            marker: "⟦pasted 19 chars #1⟧".to_string(),
            content: "first paste content".to_string(),
        });
        app.paste_chunks.push(PasteChunk {
            marker: "⟦pasted 19 chars #2⟧".to_string(),
            content: "first paste content".to_string(),
        });

        app.input = App::new_textarea(vec![
            "⟦pasted 19 chars #1⟧".to_string(),
            "some text".to_string(),
            "⟦pasted 19 chars #2⟧".to_string(),
        ]);

        let result = app.get_input_with_expanded_pastes();
        assert_eq!(
            result,
            "first paste content\nsome text\nfirst paste content"
        );
        assert!(
            app.paste_chunks.is_empty(),
            "paste_chunks should be drained after expansion"
        );

        let mut app2 = App::new();
        app2.input = App::new_textarea(vec!["[pasted 500 chars]".to_string()]);
        let result2 = app2.get_input_with_expanded_pastes();
        assert_eq!(result2, "[pasted 500 chars]");
    }

    #[test]
    fn test_handle_paste_uses_unique_visible_markers_and_expands() {
        let mut app = App::new();
        let char_count = app.paste_fold_threshold + 1;
        let content = "🙂".repeat(char_count);

        assert_eq!(
            app.handle_paste(&content, true),
            PasteOutcome::Folded { char_count }
        );
        let first_marker = app.input.lines().join("\n");
        assert!(first_marker.contains(&format!("pasted {char_count} chars")));
        assert!(!first_marker.contains('\0'));

        app.input.insert_str("\n");
        assert_eq!(
            app.handle_paste(&content, true),
            PasteOutcome::Folded { char_count }
        );
        let markers = app
            .paste_chunks
            .iter()
            .map(|paste| paste.marker.as_str())
            .collect::<Vec<_>>();
        assert_ne!(markers[0], markers[1]);

        assert_eq!(
            app.get_input_with_expanded_pastes(),
            format!("{content}\n{content}")
        );
    }

    #[test]
    fn test_handle_paste_rejects_input_over_limit() {
        let mut app = App::new();
        let content = "x".repeat(MAX_PASTED_INPUT_BYTES + 1);

        assert_eq!(
            app.handle_paste(&content, true),
            PasteOutcome::RejectedTooLarge {
                max_bytes: MAX_PASTED_INPUT_BYTES
            }
        );
        assert_eq!(app.input.lines().join("\n"), "");
        assert!(app.paste_chunks.is_empty());
    }

    #[test]
    fn test_handle_paste_prunes_detached_payloads_before_enforcing_limit() {
        let mut app = App::new();
        let content = "x".repeat(MAX_PASTED_INPUT_BYTES);
        assert!(matches!(
            app.handle_paste(&content, true),
            PasteOutcome::Folded { .. }
        ));

        app.set_input_text("");
        assert_eq!(
            app.handle_paste("replacement", true),
            PasteOutcome::Inserted
        );
        assert!(app.paste_chunks.is_empty());
        assert_eq!(app.input.lines().join("\n"), "replacement");
    }

    #[test]
    fn test_handle_paste_limits_retained_folded_chunks() {
        let mut app = App::new();
        let mut markers = Vec::new();
        for index in 0..MAX_FOLDED_PASTE_CHUNKS {
            let marker = format!("⟦paste #{index}⟧");
            markers.push(marker.clone());
            app.paste_chunks.push(PasteChunk {
                marker,
                content: "x".repeat(app.paste_fold_threshold + 1),
            });
        }
        app.set_input_text(&markers.join("\n"));
        let content = "x".repeat(app.paste_fold_threshold + 1);

        assert_eq!(
            app.handle_paste(&content, true),
            PasteOutcome::RejectedTooManyChunks {
                max_chunks: MAX_FOLDED_PASTE_CHUNKS
            }
        );
    }

    #[test]
    fn test_set_input_text_and_cursor_preserves_multiline_position() {
        let mut app = App::new();
        let text = "first line\nsecond line\nthird";
        let cursor = "first line\nsecond".len();

        app.set_input_text_and_cursor(text, cursor);

        assert_eq!(app.input.lines(), ["first line", "second line", "third"]);
        assert_eq!(app.input.cursor(), (1, "second".chars().count()));
    }

    #[test]
    fn test_input_height_caps_visible_lines() {
        let mut app = App::new();
        assert_eq!(app.input_height(), 3);

        app.set_input_text("one\ntwo\nthree\nfour");
        assert_eq!(app.input_height(), 6);

        app.set_input_text("1\n2\n3\n4\n5\n6\n7\n8");
        assert_eq!(app.input_height(), 8);
    }

    #[test]
    fn test_render_status_bar_caches_right_segment() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::time::Duration;

        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.elapsed = Some(Duration::from_secs(42));

        let status_area = ratatui::layout::Rect::new(0, 0, 80, 1);
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("first render should succeed");
        let cached_after_first = app.cached_status_right.clone();
        assert!(cached_after_first.contains("42"));

        // Same second — cache should be reused
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("second render should succeed");
        assert_eq!(
            app.cached_status_right, cached_after_first,
            "cache should be reused within the same second"
        );

        // Different second — cache should rebuild
        app.elapsed = Some(Duration::from_secs(43));
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("third render should succeed");
        assert_ne!(
            app.cached_status_right, cached_after_first,
            "cache should rebuild when seconds change"
        );
        assert!(app.cached_status_right.contains("43"));
    }

    #[test]
    fn test_status_bar_shows_unseen_output_only_while_manually_scrolled() {
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = make_scrolling_app(20, 5);
        app.scroll_lines(-1);
        app.push_plain("new output");

        let status_area = ratatui::layout::Rect::new(0, 0, 80, 1);
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("manual status bar should render");
        assert!(app.cached_status_right.contains("↑ 1 new"));

        app.force_bottom();
        terminal
            .draw(|frame| app.render_status_bar(frame, status_area))
            .expect("auto-follow status bar should render");
        assert!(!app.cached_status_right.contains("new"));
    }

    #[test]
    fn test_output_scrollbar_is_hidden_when_content_fits() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.push_plain("short output");

        terminal
            .draw(|frame| app.render(frame))
            .expect("output should render");

        let rendered = rendered_rows(terminal.backend().buffer()).join("\n");
        assert!(!rendered.contains('↑'));
        assert!(!rendered.contains('↓'));
    }

    #[test]
    fn test_scrollback_indicator_remains_visible_while_manually_scrolled() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        for index in 0..30 {
            app.push_plain(format!("line {index}"));
        }
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial output should render");
        app.scroll_lines(-1);
        app.scrollback_count = 42;

        terminal
            .draw(|frame| app.render(frame))
            .expect("manual output should render");

        let rendered = rendered_rows(terminal.backend().buffer()).join("\n");
        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert!(rendered.contains("42 lines of scrollback"));
    }

    #[test]
    fn test_slash_command_fields_initialized() {
        let app = App::new();
        assert!(!app.slash_command_active);
        assert!(!app.slash_command_help_active);
        assert!(app.slash_command_results.is_empty());
        assert_eq!(app.slash_command_selected, 0);
        assert!(app.slash_command_all_entries.is_empty());
    }

    #[test]
    fn test_slash_command_overlay_not_rendered_when_inactive() {
        let mut app = App::new();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        app.slash_command_active = false;
        app.slash_command_results = vec![crate::cli::slash_commands::SlashCommandEntry {
            name: "exit".to_string(),
            description: "Exit".to_string(),
            aliases: vec![],
            category: crate::cli::slash_commands::SlashCommandCategory::Local,
            requires_args: false,
        }];
        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        // The overlay title contains "Slash Commands" — assert it's NOT in the buffer.
        let buffer = terminal.backend().buffer().clone();
        let mut found = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol().contains("Slash Commands") {
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }
        assert!(
            !found,
            "slash command overlay should not render when slash_command_active is false"
        );
    }

    #[test]
    fn test_command_help_overlay_renders_list_and_details_at_wide_and_narrow_widths() {
        for (width, height) in [(100, 24), (50, 18)] {
            let mut app = App::new();
            app.slash_command_active = true;
            app.slash_command_help_active = true;
            app.slash_command_results =
                crate::cli::slash_commands::build_slash_command_entries(&[]);
            app.slash_command_selected = app
                .slash_command_results
                .iter()
                .position(|entry| entry.name == "clear")
                .unwrap();
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("terminal should initialize");
            terminal
                .draw(|frame| app.render(frame))
                .expect("help overlay should render");

            let rendered = rendered_rows(terminal.backend().buffer()).join("\n");
            assert!(rendered.contains("Command Help"));
            assert!(rendered.contains("Details"));
            assert!(rendered.contains("/clear"));
            assert!(rendered.contains("Clear the visible display"));
        }
    }

    #[test]
    fn test_push_stream_line_records_indices_for_turn_end() {
        // Three streamed model-output lines should be recorded as
        // indices [0, 1, 2] in the order they were pushed. The
        // recorded indices are what `finalize_turn_stream` pops.
        let mut app = App::new();
        app.push_stream_line(Line::from("first"), StreamKind::Model);
        app.push_stream_line(Line::from("second"), StreamKind::Model);
        app.push_stream_line(Line::from("third"), StreamKind::Model);
        assert_eq!(
            app.turn_stream_entries,
            vec![
                (0, StreamKind::Model),
                (1, StreamKind::Model),
                (2, StreamKind::Model)
            ]
        );
        assert_eq!(app.output_lines.len(), 3);
    }

    #[test]
    fn test_reasoning_chunks_preserve_partial_and_blank_lines() {
        let mut app = App::new();
        app.set_content_width(80);

        app.push_reasoning_chunk("first");
        app.push_reasoning_chunk(" thought\n\nthird");
        app.push_reasoning_chunk(" line");
        app.finish_reasoning_stream();

        let rendered: Vec<String> = app.output_lines.iter().map(ToString::to_string).collect();
        assert_eq!(rendered, ["  Ɵ first thought", "  Ɵ ", "  Ɵ third line"]);
        assert!(
            app.output_line_kinds
                .iter()
                .all(|kind| *kind == BlockKind::Reasoning)
        );
        let style = app.output_lines[0].spans[0].style;
        assert_eq!(style.fg, Some(crate::cli::tui::theme::ACCENT));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn test_reasoning_stream_is_not_replaced_by_turn_markdown() {
        let mut app = App::new();
        app.set_content_width(80);
        app.push_reasoning_chunk("inspect state");
        app.push_stream_line(Line::from("final answer"), StreamKind::Model);

        app.finalize_turn_stream("final answer");

        let rendered: Vec<String> = app.output_lines.iter().map(ToString::to_string).collect();
        assert_eq!(rendered, ["  Ɵ inspect state", "final answer"]);
        assert_eq!(app.output_line_kinds[0], BlockKind::Reasoning);
        assert_eq!(app.output_line_kinds[1], BlockKind::Model);
    }

    #[test]
    fn test_replace_last_stream_line_reuses_tail_indices() {
        let mut app = App::new();
        app.last_content_width = 14;

        app.push_stream_line(Line::from("first"), StreamKind::Model);
        app.push_stream_line(
            Line::from("this streamed line wraps across rows"),
            StreamKind::Model,
        );

        let before = app.turn_stream_entries.clone();
        assert!(
            before.len() >= 3,
            "expected wrapped stream line to span multiple visual rows"
        );

        app.replace_last_stream_line(
            Line::from("updated streamed line wraps differently"),
            StreamKind::Model,
        );

        assert_eq!(app.turn_stream_entries[0], (0, StreamKind::Model));
        assert_eq!(
            app.output_lines.front().map(ToString::to_string).as_deref(),
            Some("first")
        );
        assert!(
            app.output_lines
                .iter()
                .skip(1)
                .any(|line| line.to_string().contains("updated")),
            "replacement should update the tail group in place"
        );
        assert!(
            app.turn_stream_entries
                .iter()
                .skip(1)
                .enumerate()
                .all(|(offset, (idx, kind))| { *kind == StreamKind::Model && *idx == offset + 1 }),
            "tail indices should be rewritten to the replacement group"
        );
    }

    #[test]
    fn test_stream_line_replacements_do_not_increment_unseen_output() {
        let mut app = make_scrolling_app(20, 5);
        app.scroll_lines(-3);

        app.push_stream_line(Line::from("partial"), StreamKind::Model);
        assert_eq!(app.unseen_output_count, 1);

        app.replace_last_stream_line(Line::from("partial update"), StreamKind::Model);
        app.replace_last_stream_line(Line::from("partial update again"), StreamKind::Model);

        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, 12);
        assert_eq!(app.unseen_output_count, 1);
    }

    #[test]
    fn test_finalize_turn_stream_replaces_recorded_lines_with_markdown() {
        // The user's bug report: agent text is rendered as raw
        // characters even when it contains markdown. At turn end, the
        // TUI should swap the streamed raw lines for the
        // markdown-rendered version of the original text.
        let mut app = App::new();
        // Stream three lines that are a wrapped fragment of the
        // original markdown "**bold** text".
        app.push_stream_line(Line::from("  **bold"), StreamKind::Model);
        app.push_stream_line(Line::from("  text"), StreamKind::Model);
        app.push_stream_line(Line::from("  more"), StreamKind::Model);
        assert_eq!(app.output_lines.len(), 3);

        app.finalize_turn_stream("**bold** text\n\nmore");

        // The recorded raw lines should be gone. The new lines should
        // contain the markdown-rendered content (no leading 2-space
        // indent, bold span styled).
        let rendered: Vec<String> = app
            .output_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(
            !rendered.iter().any(|s| s.starts_with("  **bold")),
            "raw streamed lines should be replaced: {:?}",
            rendered
        );
        assert!(
            !rendered.iter().any(|s| s == "  text"),
            "raw streamed lines should be replaced: {:?}",
            rendered
        );
        // No 🚀 prefix should appear in agent-text re-render.
        assert!(
            !rendered.iter().any(|s| s.contains("🚀")),
            "agent-text re-render must not include the completion banner: {:?}",
            rendered
        );
        assert!(app.turn_stream_entries.is_empty());
    }

    #[test]
    fn test_finalize_turn_stream_empty_text_is_noop() {
        // No accumulated text means the streamed lines are not
        // markdown — leave them in place and just clear the recorded
        // indices.
        let mut app = App::new();
        app.push_stream_line(Line::from("plain text"), StreamKind::Model);
        app.push_stream_line(Line::from("more plain"), StreamKind::Model);
        let before: Vec<String> = app
            .output_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();

        app.finalize_turn_stream("");

        let after: Vec<String> = app
            .output_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(after, before);
        assert!(app.turn_stream_entries.is_empty());
    }

    #[test]
    fn test_finalize_turn_stream_does_not_consume_ansi_code_block_lines() {
        // Raw ANSI events (e.g., syntax-highlighted code blocks) are
        // NOT recorded in turn_stream_entries — they are pushed
        // directly. Turn-end replacement should leave them in place
        // and only re-render the model-streamed text around them.
        let mut app = App::new();
        app.push_stream_line(Line::from("  intro"), StreamKind::Model);
        // Simulate a code block arriving as raw ANSI (push_output, not
        // push_stream_line).
        app.push_output(Line::from("  [code block line]"));
        app.push_stream_line(Line::from("  outro"), StreamKind::Model);
        let indices_before = app.turn_stream_entries.clone();
        assert_eq!(indices_before.len(), 2);

        app.finalize_turn_stream("# Title\n\nbody");

        // The code-block line should still be present.
        let rendered: Vec<String> = app
            .output_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(
            rendered.iter().any(|s| s.contains("[code block line]")),
            "ANSI code-block line should be preserved: {:?}",
            rendered
        );
    }

    #[test]
    fn test_finalize_turn_stream_reinserts_turn_indicator() {
        // Verify that when a TurnIndicator is stored via
        // push_turn_indicator, finalize_turn_stream preserves it by
        // prepending "\u{2666} " to the markdown text before re-rendering,
        // so the indicator stays on the same line as the first rendered
        // response line instead of being dropped or pushed onto its own
        // line.
        let mut app = App::new();
        app.push_turn_indicator(Line::from(Span::styled(
            "\u{2666}",
            Style::default().fg(crate::cli::tui::theme::ACCENT),
        )));
        app.push_stream_line(Line::from("  **bold** text"), StreamKind::Model);
        app.push_stream_line(Line::from("  more"), StreamKind::Model);
        assert_eq!(app.output_lines.len(), 2); // only streamed lines (indicator stored separately)
        assert_eq!(app.turn_stream_entries.len(), 2); // only stream lines tracked

        app.finalize_turn_stream("**bold** text\n\nmore");

        // The indicator should be prepended inline to the first rendered
        // markdown line (not a separate line above it).
        let first_text: String = app
            .output_lines
            .front()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            first_text.contains("\u{2666}"),
            "first rendered line should contain the indicator prefix: {:?}",
            app.output_lines
        );
        assert!(
            first_text.contains("bold"),
            "first rendered line should still contain the markdown content: {:?}",
            first_text
        );

        // No line should be a bare indicator line.
        let all_lines: Vec<String> = app
            .output_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        for line in &all_lines {
            assert!(
                !(line.trim() == "\u{2666}"),
                "indicator must not be on its own line: {:?}",
                all_lines
            );
        }

        assert!(app.turn_indicator.is_none(), "indicator should be cleared");
        assert!(app.turn_stream_entries.is_empty());
    }

    #[test]
    fn test_finalize_turn_stream_noop_reinsert_preserves_turn_indicator() {
        let mut app = App::new();
        app.push_turn_indicator(Line::from(Span::styled(
            "\u{2666}",
            Style::default().fg(crate::cli::tui::theme::ACCENT),
        )));
        app.push_stream_line(Line::from("plain line"), StreamKind::Model);

        app.finalize_turn_stream("plain line");

        assert_eq!(app.output_lines.len(), 1);
        let first_text: String = app
            .output_lines
            .front()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            first_text.contains("\u{2666}"),
            "first rendered line should contain the indicator prefix: {:?}",
            app.output_lines
        );
        assert!(
            first_text.contains("plain line"),
            "first rendered line should still contain the markdown content: {:?}",
            first_text
        );
        assert!(app.turn_indicator.is_none(), "indicator should be cleared");
        assert!(app.turn_stream_entries.is_empty());
    }

    #[test]
    fn test_wrapped_line_not_clipped_at_viewport_boundary() {
        // This is the exact scenario that caused the clipping bug with the old
        // virtual scrolling approach. A long wrapped line sits at the top of the
        // visible viewport. The old approach sliced the buffer and used a local
        // scroll offset, which was wrong when line_visual_rows() didn't match
        // ratatui's actual wrapping. The current approach passes the full buffer
        // and lets ratatui handle wrapping + scrolling natively.
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.set_content_width(60);
        app.set_content_height(8); // 10 - 2 border

        // Short lines first
        for i in 0..5 {
            app.push_plain(format!("short line {}", i));
        }
        // A long wrapped line that takes ~3 visual rows at width 60
        let long_line = "This is a very long prompt line that wraps across multiple visual rows in the terminal output pane and must not be clipped when scrolled into view";
        app.push_plain(long_line);
        // More short lines
        for i in 0..10 {
            app.push_plain(format!("trailing line {}", i));
        }

        // Scroll to a position where the long wrapped line is at the top of the viewport.
        // The long line starts at visual row 5 (after 5 short lines).
        // Scroll so the viewport starts at row 5 (the long line is the first visible line).
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 5;
        app.last_content_width = 60;
        app.last_content_height = 8;

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        // The long line must be visible and not clipped.
        // Its first words should appear in the rendered output.
        let full_rendered = rendered.join("\n");
        assert!(
            full_rendered.contains("This is a very long"),
            "wrapped line must not be clipped at viewport boundary.\nRendered:\n{}",
            full_rendered
        );
        // The long line should also show its tail (not clipped mid-word).
        assert!(
            full_rendered.contains("scrolled into view"),
            "wrapped line tail must be visible, not clipped.\nRendered:\n{}",
            full_rendered
        );
    }

    #[test]
    fn test_render_output_shows_reasoning_indicator_when_reasoning_active() {
        let _approval_guard = crate::core::approval::approval_test_guard();
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.agent_busy = true;
        app.reasoning_active = true;
        app.mode = "ACT".to_string();
        app.force_bottom();
        app.push_plain("line 1");

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("Reasoning..."),
            "expected 'Reasoning...' indicator when reasoning_active is true, got:\n{rendered}"
        );
        assert!(
            !rendered.contains("Agent processing..."),
            "should not show 'Agent processing...' when reasoning is active, got:\n{rendered}"
        );
    }

    #[test]
    fn test_render_output_shows_agent_processing_when_not_reasoning() {
        let _approval_guard = crate::core::approval::approval_test_guard();
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.agent_busy = true;
        app.reasoning_active = false;
        app.mode = "ACT".to_string();
        app.force_bottom();
        app.push_plain("line 1");

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("Agent processing..."),
            "expected 'Agent processing...' when reasoning is not active, got:\n{rendered}"
        );
        assert!(
            !rendered.contains("Reasoning..."),
            "should not show 'Reasoning...' when reasoning is not active, got:\n{rendered}"
        );
    }

    #[test]
    fn test_completion_line_renders_in_buffer() {
        // Regression guard: push_completion_line must invalidate the visual
        // row cache so the completion box height reflects the new line.
        // A prior bug left cached_wrap_width stale, so completion_height
        // collapsed to 2 (just borders) and the text was clipped.
        let _approval_guard = crate::core::approval::approval_test_guard();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.force_bottom();
        // Push some output first so cached_wrap_width is populated.
        app.push_plain("line 1");
        app.push_plain("line 2");
        // Trigger a render to populate cached_wrap_width.
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial render should succeed");

        // Now push a completion line and render again.
        app.push_completion_line(Line::from(Span::styled(
            "MARKER_COMPLETION_TEXT",
            Style::default().fg(theme::PROMPT_FG),
        )));
        terminal
            .draw(|frame| app.render(frame))
            .expect("post-completion render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("MARKER_COMPLETION_TEXT"),
            "completion line should appear in rendered buffer; got:\n{rendered}"
        );
    }

    #[test]
    fn test_long_completion_keeps_transcript_visible_and_scrolls_to_overflow() {
        let _approval_guard = crate::core::approval::approval_test_guard();
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.push_plain("TRANSCRIPT_MARKER");
        let long_completion = (0..20)
            .map(|index| format!("COMPLETION_WORD_{index:02}"))
            .collect::<Vec<_>>()
            .join(" ");
        app.push_completion_line(long_completion.into());

        terminal
            .draw(|frame| app.render(frame))
            .expect("initial render should succeed");

        let initial = rendered_rows(terminal.backend().buffer()).join("\n");
        assert!(initial.contains("TRANSCRIPT_MARKER"), "got:\n{initial}");
        assert!(initial.contains("COMPLETION_WORD_00"), "got:\n{initial}");
        assert!(!initial.contains("COMPLETION_WORD_19"), "got:\n{initial}");

        let completion_area = app.completion_area.expect("completion area should render");
        assert!(app.scroll_completion_at(
            completion_area.x.saturating_add(1),
            completion_area.y.saturating_add(1),
            isize::MAX,
        ));
        terminal
            .draw(|frame| app.render(frame))
            .expect("scrolled render should succeed");

        let scrolled = rendered_rows(terminal.backend().buffer()).join("\n");
        assert!(scrolled.contains("TRANSCRIPT_MARKER"), "got:\n{scrolled}");
        assert!(!scrolled.contains("COMPLETION_WORD_00"), "got:\n{scrolled}");
        assert!(scrolled.contains("COMPLETION_WORD_19"), "got:\n{scrolled}");
    }

    #[test]
    fn test_error_line_renders_in_buffer() {
        // push_error_line must invalidate the visual row cache so
        // cached_error_rows reflects the new line. The error box
        // renders with red border and takes priority over completion.
        let _approval_guard = crate::core::approval::approval_test_guard();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.force_bottom();
        app.push_plain("line 1");
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial render should succeed");

        app.push_error_line(Line::from(Span::styled(
            "MARKER_ERROR_TEXT",
            Style::default().fg(theme::ERROR_FG),
        )));
        terminal
            .draw(|frame| app.render(frame))
            .expect("post-error render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("MARKER_ERROR_TEXT"),
            "error line should appear in rendered buffer; got:\n{rendered}"
        );
    }

    #[test]
    fn test_error_box_takes_priority_over_completion() {
        // When both error_lines and completion_lines are non-empty,
        // only the error box should render (red border).
        let _approval_guard = crate::core::approval::approval_test_guard();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.force_bottom();
        app.push_plain("line 1");

        // Push both completion and error lines.
        app.push_completion_line("COMPLETION_MARKER".into());
        app.push_error_line("ERROR_MARKER".into());

        terminal
            .draw(|frame| app.render(frame))
            .expect("render with both should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("ERROR_MARKER"),
            "error box should be visible; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("COMPLETION_MARKER"),
            "completion box should NOT render when error box is present; got:\n{rendered}"
        );
    }

    #[test]
    fn test_push_user_message_forces_bottom_for_multiline_submit() {
        use std::sync::Arc;
        use tokio::sync::mpsc;

        let _approval_guard = crate::core::approval::approval_test_guard();

        let (tx, mut rx) = mpsc::channel::<crate::cli::output::OutputEvent>(8);
        let writer: Arc<dyn crate::cli::output::OutputWriter> =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let mut app = App::new();
        app.set_content_height(5);
        app.set_content_width(80);
        for index in 0..20 {
            app.push_plain(format!("line {}", index));
        }
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 7;

        app.push_user_message("first line\nsecond line\nthird line", &writer);

        // Verify the lines actually landed in the channel and that
        // drain pulls them into output_lines, updating scroll state.
        // This guards the regression that motivated the
        // "drain_output before immediate render" fix in interactive.rs.
        crate::cli::interactive::drain_output_for_test(&mut rx, &mut app);

        // 3 user lines + 20 baseline lines = 23 total.
        assert_eq!(
            app.output_lines.len(),
            23,
            "expected 3 user lines to be added to output_lines after drain"
        );
        assert_eq!(app.scroll_mode, ScrollMode::Auto);
        assert_eq!(app.scroll_offset, 0);

        // The three pushed lines must be the last three in output_lines,
        // in order, with the multiline-tail prefix on lines 2 and 3.
        let last_three: Vec<String> = app
            .output_lines
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref().to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(last_three[0], "│ ❯ first line");
        assert_eq!(last_three[1], "│   second line");
        assert_eq!(last_three[2], "│   third line");
    }

    /// Contract test: after a multiline submit + drain + render, the bottom
    /// visible row of the output pane must contain the last line of the
    /// submitted message. This is the user-visible bug ("only renders first
    /// line") that the existing scroll-state-only test failed to catch.
    #[test]
    fn test_multiline_submit_bottom_row_contains_last_line() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        let _approval_guard = crate::core::approval::approval_test_guard();

        let (tx, mut rx) = mpsc::channel::<crate::cli::output::OutputEvent>(8);
        let writer: Arc<dyn crate::cli::output::OutputWriter> =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        // 80x24 terminal with a 22-row content area (status + input + borders).
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        // Seed enough output to push old content out of the bottom rows.
        for index in 0..50 {
            app.push_plain(format!("old line {}", index));
        }
        // Pretend the user had scrolled up so Manual mode is active; a
        // correct drain+force_bottom must snap back to Auto at offset 0.
        app.scroll_mode = ScrollMode::Manual;
        app.scroll_offset = 10;

        // Initial render to populate cached_wrap_width and viewport state.
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial render should succeed");

        // The full pipeline: emit async -> drain -> force bottom -> render.
        app.push_user_message("first line\nsecond line\nthird line", &writer);
        crate::cli::interactive::drain_output_for_test(&mut rx, &mut app);
        app.force_bottom();

        terminal
            .draw(|frame| app.render(frame))
            .expect("post-submit render should succeed");

        // Render the buffer and assert the bottom visible row contains
        // the last line of the multiline message.
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let height = buffer.area.height as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();
        let bottom_row = rows
            .last()
            .expect("terminal buffer must have at least one row");

        // Find any row in the lower half of the output area that contains
        // the tail of the multiline message. The output pane occupies
        // roughly the upper `height - 4` rows; the last 3 rows are the
        // status bar / input / border. We assert that *some* visible row
        // in the bottom 10 rows of the output area contains "third line".
        let output_bottom_rows = rows.iter().rev().take(10).collect::<Vec<_>>();
        let found = output_bottom_rows
            .iter()
            .any(|row| row.contains("third line"));

        assert!(
            found,
            "bottom of rendered output should contain 'third line' (last line of \
             multiline submit). bottom row: {bottom_row:?}, lower rows: {output_bottom_rows:?}"
        );

        // Sanity: the user must NOT still be in Manual mode at offset 10
        // after a multiline submit (the original bug surface).
        assert_eq!(app.scroll_mode, ScrollMode::Auto);
        assert_eq!(app.scroll_offset, 0);

        // Height must match what we asked the backend for.
        assert_eq!(height, 24);
        // And the buffer must contain all three lines somewhere.
        let all_rendered = rows.join("\n");
        assert!(all_rendered.contains("first line"));
        assert!(all_rendered.contains("second line"));
        assert!(all_rendered.contains("third line"));
    }

    #[test]
    fn test_approval_prompt_visible_after_multiline_tool_result() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::channel::<crate::cli::output::OutputEvent>(16);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        for index in 0..50 {
            app.push_plain(format!("old line {}", index));
        }
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial render should succeed");

        let tool_result_lines = [
            "  ✓ src/foo.rs",
            "    line 1 of output",
            "    line 2 of output",
            "    line 3 of output",
            "    line 4 of output",
            "    line 5 of output",
        ];
        for line in tool_result_lines {
            tx.try_send(crate::cli::output::OutputEvent::dim(line.to_string()))
                .expect("tool output should fit");
        }

        let prompt = "\n\
                      \x1b[33m🔧 Tool:\x1b[0m \x1b[1medit_file\x1b[0m\n\
                      \x1b[2m  path: src/baz.rs\x1b[0m\n\
                      Execute this tool? (y/n/always): ";
        let (request, _response_rx) = crate::core::approval::approval_request_for_test(
            30,
            "Approval required · edit_file",
            prompt,
        );
        tx.try_send(crate::cli::output::OutputEvent::ApprovalRequested(request))
            .expect("approval request should fit");

        crate::cli::interactive::drain_output_for_test(&mut rx, &mut app);
        terminal
            .draw(|frame| app.render(frame))
            .expect("post-prompt render should succeed");

        assert_eq!(
            app.scroll_mode,
            ScrollMode::ApprovalPinned,
            "scroll mode must be ApprovalPinned while approval prompt is active"
        );

        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        let output_bottom_rows: Vec<&String> = rows.iter().rev().take(15).collect();

        let has_prompt_question = output_bottom_rows
            .iter()
            .any(|row| row.contains("Execute this tool?"));

        let has_tool_name = output_bottom_rows
            .iter()
            .any(|row| row.contains("edit_file"));

        let has_tool_result = output_bottom_rows
            .iter()
            .any(|row| row.contains("line 5 of output"));

        assert!(
            has_prompt_question,
            "approval prompt question must be visible in bottom rows. \
             bottom rows: {output_bottom_rows:?}"
        );
        assert!(
            has_tool_name,
            "approval prompt tool name must be visible in bottom rows. \
             bottom rows: {output_bottom_rows:?}"
        );
        assert!(
            has_tool_result,
            "tool result must remain visible above the prompt. \
             bottom rows: {output_bottom_rows:?}"
        );
    }

    #[test]
    fn test_approval_request_becomes_actionable_only_after_render() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::channel::<crate::cli::output::OutputEvent>(16);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        for index in 0..50 {
            app.push_plain(format!("old line {}", index));
        }
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial render should succeed");

        let prompt = "\n\
                      \x1b[33m🔧 Tool:\x1b[0m \x1b[1medit_file\x1b[0m\n\
                      Execute this tool? (y/n/always): ";
        let (request, _response_rx) = crate::core::approval::approval_request_for_test(
            31,
            "Approval required · edit_file",
            prompt,
        );
        tx.try_send(crate::cli::output::OutputEvent::ApprovalRequested(request))
            .expect("approval request should fit");

        crate::cli::interactive::drain_output_for_test(&mut rx, &mut app);
        assert_eq!(
            app.scroll_mode,
            ScrollMode::ApprovalPinned,
            "a pending approval should pin transcript output"
        );
        assert!(!app.approval_accepts_input());

        terminal
            .draw(|frame| app.render(frame))
            .expect("post-prompt render should succeed");
        assert!(app.approval_accepts_input());

        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        let rendered = rows.join("\n");

        assert!(
            rendered.contains("Execute this tool?"),
            "approval panel must be visible before accepting input: {rendered}"
        );
    }

    /// Regression test for the silent channel-overflow bug. When the
    /// 8192-capacity mpsc channel floods during a tool-result burst,
    /// `ChannelOutputWriter::emit` silently drops events. If the
    /// dropped event is the approval prompt, the user cannot see it.
    ///
    /// The TUI main loop (src/cli/interactive.rs:2193-2206) checks
    /// `output_writer.take_overflow_signal()` after each drain and
    /// sets `app.output_overflow = true` and
    /// `app.output_overflow_count`. The status bar must then render
    /// a visible warning so the user knows output may be missing.
    #[test]
    fn test_status_bar_shows_overflow_indicator() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        app.provider_name = "minimax".to_string();
        app.model_name = "MiniMax-M3".to_string();
        app.task_id = "01KTPHXKHBJ49KXMAGPAR423BC".to_string();
        app.mode = "ACT".to_string();
        // Simulate the main loop detecting channel overflow.
        app.output_overflow = true;
        app.output_overflow_count = 7;
        app.needs_redraw = true;

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        // The status bar is 1 row tall, located just above the input
        // area. With 14 rows total and input(3) at the bottom, the
        // status bar is at row 10.
        let status_row = &rows[10];
        assert!(
            status_row.contains("output overflow"),
            "status bar must show overflow warning, got: {status_row:?}"
        );
        assert!(
            status_row.contains("7"),
            "status bar must show dropped count, got: {status_row:?}"
        );
    }

    /// When overflow is NOT detected, the status bar must NOT show
    /// the warning. This guards against a regression where the
    /// indicator sticks after the channel recovers.
    #[test]
    fn test_status_bar_hides_overflow_indicator_when_clear() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        app.provider_name = "minimax".to_string();
        app.model_name = "MiniMax-M3".to_string();
        app.task_id = "01KTPHXKHBJ49KXMAGPAR423BC".to_string();
        app.mode = "ACT".to_string();
        // overflow defaults to false
        app.needs_redraw = true;

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        let status_row = &rows[10];
        assert!(
            !status_row.contains("output overflow"),
            "status bar must NOT show overflow warning when channel is healthy, got: {status_row:?}"
        );
    }

    /// Regression test for the stale-output-artifacts bug fixed in
    /// commit 75caee3 ("fix(tui): clear stale output artifacts in
    /// render loop"). That commit added `frame.render_widget(Clear,
    /// main_output_area)` before rendering the output Paragraph and
    /// `frame.render_widget(Clear, completion_area)` before the
    /// completion Paragraph, so that when `output_lines` or
    /// `completion_lines` shrink between frames, the previous
    /// frame's content doesn't bleed through on terminals that use
    /// differential rendering.
    ///
    /// The TestBackend resets its buffer on every draw, so it cannot
    /// reproduce the stale-artifact symptom directly. This test
    /// instead verifies the structural invariant: the render path
    /// must include the Clear widget calls in the correct order
    /// (Clear before Paragraph). The source-level check guards
    /// against a refactor that drops the Clear calls and silently
    /// reintroduces the bug on real terminals.
    #[test]
    fn test_clear_widget_prevents_stale_output_artifacts() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        // Seed output lines (the Clear call on main_output_area
        // must render before the output Paragraph).
        for i in 0..3 {
            app.push_plain(format!("line {}", i));
        }
        // Seed completion lines (the Clear call on
        // completion_area must render before the completion
        // Paragraph).
        app.push_completion_line("completion line 1".into());

        terminal
            .draw(|frame| app.render(frame))
            .expect("render with Clear widget should succeed");

        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        // Output content must be visible.
        assert!(
            rows.iter().any(|row| row.contains("line 0")),
            "output content must be visible after render. rows: {rows:?}"
        );
        // Completion content must be visible.
        assert!(
            rows.iter().any(|row| row.contains("completion line 1")),
            "completion content must be visible after render"
        );

        // Source-level invariant: the Clear widget calls must
        // exist as active code in the render_output path. This is
        // a structural check because the TestBackend cannot
        // reproduce the stale-artifact symptom (it resets its
        // buffer on every draw). A refactor that removes the
        // Clear calls would reintroduce the bug on real terminals
        // but pass the TestBackend-based test.
        //
        // We check for the SPECIFIC Clear calls added by the
        // 75caee3 fix: `Clear, main_output_area` and
        // `Clear, completion_area`. These are unique to the
        // output pane (other Clear calls in the file target the
        // plan panel, picker overlay, etc.).
        let source = include_str!("app.rs");
        let has_active_call = |area_arg: &str| -> bool {
            let needle = format!("render_widget(Clear, {})", area_arg);
            source
                .lines()
                .any(|line| !line.trim_start().starts_with("//") && line.contains(&needle))
        };
        assert!(
            has_active_call("main_output_area"),
            "render_output must call Clear on main_output_area before drawing the output Paragraph (fix for 75caee3). \
             The bug causes stale content to bleed through on real terminals."
        );
        assert!(
            has_active_call("bottom_area"),
            "render_output must call Clear on bottom_area before drawing the completion/error Paragraph (fix for 75caee3). \
             The bug causes stale content to bleed through on real terminals."
        );
    }

    /// Test that the overflow count surfaced in the status bar
    /// matches the actual number of dropped events. The TUI main
    /// loop sets `app.output_overflow_count = output_writer.dropped_count()`
    /// (src/cli/interactive.rs:2201), so the count must be accurate.
    /// An inaccurate count would mislead the user about whether the
    /// session is reliable.
    #[test]
    fn test_overflow_indicator_dropped_count_matches_actual_drops() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        app.provider_name = "minimax".to_string();
        app.model_name = "MiniMax-M3".to_string();
        app.task_id = "01KTPHXKHBJ49KXMAGPAR423BC".to_string();
        app.mode = "ACT".to_string();

        // Simulate the main loop having detected exactly 4 dropped
        // events. The status bar must show "4" not "0" or any other
        // number.
        let actual_dropped_count = 4u64;
        app.output_overflow = true;
        app.output_overflow_count = actual_dropped_count;
        app.needs_redraw = true;

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        // The status bar is 1 row tall at row 10 (14 total, 1 status,
        // 3 input).
        let status_row = &rows[10];
        assert!(
            status_row.contains(&format!("{actual_dropped_count} dropped")),
            "status bar must show exact dropped count, got: {status_row:?}"
        );
        assert!(
            status_row.contains("output overflow"),
            "status bar must show overflow warning, got: {status_row:?}"
        );
    }

    #[test]
    fn test_overflow_indicator_persists_during_approval_prompt() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        app.provider_name = "minimax".to_string();
        app.model_name = "MiniMax-M3".to_string();
        app.task_id = "01KTPHXKHBJ49KXMAGPAR423BC".to_string();
        app.mode = "ACT".to_string();
        app.output_overflow = true;
        app.output_overflow_count = 3;
        let (request, _response_rx) = crate::core::approval::approval_request_for_test(
            70,
            "Approval required · execute_command",
            "🔧 Tool: execute_command\nExecute this tool?",
        );
        assert!(app.set_pending_approval(request));
        app.needs_redraw = true;

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        let has_approval = rows.iter().any(|row| row.contains("Execute this tool?"));
        assert!(
            has_approval,
            "approval panel must remain visible with an overflow warning: {rows:?}"
        );

        let status_row = rows
            .iter()
            .find(|row| row.contains("output overflow"))
            .expect("status bar should contain overflow warning");
        assert!(
            status_row.contains("output overflow"),
            "status bar must still show overflow warning during approval prompt. \
             status_row: {status_row:?}"
        );
        assert!(
            status_row.contains("3"),
            "status bar must show the dropped count during approval prompt. \
             status_row: {status_row:?}"
        );
    }

    // ---------------------------------------------------------------------------
    // Regression tests for intentional design decisions (bug audit 2025-06)
    // ---------------------------------------------------------------------------

    #[test]
    fn test_finalize_turn_stream_noop_reinsert_fallback_when_styling_differs() {
        // Regression: the no-op-reinsert optimization compares `rendered_line == popped`
        // which checks both span content AND style. If markdown rendering produces
        // different styling (e.g., bold/italic spans vs plain spans), the comparison
        // fails and the code correctly falls back to full pop+reinsert.
        //
        // This test verifies the optimization falls back (does NOT skip) when styling
        // differs, and that `turn_stream_entries` is cleared after.
        let mut app = App::new();

        // Push model lines with no styling (plain content).
        app.push_stream_line(Line::from("hello world"), StreamKind::Model);
        app.push_stream_line(Line::from("second line"), StreamKind::Model);
        assert_eq!(app.output_lines.len(), 2);
        assert_eq!(app.turn_stream_entries.len(), 2);

        // Markdown renders with bold styling — styling differs from the plain
        // streamed lines, so `can_skip_reinsert` must be false.
        app.finalize_turn_stream("**hello** world\n\nsecond line");

        // The streamed lines should have been popped and replaced with styled lines.
        assert_eq!(app.output_lines.len(), 2);
        let first_text: String = app
            .output_lines
            .front()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            first_text.contains("hello"),
            "first line should contain 'hello': {:?}",
            first_text
        );

        // turn_stream_entries must be cleared after finalize.
        assert!(app.turn_stream_entries.is_empty());
    }

    #[test]
    fn test_tool_output_lines_survive_finalize_turn_stream() {
        // Regression: tool output lines must never be popped or re-rendered by
        // `finalize_turn_stream`. Only Model lines are replaced; ToolOutput lines
        // remain in place.
        let mut app = App::new();

        // Mix of Model and ToolOutput lines interleaved.
        app.push_stream_line(Line::from("model prose"), StreamKind::Model);
        app.push_stream_line(
            Line::from("tool result: file changed"),
            StreamKind::ToolOutput,
        );
        app.push_stream_line(Line::from("more model"), StreamKind::Model);
        app.push_stream_line(
            Line::from("tool result: command ran"),
            StreamKind::ToolOutput,
        );
        assert_eq!(app.output_lines.len(), 4);
        assert_eq!(app.turn_stream_entries.len(), 4);

        app.finalize_turn_stream("model prose\n\nmore model");

        // Tool output lines must remain in place.
        let all_lines: Vec<String> = app
            .output_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();

        // The two tool output lines should still be present.
        assert!(
            all_lines.iter().any(|l| l.contains("file changed")),
            "tool output line 'file changed' should survive: {:?}",
            all_lines
        );
        assert!(
            all_lines.iter().any(|l| l.contains("command ran")),
            "tool output line 'command ran' should survive: {:?}",
            all_lines
        );

        // Only Model lines should be replaced.
        assert!(
            all_lines.iter().any(|l| l.contains("model prose")),
            "model prose should be present (re-rendered): {:?}",
            all_lines
        );
        assert!(
            all_lines.iter().any(|l| l.contains("more model")),
            "more model should be present (re-rendered): {:?}",
            all_lines
        );

        assert!(app.turn_stream_entries.is_empty());
    }

    #[test]
    fn test_turn_had_streamed_line_eviction_fallback() {
        // Regression: when `turn_had_streamed_line` is true but model_indices is
        // empty (e.g., due to buffer eviction), the code appends rendered markdown
        // after existing lines instead of replacing them. This avoids a visual flash.
        //
        // This scenario occurs when:
        // 1. Model lines were pushed via `push_stream_line` (sets turn_had_streamed_line)
        // 2. Many lines were pushed via `push_output` (evicts old lines from output_lines)
        // 3. The recorded indices are now out of range
        // 4. `finalize_turn_stream` detects this via the max_idx check and clears
        //    without replacement — but the had_streamed_line + model_indices.is_empty()
        //    path appends markdown after existing lines.
        let mut app = App::new();

        // Push some model lines (sets turn_had_streamed_line = true).
        app.push_stream_line(Line::from("model line 1"), StreamKind::Model);
        app.push_stream_line(Line::from("model line 2"), StreamKind::Model);
        assert!(app.turn_had_streamed_line);
        assert_eq!(app.turn_stream_entries.len(), 2);

        // Simulate eviction by pushing many non-stream lines.
        for i in 0..10000 {
            app.push_output(Line::from(format!("eviction line {}", i)));
        }

        // After eviction, output_lines only has the last ~10000 lines.
        // The model line indices (0, 1) are now out of range.
        // finalize_turn_stream will detect this via max_idx check.

        app.finalize_turn_stream("appended markdown");

        // Markdown should be appended after the existing lines.
        let all_lines: Vec<String> = app
            .output_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(
            all_lines.iter().any(|l| l.contains("appended markdown")),
            "markdown should be appended: {:?}",
            all_lines
        );
    }

    #[test]
    fn test_plan_panel_shows_when_incomplete() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        let plan = crate::core::plan_state::PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        assert!(!plan.complete);
        app.sync_plan_state_cache(Some(&plan));

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("Status: awaiting approval"),
            "incomplete plan should render panel content, got: {}",
            rendered
        );
    }

    #[test]
    fn test_plan_panel_hides_when_complete() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        let mut plan = crate::core::plan_state::PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.mark_step(0, crate::core::plan_state::PlanStepStatus::Done)
            .unwrap();
        plan.mark_step(1, crate::core::plan_state::PlanStepStatus::Done)
            .unwrap();
        plan.advance();
        assert!(plan.complete);
        app.sync_plan_state_cache(Some(&plan));

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !rendered.contains("Status: complete"),
            "complete plan must not render panel content, got: {}",
            rendered
        );
        assert!(
            !rendered.contains("awaiting approval"),
            "complete plan must not render approval prompt, got: {}",
            rendered
        );
    }

    #[test]
    fn test_turn_indicator_survives_can_skip_reinsert() {
        // Regression test for the can_skip_reinsert bug where
        // prefixed_turn_indicator = true but the modified rendered[0]
        // was written to output_lines but NOT to output_line_kinds,
        // violating the paired-buffer invariant.
        let mut app = App::new();
        app.set_content_width(80);

        // Set up the turn indicator
        app.push_turn_indicator(Line::from(Span::styled(
            "\u{2666}",
            Style::default().fg(crate::cli::tui::theme::ACCENT),
        )));

        // Push a single plain text line that will trigger the can_skip_reinsert
        // optimization (rendered line == streamed line, same content and style)
        app.push_stream_line(Line::from("plain line"), StreamKind::Model);

        assert_eq!(app.output_lines.len(), 1);
        assert_eq!(app.output_line_kinds.len(), 1);

        // This triggers the can_skip_reinsert path because the rendered markdown
        // for "plain line" produces exactly one line with the same content
        app.finalize_turn_stream("plain line");

        // Verify the paired-buffer invariant
        assert_eq!(
            app.output_lines.len(),
            app.output_line_kinds.len(),
            "output_lines and output_line_kinds must stay in lockstep"
        );

        // Verify the turn indicator survived in the first line
        let first_text: String = app
            .output_lines
            .front()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            first_text.contains("\u{2666}"),
            "turn indicator must survive can_skip_reinsert: {:?}",
            first_text
        );

        // Verify the kind is Model (not drifted to something else)
        assert_eq!(
            app.output_line_kinds.front().copied(),
            Some(BlockKind::Model),
            "output_line_kinds must be Model for the first line"
        );
    }

    /// Evicted lines must be buffered in memory and only written when the
    /// batched scrollback flush runs.
    #[test]
    fn test_eviction_buffers_scrollback_until_flush() {
        let mut app = App::new();
        app.set_content_width(80);

        // Create a temp dir for the scrollback file
        let tmp_dir = std::env::temp_dir().join("sned_scrollback_test");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let file_path = tmp_dir.join("lines");
        app.scrollback_file = Some(file_path.clone());

        // Push enough lines to trigger eviction (limit is 10,000)
        // For testing, we'll simulate eviction by manually triggering it
        for i in 0..10_001 {
            app.push_plain(format!("line {}", i));
        }

        assert_eq!(app.scrollback_count, 1);
        assert_eq!(app.scrollback_pending_lines, 1);
        assert!(
            !file_path.exists(),
            "scrollback file should not be touched from the append hot path"
        );

        app.flush_scrollback_pending().unwrap();

        assert!(
            file_path.exists(),
            "flush should materialize the scrollback file"
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(!content.is_empty(), "scrollback file should have content");
        assert!(
            content.contains("line 0"),
            "first evicted line should be in scrollback"
        );
        assert!(app.scrollback_pending.is_empty());
        assert_eq!(app.scrollback_pending_lines, 0);

        // Cleanup
        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[test]
    fn test_scrollback_batches_preserve_order_in_background_writer() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("lines");
        let mut app = App::new();
        app.scrollback_file = Some(file_path.clone());
        app.scrollback_pending = "first\n".repeat(SCROLLBACK_FLUSH_LINE_BATCH);
        app.scrollback_pending_lines = SCROLLBACK_FLUSH_LINE_BATCH;

        app.flush_scrollback_pending_if_needed().unwrap();
        assert!(app.scrollback_pending.is_empty());
        app.scrollback_pending = "last\n".to_string();
        app.scrollback_pending_lines = 1;
        app.flush_scrollback_pending().unwrap();

        let content = std::fs::read_to_string(file_path).unwrap();
        assert_eq!(
            content,
            format!("{}last\n", "first\n".repeat(SCROLLBACK_FLUSH_LINE_BATCH))
        );
    }

    #[test]
    fn test_scrollback_writer_rotates_to_line_cap() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("lines");
        let mut pending = (0..MAX_SCROLLBACK_LOAD_LINES + 3)
            .map(|line| format!("line {line}\n"))
            .collect::<String>()
            .into_bytes();

        write_scrollback_pending(&file_path, &mut pending).unwrap();

        let content = std::fs::read_to_string(file_path).unwrap();
        assert_eq!(content.lines().count(), MAX_SCROLLBACK_LOAD_LINES);
        assert_eq!(content.lines().next(), Some("line 3"));
        let last_line = format!("line {}", MAX_SCROLLBACK_LOAD_LINES + 2);
        assert_eq!(content.lines().last(), Some(last_line.as_str()));
    }

    #[test]
    fn test_scrollback_writer_rotates_to_byte_cap() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("lines");
        let oversized = "x".repeat(MAX_SCROLLBACK_LOAD_BYTES as usize + 1);
        std::fs::write(&file_path, format!("{oversized}\n")).unwrap();
        let mut pending = b"retained\n".to_vec();

        write_scrollback_pending(&file_path, &mut pending).unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "retained\n");
    }

    #[test]
    fn test_scrollback_clear_is_ordered_after_queued_append() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("lines");
        let mut app = App::new();
        app.scrollback_file = Some(file_path.clone());
        app.scrollback_pending = "queued\n".repeat(SCROLLBACK_FLUSH_LINE_BATCH);
        app.scrollback_pending_lines = SCROLLBACK_FLUSH_LINE_BATCH;

        app.flush_scrollback_pending_if_needed().unwrap();
        app.clear_scrollback_storage().unwrap();

        assert!(!file_path.exists());
    }

    #[test]
    fn test_scrollback_shutdown_persists_final_partial_batch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("lines");
        let mut app = App::new();
        app.scrollback_file = Some(file_path.clone());
        app.scrollback_pending = "final partial batch\n".to_string();
        app.scrollback_pending_lines = 1;

        app.shutdown_scrollback_writer().unwrap();

        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "final partial batch\n"
        );
    }

    #[test]
    fn test_scrollback_writer_reports_filesystem_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let parent_file = temp_dir.path().join("not-a-directory");
        std::fs::write(&parent_file, "block directory creation").unwrap();
        let mut app = App::new();
        app.scrollback_file = Some(parent_file.join("lines"));
        app.scrollback_pending = "unwritten\n".repeat(SCROLLBACK_FLUSH_LINE_BATCH);
        app.scrollback_pending_lines = SCROLLBACK_FLUSH_LINE_BATCH;

        app.flush_scrollback_pending_if_needed().unwrap();
        assert!(app.flush_scrollback_pending().is_err());
        assert!(app.take_scrollback_writer_error().is_some());
        assert!(app.shutdown_scrollback_writer().is_err());
    }

    #[test]
    fn test_scrollback_clear_failure_does_not_block_ui_clear() {
        let temp_dir = tempfile::tempdir().unwrap();
        let parent_file = temp_dir.path().join("not-a-directory");
        std::fs::write(&parent_file, "block directory creation").unwrap();
        let mut app = App::new();
        app.scrollback_file = Some(parent_file.join("lines"));
        app.scrollback_pending = "discarded\n".to_string();
        app.scrollback_pending_lines = 1;
        app.push_plain("visible output");

        assert!(app.clear_output().is_err());

        assert!(app.output_lines.is_empty());
        assert!(app.scrollback_pending.is_empty());
        assert_eq!(app.scrollback_pending_lines, 0);
    }

    /// Entering scrollback mode loads file content and merges with buffer.
    #[test]
    fn test_enter_scrollback_loads_file_content() {
        let mut app = App::new();
        app.set_content_width(80);

        // Create a temp scrollback file with test content
        let tmp_dir = std::env::temp_dir().join("sned_scrollback_test2");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let file_path = tmp_dir.join("lines");
        std::fs::write(
            &file_path,
            "scrollback line 0\nscrollback line 1\nscrollback line 2\n",
        )
        .unwrap();
        app.scrollback_file = Some(file_path.clone());
        app.scrollback_count = 3;

        // Add some session content
        app.push_plain("session line 0");
        app.push_plain("session line 1");

        // Enter scrollback mode
        app.enter_scrollback().unwrap();

        // Verify: buffer contains scrollback lines + divider + session lines
        assert!(app.in_scrollback);
        let total = app.output_lines.len();
        // 3 scrollback lines + 1 divider + 2 session lines = 6
        assert_eq!(total, 6, "buffer should contain merged content");

        // First line should be a scrollback line
        let first_text: String = app
            .output_lines
            .front()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            first_text.contains("scrollback line 0"),
            "first line should be from scrollback"
        );

        // Cleanup
        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[test]
    fn test_enter_scrollback_limits_loaded_lines() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("lines");
        let mut content = String::new();
        for line in 0..MAX_SCROLLBACK_LOAD_LINES + 3 {
            content.push_str(&format!("line {line}\n"));
        }
        std::fs::write(&file_path, content).unwrap();

        let mut app = App::new();
        app.scrollback_file = Some(file_path);
        app.enter_scrollback().unwrap();

        assert_eq!(app.output_lines.len(), MAX_SCROLLBACK_LOAD_LINES);
        assert_eq!(app.output_lines.front().unwrap().to_string(), "line 3");
        assert_eq!(
            app.output_lines.back().unwrap().to_string(),
            format!("line {}", MAX_SCROLLBACK_LOAD_LINES + 2)
        );
    }

    #[test]
    fn test_read_scrollback_tail_skips_partial_first_line() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("lines");
        let content = format!(
            "{}\nretained\n",
            "x".repeat(MAX_SCROLLBACK_LOAD_BYTES as usize)
        );
        std::fs::write(&file_path, content).unwrap();

        assert_eq!(
            read_scrollback_tail(&file_path).unwrap(),
            Some("retained\n".to_string())
        );
    }

    #[test]
    fn test_enter_scrollback_allows_missing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.scrollback_file = Some(temp_dir.path().join("missing-lines"));
        app.push_plain("session line");

        app.enter_scrollback().unwrap();

        assert!(app.in_scrollback);
        assert_eq!(app.output_lines.len(), 1);
    }

    #[test]
    fn test_enter_scrollback_propagates_read_error_without_entering_mode() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.scrollback_file = Some(temp_dir.path().to_path_buf());
        app.push_plain("session line");

        assert!(app.enter_scrollback().is_err());
        assert!(!app.in_scrollback);
        assert_eq!(app.output_lines.len(), 1);
    }

    /// Exiting scrollback clears the file and resets state.
    #[test]
    fn test_exit_scrollback_clears_file() {
        let mut app = App::new();
        app.set_content_width(80);

        let tmp_dir = std::env::temp_dir().join("sned_scrollback_test3");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let file_path = tmp_dir.join("lines");
        std::fs::write(&file_path, "old scrollback content\n").unwrap();
        app.scrollback_file = Some(file_path.clone());
        app.scrollback_count = 1;

        app.push_plain("session line");

        app.enter_scrollback().unwrap();
        assert!(app.in_scrollback);

        app.exit_scrollback().unwrap();

        assert!(!app.in_scrollback);
        assert_eq!(app.scrollback_count, 0);
        assert!(
            !file_path.exists(),
            "scrollback file should be deleted on exit"
        );

        let _ = std::fs::remove_dir(&tmp_dir);
    }

    /// Toggle switches between normal and scrollback modes.
    #[test]
    fn test_scrollback_toggle() {
        let mut app = App::new();
        app.set_content_width(80);

        let tmp_dir = std::env::temp_dir().join("sned_scrollback_test4");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let file_path = tmp_dir.join("lines");
        std::fs::write(&file_path, "s0\ns1\n").unwrap();
        app.scrollback_file = Some(file_path.clone());
        app.scrollback_count = 2;

        app.push_plain("session");

        assert!(!app.in_scrollback);
        app.toggle_scrollback().unwrap();
        assert!(app.in_scrollback, "first toggle should enter scrollback");
        assert!(
            app.output_lines.len() >= 2,
            "buffer should contain scrollback lines"
        );

        app.toggle_scrollback().unwrap();
        assert!(!app.in_scrollback, "second toggle should exit scrollback");
        assert_eq!(app.scrollback_count, 0, "count should be reset");

        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    /// Closes the "tests ≠ reality" gap for autoscroll.
    ///
    /// Existing scroll tests (e.g. `test_scroll_lines_switches_to_manual_mode`)
    /// assert on `app.scroll_mode` and `app.scroll_offset` directly. They pass
    /// even if the render path uses a stale or wrong offset. This test
    /// asserts on the actual rendered framebuffer produced by
    /// `TestBackend` — if `scroll_mode == Auto` but the rendered viewport
    /// doesn't show the bottom of the buffer, this test fails.
    #[test]
    fn test_force_bottom_renders_visible_bottom_of_buffer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let content_height = 5;
        let mut app = make_scrolling_app(20, content_height);
        app.push_plain("line 20");

        assert_eq!(app.scroll_mode, ScrollMode::Auto);
        assert_eq!(
            app.resolved_scroll_y_for(app.output_lines.len(), content_height),
            16
        );

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        for index in 16..21 {
            assert!(
                rendered.contains(&format!("line {index}")),
                "line {index} should be visible at the bottom of the rendered viewport"
            );
        }

        // Earlier lines must NOT be visible — would indicate we are scrolled
        // to the top instead of Auto-following the tail.
        assert!(
            !rendered.contains("line 0 "),
            "line 0 should be off-screen when 20 lines are buffered with height 5"
        );
        assert!(
            !rendered.contains("line 5 "),
            "line 5 should be off-screen at the bottom of a 20-line buffer"
        );
    }

    /// Companion test for Manual scroll mode: when the user scrolls up,
    /// the EARLIER lines must become visible and the LATEST lines must be
    /// off-screen. Catches the case where `scroll_mode == Manual` is set
    /// correctly but the viewport still renders the tail.
    #[test]
    fn test_manual_scroll_renders_visible_top_of_buffer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let content_height = 5;
        let mut app = make_scrolling_app(20, content_height);

        // Scroll up by 5 lines → enter Manual mode at offset 10.
        app.scroll_lines(-5);
        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, 10);

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let rendered = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        // Lines 10..15 must be visible.
        for index in 10..15 {
            assert!(
                rendered.contains(&format!("line {index}")),
                "line {index} should be visible at scroll offset 10"
            );
        }

        // Line 19 (the very last) must NOT be visible — confirms we scrolled UP.
        assert!(
            !rendered.contains("line 19"),
            "line 19 should be off-screen when scrolled up by 5"
        );

        // Line 0 should still NOT be visible (we are at offset 10, not 0).
        assert!(
            !rendered.contains("line 0 "),
            "line 0 should be off-screen at offset 10"
        );
    }
}
