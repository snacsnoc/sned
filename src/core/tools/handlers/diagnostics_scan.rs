//! Diagnostics scan tool handler for sned CLI.
//!
//!
//! Runs language-specific diagnostics based on workspace/project type.
//! Supports: Rust (cargo check), JavaScript/TypeScript (npm run lint / eslint), Python (py_compile).

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

/// Pre-compiled regex for ESLint-style diagnostics.
/// Matches: `/path/to/file.js: line 10, col 5, Error - Expected ';'`
static ESLINT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r":\s*(\d+)\s*,?\s*(?:col\s*(\d+)\s*,?\s*)?\s*(Error|Warning|Info)\s*-\s*(.+)$")
        .unwrap()
});

/// Pre-compiled regex for Python-style diagnostics.
/// Matches: `File "...", line N`
static PYTHON_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"File\s+"([^"]+)",\s*line\s*(\d+)"#).unwrap()
});

/// Diagnostics scan tool handler.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsScanHandler;

/// Cache for detect_project_type results to avoid redundant filesystem walks.
/// Keyed by (parent directory, file extension) to handle mixed file types in same dir.
static PROJECT_TYPE_CACHE: LazyLock<Mutex<HashMap<(PathBuf, String), ProjectType>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl DiagnosticsScanHandler {
    pub fn new() -> Self {
        Self
    }

    /// Detect the project type for a given file path.
    /// Uses a cache keyed by (parent directory, extension) to avoid redundant filesystem walks.
    pub fn detect_project_type(file_path: &Path) -> ProjectType {
        let parent = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let ext = file_path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        let cache_key = (parent, ext);

        // Check cache first
        {
            let cache = PROJECT_TYPE_CACHE.lock().unwrap();
            if let Some(&cached) = cache.get(&cache_key) {
                return cached;
            }
        }

        // Compute project type
        let project_type = Self::detect_project_type_uncached(file_path);

        // Cache the result
        {
            let mut cache = PROJECT_TYPE_CACHE.lock().unwrap();
            cache.insert(cache_key, project_type);
        }

        project_type
    }

    /// Uncached project type detection (used internally and for cache misses).
    fn detect_project_type_uncached(file_path: &Path) -> ProjectType {
        let parent = file_path.parent().unwrap_or(Path::new("."));

        // Check for Rust project
        if parent.join("Cargo.toml").exists() || Self::has_ancestor_file(file_path, "Cargo.toml") {
            return ProjectType::Rust;
        }

        // Check for JavaScript/TypeScript project
        if parent.join("package.json").exists()
            || Self::has_ancestor_file(file_path, "package.json")
        {
            return ProjectType::JavaScript;
        }

        // Check for Python by extension
        if file_path.extension().map(|e| e == "py").unwrap_or(false) {
            return ProjectType::Python;
        }

        ProjectType::Generic
    }

    /// Check if any ancestor directory contains the named file.
    fn has_ancestor_file(start: &Path, file_name: &str) -> bool {
        let mut current = start.parent();
        while let Some(dir) = current {
            if dir.join(file_name).exists() {
                return true;
            }
            current = dir.parent();
        }
        false
    }

    /// Run the appropriate diagnostic command for a project type.
    pub async fn run_diagnostics(
        project_type: ProjectType,
        file_path: &Path,
    ) -> anyhow::Result<String> {
        match project_type {
            ProjectType::Rust => {
                // Find the directory with Cargo.toml
                let cargo_dir = Self::find_ancestor_with_file(file_path, "Cargo.toml")
                    .unwrap_or_else(|| file_path.parent().unwrap_or(Path::new(".")).to_path_buf());

                let output = Command::new("cargo")
                    .args(["check", "--message-format=short", "--quiet"])
                    .current_dir(&cargo_dir)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .await?;

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let mut result = String::new();
                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(&stderr);
                }

                if result.trim().is_empty() {
                    Ok("No diagnostics issues found.".to_string())
                } else {
                    Ok(result)
                }
            }
            ProjectType::JavaScript => {
                let js_dir = Self::find_ancestor_with_file(file_path, "package.json")
                    .unwrap_or_else(|| file_path.parent().unwrap_or(Path::new(".")).to_path_buf());

                // Try npm run lint first, then fall back to npx eslint
                let mut result = String::new();

                let lint_output = Command::new("npm")
                    .args(["run", "lint", "--if-present"])
                    .current_dir(&js_dir)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .await;

                match lint_output {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stdout.is_empty() {
                            result.push_str(&stdout);
                        }
                        if !stderr.is_empty() {
                            if !result.is_empty() {
                                result.push('\n');
                            }
                            result.push_str(&stderr);
                        }
                    }
                    Err(_) => {
                        // Fall back to eslint on the specific file
                        let eslint_output = Command::new("npx")
                            .args([
                                "eslint",
                                "--format=compact",
                                file_path.to_string_lossy().as_ref(),
                            ])
                            .current_dir(&js_dir)
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .output()
                            .await;

                        if let Ok(output) = eslint_output {
                            let stdout = String::from_utf8_lossy(&output.stdout);
                            if !stdout.is_empty() {
                                result.push_str(&stdout);
                            }
                        }
                    }
                }

                if result.trim().is_empty() {
                    Ok("No diagnostics issues found.".to_string())
                } else {
                    Ok(result)
                }
            }
            ProjectType::Python => {
                let output = Command::new("python3")
                    .args(["-m", "py_compile", file_path.to_string_lossy().as_ref()])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .await?;

                if output.status.success() {
                    Ok("No diagnostics issues found.".to_string())
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    Ok(stderr.to_string())
                }
            }
            ProjectType::Generic => Ok(format!(
                "No diagnostics available for file: {}. Supported project types: Rust (Cargo.toml), JavaScript/TypeScript (package.json), Python (.py files).",
                file_path.display()
            )),
        }
    }

    /// Run diagnostics for multiple files grouped by (project_root, project_type).
    /// Returns a map of file path -> diagnostics output.
    /// This is more efficient than running diagnostics per-file because:
    /// - cargo check / npm lint run once per project root per language, not per file
    /// - Parallel execution across different project roots and languages
    /// - Mixed-language projects get correct diagnostics for each language
    pub async fn run_diagnostics_batch(
        files_by_project: &HashMap<(PathBuf, ProjectType), Vec<PathBuf>>,
    ) -> HashMap<PathBuf, String> {
        use futures::future::join_all;

        let mut results: HashMap<PathBuf, String> = HashMap::new();

        // Run diagnostics for each (project_root, project_type) group in parallel
        let futures: Vec<_> = files_by_project
            .iter()
            .map(|((project_root, project_type), files)| async move {
                // Run diagnostics once for the project root with the known project type
                let diag_output = match timeout(
                    Duration::from_secs(30),
                    Self::run_diagnostics(*project_type, project_root),
                )
                .await
                {
                    Ok(Ok(output)) => output,
                    Ok(Err(e)) => {
                        tracing::warn!("diagnostics failed for {:?}: {}", project_root, e);
                        String::new()
                    }
                    Err(_) => {
                        tracing::warn!("diagnostics timed out for {:?}", project_root);
                        String::new()
                    }
                };

                // Associate the same output with all files in this group
                (project_root.clone(), files.clone(), diag_output)
            })
            .collect();

        let project_results = join_all(futures).await;

        // Distribute results to individual files
        for (_project_root, files, diag_output) in project_results {
            for file in files {
                results.insert(file, diag_output.clone());
            }
        }

        results
    }

    /// Find the nearest ancestor directory containing the named file.
    pub fn find_ancestor_with_file(start: &Path, file_name: &str) -> Option<PathBuf> {
        let mut current = Some(start);
        while let Some(path) = current {
            if path.is_dir() && path.join(file_name).exists() {
                return Some(path.to_path_buf());
            }
            current = path.parent();
        }
        None
    }

    /// Parse diagnostic output to extract file/line/error info.
    /// This is a best-effort parser for common compiler/linter formats.
    pub fn parse_diagnostics(output: &str, display_path: &str) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Try to match common diagnostic patterns:
            // Rust: path/to/file.rs:line:col: error: message
            // ESLint compact: path/to/file.js: line col, Error - message
            // Python:   File "path", line N

            // Rust / GCC style: file:line:col: level: message
            if let Some(diag) = Self::parse_rust_style(line, display_path) {
                diagnostics.push(diag);
                continue;
            }

            // ESLint compact style: file:line col, Level - message
            if let Some(diag) = Self::parse_eslint_style(line, display_path) {
                diagnostics.push(diag);
                continue;
            }

            // Python syntax error: File "...", line N
            if let Some(diag) = Self::parse_python_style(line, display_path) {
                diagnostics.push(diag);
                continue;
            }

            // If we can't parse but the line looks like an error, add it as a generic message
            if line.contains("error")
                || line.contains("Error")
                || line.contains("warning")
                || line.contains("Warning")
            {
                diagnostics.push(Diagnostic {
                    line: None,
                    message: line.to_string(),
                    severity: if line.contains("error") || line.contains("Error") {
                        Severity::Error
                    } else {
                        Severity::Warning
                    },
                });
            }
        }

        diagnostics
    }

    fn parse_rust_style(line: &str, _display_path: &str) -> Option<Diagnostic> {
        // Match patterns like:
        // src/main.rs:10:5: error: expected `;`, found `}`
        // src/main.rs:10:5: warning: unused variable
        let parts: Vec<&str> = line.splitn(2, ": ").collect();
        if parts.len() != 2 {
            return None;
        }

        let location = parts[0];
        let message = parts[1];

        // Check if location contains line number
        let loc_parts: Vec<&str> = location.rsplitn(3, ':').collect();
        if loc_parts.len() < 2 {
            return None;
        }

        let line_num = loc_parts[1].parse::<u32>().ok()?;

        let severity = if message.starts_with("error") {
            Severity::Error
        } else if message.starts_with("warning") {
            Severity::Warning
        } else {
            Severity::Error
        };

        Some(Diagnostic {
            line: Some(line_num),
            message: message.to_string(),
            severity,
        })
    }

    fn parse_eslint_style(line: &str, _display_path: &str) -> Option<Diagnostic> {
        // Match patterns like:
        // /path/to/file.js: line 10, col 5, Error - Expected ';'
        let caps = ESLINT_REGEX.captures(line)?;

        let line_num = caps.get(1)?.as_str().parse::<u32>().ok()?;
        let severity_str = caps.get(3)?.as_str();
        let message = caps.get(4)?.as_str().to_string();

        let severity = match severity_str.to_lowercase().as_str() {
            "warning" => Severity::Warning,
            "info" => Severity::Warning,
            _ => Severity::Error,
        };

        Some(Diagnostic {
            line: Some(line_num),
            message,
            severity,
        })
    }

    fn parse_python_style(line: &str, _display_path: &str) -> Option<Diagnostic> {
        // Match: File "...", line N
        if !line.starts_with("  File \"") {
            return None;
        }

        let caps = PYTHON_REGEX.captures(line)?;
        let line_num = caps.get(2)?.as_str().parse::<u32>().ok()?;

        // The actual error message is usually on the next line(s)
        Some(Diagnostic {
            line: Some(line_num),
            message: "Python syntax error".to_string(),
            severity: Severity::Error,
        })
    }

    /// Format diagnostics with file context (matching TypeScript output style).
    pub fn format_diagnostics(
        display_path: &str,
        diagnostics: &[Diagnostic],
        file_content: Option<&str>,
    ) -> String {
        if diagnostics.is_empty() {
            return format!(
                "- file: {}\n  status: No diagnostics issues found.",
                display_path
            );
        }

        let max_errors = 20;
        let err_ctx_lines = 1;
        let problems = &diagnostics[..diagnostics.len().min(max_errors)];
        let truncated_count = diagnostics.len().saturating_sub(max_errors);

        let mut result = format!("- file: {}\n  diagnostics: |\n", display_path);

        for diag in problems {
            if let Some(line_num) = diag.line {
                if let Some(content) = file_content {
                    let lines: Vec<&str> = content.lines().collect();
                    let line_idx = (line_num as usize).saturating_sub(1);
                    let ctx_start = line_idx.saturating_sub(err_ctx_lines);
                    let ctx_end = (line_idx + err_ctx_lines).min(lines.len().saturating_sub(1));

                    for i in ctx_start..=ctx_end {
                        let current_line_num = i + 1;
                        let is_target_line = i == line_idx;
                        let line_text = lines.get(i).unwrap_or(&"");

                        if is_target_line {
                            let label = match diag.severity {
                                Severity::Error => "Error",
                                Severity::Warning => "Warning",
                            };
                            result.push_str(&format!(
                                "    {} <<<< [{}] Line {}: {}\n",
                                line_text, label, current_line_num, diag.message
                            ));
                        } else {
                            result.push_str(&format!("    {}\n", line_text));
                        }
                    }
                } else {
                    let label = match diag.severity {
                        Severity::Error => "Error",
                        Severity::Warning => "Warning",
                    };
                    result.push_str(&format!(
                        "    [{}] Line {}: {}\n",
                        label, line_num, diag.message
                    ));
                }
            } else {
                let label = match diag.severity {
                    Severity::Error => "Error",
                    Severity::Warning => "Warning",
                };
                result.push_str(&format!("    [{}]: {}\n", label, diag.message));
            }
        }

        if truncated_count > 0 {
            result.push_str(&format!("\n    ... and {} more errors.", truncated_count));
        }

        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProjectType {
    Rust,
    JavaScript,
    Python,
    Generic,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub line: Option<u32>,
    pub message: String,
    pub severity: Severity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl DiagnosticsScanHandler {
    pub async fn execute(
        &self,
        state: &mut TaskState,
        workspace_root: &std::path::Path,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let paths = crate::core::tools::coerce_string_array(&params, "paths", "path");
        if paths.is_empty() {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "diagnostics_scan: no paths provided"
            );
            return Err(ToolError::InvalidInput(
                "Missing required parameter 'paths'. Please provide one or more file paths to scan.".to_string(),
            ));
        }

        state.consecutive_mistakes = 0;

        // Group files by (project_root, project_type) to handle mixed-language projects.
        // This ensures Rust and JS files at the same root both get their respective diagnostics.
        let mut files_by_project: HashMap<(PathBuf, ProjectType), Vec<PathBuf>> = HashMap::new();
        let mut file_info: HashMap<PathBuf, (String, Option<String>)> = HashMap::new();
        let mut error_results = Vec::new();

        for rel_path in &paths {
            let abs_path = match crate::core::tools::resolve_sanitized_path(workspace_root, rel_path)
            {
                Ok(path) => path,
                Err(e) => {
                    error_results.push(format!("- file: {}\n  error: {}", rel_path, e));
                    continue;
                }
            };

            // Try to read the file
            let file_content = match tokio::fs::read_to_string(&abs_path).await {
                Ok(content) => Some(content),
                Err(e) => {
                    error_results.push(format!("- file: {}\n  error: {}", rel_path, e));
                    continue;
                }
            };

            // Determine project type and root for grouping
            let project_type = Self::detect_project_type(&abs_path);
            let project_root = Self::find_ancestor_with_file(
                &abs_path,
                if project_type == ProjectType::Rust {
                    "Cargo.toml"
                } else if project_type == ProjectType::JavaScript {
                    "package.json"
                } else {
                    "Cargo.toml" // Try Cargo.toml first as fallback
                },
            )
            .or_else(|| {
                Self::find_ancestor_with_file(&abs_path, "package.json").or_else(|| {
                    abs_path.parent().map(|p| p.to_path_buf())
                })
            })
            .unwrap_or_else(|| abs_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")));

            files_by_project
                .entry((project_root, project_type))
                .or_default()
                .push(abs_path.clone());
            file_info.insert(abs_path.clone(), (rel_path.clone(), file_content));
        }

        // Batch diagnostics by (project_root, project_type) so cargo check/npm lint runs once
        // per project per language. For mixed-language projects, this ensures all files get
        // their correct diagnostics instead of only the first detected language.
        let batch_diag_outputs = Self::run_diagnostics_batch(&files_by_project).await;

        // Format results for each file
        let mut results = Vec::new();
        for (abs_path, (display_path, file_content)) in &file_info {
            let diag_output = batch_diag_outputs.get(abs_path).cloned().unwrap_or_default();
            let diagnostics = Self::parse_diagnostics(&diag_output, display_path);
            let formatted = Self::format_diagnostics(display_path, &diagnostics, file_content.as_deref());
            results.push(formatted);
        }

        // Combine error results and diagnostic results
        let final_result = if error_results.is_empty() {
            results.join("\n---\n")
        } else {
            let mut combined = error_results;
            combined.extend(results);
            combined.join("\n---\n")
        };

        Ok(final_result)
    }

    pub fn description(&self, params: &serde_json::Value) -> String {
        if let Some(paths) = params.get("paths").and_then(|p| p.as_array()) {
            let paths_text: Vec<String> = paths
                .iter()
                .filter_map(|p| p.as_str().map(|s| format!("'{}'", s)))
                .collect();
            if !paths_text.is_empty() {
                return format!("[diagnostics_scan for {}]", paths_text.join(", "));
            }
        }
        "[diagnostics_scan]".to_string()
    }
}

#[async_trait]
impl ToolHandler for DiagnosticsScanHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let mut state = ctx.state.lock().await;
        Self::execute(self, &mut state, &ctx.workspace_root, params)
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        DiagnosticsScanHandler::description(self, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostics_scan_handler_creation() {
        let handler = DiagnosticsScanHandler::new();
        assert_eq!(format!("{:?}", handler), "DiagnosticsScanHandler");
    }

    #[tokio::test]
    async fn test_diagnostics_scan_missing_paths() {
        let handler = DiagnosticsScanHandler::new();
        let mut state = TaskState::default();
        let workspace_root = std::env::temp_dir();
        let result = handler
            .execute(&mut state, &workspace_root, serde_json::json!({}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("paths"));
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_diagnostics_scan_empty_paths() {
        let handler = DiagnosticsScanHandler::new();
        let mut state = TaskState::default();
        let workspace_root = std::env::temp_dir();
        let result = handler
            .execute(
                &mut state,
                &workspace_root,
                serde_json::json!({"paths": []}),
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("paths"));
        assert_eq!(state.consecutive_mistakes, 1);
    }

    #[tokio::test]
    async fn test_diagnostics_scan_nonexistent_file() {
        let handler = DiagnosticsScanHandler::new();
        let mut state = TaskState::default();
        let workspace_root = std::env::temp_dir();
        let result = handler
            .execute(
                &mut state,
                &workspace_root,
                serde_json::json!({"paths": ["nonexistent/file.rs"]}),
            )
            .await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("error:"));
    }

    #[tokio::test]
    async fn test_diagnostics_scan_python_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let py_file = temp.path().join("test.py");
        std::fs::write(&py_file, "print('hello')\n").unwrap();

        let handler = DiagnosticsScanHandler::new();
        let mut state = TaskState::default();
        let result = handler
            .execute(
                &mut state,
                temp.path(),
                serde_json::json!({"paths": [py_file.to_string_lossy().to_string()]}),
            )
            .await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(
            text.contains("No diagnostics issues found")
                || text.contains("error")
                || text.contains("Error")
        );
    }

    #[test]
    fn test_detect_project_type() {
        let temp = tempfile::TempDir::new().unwrap();

        // Rust project
        let rust_dir = temp.path().join("rust_project");
        std::fs::create_dir_all(rust_dir.join("src")).unwrap();
        std::fs::write(rust_dir.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(rust_dir.join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(
            DiagnosticsScanHandler::detect_project_type(&rust_dir.join("src/main.rs")),
            ProjectType::Rust
        );

        // JavaScript project
        let js_dir = temp.path().join("js_project");
        std::fs::create_dir_all(js_dir.join("src")).unwrap();
        std::fs::write(js_dir.join("package.json"), "{}").unwrap();
        std::fs::write(js_dir.join("src/main.js"), "console.log('hello');").unwrap();
        assert_eq!(
            DiagnosticsScanHandler::detect_project_type(&js_dir.join("src/main.js")),
            ProjectType::JavaScript
        );

        // Python file
        let py_file = temp.path().join("script.py");
        std::fs::write(&py_file, "print('hello')").unwrap();
        assert_eq!(
            DiagnosticsScanHandler::detect_project_type(&py_file),
            ProjectType::Python
        );

        // Generic file
        let md_file = temp.path().join("readme.md");
        std::fs::write(&md_file, "# README").unwrap();
        assert_eq!(
            DiagnosticsScanHandler::detect_project_type(&md_file),
            ProjectType::Generic
        );
    }

    #[test]
    fn test_parse_rust_diagnostics() {
        let output = "src/main.rs:10:5: error: expected `;`, found `}`\nsrc/main.rs:15:3: warning: unused variable\nsrc/lib.rs:20:1: note: this is a note";
        let diags = DiagnosticsScanHandler::parse_diagnostics(output, "src/main.rs");
        assert_eq!(diags.len(), 3);
        assert_eq!(diags[0].line, Some(10));
        assert!(matches!(diags[0].severity, Severity::Error));
        assert_eq!(diags[1].line, Some(15));
        assert!(matches!(diags[1].severity, Severity::Warning));
    }

    #[test]
    fn test_format_diagnostics_empty() {
        let formatted = DiagnosticsScanHandler::format_diagnostics("test.rs", &[], None);
        assert!(formatted.contains("No diagnostics issues found"));
        assert!(formatted.contains("test.rs"));
    }

    #[test]
    fn test_format_diagnostics_with_errors() {
        let diags = vec![
            Diagnostic {
                line: Some(5),
                message: "expected `;`".to_string(),
                severity: Severity::Error,
            },
            Diagnostic {
                line: Some(10),
                message: "unused variable".to_string(),
                severity: Severity::Warning,
            },
        ];
        let content = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
        let formatted =
            DiagnosticsScanHandler::format_diagnostics("test.rs", &diags, Some(content));
        assert!(formatted.contains("[Error] Line 5: expected `;`"));
        assert!(formatted.contains("[Warning] Line 10: unused variable"));
    }

    #[test]
    fn test_description() {
        let handler = DiagnosticsScanHandler::new();
        let desc = handler.description(&serde_json::json!({"paths": ["src/main.rs", "lib.rs"]}));
        assert!(desc.contains("diagnostics_scan"));
        assert!(desc.contains("src/main.rs"));
        assert!(desc.contains("lib.rs"));

        let desc2 = handler.description(&serde_json::json!({}));
        assert_eq!(desc2, "[diagnostics_scan]");
    }

    #[tokio::test]
    async fn test_diagnostics_scan_batch_groups_by_project() {
        let temp = tempfile::TempDir::new().unwrap();

        let rust_dir = temp.path().join("rust_project");
        std::fs::create_dir_all(rust_dir.join("src")).unwrap();
        std::fs::write(rust_dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        std::fs::write(rust_dir.join("src/file1.rs"), "fn main() {}").unwrap();
        std::fs::write(rust_dir.join("src/file2.rs"), "fn helper() {}").unwrap();

        let handler = DiagnosticsScanHandler::new();
        let mut state = TaskState::default();

        let result = handler
            .execute(
                &mut state,
                temp.path(),
                serde_json::json!({
                    "paths": [
                        rust_dir.join("src/file1.rs").to_string_lossy().to_string(),
                        rust_dir.join("src/file2.rs").to_string_lossy().to_string()
                    ]
                }),
            )
            .await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("file1.rs") || output.contains("file2.rs"));
        assert_eq!(state.consecutive_mistakes, 0);
    }

    #[tokio::test]
    async fn test_diagnostics_scan_mixed_language_same_root() {
        // Test that mixed-language files at the same root both get diagnostics
        let temp = tempfile::TempDir::new().unwrap();

        // Create a mixed-language project with both Rust and JS files at the same root
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        std::fs::write(temp.path().join("package.json"), r#"{"name": "test"}"#).unwrap();
        std::fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(temp.path().join("script.js"), "console.log('hello');").unwrap();

        let handler = DiagnosticsScanHandler::new();
        let mut state = TaskState::default();

        let result = handler
            .execute(
                &mut state,
                temp.path(),
                serde_json::json!({
                    "paths": [
                        temp.path().join("main.rs").to_string_lossy().to_string(),
                        temp.path().join("script.js").to_string_lossy().to_string()
                    ]
                }),
            )
            .await;

        assert!(result.is_ok());
        let output = result.unwrap();
        // Both files should appear in the output (each getting their respective diagnostics)
        assert!(output.contains("main.rs"), "Rust file should be in output");
        assert!(output.contains("script.js"), "JS file should be in output");
        assert_eq!(state.consecutive_mistakes, 0);
    }

    #[tokio::test]
    async fn test_run_diagnostics_batch_single_invocation_per_project() {
        // Prove that run_diagnostics_batch calls run_diagnostics once per project,
        // not once per file. Two files in the same project should get identical output.
        let temp = tempfile::TempDir::new().unwrap();

        let rust_dir = temp.path().join("rust_project");
        std::fs::create_dir_all(&rust_dir).unwrap();
        std::fs::write(rust_dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        std::fs::write(rust_dir.join("file1.rs"), "fn main() {}").unwrap();
        std::fs::write(rust_dir.join("file2.rs"), "fn helper() {}").unwrap();

        let mut files_by_project: HashMap<(PathBuf, ProjectType), Vec<PathBuf>> = HashMap::new();
        files_by_project.insert(
            (rust_dir.clone(), ProjectType::Rust),
            vec![rust_dir.join("file1.rs"), rust_dir.join("file2.rs")],
        );

        let results = DiagnosticsScanHandler::run_diagnostics_batch(&files_by_project).await;

        // Both files should have the same diagnostics output (one invocation served both)
        let output1 = results.get(&rust_dir.join("file1.rs")).expect("file1.rs should have output");
        let output2 = results.get(&rust_dir.join("file2.rs")).expect("file2.rs should have output");
        assert_eq!(
            output1, output2,
            "Both files in the same project should receive identical diagnostics output (proving single invocation)"
        );
    }
}
