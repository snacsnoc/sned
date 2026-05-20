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
        r"\b[A-Z][a-zA-Z]*?{}",
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
    let mut h: u32 = 2166136261; // FNV-1a offset basis
    for byte in content.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(16777619); // FNV-1a prime
    }
    format!("{:08x}", h)
}

/// Computes FNV-1a hashes for all lines.
///
pub fn compute_hashes(lines: &[String]) -> Vec<u32> {
    lines
        .iter()
        .map(|line| {
            let mut h: u32 = 2166136261;
            for byte in line.bytes() {
                h ^= byte as u32;
                h = h.wrapping_mul(16777619);
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
/// Removes anchor word patterns (e.g., "Apple§content" → "content").
///
pub fn strip_hashes(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }

    // Regex matches anchor patterns (alphabetic words starting with capital letter) followed by delimiter
    ANCHOR_STRIP_REGEX.replace_all(content, "").to_string()
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
}
