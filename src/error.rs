//! sned CLI error types with exit code support.
//!
//! This module provides error types that carry exit code information,
//! enabling proper shell integration and CI/CD pipelines.

use thiserror::Error;

#[derive(Error, Debug)]
#[error("{0}")]
pub struct CliError(String);

impl CliError {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        crate::exit_codes::EXIT_CONFIG
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}
