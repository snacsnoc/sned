//! App struct for ratatui TUI.
//!
//! This is the main application state for the ratatui render loop.

use super::history::FileHistory;
use crate::core::file_search::FileSearchResult;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use std::time::Instant;
use tui_textarea::TextArea;

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
    pub picker_results: Vec<FileSearchResult>,
    /// Selected index in picker results
    pub picker_index: usize,
    /// File-backed command history with navigation state
    pub history: FileHistory,
    /// Folded paste chunks (marker -> original content)
    pub paste_chunks: Vec<PasteChunk>,
    /// Threshold for folding pastes (in characters)
    pub paste_fold_threshold: usize,
}

impl App {
    /// Create a new TextArea with default styling (no underline on cursor line).
    pub fn new_textarea(lines: Vec<String>) -> TextArea<'static> {
        let mut input = TextArea::new(lines);
        input.set_placeholder_text("❯ ");
        input.set_cursor_line_style(Style::default());
        input
    }

    /// Create a new App instance.
    pub fn new() -> Self {
        Self {
            output_lines: Vec::new(),
            input: Self::new_textarea(Vec::new()),
            agent_busy: false,
            scroll_offset: 0,
            auto_scroll: true,
            start_time: None,
            spinner_index: 0,
            cwd: String::new(),
            picker_active: false,
            picker_results: Vec::new(),
            picker_index: 0,
            history: FileHistory::load(),
            paste_chunks: Vec::new(),
            paste_fold_threshold: 500, // Fold pastes > 500 chars
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
        let [output_area, input_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(frame.area());

        // Update input block title with spinner when busy
        let title = if self.agent_busy {
            format!(" {} Working ", self.spinner_char())
        } else {
            " Input ".to_string()
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

        // File picker overlay (when active)
        if self.picker_active && !self.picker_results.is_empty() {
            self.render_picker_overlay(frame, output_area);
        }
    }

    /// Render file picker overlay as a floating Table widget.
    fn render_picker_overlay(&self, frame: &mut Frame, output_area: Rect) {
        let max_height = 10.min(self.picker_results.len() as u16);
        let width = 50.min(output_area.width);

        let overlay_area = Rect {
            x: output_area.x + 2,
            y: output_area.y + 2,
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
                    Line::from(Span::styled(
                        label,
                        Style::default()
                            .bg(Color::Blue)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(label)
                }
            })
            .collect();

        let picker = Paragraph::new(rows)
            .block(Block::bordered().title(format!(" Files ({}) ", self.picker_results.len())))
            .style(Style::default().bg(Color::Black));

        frame.render_widget(Clear, overlay_area);
        frame.render_widget(picker, overlay_area);
    }

    /// Handle a paste event, folding large pastes into markers.
    /// Returns true if the paste was folded, false if inserted directly.
    pub fn handle_paste(&mut self, content: &str) -> bool {
        let folded = content.len() > self.paste_fold_threshold;

        if folded {
            // Create a marker for the folded paste
            let marker = format!("[pasted {} chars]", content.len());

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
