//! Structured error guidance for tool handlers.
//!
//! When a tool fails, the model needs actionable advice on how to change strategy.
//! Without guidance, the model retries with the same parameters, creating infinite
//! retry loops until MAX_CONSECUTIVE_MISTAKES terminates the loop.
//!
//! Each function provides escalating guidance based on consecutive failures.

use crate::core::file_editor::EditFailureReason;

/// Guidance for missing or invalid parameters.
#[must_use]
pub fn missing_parameter(param: &str, consecutive_failures: u32) -> String {
    let base =
        format!("Missing or invalid '{param}' parameter. The tool requires this to proceed.");

    match consecutive_failures {
        0 | 1 => format!("{base} Check the tool schema and provide a valid value."),
        2 => format!(
            "{base} This is the second failed attempt. Re-read the tool schema carefully and provide all required parameters."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying this tool with the same parameters. Either find an alternative approach or ask the user for clarification."
        ),
    }
}

/// Guidance for symbol not found errors.
#[must_use]
pub fn symbol_not_found(symbol: &str, path: &str, consecutive_failures: u32) -> String {
    let base = format!("Symbol '{symbol}' not found in {path}.");

    match consecutive_failures {
        0 | 1 => {
            format!("{base} Verify the symbol name and file path are correct. Check for typos.")
        }
        2 => format!(
            "{base} This is the second failed attempt. The symbol genuinely does not exist in this file. Check if the symbol was renamed, moved to another file, or never existed."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying replace/rename on this symbol. Search for the symbol in the codebase or use a different approach."
        ),
    }
}

/// Guidance for overlapping replacements.
#[must_use]
pub fn overlapping_replacements(symbols: &[&str], path: &str, consecutive_failures: u32) -> String {
    let symbol_list = symbols.join("', '");
    let base = format!("Overlapping replacements detected for symbols '{symbol_list}' in {path}.");

    match consecutive_failures {
        0 | 1 => {
            format!("{base} Process symbols one at a time, or ensure replacements do not overlap.")
        }
        2 => format!(
            "{base} This is the second failed attempt. Split the overlapping replacements into separate tool calls."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying overlapping replacements. Process each symbol in a separate tool call."
        ),
    }
}

/// Guidance for empty content in write operations.
#[must_use]
pub fn empty_content(path: &str, consecutive_failures: u32) -> String {
    let base = format!(
        "Failed to write '{path}': the 'content' parameter was empty. This usually means the model ran out of output budget or tried to emit the file in one oversized response."
    );

    match consecutive_failures {
        0 | 1 => format!(
            "{base} Try writing a smaller skeleton first, then use edit_file for the remaining sections."
        ),
        2 => format!(
            "{base} This is the second failed attempt. Switch strategies: write a minimal skeleton first, then fill sections incrementally with edit_file."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying write_to_file for this file and create a skeleton or split the file into smaller pieces before continuing."
        ),
    }
}

/// Recovery guidance for hash-anchored edit failures.
#[must_use]
pub(crate) fn edit_failure(reason: EditFailureReason, consecutive_failures: u32) -> String {
    let base = match reason {
        EditFailureReason::MissingAnchor => {
            "The edit anchor is missing or malformed. Use a complete single-line `Word§line content` anchor copied from the latest read_file output."
        }
        EditFailureReason::UnknownAnchor => {
            "The anchor is unknown or stale. Re-read the target file and copy fresh anchors; do not reuse the failed anchor."
        }
        EditFailureReason::DuplicateContent => {
            "The anchor content is duplicated. For a replace, provide unique anchor and end_anchor values plus the exact interior lines in the content array; use write_to_file for a broad rewrite."
        }
        EditFailureReason::WhitespaceMismatch => {
            "The anchor differs only in whitespace. Copy the line exactly, preserving every leading and trailing space; re-reading is not required for this specific error."
        }
        EditFailureReason::RangeOverlap => {
            "The requested edit ranges overlap. Split them into separate tool calls or make every range non-overlapping."
        }
        EditFailureReason::GluedAnchor => {
            "The assembled replacement joined anchored lines together. Re-read the file and preserve each physical line break; do not paste multiple anchors into one line."
        }
    };

    match consecutive_failures {
        0 | 1 => base.to_string(),
        2 => format!(
            "{base} This is the second failed attempt. Stop retrying the same arguments and follow that recovery step before trying again."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying the same edit arguments; switch to the stated alternative or ask the user for clarification."
        ),
    }
}

/// Recovery guidance for one or more joined edit diagnostics.
#[must_use]
pub(crate) fn edit_failure_for_diagnostic(diagnostic: &str, consecutive_failures: u32) -> String {
    let reasons = EditFailureReason::from_diagnostics(diagnostic);
    if reasons.len() == 1 {
        return edit_failure(reasons[0], consecutive_failures);
    }

    let categories = reasons
        .iter()
        .map(|reason| reason.label())
        .collect::<Vec<_>>()
        .join(", ");
    let base = format!(
        "Multiple edit failure types were detected ({categories}). Correct each individual diagnostic with its matching recovery strategy; do not retry the whole batch unchanged."
    );

    match consecutive_failures {
        0 | 1 => base,
        2 => format!(
            "{base} This is the second failed attempt. Re-read the affected file and split the corrections into focused edits before retrying."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying the same edit arguments; switch to the stated alternatives or ask the user for clarification."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_failure_guidance_escalates_without_changing_strategy() {
        let first = edit_failure(EditFailureReason::DuplicateContent, 0);
        let second = edit_failure(EditFailureReason::DuplicateContent, 2);
        let repeated = edit_failure(EditFailureReason::DuplicateContent, 3);

        assert!(first.contains("content array"));
        assert!(second.contains("second failed attempt"));
        assert!(repeated.contains("Stop retrying the same edit arguments"));
    }

    #[test]
    fn edit_failure_guidance_keeps_non_read_recovery_specific() {
        let whitespace = edit_failure(EditFailureReason::WhitespaceMismatch, 0);
        let overlap = edit_failure(EditFailureReason::RangeOverlap, 0);
        let glued = edit_failure(EditFailureReason::GluedAnchor, 3);

        assert!(whitespace.contains("re-reading is not required"));
        assert!(overlap.contains("Split them into separate tool calls"));
        assert!(glued.contains("physical line break"));
        assert!(glued.contains("Stop retrying the same edit arguments"));
    }

    #[test]
    fn edit_failure_guidance_preserves_mixed_diagnostics() {
        let guidance = edit_failure_for_diagnostic(
            "anchor matches 2 lines with identical content\n\nanchor matches only after trimming whitespace",
            0,
        );

        assert!(guidance.contains("duplicate anchor content"));
        assert!(guidance.contains("whitespace mismatch"));
        assert!(guidance.contains("matching recovery strategy"));
    }
}
