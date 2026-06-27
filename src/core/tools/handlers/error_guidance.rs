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
    let base = format!(
        "Missing or invalid '{param}' parameter. The tool requires this to proceed."
    );

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

/// Guidance for file not found errors.
#[must_use] 
pub fn file_not_found(path: &str, consecutive_failures: u32) -> String {
    let base = format!("File '{path}' does not exist.");

    match consecutive_failures {
        0 | 1 => format!(
            "{base} Verify the path is correct. If the file should exist, check that it hasn't been moved or deleted."
        ),
        2 => format!(
            "{base} This is the second failed attempt. The file genuinely does not exist. Create it first with write_to_file, or use a different file."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying read operations on this file. Create it first, or find an alternative file to work with."
        ),
    }
}

/// Guidance for permission denied errors.
#[must_use] 
pub fn permission_denied(path: &str, action: &str, consecutive_failures: u32) -> String {
    let base = format!("Permission denied when trying to {action} '{path}'.");

    match consecutive_failures {
        0 | 1 => format!(
            "{base} Check file permissions and ensure you have read/write access."
        ),
        2 => format!(
            "{base} This is the second failed attempt. You cannot proceed without permissions. Ask the user to adjust permissions or choose a different file."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying. Ask the user for help with permissions or choose a different file."
        ),
    }
}

/// Guidance for symbol not found errors.
#[must_use] 
pub fn symbol_not_found(symbol: &str, path: &str, consecutive_failures: u32) -> String {
    let base = format!("Symbol '{symbol}' not found in {path}.");

    match consecutive_failures {
        0 | 1 => format!(
            "{base} Verify the symbol name and file path are correct. Check for typos."
        ),
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
    let base = format!(
        "Overlapping replacements detected for symbols '{symbol_list}' in {path}."
    );

    match consecutive_failures {
        0 | 1 => format!(
            "{base} Process symbols one at a time, or ensure replacements do not overlap."
        ),
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

/// Guidance for network/URL errors.
#[must_use] 
pub fn network_error(url: &str, consecutive_failures: u32) -> String {
    let base = format!(
        "Failed to fetch '{url}'. The request may have failed due to network issues, DNS resolution, or the server being unreachable."
    );

    match consecutive_failures {
        0 | 1 => format!(
            "{base} Try again in a moment. If the issue persists, check that the URL is accessible."
        ),
        2 => format!(
            "{base} This is the second failed attempt. The URL may be unreachable or the network may be down. Try a different URL or wait and retry."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying this URL. Try a different source or proceed without this content."
        ),
    }
}

/// Guidance for command execution failures.
#[must_use] 
pub fn command_failed(cmd: &str, consecutive_failures: u32) -> String {
    let base = format!("Command failed: {cmd}");

    match consecutive_failures {
        0 | 1 => format!(
            "{base} Check the command syntax and ensure all required dependencies are installed."
        ),
        2 => format!(
            "{base} This is the second failed attempt. Verify the command works in the terminal before retrying. Check for missing arguments or dependencies."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying this command. Fix the command syntax or skip this step."
        ),
    }
}

/// Guidance for tool-specific schema errors.
#[must_use] 
pub fn invalid_tool_input(tool: &str, reason: &str, consecutive_failures: u32) -> String {
    let base = format!("Invalid input for '{tool}': {reason}");

    match consecutive_failures {
        0 | 1 => format!("{base} Review the tool schema and correct the input."),
        2 => format!(
            "{base} This is the second failed attempt. Re-read the tool schema carefully and provide valid input."
        ),
        _ => format!(
            "{base} This has failed {consecutive_failures} times in a row. Stop retrying this tool. Either fix the input according to the schema or find an alternative approach."
        ),
    }
}
