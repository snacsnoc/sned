//! ANSI to ratatui converter.
//!
//! Parses ANSI escape sequences and converts them to ratatui Line<Span> structures.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use vte::{Params, Perform};

/// Convert a string with ANSI escape sequences to ratatui lines.
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
}

impl RatatuiPerformer {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            current_style: Style::default(),
            lines: Vec::new(),
            current_spans: Vec::new(),
            current_text: String::with_capacity(capacity),
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
        self.current_text.push(c);
    }

    fn execute(&mut self, byte: u8) {
        if byte == 0x0A {
            // newline
            self.flush_current_text();
            self.lines
                .push(Line::from(std::mem::take(&mut self.current_spans)));
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
            self.flush_current_text();
            for param in params.iter() {
                match param {
                    [0] => self.current_style = Style::default(),
                    [1] => self.current_style = self.current_style.add_modifier(Modifier::BOLD),
                    [2] => self.current_style = self.current_style.add_modifier(Modifier::DIM),
                    [3] => self.current_style = self.current_style.add_modifier(Modifier::ITALIC),
                    [4] => {
                        self.current_style = self.current_style.add_modifier(Modifier::UNDERLINED)
                    }
                    [30] => self.current_style = self.current_style.fg(Color::Black),
                    [31] => self.current_style = self.current_style.fg(Color::Red),
                    [32] => self.current_style = self.current_style.fg(Color::Green),
                    [33] => self.current_style = self.current_style.fg(Color::Yellow),
                    [34] => self.current_style = self.current_style.fg(Color::Blue),
                    [35] => self.current_style = self.current_style.fg(Color::Magenta),
                    [36] => self.current_style = self.current_style.fg(Color::Cyan),
                    [37] => self.current_style = self.current_style.fg(Color::White),
                    [90] => self.current_style = self.current_style.fg(Color::DarkGray),
                    [91] => self.current_style = self.current_style.fg(Color::Red),
                    [92] => self.current_style = self.current_style.fg(Color::Green),
                    [93] => self.current_style = self.current_style.fg(Color::Yellow),
                    [94] => self.current_style = self.current_style.fg(Color::Blue),
                    [95] => self.current_style = self.current_style.fg(Color::Magenta),
                    [96] => self.current_style = self.current_style.fg(Color::Cyan),
                    [97] => self.current_style = self.current_style.fg(Color::White),
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
                        self.current_style = self.current_style.add_modifier(Modifier::SLOW_BLINK)
                    }
                    // Reverse video
                    [7] => self.current_style = self.current_style.add_modifier(Modifier::REVERSED),
                    // Modifier resets
                    [22] => {
                        self.current_style = self.current_style.remove_modifier(Modifier::BOLD);
                        self.current_style = self.current_style.remove_modifier(Modifier::DIM);
                    }
                    [23] => {
                        self.current_style = self.current_style.remove_modifier(Modifier::ITALIC)
                    }
                    [24] => {
                        self.current_style =
                            self.current_style.remove_modifier(Modifier::UNDERLINED)
                    }
                    // 256-color: [38, 5, N] → Color::Indexed(N)
                    [38, 5, n] => {
                        self.current_style = self.current_style.fg(Color::Indexed(*n as u8))
                    }
                    // Truecolor: [38, 2, R, G, B] → Color::Rgb(R, G, B)
                    [38, 2, r, g, b] => {
                        self.current_style = self
                            .current_style
                            .fg(Color::Rgb(*r as u8, *g as u8, *b as u8))
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
}
