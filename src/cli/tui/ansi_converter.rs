//! ANSI to ratatui converter.
//!
//! Parses ANSI escape sequences and converts them to ratatui Line<Span> structures.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use vte::{Params, Perform};

/// Convert a string with ANSI escape sequences to ratatui lines.
pub fn ansi_to_ratatui_lines(text: &str) -> Vec<Line<'static>> {
    let mut performer = RatatuiPerformer::new();
    let mut parser = vte::Parser::new();
    parser.advance(&mut performer, text.as_bytes());
    performer.finish()
}

/// VTE performer that builds ratatui Lines.
struct RatatuiPerformer {
    current_style: Style,
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
}

impl RatatuiPerformer {
    fn new() -> Self {
        Self {
            current_style: Style::default(),
            lines: Vec::new(),
            current_spans: Vec::new(),
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current_spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.current_spans)));
        }
        self.lines
    }
}

impl Perform for RatatuiPerformer {
    fn print(&mut self, c: char) {
        self.current_spans.push(Span::styled(
            c.to_string(),
            self.current_style,
        ));
    }

    fn execute(&mut self, byte: u8) {
        if byte == 0x0A { // newline
            self.lines.push(Line::from(std::mem::take(&mut self.current_spans)));
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        if action == 'm' {
            // SGR — Select Graphic Rendition
            for param in params.iter() {
                match param {
                    [0] => self.current_style = Style::default(),
                    [1] => self.current_style = self.current_style.add_modifier(Modifier::BOLD),
                    [2] => self.current_style = self.current_style.add_modifier(Modifier::DIM),
                    [3] => self.current_style = self.current_style.add_modifier(Modifier::ITALIC),
                    [4] => self.current_style = self.current_style.add_modifier(Modifier::UNDERLINED),
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
                    // 256-color: [38, 5, N] → Color::Indexed(N)
                    [38, 5, n] => self.current_style = self.current_style.fg(Color::Indexed(*n as u8)),
                    // Truecolor: [38, 2, R, G, B] → Color::Rgb(R, G, B)
                    [38, 2, r, g, b] => {
                        self.current_style = self.current_style.fg(Color::Rgb(*r as u8, *g as u8, *b as u8))
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
        assert!(lines[0].spans[0].style.add_modifier.intersects(Modifier::BOLD));
    }

    #[test]
    fn test_color() {
        let lines = ansi_to_ratatui_lines("\x1b[31mred\x1b[0m");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
    }
}
