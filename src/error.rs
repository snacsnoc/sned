//! sned CLI error types with exit code support.
//!
//! This module provides error types that carry exit code information,
//! enabling proper shell integration and CI/CD pipelines.

use thiserror::Error;

/// Sned CLI error with exit code information
#[derive(Error, Debug)]
pub enum CliError {
    /// General error (API failure, unexpected error)
    #[error("{0}")]
    General(String),

    /// Configuration error (missing API key, invalid config)
    #[error("{0}")]
    Config(String),

    /// Input error (invalid prompt, bad flag)
    #[error("{0}")]
    Input(String),

    /// Tool error (edit_file failure, command execution failure)
    #[error("{0}")]
    Tool(String),

    /// Signal/interrupted
    #[error("interrupted")]
    Interrupted,

    /// Wrapped anyhow error (defaults to EXIT_ERROR)
    #[error("{0}")]
    Anyhow(#[from] anyhow::Error),
}

impl CliError {
    /// Get the exit code for this error
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::General(_) => crate::exit_codes::EXIT_ERROR,
            Self::Config(_) => crate::exit_codes::EXIT_CONFIG,
            Self::Input(_) => crate::exit_codes::EXIT_INPUT,
            Self::Tool(_) => crate::exit_codes::EXIT_TOOL,
            Self::Interrupted => crate::exit_codes::EXIT_INTERRUPTED,
            Self::Anyhow(_) => crate::exit_codes::EXIT_ERROR,
        }
    }

    /// Create a config error
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    /// Create an input error
    pub fn input(msg: impl Into<String>) -> Self {
        Self::Input(msg.into())
    }

    /// Create a tool error
    pub fn tool(msg: impl Into<String>) -> Self {
        Self::Tool(msg.into())
    }

    /// Create a general error
    pub fn general(msg: impl Into<String>) -> Self {
        Self::General(msg.into())
    }
}

/// Result type alias for CliError
pub type Result<T> = std::result::Result<T, CliError>;
