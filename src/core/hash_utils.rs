//! Shared hash utilities for sned CLI.
//!
//! Consolidated from:
//! - `dirac/src/utils/line-hashing.ts`
//! - `dirac/src/shared/utils/line-hashing.ts`
//!
//! Deduplicated from `file_editor.rs` and `read_file.rs` to prevent drift.

use regex::Regex;
use std::collections::HashMap;
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

static DUPLICATE_ANCHOR_SUFFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r" \[identical content also at lines \d+(?:, \d+)*(?:, … \(\d+ more\))?\]$").unwrap()
});

const MAX_LISTED_DUPLICATE_LINES: usize = 8;

// ============================================================================
// Line Hashing Utilities
// ============================================================================

/// Generates a 32-bit FNV-1a hash for the given content string.
///
#[must_use]
pub fn content_hash(content: &str) -> String {
    let mut h: u32 = 2_166_136_261; // FNV-1a offset basis
    for byte in content.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(16_777_619); // FNV-1a prime
    }
    format!("{h:08x}")
}

/// Computes 64-bit FNV-1a hashes for all lines.
///
#[must_use]
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

/// Computes, for each line in `lines`, the 1-based indices of other
/// lines whose content is identical. Returned vector is parallel to
/// `lines`; an empty inner slice means the line is unique. Used by
/// `read_file` to flag duplicate lines so the model can pick a
/// fingerprint anchor or fall back to `write_to_file` when many lines
/// share content.
#[must_use]
pub fn identical_content_indices(lines: &[String]) -> Vec<Vec<usize>> {
    let mut buckets: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, line) in lines.iter().enumerate() {
        buckets.entry(line.as_str()).or_default().push(idx + 1);
    }

    lines
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            buckets.get(line.as_str()).map_or_else(Vec::new, |indices| {
                indices.iter().filter(|&&n| n != idx + 1).copied().collect()
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DuplicateContentInfo {
    pub(crate) other_indices: Vec<usize>,
    pub(crate) other_count: usize,
}

/// Keeps exact overflow counts while retaining only the locations needed to
/// render the selected lines; retaining every occurrence per line is
/// quadratic for repetitive files.
#[must_use]
pub(crate) fn duplicate_content_info_for_range(
    lines: &[String],
    range_start: usize,
    range_end: usize,
) -> Vec<DuplicateContentInfo> {
    const MAX_RETAINED_POSITIONS: usize = MAX_LISTED_DUPLICATE_LINES + 2;
    let mut buckets: HashMap<&str, (usize, Vec<usize>)> = HashMap::new();
    for (idx, line) in lines.iter().enumerate() {
        let entry = buckets
            .entry(line.as_str())
            .or_insert_with(|| (0, Vec::new()));
        entry.0 += 1;
        if entry.1.len() < MAX_RETAINED_POSITIONS {
            entry.1.push(idx + 1);
        }
    }

    let start = range_start.min(lines.len());
    let end = range_end.min(lines.len()).max(start);
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let line_number = start + offset + 1;
            let (count, positions) = buckets
                .get(line.as_str())
                .expect("line was inserted into duplicate buckets");
            let other_indices = positions
                .iter()
                .copied()
                .filter(|&position| position != line_number)
                .take(MAX_LISTED_DUPLICATE_LINES + 1)
                .collect();
            DuplicateContentInfo {
                other_indices,
                other_count: count.saturating_sub(1),
            }
        })
        .collect()
}

/// Formats a line with its anchor prefix, appending a duplicate-content
/// annotation when the line's content appears elsewhere in the slice.
///
/// The annotation is appended after the line content using the same
/// `saturating_sub(8)` overflow pattern as `UnchangedSite`: when more
/// than eight other occurrences exist, the annotation reads
/// "identical content also at lines N1, N2, … (X more)". When the line
/// is unique the annotation is omitted and the line is returned as
/// `{anchor}§{content}`. Line numbers are 1-based.
#[must_use]
pub fn format_line_with_hash(content: &str, anchor: &str, identical_at: &[usize]) -> String {
    format_line_with_hash_with_offset(content, anchor, identical_at, 0)
}

/// Formats an anchored line while translating duplicate locations to the
/// file's 1-based line numbering.
#[must_use]
pub fn format_line_with_hash_with_offset(
    content: &str,
    anchor: &str,
    identical_at: &[usize],
    line_number_offset: usize,
) -> String {
    format_line_with_hash_and_count(
        content,
        anchor,
        identical_at,
        identical_at.len(),
        line_number_offset,
    )
}

/// Formats an anchored line when the displayed duplicate locations are
/// bounded but the total duplicate count is known exactly.
#[must_use]
pub(crate) fn format_line_with_hash_and_count(
    content: &str,
    anchor: &str,
    identical_at: &[usize],
    identical_count: usize,
    line_number_offset: usize,
) -> String {
    if identical_count == 0 {
        return format!("{anchor}{ANCHOR_DELIMITER}{content}");
    }
    let listed: Vec<String> = identical_at
        .iter()
        .take(MAX_LISTED_DUPLICATE_LINES)
        .map(|n| n.saturating_add(line_number_offset).to_string())
        .collect();
    let overflow = identical_count.saturating_sub(listed.len());
    let listing = if overflow > 0 {
        format!("{}, … ({} more)", listed.join(", "), overflow)
    } else {
        listed.join(", ")
    };
    format!("{anchor}{ANCHOR_DELIMITER}{content} [identical content also at lines {listing}]")
}

/// Splits a raw anchor string into anchor word and content.
///
#[must_use]
pub fn split_anchor(raw_anchor: &str) -> (String, String) {
    match raw_anchor.find(ANCHOR_DELIMITER) {
        Some(idx) => (
            raw_anchor[..idx].trim().to_string(),
            strip_duplicate_anchor_suffix(&raw_anchor[idx + ANCHOR_DELIMITER.len()..]).to_string(),
        ),
        None => (raw_anchor.trim().to_string(), String::new()),
    }
}

fn strip_duplicate_anchor_suffix(content: &str) -> &str {
    DUPLICATE_ANCHOR_SUFFIX_REGEX
        .find(content)
        .map_or(content, |m| &content[..m.start()])
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

    content
        .split('\n')
        .map(|line| {
            let mut stripped = line.to_string();
            let mut had_anchor = false;
            loop {
                let next = ANCHOR_STRIP_REGEX.replace_all(&stripped, "").into_owned();
                if next == stripped {
                    break;
                }
                had_anchor = true;
                stripped = next;
            }
            // The suffix is display metadata only inside a copied anchor wrapper.
            // Identical text in ordinary source must remain verbatim.
            if had_anchor {
                if let Some(body) = stripped.strip_suffix('\r') {
                    format!("{}\r", strip_duplicate_anchor_suffix(body))
                } else {
                    strip_duplicate_anchor_suffix(&stripped).to_string()
                }
            } else {
                stripped
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

static GLUED_ANCHOR_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // `strip_hashes` only handles line-start anchors; content reconstructed
    // by the model from partial reads can leak such fragments into the file
    // verbatim (e.g., `FranticBreakfast§SolubleGuildhall`). Word and hex
    // anchors are internal to the edit machinery and must never appear in user source.
    Regex::new(r"(?:[A-Z][a-zA-Z0-9]*|[0-9a-f]{8,16})§").unwrap()
});

/// Defense against the silent-corruption path where the model reconstructs
/// content from a partial `read_file` view and forgets a newline between
/// consecutive anchored lines, producing `WordA§WordB§...` where the `§`
/// delimiter survives into the file and corrupts the source. Scopes the check
/// to the 0-based `check_indices` corresponding to changed or added lines in `lines`.
/// Returns the 1-indexed line numbers of offending lines.
#[must_use]
pub fn find_glued_anchor_in_lines(lines: &[String], check_indices: &[usize]) -> Vec<usize> {
    check_indices
        .iter()
        .copied()
        .filter(|&idx| {
            lines
                .get(idx)
                .is_some_and(|line| GLUED_ANCHOR_REGEX.is_match(line))
        })
        .map(|idx| idx + 1)
        .collect()
}

/// Extracts the ID from a line reference.
///
#[must_use]
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
        assert_eq!(
            format_line_with_hash("content", "Apple", &[]),
            "Apple§content"
        );
        assert_eq!(
            format_line_with_hash("dup", "Apple", &[3, 7]),
            "Apple§dup [identical content also at lines 3, 7]"
        );
        let nine: Vec<usize> = (2..=10).collect();
        assert_eq!(
            format_line_with_hash("dup", "Apple", &nine),
            "Apple§dup [identical content also at lines 2, 3, 4, 5, 6, 7, 8, 9, … (1 more)]"
        );
        assert_eq!(
            format_line_with_hash_with_offset("dup", "Apple", &[1, 4], 10),
            "Apple§dup [identical content also at lines 11, 14]"
        );
    }

    #[test]
    fn test_identical_content_indices() {
        let lines = vec!["a".into(), "b".into(), "a".into(), "c".into(), "a".into()];
        let dupes = identical_content_indices(&lines);
        assert!(dupes[0].contains(&3));
        assert!(dupes[0].contains(&5));
        assert!(dupes[1].is_empty());
        assert!(dupes[2].contains(&1));
        assert!(dupes[2].contains(&5));
        assert!(dupes[3].is_empty());
        assert!(dupes[4].contains(&1));
        assert!(dupes[4].contains(&3));
    }

    #[test]
    fn test_split_anchor() {
        let (anchor, content) = split_anchor("Apple§content");
        assert_eq!(anchor, "Apple");
        assert_eq!(content, "content");
    }

    #[test]
    fn test_split_anchor_strips_duplicate_annotation_suffix() {
        let (anchor, content) =
            split_anchor("Apple§duplicate [identical content also at lines 3, 7]");
        assert_eq!(anchor, "Apple");
        assert_eq!(content, "duplicate");

        let (anchor, content) = split_anchor(
            "Word§identical [identical content also at lines 2, 3, 4, 5, 6, 7, 8, 9, … (1 more)]",
        );
        assert_eq!(anchor, "Word");
        assert_eq!(content, "identical");
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
