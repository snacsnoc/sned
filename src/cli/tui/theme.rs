//! Centralized theme and color palette for the TUI.
//!
//! This module defines all colors and styles used in the TUI to ensure
//! visual consistency across the application.

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};
use std::sync::atomic::{AtomicBool, Ordering};

/// Palette switch. Set once at startup; only read afterwards.
static HIGH_CONTRAST: AtomicBool = AtomicBool::new(false);

/// Select the high-contrast palette.
pub fn set_high_contrast(enabled: bool) {
    HIGH_CONTRAST.store(enabled, Ordering::Release);
}

/// Whether the high-contrast palette is active. Builders consult this to
/// add weight (bold) to agent-voice text that brightness alone cannot lift.
#[must_use]
pub fn high_contrast() -> bool {
    HIGH_CONTRAST.load(Ordering::Acquire)
}

/// Bold `style` when `high_contrast`. Brightness caps at the ANSI brights;
/// weight is the remaining lever for agent-voice text.
pub(crate) fn emphasize_for(high_contrast: bool, style: Style) -> Style {
    if high_contrast {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// Reasoning text modifier. Italic renders thin on most terminals, so high
/// contrast uses bold instead.
pub(crate) fn reasoning_modifier_for(high_contrast: bool) -> Modifier {
    if high_contrast {
        Modifier::BOLD
    } else {
        Modifier::ITALIC
    }
}

fn current() -> &'static Palette {
    if HIGH_CONTRAST.load(Ordering::Acquire) {
        &HIGH_CONTRAST_PALETTE
    } else {
        &DEFAULT_PALETTE
    }
}

/// Border color - visible on dark terminals.
#[must_use]
pub fn border_fg() -> Color {
    current().border_fg
}

/// Status bar foreground. Load-bearing state lives here; it must stay
/// readable on dark terminals.
#[must_use]
pub fn status_fg() -> Color {
    current().status_fg
}

/// Accent color for active states (spinner, busy borders).
#[must_use]
pub fn accent() -> Color {
    current().accent
}

/// Selectable TUI palette. Default stays byte-identical to the
/// established scheme; high-contrast pairs bright colors with non-hue cues.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub border_fg: Color,
    pub accent: Color,
    pub status_fg: Color,
    pub prompt_fg: Color,
    pub echo_fg: Color,
    pub success_fg: Color,
    pub warning_fg: Color,
    pub error_fg: Color,
    pub tool_call_fg: Color,
    pub info_fg: Color,
    pub approval_fg: Color,
    pub reasoning_fg: Color,
    pub picker_selected_bg: Color,
    pub picker_selected_fg: Color,
    pub scrollbar_track_fg: Color,
    pub scrollbar_thumb_fg: Color,
    pub input_idle_fg: Color,
    /// Reasoning input border is double-lined: idle blue vs reasoning
    /// light-blue is otherwise a shade-only distinction.
    pub reasoning_double_border: bool,
    /// Bold the working-state border. Bright cyan alone reads thin at the
    /// one-cell border weight.
    pub bold_processing_border: bool,
    /// Dimmed hint text. Off in high-contrast: dimming is low contrast
    /// by definition.
    pub dim_hints: bool,
}

/// The default palette. Values preserve the established scheme exactly.
pub const DEFAULT_PALETTE: Palette = Palette {
    border_fg: Color::Gray,
    accent: Color::Cyan,
    status_fg: Color::DarkGray,
    prompt_fg: Color::LightGreen,
    echo_fg: Color::White,
    success_fg: Color::LightGreen,
    warning_fg: Color::LightYellow,
    error_fg: Color::Red,
    tool_call_fg: Color::Magenta,
    info_fg: Color::White,
    approval_fg: Color::LightMagenta,
    reasoning_fg: Color::LightBlue,
    picker_selected_bg: Color::Blue,
    picker_selected_fg: Color::White,
    scrollbar_track_fg: Color::DarkGray,
    scrollbar_thumb_fg: Color::Gray,
    input_idle_fg: Color::Blue,
    reasoning_double_border: false,
    bold_processing_border: false,
    dim_hints: true,
};

/// High-contrast palette: black ground, pure-RGB brights that bypass the
/// terminal palette (named ANSI brights render muted on many setups).
/// White appears only on the user's echo; selection and the status bar
/// carry their own saturated colors.
pub const HIGH_CONTRAST_PALETTE: Palette = Palette {
    border_fg: Color::Rgb(0, 0, 255),
    accent: Color::Rgb(0, 255, 255),
    status_fg: Color::Rgb(255, 255, 0),
    prompt_fg: Color::Rgb(0, 255, 0),
    echo_fg: Color::White,
    success_fg: Color::Rgb(0, 255, 0),
    warning_fg: Color::Rgb(255, 255, 0),
    error_fg: Color::Rgb(255, 0, 0),
    tool_call_fg: Color::Rgb(255, 0, 255),
    info_fg: Color::Rgb(0, 255, 255),
    approval_fg: Color::Rgb(255, 0, 255),
    reasoning_fg: Color::Rgb(0, 0, 255),
    picker_selected_bg: Color::Rgb(255, 255, 0),
    picker_selected_fg: Color::Black,
    scrollbar_track_fg: Color::Rgb(255, 255, 0),
    scrollbar_thumb_fg: Color::Black,
    input_idle_fg: Color::Rgb(0, 0, 255),
    reasoning_double_border: true,
    bold_processing_border: true,
    dim_hints: false,
};

/// Prompt-adjacent green: markdown inline code, tool success lines.
#[must_use]
pub fn prompt_fg() -> Color {
    current().prompt_fg
}

/// User echo color. Scoped to echo sites only; code spans and success
/// lines keep `prompt_fg`.
#[must_use]
pub fn echo_fg() -> Color {
    current().echo_fg
}

/// Success color for completed work and approval-ready input.
#[must_use]
pub fn success_fg() -> Color {
    current().success_fg
}

/// Warning color.
#[must_use]
pub fn warning_fg() -> Color {
    current().warning_fg
}

/// Error color.
#[must_use]
pub fn error_fg() -> Color {
    current().error_fg
}

/// Tool call color (e.g., execute_command, file operations).
#[must_use]
pub fn tool_call_fg() -> Color {
    current().tool_call_fg
}

/// Approval input border. Border-only, so it never meets magenta tool text.
#[must_use]
pub fn approval_fg() -> Color {
    current().approval_fg
}

/// Reasoning input border. Busy-wait, not a warning: never warning yellow.
#[must_use]
pub fn reasoning_fg() -> Color {
    current().reasoning_fg
}

/// Info/subtle color (dim white for status messages).
#[must_use]
pub fn info_fg() -> Color {
    current().info_fg
}

/// File picker selected row background.
#[must_use]
pub fn picker_selected_bg() -> Color {
    current().picker_selected_bg
}

/// File picker selected row foreground.
#[must_use]
pub fn picker_selected_fg() -> Color {
    current().picker_selected_fg
}

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
        .border_style(Style::default().fg(border_fg()))
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
#[must_use]
pub fn input_block(title: Option<String>, state: InputBorderState) -> Block<'static> {
    input_block_with(current(), title, state)
}

/// Pure palette-parameterized core of [`input_block`], so tests can render
/// either palette without flipping the process-wide selection.
pub(crate) fn input_block_with(
    palette: &Palette,
    title: Option<String>,
    state: InputBorderState,
) -> Block<'static> {
    let border_color = match state {
        InputBorderState::Idle => palette.input_idle_fg,
        InputBorderState::Processing => palette.accent,
        InputBorderState::Reasoning => palette.reasoning_fg,
        InputBorderState::Error => palette.error_fg,
        InputBorderState::Approval => palette.approval_fg,
    };
    let border_type = match state {
        InputBorderState::Reasoning if palette.reasoning_double_border => BorderType::Double,
        _ => BorderType::Rounded,
    };
    let mut border_style = Style::default().fg(border_color);
    if palette.bold_processing_border && state == InputBorderState::Processing {
        border_style = border_style.add_modifier(Modifier::BOLD);
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(border_style);
    match title {
        Some(title) => block.title(title),
        None => block,
    }
}

/// Create a styled block for overlays (file picker, etc.).
///
/// Square corners set overlays apart from the rounded input box.
///
/// # Arguments
/// * `title` - The title to display
///
/// # Returns
/// A `Block` with:
/// - Plain (square) border type
/// - DarkGray border color
/// - Transparent background
/// - The provided title
pub fn overlay_block(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(border_fg()))
        .title(title.into())
}

/// Style for status bar text. High contrast inverts the bar.
#[must_use]
pub fn status_style() -> Style {
    status_style_for(HIGH_CONTRAST.load(Ordering::Acquire))
}

pub(crate) fn status_style_for(high_contrast: bool) -> Style {
    if high_contrast {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(255, 255, 0))
    } else {
        Style::default().fg(status_fg())
    }
}

/// Error-segment style for the status bar (dropped-output count). Plain
/// status color by default; white on red under high contrast.
#[must_use]
pub fn status_alert_style() -> Style {
    status_alert_style_for(HIGH_CONTRAST.load(Ordering::Acquire))
}

pub(crate) fn status_alert_style_for(high_contrast: bool) -> Style {
    if high_contrast {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(255, 0, 0))
            .add_modifier(Modifier::BOLD)
    } else {
        status_style_for(false)
    }
}

/// Style for dim text (hints, metadata). Plain under high contrast.
#[must_use]
pub fn dim_style() -> Style {
    dim_style_for(current().dim_hints)
}

pub(crate) fn dim_style_for(dim_hints: bool) -> Style {
    if dim_hints {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    }
}

/// Style for bold text (headers, emphasis).
#[must_use]
pub fn bold_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Style for selected file picker row.
#[must_use]
pub fn picker_selected_style() -> Style {
    let palette = current();
    Style::default()
        .bg(palette.picker_selected_bg)
        .fg(palette.picker_selected_fg)
        .add_modifier(Modifier::BOLD)
}

#[must_use]
pub fn selection_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Style for scrollbar track.
#[must_use]
pub fn scrollbar_style() -> Style {
    Style::default().fg(current().scrollbar_track_fg)
}

/// Style for scrollbar thumb (the movable part).
#[must_use]
pub fn scrollbar_thumb_style() -> Style {
    Style::default().fg(current().scrollbar_thumb_fg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    #[test]
    fn default_palette_preserves_established_scheme() {
        assert_eq!(DEFAULT_PALETTE.status_fg, Color::DarkGray);
        assert_eq!(DEFAULT_PALETTE.border_fg, Color::Gray);
        assert_eq!(DEFAULT_PALETTE.error_fg, Color::Red);
        assert_eq!(DEFAULT_PALETTE.picker_selected_bg, Color::Blue);
        assert_eq!(DEFAULT_PALETTE.picker_selected_fg, Color::White);
        assert!(!DEFAULT_PALETTE.reasoning_double_border);
        assert!(DEFAULT_PALETTE.dim_hints);
    }

    #[test]
    fn high_contrast_palette_is_dos_bright() {
        use Color::Rgb;
        assert_eq!(HIGH_CONTRAST_PALETTE.border_fg, Rgb(0, 0, 255));
        assert_eq!(HIGH_CONTRAST_PALETTE.input_idle_fg, Rgb(0, 0, 255));
        assert_eq!(HIGH_CONTRAST_PALETTE.error_fg, Rgb(255, 0, 0));
        assert_eq!(HIGH_CONTRAST_PALETTE.warning_fg, Rgb(255, 255, 0));
        assert_eq!(HIGH_CONTRAST_PALETTE.success_fg, Rgb(0, 255, 0));
        assert_eq!(HIGH_CONTRAST_PALETTE.info_fg, Rgb(0, 255, 255));
        assert_eq!(HIGH_CONTRAST_PALETTE.tool_call_fg, Rgb(255, 0, 255));
        assert_eq!(HIGH_CONTRAST_PALETTE.echo_fg, Color::White);
        assert_eq!(
            HIGH_CONTRAST_PALETTE.picker_selected_bg,
            Rgb(255, 255, 0)
        );
        assert_eq!(HIGH_CONTRAST_PALETTE.picker_selected_fg, Color::Black);
        assert!(HIGH_CONTRAST_PALETTE.reasoning_double_border);
        assert!(!HIGH_CONTRAST_PALETTE.dim_hints);
    }

    fn input_corner(palette: &Palette, state: InputBorderState) -> String {
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("").block(input_block_with(palette, None, state)),
                    frame.area(),
                );
            })
            .expect("render should succeed");
        terminal
            .backend()
            .buffer()
            .cell((0, 0))
            .map(|cell| cell.symbol().to_string())
            .unwrap_or_default()
    }

    #[test]
    fn status_style_inverts_under_high_contrast() {
        assert_eq!(
            status_style_for(false),
            Style::default().fg(Color::DarkGray)
        );
        assert_eq!(
            status_style_for(true),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(255, 255, 0))
        );
        assert_eq!(
            status_alert_style_for(false),
            Style::default().fg(Color::DarkGray)
        );
        assert_eq!(
            status_alert_style_for(true),
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(255, 0, 0))
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            dim_style_for(true),
            Style::default().add_modifier(Modifier::DIM)
        );
        assert_eq!(dim_style_for(false), Style::default());
    }

    #[test]
    fn emphasis_helpers_bold_only_under_high_contrast() {
        let base = Style::default().fg(Color::Magenta);
        assert_eq!(emphasize_for(false, base), base);
        assert_eq!(
            emphasize_for(true, base),
            base.add_modifier(Modifier::BOLD)
        );
        assert_eq!(reasoning_modifier_for(false), Modifier::ITALIC);
        assert_eq!(reasoning_modifier_for(true), Modifier::BOLD);
    }

    #[test]
    fn processing_border_bolds_only_under_high_contrast() {
        use ratatui::style::Modifier;
        for (palette, bold) in [
            (&DEFAULT_PALETTE, false),
            (&HIGH_CONTRAST_PALETTE, true),
        ] {
            let backend = TestBackend::new(10, 3);
            let mut terminal = Terminal::new(backend).expect("terminal should initialize");
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("").block(input_block_with(
                            palette,
                            None,
                            InputBorderState::Processing,
                        )),
                        frame.area(),
                    );
                })
                .expect("render should succeed");
            let modifiers = terminal
                .backend()
                .buffer()
                .cell((0, 0))
                .map(|cell| cell.modifier)
                .unwrap_or_default();
            assert_eq!(modifiers.contains(Modifier::BOLD), bold);
        }
    }

    #[test]
    fn reasoning_border_shape_follows_palette() {
        assert_eq!(
            input_corner(&DEFAULT_PALETTE, InputBorderState::Reasoning),
            "╭"
        );
        assert_eq!(
            input_corner(&HIGH_CONTRAST_PALETTE, InputBorderState::Reasoning),
            "╔"
        );
        assert_eq!(
            input_corner(&HIGH_CONTRAST_PALETTE, InputBorderState::Idle),
            "╭"
        );
    }
}
