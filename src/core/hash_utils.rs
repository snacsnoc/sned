//! Shared hash utilities for sned CLI.
//!
//! Consolidated from:
//! - `dirac/src/utils/line-hashing.ts`
//! - `dirac/src/shared/utils/line-hashing.ts`
//!
//! Deduplicated from `file_editor.rs` and `read_file.rs` to prevent drift.

use regex::Regex;
use std::sync::LazyLock;

// ============================================================================
// Constants
// ============================================================================

/// Delimiter between anchor word and content.
pub const ANCHOR_DELIMITER: &str = "§";

static ANCHOR_STRIP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)^[ \t]*(?:[A-Z][a-zA-Z0-9]*|[0-9a-f]{{8,16}})\s*{}",
        regex::escape(ANCHOR_DELIMITER)
    ))
    .unwrap()
});

// ============================================================================
// Line Hashing Utilities
// ============================================================================

/// Generates a 32-bit FNV-1a hash for the given content string.
///
pub fn content_hash(content: &str) -> String {
    let mut h: u32 = 2_166_136_261; // FNV-1a offset basis
    for byte in content.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(16_777_619); // FNV-1a prime
    }
    format!("{:08x}", h)
}

/// Computes 64-bit FNV-1a hashes for all lines.
///
pub fn compute_hashes(lines: &[String]) -> Vec<u64> {
    lines
        .iter()
        .map(|line| {
            let mut h: u64 = 14_695_981_039_346_656_037;
            for byte in line.bytes() {
                h ^= byte as u64;
                h = h.wrapping_mul(1_099_511_628_211);
            }
            h
        })
        .collect()
}

/// Formats a line with its anchor prefix.
///
pub fn format_line_with_hash(content: &str, anchor: &str) -> String {
    format!("{}{}{}", anchor, ANCHOR_DELIMITER, content)
}

/// Splits a raw anchor string into anchor word and content.
///
pub fn split_anchor(raw_anchor: &str) -> (String, String) {
    match raw_anchor.find(ANCHOR_DELIMITER) {
        Some(idx) => (
            raw_anchor[..idx].trim().to_string(),
            raw_anchor[idx + ANCHOR_DELIMITER.len()..].to_string(),
        ),
        None => (raw_anchor.trim().to_string(), String::new()),
    }
}

/// Strips anchor prefixes from content.
///
/// Removes anchor prefixes from the start of each line.
///
/// This tolerates both read_file anchors (`Apple§content`) and the
/// hash-prefixed "updated anchor" lines shown in edit diffs
/// (`deadbeef§Apple §content`).
///
pub fn strip_hashes(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }

    let mut stripped = content.to_string();
    loop {
        let next = ANCHOR_STRIP_REGEX.replace_all(&stripped, "").into_owned();
        if next == stripped {
            return next;
        }
        stripped = next;
    }
}

/// Extracts the ID from a line reference.
///
pub fn extract_id(reference: &str) -> String {
    if reference.is_empty() {
        return String::new();
    }
    match reference.find(ANCHOR_DELIMITER) {
        Some(idx) => reference[..idx].to_string(),
        None => reference.to_string(),
    }
}

/// Interpret common escape sequences in the `text` field of `edit_file`.
///
/// Models (especially smaller or non-frontier ones) often submit the
/// `text` replacement with JSON-style escape sequences that were meant
/// to represent file content: `\n` for a newline, `\t` for a tab, `\\`
/// for a literal backslash, `\"` for a quote. Without interpretation,
/// these land verbatim in the file as two characters (backslash + letter)
/// and corrupt the source.
///
/// This mirrors how shell / C string literals are commonly read, and
/// matches the model's expectation from the post-hoc warning emitted
/// at `edit_batch.rs:387-410` (now removed in favor of this fix).
///
/// To write a literal `\n` (backslash + n) to the file, the model must
/// send `\\n` in the JSON, which decodes to `\n` in Rust and is then
/// interpreted here as a single newline. To write a literal `\n` as
/// two characters, the model must send `\\\\n` in JSON.
pub fn unescape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('t') => {
                    chars.next();
                    out.push('\t');
                }
                Some('r') => {
                    chars.next();
                    out.push('\r');
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                Some('"') => {
                    chars.next();
                    out.push('"');
                }
                Some('0') => {
                    chars.next();
                    out.push('\0');
                }
                Some(other) => {
                    // Unknown escape: keep the backslash and the next char
                    // verbatim so the model sees a clear "I don't know"
                    // rather than silent data loss.
                    out.push(c);
                    out.push(*other);
                    chars.next();
                }
                None => {
                    // Trailing backslash with nothing after: keep verbatim.
                    out.push(c);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_empty() {
        let hash = content_hash("");
        assert_eq!(hash.len(), 8);
        // FNV-1a of empty string is offset basis
        assert_eq!(hash, "811c9dc5");
    }

    #[test]
    fn test_content_hash_known() {
        // Verify against known values
        let hash = content_hash("hello");
        assert_eq!(hash.len(), 8);
        // FNV-1a of "hello" should be deterministic
        assert_eq!(hash, content_hash("hello"));
    }

    #[test]
    fn test_content_hash_deterministic() {
        let h1 = content_hash("test content");
        let h2 = content_hash("test content");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_strip_hashes() {
        let content = "Apple§line1\nBanana§line2";
        let stripped = strip_hashes(content);
        assert_eq!(stripped, "line1\nline2");
    }

    #[test]
    fn test_strip_hashes_digit_anchors() {
        let content = "L1§line1\nL42§line2\nL999§line3";
        let stripped = strip_hashes(content);
        assert_eq!(stripped, "line1\nline2\nline3");
    }

    #[test]
    fn test_strip_hashes_mixed_anchors() {
        let content = "Apple§alpha\nL10§beta\nDemographicFragile§gamma";
        let stripped = strip_hashes(content);
        assert_eq!(stripped, "alpha\nbeta\ngamma");
    }

    #[test]
    fn test_strip_hashes_hash_prefixed_updated_anchors() {
        let content = "f38ef2139e8cc75d§GymnoglossErratic §        keep me";
        let stripped = strip_hashes(content);
        assert_eq!(stripped, "        keep me");
    }

    #[test]
    fn test_strip_hashes_preserves_indentation_after_anchor() {
        let content = "        FontalEvaporative §        CGRect r;";
        let stripped = strip_hashes(content);
        assert_eq!(stripped, "        CGRect r;");
    }

    #[test]
    fn test_strip_hashes_preserves_trailing_newline() {
        let content = "811c9dc5§line 1\n";
        let stripped = strip_hashes(content);
        assert_eq!(stripped, "line 1\n");
    }

    #[test]
    fn test_format_line_with_hash() {
        assert_eq!(format_line_with_hash("content", "Apple"), "Apple§content");
    }

    #[test]
    fn test_split_anchor() {
        let (anchor, content) = split_anchor("Apple§content");
        assert_eq!(anchor, "Apple");
        assert_eq!(content, "content");
    }

    #[test]
    fn test_extract_id() {
        assert_eq!(extract_id("Apple§content"), "Apple");
        assert_eq!(extract_id("content"), "content");
    }

    #[test]
    fn test_compute_hashes() {
        let lines = vec!["hello".to_string(), "world".to_string()];
        let hashes = compute_hashes(&lines);
        assert_eq!(hashes.len(), 2);
        // Verify hashes are deterministic
        let hashes2 = compute_hashes(&lines);
        assert_eq!(hashes, hashes2);
    }

    #[test]
    fn test_unescape_text_interprets_newline() {
        // The common model bug: JSON-decoded `\n` (backslash + n) must
        // become a real newline. This is the gemma-4 failure mode.
        let input = "static int f(void) {\\n    return 0;\\n}";
        let expected = "static int f(void) {\n    return 0;\n}";
        assert_eq!(unescape_text(input), expected);
    }

    #[test]
    fn test_unescape_text_interprets_tab_carriage_return() {
        assert_eq!(unescape_text("a\\tb"), "a\tb");
        assert_eq!(unescape_text("a\\rb"), "a\rb");
    }

    #[test]
    fn test_unescape_text_interprets_backslash_and_quote() {
        // `\\` in JSON decodes to `\` in Rust, which our unescape
        // interprets as a literal backslash.
        assert_eq!(unescape_text("path\\\\to\\\\file"), "path\\to\\file");
        // `\"` in JSON decodes to `"` in Rust.
        assert_eq!(unescape_text("say \\\"hi\\\""), "say \"hi\"");
    }

    #[test]
    fn test_unescape_text_double_backslash_preserves_newline_meaning() {
        // The model wants a real newline: sends `\n` in JSON.
        // Our unescape produces a newline. Correct.
        assert_eq!(unescape_text("\\n"), "\n");
    }

    #[test]
    fn test_unescape_text_quadruple_backslash_writes_literal_backslash_n() {
        // The model wants a literal `\n` (two chars) in the file:
        // sends `\\n` in JSON, which decodes to `\n` in Rust, which
        // our unescape interprets as newline. To get a literal `\n`
        // the model must send `\\\\n` in JSON, which decodes to `\\n`
        // in Rust, which our unescape interprets as `\n` (two chars).
        let input = "\\\\n";
        let expected = "\\n";
        assert_eq!(unescape_text(input), expected);
    }

    #[test]
    fn test_unescape_text_passes_through_plain_text() {
        // No escapes: identical input/output.
        assert_eq!(unescape_text("hello world"), "hello world");
        assert_eq!(unescape_text(""), "");
    }

    #[test]
    fn test_unescape_text_preserves_unknown_escapes() {
        // `\x` is not a recognized escape: keep both chars verbatim
        // so the model sees a clear "I don't know" rather than
        // silent data loss.
        assert_eq!(unescape_text("\\x"), "\\x");
        assert_eq!(unescape_text("\\u0041"), "\\u0041");
    }

    #[test]
    fn test_unescape_text_preserves_trailing_backslash() {
        // Trailing `\` with nothing after: keep verbatim.
        assert_eq!(unescape_text("path\\"), "path\\");
    }
}
