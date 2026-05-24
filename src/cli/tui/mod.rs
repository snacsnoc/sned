//! TUI module for ratatui-based rendering.
//!
//! This module provides the ratatui render loop and related components.

pub mod app;
pub mod ansi_converter;
pub mod history;

pub use app::App;
pub use ansi_converter::ansi_to_ratatui_lines;
pub use history::FileHistory;
