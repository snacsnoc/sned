//! Context mentions system for sned CLI.
//!
//!
//! Parses `@file`, `@folder`, `@git-changes`, and `@commit-hash`
//! in user prompts and expands them into inline context.

use regex::Regex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

const MAX_FILE_READ_SIZE: u64 = 100 * 1024;

static MENTION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@(/[^\s]*|[a-f0-9]{7,40}|git-changes)").unwrap());

static COMMIT_HASH_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-f0-9]{7,40}$").unwrap());

/// Regex for matching mentions in text.
/// Matches: @/path/to/file, @folder/, @git-changes, @commit-hash
pub fn get_mention_regex() -> &'static Regex {
    &MENTION_REGEX
}

/// Parsed mention with its type.
#[derive(Debug, Clone, PartialEq)]
pub enum Mention {
    File(String),
    Folder(String),
    GitChanges,
    Commit(String),
}

impl Mention {
    /// Parse a mention string into a typed Mention.
    pub fn parse(mention: &str) -> Option<Self> {
        if mention == "git-changes" {
            return Some(Mention::GitChanges);
        }

        if COMMIT_HASH_REGEX.is_match(mention) {
            return Some(Mention::Commit(mention.to_string()));
        }

        if mention.starts_with('/') {
            if mention.ends_with('/') {
                return Some(Mention::Folder(mention.to_string()));
            }
            return Some(Mention::File(mention.to_string()));
        }

        None
    }

    fn display_path(path: &str) -> &str {
        path.trim_start_matches('/')
    }

    /// Get a description for the mention.
    pub fn description(&self) -> String {
        match self {
            Mention::File(path) => format!(
                "'{}' (see below for file content)",
                Self::display_path(path)
            ),
            Mention::Folder(path) => format!(
                "'{}' (see below for folder content)",
                Self::display_path(path)
            ),
            Mention::GitChanges => "Working directory changes (see below for details)".to_string(),
            Mention::Commit(hash) => format!("Git commit '{}' (see below for commit info)", hash),
        }
    }
}

/// Expand mentions in text and return expanded content.
pub async fn expand_mentions(text: &str, workspace_root: &Path) -> (String, Vec<String>) {
    let regex = get_mention_regex();
    let mut mentions = HashSet::new();
    let mut expanded = Vec::new();

    // First pass: collect mentions and replace with placeholders
    let parsed_text = regex.replace_all(text, |caps: &regex::Captures| {
        let mention_str = &caps[1];
        mentions.insert(mention_str.to_string());

        if let Some(mention) = Mention::parse(mention_str) {
            mention.description()
        } else {
            caps[0].to_string()
        }
    });

    // Second pass: expand each mention
    for mention_str in mentions {
        let Some(mention) = Mention::parse(&mention_str) else {
            continue;
        };
        if let Ok(content) = expand_mention(&mention, workspace_root).await {
            expanded.push(content);
        }
    }

    (parsed_text.to_string(), expanded)
}

/// Expand a single mention to its content.
async fn expand_mention(mention: &Mention, workspace_root: &Path) -> Result<String, String> {
    match mention {
        Mention::File(path) => expand_file_mention(path, workspace_root).await,
        Mention::Folder(path) => expand_folder_mention(path, workspace_root).await,
        Mention::GitChanges => expand_git_changes_mention(workspace_root).await,
        Mention::Commit(hash) => expand_commit_mention(hash, workspace_root).await,
    }
}

async fn expand_file_mention(path: &str, workspace_root: &Path) -> Result<String, String> {
    let clean_path = path.trim_start_matches('/');
    let full_path = crate::core::tools::resolve_sanitized_path(workspace_root, clean_path)
        .map_err(|e| format!("Invalid path {}: {}", path, e))?;

    let metadata = tokio::fs::metadata(&full_path)
        .await
        .map_err(|e| format!("Failed to stat file {}: {}", path, e))?;

    if metadata.len() > MAX_FILE_READ_SIZE {
        return Err(format!(
            "File {} is too large ({}KB, max {}KB)",
            path,
            metadata.len() / 1024,
            MAX_FILE_READ_SIZE / 1024
        ));
    }

    match tokio::fs::read_to_string(&full_path).await {
        Ok(content) => Ok(format!(
            "<file_mention path=\"{}\">\n{}\n</file_mention>",
            clean_path, content
        )),
        Err(e) => Err(format!("Failed to read file {}: {}", path, e)),
    }
}

async fn expand_folder_mention(path: &str, workspace_root: &Path) -> Result<String, String> {
    let clean_path = path.trim_start_matches('/');
    let full_path = crate::core::tools::resolve_sanitized_path(workspace_root, clean_path)
        .map_err(|e| format!("Invalid path {}: {}", path, e))?;

    match tokio::fs::read_dir(&full_path).await {
        Ok(mut entries) => {
            let mut lines = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    lines.push(format!("{}/", name));
                } else {
                    lines.push(name);
                }
            }

            Ok(format!(
                "<folder_mention path=\"{}\">\n{}\n</folder_mention>",
                clean_path,
                lines.join("\n")
            ))
        }
        Err(e) => Err(format!("Failed to read directory {}: {}", path, e)),
    }
}

async fn expand_git_changes_mention(workspace_root: &Path) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(workspace_root)
        .output()
        .await
        .map_err(|e| format!("Failed to run git status: {}", e))?;

    if !output.status.success() {
        return Err("Not a git repository".to_string());
    }

    let status = String::from_utf8_lossy(&output.stdout);

    // Get diff stats
    let diff_output = tokio::process::Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(workspace_root)
        .output()
        .await
        .map_err(|e| format!("Failed to run git diff: {}", e))?;

    let diff_stats = String::from_utf8_lossy(&diff_output.stdout);

    Ok(format!(
        "**git-changes**:\n\nStatus:\n{}\nDiff stats:\n{}",
        status, diff_stats
    ))
}

async fn expand_commit_mention(hash: &str, workspace_root: &Path) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .args(["show", "--stat", "--oneline", hash])
        .current_dir(workspace_root)
        .output()
        .await
        .map_err(|e| format!("Failed to run git show: {}", e))?;

    if !output.status.success() {
        return Err(format!("Invalid commit hash: {}", hash));
    }

    let info = String::from_utf8_lossy(&output.stdout);

    Ok(format!("**commit**:\n\n{}", info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mention_regex() {
        let regex = get_mention_regex();

        assert!(regex.is_match("Check @/src/main.rs for details"));
        assert!(regex.is_match("Review @git-changes"));
        assert!(regex.is_match("Commit @abc1234"));
    }

    #[test]
    fn test_parse_file_mention() {
        assert_eq!(
            Mention::parse("/src/main.rs"),
            Some(Mention::File("/src/main.rs".to_string()))
        );
    }

    #[test]
    fn test_parse_folder_mention() {
        assert_eq!(
            Mention::parse("/src/"),
            Some(Mention::Folder("/src/".to_string()))
        );
    }

    #[test]
    fn test_parse_commit_mention() {
        assert_eq!(
            Mention::parse("abc1234"),
            Some(Mention::Commit("abc1234".to_string()))
        );
    }

    #[test]
    fn test_mention_description() {
        assert_eq!(
            Mention::File("/test.rs".to_string()).description(),
            "'test.rs' (see below for file content)"
        );
        assert_eq!(
            Mention::GitChanges.description(),
            "Working directory changes (see below for details)"
        );
    }

    #[tokio::test]
    async fn test_expand_mentions_file() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_mention.txt");
        tokio::fs::write(&test_file, "Hello from mention")
            .await
            .unwrap();

        let text = "Check @/test_mention.txt for info";
        let (parsed, expanded) = expand_mentions(text, &temp_dir).await;

        assert!(parsed.contains("see below for file content"));
        assert!(parsed.contains("'test_mention.txt'"));
        assert!(!parsed.contains("'/test_mention.txt'"));
        assert_eq!(expanded.len(), 1);
        assert!(expanded[0].contains("Hello from mention"));

        tokio::fs::remove_file(&test_file).await.unwrap();
    }

    #[tokio::test]
    async fn test_expand_mentions_no_mentions() {
        let text = "No mentions here";
        let temp_dir = std::env::temp_dir();
        let (parsed, expanded) = expand_mentions(text, &temp_dir).await;

        assert_eq!(parsed, text);
        assert!(expanded.is_empty());
    }

    #[tokio::test]
    async fn test_expand_mentions_path_traversal_blocked() {
        let temp_dir = std::env::temp_dir();
        let text = "Check @/../etc/passwd for info";
        let (_parsed, expanded) = expand_mentions(text, &temp_dir).await;

        // Path traversal should be blocked, so no expansion
        assert!(expanded.is_empty());
    }
}
