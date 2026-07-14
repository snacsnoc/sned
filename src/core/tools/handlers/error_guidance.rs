//! Structured error guidance for tool handlers.
//!
//! When a tool fails, the model needs actionable advice on how to change strategy.
//! Without guidance, the model retries with the same parameters, creating infinite
//! retry loops until MAX_CONSECUTIVE_MISTAKES terminates the loop.
//!
//! Each function provides escalating guidance based on consecutive failures.

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
