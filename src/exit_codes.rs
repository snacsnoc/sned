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

/// Success
pub const EXIT_SUCCESS: i32 = 0;

/// General error (API failure, unexpected error)
pub const EXIT_ERROR: i32 = 1;

/// Configuration error (missing API key, invalid config)
pub const EXIT_CONFIG: i32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_constants() {
        assert_eq!(EXIT_SUCCESS, 0);
        assert_eq!(EXIT_ERROR, 1);
        assert_eq!(EXIT_CONFIG, 2);
    }
}
