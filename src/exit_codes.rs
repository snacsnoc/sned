//! Exit codes for sned CLI.
//!
//! These exit codes enable shell integration and CI/CD pipelines to distinguish
//! between different error types.
//!
//! # Exit Code Conventions
//!
//! - 0: Success
//! - 1: General error (API failure, unexpected error)
//! - 2: Configuration error (missing API key, invalid config)
//! - 3: Input error (invalid prompt, bad flag)
//! - 4: Tool error (edit_file failure, command execution failure)
//! - 5: Signal/interrupted

/// Success
pub const EXIT_SUCCESS: i32 = 0;

/// General error (API failure, unexpected error)
pub const EXIT_ERROR: i32 = 1;

/// Configuration error (missing API key, invalid config)
pub const EXIT_CONFIG: i32 = 2;

/// Input error (invalid prompt, bad flag)
pub const EXIT_INPUT: i32 = 3;

/// Tool error (edit_file failure, command execution failure)
pub const EXIT_TOOL: i32 = 4;

/// Signal/interrupted
pub const EXIT_INTERRUPTED: i32 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_constants() {
        assert_eq!(EXIT_SUCCESS, 0);
        assert_eq!(EXIT_ERROR, 1);
        assert_eq!(EXIT_CONFIG, 2);
        assert_eq!(EXIT_INPUT, 3);
        assert_eq!(EXIT_TOOL, 4);
        assert_eq!(EXIT_INTERRUPTED, 5);
    }
}
