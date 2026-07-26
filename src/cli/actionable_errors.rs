//! Actionable error suggestions for common failure patterns.
//!
//! Maps known error signatures to human-friendly suggestions that help
//! users fix problems instead of just seeing "Failed to..." messages.

/// A structured error with an optional actionable suggestion.
#[derive(Debug, Clone)]
pub struct ActionableError {
    pub message: String,
    pub suggestion: Option<String>,
}

impl ActionableError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            suggestion: None,
        }
    }

    pub fn with_suggestion(message: impl Into<String>, suggestion: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            suggestion: Some(suggestion.into()),
        }
    }

    #[must_use]
    pub fn display(&self) -> String {
        match &self.suggestion {
            Some(s) => format!("{}\n  Suggestion: {}", self.message, s),
            None => self.message.clone(),
        }
    }
}

impl std::fmt::Display for ActionableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display())
    }
}

/// Add an actionable suggestion to a file-not-found error.
#[must_use]
pub fn file_not_found(path: &str, original_error: &str) -> ActionableError {
    let suggestion = if path.contains("..") {
        "Path contains '..' — check that the relative path is correct from the workspace root."
            .to_string()
    } else if path.starts_with('/') || path.starts_with('~') {
        "Absolute paths outside the workspace are not allowed. Use a relative path from the workspace root.".to_string()
    } else {
        "Check the file path for typos, or use list_files to see available files.".to_string()
    };
    ActionableError::with_suggestion(format!("Error reading file: {original_error}"), suggestion)
}

/// Add an actionable suggestion to a permission-denied error.
#[must_use]
pub fn permission_denied(path: &str, operation: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("Permission denied: cannot {operation} '{path}'"),
        "Check file permissions with `ls -la`. You may need to adjust ownership or use a different location.",
    )
}

/// Add an actionable suggestion to a command-timeout error.
#[must_use]
pub fn command_timeout(cmd: &str, timeout_secs: u64) -> ActionableError {
    let suggestion = if timeout_secs <= 30 {
        "This is a short-running command timeout (30s). If the command needs more time, \
         consider running it as a long-running command (e.g., npm install, cargo build) \
         which automatically gets a 5-minute timeout."
            .to_string()
    } else {
        format!(
            "The command exceeded the {timeout_secs}s timeout. Consider: \
             (1) breaking the task into smaller steps, \
             (2) checking for infinite loops or hangs, \
             (3) running the command manually to diagnose the issue."
        )
    };
    ActionableError::with_suggestion(
        format!("Command timed out after {timeout_secs}s: {cmd}"),
        suggestion,
    )
}

/// Add an actionable suggestion to a command-exit-code error.
#[must_use]
pub fn command_exit_code(cmd: &str, exit_code: Option<i32>) -> ActionableError {
    let code = exit_code.map_or_else(|| "unknown".to_string(), |c| c.to_string());
    let suggestion = match exit_code {
        Some(126) => "Exit code 126 means 'Permission denied' — the command exists but is not executable. Try `chmod +x` on the script.".to_string(),
        Some(127) => "Exit code 127 means 'Command not found' — check the command name and ensure it is installed and on your PATH.".to_string(),
        Some(1) => "This may indicate a general error in the command. Check the output above for error details.".to_string(),
        Some(2) => {
            let base_cmd = cmd
                .split_whitespace()
                .next()
                .unwrap_or("")
                .rsplit('/')
                .next()
                .unwrap_or("");
            let build_commands = [
                "make", "cargo", "cmake", "npm", "pnpm", "yarn",
                "go", "pip", "pip3", "dotnet", "msbuild", "gradle", "mvn",
            ];
            if build_commands.contains(&base_cmd) {
                "Build failed — check the compiler/linter output above for the actual error.".to_string()
            } else {
                "Many tools (grep, diff, clippy) use exit code 2 for usage errors — check the command syntax.".to_string()
            }
        }
        _ => "Check the command output above for error details. You can also run the command manually to debug.".to_string(),
    };
    ActionableError::with_suggestion(
        format!("Command failed with exit code {code}: {cmd}"),
        suggestion,
    )
}

/// Add an actionable suggestion to a directory-not-found error.
#[must_use]
pub fn directory_not_found(path: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("Working directory does not exist or is not a directory: {path}"),
        "Check the path for typos. Use list_files to see available directories.",
    )
}

/// Add an actionable suggestion to a search-no-results case.
#[must_use]
pub fn search_no_results(pattern: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("No matches found for pattern: {pattern}"),
        "Try: (1) simplifying the regex, (2) removing file pattern filters, \
         (3) checking for case sensitivity (regex is case-sensitive by default — \
         try prepending (?i) for case-insensitive search).",
    )
}

/// Add an actionable suggestion to a provider API error.
#[must_use]
pub fn provider_error(error: &crate::providers::ProviderError) -> ActionableError {
    use crate::providers::ProviderError;

    let suggestion = match error {
        ProviderError::AuthenticationError(_) => {
            "Check your API key with `sned auth --provider <name>` or set the appropriate environment variable \
             (e.g., OPENAI_API_KEY, ANTHROPIC_API_KEY)."
                .to_string()
        }
        ProviderError::RateLimitError {
            retry_delay_ms: Some(delay_ms),
            ..
        } => format!(
            "You've hit a rate limit or quota. The provider requested a retry after {} seconds. \
             Wait before retrying, or check your provider dashboard for usage limits.",
            *delay_ms as f64 / 1_000.0
        ),
        ProviderError::RateLimitError {
            retry_delay_ms: None,
            ..
        } => "You've hit a rate limit or quota. Wait a moment and retry, or check your \
              provider dashboard for usage limits."
            .to_string(),
        ProviderError::InvalidRequest(_) => {
            "The provider rejected this request. Check the model name with `/model` or verify \
             the provider configuration."
                .to_string()
        }
        ProviderError::ApiError(_) => {
            "The provider is experiencing issues. Wait a moment and retry. \
             If persistent, check the provider status page."
                .to_string()
        }
        ProviderError::NetworkError(_) => {
            "Network error — check your internet connection and any proxy/VPN settings. \
             If the error persists, the provider endpoint may be temporarily unavailable."
                .to_string()
        }
        ProviderError::UnexpectedError(_) => {
            "Check your provider configuration with `sned config --validate`.".to_string()
        }
    };
    ActionableError::with_suggestion(format!("Provider error: {error}"), suggestion)
}

/// Add an actionable suggestion for an unsupported language error.
#[must_use]
pub fn unsupported_language(language: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("Unsupported language: {language}"),
        "Supported languages: python, python3, node, javascript, bash, sh, zsh. \
         For other languages, use execute_command with the appropriate interpreter.",
    )
}

/// Add an actionable suggestion for a regex compilation error.
#[must_use]
pub fn invalid_regex(pattern: &str, error: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("Invalid regex pattern '{pattern}': {error}"),
        "Common fixes: escape special characters with \\ (e.g., \\., \\*, \\[, \\( ), \
         or use simpler patterns. Test your regex at regex101.com.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_not_found_relative() {
        let err = file_not_found("src/maiin.rs", "No such file");
        assert!(err.suggestion.is_some());
        assert!(err.display().contains("Suggestion:"));
        assert!(err.display().contains("list_files"));
    }

    #[test]
    fn test_file_not_found_traversal() {
        let err = file_not_found("../../../etc/passwd", "No such file");
        assert!(err.suggestion.as_ref().unwrap().contains("'..'"));
    }

    #[test]
    fn test_file_not_found_absolute() {
        let err = file_not_found("/etc/passwd", "No such file");
        assert!(err.suggestion.as_ref().unwrap().contains("Absolute paths"));
    }

    #[test]
    fn test_command_exit_code_127() {
        let err = command_exit_code("badcmd", Some(127));
        assert!(
            err.suggestion
                .as_ref()
                .unwrap()
                .contains("Command not found")
        );
    }

    #[test]
    fn test_command_exit_code_2_build_tool() {
        let err = command_exit_code("cargo build", Some(2));
        assert!(err.suggestion.as_ref().unwrap().contains("Build failed"));
        assert!(
            err.suggestion
                .as_ref()
                .unwrap()
                .contains("compiler/linter output")
        );
    }

    #[test]
    fn test_command_exit_code_2_build_tool_with_path() {
        let err = command_exit_code("/usr/bin/make all", Some(2));
        assert!(err.suggestion.as_ref().unwrap().contains("Build failed"));
    }

    #[test]
    fn test_command_exit_code_2_non_build_tool() {
        let err = command_exit_code("grep pattern file.txt", Some(2));
        assert!(err.suggestion.as_ref().unwrap().contains("usage errors"));
        assert!(err.suggestion.as_ref().unwrap().contains("command syntax"));
    }

    #[test]
    fn test_command_exit_code_126() {
        let err = command_exit_code("./script.sh", Some(126));
        assert!(err.suggestion.as_ref().unwrap().contains("chmod +x"));
    }

    #[test]
    fn test_provider_auth_error() {
        let err = provider_error(&crate::providers::ProviderError::AuthenticationError(
            "token rejected".to_string(),
        ));
        let suggestion = err.suggestion.as_ref().unwrap();
        assert!(suggestion.contains("API key"));
        assert!(suggestion.contains("`sned auth --provider <name>`"));
    }

    #[test]
    fn test_provider_rate_limit() {
        let err = provider_error(&crate::providers::ProviderError::RateLimitError {
            message: "429 Rate limit exceeded".to_string(),
            retry_delay_ms: Some(5_000),
        });
        assert!(err.suggestion.as_ref().unwrap().contains("rate limit"));
        assert!(err.suggestion.as_ref().unwrap().contains("5 seconds"));
    }

    #[test]
    fn test_provider_not_found_error_recommends_model_picker() {
        let err = provider_error(&crate::providers::ProviderError::InvalidRequest(
            "404 Model not found".to_string(),
        ));
        let suggestion = err.suggestion.as_ref().unwrap();
        assert!(suggestion.contains("`/model`"));
        assert!(!suggestion.contains("`/models`"));
    }

    #[test]
    fn test_provider_network_error() {
        let err = provider_error(&crate::providers::ProviderError::NetworkError(
            "Connection timeout".to_string(),
        ));
        assert!(
            err.suggestion
                .as_ref()
                .unwrap()
                .contains("internet connection")
        );
    }

    #[test]
    fn test_no_suggestion() {
        let err = ActionableError::new("Something went wrong");
        assert!(err.suggestion.is_none());
        assert_eq!(err.display(), "Something went wrong");
    }

    #[test]
    fn test_display_trait() {
        let err = ActionableError::with_suggestion("Bad input", "Try again");
        let display = format!("{}", err);
        assert!(display.contains("Suggestion: Try again"));
    }

}
