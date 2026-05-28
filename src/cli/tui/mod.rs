//! TUI module for ratatui-based rendering.
//!
//! This module provides the ratatui render loop and related components.

pub mod ansi_converter;
pub mod app;
pub mod history;
pub mod theme;

pub use ansi_converter::ansi_to_ratatui_lines;
pub use app::{App, format_duration};
pub use history::FileHistory;
