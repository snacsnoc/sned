//! List files tool handler for sned CLI.
//!
//!
//! Core behavior:
//! - List files in one or more directories
//! - Support recursive listing
//! - Enforce 200 file limit
//! - Return formatted file listing

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
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
}

/// List files tool handler.
#[derive(Debug, Clone, Default)]
pub struct ListFilesHandler;

impl ListFilesHandler {
    pub fn new() -> Self {
        Self
    }

    /// List files in one or more directories.
    ///
    #[allow(dead_code)]
    async fn list_files(&self, paths: Vec<String>, recursive: bool) -> Vec<ListFilesResult> {
        let mut results = Vec::with_capacity(paths.len());

        for path in paths {
            let result = self
                .list_directory_with_line_counts(&path, recursive, true)
                .await;
            results.push(result);
        }

        results
    }

    /// List files in a single directory.
    async fn list_directory_with_line_counts(
        &self,
        path: &str,
        recursive: bool,
        include_line_counts: bool,
    ) -> ListFilesResult {
        let path_obj = Path::new(path);

        // Check if path exists
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
            };
        }

        // Collect files
        let mut files = Vec::new();
        let mut hit_limit = false;

        if recursive {
            self.collect_files_recursive(path_obj, &mut files, &mut hit_limit, include_line_counts)
                .await;
        } else {
            self.collect_files_top_level(path_obj, &mut files, &mut hit_limit, include_line_counts)
                .await;
        }

        ListFilesResult {
            path: path.to_string(),
            files,
            hit_limit,
            success: true,
            error: None,
        }
    }

    async fn collect_files_top_level(
        &self,
        dir: &Path,
        files: &mut Vec<FileInfo>,
        hit_limit: &mut bool,
        include_line_counts: bool,
    ) {
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(_) => return,
        };

        // Collect file paths first, respecting limit and filters
        let mut file_paths: Vec<(usize, std::path::PathBuf, bool)> = Vec::new();
        let mut dir_entries: Vec<FileInfo> = Vec::new();
        let mut file_index = 0;

        while let Ok(Some(entry)) = entries.next_entry().await {
            if file_paths.len() + dir_entries.len() >= MAX_FILES_LIMIT {
                *hit_limit = true;
                break;
            }

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
    }

    async fn collect_files_recursive(
        &self,
        dir: &Path,
        files: &mut Vec<FileInfo>,
        hit_limit: &mut bool,
        include_line_counts: bool,
    ) {
        let walker = walkdir::WalkDir::new(dir).follow_links(false).into_iter();

        // Collect file paths first, respecting limit and filters
        let mut file_paths: Vec<(usize, std::path::PathBuf)> = Vec::new();
        let mut dir_entries: Vec<FileInfo> = Vec::new();
        let mut file_index = 0;

        for entry in walker {
            if file_paths.len() + dir_entries.len() >= MAX_FILES_LIMIT {
                *hit_limit = true;
                break;
            }

            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
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
                continue; // Skip hidden directories
            }
            // For hidden files, still include them if not in a hidden dir

            // Skip common ignored directories
            if entry.file_type().is_dir() && should_ignore_directory(path) {
                continue;
            }

            // Skip common ignored files
            if entry.file_type().is_file() && should_ignore_file(path) {
                continue;
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
    }

    /// Format files list as a string.
    ///
    fn format_files_list(&self, files: &[FileInfo], hit_limit: bool) -> String {
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
            Ok(self.format_files_list(&result.files, result.hit_limit))
        } else {
            Err(ToolError::ExecutionFailed(
                result.error.unwrap_or_else(|| "Unknown error".to_string()),
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

        let formatted = handler.format_files_list(&files, false);
        assert!(formatted.contains("📁 dir"));
        assert!(formatted.contains("📄 file.txt"));
        assert!(formatted.contains("(42 lines)"));
    }

    #[test]
    fn test_format_files_list_empty() {
        let handler = ListFilesHandler::new();
        let formatted = handler.format_files_list(&[], false);
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
        fs::write(temp_dir.path().join("file5.txt"), "line1\nline2\nline3\nline4\nline5\n").unwrap();

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

        assert_eq!(file_counts.remove(&format!("{}/file1.txt", temp_dir.path().display())), Some(Some(3)));
        assert_eq!(file_counts.remove(&format!("{}/file2.txt", temp_dir.path().display())), Some(Some(2)));
        assert_eq!(file_counts.remove(&format!("{}/file3.txt", temp_dir.path().display())), Some(Some(1)));
        assert_eq!(file_counts.remove(&format!("{}/file4.txt", temp_dir.path().display())), Some(Some(0)));
        assert_eq!(file_counts.remove(&format!("{}/file5.txt", temp_dir.path().display())), Some(Some(5)));
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
        fs::write(temp_dir.path().join("subdir/deeper/deep.txt"), "1\n2\n3\n4\n5\n6\n").unwrap();

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

        assert_eq!(file_counts.remove(&format!("{}/root.txt", temp_dir.path().display())), Some(Some(2)));
        assert_eq!(file_counts.remove(&format!("{}/subdir/nested1.txt", temp_dir.path().display())), Some(Some(4)));
        assert_eq!(file_counts.remove(&format!("{}/subdir/nested2.txt", temp_dir.path().display())), Some(Some(1)));
        assert_eq!(file_counts.remove(&format!("{}/subdir/deeper/deep.txt", temp_dir.path().display())), Some(Some(6)));
        assert!(file_counts.is_empty());
    }
}
