use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use tokio::sync::RwLock;

/// Cache entry with timestamp for TTL-based invalidation
#[derive(Debug, Clone)]
struct FileSearchCacheEntry {
    results: Vec<FileSearchResult>,
    timestamp: std::time::Instant,
}

/// Global cache for workspace file listings, keyed by workspace path
/// TTL: 5 seconds - balances freshness vs performance for @-mention autocomplete
static FILE_SEARCH_CACHE: LazyLock<Arc<RwLock<HashMap<String, FileSearchCacheEntry>>>> =
    LazyLock::new(|| Arc::new(RwLock::new(HashMap::with_capacity(4))));

const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// Clear the file search cache - used for testing
#[cfg(test)]
pub async fn clear_file_search_cache() {
    let mut cache = FILE_SEARCH_CACHE.write().await;
    cache.clear();
}

#[derive(Debug, Clone)]
pub struct FileSearchResult {
    pub path: String,
    pub file_type: FileType,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Folder,
}

/// Check if a directory name should be excluded from search.
/// Uses patterns from workspace::DEFAULT_IGNORE_PATTERNS for consistency.
fn is_excluded_dir(name: &str) -> bool {
    // Directory patterns from workspace::DEFAULT_IGNORE_PATTERNS
    // (excluding glob patterns like *.zip)
    matches!(
        name,
        "node_modules"
            | ".git"
            | ".github"
            | "out"
            | "dist"
            | "__pycache__"
            | ".venv"
            | ".env"
            | "venv"
            | "env"
            | ".cache"
            | "tmp"
            | "temp"
            | ".next"
            | "coverage"
            | "build"
            | "target"
            | "bin"
            | "obj"
            | "generated"
            | "gen"
            | ".gradle"
            | ".turbo"
            | ".nuxt"
            | ".svelte-kit"
            | ".idea"
            | ".vs"
            | ".vscode"
            | ".tox"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".ruff_cache"
            | ".yarn"
            | "jspm_packages"
            | "bower_components"
            | "vendor"
            | ".svn"
            | ".hg"
            | ".fslckout"
            | "_fslckout"
            | ".bzr"
            | "_darcs"
            | ".fossil-settings"
    )
}

pub fn check_ripgrep() -> bool {
    std::process::Command::new("which")
        .arg("rg")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn add_parent_dirs(relative_path: &str, dir_set: &mut std::collections::HashSet<String>) {
    let mut dir = Path::new(relative_path);
    while let Some(parent) = dir.parent() {
        let parent_str = parent.to_string_lossy().to_string();
        if parent_str.is_empty() || parent_str == "." || parent_str == "/" {
            break;
        }
        dir_set.insert(parent_str);
        dir = parent;
    }
}

fn dirs_to_results(dir_set: &std::collections::HashSet<String>) -> Vec<FileSearchResult> {
    dir_set
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| {
            let label = Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.clone());
            FileSearchResult {
                path: p.clone(),
                file_type: FileType::Folder,
                label,
            }
        })
        .collect()
}

pub async fn list_workspace_files(
    workspace_path: &str,
    limit: usize,
) -> std::io::Result<Vec<FileSearchResult>> {
    let workspace_path_str = workspace_path.to_string();

    // Check cache first
    if let Some(entry) = FILE_SEARCH_CACHE.read().await.get(&workspace_path_str)
        && entry.timestamp.elapsed() < CACHE_TTL
    {
        // Cache hit - return cached results (filtered by limit)
        return Ok(entry.results.iter().take(limit).cloned().collect());
    }

    // Cache miss or expired - run blocking WalkBuilder iteration
    let workspace = PathBuf::from(workspace_path);
    let results = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let mut dir_set = std::collections::HashSet::new();

        let walker = WalkBuilder::new(&workspace)
            .hidden(false)
            .follow_links(false)
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                if name.starts_with('.') && !name.starts_with(".sned") {
                    return false;
                }
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    return !is_excluded_dir(&name);
                }
                // Filter out database files
                if name.ends_with(".db") || name.ends_with(".sqlite") || name.ends_with(".sqlite3")
                {
                    return false;
                }
                true
            })
            .build();

        for entry in walker.flatten() {
            if files.len() >= limit {
                break;
            }

            let file_type = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                FileType::Folder
            } else {
                FileType::File
            };

            let relative = entry
                .path()
                .strip_prefix(&workspace)
                .map(|p: &std::path::Path| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| entry.path().to_string_lossy().to_string());

            if file_type == FileType::File {
                let label = Path::new(&relative)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| relative.clone());
                files.push(FileSearchResult {
                    path: relative.clone(),
                    file_type: FileType::File,
                    label,
                });
                add_parent_dirs(&relative, &mut dir_set);
            } else {
                dir_set.insert(relative);
            }
        }

        let mut all_results = files;
        all_results.extend(dirs_to_results(&dir_set));
        all_results
    })
    .await
    .map_err(std::io::Error::other)?;

    // Update cache with full results (before limit applied)
    {
        let mut cache = FILE_SEARCH_CACHE.write().await;
        cache.insert(
            workspace_path_str,
            FileSearchCacheEntry {
                results: results.clone(),
                timestamp: std::time::Instant::now(),
            },
        );
    }

    Ok(results.into_iter().take(limit).collect())
}

#[cfg(test)]
fn fuzzy_score(query: &str, target: &str) -> Option<usize> {
    let query_bytes = query.to_lowercase().into_bytes();
    fuzzy_score_normalized(&query_bytes, target)
}

fn fuzzy_score_normalized(query_bytes: &[u8], target: &str) -> Option<usize> {
    let target_bytes = target.to_lowercase().into_bytes();

    if query_bytes.is_empty() {
        return Some(0);
    }

    let mut qi = 0;
    let mut last_match_idx: isize = -1;
    let mut score = 0usize;
    let mut first_match_idx: Option<usize> = None;

    for (ti, tb) in target_bytes.iter().enumerate() {
        if qi < query_bytes.len() && *tb == query_bytes[qi] {
            let ti_usize = ti as isize;

            if qi == 0 {
                first_match_idx = Some(ti);
            }

            // Score: consecutive > word boundary > scattered
            if ti_usize == last_match_idx + 1 {
                // Consecutive match (e.g., "main" in "main.rs")
                score += 5;
            } else if ti == 0
                || (ti > 0 && target_bytes[ti - 1] == b'/' || target_bytes[ti - 1] == b'_')
            {
                // Word boundary (e.g., "AGE" at start of "AGENTS" or after "/")
                score += 4;
            } else {
                // Scattered match
                score += 1;
            }

            last_match_idx = ti_usize;
            qi += 1;
        }
    }

    if qi == query_bytes.len() {
        // Prefix bonus: matches starting at position 0 rank higher
        if first_match_idx == Some(0) {
            score += 10;
        }
        // Length normalization: shorter targets rank higher for same query
        let target_len = target_bytes.len();
        let query_len = query_bytes.len();
        if target_len > 0 {
            score = score * 100 / (target_len - query_len + 100);
        }
        Some(score)
    } else {
        None
    }
}

pub async fn search_workspace_files(
    query: &str,
    workspace_path: &str,
    limit: usize,
) -> Vec<FileSearchResult> {
    let items = match list_workspace_files(workspace_path, 5000).await {
        Ok(items) => items,
        Err(_) => return Vec::new(),
    };

    if query.trim().is_empty() {
        return items.into_iter().take(limit).collect();
    }

    let query_lower = query.to_lowercase();
    let query_bytes = query_lower.into_bytes();
    let mut scored: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let search_target = format!("{} {}", item.label, item.path);
            fuzzy_score_normalized(&query_bytes, &search_target).map(|score| (score, item.clone()))
        })
        .collect();

    scored.sort_by_key(|b| std::cmp::Reverse(b.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, item)| item)
        .collect()
}

pub fn extract_mention_query(text: &str) -> MentionQuery {
    let last_at = match text.rfind('@') {
        Some(i) => i,
        None => {
            return MentionQuery {
                in_mention_mode: false,
                query: String::new(),
                at_index: -1,
            };
        }
    };

    if last_at > 0 {
        let prev = text.as_bytes()[last_at - 1];
        if !char::from_u32(prev as u32)
            .map(|c| c.is_whitespace())
            .unwrap_or(false)
        {
            return MentionQuery {
                in_mention_mode: false,
                query: String::new(),
                at_index: -1,
            };
        }
    }

    let after_at = &text[last_at + 1..];
    if after_at.contains(' ') {
        return MentionQuery {
            in_mention_mode: false,
            query: String::new(),
            at_index: -1,
        };
    }

    MentionQuery {
        in_mention_mode: true,
        query: after_at.to_string(),
        at_index: last_at as isize,
    }
}

#[derive(Debug, Clone)]
pub struct MentionQuery {
    pub in_mention_mode: bool,
    pub query: String,
    pub at_index: isize,
}

pub fn insert_mention(
    text: &str,
    at_index: usize,
    file_path: &str,
    file_type: FileType,
) -> (String, usize) {
    let after_at = text[at_index..].find(' ');
    let end = after_at.map(|i| at_index + i).unwrap_or(text.len());

    let mut normalized = if file_path.starts_with('/') {
        file_path.to_string()
    } else {
        format!("/{}", file_path)
    };

    if matches!(file_type, FileType::Folder) && !normalized.ends_with('/') {
        normalized.push('/');
    }

    let mention = if normalized.contains(' ') {
        format!("@\"{}\"", normalized)
    } else {
        format!("@{}", normalized)
    };

    let cursor_pos = at_index + mention.len() + 1; // +1 for trailing space
    let new_text = format!(
        "{}{} {}",
        &text[..at_index],
        mention,
        text[end..].trim_start()
    );
    (new_text, cursor_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_mention_query_basic() {
        let r = extract_mention_query("hello @FILE");
        assert!(r.in_mention_mode);
        assert_eq!(r.query, "FILE");
        assert_eq!(r.at_index, 6);
    }

    #[test]
    fn test_extract_mention_query_no_space_after() {
        let r = extract_mention_query("hello @FILE foo");
        assert!(!r.in_mention_mode);
    }

    #[test]
    fn test_extract_mention_query_at_start() {
        let r = extract_mention_query("@FILE");
        assert!(r.in_mention_mode);
        assert_eq!(r.query, "FILE");
        assert_eq!(r.at_index, 0);
    }

    #[test]
    fn test_extract_mention_query_no_mention() {
        let r = extract_mention_query("hello world");
        assert!(!r.in_mention_mode);
    }

    #[test]
    fn test_extract_mention_query_not_after_whitespace() {
        let r = extract_mention_query("foo@bar");
        assert!(!r.in_mention_mode);
    }

    #[test]
    fn test_insert_mention_simple() {
        let (result, cursor_pos) =
            insert_mention("hello @", 6, "src/main.rs", FileType::File);
        assert_eq!(result, "hello @/src/main.rs ");
        assert_eq!(cursor_pos, 20); // after "@/src/main.rs "
    }

    #[test]
    fn test_insert_mention_spaces() {
        let (result, cursor_pos) =
            insert_mention("hello @", 6, "/src/my file.rs", FileType::File);
        assert_eq!(result, "hello @\"/src/my file.rs\" ");
        assert_eq!(cursor_pos, 25); // after "@/src/my file.rs "
    }

    #[test]
    fn test_insert_mention_folder_spaces() {
        let (result, cursor_pos) =
            insert_mention("hello @", 6, "/src/my folder", FileType::Folder);
        assert_eq!(result, "hello @\"/src/my folder/\" ");
        assert_eq!(cursor_pos, 25); // after @"/src/my folder/ "
    }

    #[test]
    fn test_fuzzy_score() {
        assert!(fuzzy_score("abc", "abc").is_some());
        assert!(fuzzy_score("abc", "aBC").is_some());
        assert!(fuzzy_score("abc", "axbc").is_some());
        assert!(fuzzy_score("abc", "ab").is_none());
        assert!(fuzzy_score("abc", "xyz").is_none());
    }

    #[test]
    fn test_fuzzy_score_prefix_bonus() {
        let score1 = fuzzy_score("age", "AGENTS.md").unwrap();
        let score2 = fuzzy_score("age", "src/agents.rs").unwrap();
        assert!(
            score1 > score2,
            "AGENTS.md (prefix match) should score higher than src/agents.rs, got {} vs {}",
            score1,
            score2
        );
    }

    #[test]
    fn test_fuzzy_score_shorter_target_bonus() {
        let score1 = fuzzy_score("main", "main.rs").unwrap();
        let score2 = fuzzy_score("main", "src/main.rs").unwrap();
        assert!(
            score1 > score2,
            "main.rs should score higher than src/main.rs, got {} vs {}",
            score1,
            score2
        );
    }

    #[test]
    fn test_fuzzy_score_consecutive_bonus() {
        let score1 = fuzzy_score("abc", "abc.rs").unwrap();
        let score2 = fuzzy_score("abc", "a_b_c.rs").unwrap();
        assert!(
            score1 > score2,
            "Consecutive match should score higher than scattered, got {} vs {}",
            score1,
            score2
        );
    }

    #[test]
    fn test_fuzzy_score_normalized_mixed_case() {
        // Regression: normalized query should match regardless of case
        let query_lower = "main".to_lowercase().into_bytes();
        let score1 = fuzzy_score_normalized(&query_lower, "Main.rs").unwrap();
        let score2 = fuzzy_score_normalized(&query_lower, "MAIN.RS").unwrap();
        let score3 = fuzzy_score_normalized(&query_lower, "main.rs").unwrap();
        assert!(score1 > 0, "should match mixed-case Main.rs");
        assert!(score2 > 0, "should match uppercase MAIN.RS");
        assert_eq!(score1, score3, "mixed-case and lowercase should score equally");
    }

    #[test]
    fn test_add_parent_dirs() {
        let mut dir_set = std::collections::HashSet::new();
        add_parent_dirs("src/main.rs", &mut dir_set);
        assert!(dir_set.contains("src"));
        // Should have at least the immediate parent
        assert!(!dir_set.is_empty());

        dir_set.clear();
        add_parent_dirs("deep/nested/path/file.rs", &mut dir_set);
        assert!(dir_set.contains("deep"));
        assert!(dir_set.contains("deep/nested"));
        assert!(dir_set.contains("deep/nested/path"));
        // Should have at least the 3 parents
        assert!(dir_set.len() >= 3);
    }

    #[test]
    fn test_dirs_to_results() {
        let mut dir_set = std::collections::HashSet::new();
        dir_set.insert("src".to_string());
        dir_set.insert("deep/nested".to_string());

        let results = dirs_to_results(&dir_set);
        assert_eq!(results.len(), 2);

        let labels: Vec<_> = results.iter().map(|r| r.label.clone()).collect();
        assert!(labels.contains(&"src".to_string()));
        assert!(labels.contains(&"nested".to_string()));

        for r in &results {
            assert_eq!(r.file_type, FileType::Folder);
        }
    }

    #[tokio::test]
    async fn test_list_workspace_files_basic() {
        use std::fs;
        use tempfile::TempDir;

        clear_file_search_cache().await;
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Create some files
        fs::write(workspace.join("main.rs"), "fn main() {}").unwrap();
        fs::write(workspace.join("lib.rs"), "pub fn lib() {}").unwrap();
        fs::create_dir(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/utils.rs"), "pub fn util() {}").unwrap();

        let results = list_workspace_files(workspace.to_str().unwrap(), 100)
            .await
            .unwrap();

        // Should find files and their parent directories
        let paths: Vec<_> = results.iter().map(|r| r.path.clone()).collect();
        assert!(paths.contains(&"main.rs".to_string()));
        assert!(paths.contains(&"lib.rs".to_string()));
        assert!(paths.contains(&"src/utils.rs".to_string()));
        assert!(paths.contains(&"src".to_string()));
    }

    #[tokio::test]
    async fn test_list_workspace_files_respects_limit() {
        use std::fs;
        use tempfile::TempDir;

        clear_file_search_cache().await;
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Create many files
        for i in 0..10 {
            fs::write(workspace.join(format!("file{}.rs", i)), "").unwrap();
        }

        let results = list_workspace_files(workspace.to_str().unwrap(), 5)
            .await
            .unwrap();
        let file_count = results
            .iter()
            .filter(|r| r.file_type == FileType::File)
            .count();
        assert!(
            file_count <= 5,
            "Should have at most 5 files, got {}",
            file_count
        );
        assert!(
            results.len() >= file_count,
            "Total results should be >= file count"
        );
    }

    #[tokio::test]
    async fn test_list_workspace_files_excludes_dirs() {
        use std::fs;
        use tempfile::TempDir;

        clear_file_search_cache().await;
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        fs::write(workspace.join("main.rs"), "").unwrap();
        fs::create_dir(workspace.join("node_modules")).unwrap();
        fs::write(workspace.join("node_modules/package.json"), "").unwrap();
        fs::create_dir(workspace.join(".git")).unwrap();
        fs::write(workspace.join(".git/config"), "").unwrap();

        let results = list_workspace_files(workspace.to_str().unwrap(), 100)
            .await
            .unwrap();
        let paths: Vec<_> = results.iter().map(|r| r.path.clone()).collect();

        assert!(paths.contains(&"main.rs".to_string()));
        assert!(
            !paths.iter().any(|p| p.contains("node_modules")),
            "Should exclude node_modules"
        );
        assert!(
            !paths.iter().any(|p| p.contains(".git")),
            "Should exclude .git"
        );
    }

    #[tokio::test]
    async fn test_search_workspace_files_basic() {
        use std::fs;
        use tempfile::TempDir;

        clear_file_search_cache().await;
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        fs::write(workspace.join("main.rs"), "").unwrap();
        fs::write(workspace.join("lib.rs"), "").unwrap();
        fs::write(workspace.join("README.md"), "").unwrap();

        let results = search_workspace_files("main", workspace.to_str().unwrap(), 10).await;
        assert!(!results.is_empty(), "Should find main.rs");

        let results = search_workspace_files("readme", workspace.to_str().unwrap(), 10).await;
        assert!(!results.is_empty(), "Should find README.md");

        let results = search_workspace_files("nonexistent", workspace.to_str().unwrap(), 10).await;
        assert!(results.is_empty(), "Should not find nonexistent file");
    }

    #[tokio::test]
    async fn test_search_workspace_files_empty_query() {
        use std::fs;
        use tempfile::TempDir;

        clear_file_search_cache().await;
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        fs::write(workspace.join("a.rs"), "").unwrap();
        fs::write(workspace.join("b.rs"), "").unwrap();

        let results = search_workspace_files("", workspace.to_str().unwrap(), 10).await;
        let paths: Vec<_> = results.iter().map(|r| r.path.clone()).collect();
        assert!(
            paths.contains(&"a.rs".to_string()),
            "Should find a.rs, got {:?}",
            paths
        );
        assert!(
            paths.contains(&"b.rs".to_string()),
            "Should find b.rs, got {:?}",
            paths
        );
        // May include empty string parent directory, so just verify both files are present
    }

    #[tokio::test]
    async fn test_search_workspace_files_limit() {
        use std::fs;
        use tempfile::TempDir;

        clear_file_search_cache().await;
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        for i in 0..20 {
            fs::write(workspace.join(format!("file{}.rs", i)), "").unwrap();
        }

        let results = search_workspace_files("", workspace.to_str().unwrap(), 5).await;
        assert_eq!(results.len(), 5, "Should respect limit");
    }

    #[tokio::test]
    async fn test_list_workspace_files_filters_db() {
        use std::fs;
        use tempfile::TempDir;

        clear_file_search_cache().await;
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        fs::write(workspace.join("main.rs"), "").unwrap();
        fs::write(workspace.join("data.db"), "binary").unwrap();
        fs::write(workspace.join("build.sqlite"), "binary").unwrap();
        fs::write(workspace.join("cache.sqlite3"), "binary").unwrap();

        let results = list_workspace_files(workspace.to_str().unwrap(), 100)
            .await
            .unwrap();
        let paths: Vec<_> = results.iter().map(|r| r.path.clone()).collect();

        assert!(
            paths.contains(&"main.rs".to_string()),
            "Should include main.rs"
        );
        assert!(
            !paths.iter().any(|p| p.ends_with("data.db")),
            "Should exclude data.db"
        );
        assert!(
            !paths.iter().any(|p| p.ends_with("build.sqlite")),
            "Should exclude build.sqlite"
        );
        assert!(
            !paths.iter().any(|p| p.ends_with("cache.sqlite3")),
            "Should exclude cache.sqlite3"
        );
    }
}
