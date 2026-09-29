//! sned CLI error types with exit code support.
//!
//! This module provides error types that carry exit code information,
//! enabling proper shell integration and CI/CD pipelines.

use thiserror::Error;

/// Sned CLI error with exit code information
#[derive(Error, Debug)]
pub enum CliError {
    /// Configuration error (missing API key, invalid config)
    #[error("{0}")]
    Config(String),
}

impl CliError {
    /// Get the exit code for this error
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) => crate::exit_codes::EXIT_CONFIG,
        }
    }

    /// Create a config error
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }
}
