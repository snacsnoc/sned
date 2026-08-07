//! Centralized theme and color palette for the TUI.
//!
//! This module defines all colors and styles used in the TUI to ensure
//! visual consistency across the application.

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};

/// Border color - visible on dark terminals.
pub const BORDER_FG: Color = Color::Gray;

/// Accent color for active states (spinner, busy borders).
pub const ACCENT: Color = Color::Cyan;

/// Status bar foreground (dim for subtle appearance).
pub const STATUS_FG: Color = Color::DarkGray;

/// Prompt echo color (user input confirmation).
pub const PROMPT_FG: Color = Color::LightGreen;

/// Success color for completed work and approval-ready input.
pub const SUCCESS_FG: Color = Color::LightGreen;

/// Warning color.
pub const WARNING_FG: Color = Color::LightYellow;

/// Error color.
pub const ERROR_FG: Color = Color::Red;

/// Tool call color (e.g., execute_command, file operations).
pub const TOOL_CALL_FG: Color = Color::Magenta;

/// Info/subtle color (dim white for status messages).
pub const INFO_FG: Color = Color::White;

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

/// Visual state for the prompt border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBorderState {
    Idle,
    Processing,
    Reasoning,
    Error,
    Approval,
}

/// Create a styled block for the input area.
pub fn input_block(title: Option<String>, state: InputBorderState) -> Block<'static> {
    let border_color = match state {
        InputBorderState::Idle => Color::Blue,
        InputBorderState::Processing => ACCENT,
        InputBorderState::Reasoning => WARNING_FG,
        InputBorderState::Error => ERROR_FG,
        InputBorderState::Approval => SUCCESS_FG,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    match title {
        Some(title) => block.title(title),
        None => block,
    }
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
#[must_use]
pub fn status_style() -> Style {
    Style::default().fg(STATUS_FG)
}

/// Style for dim text (hints, metadata).
#[must_use]
pub fn dim_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Style for bold text (headers, emphasis).
#[must_use]
pub fn bold_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Style for selected file picker row.
#[must_use]
pub fn picker_selected_style() -> Style {
    Style::default()
        .bg(PICKER_SELECTED_BG)
        .fg(PICKER_SELECTED_FG)
        .add_modifier(Modifier::BOLD)
}

#[must_use]
pub fn selection_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Style for scrollbar track.
#[must_use]
pub fn scrollbar_style() -> Style {
    Style::default().fg(STATUS_FG)
}

/// Style for scrollbar thumb (the movable part).
#[must_use]
pub fn scrollbar_thumb_style() -> Style {
    Style::default().fg(BORDER_FG)
}
