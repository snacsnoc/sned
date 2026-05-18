//! Terminal smoke tests for VT rendering.
//!
//! These tests verify basic VT rendering functionality including:
//! - ANSI color output
//! - Cursor movement
//! - Screen clear operations

use sned::terminal::vt_renderer::VtRenderer;

#[test]
fn test_ansi_color_codes_stripped() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m \x1b[34mblue\x1b[0m");
    let text = renderer.screen_text();
    assert_eq!(text, "red green blue");
}

#[test]
fn test_cursor_up_down() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"line1\nline2\nline3\x1b[2A");
    let text = renderer.screen_text();
    assert!(text.contains("line1"));
    assert!(text.contains("line2"));
    assert!(text.contains("line3"));
}

#[test]
fn test_cursor_forward_backward() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"hello\x1b[2DXY");
    let text = renderer.screen_text();
    assert_eq!(text, "helXY");
}

#[test]
fn test_clear_entire_screen() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"old text\x1b[2Jnew text");
    let text = renderer.screen_text();
    assert_eq!(text, "new text");
}

#[test]
fn test_clear_line_from_cursor() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"hello world\x1b[5D\x1b[Kend");
    let text = renderer.screen_text();
    assert_eq!(text, "hello end");
}

#[test]
fn test_carriage_return_overwrite() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"Downloading 50%\rDownloading 100%\nDone");
    let text = renderer.screen_text();
    assert_eq!(text, "Downloading 100%\nDone");
}

#[test]
fn test_newline_cursor_movement() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"line1\nline2\nline3");
    let text = renderer.screen_text();
    assert_eq!(text, "line1\nline2\nline3");
}

#[test]
fn test_cursor_position_absolute() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"\x1b[5;10Htext");
    let text = renderer.screen_text();
    assert!(text.contains("text"));
}

#[test]
fn test_tab_expansion() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"col1\tcol2");
    let text = renderer.screen_text();
    assert!(text.contains("col1"));
    assert!(text.contains("col2"));
}

#[test]
fn test_backspace() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"hello\x08\x08XY");
    let text = renderer.screen_text();
    assert_eq!(text, "helXY");
}

#[test]
fn test_empty_renderer() {
    let renderer = VtRenderer::new(24, 80);
    let text = renderer.screen_text();
    assert_eq!(text, "");
}

#[test]
fn test_scroll_up() {
    let mut renderer = VtRenderer::new(5, 20);
    renderer.parse_bytes(b"line1\nline2\nline3\nline4\nline5\x1b[S");
    let text = renderer.screen_text();
    assert!(!text.contains("line1"));
    assert!(text.contains("line2"));
    assert!(text.contains("line5"));
}

#[test]
fn test_mixed_ansi_sequences() {
    let mut renderer = VtRenderer::new(24, 80);
    renderer.parse_bytes(b"\x1b[1;32mOK\x1b[0m \x1b[34mBuild\x1b[0m \x1b[90mcomplete\x1b[0m");
    let text = renderer.screen_text();
    assert_eq!(text, "OK Build complete");
}
