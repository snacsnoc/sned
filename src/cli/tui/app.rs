//! App struct for ratatui TUI.
//!
//! This is the main application state for the ratatui render loop.

use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};
use tui_textarea::TextArea;
use std::time::Instant;

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Application state for the ratatui TUI.
pub struct App {
    /// Output lines buffer (agent output, submitted prompts, etc.)
    pub output_lines: Vec<Line<'static>>,
    /// Input textarea (live user input)
    pub input: TextArea<'static>,
    /// Whether the agent is currently busy
    pub agent_busy: bool,
    /// Manual scroll offset (when auto_scroll is false)
    pub scroll_offset: u16,
    /// Whether to auto-scroll to bottom on new output
    pub auto_scroll: bool,
    /// Session start time (for elapsed time display)
    pub start_time: Option<Instant>,
    /// Spinner animation frame index
    pub spinner_index: usize,
    /// Current working directory (for file search)
    pub cwd: String,
    /// Whether @ mention file picker is active
    pub picker_active: bool,
    /// Current file search results
    pub picker_results: Vec<String>,
    /// Selected index in picker results
    pub picker_index: usize,
    /// Command history (most recent last)
    pub command_history: Vec<String>,
    /// Current history navigation index (-1 = no selection, 0 = most recent)
    pub history_index: isize,
}

impl App {
    /// Create a new App instance.
    pub fn new() -> Self {
        let input = TextArea::new(Vec::new());
        Self {
            output_lines: Vec::new(),
            input,
            agent_busy: false,
            scroll_offset: 0,
            auto_scroll: true,
            start_time: None,
            spinner_index: 0,
            cwd: String::new(),
            picker_active: false,
            picker_results: Vec::new(),
            picker_index: 0,
            command_history: Vec::new(),
            history_index: -1,
        }
    }

    /// Push an output line to the buffer.
    pub fn push_output(&mut self, line: Line<'static>) {
        self.output_lines.push(line);
        // Cap at 10K lines to avoid O(n) render cost
        if self.output_lines.len() > 10_000 {
            self.output_lines.drain(..self.output_lines.len() - 10_000);
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

    /// Render the application state to the frame.
    pub fn render(&mut self, frame: &mut Frame) {
        let [output_area, input_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(3),
        ]).areas(frame.area());

        // Update input block title with spinner when busy
        let title = if self.agent_busy {
            format!("{} Working...", self.spinner_char())
        } else {
            "❯".to_string()
        };
        self.input.set_block(Block::bordered().title(title));

        // Output pane
        let visible_height = output_area.height as usize;
        let total_lines = self.output_lines.len();
        let scroll_y = if self.auto_scroll {
            total_lines.saturating_sub(visible_height) as u16
        } else {
            self.scroll_offset
        };

        let output = Paragraph::new(self.output_lines.clone())
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0));
        frame.render_widget(output, output_area);

        // Input pane
        frame.render_widget(&self.input, input_area);
    }

    /// Increment spinner frame (call on each render tick when agent_busy).
    pub fn tick_spinner(&mut self) {
        if self.agent_busy {
            self.spinner_index = (self.spinner_index + 1) % SPINNER_FRAMES.len();
        }
    }

    /// Get current spinner character.
    pub fn spinner_char(&self) -> char {
        SPINNER_FRAMES[self.spinner_index]
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
