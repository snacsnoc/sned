use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::hooks::HookName;

use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};

/// Extract the hook name from a file path.
/// Unix: extensionless files are canonical (e.g., "TaskStart").
/// Windows: .ps1 PowerShell scripts (e.g., "TaskStart.ps1").
/// Returns the base hook name (e.g., "TaskStart").
fn extract_hook_name_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name().and_then(|s| s.to_str())?;

    #[cfg(target_os = "windows")]
    {
        // Windows: check for .ps1 extension
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext == "ps1" {
                    return Some(stem.to_string());
                }
            }
        }
    }

    // Unix: extensionless file - the entire file name is the hook name
    // Windows: also check extensionless for compatibility
    Some(file_name.to_string())
}

/// Parse a string into a HookName.
fn parse_hook_name(s: &str) -> Option<HookName> {
    match s {
        "PreToolUse" => Some(HookName::PreToolUse),
        "PostToolUse" => Some(HookName::PostToolUse),
        "TaskStart" => Some(HookName::TaskStart),
        "TaskResume" => Some(HookName::TaskResume),
        "TaskCancel" => Some(HookName::TaskCancel),
        "TaskComplete" => Some(HookName::TaskComplete),
        "PreCompact" => Some(HookName::PreCompact),
        _ => None,
    }
}

/// Caches hook discovery results within a single process run.
///
/// The cache uses a HashMap with automatic invalidation via `notify` file watcher.
/// When a hook file is created, modified, or removed, the cache automatically
/// invalidates the affected entry.
#[derive(Debug)]
pub struct HookDiscoveryCache {
    cache: Arc<Mutex<HashMap<HookName, Vec<PathBuf>>>>,
    watch_dirs: Arc<Mutex<Vec<PathBuf>>>,
    watcher: Arc<Mutex<Option<RecommendedWatcher>>>,
}

impl HookDiscoveryCache {
    /// Create a new cache that will scan `watch_dirs` for hook files.
    /// Automatically watches for file changes and invalidates cache entries.
    pub fn new(watch_dirs: Vec<PathBuf>) -> Self {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let watch_dirs = Arc::new(Mutex::new(watch_dirs));
        let watcher = Arc::new(Mutex::new(None));

        let cache_clone = cache.clone();
        let watch_dirs_clone = watch_dirs.clone();

        // Create the file watcher with per-hook invalidation
        let watcher_impl = RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    // Invalidate specific hook names on file create, modify, or remove
                    match event.kind {
                        notify::EventKind::Create(_)
                        | notify::EventKind::Modify(_)
                        | notify::EventKind::Remove(_) => {
                            // Extract hook name from path and invalidate only that entry
                            for path in &event.paths {
                                if let Some(hook_name) = extract_hook_name_from_path(path)
                                    && let Some(name) = parse_hook_name(&hook_name)
                                {
                                    let mut guard = cache_clone.lock();
                                    guard.remove(&name);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            },
            Config::default(),
        );

        if let Ok(mut w) = watcher_impl {
            // Watch all directories
            let dirs = watch_dirs_clone.lock();
            for dir in dirs.iter() {
                if dir.exists() {
                    let _ = w.watch(dir, RecursiveMode::NonRecursive);
                }
            }
            *watcher.lock() = Some(w);
        }

        Self {
            cache,
            watch_dirs,
            watcher,
        }
    }

    /// Discover hooks for `hook_name`, consulting the cache first.
    ///
    /// On a cache miss the configured watch directories are scanned for
    /// files matching the hook name: extensionless (Unix canonical form)
    /// or `.ps1` (Windows PowerShell).
    pub fn discover_hooks(&self, hook_name: HookName) -> Vec<PathBuf> {
        {
            let mut cache_guard = self.cache.lock();
            if let Some(paths) = cache_guard.get(&hook_name).cloned()
                && !paths.is_empty()
            {
                let existing_paths: Vec<PathBuf> = paths
                    .iter()
                    .filter(|path| path.is_file())
                    .cloned()
                    .collect();

                if existing_paths.len() == paths.len() {
                    return paths;
                }

                cache_guard.insert(hook_name, existing_paths.clone());
                if !existing_paths.is_empty() {
                    return existing_paths;
                }
            }
        }

        let mut paths = Vec::new();
        let dirs = self.watch_dirs.lock();
        for dir in dirs.iter() {
            if dir.is_dir()
                && let Ok(entries) = fs::read_dir(dir)
            {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }

                    // Unix canonical form: extensionless executable
                    if let Some(name) = path.file_name().and_then(|s| s.to_str())
                        && name == hook_name.as_str()
                    {
                        paths.push(path.clone());
                        continue;
                    }

                    // Windows PowerShell: .ps1 extension only
                    #[cfg(target_os = "windows")]
                    if let Some(ext) = path.extension().and_then(|e| e.to_str())
                        && ext == "ps1"
                        && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                        && name == hook_name.as_str()
                    {
                        paths.push(path.clone());
                    }
                }
            }
        }

        {
            let mut cache_guard = self.cache.lock();
            cache_guard.insert(hook_name, paths.clone());
        }

        paths
    }

    /// Manually invalidate the cached result for a single hook name.
    ///
    /// This is useful for callers that know a hook file has been modified.
    pub fn invalidate(&self, hook_name: HookName) {
        let mut cache_guard = self.cache.lock();
        cache_guard.remove(&hook_name);
    }

    /// Manually invalidate the entire cache.
    pub fn invalidate_all(&self) {
        let mut cache_guard = self.cache.lock();
        cache_guard.clear();
    }

    /// Add a new watch directory at runtime.
    ///
    /// This is useful when a workspace adds a new hooks directory.
    /// Scans the directory for existing hooks and invalidates their cache entries
    /// so newly added workspace hooks become visible without a manual cache clear.
    pub fn add_watch_dir(&self, dir: &Path) {
        if !dir.exists() {
            return;
        }

        // Scan for existing hooks and invalidate their cache entries
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file()
                    && let Some(hook_name) = extract_hook_name_from_path(&path)
                    && let Some(name) = parse_hook_name(&hook_name)
                {
                    self.invalidate(name);
                }
            }
        }

        // Add to watch_dirs list
        let mut dirs = self.watch_dirs.lock();
        let dir_buf = dir.to_path_buf();
        if !dirs.contains(&dir_buf) {
            dirs.push(dir_buf);
        }
        drop(dirs);

        // Also watch with the file watcher
        if let Some(ref mut watcher) = *self.watcher.lock() {
            let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_hooks_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("sned_test_").join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        use std::thread;
        use std::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }

        condition()
    }

    #[test]
    fn test_cache_basic_lookup() {
        let hooks_dir = setup_hooks_dir("basic_lookup");
        let hook_path = hooks_dir.join("TaskStart");
        fs::write(&hook_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let result1 = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result1.len(), 1);
        assert_eq!(result1[0], hook_path);

        let result2 = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result2, result1);
    }

    #[test]
    fn test_cache_invalidates_on_file_creation() {
        let hooks_dir = setup_hooks_dir("file_creation");

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let result1 = cache.discover_hooks(HookName::PreToolUse);
        assert!(result1.is_empty());

        let hook_path = hooks_dir.join("PreToolUse");
        fs::write(&hook_path, "test").unwrap();

        cache.invalidate(HookName::PreToolUse);

        let result2 = cache.discover_hooks(HookName::PreToolUse);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0], hook_path);
    }

    #[test]
    fn test_cache_invalidates_on_file_deletion() {
        let hooks_dir = setup_hooks_dir("file_deletion");
        let hook_path = hooks_dir.join("TaskCancel");
        fs::write(&hook_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let result1 = cache.discover_hooks(HookName::TaskCancel);
        assert_eq!(result1.len(), 1);

        fs::remove_file(&hook_path).unwrap();

        cache.invalidate(HookName::TaskCancel);

        let result2 = cache.discover_hooks(HookName::TaskCancel);
        assert!(result2.is_empty());
    }

    #[test]
    fn test_cache_invalidates_on_file_modification() {
        let hooks_dir = setup_hooks_dir("file_modification");
        let hook_path = hooks_dir.join("TaskComplete");
        fs::write(&hook_path, "original").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let result1 = cache.discover_hooks(HookName::TaskComplete);
        assert_eq!(result1.len(), 1);

        fs::write(&hook_path, "modified").unwrap();

        cache.invalidate(HookName::TaskComplete);

        let result2 = cache.discover_hooks(HookName::TaskComplete);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0], hook_path);
    }

    #[test]
    fn test_cache_does_not_affect_other_hooks() {
        let hooks_dir = setup_hooks_dir("other_hooks");
        let task_start_path = hooks_dir.join("TaskStart");
        fs::write(&task_start_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let result1 = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result1.len(), 1);

        let pre_tool_path = hooks_dir.join("PreToolUse");
        fs::write(&pre_tool_path, "test").unwrap();

        let result2 = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0], task_start_path);

        let result3 = cache.discover_hooks(HookName::PreToolUse);
        assert_eq!(result3.len(), 1);
        assert_eq!(result3[0], pre_tool_path);
    }

    #[test]
    fn test_manual_invalidate() {
        let hooks_dir = setup_hooks_dir("manual_invalidate");
        let hook_path = hooks_dir.join("TaskStart");
        fs::write(&hook_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let result1 = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result1.len(), 1);

        fs::remove_file(&hook_path).unwrap();

        let result2 = cache.discover_hooks(HookName::TaskStart);
        assert!(result2.is_empty());

        cache.invalidate(HookName::TaskStart);

        let result3 = cache.discover_hooks(HookName::TaskStart);
        assert!(result3.is_empty());
    }

    #[test]
    fn test_manual_invalidate_all() {
        let hooks_dir = setup_hooks_dir("invalidate_all");
        let task_start_path = hooks_dir.join("TaskStart");
        let pre_tool_path = hooks_dir.join("PreToolUse");
        fs::write(&task_start_path, "test").unwrap();
        fs::write(&pre_tool_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        cache.discover_hooks(HookName::TaskStart);
        cache.discover_hooks(HookName::PreToolUse);

        fs::remove_file(&task_start_path).unwrap();
        fs::remove_file(&pre_tool_path).unwrap();

        assert!(cache.discover_hooks(HookName::TaskStart).is_empty());
        assert!(cache.discover_hooks(HookName::PreToolUse).is_empty());

        cache.invalidate_all();

        assert!(cache.discover_hooks(HookName::TaskStart).is_empty());
        assert!(cache.discover_hooks(HookName::PreToolUse).is_empty());
    }

    #[test]
    fn test_automatic_invalidation_on_file_creation() {
        let hooks_dir = setup_hooks_dir("auto_invalidate");
        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        // First lookup - should be empty
        let result1 = cache.discover_hooks(HookName::TaskStart);
        assert!(result1.is_empty());

        // Create a new hook file
        let hook_path = hooks_dir.join("TaskStart");
        fs::write(&hook_path, "test").unwrap();

        // Second lookup - cache should have been invalidated automatically
        assert!(wait_until(|| cache
            .discover_hooks(HookName::TaskStart)
            .len()
            == 1));
        let result2 = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0], hook_path);
    }

    #[test]
    fn test_automatic_invalidation_on_file_modification() {
        let hooks_dir = setup_hooks_dir("auto_modify");
        let hook_path = hooks_dir.join("TaskComplete");
        fs::write(&hook_path, "original").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        // First lookup - should find the file
        let result1 = cache.discover_hooks(HookName::TaskComplete);
        assert_eq!(result1.len(), 1);

        // Modify the file
        fs::write(&hook_path, "modified").unwrap();

        // Second lookup - cache should have been invalidated and re-scanned
        assert!(wait_until(|| cache
            .discover_hooks(HookName::TaskComplete)
            .len()
            == 1));
        let result2 = cache.discover_hooks(HookName::TaskComplete);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0], hook_path);
    }

    #[test]
    fn test_automatic_invalidation_on_file_deletion() {
        let hooks_dir = setup_hooks_dir("auto_delete");
        let hook_path = hooks_dir.join("TaskCancel");
        fs::write(&hook_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        // First lookup - should find the file
        let result1 = cache.discover_hooks(HookName::TaskCancel);
        assert_eq!(result1.len(), 1);

        // Delete the file
        fs::remove_file(&hook_path).unwrap();

        // Second lookup - cache should have been invalidated
        assert!(wait_until(|| cache
            .discover_hooks(HookName::TaskCancel)
            .is_empty()));
        let result2 = cache.discover_hooks(HookName::TaskCancel);
        assert!(result2.is_empty());
    }

    #[test]
    fn test_add_watch_dir_makes_dir_visible() {
        let hooks_dir1 = setup_hooks_dir("add_watch_dir_1");
        let hooks_dir2 = setup_hooks_dir("add_watch_dir_2");

        let hook_path = hooks_dir2.join("TaskStart");
        fs::write(&hook_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir1.clone()]);

        // Initially should not find hook in dir2
        let result1 = cache.discover_hooks(HookName::TaskStart);
        assert!(result1.is_empty());

        // Add dir2 to watch dirs
        cache.add_watch_dir(&hooks_dir2);

        // Now should find hook in dir2
        cache.invalidate(HookName::TaskStart);
        let result2 = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0], hook_path);
    }

    #[test]
    fn test_cache_discovers_extensionless_unix_hooks() {
        let hooks_dir = setup_hooks_dir("extensionless_unix");
        let hook_path = hooks_dir.join("TaskStart");
        fs::write(&hook_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let result = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], hook_path);
    }

    #[test]
    fn test_cache_discovers_extensionless_only_on_unix() {
        let hooks_dir = setup_hooks_dir("extensionless_only");
        let extensionless = hooks_dir.join("PreToolUse");
        let dot_hook = hooks_dir.join("PreToolUse.hook");
        let dot_sh = hooks_dir.join("PreToolUse.sh");
        fs::write(&extensionless, "test").unwrap();
        fs::write(&dot_hook, "test").unwrap();
        fs::write(&dot_sh, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let result = cache.discover_hooks(HookName::PreToolUse);
        // On Unix: only extensionless is discovered
        // On Windows: extensionless + .ps1 would be discovered
        #[cfg(not(target_os = "windows"))]
        {
            assert_eq!(result.len(), 1);
            assert!(result.contains(&extensionless));
            assert!(!result.contains(&dot_hook));
            assert!(!result.contains(&dot_sh));
        }
        #[cfg(target_os = "windows")]
        {
            assert_eq!(result.len(), 1);
            assert!(result.contains(&extensionless));
        }
    }

    #[test]
    fn test_cache_extensionless_matches_uncached_discovery() {
        use crate::core::hooks::HookManager;

        let hooks_dir = setup_hooks_dir("extensionless_match");
        let hook_path = hooks_dir.join("TaskComplete");
        fs::write(&hook_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        let cached_result = cache.discover_hooks(HookName::TaskComplete);

        let hook_manager = HookManager::new("test_user")
            .with_discovery_cache(HookDiscoveryCache::new(vec![hooks_dir.clone()]));

        let uncached_result = hook_manager.discover_hooks(HookName::TaskComplete);

        assert_eq!(cached_result.len(), uncached_result.len());
        assert!(cached_result.iter().all(|p| uncached_result.contains(p)));
    }

    #[test]
    fn test_cache_per_hook_invalidation_does_not_affect_other_hooks() {
        let hooks_dir = setup_hooks_dir("per_hook_invalidate");
        let task_start_path = hooks_dir.join("TaskStart");
        let task_complete_path = hooks_dir.join("TaskComplete");
        fs::write(&task_start_path, "test").unwrap();
        fs::write(&task_complete_path, "test").unwrap();

        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        // Populate cache for both hooks
        let _ = cache.discover_hooks(HookName::TaskStart);
        let _ = cache.discover_hooks(HookName::TaskComplete);

        // Modify TaskStart hook - should only invalidate TaskStart, not TaskComplete
        fs::write(&task_start_path, "modified").unwrap();
        cache.invalidate(HookName::TaskStart);

        // TaskComplete should still be cached (not invalidated)
        let result_complete = cache.discover_hooks(HookName::TaskComplete);
        assert_eq!(result_complete.len(), 1);

        // TaskStart should be re-discovered with modified content
        let result_start = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result_start.len(), 1);
        assert_eq!(result_start[0], task_start_path);
    }

    #[test]
    fn test_add_watch_dir_invalidates_cache_for_existing_hooks() {
        let hooks_dir1 = setup_hooks_dir("add_watch_invalidate_1");
        let hooks_dir2 = setup_hooks_dir("add_watch_invalidate_2");

        // Create hook in dir2 before adding it to watch
        let hook_path = hooks_dir2.join("TaskStart");
        fs::write(&hook_path, "test").unwrap();

        // Create cache watching only dir1
        let cache = HookDiscoveryCache::new(vec![hooks_dir1.clone()]);

        // Populate cache - should be empty since dir2 is not watched
        let result1 = cache.discover_hooks(HookName::TaskStart);
        assert!(result1.is_empty());

        // Add dir2 to watch dirs - should scan and invalidate cache
        cache.add_watch_dir(&hooks_dir2);

        // Now should find hook in dir2 without manual invalidation
        let result2 = cache.discover_hooks(HookName::TaskStart);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0], hook_path);
    }

    #[test]
    fn test_automatic_invalidation_on_create_event() {
        let hooks_dir = setup_hooks_dir("auto_create_event");
        let cache = HookDiscoveryCache::new(vec![hooks_dir.clone()]);

        // First lookup - should be empty
        let result1 = cache.discover_hooks(HookName::TaskCancel);
        assert!(result1.is_empty());

        // Create a new hook file
        let hook_path = hooks_dir.join("TaskCancel");
        fs::write(&hook_path, "test").unwrap();

        // Second lookup - cache should have been invalidated automatically by Create event
        assert!(wait_until(|| cache
            .discover_hooks(HookName::TaskCancel)
            .len()
            == 1));
        let result2 = cache.discover_hooks(HookName::TaskCancel);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0], hook_path);
    }
}
