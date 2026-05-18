//! VT renderer for stripping ANSI escape sequences from PTY output.
//!
//! Implements `vte::Perform` to maintain a cell grid and extract clean
//! rendered text from raw terminal output.

use std::fmt;

use vte::{Params, Perform};

/// A single cell in the terminal grid.
#[derive(Debug, Clone, Default)]
struct Cell {
    ch: char,
}

/// VT renderer that maintains a cell grid and extracts clean text.
///
/// Implements `vte::Perform` to handle terminal escape sequences and
/// produce clean rendered text without ANSI codes.
#[derive(Debug, Clone)]
pub struct VtRenderer {
    grid: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    rows: usize,
    cols: usize,
}

impl VtRenderer {
    /// Create a new VT renderer with the given dimensions.
    pub fn new(rows: usize, cols: usize) -> Self {
        let grid = vec![vec![Cell::default(); cols]; rows];
        Self {
            grid,
            cursor_row: 0,
            cursor_col: 0,
            rows,
            cols,
        }
    }

    /// Extract clean rendered text from the cell grid.
    ///
    /// Returns the text content of all non-empty lines, with trailing
    /// whitespace stripped from each line.
    pub fn screen_text(&self) -> String {
        let mut lines = Vec::new();
        let mut current_line = String::new();

        for row in &self.grid {
            current_line.clear();
            for cell in row {
                if cell.ch != '\0' {
                    current_line.push(cell.ch);
                }
            }
            // Strip trailing whitespace
            let trimmed = current_line.trim_end();
            if !trimmed.is_empty() || lines.is_empty() {
                lines.push(trimmed.to_string());
            }
        }

        // Remove trailing empty lines
        while lines.last().map(|s| s.is_empty()).unwrap_or(false) {
            lines.pop();
        }

        lines.join("\n")
    }

    /// Clear the entire grid.
    fn clear_all(&mut self) {
        for row in &mut self.grid {
            for cell in row {
                *cell = Cell::default();
            }
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Clear from cursor to end of line.
    fn clear_line_from_cursor(&mut self) {
        if self.cursor_row < self.rows {
            for col in self.cursor_col..self.cols {
                self.grid[self.cursor_row][col] = Cell::default();
            }
        }
    }

    /// Clear entire line.
    fn clear_entire_line(&mut self) {
        if self.cursor_row < self.rows {
            for col in 0..self.cols {
                self.grid[self.cursor_row][col] = Cell::default();
            }
        }
    }

    /// Clear from start of line to cursor.
    fn clear_line_to_cursor(&mut self) {
        if self.cursor_row < self.rows {
            for col in 0..=self.cursor_col.min(self.cols - 1) {
                self.grid[self.cursor_row][col] = Cell::default();
            }
        }
    }

    /// Clear from cursor to end of screen.
    fn clear_screen_from_cursor(&mut self) {
        self.clear_line_from_cursor();
        for row in (self.cursor_row + 1)..self.rows {
            for col in 0..self.cols {
                self.grid[row][col] = Cell::default();
            }
        }
    }

    /// Clear entire screen.
    fn clear_entire_screen(&mut self) {
        self.clear_all();
    }

    /// Clear from start of screen to cursor.
    fn clear_screen_to_cursor(&mut self) {
        for row in 0..self.cursor_row {
            for col in 0..self.cols {
                self.grid[row][col] = Cell::default();
            }
        }
        self.clear_line_to_cursor();
    }

    /// Move cursor up, clamping to top.
    fn cursor_up(&mut self, n: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(n);
    }

    /// Move cursor down, clamping to bottom.
    fn cursor_down(&mut self, n: usize) {
        self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
    }

    /// Move cursor forward, clamping to right edge.
    fn cursor_forward(&mut self, n: usize) {
        self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
    }

    /// Move cursor backward, clamping to left edge.
    fn cursor_back(&mut self, n: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(n);
    }

    /// Move cursor to column.
    fn cursor_to_column(&mut self, col: usize) {
        self.cursor_col = col.min(self.cols - 1);
    }

    /// Move cursor to position.
    fn cursor_to_position(&mut self, row: usize, col: usize) {
        self.cursor_row = row.saturating_sub(1).min(self.rows - 1);
        self.cursor_col = col.saturating_sub(1).min(self.cols - 1);
    }

    /// Insert character at cursor, shifting right.
    fn put_char(&mut self, c: char) {
        if self.cursor_row < self.rows && self.cursor_col < self.cols {
            self.grid[self.cursor_row][self.cursor_col] = Cell { ch: c };
            self.cursor_col += 1;
            // Line wrap
            if self.cursor_col >= self.cols {
                self.cursor_col = 0;
                if self.cursor_row < self.rows - 1 {
                    self.cursor_row += 1;
                }
            }
        }
    }

    /// Parse raw bytes and feed them through the VT parser.
    pub fn parse_bytes(&mut self, bytes: &[u8]) {
        let mut parser = vte::Parser::new();
        parser.advance(self, bytes);
    }
}

impl Perform for VtRenderer {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => {
                // Carriage return: move to start of line
                self.cursor_col = 0;
            }
            b'\n' | 0x0B | 0x0C => {
                // Line feed / vertical tab / form feed: move down
                self.cursor_down(1);
            }
            b'\t' => {
                // Tab: move to next tab stop (every 8 columns)
                let next_tab = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next_tab.min(self.cols - 1);
            }
            b'\x08' => {
                // Backspace: move left
                self.cursor_back(1);
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
        let params: Vec<u16> = params.iter().map(|p| p[0]).collect();

        match action {
            'A' => {
                // Cursor Up (CUU)
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_up(n);
            }
            'B' => {
                // Cursor Down (CUD)
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_down(n);
            }
            'C' => {
                // Cursor Forward (CUF)
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_forward(n);
            }
            'D' => {
                // Cursor Back (CUB)
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_back(n);
            }
            'E' => {
                // Cursor Next Line (CNL)
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_col = 0;
                self.cursor_down(n);
            }
            'F' => {
                // Cursor Previous Line (CPL)
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_col = 0;
                self.cursor_up(n);
            }
            'G' | '`' => {
                // Cursor Horizontal Absolute (CHA)
                let col = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_to_column(col);
            }
            'H' | 'f' => {
                // Cursor Position (CUP)
                let row = params.first().copied().unwrap_or(1).max(1) as usize;
                let col = params.get(1).copied().unwrap_or(1).max(1) as usize;
                self.cursor_to_position(row, col);
            }
            'J' => {
                // Erase in Display (ED)
                let mode = params.first().copied().unwrap_or(0);
                match mode {
                    0 => self.clear_screen_from_cursor(),
                    1 => self.clear_screen_to_cursor(),
                    2 | 3 => self.clear_entire_screen(),
                    _ => {}
                }
            }
            'K' => {
                // Erase in Line (EL)
                let mode = params.first().copied().unwrap_or(0);
                match mode {
                    0 => self.clear_line_from_cursor(),
                    1 => self.clear_line_to_cursor(),
                    2 => self.clear_entire_line(),
                    _ => {}
                }
            }
            'S' => {
                // Scroll Up (SU)
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.grid.remove(0);
                    self.grid.push(vec![Cell::default(); self.cols]);
                }
            }
            'T' => {
                // Scroll Down (SD)
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.grid.pop();
                    self.grid.insert(0, vec![Cell::default(); self.cols]);
                }
            }
            'm' => {
                // Select Graphic Rendition (SGR) - colors, styles (ignored for text extraction)
            }
            's' => {
                // Save Cursor Position (DECSC) - ignored
            }
            'u' => {
                // Restore Cursor Position (DECRC) - ignored
            }
            _ => {
                // Ignore unknown CSI sequences
            }
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'M' => {
                // Reverse Index (RI) - move up, scroll if needed
                if self.cursor_row == 0 {
                    self.grid.pop();
                    self.grid.insert(0, vec![Cell::default(); self.cols]);
                } else {
                    self.cursor_up(1);
                }
            }
            b'7' => {
                // Save Cursor (DECSC) - ignored
            }
            b'8' => {
                // Restore Cursor (DECRC) - ignored
            }
            b'c' => {
                // Reset (RIS)
                self.clear_all();
            }
            _ => {}
        }
    }
}

impl fmt::Display for VtRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.screen_text())
    }
}

/// Strip progress bar artifacts from rendered text.
///
/// Progress bars use carriage returns to overwrite the current line, which
/// the VT renderer handles correctly. However, if a command is killed mid-progress,
/// the last line may contain a partial progress bar. This function strips lines
/// that match common progress patterns.
///
/// Patterns stripped:
/// - Progress bars: `===   |`, `[====    ]`, etc.
/// - Percentage lines: `45%`, `Downloading... 50%`
/// - Spinner characters: `|`, `/`, `-`, `\` alone or with text
/// - Download/upload/extract progress: `Downloading...`, `Uploading...`, `Extracting...`
pub fn strip_progress_artifacts(text: &str) -> String {
    use once_cell::sync::Lazy;
    use regex::Regex;

    static PROGRESS_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
        vec![
            // Progress bar with equals/dashes: "===   |", "[====    ]", etc.
            Regex::new(r"^[\[\(]?[=\-|#.]+\s*\]?\s*\|?\s*$").unwrap(),
            // Progress bar with percentage: "[====    ] 45%", "(###      ) 30%"
            Regex::new(r"^[\[\(][=\-#.\s]+[\]\)]\s+\d{1,3}%\s*$").unwrap(),
            // Percentage alone or with simple prefix: "45%", "Downloading 50%"
            Regex::new(r"^(?:[A-Za-z]+\s+)?\d{1,3}%\s*$").unwrap(),
            // Spinner characters alone: "|", "/", "-", "\"
            Regex::new(r"^[/|\\-]\s*$").unwrap(),
            // Download/upload/extract progress: "Downloading...", "Extracting..."
            Regex::new(r"^(?:Downloading|Uploading|Extracting|Installing|Building|Compiling)\s*\.{0,3}\s*\d*%?\s*$").unwrap(),
            // Cargo build progress: "Compiling crate v0.1.0" followed by percentage or bar
            Regex::new(r"^(?:Compiling|Checking)\s+\S+\s+v?\d.*\s+\[\d+/\d+\]\s*$").unwrap(),
            // npm/yarn progress bars
            Regex::new(r"^\s*\[+\s*#+\s*\]*\s*\d*%?\s*$").unwrap(),
        ]
    });

    let has_trailing_newline = text.ends_with('\n');

    // Process each line, handling carriage returns
    let mut result_lines = Vec::new();
    for line in text.lines() {
        // Handle carriage returns: \r overwrites the line, so take only the final segment
        // This simulates terminal behavior where "foo\rbar" shows as "bar"
        let final_segment = line.split('\r').next_back().unwrap_or(line);

        let trimmed = final_segment.trim();
        if trimmed.is_empty() {
            result_lines.push(final_segment);
            continue;
        }

        // Keep segment if it doesn't match any progress pattern
        if !PROGRESS_PATTERNS.iter().any(|p| p.is_match(trimmed)) {
            result_lines.push(final_segment);
        }
    }

    let mut result = result_lines.join("\n");

    if has_trailing_newline && !result.is_empty() {
        result.push('\n');
    }

    result
}

#[cfg(test)]
mod strip_progress_tests {
    use super::*;

    #[test]
    fn test_strip_percentage_line() {
        let input = "Downloading 50%\nDone\n";
        let output = strip_progress_artifacts(input);
        assert_eq!(output, "Done\n");
    }

    #[test]
    fn test_strip_progress_bar() {
        let input = "[====      ] 45%\nComplete\n";
        let output = strip_progress_artifacts(input);
        assert_eq!(output, "Complete\n");
    }

    #[test]
    fn test_strip_spinner() {
        let input = "|\n/\n-\n\\\nDone\n";
        let output = strip_progress_artifacts(input);
        assert_eq!(output, "Done\n");
    }

    #[test]
    fn test_keep_normal_output() {
        let input = "Compiling crate\nFinished\n";
        let output = strip_progress_artifacts(input);
        assert_eq!(output, "Compiling crate\nFinished\n");
    }

    #[test]
    fn test_mixed_output() {
        let input = "Starting...\nDownloading 50%\nDownloading 100%\nDone\n";
        let output = strip_progress_artifacts(input);
        assert_eq!(output, "Starting...\nDone\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_color_codes() {
        let mut renderer = VtRenderer::new(24, 80);
        // \033[31mred\033[0m normal
        renderer.parse_bytes(b"\x1b[31mred\x1b[0m normal");
        let text = renderer.screen_text();
        assert_eq!(text, "red normal");
    }

    #[test]
    fn test_clear_screen() {
        let mut renderer = VtRenderer::new(24, 80);
        // Write some text, then clear screen, then write new text
        renderer.parse_bytes(b"old text");
        renderer.parse_bytes(b"\x1b[2J"); // Clear entire screen
        renderer.parse_bytes(b"cleared");
        let text = renderer.screen_text();
        assert_eq!(text, "cleared");
    }

    #[test]
    fn test_line_redraw() {
        let mut renderer = VtRenderer::new(24, 80);
        // Simulate progress bar: "Downloading 50%\rDownloading 100%\nDone\n"
        renderer.parse_bytes(b"Downloading 50%\rDownloading 100%\nDone\n");
        let text = renderer.screen_text();
        assert_eq!(text, "Downloading 100%\nDone");
    }

    #[test]
    fn test_cursor_movement() {
        let mut renderer = VtRenderer::new(24, 80);
        // Write "hello", move cursor back 2, write "XY"
        renderer.parse_bytes(b"hello\x1b[2DXY");
        let text = renderer.screen_text();
        assert_eq!(text, "helXY");
    }

    #[test]
    fn test_simple_text() {
        let mut renderer = VtRenderer::new(24, 80);
        renderer.parse_bytes(b"hello world");
        let text = renderer.screen_text();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_multiple_lines() {
        let mut renderer = VtRenderer::new(24, 80);
        renderer.parse_bytes(b"line1\nline2\nline3");
        let text = renderer.screen_text();
        assert_eq!(text, "line1\nline2\nline3");
    }

    #[test]
    fn test_empty_output() {
        let renderer = VtRenderer::new(24, 80);
        let text = renderer.screen_text();
        assert_eq!(text, "");
    }
}
