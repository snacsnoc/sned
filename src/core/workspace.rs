//! Workspace path resolution and file safety.
//!
//! - `dirac/src/core/workspace/WorkspaceResolver.ts`
//! - `dirac/src/core/workspace/index.ts`
//! - `dirac/src/core/ignore/DiracIgnoreController.ts`

use std::path::{Path, PathBuf};

// ============================================================================
// Default Ignore Patterns
// ============================================================================

/// Default ignore patterns for Sned.
///
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    // Version control
    ".git",
    ".svn",
    ".hg",
    ".fslckout",
    "_fslckout",
    ".bzr",
    "_darcs",
    ".fossil-settings",
    // Dependencies
    "node_modules",
    "bower_components",
    "jspm_packages",
    "vendor",
    ".cache",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    "venv",
    "env",
    ".env",
    ".yarn",
    // Build & Output
    "dist",
    "build",
    "out",
    "target",
    "bin",
    "obj",
    "generated",
    "gen",
    "CMakeFiles",
    ".gradle",
    ".turbo",
    ".next",
    ".nuxt",
    ".svelte-kit",
    "coverage",
    ".nyc_output",
    "__snapshots__",
    // Temporary files
    "tmp",
    "temp",
    // GitHub
    ".github",
    // IDEs
    ".idea",
    ".vs",
    ".vscode",
    "*.egg-info",
    "*.suo",
    "*.user",
    "*.userosscache",
    "*.sln.doccache",
    "*.ncb",
    // OS files
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
    // Binaries & Archives
    "*.vsix",
    "*.zip",
    "*.tar",
    "*.tar.gz",
    "*.tgz",
    "*.tar.bz2",
    "*.tar.xz",
    "*.gz",
    "*.jar",
    "*.war",
    "*.ear",
    "*.exe",
    "*.dll",
    "*.so",
    "*.dylib",
    "*.a",
    "*.o",
    "*.obj",
    "*.class",
    "*.pyc",
    "*.pyo",
    "*.wasm",
    "*.bin",
    "*.dat",
    "*.db",
    "*.sqlite",
    "*.sqlite3",
    "*.pdb",
    // Locks & Metadata
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Gemfile.lock",
    "Cargo.lock",
    "composer.lock",
    "poetry.lock",
    "Pipfile.lock",
    "bun.lockb",
    // Misc
    "*.min.js",
    "*.min.css",
    "*.map",
];

/// Symbol indicating a locked file.
pub const LOCK_TEXT_SYMBOL: &str = "\u{1F512}";

// ============================================================================
// Workspace Path Resolution
// ============================================================================

/// Resolves a relative path against a workspace root.
///
#[must_use] 
pub fn resolve_workspace_path(cwd: &str, relative_path: &str) -> PathBuf {
    let cwd_path = Path::new(cwd);
    cwd_path.join(relative_path)
}

// ============================================================================
// Ignore Controller
// ============================================================================

/// Controls file access based on ignore patterns.
///
#[derive(Debug, Clone)]
pub struct SnedIgnoreController {
    pub yolo_mode: bool,
    cwd: PathBuf,
    ignore_builder: ignore::gitignore::GitignoreBuilder,
    ignore: Option<ignore::gitignore::Gitignore>,
    sned_ignore_content: Option<String>,
}

impl SnedIgnoreController {
    #[must_use] 
    pub fn new(cwd: &str) -> Self {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(cwd);

        // Add default ignore patterns
        for pattern in DEFAULT_IGNORE_PATTERNS {
            let _ = builder.add_line(None, pattern);
            // For directory patterns (no extension), also add recursive pattern
            // to match TypeScript ignore package behavior
            if !pattern.contains('.') && !pattern.contains('*') {
                let _ = builder.add_line(None, &format!("{pattern}/**"));
            }
        }

        let ignore = builder.build().ok();

        Self {
            yolo_mode: false,
            cwd: PathBuf::from(cwd),
            ignore_builder: builder,
            ignore,
            sned_ignore_content: None,
        }
    }

    /// Loads custom patterns from `.snedignore` if it exists.
    ///
    pub fn load_sned_ignore(&mut self) -> Result<(), String> {
        let ignore_path = self.cwd.join(".snedignore");

        if ignore_path.exists() {
            match std::fs::read_to_string(&ignore_path) {
                Ok(content) => {
                    self.sned_ignore_content = Some(content.clone());
                    self.process_ignore_content(&content)?;
                    // Add .snedignore itself to ignored files
                    let _ = self.ignore_builder.add_line(None, ".snedignore");
                }
                Err(e) => {
                    return Err(format!("Failed to read .snedignore: {e}"));
                }
            }
        } else {
            self.sned_ignore_content = None;
        }

        // Build the ignore matcher
        match self.ignore_builder.build() {
            Ok(ignore) => {
                self.ignore = Some(ignore);
                Ok(())
            }
            Err(e) => Err(format!("Failed to build ignore matcher: {e}")),
        }
    }

    /// Process ignore content, handling `!include` directives.
    ///
    fn process_ignore_content(&mut self, content: &str) -> Result<(), String> {
        if !content.contains("!include ") {
            // No includes, just add the content directly
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    let _ = self.ignore_builder.add_line(None, trimmed);
                }
            }
            return Ok(());
        }

        // Process !include directives
        let combined = self.process_sned_ignore_includes(content)?;
        for line in combined.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                let _ = self.ignore_builder.add_line(None, trimmed);
            }
        }
        Ok(())
    }

    /// Process !include directives and combine all included file contents.
    ///
    fn process_sned_ignore_includes(&self, content: &str) -> Result<String, String> {
        let mut combined = String::new();

        for line in content.lines() {
            let trimmed = line.trim();

            if !trimmed.starts_with("!include ") {
                combined.push('\n');
                combined.push_str(line);
                continue;
            }

            // Process !include directive
            let include_path = trimmed.strip_prefix("!include ").unwrap_or("").trim();
            let resolved_path = self.cwd.join(include_path);

            if resolved_path.exists() {
                match std::fs::read_to_string(&resolved_path) {
                    Ok(included_content) => {
                        combined.push('\n');
                        combined.push_str(&included_content);
                    }
                    Err(e) => {
                        return Err(format!(
                            "Failed to read included file {}: {}",
                            resolved_path.display(),
                            e
                        ));
                    }
                }
            } else {
                return Err(format!(
                    "Included file not found: {}",
                    resolved_path.display()
                ));
            }
        }

        Ok(combined)
    }

    /// Check if a file should be accessible.
    ///
    #[must_use] 
    pub fn validate_access(&self, file_path: &str) -> bool {
        if self.yolo_mode {
            return true;
        }

        let absolute_path = self.cwd.join(file_path);
        let Ok(relative_path) = absolute_path.strip_prefix(&self.cwd) else {
            // Path is outside cwd, allow access
            return true;
        };

        // Convert to forward slashes for gitignore matching
        let relative_str = relative_path.to_string_lossy().replace('\\', "/");

        if let Some(ref ignore) = self.ignore {
            !ignore.matched(&relative_str, false).is_ignore()
        } else {
            true
        }
    }

    /// Check if a terminal command should be allowed.
    ///
    #[must_use] 
    pub fn validate_command(&self, command: &str) -> Option<String> {
        if self.yolo_mode {
            return None;
        }

        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return None;
        }

        let base_command = parts[0].to_lowercase();

        // Commands that read file contents
        let file_reading_commands = [
            "cat",
            "less",
            "more",
            "head",
            "tail",
            "grep",
            "awk",
            "sed",
            "get-content",
            "gc",
            "type",
            "select-string",
            "sls",
        ];

        if file_reading_commands.contains(&base_command.as_str()) {
            for arg in &parts[1..] {
                // Skip flags/options
                if arg.starts_with('-') || arg.starts_with('/') {
                    continue;
                }
                // Skip PowerShell parameter names
                if arg.contains(':') {
                    continue;
                }
                // Validate file access
                if !self.validate_access(arg) {
                    return Some(arg.to_string());
                }
            }
        }

        None
    }

    /// Filter an array of paths, removing ignored ones.
    ///
    #[must_use] 
    pub fn filter_paths(&self, paths: &[String]) -> Vec<String> {
        paths
            .iter()
            .filter(|p| self.validate_access(p))
            .cloned()
            .collect()
    }
}

// ============================================================================
// Path Utilities
// ============================================================================

/// Checks if a path is within the workspace.
#[must_use] 
pub fn is_within_workspace(workspace_root: &str, path: &str) -> bool {
    let root = Path::new(workspace_root);
    let target = Path::new(path);
    target.starts_with(root)
}

/// Checks if a file is a binary file based on extension.
#[must_use] 
pub fn is_binary_file(path: &str) -> bool {
    let binary_extensions = [
        ".exe", ".dll", ".so", ".dylib", ".bin", ".dat", ".db", ".sqlite", ".sqlite3", ".pdb",
        ".o", ".obj", ".a", ".lib", ".class", ".pyc", ".pyo", ".wasm", ".jar", ".war", ".ear",
        ".zip", ".tar", ".gz", ".tgz", ".bz2", ".xz", ".7z", ".rar", ".jpg", ".jpeg", ".png",
        ".gif", ".bmp", ".ico", ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx",
    ];

    let path_lower = path.to_lowercase();
    binary_extensions
        .iter()
        .any(|ext| path_lower.ends_with(ext))
}

/// Checks if a file is too large to process.
#[must_use] 
pub fn is_large_file(size_bytes: u64, max_size: u64) -> bool {
    size_bytes > max_size
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_workspace_path() {
        let result = resolve_workspace_path("/home/user/project", "src/main.rs");
        assert_eq!(result, PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn test_resolve_absolute_path() {
        let result = resolve_workspace_path("/home/user", "/absolute/path.rs");
        assert_eq!(result, PathBuf::from("/absolute/path.rs"));
    }

    #[test]
    fn test_ignore_controller_default_patterns() {
        let controller = SnedIgnoreController::new("/tmp/test");

        // Should ignore common directories
        // Note: gitignore patterns like "node_modules" match the directory but not contents
        // unless suffixed with "/" or "/**". The TypeScript ignore package handles this
        // differently than git semantics. For parity, we check both exact and nested.
        assert!(!controller.validate_access("node_modules"));
        assert!(!controller.validate_access(".git"));
        assert!(!controller.validate_access("dist"));
        assert!(!controller.validate_access(".DS_Store"));

        // Should allow normal files
        assert!(controller.validate_access("src/main.rs"));
        assert!(controller.validate_access("README.md"));
        assert!(controller.validate_access("package.json"));
    }

    #[test]
    fn test_ignore_controller_yolo_mode() {
        let mut controller = SnedIgnoreController::new("/tmp/test");
        controller.yolo_mode = true;

        // Should allow everything in yolo mode
        assert!(controller.validate_access("node_modules/package.json"));
        assert!(controller.validate_access(".git/config"));
        assert!(controller.validate_access("src/main.rs"));
    }

    #[test]
    fn test_ignore_controller_binary_patterns() {
        let controller = SnedIgnoreController::new("/tmp/test");

        // Should ignore binary files by extension
        assert!(!controller.validate_access("app.exe"));
        assert!(!controller.validate_access("lib.dll"));
        assert!(!controller.validate_access("archive.zip"));
        assert!(controller.validate_access("image.jpg")); // not in default patterns
    }

    #[test]
    fn test_ignore_controller_lock_files() {
        let controller = SnedIgnoreController::new("/tmp/test");

        // Should ignore lock files
        assert!(!controller.validate_access("package-lock.json"));
        assert!(!controller.validate_access("Cargo.lock"));
        assert!(!controller.validate_access("yarn.lock"));
    }

    #[test]
    fn test_ignore_controller_outside_cwd() {
        let controller = SnedIgnoreController::new("/tmp/test");

        // Should allow paths outside cwd
        assert!(controller.validate_access("/../outside.rs"));
        assert!(controller.validate_access("/absolute/path.rs"));
    }

    #[test]
    fn test_validate_command() {
        let controller = SnedIgnoreController::new("/tmp/test");

        // cat on ignored file should fail
        assert_eq!(
            controller.validate_command("cat dist/bundle.js"),
            Some("dist/bundle.js".to_string())
        );

        // cat on allowed file should pass
        assert_eq!(controller.validate_command("cat src/main.rs"), None);

        // Non-file-reading command should pass
        assert_eq!(controller.validate_command("ls -la"), None);
    }

    #[test]
    fn test_filter_paths() {
        let controller = SnedIgnoreController::new("/tmp/test");

        let paths = vec![
            "src/main.rs".to_string(),
            "dist/bundle.js".to_string(),
            "README.md".to_string(),
            ".DS_Store".to_string(),
        ];

        let filtered = controller.filter_paths(&paths);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains(&"src/main.rs".to_string()));
        assert!(filtered.contains(&"README.md".to_string()));
    }

    #[test]
    fn test_is_within_workspace() {
        assert!(is_within_workspace(
            "/home/user/project",
            "/home/user/project/src/main.rs"
        ));
        assert!(!is_within_workspace(
            "/home/user/project",
            "/home/other/file.rs"
        ));
        assert!(!is_within_workspace(
            "/home/user/project",
            "/home/user/project2/src/main.rs"
        ));
    }

    #[test]
    fn test_is_binary_file() {
        assert!(is_binary_file("file.exe"));
        assert!(is_binary_file("image.PNG"));
        assert!(!is_binary_file("main.rs"));
        assert!(!is_binary_file("README.md"));
    }

    #[test]
    fn test_is_large_file() {
        assert!(is_large_file(1024 * 1024 + 1, 1024 * 1024)); // > 1MB
        assert!(!is_large_file(1024, 1024 * 1024)); // < 1MB
    }
}
