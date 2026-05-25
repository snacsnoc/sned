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
pub fn file_not_found(path: &str, original_error: &str) -> ActionableError {
    let suggestion = if path.contains("..") {
        "Path contains '..' — check that the relative path is correct from the workspace root."
            .to_string()
    } else if path.starts_with('/') || path.starts_with('~') {
        "Absolute paths outside the workspace are not allowed. Use a relative path from the workspace root.".to_string()
    } else {
        "Check the file path for typos, or use list_files to see available files.".to_string()
    };
    ActionableError::with_suggestion(
        format!("Error reading file: {}", original_error),
        suggestion,
    )
}

/// Add an actionable suggestion to a file-too-large error.
pub fn file_too_large(size_kb: u64, max_kb: u64) -> ActionableError {
    ActionableError::with_suggestion(
        format!(
            "The file size is {}KB, which exceeds the {}KB limit for full file reads.",
            size_kb, max_kb
        ),
        "Use start_line and end_line parameters to read only the relevant section, \
         or use search_files to find specific content within the file.",
    )
}

/// Add an actionable suggestion to a permission-denied error.
pub fn permission_denied(path: &str, operation: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("Permission denied: cannot {} '{}'", operation, path),
        "Check file permissions with `ls -la`. You may need to adjust ownership or use a different location.",
    )
}

/// Add an actionable suggestion to a command-timeout error.
pub fn command_timeout(cmd: &str, timeout_secs: u64) -> ActionableError {
    let suggestion = if timeout_secs <= 30 {
        "This is a short-running command timeout (30s). If the command needs more time, \
         consider running it as a long-running command (e.g., npm install, cargo build) \
         which automatically gets a 5-minute timeout."
            .to_string()
    } else {
        format!(
            "The command exceeded the {}s timeout. Consider: \
             (1) breaking the task into smaller steps, \
             (2) checking for infinite loops or hangs, \
             (3) running the command manually to diagnose the issue.",
            timeout_secs
        )
    };
    ActionableError::with_suggestion(
        format!("Command timed out after {}s: {}", timeout_secs, cmd),
        suggestion,
    )
}

/// Add an actionable suggestion to a command-exit-code error.
pub fn command_exit_code(cmd: &str, exit_code: Option<i32>) -> ActionableError {
    let code = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "unknown".to_string());
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
        format!("Command failed with exit code {}: {}", code, cmd),
        suggestion,
    )
}

/// Add an actionable suggestion to a directory-not-found error.
pub fn directory_not_found(path: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!(
            "Working directory does not exist or is not a directory: {}",
            path
        ),
        "Check the path for typos. Use list_files to see available directories.",
    )
}

/// Add an actionable suggestion to a search-no-results case.
pub fn search_no_results(pattern: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("No matches found for pattern: {}", pattern),
        "Try: (1) simplifying the regex, (2) removing file pattern filters, \
         (3) checking for case sensitivity (regex is case-sensitive by default — \
         try prepending (?i) for case-insensitive search).",
    )
}

/// Add an actionable suggestion to a provider API error.
pub fn provider_error(error_text: &str) -> ActionableError {
    let lower = error_text.to_lowercase();
    let suggestion = if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
    {
        "Check your API key with `sned config` or set the appropriate environment variable \
         (e.g., OPENAI_API_KEY, ANTHROPIC_API_KEY)."
            .to_string()
    } else if lower.contains("429") || lower.contains("rate limit") || lower.contains("quota") {
        "You've hit a rate limit or quota. Wait a moment and retry, or check your \
         provider dashboard for usage limits."
            .to_string()
    } else if lower.contains("403") || lower.contains("forbidden") {
        "Your API key does not have access to this model or endpoint. \
         Check your provider plan and API key permissions."
            .to_string()
    } else if lower.contains("404") || lower.contains("not found") {
        "The model or endpoint was not found. Check the model name with \
         `/models` or verify the provider configuration."
            .to_string()
    } else if lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("internal")
    {
        "The provider is experiencing issues. Wait a moment and retry. \
         If persistent, check the provider status page."
            .to_string()
    } else if lower.contains("connection") || lower.contains("timeout") || lower.contains("network")
    {
        "Network error — check your internet connection and any proxy/VPN settings. \
         If the error persists, the provider endpoint may be temporarily unavailable."
            .to_string()
    } else {
        "Check your provider configuration with `sned config --validate`.".to_string()
    };
    ActionableError::with_suggestion(format!("Provider error: {}", error_text), suggestion)
}

/// Add an actionable suggestion to an edit anchor mismatch error.
pub fn edit_anchor_mismatch(path: &str, anchor: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("Anchor not found in {}: {}", path, anchor),
        "The file may have changed since it was last read. Re-read the file \
         to get updated anchors, then retry the edit with the correct line content.",
    )
}

/// Add an actionable suggestion for an unsupported language error.
pub fn unsupported_language(language: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("Unsupported language: {}", language),
        "Supported languages: python, python3, node, javascript, bash, sh, zsh. \
         For other languages, use execute_command with the appropriate interpreter.",
    )
}

/// Add an actionable suggestion for a regex compilation error.
pub fn invalid_regex(pattern: &str, error: &str) -> ActionableError {
    ActionableError::with_suggestion(
        format!("Invalid regex pattern '{}': {}", pattern, error),
        "Common fixes: escape special characters with \\ (e.g., \\., \\*, \\[, \\( ), \
         or use simpler patterns. Test your regex at regex101.com.",
    )
}

/// Add an actionable suggestion for a git operation error.
pub fn git_operation_failed(operation: &str, error: &str) -> ActionableError {
    let lower = error.to_lowercase();
    let suggestion = if lower.contains("not a git repository") || lower.contains("git repository") {
        "This directory is not a Git repository. Run `git init` to initialize one, \
         or navigate to a Git repository directory."
            .to_string()
    } else if lower.contains("nothing to commit") || lower.contains("working tree clean") {
        "No changes to commit. Use `git status` to see the current state, \
         or make some file changes before committing."
            .to_string()
    } else if lower.contains("permission denied") || lower.contains("could not lock") {
        "Git cannot acquire a lock on the repository. Ensure no other Git process \
         is running, or remove .git/index.lock if it exists."
            .to_string()
    } else if lower.contains("conflict") || lower.contains("merge conflict") {
        "Resolve merge conflicts before committing. Use `git status` to see \
         conflicting files, then edit and `git add` them."
            .to_string()
    } else if lower.contains("certificate") || lower.contains("ssl") || lower.contains("tls") {
        "SSL/certificate error. Check your network connection, or configure \
         Git's SSL settings with `git config http.sslVerify false` for testing."
            .to_string()
    } else {
        "Run `git status` to see the repository state. Check that Git is \
         properly configured and you have the necessary permissions."
            .to_string()
    };
    ActionableError::with_suggestion(format!("Git {} failed: {}", operation, error), suggestion)
}

/// Add an actionable suggestion for a checkpoint operation error.
pub fn checkpoint_operation_failed(operation: &str, error: &str) -> ActionableError {
    let lower = error.to_lowercase();
    let suggestion = if lower.contains("not found") || lower.contains("does not exist") {
        "The checkpoint does not exist. Use `/checkpoint list` to see available checkpoints."
            .to_string()
    } else if lower.contains("corrupt") || lower.contains("invalid") {
        "The checkpoint data may be corrupted. Try listing checkpoints with \
         `/checkpoint list` to see if others are available."
            .to_string()
    } else if lower.contains("permission") || lower.contains("access") {
        "Permission denied accessing checkpoint storage. Check file permissions \
         in the .sned directory."
            .to_string()
    } else {
        "Use `/checkpoint list` to verify available checkpoints. Check the \
         .sned directory for storage issues."
            .to_string()
    };
    ActionableError::with_suggestion(
        format!("Checkpoint {} failed: {}", operation, error),
        suggestion,
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
        let err = provider_error("401 Unauthorized");
        assert!(err.suggestion.as_ref().unwrap().contains("API key"));
    }

    #[test]
    fn test_provider_rate_limit() {
        let err = provider_error("429 Rate limit exceeded");
        assert!(err.suggestion.as_ref().unwrap().contains("rate limit"));
    }

    #[test]
    fn test_provider_network_error() {
        let err = provider_error("Connection timeout");
        assert!(
            err.suggestion
                .as_ref()
                .unwrap()
                .contains("internet connection")
        );
    }

    #[test]
    fn test_edit_anchor_mismatch() {
        let err = edit_anchor_mismatch("src/main.rs", "fn main§hash1234");
        assert!(err.suggestion.as_ref().unwrap().contains("Re-read"));
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

    #[test]
    fn test_git_operation_failed_not_a_repo() {
        let err = git_operation_failed("diff", "fatal: not a git repository");
        assert!(err.suggestion.as_ref().unwrap().contains("git init"));
    }

    #[test]
    fn test_git_operation_failed_nothing_to_commit() {
        let err = git_operation_failed("commit", "nothing to commit, working tree clean");
        assert!(
            err.suggestion
                .as_ref()
                .unwrap()
                .contains("No changes to commit")
        );
    }

    #[test]
    fn test_git_operation_failed_permission_denied() {
        let err = git_operation_failed("push", "Permission denied");
        assert!(err.suggestion.as_ref().unwrap().contains("lock"));
    }

    #[test]
    fn test_git_operation_failed_merge_conflict() {
        let err = git_operation_failed("merge", "merge conflict");
        assert!(
            err.suggestion
                .as_ref()
                .unwrap()
                .contains("Resolve merge conflicts")
        );
    }

    #[test]
    fn test_checkpoint_operation_failed_not_found() {
        let err = checkpoint_operation_failed("restore", "checkpoint not found");
        assert!(
            err.suggestion
                .as_ref()
                .unwrap()
                .contains("/checkpoint list")
        );
    }

    #[test]
    fn test_checkpoint_operation_failed_corrupt() {
        let err = checkpoint_operation_failed("restore", "data is corrupt");
        assert!(err.suggestion.as_ref().unwrap().contains("corrupted"));
    }

    #[test]
    fn test_checkpoint_operation_failed_permission() {
        let err = checkpoint_operation_failed("list", "permission denied");
        assert!(err.suggestion.as_ref().unwrap().contains(".sned directory"));
    }
}
