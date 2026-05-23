use crate::core::file_search::{FileSearchResult, FileType};
use std::io::Write;

const MAX_VISIBLE_RESULTS: usize = 8;

pub struct FilePicker {
    results: Vec<FileSearchResult>,
    pub selected_index: usize,
    query: String,
    scroll_offset: usize,
}

impl FilePicker {
    pub fn new(_rows: usize, _cols: usize) -> Self {
        Self {
            results: Vec::new(),
            selected_index: 0,
            query: String::new(),
            scroll_offset: 0,
        }
    }

    pub fn update_results(&mut self, results: Vec<FileSearchResult>) {
        self.results = results;
        self.selected_index = self
            .selected_index
            .min(self.results.len().saturating_sub(1));
        self.scroll_offset = 0;
    }

    pub fn selected(&self) -> Option<&FileSearchResult> {
        self.results.get(self.selected_index)
    }

    pub fn up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            if self.selected_index < self.scroll_offset {
                self.scroll_offset = self.selected_index;
            }
        }
    }

    pub fn down(&mut self) {
        if self.selected_index < self.results.len().saturating_sub(1) {
            self.selected_index += 1;
            let visible_end = self.scroll_offset + MAX_VISIBLE_RESULTS;
            if self.selected_index >= visible_end {
                self.scroll_offset = self.selected_index - MAX_VISIBLE_RESULTS + 1;
            }
        }
    }

    pub fn page_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(MAX_VISIBLE_RESULTS);
        self.scroll_offset = self.selected_index;
    }

    pub fn page_down(&mut self) {
        self.selected_index =
            (self.selected_index + MAX_VISIBLE_RESULTS).min(self.results.len().saturating_sub(1));
        let visible_end = self.scroll_offset + MAX_VISIBLE_RESULTS;
        if self.selected_index >= visible_end {
            self.scroll_offset = self.selected_index - MAX_VISIBLE_RESULTS + 1;
        }
    }

    pub fn home(&mut self) {
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    pub fn end(&mut self) {
        self.selected_index = self.results.len().saturating_sub(1);
        self.scroll_offset = self.selected_index.saturating_sub(MAX_VISIBLE_RESULTS - 1);
    }

    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
    }

    pub fn render_to<W: Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.render_at(writer, 0)
    }

    pub fn render_at<W: Write>(&mut self, writer: &mut W, start_row: usize) -> std::io::Result<()> {
        let fg_header = "\x1b[38;2;150;150;150m";
        let fg_selected = "\x1b[38;2;0;0;0m";
        let bg_selected = "\x1b[48;2;100;149;237m";
        let fg_normal = "\x1b[0m";
        let reset = "\x1b[0m";

        if self.results.is_empty() {
            let msg = if self.query.is_empty() {
                "Type to search files..."
            } else {
                "No matching files"
            };
            writeln!(
                writer,
                "\x1b[{};1H{}{}{}",
                start_row + 1,
                fg_header,
                msg,
                reset
            )?;
        } else {
            let visible: Vec<_> = self
                .results
                .iter()
                .skip(self.scroll_offset)
                .take(MAX_VISIBLE_RESULTS)
                .enumerate()
                .collect();

            for (dy, (i, result)) in visible.iter().enumerate() {
                let global_index = self.scroll_offset + i;
                let is_selected = global_index == self.selected_index;
                let prefix = if is_selected { ">" } else { " " };
                let label = if result.file_type == FileType::Folder {
                    format!("{}/", result.label)
                } else {
                    result.label.clone()
                };
                let line = format!("{} {}", prefix, label);

                if is_selected {
                    writeln!(
                        writer,
                        "\x1b[{};1H{}{}{}{}",
                        start_row + dy + 1,
                        fg_selected,
                        bg_selected,
                        line,
                        reset
                    )?;
                } else {
                    writeln!(
                        writer,
                        "\x1b[{};1H{}{}{}",
                        start_row + dy + 1,
                        fg_normal,
                        line,
                        reset
                    )?;
                }
            }

            if self.results.len() > MAX_VISIBLE_RESULTS {
                let indicator = format!("  ▼ {} files", self.results.len());
                writeln!(
                    writer,
                    "\x1b[{};1H{}{}{}",
                    start_row + MAX_VISIBLE_RESULTS + 1,
                    fg_header,
                    indicator,
                    reset
                )?;
            }
        }

        Ok(())
    }

    pub fn overlay_height(&self) -> usize {
        if self.results.is_empty() {
            3
        } else {
            (self.results.len().min(MAX_VISIBLE_RESULTS) + 3).min(13)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_picker_creation() {
        let picker = FilePicker::new(24, 80);
        assert_eq!(picker.selected_index, 0);
        assert!(picker.selected().is_none());
    }

    #[test]
    fn test_picker_navigation() {
        let mut picker = FilePicker::new(24, 80);
        let results = vec![
            FileSearchResult {
                path: "a.rs".into(),
                file_type: FileType::File,
                label: "a.rs".into(),
            },
            FileSearchResult {
                path: "b.rs".into(),
                file_type: FileType::File,
                label: "b.rs".into(),
            },
            FileSearchResult {
                path: "c.rs".into(),
                file_type: FileType::Folder,
                label: "c".into(),
            },
        ];
        picker.update_results(results);

        assert_eq!(picker.selected().map(|r| r.path.as_str()), Some("a.rs"));
        picker.down();
        assert_eq!(picker.selected().map(|r| r.path.as_str()), Some("b.rs"));
        picker.down();
        assert_eq!(
            picker.selected().map(|r| r.file_type),
            Some(FileType::Folder)
        );
        picker.up();
        assert_eq!(picker.selected().map(|r| r.path.as_str()), Some("b.rs"));
        picker.home();
        assert_eq!(picker.selected().map(|r| r.path.as_str()), Some("a.rs"));
        picker.end();
        assert_eq!(picker.selected().map(|r| r.path.as_str()), Some("c.rs"));
    }

    #[test]
    fn test_picker_page_up_down() {
        let mut picker = FilePicker::new(24, 80);
        let results: Vec<_> = (0..20)
            .map(|i| FileSearchResult {
                path: format!("file{}.rs", i),
                file_type: FileType::File,
                label: format!("file{}.rs", i),
            })
            .collect();
        picker.update_results(results);

        picker.end();
        assert_eq!(picker.selected_index, 19);
        picker.page_up();
        assert_eq!(picker.selected_index, 11);
        picker.home();
        assert_eq!(picker.selected_index, 0);
        picker.page_down();
        assert_eq!(picker.selected_index, 8);
    }

    #[test]
    fn test_picker_scroll_offset() {
        let mut picker = FilePicker::new(24, 80);
        let results: Vec<_> = (0..20)
            .map(|i| FileSearchResult {
                path: format!("file{}.rs", i),
                file_type: FileType::File,
                label: format!("file{}.rs", i),
            })
            .collect();
        picker.update_results(results);

        // Initially at top, scroll_offset should be 0
        assert_eq!(picker.scroll_offset, 0);
        assert_eq!(picker.selected_index, 0);

        // Navigate down to item 7 (still visible in first page)
        for _ in 0..7 {
            picker.down();
        }
        assert_eq!(picker.selected_index, 7);
        assert_eq!(picker.scroll_offset, 0);

        // Navigate down to item 8 (should trigger scroll)
        picker.down();
        assert_eq!(picker.selected_index, 8);
        assert_eq!(picker.scroll_offset, 1);

        // Navigate down to item 15
        for _ in 0..7 {
            picker.down();
        }
        assert_eq!(picker.selected_index, 15);
        assert_eq!(picker.scroll_offset, 8);

        // Navigate up within visible range - scroll_offset stays same
        picker.up();
        assert_eq!(picker.selected_index, 14);
        assert_eq!(picker.scroll_offset, 8);

        // Navigate up to scroll_offset boundary
        for _ in 0..6 {
            picker.up();
        }
        assert_eq!(picker.selected_index, 8);
        assert_eq!(picker.scroll_offset, 8);

        // Navigate up past boundary - should scroll back
        picker.up();
        assert_eq!(picker.selected_index, 7);
        assert_eq!(picker.scroll_offset, 7);
    }
}
