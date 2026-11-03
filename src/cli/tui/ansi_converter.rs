//! ANSI to ratatui converter.
//!
//! Parses ANSI escape sequences and converts them to ratatui Line<Span> structures.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use vte::{Params, Perform};

/// Convert a string with ANSI escape sequences to ratatui lines.
#[must_use]
pub fn ansi_to_ratatui_lines(text: &str) -> Vec<Line<'static>> {
    let mut performer = RatatuiPerformer::with_capacity(text.len());
    let mut parser = vte::Parser::new();
    parser.advance(&mut performer, text.as_bytes());
    performer.finish()
}

/// VTE performer that builds ratatui Lines.
struct RatatuiPerformer {
    current_style: Style,
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    current_text: String,
    current_column: usize,
    pending_cr: bool,
}

impl RatatuiPerformer {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            current_style: Style::default(),
            lines: Vec::new(),
            current_spans: Vec::new(),
            current_text: String::with_capacity(capacity),
            current_column: 0,
            pending_cr: false,
        }
    }

    // A carriage return overwrites the in-progress segment; a CR
    // immediately followed by LF is a Windows line ending instead.
    fn resolve_pending_cr(&mut self) {
        if self.pending_cr {
            self.pending_cr = false;
            self.current_text.clear();
            self.current_column = 0;
        }
    }

    // A lone 38/48 with no recognized follower stays inert.
    fn extended_color<'a, I>(params_iter: &mut std::iter::Peekable<I>) -> Option<Color>
    where
        I: Iterator<Item = &'a [u16]>,
    {
        match params_iter.peek().copied() {
            Some([5]) => {
                params_iter.next();
                params_iter
                    .next()
                    .and_then(|param| param.first())
                    .map(|n| Color::Indexed((*n).min(255) as u8))
            }
            Some([2]) => {
                params_iter.next();
                let mut rgb = [0u8; 3];
                for slot in rgb.iter_mut() {
                    let component = params_iter.next()?.first()?;
                    *slot = (*component).min(255) as u8;
                }
                Some(Color::Rgb(rgb[0], rgb[1], rgb[2]))
            }
            _ => None,
        }
    }

    fn flush_current_text(&mut self) {
        if !self.current_text.is_empty() {
            self.current_spans.push(Span::styled(
                std::mem::take(&mut self.current_text),
                self.current_style,
            ));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_current_text();
        if !self.current_spans.is_empty() {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current_spans)));
        }
        self.lines
    }
}

impl Perform for RatatuiPerformer {
    fn print(&mut self, c: char) {
        self.resolve_pending_cr();
        self.current_text.push(c);
        self.current_column += usize::from(c != '\t');
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0D => {
                self.pending_cr = true;
                return;
            }
            0x0A => {
                self.pending_cr = false;
                self.flush_current_text();
                self.lines
                    .push(Line::from(std::mem::take(&mut self.current_spans)));
                self.current_column = 0;
            }
            0x09 => {
                self.resolve_pending_cr();
                let remaining = 4 - (self.current_column % 4);
                for _ in 0..remaining {
                    self.current_text.push(' ');
                }
                self.current_column += remaining;
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        if action == 'm' {
            // SGR — Select Graphic Rendition
            // Flush accumulated text before changing style
            self.resolve_pending_cr();
            self.flush_current_text();
            let mut params_iter = params.iter().peekable();
            while let Some(param) = params_iter.next() {
                match param {
                    [0] => self.current_style = Style::default(),
                    [1] => self.current_style = self.current_style.add_modifier(Modifier::BOLD),
                    [2] => self.current_style = self.current_style.add_modifier(Modifier::DIM),
                    [3] => self.current_style = self.current_style.add_modifier(Modifier::ITALIC),
                    [4] => {
                        self.current_style = self.current_style.add_modifier(Modifier::UNDERLINED);
                    }
                    [30] => self.current_style = self.current_style.fg(Color::Black),
                    [31 | 91] => self.current_style = self.current_style.fg(Color::Red),
                    [32 | 92] => self.current_style = self.current_style.fg(Color::Green),
                    [33 | 93] => self.current_style = self.current_style.fg(Color::Yellow),
                    [34 | 94] => self.current_style = self.current_style.fg(Color::Blue),
                    [35 | 95] => self.current_style = self.current_style.fg(Color::Magenta),
                    [36 | 96] => self.current_style = self.current_style.fg(Color::Cyan),
                    [37 | 97] => self.current_style = self.current_style.fg(Color::White),
                    [90] => self.current_style = self.current_style.fg(Color::DarkGray),
                    // Background colors: 40-47
                    [40] => self.current_style = self.current_style.bg(Color::Black),
                    [41] => self.current_style = self.current_style.bg(Color::Red),
                    [42] => self.current_style = self.current_style.bg(Color::Green),
                    [43] => self.current_style = self.current_style.bg(Color::Yellow),
                    [44] => self.current_style = self.current_style.bg(Color::Blue),
                    [45] => self.current_style = self.current_style.bg(Color::Magenta),
                    [46] => self.current_style = self.current_style.bg(Color::Cyan),
                    [47] => self.current_style = self.current_style.bg(Color::White),
                    // Bright background colors: 100-107 (use Indexed colors 8-15 for bright palette)
                    [100] => self.current_style = self.current_style.bg(Color::Indexed(8)), // Bright black
                    [101] => self.current_style = self.current_style.bg(Color::Indexed(9)), // Bright red
                    [102] => self.current_style = self.current_style.bg(Color::Indexed(10)), // Bright green
                    [103] => self.current_style = self.current_style.bg(Color::Indexed(11)), // Bright yellow
                    [104] => self.current_style = self.current_style.bg(Color::Indexed(12)), // Bright blue
                    [105] => self.current_style = self.current_style.bg(Color::Indexed(13)), // Bright magenta
                    [106] => self.current_style = self.current_style.bg(Color::Indexed(14)), // Bright cyan
                    [107] => self.current_style = self.current_style.bg(Color::Indexed(15)), // Bright white
                    // Blinking
                    [5] => {
                        self.current_style = self.current_style.add_modifier(Modifier::SLOW_BLINK);
                    }
                    // Reverse video
                    [7] => self.current_style = self.current_style.add_modifier(Modifier::REVERSED),
                    // Modifier resets
                    [22] => {
                        self.current_style = self.current_style.remove_modifier(Modifier::BOLD);
                        self.current_style = self.current_style.remove_modifier(Modifier::DIM);
                    }
                    [23] => {
                        self.current_style = self.current_style.remove_modifier(Modifier::ITALIC);
                    }
                    [24] => {
                        self.current_style =
                            self.current_style.remove_modifier(Modifier::UNDERLINED);
                    }
                    [25] => {
                        self.current_style =
                            self.current_style.remove_modifier(Modifier::SLOW_BLINK);
                    }
                    [27] => {
                        self.current_style = self.current_style.remove_modifier(Modifier::REVERSED);
                    }
                    // Default color resets
                    [39] => self.current_style.fg = None,
                    [49] => self.current_style.bg = None,
                    // Colon-form equivalents match the combined arms below.
                    [38] => {
                        if let Some(color) = Self::extended_color(&mut params_iter) {
                            self.current_style = self.current_style.fg(color);
                        }
                    }
                    [48] => {
                        if let Some(color) = Self::extended_color(&mut params_iter) {
                            self.current_style = self.current_style.bg(color);
                        }
                    }
                    // 256-color: [38, 5, N] → Color::Indexed(N)
                    [38, 5, n] => {
                        self.current_style = self.current_style.fg(Color::Indexed(*n as u8));
                    }
                    // Truecolor: [38, 2, R, G, B] → Color::Rgb(R, G, B)
                    [38, 2, r, g, b] => {
                        self.current_style = self
                            .current_style
                            .fg(Color::Rgb(*r as u8, *g as u8, *b as u8));
                    }
                    _ => {}
                }
            }
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text() {
        let lines = ansi_to_ratatui_lines("hello\nworld");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "hello");
        assert_eq!(lines[1].to_string(), "world");
    }

    #[test]
    fn test_bold() {
        let lines = ansi_to_ratatui_lines("\x1b[1mbold\x1b[0m");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .intersects(Modifier::BOLD)
        );
    }

    #[test]
    fn test_color() {
        let lines = ansi_to_ratatui_lines("\x1b[31mred\x1b[0m");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn test_tab_expands_to_spaces() {
        let lines = ansi_to_ratatui_lines("col1\tvalue");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "col1    value");
    }

    #[test]
    fn test_tab_expands_to_next_tab_stop() {
        let lines = ansi_to_ratatui_lines("abcdefgh\tvalue");
        assert_eq!(lines.len(), 1);
        // "abcdefgh" is 8 chars, next tab stop at 12 → 4 spaces
        assert_eq!(lines[0].to_string(), "abcdefgh    value");
    }

    #[test]
    fn test_sgr_default_fg_reset() {
        let lines = ansi_to_ratatui_lines("\x1b[31mred\x1b[39mreset");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[0].spans[1].style.fg, None);
    }

    #[test]
    fn test_sgr_default_bg_reset() {
        let lines = ansi_to_ratatui_lines("\x1b[41mred_bg\x1b[49mreset");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Red));
        assert_eq!(lines[0].spans[1].style.bg, None);
    }

    #[test]
    fn test_sgr_blink_off_reset() {
        let lines = ansi_to_ratatui_lines("\x1b[5mblink\x1b[25mreset");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .intersects(Modifier::SLOW_BLINK)
        );
        assert!(
            !lines[0].spans[1]
                .style
                .add_modifier
                .intersects(Modifier::SLOW_BLINK)
        );
    }

    #[test]
    fn test_sgr_combined_bold_and_color() {
        let lines = ansi_to_ratatui_lines("\x1b[1;31mboldred\x1b[0m");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .intersects(Modifier::BOLD)
        );
    }

    #[test]
    fn test_sgr_semicolon_256_color() {
        let lines = ansi_to_ratatui_lines("\x1b[38;5;196mextended\x1b[0m");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(196)));
        assert!(
            !lines[0].spans[0]
                .style
                .add_modifier
                .intersects(Modifier::SLOW_BLINK)
        );
    }

    #[test]
    fn test_sgr_semicolon_truecolor() {
        let lines = ansi_to_ratatui_lines("\x1b[38;2;255;0;0mrgb\x1b[0m");
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(Color::Rgb(255, 0, 0))
        );
    }

    #[test]
    fn test_sgr_semicolon_256_bg() {
        let lines = ansi_to_ratatui_lines("\x1b[48;5;27mbg\x1b[0m");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Indexed(27)));
    }

    #[test]
    fn test_cr_overwrites_progress_segment() {
        let lines = ansi_to_ratatui_lines("50%\r100%");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "100%");
    }

    #[test]
    fn test_crlf_keeps_line() {
        let lines = ansi_to_ratatui_lines("foo\r\nbar");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "foo");
        assert_eq!(lines[1].to_string(), "bar");
    }

    #[test]
    fn test_sgr_reverse_off_reset() {
        let lines = ansi_to_ratatui_lines("\x1b[7mreverse\x1b[27mreset");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .intersects(Modifier::REVERSED)
        );
        assert!(
            !lines[0].spans[1]
                .style
                .add_modifier
                .intersects(Modifier::REVERSED)
        );
    }
}
