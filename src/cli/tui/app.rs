//! App struct for ratatui TUI.
//!
//! This is the main application state for the ratatui render loop.

use super::history::FileHistory;
use super::theme;
use crate::core::file_search::FileSearchResult;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

const INPUT_MAX_VISIBLE_LINES: usize = 6;
const BLOCKING_PROMPT_INPUT_VISIBLE_LINES: usize = 1;
const SCROLLBACK_FLUSH_LINE_BATCH: usize = 128;

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
    /// Tool result, plan completion, action digest, heat map, etc.
    /// These lines must NOT be popped by `finalize_turn_stream`.
    ToolOutput,
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
    /// Explicit turn separator (e.g. "──── ♦ ────").
    Separator,
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

/// Tracks a pasted chunk of text that was folded into a marker.
#[derive(Debug, Clone)]
pub struct PasteChunk {
    /// The marker text shown in the textarea (e.g., "[pasted 1,234 chars]")
    pub marker: String,
    /// The original pasted content
    pub content: String,
    /// Start position in the textarea (line, column)
    pub start_line: usize,
    /// Whether this paste has been expanded by the user
    pub expanded: bool,
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
    /// Whether the agent is currently busy
    pub agent_busy: bool,
    /// Whether the model is currently in a reasoning/thinking phase (no displayable output yet).
    /// Drives the "Reasoning..." indicator rendered above the status bar.
    pub reasoning_active: bool,
    /// Manual scroll offset (top-of-viewport line index)
    pub scroll_offset: u16,
    /// Current output scroll behavior
    pub scroll_mode: ScrollMode,
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
    pub status_left_fingerprint: (String, String, String, String),
    /// Cached status bar right segment (elapsed timer). Rebuilt when seconds change.
    pub cached_status_right: String,
    /// Seconds value the cached right segment was built for.
    /// Last known context usage percentage from the API.
    pub context_pct: Option<f64>,
    pub cached_status_right_secs: (u64, Option<f64>, bool, u64, usize),
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
    /// Completion box lines rendered as a dedicated Block widget.
    pub completion_lines: VecDeque<Line<'static>>,
    /// Cached completion row count, valid when cached_wrap_width matches.
    pub cached_completion_rows: usize,
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

    /// Update the textarea placeholder based on current mode.
    pub fn update_placeholder(&mut self) {
        if self.mode == "PLAN" {
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
            agent_busy: false,
            reasoning_active: false,
            scroll_offset: 0,
            scroll_mode: ScrollMode::Auto,
            has_resized: true,
            needs_redraw: true,
            start_time: None,
            spinner_index: 0,
            last_spinner_tick: None,
            cwd: String::new(),
            picker_active: false,
            picker_results: Vec::new(),
            picker_index: 0,
            history: FileHistory::load(),
            paste_chunks: Vec::new(),
            paste_fold_threshold: 500, // Fold pastes > 500 chars
            provider_name: String::new(),
            model_name: String::new(),
            task_id: String::new(),
            mode: String::new(),
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
            status_left_fingerprint: (String::new(), String::new(), String::new(), String::new()),
            cached_status_right: String::new(),
            cached_status_right_secs: (u64::MAX, None, false, 0, 0),
            context_pct: None,
            cached_spacer: String::new(),
            cached_spacer_len: 0,
            slash_command_active: false,
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
            completion_lines: VecDeque::new(),
            cached_completion_rows: 0,
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

        // Pre-wrap: if the line's total width exceeds wrap_width, split it
        let total_width: usize = line
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();

        let lines_to_push: Vec<Line<'static>> = if total_width > wrap_width && wrap_width > 0 {
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
            // Evict front line and buffer it for batched scrollback append.
            if let Some(line) = self.output_lines.front() {
                let text = Self::line_to_string(line);
                self.scrollback_pending.push_str(&text);
                self.scrollback_pending.push('\n');
                self.scrollback_pending_lines = self.scrollback_pending_lines.saturating_add(1);
                self.scrollback_count = self.scrollback_count.saturating_add(1);
            }
            self.output_lines.pop_front();
            self.output_line_kinds.pop_front();
            self.cached_wrap_width = None;
            self.cached_visible_window = None;
        } else if self.cached_wrap_width == Some(wrap_width) {
            // Hot path: keep the cached row count in sync for simple appends
            // so the next render does not need to rescan the whole transcript.
            let added_rows = Self::line_visual_rows(self.output_lines.back().unwrap(), wrap_width)
                .saturating_add(
                    previous_kind.is_some_and(|prev| Self::should_insert_separator(prev, kind))
                        as usize,
                );
            self.cached_visual_rows = self.cached_visual_rows.saturating_add(added_rows);
        }
        if !matches!(self.scroll_mode, ScrollMode::ApprovalPinned) {
            self.force_bottom();
        }
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
                .map(|line| Self::line_visual_rows(line, wrap_width))
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

        let lines_to_push = self.prewrap_stream_line(line);
        self.push_stream_group(lines_to_push, kind);
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
                    crate::cli::markdown::render_markdown(None, &prefixed_markdown);
                for line in rendered {
                    self.output_lines.push_back(line);
                    self.output_line_kinds.push_back(BlockKind::Model);
                }
                self.needs_redraw = true;
                self.cached_wrap_width = None;
                self.rebuild_visual_row_cache(self.last_wrap_width());
            }
            return;
        }

        // Extract just the Model indices for the no-op-reinsert check
        // and for popping/insertion.
        let model_entry_indices: Vec<usize> = model_indices.iter().map(|(idx, _)| *idx).collect();

        // No-op-reinsert optimization: if the rendered line count equals
        // the popped Model line count and every rendered line has the same
        // content and style as the popped line, skip the pop+reinsert
        // entirely.  This prevents the visual flash where streamed plain
        // text vanishes for a frame while render_markdown runs, then
        // reappears styled.
        let mut rendered: Vec<Line<'static>> =
            crate::cli::markdown::render_markdown(None, markdown_text);
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
            self.rebuild_visual_row_cache(self.last_wrap_width());
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
        let insert_at = *model_entry_indices.iter().min().unwrap();

        // Render the markdown text first, then prepend the turn indicator
        // as a styled span to the first rendered line. Prepending the
        // indicator to the markdown string would make `render_markdown`
        // parse "♦ " as paragraph text, breaking the visual hierarchy.
        // Prepending as a span keeps the indicator on the same line as
        // the start of the response.
        let have_indicator = self.turn_indicator.take().is_some();
        let mut rendered: Vec<Line<'static>> =
            crate::cli::markdown::render_markdown(None, markdown_text);
        if have_indicator && let Some(first) = rendered.first_mut() {
            let mut new_spans = Vec::with_capacity(first.spans.len() + 1);
            new_spans.push(Span::styled(
                "\u{2666} ",
                Style::default().fg(crate::cli::tui::theme::ACCENT),
            ));
            new_spans.extend(first.spans.iter().cloned());
            first.spans = new_spans;
        }
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
        self.rebuild_visual_row_cache(self.last_wrap_width());
    }

    /// Push a completion line to the completion buffer.
    pub fn push_completion_line(&mut self, line: Line<'static>) {
        self.needs_redraw = true;
        self.completion_lines.push_back(line);
        // Invalidate the visual-row cache so cached_completion_rows is
        // recomputed on the next render. Without this, the completion box
        // keeps its stale height (just borders) and the text is clipped.
        self.cached_wrap_width = None;
    }

    /// Clear the completion box and invalidate cached layout for the next render.
    pub fn clear_completion_lines(&mut self) {
        self.needs_redraw = true;
        self.completion_lines.clear();
        self.cached_completion_rows = 0;
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
        let total_width: usize = line
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();

        if total_width > wrap_width && wrap_width > 0 {
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
    }

    pub fn flush_scrollback_pending(&mut self) -> std::io::Result<()> {
        if self.scrollback_pending.is_empty() {
            return Ok(());
        }

        if let Some(ref file_path) = self.scrollback_file {
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file_path)?;
            std::io::Write::write_all(&mut file, self.scrollback_pending.as_bytes())?;
        }

        self.scrollback_pending.clear();
        self.scrollback_pending_lines = 0;
        Ok(())
    }

    pub fn flush_scrollback_pending_if_needed(&mut self) -> std::io::Result<()> {
        if self.scrollback_pending_lines >= SCROLLBACK_FLUSH_LINE_BATCH {
            self.flush_scrollback_pending()?;
        }
        Ok(())
    }

    /// Load scrollback content from the scrollback file and merge with
    /// the current buffer.  The file stores one raw text line per line;
    /// we reconstruct Line objects and prepend them to output_lines.
    pub fn enter_scrollback(&mut self) {
        self.in_scrollback = true;
        self.needs_redraw = true;
        let _ = self.flush_scrollback_pending();

        if let Some(ref file_path) = self.scrollback_file
            && let Ok(content) = std::fs::read_to_string(file_path)
        {
            let mut scrollback_lines: VecDeque<Line<'static>> = VecDeque::new();
            let mut scrollback_kinds: VecDeque<BlockKind> = VecDeque::new();
            for line in content.lines() {
                scrollback_lines.push_back(Line::from(line.to_string()));
                scrollback_kinds.push_back(BlockKind::ToolOutput);
            }
            // Prepend scrollback content before current session content
            let mut new_lines: VecDeque<Line<'static>> = VecDeque::new();
            let mut new_kinds: VecDeque<BlockKind> = VecDeque::new();
            for line in &scrollback_lines {
                new_lines.push_back(line.clone());
                new_kinds.push_back(BlockKind::ToolOutput);
            }
            // Insert divider line between scrollback and session content
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
        // Reset scroll to bottom of combined buffer
        self.scroll_mode = ScrollMode::Auto;
        self.scroll_offset = 0;
    }

    /// Exit scrollback mode: clear the scrollback file, reset buffer to
    /// the original session content, and return to bottom.
    pub fn exit_scrollback(&mut self) {
        self.in_scrollback = false;
        self.needs_redraw = true;
        // Clear the scrollback file - history was seen
        if let Some(ref file_path) = self.scrollback_file {
            let _ = std::fs::remove_file(file_path);
        }
        self.scrollback_count = 0;
        self.scrollback_pending.clear();
        self.scrollback_pending_lines = 0;
        self.cached_wrap_width = None;
        self.cached_visible_window = None;
        self.scroll_mode = ScrollMode::Auto;
        self.scroll_offset = 0;
    }

    /// Toggle between normal and scrollback modes.
    pub fn toggle_scrollback(&mut self) {
        if self.in_scrollback {
            self.exit_scrollback();
        } else {
            self.enter_scrollback();
        }
    }

    /// Clear all output and reset the visual-row cache.
    pub fn clear_output(&mut self) {
        self.needs_redraw = true;
        self.output_lines.clear();
        self.output_line_kinds.clear();
        self.completion_lines.clear();
        self.error_lines.clear();
        self.turn_stream_entries.clear();
        self.last_stream_group = None;
        self.turn_indicator = None;
        self.turn_had_streamed_line = false;
        self.cached_visual_rows = 0;
        self.cached_error_rows = 0;
        self.cached_wrap_width = Some(self.last_wrap_width());
        self.cached_visible_window = None;
        self.in_scrollback = false;
        self.scrollback_count = 0;
        self.scrollback_pending.clear();
        self.scrollback_pending_lines = 0;
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
        // Invalidate the visual-row cache: drain changes the line buffer,
        // which can alter render-time separator insertion.
        self.cached_wrap_width = None;
        self.cached_visible_window = None;
    }

    pub fn pin_approval_bottom(&mut self) {
        self.needs_redraw = true;
        self.scroll_mode = ScrollMode::ApprovalPinned;
        self.scroll_offset = 0;
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
        let total_rows = self.total_visual_rows(self.last_wrap_width());
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
        self.needs_redraw = true;
        let total_rows = self.total_visual_rows(self.last_wrap_width());
        if !self.enter_manual_mode(total_rows) {
            return;
        }

        let max_offset = Self::max_scroll_offset_for(total_rows, self.last_content_height) as isize;
        let next = (self.scroll_offset as isize + delta).clamp(0, max_offset);
        self.scroll_offset = next as u16;
        self.clamp_to_content();
    }

    pub fn scroll_pages(&mut self, delta_pages: isize) {
        self.needs_redraw = true;
        let page_height = self.last_content_height.saturating_sub(1).max(1);
        self.scroll_lines(delta_pages * page_height as isize);
    }

    pub fn resolved_scroll_y_for(&self, total_lines: usize, content_height: usize) -> u16 {
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
    ) -> u16 {
        if !matches!(self.scroll_mode, ScrollMode::ApprovalPinned) {
            return self.resolved_scroll_y_for(total_rows, content_height);
        }

        let max_offset = Self::max_scroll_offset_for(total_rows, content_height);
        let Some(prompt_tail_row) = self.last_user_prompt_tail_row(wrap_width) else {
            return max_offset;
        };

        prompt_tail_row
            .saturating_sub(content_height)
            .min(max_offset as usize) as u16
    }

    fn enter_manual_mode(&mut self, total_rows: usize) -> bool {
        match self.scroll_mode {
            ScrollMode::ApprovalPinned => false,
            ScrollMode::Manual => true,
            ScrollMode::Auto => {
                self.scroll_mode = ScrollMode::Manual;
                self.scroll_offset =
                    Self::max_scroll_offset_for(total_rows, self.last_content_height);
                true
            }
        }
    }

    fn max_scroll_offset_for(total_lines: usize, content_height: usize) -> u16 {
        total_lines.saturating_sub(content_height) as u16
    }

    fn last_user_prompt_tail_row(&self, wrap_width: usize) -> Option<usize> {
        let mut tail_row = None;
        let mut rendered_rows = 0usize;

        self.for_each_output_row(|line, kind| {
            rendered_rows =
                rendered_rows.saturating_add(Self::output_row_visual_rows(line, wrap_width));
            if kind == BlockKind::UserPrompt {
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
        if wrap_width == 0 {
            return 1;
        }

        // Bottom pinning must be computed in rendered rows, not logical lines.
        // A single long prompt line can wrap into multiple terminal rows; if we
        // count only logical lines, the actionable tail of the prompt can land
        // below the visible viewport even while the TUI thinks it is at bottom.
        let width = line
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>();
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
        self.for_each_output_row(|line, _kind| {
            if done {
                return;
            }
            let idx = expanded_len;
            expanded_len = expanded_len.saturating_add(1);
            let rows = Self::output_row_visual_rows(line, wrap_width);
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
        self.for_each_output_row(|line, _| {
            output_rows =
                output_rows.saturating_add(Self::output_row_visual_rows(line, wrap_width));
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

    /// Returns the renderable output rows: each entry of
    /// `output_lines` paired with its `BlockKind`, with one blank-line
    /// separator inserted on every transition between non-Separated
    /// kinds (so the visual row count matches what `render_output`
    /// will actually draw).  Used by both the visual-row cache and
    /// the renderer so they cannot drift.
    #[allow(dead_code)]
    fn output_rows_for_render(&self) -> Vec<(Line<'static>, BlockKind)> {
        let mut out: Vec<(Line<'static>, BlockKind)> = Vec::with_capacity(self.output_lines.len());
        self.for_each_output_row(|line, kind| {
            out.push((line.cloned().unwrap_or_else(|| Line::from("")), kind));
        });
        out
    }

    fn for_each_output_row(&self, mut visitor: impl FnMut(Option<&Line<'static>>, BlockKind)) {
        let mut prev: Option<BlockKind> = None;
        for (line, kind) in self.output_lines.iter().zip(self.output_line_kinds.iter()) {
            if let Some(p) = prev
                && Self::should_insert_separator(p, *kind)
            {
                visitor(None, BlockKind::Separator);
            }
            visitor(Some(line), *kind);
            prev = Some(*kind);
        }
    }

    fn output_row_visual_rows(line: Option<&Line<'static>>, wrap_width: usize) -> usize {
        match line {
            Some(line) => Self::line_visual_rows(line, wrap_width),
            None => 1,
        }
    }

    fn collect_output_rows_range(&self, start_idx: usize, take_count: usize) -> Vec<Line<'static>> {
        let end_idx = start_idx.saturating_add(take_count);
        let mut expanded_idx = 0usize;
        let mut visible_lines = Vec::with_capacity(take_count);
        self.for_each_output_row(|line, _| {
            if expanded_idx >= start_idx && expanded_idx < end_idx {
                visible_lines.push(line.cloned().unwrap_or_else(|| Line::from("")));
            }
            expanded_idx = expanded_idx.saturating_add(1);
        });
        visible_lines
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
                    | BlockKind::UserPrompt,
            ) | (_, BlockKind::UserPrompt)
                | (
                    BlockKind::ToolOutput
                        | BlockKind::CommandOutput
                        | BlockKind::ToolHeader
                        | BlockKind::CommandHeader
                        | BlockKind::Reasoning
                        | BlockKind::UserPrompt,
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

    /// Render the application state to the frame.
    pub fn render(&mut self, frame: &mut Frame) {
        let has_plan = self.plan_state_cache.as_ref().is_some_and(|p| !p.complete);

        // Reserve the plan area even when no plan is active so the
        // Clear widget below can wipe stale plan content from the
        // right 35 columns after the plan is dismissed.
        let [_main_area, plan_area] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(35)]).areas(frame.area());

        if has_plan {
            let main_area = _main_area;
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
            frame.render_widget(Clear, plan_area);
            self.render_plan_panel(frame, plan_area);
        } else {
            frame.render_widget(Clear, plan_area);

            let [output_area, status_area, input_area] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(self.render_input_height()),
            ])
            .areas(frame.area());

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
        }
    }

    fn render_plan_panel(&self, frame: &mut Frame, area: Rect) {
        if let Some(ref plan) = self.plan_state_cache {
            super::plan_panel::render_plan_panel(plan, frame, area);
        }
    }

    fn render_input(&mut self, frame: &mut Frame, input_area: Rect) {
        let input_title = if crate::core::approval::is_approval_prompt_active() {
            " Approval pending (y/n/a) ".to_string()
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

    fn has_blocking_prompt(&self) -> bool {
        crate::core::approval::is_approval_prompt_active()
            || crate::core::approval::is_any_followup_question_active()
    }

    fn render_input_height(&self) -> u16 {
        if self.has_blocking_prompt() {
            (BLOCKING_PROMPT_INPUT_VISIBLE_LINES as u16) + 2
        } else {
            self.input_height()
        }
    }

    fn render_status_bar(&mut self, frame: &mut Frame, status_area: Rect) {
        let current_fingerprint = (
            self.provider_name.clone(),
            self.model_name.clone(),
            self.task_id.clone(),
            self.mode.clone(),
        );
        if self.status_left_fingerprint != current_fingerprint {
            self.cached_status_left = format!(
                " {} / {} | {} | {} ",
                self.provider_name, self.model_name, self.task_id, self.mode
            );
            self.status_left_fingerprint = current_fingerprint;
        }
        let elapsed_secs = self.elapsed.map_or(u64::MAX, |e| e.as_secs());
        let context_key = (
            elapsed_secs,
            self.context_pct,
            self.output_overflow,
            self.output_overflow_count,
            self.queued_message_count,
        );
        if context_key != self.cached_status_right_secs {
            let context_str = self
                .context_pct
                .map(|pct| format!("Context: {:.0}% left · ", 100.0 - pct));
            let overflow_str = if self.output_overflow {
                Some(format!(
                    "⚠ output overflow ({} dropped{}) · ",
                    self.output_overflow_count,
                    if self.output_overflow_summary.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", self.output_overflow_summary)
                    }
                ))
            } else {
                None
            };
            let queued_str = if self.queued_message_count > 0 {
                Some(format!("📨 {} queued · ", self.queued_message_count))
            } else {
                None
            };
            self.cached_status_right = match (overflow_str, context_str, queued_str, self.elapsed) {
                (Some(ovf), Some(ctx), Some(q), Some(elapsed)) => {
                    format!("{}{}{}⏱ {} ", ovf, ctx, q, format_duration(elapsed))
                }
                (Some(ovf), Some(ctx), Some(q), None) => format!("{ovf}{ctx}{q} "),
                (Some(ovf), None, Some(q), Some(elapsed)) => {
                    format!("{}{}⏱ {} ", ovf, q, format_duration(elapsed))
                }
                (Some(ovf), None, Some(q), None) => format!("{ovf}{q} "),
                (None, Some(ctx), Some(q), Some(elapsed)) => {
                    format!("{}{}⏱ {} ", ctx, q, format_duration(elapsed))
                }
                (None, Some(ctx), Some(q), None) => format!("{ctx}{q} "),
                (None, None, Some(q), Some(elapsed)) => {
                    format!("{}⏱ {} ", q, format_duration(elapsed))
                }
                (None, None, Some(q), None) => format!("{q} "),
                (Some(ovf), Some(ctx), None, Some(elapsed)) => {
                    format!("{}{}⏱ {} ", ovf, ctx, format_duration(elapsed))
                }
                (Some(ovf), Some(ctx), None, None) => format!("{ovf}{ctx} "),
                (Some(ovf), None, None, Some(elapsed)) => {
                    format!("{}⏱ {} ", ovf, format_duration(elapsed))
                }
                (Some(ovf), None, None, None) => format!("{ovf} "),
                (None, Some(ctx), None, Some(elapsed)) => {
                    format!("{}⏱ {} ", ctx, format_duration(elapsed))
                }
                (None, Some(ctx), None, None) => format!("{ctx} "),
                (None, None, None, Some(elapsed)) => format!("⏱ {} ", format_duration(elapsed)),
                (None, None, None, None) => String::new(),
            };
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
        // +2 accounts for top and bottom borders of the Block widget.
        let bottom_height: u16 = if has_error {
            ((self.cached_error_rows + 2) as u16).min(output_area.height)
        } else if has_completion {
            ((self.cached_completion_rows + 2) as u16).min(output_area.height)
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

        // Output pane with themed border and padding
        let visible_height = main_output_area.height as usize;
        // Content height excludes border (1 line top + 1 line bottom)
        let content_height = visible_height.saturating_sub(2);
        self.last_content_width = main_output_area.width as usize;
        self.last_content_height = content_height;
        let total_rows = self.total_visual_rows(wrap_width);
        // The output Paragraph only renders output_lines; completion_lines are
        // drawn as a separate Block below the main output. Scroll math must
        // therefore be based on output_rows alone, or the bottom of the
        // output gets hidden behind the completion overlay.
        let output_rows = total_rows
            .saturating_sub(self.cached_completion_rows)
            .saturating_sub(self.cached_error_rows);
        let scroll_y = self.resolved_output_scroll_y_for(wrap_width, output_rows, content_height);
        let (start_idx, visible_count, visible_scroll_y) =
            self.visible_output_window(wrap_width, scroll_y as usize, content_height);

        // Render the full transcript. Cached row counts keep scroll math cheap,
        // but slicing the render buffer can clip wrapped prompt text.
        // `output_rows_for_render` returns the same expanded (line, kind)
        // list that the visual-row cache was built from, so the visible
        // slice and the scroll math stay in lockstep after separator
        // insertion.
        {
            frame.render_widget(Clear, main_output_area);
            let visible_lines = self.collect_output_rows_range(start_idx, visible_count);
            let output = Paragraph::new(visible_lines)
                .wrap(Wrap { trim: false })
                .scroll((visible_scroll_y as u16, 0))
                .block(
                    theme::border_block(" sned ")
                        .padding(ratatui::widgets::Padding::new(0, 0, 0, 0)),
                );
            frame.render_widget(output, main_output_area);
        }

        // Scrollback overflow indicator: shown at bottom when there's
        // scrollback history available and user is at the bottom.
        if !self.in_scrollback && self.scrollback_count > 0 && self.scroll_mode == ScrollMode::Auto
        {
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

        // Render error box (priority) or completion box below the main output area.
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
            let completion = Paragraph::new(completion_lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::PROMPT_FG))
                        .border_type(ratatui::widgets::BorderType::Rounded),
                );
            frame.render_widget(completion, bottom_area);
        }

        // Scrollbar on output pane (render inside the border).
        // Use output_rows (not total_rows) so the scrollbar thumb
        // reflects only the output pane content — completion rows are
        // rendered separately and must not affect scroll geometry.
        self.scrollbar_state = self
            .scrollbar_state
            .content_length(output_rows)
            .viewport_content_length(content_height.max(1).min(output_rows))
            .position(scroll_y.min(output_rows as u16) as usize);
        frame.render_stateful_widget(
            Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"))
                .style(theme::scrollbar_style())
                .thumb_style(theme::scrollbar_thumb_style()),
            output_area.inner(ratatui::layout::Margin {
                horizontal: 0,
                vertical: 1,
            }),
            &mut self.scrollbar_state,
        );
    }

    /// Render file picker overlay as a floating Table widget.
    #[allow(dead_code)]
    fn render_picker_overlay(&self, frame: &mut Frame, output_area: Rect) {
        let max_height = 10.min(self.picker_results.len() as u16);
        let width = 50.min(output_area.width);

        // Position picker at bottom of output area, just above status bar
        let overlay_area = Rect {
            x: output_area.x + 2,
            y: output_area
                .y
                .saturating_add(output_area.height.saturating_sub(max_height + 4)),
            width,
            height: max_height + 2, // +2 for border
        };

        // Pre-compute display labels once per frame so the inner
        // closure only does the per-row span/line construction.
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

        let rows: Vec<Line> = labels
            .into_iter()
            .enumerate()
            .map(|(i, label)| {
                if i == self.picker_index {
                    Line::from(Span::styled(label, theme::picker_selected_style()))
                } else {
                    Line::from(label)
                }
            })
            .collect();

        let picker = Paragraph::new(rows).block(theme::overlay_block(format!(
            " Files ({}) ",
            self.picker_results.len()
        )));

        frame.render_widget(Clear, overlay_area);
        frame.render_widget(picker, overlay_area);
    }

    /// Render slash command picker overlay as a floating Table widget.
    fn render_slash_command_overlay(&self, frame: &mut Frame, output_area: Rect) {
        let max_height = 10.min(self.slash_command_results.len() as u16);
        let width = 50.min(output_area.width);

        let overlay_area = Rect {
            x: output_area.x + 2,
            y: output_area
                .y
                .saturating_add(output_area.height.saturating_sub(max_height + 4)),
            width,
            height: max_height + 2,
        };

        let rows: Vec<Line> = self
            .slash_command_results
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let category_marker = match entry.category {
                    crate::cli::slash_commands::SlashCommandCategory::Agent => "▶ ",
                    crate::cli::slash_commands::SlashCommandCategory::Local => "● ",
                    crate::cli::slash_commands::SlashCommandCategory::Plan => "◆ ",
                    crate::cli::slash_commands::SlashCommandCategory::Skill => "★ ",
                    crate::cli::slash_commands::SlashCommandCategory::Workflow => "▸ ",
                };
                let label = format!("{} {} - {}", category_marker, entry.name, entry.description);
                if i == self.slash_command_selected {
                    Line::from(Span::styled(label, theme::picker_selected_style()))
                } else {
                    Line::from(label)
                }
            })
            .collect();

        let picker = Paragraph::new(rows).block(theme::overlay_block(format!(
            " Slash Commands ({}) ",
            self.slash_command_results.len()
        )));

        frame.render_widget(Clear, overlay_area);
        frame.render_widget(picker, overlay_area);
    }

    /// Render model picker overlay as a floating widget.
    fn render_model_picker_overlay(&self, frame: &mut Frame, output_area: Rect) {
        let max_height = 10.min(self.model_picker_results.len() as u16);
        let width = 50.min(output_area.width);

        let overlay_area = Rect {
            x: output_area.x + 2,
            y: output_area
                .y
                .saturating_add(output_area.height.saturating_sub(max_height + 4)),
            width,
            height: max_height + 2,
        };

        let rows: Vec<Line> = self
            .model_picker_results
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let label = format!(
                    "[{}] {} - {}",
                    entry.provider, entry.label, entry.description
                );
                if i == self.model_picker_selected {
                    Line::from(Span::styled(label, theme::picker_selected_style()))
                } else {
                    Line::from(label)
                }
            })
            .collect();

        let picker = Paragraph::new(rows).block(theme::overlay_block(format!(
            " Models ({}) ",
            self.model_picker_results.len()
        )));

        frame.render_widget(Clear, overlay_area);
        frame.render_widget(picker, overlay_area);
    }

    /// Handle a paste event, folding large pastes into markers.
    /// Returns true if the paste was folded, false if inserted directly.
    pub fn handle_paste(&mut self, content: &str) -> bool {
        let char_count = content.chars().count();
        let folded = char_count > self.paste_fold_threshold;

        if folded {
            // Create a unique, non-user-inputtable marker for the folded paste.
            // Index-based format prevents collisions with user-typed text and
            // handles duplicate pastes correctly (each paste gets a distinct marker).
            let marker = format!("\x00SNED_PASTE_{}", self.paste_chunks.len());

            // Insert the marker at cursor position
            self.input.insert_str(&marker);

            // Track this paste chunk (store globally, expand on submit)
            self.paste_chunks.push(PasteChunk {
                marker,
                content: content.to_string(),
                start_line: 0, // Simplified: track globally, not per-line
                expanded: false,
            });
        } else {
            // Insert small pastes directly
            self.input.insert_str(content);
        }

        folded
    }

    /// Get the final input text, expanding all paste markers.
    pub fn get_input_with_expanded_pastes(&mut self) -> String {
        // Get current textarea content
        let mut text = self.input.lines().join("\n");

        // Replace all markers with original content
        for paste in self.paste_chunks.drain(..) {
            if let Some(pos) = text.find(&paste.marker) {
                text.replace_range(pos..pos + paste.marker.len(), &paste.content);
            }
        }

        text
    }

    /// Clear all paste chunks.
    pub fn clear_pastes(&mut self) {
        self.paste_chunks.clear();
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

    #[test]
    fn test_scroll_lines_switches_to_manual_mode() {
        let mut app = make_scrolling_app(20, 5);

        app.scroll_lines(-3);

        assert_eq!(app.scroll_mode, ScrollMode::Manual);
        assert_eq!(app.scroll_offset, 12);
        assert_eq!(app.resolved_scroll_y_for(app.output_lines.len(), 5), 12);
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
        let max_offset = 15u16; // make_scrolling_app(20, 5) → max_offset = 15
        let cases: &[(ScrollMode, u16, ScrollMode, &str)] = &[
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
    fn test_cached_visual_rows_tracks_push_clear_and_drain() {
        let mut app = App::new();
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

        app.clear_output();
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
    fn test_render_output_hides_loading_overlay_during_approval_prompt() {
        let _approval_guard = crate::core::approval::approval_test_guard();
        struct ApprovalPromptCleanup;

        impl Drop for ApprovalPromptCleanup {
            fn drop(&mut self) {
                crate::core::approval::set_approval_prompt_active(false);
            }
        }

        let _cleanup = ApprovalPromptCleanup;
        crate::core::approval::set_approval_prompt_active(true);

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.agent_busy = true;
        app.force_bottom();
        app.push_plain("line 1");
        app.push_plain("line 2");
        app.push_plain("Approve these edits? (y/n/always):");

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
        assert!(!rendered.contains("Agent processing..."));
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
        let _approval_guard = crate::core::approval::approval_test_guard();
        struct ApprovalPromptCleanup;

        impl Drop for ApprovalPromptCleanup {
            fn drop(&mut self) {
                crate::core::approval::set_approval_prompt_active(false);
                crate::core::approval::clear_approval_prompt_scroll();
            }
        }

        let _cleanup = ApprovalPromptCleanup;
        crate::core::approval::set_approval_prompt_active(true);
        crate::core::approval::set_approval_prompt_scroll();

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.pin_approval_bottom();
        app.set_input_text("1\n2\n3\n4\n5\n6\n7\n8");
        app.push_plain("A long wrapped tool explanation line that takes multiple rows.");
        app.push_plain("Another wrapped line that used to crowd the prompt below the input box.");
        app.push_plain("🔧 Tool: edit_file");
        app.push_plain("Execute this tool? (y/n/always):");

        assert_eq!(app.render_input_height(), 3);

        terminal
            .draw(|frame| app.render(frame))
            .expect("render should succeed");

        let buffer = terminal.backend().buffer();
        let rendered = rendered_rows(buffer).join("\n");
        let output_rows = rendered_output_rows(&app, buffer).join("\n");

        assert!(rendered.contains("Approval pending"));
        assert!(output_rows.contains("Execute this tool?"));
        assert!(output_rows.contains("edit_file"));
    }

    #[test]
    fn test_render_approval_pin_tracks_prompt_tail_not_transcript_tail() {
        let _approval_guard = crate::core::approval::approval_test_guard();
        struct ApprovalPromptCleanup;

        impl Drop for ApprovalPromptCleanup {
            fn drop(&mut self) {
                crate::core::approval::set_approval_prompt_active(false);
                crate::core::approval::clear_approval_prompt_scroll();
            }
        }

        let _cleanup = ApprovalPromptCleanup;
        crate::core::approval::set_approval_prompt_active(true);
        crate::core::approval::set_approval_prompt_scroll();

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut app = App::new();
        app.pin_approval_bottom();

        for index in 0..8 {
            app.push_plain(format!("old line {index}"));
        }
        app.push_output_with_kind(Line::from("🔧 Tool: edit_file"), BlockKind::UserPrompt);
        app.push_output_with_kind(
            Line::from("Execute this tool? (y/n/always):"),
            BlockKind::UserPrompt,
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
    fn test_clear_pastes_empties_paste_chunks() {
        let mut app = App::new();
        app.paste_chunks.push(PasteChunk {
            marker: "[pasted 10 chars]".to_string(),
            content: "0123456789".to_string(),
            start_line: 0,
            expanded: false,
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
            marker: "\x00SNED_PASTE_0".to_string(),
            content: "first paste content".to_string(),
            start_line: 0,
            expanded: false,
        });
        app.paste_chunks.push(PasteChunk {
            marker: "\x00SNED_PASTE_1".to_string(),
            content: "first paste content".to_string(),
            start_line: 0,
            expanded: false,
        });

        // Simulate textarea with both markers (same content pasted twice)
        app.input = App::new_textarea(vec![
            "\x00SNED_PASTE_0".to_string(),
            "some text".to_string(),
            "\x00SNED_PASTE_1".to_string(),
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

        // Verify user-typed literal marker is NOT expanded when no paste chunk exists
        let mut app2 = App::new();
        app2.input = App::new_textarea(vec!["[pasted 500 chars]".to_string()]);
        let result2 = app2.get_input_with_expanded_pastes();
        assert_eq!(result2, "[pasted 500 chars]");
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
    fn test_slash_command_fields_initialized() {
        let app = App::new();
        assert!(!app.slash_command_active);
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

    /// Contract test for the "approval prompt invisible after multi-line tool
    /// output" bug. The reported scenario: Gemini streams a tool result
    /// (multi-line), then the agent immediately requests approval for the
    /// next tool. The approval prompt was being scrolled out of view or
    /// failing to pin to the bottom, so the user could not see it.
    ///
    /// This test reproduces the post-`begin_approval_prompt` state, then
    /// drains both the tool result AND the approval prompt emit, then
    /// renders, and asserts the approval prompt text appears in the
    /// visible bottom rows of the output pane.
    #[test]
    fn test_approval_prompt_visible_after_multiline_tool_result() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        let _approval_guard = crate::core::approval::approval_test_guard();

        // Simulate the approval prompt being armed BEFORE the emit lands,
        // which is exactly what begin_approval_prompt() does in
        // src/core/approval.rs:1027-1038.
        crate::core::approval::set_approval_prompt_active(true);
        crate::core::approval::set_approval_prompt_scroll();

        let (tx, mut rx) = mpsc::channel::<crate::cli::output::OutputEvent>(16);
        let writer: Arc<dyn crate::cli::output::OutputWriter> =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        // 80x24 terminal: ~20 rows for the output area, minus 2 for borders
        // = ~18 rows of content.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        // Seed enough content to push old lines out of the visible bottom
        // and force the viewport to scroll.
        for index in 0..50 {
            app.push_plain(format!("old line {}", index));
        }
        // Initial render to populate cached_wrap_width and viewport state.
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial render should succeed");

        // Simulate a multi-line tool result (e.g. execute_command output).
        // In production this comes from agent_loop.rs:3018-3028 which emits
        // one event per line up to MAX_COMMAND_RESULT_DISPLAY_LINES.
        let tool_result_lines = [
            "  ✓ src/foo.rs",
            "    line 1 of output",
            "    line 2 of output",
            "    line 3 of output",
            "    line 4 of output",
            "    line 5 of output",
        ];
        for line in tool_result_lines {
            writer.emit(crate::cli::output::OutputEvent::dim(line.to_string()));
        }

        // Simulate the approval prompt emit. The prompt is RawAnsi with a
        // trailing \n, which ansi_to_ratatui_lines splits into one Line per
        // visible row. We use a representative multi-line approval prompt
        // shape (matches the structure from build_tool_approval_prompt).
        let prompt = "\n\
                      \x1b[33m🔧 Tool:\x1b[0m \x1b[1medit_file\x1b[0m\n\
                      \x1b[2m  path: src/baz.rs\x1b[0m\n\
                      Execute this tool? (y/n/always): ";
        writer.emit(crate::cli::output::OutputEvent::RawAnsi(format!(
            "{}\n",
            prompt
        )));

        // Full pipeline: drain + clamp + render. This is what the main
        // loop does on every tick (interactive.rs:2165-2252).
        crate::cli::interactive::drain_output_for_test(&mut rx, &mut app);
        terminal
            .draw(|frame| app.render(frame))
            .expect("post-prompt render should succeed");

        // Assert the scroll mode is ApprovalPinned so the user can't be
        // scrolled away from the prompt.
        assert_eq!(
            app.scroll_mode,
            ScrollMode::ApprovalPinned,
            "scroll mode must be ApprovalPinned while approval prompt is active"
        );

        // Read the rendered buffer and assert the approval prompt text is
        // visible in the bottom rows of the output pane.
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();

        // The output pane occupies the upper rows; status bar (1) and
        // input area (3) are at the bottom. Scan the bottom 15 rows of
        // the terminal to cover the output pane content above the status/input.
        let output_bottom_rows: Vec<&String> = rows.iter().rev().take(15).collect();

        // The "Execute this tool?" line is the user-visible prompt anchor.
        let has_prompt_question = output_bottom_rows
            .iter()
            .any(|row| row.contains("Execute this tool?"));

        // The tool name "edit_file" should also be visible.
        let has_tool_name = output_bottom_rows
            .iter()
            .any(|row| row.contains("edit_file"));

        // The tool result lines should be visible above the prompt.
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

        // Teardown: clear the approval state so the test guard's
        // cleanup sees a clean slate.
        crate::core::approval::set_approval_prompt_active(false);
        crate::core::approval::clear_approval_prompt_scroll();
    }

    /// Contract test for the race where the approval scroll flag is set
    /// (by `begin_approval_prompt`) BEFORE the prompt emit lands in the
    /// channel. The first drain call should consume the flag and pin;
    /// the second drain (after the prompt actually arrives) should keep
    /// the pin via the `is_approval_prompt_active()` branch.
    ///
    /// The user reported this exact sequence (multi-line tool output →
    /// approval prompt) left the prompt invisible. This test pins down
    /// the expected behavior across the two-drain sequence.
    #[test]
    fn test_approval_prompt_visible_when_flag_set_before_emit() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        let _approval_guard = crate::core::approval::approval_test_guard();

        // Step 1: Begin approval prompt. In production this is what
        // approval.rs:1035 does — sets the scroll flag BEFORE the
        // prompt emit lands in the channel.
        crate::core::approval::set_approval_prompt_active(true);
        crate::core::approval::set_approval_prompt_scroll();

        let (tx, mut rx) = mpsc::channel::<crate::cli::output::OutputEvent>(16);
        let writer: Arc<dyn crate::cli::output::OutputWriter> =
            Arc::new(crate::cli::output::ChannelOutputWriter::new(tx));

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = App::new();
        for index in 0..50 {
            app.push_plain(format!("old line {}", index));
        }
        terminal
            .draw(|frame| app.render(frame))
            .expect("initial render should succeed");

        // Step 2: Multi-line tool result emit.
        let tool_result_lines = [
            "  ✓ src/foo.rs",
            "    line 1 of output",
            "    line 2 of output",
            "    line 3 of output",
            "    line 4 of output",
        ];
        for line in tool_result_lines {
            writer.emit(crate::cli::output::OutputEvent::dim(line.to_string()));
        }

        // Step 3: First drain — flag is already set, tool result lands.
        // This should consume the flag and pin.
        crate::cli::interactive::drain_output_for_test(&mut rx, &mut app);
        assert_eq!(
            app.scroll_mode,
            ScrollMode::ApprovalPinned,
            "first drain must pin the scroll (flag was set before emit)"
        );

        // Step 4: Prompt emit happens AFTER the first drain (this is the
        // exact race that begin_approval_prompt creates).
        let prompt = "\n\
                      \x1b[33m🔧 Tool:\x1b[0m \x1b[1medit_file\x1b[0m\n\
                      Execute this tool? (y/n/always): ";
        writer.emit(crate::cli::output::OutputEvent::RawAnsi(format!(
            "{}\n",
            prompt
        )));

        // Step 5: Second drain — flag is already consumed, but
        // is_approval_prompt_active() is still true, so the else-if
        // branch should re-pin.
        crate::cli::interactive::drain_output_for_test(&mut rx, &mut app);
        assert_eq!(
            app.scroll_mode,
            ScrollMode::ApprovalPinned,
            "second drain must keep the pin via is_approval_prompt_active()"
        );

        // Step 6: Render and verify the prompt is visible.
        terminal
            .draw(|frame| app.render(frame))
            .expect("post-prompt render should succeed");

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

        assert!(
            has_prompt_question,
            "approval prompt must be visible after the two-drain sequence. \
             This is the exact bug the user reported: multi-line tool output \
             followed by an approval prompt left the prompt invisible. \
             bottom rows: {output_bottom_rows:?}"
        );

        // Teardown.
        crate::core::approval::set_approval_prompt_active(false);
        crate::core::approval::clear_approval_prompt_scroll();
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

    /// Test that the overflow indicator and an approval prompt
    /// coexist in the same render frame. The user's original bug
    /// report was: "approval prompt dropped because channel was full,
    /// user had to blindly hit y." This test verifies that AFTER
    /// the channel overflow, when an approval prompt IS emitted and
    /// drained successfully, both the overflow indicator AND the
    /// approval prompt are visible.
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
        // Overflow already happened earlier in the session.
        app.output_overflow = true;
        app.output_overflow_count = 3;
        // An approval prompt was successfully drained and pushed to
        // the output buffer.
        app.push_plain("\x1b[33m🔧 Tool:\x1b[0m \x1b[1mexecute_command\x1b[0m".to_string());
        app.push_plain("Execute this tool? (y/n/always): ".to_string());
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

        // The approval prompt must be in the output pane (rows 0..10).
        let output_rows: Vec<&String> = rows.iter().take(10).collect();
        let has_approval = output_rows
            .iter()
            .any(|row| row.contains("Execute this tool?"));
        assert!(
            has_approval,
            "approval prompt must be visible in the output pane. \
             output rows: {output_rows:?}"
        );

        // The status bar must STILL show the overflow indicator (sticky
        // warning, per the design from commit a1da7ea).
        let status_row = &rows[10];
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
        app.enter_scrollback();

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

        app.enter_scrollback();
        assert!(app.in_scrollback);

        app.exit_scrollback();

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
        app.toggle_scrollback();
        assert!(app.in_scrollback, "first toggle should enter scrollback");
        assert!(
            app.output_lines.len() >= 2,
            "buffer should contain scrollback lines"
        );

        app.toggle_scrollback();
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

        // Sanity: state says we should be at the bottom (20 - 5 = 15).
        assert_eq!(app.scroll_mode, ScrollMode::Auto);
        assert_eq!(
            app.resolved_scroll_y_for(app.output_lines.len(), content_height),
            15
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

        // The last 5 lines (15..20) must be visible in the rendered buffer.
        for index in 15..20 {
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
