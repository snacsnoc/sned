//! App struct for ratatui TUI.
//!
//! This is the main application state for the ratatui render loop.

use super::history::FileHistory;
use super::theme;
use crate::core::file_search::FileSearchResult;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

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
    /// Input textarea (live user input)
    pub input: TextArea<'static>,
    /// Whether the agent is currently busy
    pub agent_busy: bool,
    /// Manual scroll offset (top-of-viewport line index)
    pub scroll_offset: u16,
    /// Current output scroll behavior
    pub scroll_mode: ScrollMode,
    /// Whether the next draw should re-sync layout from the terminal size.
    pub has_resized: bool,
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
    /// Cached status bar left segment (provider / model | task | mode).
    /// Rebuilt only when the underlying fields change.
    pub cached_status_left: String,
    /// Fingerprint of the fields used to build cached_status_left.
    pub status_left_fingerprint: (String, String, String, String),
    /// Cached status bar right segment (elapsed timer). Rebuilt when seconds change.
    pub cached_status_right: String,
    /// Seconds value the cached right segment was built for.
    pub cached_status_right_secs: u64,
}

impl App {
    /// Create a new TextArea with default styling (no underline on cursor line).
    pub fn new_textarea(lines: Vec<String>) -> TextArea<'static> {
        let mut input = TextArea::new(lines);
        input.set_placeholder_text("❯ ");
        input.set_cursor_line_style(Style::default());
        input
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
    pub fn new() -> Self {
        Self {
            output_lines: VecDeque::new(),
            input: Self::new_textarea(Vec::new()),
            agent_busy: false,
            scroll_offset: 0,
            scroll_mode: ScrollMode::Auto,
            has_resized: true,
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
            cached_status_right_secs: u64::MAX,
        }
    }

    /// Push an output line to the buffer.
    pub fn push_output(&mut self, line: Line<'static>) {
        let wrap_width = self.last_wrap_width();
        let line_rows = Self::line_visual_rows(&line, wrap_width);
        let cache_valid = self.cached_wrap_width == Some(wrap_width);
        self.output_lines.push_back(line);
        if self.output_lines.len() > 10_000 {
            let mut removed_rows = 0usize;
            while self.output_lines.len() > 10_000 {
                if cache_valid && let Some(removed) = self.output_lines.pop_front() {
                    removed_rows += Self::line_visual_rows(&removed, wrap_width);
                } else {
                    self.output_lines.pop_front();
                }
            }
            if cache_valid {
                self.cached_visual_rows = self
                    .cached_visual_rows
                    .saturating_add(line_rows)
                    .saturating_sub(removed_rows);
                return;
            }
        }

        if cache_valid {
            self.cached_visual_rows = self.cached_visual_rows.saturating_add(line_rows);
        } else {
            self.rebuild_visual_row_cache(wrap_width);
        }
    }

    /// Push a plain text line.
    pub fn push_plain(&mut self, text: impl Into<String>) {
        self.push_output(Line::from(text.into()));
    }

    /// Push a styled text line.
    pub fn push_styled(&mut self, text: impl Into<String>, style: Style) {
        self.push_output(Line::from(Span::styled(text.into(), style)));
    }

    /// Push a turn separator line.
    pub fn push_turn_separator(&mut self) {
        self.push_output(Line::from(Span::styled("─".repeat(40), theme::dim_style())));
    }

    /// Push a user message with proper formatting (splits on newlines).
    pub fn push_user_message(&mut self, text: &str, writer: &OutputWriterArc) {
        let style = Style::default()
            .fg(theme::PROMPT_FG)
            .add_modifier(Modifier::BOLD);
        for (i, line) in text.split('\n').enumerate() {
            let prefix = if i == 0 { "❯ " } else { "  " };
            let content = format!("{}{}", prefix, line);
            writer.emit(OutputEvent::styled(content, style));
        }
    }

    pub fn force_bottom(&mut self) {
        self.scroll_mode = ScrollMode::Auto;
        self.scroll_offset = 0;
    }

    /// Clear all output and reset the visual-row cache.
    pub fn clear_output(&mut self) {
        self.output_lines.clear();
        self.cached_visual_rows = 0;
        self.cached_wrap_width = Some(self.last_wrap_width());
    }

    /// Drain output from the given index onward and keep the visual-row cache in sync.
    pub fn drain_output_from(&mut self, start: usize) {
        let start = start.min(self.output_lines.len());
        if start >= self.output_lines.len() {
            return;
        }

        let wrap_width = self.last_wrap_width();
        if self.cached_wrap_width == Some(wrap_width) {
            let removed_rows = self
                .output_lines
                .drain(start..)
                .map(|line| Self::line_visual_rows(&line, wrap_width))
                .sum::<usize>();
            self.cached_visual_rows = self.cached_visual_rows.saturating_sub(removed_rows);
        } else {
            self.output_lines.drain(start..);
            self.cached_visual_rows = 0;
            self.cached_wrap_width = None;
        }
    }

    pub fn pin_approval_bottom(&mut self) {
        self.scroll_mode = ScrollMode::ApprovalPinned;
        self.scroll_offset = 0;
    }

    pub fn clear_approval_pin(&mut self) {
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
                let plan_ptr = plan as *const _ as usize;
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
        let total_rows = self.total_visual_rows(self.last_wrap_width());
        if !self.enter_manual_mode(total_rows) {
            return;
        }

        let max_offset =
            Self::max_scroll_offset_for(total_rows, self.last_content_height) as isize;
        let next = (self.scroll_offset as isize + delta).clamp(0, max_offset);
        self.scroll_offset = next as u16;
        self.clamp_to_content();
    }

    pub fn scroll_pages(&mut self, delta_pages: isize) {
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

    fn last_wrap_width(&self) -> usize {
        if self.last_content_width == 0 {
            80
        } else {
            Self::content_wrap_width(self.last_content_width)
        }
    }

    fn content_wrap_width(content_width: usize) -> usize {
        content_width.saturating_sub(3).max(1)
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

    fn rebuild_visual_row_cache(&mut self, wrap_width: usize) {
        self.cached_visual_rows = self
            .output_lines
            .iter()
            .map(|line| Self::line_visual_rows(line, wrap_width))
            .sum();
        self.cached_wrap_width = Some(wrap_width);
    }

    fn total_visual_rows(&mut self, wrap_width: usize) -> usize {
        if self.cached_wrap_width != Some(wrap_width) {
            self.rebuild_visual_row_cache(wrap_width);
        }
        self.cached_visual_rows
    }

    /// Render the application state to the frame.
    pub fn render(&mut self, frame: &mut Frame) {
        let has_plan = self.plan_state_cache.is_some();

        if has_plan {
            // Layout with plan panel on the right
            let [main_area, plan_area] =
                Layout::horizontal([Constraint::Min(40), Constraint::Length(35)])
                    .areas(frame.area());

            let [output_area, status_area, input_area] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .areas(main_area);

            self.render_output(frame, output_area);
            self.render_status_bar(frame, status_area);
            self.render_input(frame, input_area);
            if self.picker_active {
                self.render_picker_overlay(frame, output_area);
            }
            self.render_plan_panel(frame, plan_area);
        } else {
            let [output_area, status_area, input_area] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .areas(frame.area());

            self.render_output(frame, output_area);
            self.render_status_bar(frame, status_area);
            self.render_input(frame, input_area);
            if self.picker_active {
                self.render_picker_overlay(frame, output_area);
            }
        }
    }

    fn render_plan_panel(&self, frame: &mut Frame, area: Rect) {
        if let Some(ref plan) = self.plan_state_cache {
            super::plan_panel::render_plan_panel(plan, frame, area);
        }
    }

    fn render_input(&mut self, frame: &mut Frame, input_area: Rect) {
        // Update input block with themed border and styled title
        let input_title = Line::from(" Input ");
        self.input
            .set_block(theme::input_block(input_title, self.agent_busy));

        self.update_placeholder();

        frame.render_widget(&self.input, input_area);
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
        let elapsed_secs = self.elapsed.map(|e| e.as_secs()).unwrap_or(u64::MAX);
        if elapsed_secs != self.cached_status_right_secs {
            self.cached_status_right = if let Some(elapsed) = self.elapsed {
                format!("⏱ {} ", format_duration(elapsed))
            } else {
                String::new()
            };
            self.cached_status_right_secs = elapsed_secs;
        }
        let left_width = UnicodeWidthStr::width(self.cached_status_left.as_str());
        let right_width = UnicodeWidthStr::width(self.cached_status_right.as_str());
        let spacer_len = status_area
            .width
            .saturating_sub((left_width + right_width) as u16)
            as usize;
        let status_line = Line::from(vec![
            Span::styled(self.cached_status_left.clone(), theme::status_style()),
            Span::raw(" ".repeat(spacer_len)),
            Span::styled(self.cached_status_right.clone(), theme::status_style()),
        ]);
        let status = Paragraph::new(status_line);
        frame.render_widget(status, status_area);
    }

    fn render_output(&mut self, frame: &mut Frame, output_area: Rect) {
        // Output pane with themed border and padding
        let visible_height = output_area.height as usize;
        // Content height excludes border (1 line top + 1 line bottom)
        let content_height = visible_height.saturating_sub(2);
        self.last_content_width = output_area.width as usize;
        self.last_content_height = content_height;
        let wrap_width = Self::content_wrap_width(output_area.width as usize);
        let total_rows = self.total_visual_rows(wrap_width);
        let scroll_y = self.resolved_scroll_y_for(total_rows, content_height);

        // Render the full transcript. Cached row counts keep scroll math cheap,
        // but slicing the render buffer can clip wrapped prompt text.
        {
            let output = Paragraph::new(self.output_lines.iter().cloned().collect::<Vec<_>>())
                .wrap(Wrap { trim: false })
                .scroll((scroll_y, 0))
                .block(
                    theme::border_block(" sned ")
                        .padding(ratatui::widgets::Padding::new(1, 0, 0, 0)),
                );
            frame.render_widget(output, output_area);

            // Keep a single busy indicator in the output pane. Duplicating the
            // spinner in the input pane just burns redraw budget and visual space.
            if self.agent_busy && !crate::core::approval::is_approval_prompt_active() {
                let loading_area = Rect::new(
                    output_area.x,
                    output_area.y + output_area.height.saturating_sub(1),
                    output_area.width,
                    1,
                );
                let loading = Paragraph::new(Line::from(Span::styled(
                    format!("{} Agent processing...", self.spinner_char()),
                    theme::spinner_style(),
                )))
                .style(theme::status_style());
                frame.render_widget(loading, loading_area);
            }

        }

        // Scrollbar on output pane (render inside the border)
        self.scrollbar_state = self
            .scrollbar_state
            .content_length(total_rows)
            .viewport_content_length(content_height.max(1))
            .position(scroll_y as usize);
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

        let rows: Vec<Line> = self
            .picker_results
            .iter()
            .enumerate()
            .map(|(i, result)| {
                let icon = match result.file_type {
                    crate::core::file_search::FileType::Folder => "📁",
                    crate::core::file_search::FileType::File => "📄",
                };
                let label = format!("{} {}", icon, result.label);
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

    /// Handle a paste event, folding large pastes into markers.
    /// Returns true if the paste was folded, false if inserted directly.
    pub fn handle_paste(&mut self, content: &str) -> bool {
        let char_count = content.chars().count();
        let folded = char_count > self.paste_fold_threshold;

        if folded {
            // Create a marker for the folded paste
            let marker = format!("[pasted {} chars]", char_count);

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
    pub fn tick_spinner(&mut self) {
        const SPINNER_INTERVAL: Duration = Duration::from_millis(125);

        if !self.agent_busy {
            self.last_spinner_tick = None;
            return;
        }

        let now = Instant::now();
        let should_advance = self
            .last_spinner_tick
            .map(|last| now.duration_since(last) >= SPINNER_INTERVAL)
            .unwrap_or(true);

        if should_advance {
            self.spinner_index = (self.spinner_index + 1) % 10;
            self.last_spinner_tick = Some(now);
        }
    }

    /// Get current spinner character.
    pub fn spinner_char(&self) -> char {
        spinner_frame(self.spinner_index)
    }
}

/// Format a duration as a human-readable string (e.g., "2m 30s", "45s", "1h 15m").
pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    if total_secs >= 3600 {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        format!("{}h {}m", hours, mins)
    } else if total_secs >= 60 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", total_secs)
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
            (ScrollMode::Manual, max_offset, ScrollMode::Auto, "at bottom"),
            // Manual 1 from bottom → stays Manual (regression guard for <= 2 → == 0 fix)
            (ScrollMode::Manual, max_offset - 1, ScrollMode::Manual, "1 from bottom"),
            // Manual at arbitrary mid-position → stays Manual
            (ScrollMode::Manual, 5, ScrollMode::Manual, "mid-position"),
            // Manual at top → stays Manual
            (ScrollMode::Manual, 0, ScrollMode::Manual, "at top"),
            // Auto always stays Auto
            (ScrollMode::Auto, 0, ScrollMode::Auto, "auto"),
            // ApprovalPinned always stays ApprovalPinned
            (ScrollMode::ApprovalPinned, 0, ScrollMode::ApprovalPinned, "approval pinned"),
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

        app.push_plain("short line");
        let wrap_width = app.last_wrap_width();
        let first_total = app.total_visual_rows(wrap_width);
        assert_eq!(app.cached_visual_rows, first_total);

        app.push_plain("this line is intentionally long enough to wrap twice");
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
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

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
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

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
    fn test_render_status_bar_caches_right_segment() {
        use std::time::Duration;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

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
}
