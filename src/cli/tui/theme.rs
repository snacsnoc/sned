//! Centralized theme and color palette for the TUI.
//!
//! This module defines all colors and styles used in the TUI to ensure
//! visual consistency across the application.

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};

/// Border color - subtle dark gray that doesn't compete with content.
pub const BORDER_FG: Color = Color::DarkGray;

/// Accent color for active states (spinner, busy borders).
pub const ACCENT: Color = Color::Cyan;

/// Status bar background.
pub const STATUS_BG: Color = Color::DarkGray;

/// Status bar foreground.
pub const STATUS_FG: Color = Color::White;

/// Prompt echo color (user input confirmation).
pub const PROMPT_FG: Color = Color::Green;

/// Warning color.
pub const WARNING_FG: Color = Color::Yellow;

/// Error color.
pub const ERROR_FG: Color = Color::Red;

/// File picker selected row background.
pub const PICKER_SELECTED_BG: Color = Color::Blue;

/// File picker selected row foreground.
pub const PICKER_SELECTED_FG: Color = Color::White;

/// Create a styled block with rounded borders and the theme's border color.
///
/// # Arguments
/// * `title` - The title to display on the block (left-aligned)
///
/// # Returns
/// A `Block` with:
/// - Rounded border type
/// - DarkGray border color
/// - The provided title
pub fn border_block(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER_FG))
        .title(title.into())
}

/// Create a styled block for the input area.
///
/// # Arguments
/// * `title` - The title to display (left-aligned)
/// * `busy` - Whether the agent is currently busy
///
/// # Returns
/// A `Block` with:
/// - Rounded border type
/// - Cyan border when busy, DarkGray when idle
/// - The provided title
pub fn input_block(title: impl Into<String>, busy: bool) -> Block<'static> {
    let border_color = if busy { ACCENT } else { BORDER_FG };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title.into())
}

/// Create a styled block for overlays (file picker, etc.).
///
/// # Arguments
/// * `title` - The title to display
///
/// # Returns
/// A `Block` with:
/// - Rounded border type
/// - DarkGray border color
/// - Transparent background
/// - The provided title
pub fn overlay_block(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER_FG))
        .title(title.into())
}

/// Style for status bar text.
pub fn status_style() -> Style {
    Style::default().fg(STATUS_FG).bg(STATUS_BG)
}

/// Style for dim text (hints, metadata).
pub fn dim_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Style for bold text (headers, emphasis).
pub fn bold_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Style for the spinner character.
pub fn spinner_style() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Style for selected file picker row.
pub fn picker_selected_style() -> Style {
    Style::default()
        .bg(PICKER_SELECTED_BG)
        .fg(PICKER_SELECTED_FG)
        .add_modifier(Modifier::BOLD)
}

/// Style for scrollbar.
pub fn scrollbar_style() -> Style {
    Style::default().fg(BORDER_FG)
}

/// Style for scrollbar thumb (the movable part).
pub fn scrollbar_thumb_style() -> Style {
    Style::default().fg(ACCENT)
}
