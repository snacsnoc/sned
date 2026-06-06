//! Structured error guidance for tool handlers.
//!
//! When a tool fails, the model needs actionable advice on how to change strategy.
//! Without guidance, the model retries with the same parameters, creating infinite
//! retry loops until MAX_CONSECUTIVE_MISTAKES terminates the loop.
//!
//! Each function provides escalating guidance based on consecutive failures.

/// Guidance for missing or invalid parameters.
pub fn missing_parameter(param: &str, consecutive_failures: u32) -> String {
    let base = format!(
        "Missing or invalid '{}' parameter. The tool requires this to proceed.",
        param
    );

    match consecutive_failures {
        0 | 1 => format!("{} Check the tool schema and provide a valid value.", base),
        2 => format!(
            "{} This is the second failed attempt. Re-read the tool schema carefully and provide all required parameters.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying this tool with the same parameters. Either find an alternative approach or ask the user for clarification.",
            base, consecutive_failures
        ),
    }
}

/// Guidance for file not found errors.
pub fn file_not_found(path: &str, consecutive_failures: u32) -> String {
    let base = format!("File '{}' does not exist.", path);

    match consecutive_failures {
        0 | 1 => format!(
            "{} Verify the path is correct. If the file should exist, check that it hasn't been moved or deleted.",
            base
        ),
        2 => format!(
            "{} This is the second failed attempt. The file genuinely does not exist. Create it first with write_to_file, or use a different file.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying read operations on this file. Create it first, or find an alternative file to work with.",
            base, consecutive_failures
        ),
    }
}

/// Guidance for permission denied errors.
pub fn permission_denied(path: &str, action: &str, consecutive_failures: u32) -> String {
    let base = format!("Permission denied when trying to {} '{}'.", action, path);

    match consecutive_failures {
        0 | 1 => format!(
            "{} Check file permissions and ensure you have read/write access.",
            base
        ),
        2 => format!(
            "{} This is the second failed attempt. You cannot proceed without permissions. Ask the user to adjust permissions or choose a different file.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying. Ask the user for help with permissions or choose a different file.",
            base, consecutive_failures
        ),
    }
}

/// Guidance for symbol not found errors.
pub fn symbol_not_found(symbol: &str, path: &str, consecutive_failures: u32) -> String {
    let base = format!("Symbol '{}' not found in {}.", symbol, path);

    match consecutive_failures {
        0 | 1 => format!(
            "{} Verify the symbol name and file path are correct. Check for typos.",
            base
        ),
        2 => format!(
            "{} This is the second failed attempt. The symbol genuinely does not exist in this file. Check if the symbol was renamed, moved to another file, or never existed.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying replace/rename on this symbol. Search for the symbol in the codebase or use a different approach.",
            base, consecutive_failures
        ),
    }
}

/// Guidance for overlapping replacements.
pub fn overlapping_replacements(symbols: &[&str], path: &str, consecutive_failures: u32) -> String {
    let symbol_list = symbols.join("', '");
    let base = format!(
        "Overlapping replacements detected for symbols '{}' in {}.",
        symbol_list, path
    );

    match consecutive_failures {
        0 | 1 => format!(
            "{} Process symbols one at a time, or ensure replacements do not overlap.",
            base
        ),
        2 => format!(
            "{} This is the second failed attempt. Split the overlapping replacements into separate tool calls.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying overlapping replacements. Process each symbol in a separate tool call.",
            base, consecutive_failures
        ),
    }
}

/// Guidance for empty content in write operations.
pub fn empty_content(path: &str, consecutive_failures: u32) -> String {
    let base = format!(
        "Failed to write '{}': the 'content' parameter was empty. This usually means the model ran out of output budget or tried to emit the file in one oversized response.",
        path
    );

    match consecutive_failures {
        0 | 1 => format!(
            "{} Try writing a smaller skeleton first, then use edit_file for the remaining sections.",
            base
        ),
        2 => format!(
            "{} This is the second failed attempt. Switch strategies: write a minimal skeleton first, then fill sections incrementally with edit_file.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying write_to_file for this file and create a skeleton or split the file into smaller pieces before continuing.",
            base, consecutive_failures
        ),
    }
}

/// Guidance for network/URL errors.
pub fn network_error(url: &str, consecutive_failures: u32) -> String {
    let base = format!(
        "Failed to fetch '{}'. The request may have failed due to network issues, DNS resolution, or the server being unreachable.",
        url
    );

    match consecutive_failures {
        0 | 1 => format!(
            "{} Try again in a moment. If the issue persists, check that the URL is accessible.",
            base
        ),
        2 => format!(
            "{} This is the second failed attempt. The URL may be unreachable or the network may be down. Try a different URL or wait and retry.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying this URL. Try a different source or proceed without this content.",
            base, consecutive_failures
        ),
    }
}

/// Guidance for command execution failures.
pub fn command_failed(cmd: &str, consecutive_failures: u32) -> String {
    let base = format!("Command failed: {}", cmd);

    match consecutive_failures {
        0 | 1 => format!(
            "{} Check the command syntax and ensure all required dependencies are installed.",
            base
        ),
        2 => format!(
            "{} This is the second failed attempt. Verify the command works in the terminal before retrying. Check for missing arguments or dependencies.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying this command. Fix the command syntax or skip this step.",
            base, consecutive_failures
        ),
    }
}

/// Guidance for tool-specific schema errors.
pub fn invalid_tool_input(tool: &str, reason: &str, consecutive_failures: u32) -> String {
    let base = format!("Invalid input for '{}': {}", tool, reason);

    match consecutive_failures {
        0 | 1 => format!("{} Review the tool schema and correct the input.", base),
        2 => format!(
            "{} This is the second failed attempt. Re-read the tool schema carefully and provide valid input.",
            base
        ),
        _ => format!(
            "{} This has failed {} times in a row. Stop retrying this tool. Either fix the input according to the schema or find an alternative approach.",
            base, consecutive_failures
        ),
    }
}
