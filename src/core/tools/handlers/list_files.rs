//! List files tool handler for sned CLI.
//!
//!
//! Core behavior:
//! - List files in one or more directories
//! - Support recursive listing
//! - Enforce 200 file limit
//! - Return formatted file listing

use crate::core::agent_loop::TaskState;
use crate::core::tools::{
    ToolContext, ToolError, ToolFailureClass, ToolFailureMetadata, ToolHandler,
    resolve_sanitized_path,
};
use async_trait::async_trait;
use futures::future::join_all;
use tokio::io::{AsyncBufReadExt, BufReader};

use std::path::Path;

const MAX_FILES_LIMIT: usize = 200;

/// Information about a listed file or directory.
#[derive(Debug, Clone)]
struct FileInfo {
    path: String,
    is_directory: bool,
    line_count: Option<usize>,
}

/// Result of listing files in a directory.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ListFilesResult {
    path: String,
    files: Vec<FileInfo>,
    hit_limit: bool,
    success: bool,
    error: Option<String>,
    warning: Option<String>,
}

/// List files tool handler.
#[derive(Debug, Clone, Default)]
pub struct ListFilesHandler;

impl ListFilesHandler {
    pub fn new() -> Self {
        Self
    }

    /// List files in a single directory.
    async fn list_directory_with_line_counts(
        &self,
        path: &str,
        recursive: bool,
        include_line_counts: bool,
    ) -> ListFilesResult {
        let path_obj = Path::new(path);

        // Check if path exists (simple exists check is sufficient here since
        // path sanitization is done by resolve_sanitized_path before handler is called)
        if !path_obj.exists() {
            return ListFilesResult {
                path: path.to_string(),
                files: Vec::new(),
                hit_limit: false,
                success: false,
                error: Some(format!(
                    "Error listing files in {}: path does not exist",
                    path
                )),
                warning: None,
            };
        }

        // If path is a file, return info for just that file
        if path_obj.is_file() {
            let line_count = if include_line_counts {
                count_lines_fast(path_obj).await
            } else {
                None
            };

            return ListFilesResult {
                path: path.to_string(),
                files: vec![FileInfo {
                    path: path.to_string(),
                    is_directory: false,
                    line_count,
                }],
                hit_limit: false,
                success: true,
                error: None,
                warning: None,
            };
        }

        // Collect files using original path to preserve format in output
        let mut files = Vec::new();
        let mut hit_limit = false;

        let warning = if recursive {
            self.collect_files_recursive(path_obj, &mut files, &mut hit_limit, include_line_counts)
                .await
        } else {
            self.collect_files_top_level(path_obj, &mut files, &mut hit_limit, include_line_counts)
                .await
                .map(|()| None)
        };

        let warning = match warning {
            Ok(warning) => warning,
            Err(e) => {
                return ListFilesResult {
                    path: path.to_string(),
                    files: Vec::new(),
                    hit_limit: false,
                    success: false,
                    error: Some(format!(
                        "{}\n  Suggestion: Check permissions, or try list_files on the parent directory.",
                        e
                    )),
                    warning: None,
                };
            }
        };

        ListFilesResult {
            path: path.to_string(),
            files,
            hit_limit,
            success: true,
            error: None,
            warning,
        }
    }

    async fn collect_files_top_level(
        &self,
        dir: &Path,
        files: &mut Vec<FileInfo>,
        hit_limit: &mut bool,
        include_line_counts: bool,
    ) -> Result<(), String> {
        let mut entries = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| format!("Error listing files in {}: {}", dir.display(), e))?;

        // Collect file paths first, respecting limit and filters
        let mut file_paths: Vec<(usize, std::path::PathBuf, bool)> = Vec::new();
        let mut dir_entries: Vec<FileInfo> = Vec::new();
        let mut file_index = 0;

        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    return Err(format!("Error reading entry in {}: {}", dir.display(), e));
                }
            };
            let path = entry.path();
            let is_directory = path.is_dir();

            // Skip hidden files/directories in top-level listing
            if let Some(name) = path.file_name()
                && name.to_string_lossy().starts_with(".")
            {
                continue;
            }

            // Skip common ignored directories
            if is_directory && should_ignore_directory(&path) {
                continue;
            }

            // Skip common ignored files
            if !is_directory && should_ignore_file(&path) {
                continue;
            }

            if file_paths.len() + dir_entries.len() >= MAX_FILES_LIMIT {
                *hit_limit = true;
                break;
            }

            if is_directory {
                dir_entries.push(FileInfo {
                    path: path.to_string_lossy().to_string(),
                    is_directory: true,
                    line_count: None,
                });
            } else {
                file_paths.push((file_index, path, include_line_counts));
                file_index += 1;
            }
        }

        // Parallel line counting for files
        let mut file_line_counts: Vec<Option<usize>> = vec![None; file_paths.len()];
        if include_line_counts {
            let count_futures: Vec<_> = file_paths
                .iter()
                .map(|(_, path, _)| count_lines_fast(path))
                .collect();
            file_line_counts = join_all(count_futures).await;
        }

        // Build file infos with line counts in original order
        for ((_, path, _), line_count) in file_paths.into_iter().zip(file_line_counts) {
            files.push(FileInfo {
                path: path.to_string_lossy().to_string(),
                is_directory: false,
                line_count,
            });
        }
        files.extend(dir_entries);

        // Sort: directories first, then files alphabetically
        files.sort_by(|a, b| match (a.is_directory, b.is_directory) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.path.cmp(&b.path),
        });

        Ok(())
    }

    async fn collect_files_recursive(
        &self,
        dir: &Path,
        files: &mut Vec<FileInfo>,
        hit_limit: &mut bool,
        include_line_counts: bool,
    ) -> Result<Option<String>, String> {
        // Wrap blocking walkdir traversal in spawn_blocking to avoid blocking tokio worker
        let walk_result = tokio::task::spawn_blocking({
            let dir = dir.to_path_buf();
            move || {
                let mut walker = walkdir::WalkDir::new(&dir).follow_links(false).into_iter();
                let mut file_paths: Vec<(usize, std::path::PathBuf)> = Vec::new();
                let mut dir_entries: Vec<FileInfo> = Vec::new();
                let mut file_index = 0;
                let mut walk_error: Option<String> = None;
                let mut root_failed = false;

                while let Some(entry) = walker.next() {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            // Only record the first error; keep walking to collect what we can
                            if walk_error.is_none() {
                                walk_error = Some(format!("Error reading directory entry: {}", e));
                                root_failed = file_paths.is_empty() && dir_entries.is_empty();
                            }
                            continue;
                        }
                    };

                    let path = entry.path();

                    // Skip the root directory itself
                    if path == dir {
                        continue;
                    }

                    // Skip hidden files/directories
                    if let Some(name) = path.file_name()
                        && name.to_string_lossy().starts_with(".")
                        && entry.file_type().is_dir()
                    {
                        walker.skip_current_dir();
                        continue;
                    }
                    // For hidden files, still include them if not in a hidden dir

                    // Skip common ignored directories
                    if entry.file_type().is_dir() && should_ignore_directory(path) {
                        walker.skip_current_dir();
                        continue;
                    }

                    // Skip common ignored files
                    if entry.file_type().is_file() && should_ignore_file(path) {
                        continue;
                    }

                    if file_paths.len() + dir_entries.len() >= MAX_FILES_LIMIT {
                        return (file_paths, dir_entries, true, walk_error, root_failed);
                    }

                    if entry.file_type().is_dir() {
                        dir_entries.push(FileInfo {
                            path: path.to_string_lossy().to_string(),
                            is_directory: true,
                            line_count: None,
                        });
                    } else {
                        file_paths.push((file_index, path.to_path_buf()));
                        file_index += 1;
                    }
                }

                (file_paths, dir_entries, false, walk_error, root_failed)
            }
        })
        .await
        .unwrap_or_default();

        let (file_paths, dir_entries, hit_limit_walk, walk_error, root_failed) = walk_result;
        if hit_limit_walk {
            *hit_limit = true;
        }
        if root_failed {
            let err = walk_error.unwrap_or_else(|| {
                format!(
                    "Error listing files in {}: recursive traversal failed at the root",
                    dir.display()
                )
            });
            return Err(err);
        }

        // Parallel line counting for files
        let mut file_line_counts: Vec<Option<usize>> = vec![None; file_paths.len()];
        if include_line_counts {
            let count_futures: Vec<_> = file_paths
                .iter()
                .map(|(_, path)| count_lines_fast(path))
                .collect();
            file_line_counts = join_all(count_futures).await;
        }

        // Build file infos with line counts in original order
        for ((_, path), line_count) in file_paths.into_iter().zip(file_line_counts) {
            files.push(FileInfo {
                path: path.to_string_lossy().to_string(),
                is_directory: false,
                line_count,
            });
        }
        files.extend(dir_entries);

        // Sort: directories first, then files alphabetically
        files.sort_by(|a, b| match (a.is_directory, b.is_directory) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.path.cmp(&b.path),
        });

        Ok(walk_error.map(|err| {
            format!(
                "Warning: Recursive listing skipped one or more entries in {}: {}",
                dir.display(),
                err
            )
        }))
    }

    /// Format files list as a string.
    ///
    fn format_files_list(
        &self,
        files: &[FileInfo],
        hit_limit: bool,
        warning: Option<&str>,
    ) -> String {
        if files.is_empty() {
            return "(empty directory)".to_string();
        }

        let mut lines = Vec::new();

        for file in files {
            let prefix = if file.is_directory { "📁 " } else { "📄 " };
            let line_info = if let Some(count) = file.line_count {
                format!(" ({} lines)", count)
            } else {
                String::new()
            };

            // Get just the filename for display
            let name = Path::new(&file.path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| file.path.clone());

            lines.push(format!("{}{}{}", prefix, name, line_info));
        }

        if hit_limit {
            lines.push(format!(
                "\n(Listing limited to {} files. Use recursive listing or refine your search.)",
                MAX_FILES_LIMIT
            ));
        }

        if let Some(warning) = warning {
            lines.push(format!("\n({warning})"));
        }

        lines.join("\n")
    }

    pub async fn execute(
        &self,
        _state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        self.execute_without_state(Path::new("."), params).await
    }

    async fn execute_without_state(
        &self,
        workspace_root: &Path,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let path = params["path"].as_str().unwrap_or(".");
        let recursive = params["recursive"].as_bool().unwrap_or(false);
        // Default: include line counts for single files, skip for recursive (performance)
        let include_line_counts = params["include_line_counts"]
            .as_bool()
            .unwrap_or(!recursive);

        let sanitized_path = resolve_sanitized_path(workspace_root, path)?;
        let result = self
            .list_directory_with_line_counts(
                &sanitized_path.to_string_lossy(),
                recursive,
                include_line_counts,
            )
            .await;
        if result.success {
            Ok(self.format_files_list(&result.files, result.hit_limit, result.warning.as_deref()))
        } else {
            Err(ToolError::ExecutionFailedWithMetadata(
                result.error.unwrap_or_else(|| "Unknown error".to_string()),
                ToolFailureMetadata {
                    class: ToolFailureClass::RootListingFailed,
                    affected_paths: vec![sanitized_path.to_string_lossy().to_string()],
                    required_next_step: None,
                },
            ))
        }
    }
}

#[async_trait]
impl ToolHandler for ListFilesHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        self.execute_without_state(&ctx.workspace_root, params)
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let path = params["path"].as_str().unwrap_or(".");
        format!("Listing files in {}", path)
    }
}

/// Check if a directory should be ignored.
fn should_ignore_directory(path: &Path) -> bool {
    if let Some(name) = path.file_name() {
        let name = name.to_string_lossy();
        let ignored = [
            "node_modules",
            "__pycache__",
            "dist",
            "target",
            ".git",
            "build",
            "out",
            "vendor",
        ];
        return ignored.contains(&name.as_ref());
    }
    false
}

/// Check if a file should be ignored.
fn should_ignore_file(path: &Path) -> bool {
    if let Some(name) = path.file_name() {
        let name = name.to_string_lossy();
        // Ignore database files (symbol index, etc.)
        if name.ends_with(".db") || name.ends_with(".sqlite") || name.ends_with(".sqlite3") {
            return true;
        }
    }
    false
}

/// Count lines in a file using BufReader (avoids loading entire file into memory).
///
/// Uses `tokio::io::BufReader` + `read_line` loop to count newlines
/// without allocating the full file content.
async fn count_lines_fast(path: &Path) -> Option<usize> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut reader = BufReader::new(file);
    let mut count: usize = 0;
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => break, // EOF
            Ok(_) => count += 1,
            Err(_) => return None,
        }
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_list_files_top_level() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "line1\nline2\n").unwrap();
        fs::write(temp_dir.path().join("file2.txt"), "line1\n").unwrap();
        fs::create_dir(temp_dir.path().join("subdir")).unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), false, true)
            .await;

        assert!(result.success);
        assert_eq!(result.files.len(), 3); // 2 files + 1 directory
        assert!(!result.hit_limit);

        // Check that directories come first
        assert!(result.files[0].is_directory);
        assert!(!result.files[1].is_directory);
        assert!(!result.files[2].is_directory);
    }

    #[tokio::test]
    async fn test_list_files_recursive() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "content").unwrap();
        fs::create_dir(temp_dir.path().join("subdir")).unwrap();
        fs::write(temp_dir.path().join("subdir/file2.txt"), "content").unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), true, true)
            .await;

        assert!(result.success);
        assert_eq!(result.files.len(), 3); // file1.txt, subdir, subdir/file2.txt
    }

    #[tokio::test]
    async fn test_list_files_limit() {
        let temp_dir = TempDir::new().unwrap();
        // Create more than MAX_FILES_LIMIT files
        for i in 0..MAX_FILES_LIMIT + 10 {
            fs::write(temp_dir.path().join(format!("file{}.txt", i)), "content").unwrap();
        }

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), false, true)
            .await;

        assert!(result.success);
        assert_eq!(result.files.len(), MAX_FILES_LIMIT);
        assert!(result.hit_limit);
    }

    #[tokio::test]
    async fn test_list_files_limit_ignores_hidden_and_ignored_entries_top_level() {
        let temp_dir = TempDir::new().unwrap();
        for i in 0..MAX_FILES_LIMIT {
            fs::write(temp_dir.path().join(format!(".hidden{i}.txt")), "content").unwrap();
        }
        fs::create_dir(temp_dir.path().join("node_modules")).unwrap();
        fs::write(
            temp_dir.path().join("node_modules").join("ignored.js"),
            "content",
        )
        .unwrap();
        let visible_path = temp_dir.path().join("visible.txt");
        fs::write(&visible_path, "content").unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), false, false)
            .await;

        assert!(result.success);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, visible_path.to_string_lossy());
        assert!(!result.hit_limit);
    }

    #[tokio::test]
    async fn test_list_files_limit_ignores_hidden_and_ignored_entries_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let hidden_dir = temp_dir.path().join(".hidden-dir");
        fs::create_dir(&hidden_dir).unwrap();
        for i in 0..MAX_FILES_LIMIT {
            fs::write(hidden_dir.join(format!("hidden{i}.txt")), "content").unwrap();
        }
        fs::create_dir(temp_dir.path().join("node_modules")).unwrap();
        fs::write(
            temp_dir.path().join("node_modules").join("ignored.js"),
            "content",
        )
        .unwrap();
        let visible_path = temp_dir.path().join("visible.txt");
        fs::write(&visible_path, "content").unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), true, false)
            .await;

        assert!(result.success);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, visible_path.to_string_lossy());
        assert!(!result.hit_limit);
    }

    #[tokio::test]
    async fn test_list_single_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("single.txt");
        fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), false, true)
            .await;

        assert!(result.success);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].line_count, Some(3));
        assert!(!result.files[0].is_directory);
    }

    #[tokio::test]
    async fn test_list_nonexistent_path() {
        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts("/nonexistent/path", false, true)
            .await;

        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_recursive_root_failure_returns_explicit_error() {
        let temp_dir = TempDir::new().unwrap();
        let blocked_dir = temp_dir.path().join("blocked");
        fs::create_dir(&blocked_dir).unwrap();
        fs::set_permissions(&blocked_dir, fs::Permissions::from_mode(0o000)).unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(blocked_dir.to_str().unwrap(), true, false)
            .await;

        fs::set_permissions(&blocked_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!result.success);
        assert!(result.error.is_some());
        assert!(result.warning.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_recursive_partial_traversal_reports_warning() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("visible.txt"), "content").unwrap();
        let blocked_dir = temp_dir.path().join("blocked");
        fs::create_dir(&blocked_dir).unwrap();
        fs::write(blocked_dir.join("hidden.txt"), "content").unwrap();
        fs::set_permissions(&blocked_dir, fs::Permissions::from_mode(0o000)).unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), true, false)
            .await;

        fs::set_permissions(&blocked_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.success);
        assert_eq!(result.files.len(), 2);
        assert!(result.warning.is_some());
        assert!(result.files.iter().any(|f| f.path.ends_with("visible.txt")));
    }

    #[test]
    fn test_format_files_list() {
        let handler = ListFilesHandler::new();
        let files = vec![
            FileInfo {
                path: "/test/dir".to_string(),
                is_directory: true,
                line_count: None,
            },
            FileInfo {
                path: "/test/file.txt".to_string(),
                is_directory: false,
                line_count: Some(42),
            },
        ];

        let formatted = handler.format_files_list(&files, false, None);
        assert!(formatted.contains("📁 dir"));
        assert!(formatted.contains("📄 file.txt"));
        assert!(formatted.contains("(42 lines)"));
    }

    #[test]
    fn test_format_files_list_empty() {
        let handler = ListFilesHandler::new();
        let formatted = handler.format_files_list(&[], false, None);
        assert_eq!(formatted, "(empty directory)");
    }

    #[test]
    fn test_should_ignore_directory() {
        assert!(should_ignore_directory(Path::new("node_modules")));
        assert!(should_ignore_directory(Path::new(".git")));
        assert!(!should_ignore_directory(Path::new("src")));
    }

    #[test]
    fn test_should_ignore_file() {
        assert!(should_ignore_file(Path::new("data.db")));
        assert!(should_ignore_file(Path::new("build.db")));
        assert!(should_ignore_file(Path::new("index.sqlite")));
        assert!(should_ignore_file(Path::new("cache.sqlite3")));
        assert!(!should_ignore_file(Path::new("file.txt")));
        assert!(!should_ignore_file(Path::new("main.rs")));
    }

    #[tokio::test]
    async fn test_list_filters_db_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "content").unwrap();
        fs::write(temp_dir.path().join("data.db"), "binary").unwrap();
        fs::write(temp_dir.path().join("build.sqlite"), "binary").unwrap();
        fs::create_dir(temp_dir.path().join("subdir")).unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), false, true)
            .await;

        assert!(result.success);
        // Should only have 2 items: file1.txt and subdir (not data.db or build.sqlite)
        assert_eq!(result.files.len(), 2);
        assert!(
            result
                .files
                .iter()
                .all(|f| f.path.ends_with("file1.txt") || f.path.ends_with("subdir"))
        );
    }

    #[tokio::test]
    async fn test_parallel_line_counting_top_level() {
        let temp_dir = TempDir::new().unwrap();
        // Create multiple files with known line counts
        fs::write(temp_dir.path().join("file1.txt"), "line1\nline2\nline3\n").unwrap();
        fs::write(temp_dir.path().join("file2.txt"), "line1\nline2\n").unwrap();
        fs::write(temp_dir.path().join("file3.txt"), "single line").unwrap();
        fs::write(temp_dir.path().join("file4.txt"), "").unwrap();
        fs::write(
            temp_dir.path().join("file5.txt"),
            "line1\nline2\nline3\nline4\nline5\n",
        )
        .unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), false, true)
            .await;

        assert!(result.success);
        // 5 files, no directories
        assert_eq!(result.files.len(), 5);

        // Verify line counts are correct (order may vary due to sorting)
        let mut file_counts: std::collections::HashMap<String, Option<usize>> = result
            .files
            .iter()
            .map(|f| (f.path.clone(), f.line_count))
            .collect();

        assert_eq!(
            file_counts.remove(&format!("{}/file1.txt", temp_dir.path().display())),
            Some(Some(3))
        );
        assert_eq!(
            file_counts.remove(&format!("{}/file2.txt", temp_dir.path().display())),
            Some(Some(2))
        );
        assert_eq!(
            file_counts.remove(&format!("{}/file3.txt", temp_dir.path().display())),
            Some(Some(1))
        );
        assert_eq!(
            file_counts.remove(&format!("{}/file4.txt", temp_dir.path().display())),
            Some(Some(0))
        );
        assert_eq!(
            file_counts.remove(&format!("{}/file5.txt", temp_dir.path().display())),
            Some(Some(5))
        );
        assert!(file_counts.is_empty());
    }

    #[tokio::test]
    async fn test_parallel_line_counting_recursive() {
        let temp_dir = TempDir::new().unwrap();
        // Create files in nested structure
        fs::write(temp_dir.path().join("root.txt"), "line1\nline2\n").unwrap();
        fs::create_dir(temp_dir.path().join("subdir")).unwrap();
        fs::write(temp_dir.path().join("subdir/nested1.txt"), "a\nb\nc\nd\n").unwrap();
        fs::write(temp_dir.path().join("subdir/nested2.txt"), "x\n").unwrap();
        fs::create_dir(temp_dir.path().join("subdir/deeper")).unwrap();
        fs::write(
            temp_dir.path().join("subdir/deeper/deep.txt"),
            "1\n2\n3\n4\n5\n6\n",
        )
        .unwrap();

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(temp_dir.path().to_str().unwrap(), true, true)
            .await;

        assert!(result.success);
        // 4 files + 2 directories (subdir, deeper) = 6 items
        assert_eq!(result.files.len(), 6);

        // Verify line counts are correct
        let mut file_counts: std::collections::HashMap<String, Option<usize>> = result
            .files
            .iter()
            .filter(|f| !f.is_directory)
            .map(|f| (f.path.clone(), f.line_count))
            .collect();

        assert_eq!(
            file_counts.remove(&format!("{}/root.txt", temp_dir.path().display())),
            Some(Some(2))
        );
        assert_eq!(
            file_counts.remove(&format!("{}/subdir/nested1.txt", temp_dir.path().display())),
            Some(Some(4))
        );
        assert_eq!(
            file_counts.remove(&format!("{}/subdir/nested2.txt", temp_dir.path().display())),
            Some(Some(1))
        );
        assert_eq!(
            file_counts.remove(&format!(
                "{}/subdir/deeper/deep.txt",
                temp_dir.path().display()
            )),
            Some(Some(6))
        );
        assert!(file_counts.is_empty());
    }

    #[tokio::test]
    async fn test_list_files_permission_denied_returns_error() {
        let temp_dir = TempDir::new().unwrap();
        let locked_dir = temp_dir.path().join("locked");
        fs::create_dir(&locked_dir).unwrap();

        // Remove read permission
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let handler = ListFilesHandler::new();
        let result = handler
            .list_directory_with_line_counts(locked_dir.to_str().unwrap(), false, true)
            .await;

        // Restore permissions so TempDir can clean up
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(
            !result.success,
            "should fail on permission-denied directory"
        );
        assert!(result.error.is_some(), "should have an error message");
        let err = result.error.unwrap();
        assert!(
            err.contains("Permission denied")
                || err.contains("denied")
                || err.contains("Error listing"),
            "error should describe the failure: {}",
            err
        );
        assert!(
            err.contains("Suggestion"),
            "error should include actionable suggestion: {}",
            err
        );
    }
}
