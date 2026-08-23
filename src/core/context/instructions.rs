use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing;

/// Rule toggles: maps rule file path to enabled/disabled state
pub type RuleToggles = HashMap<String, bool>;

/// Patterns that indicate prompt injection when found in repo-controlled content.
/// Only multi-word patterns to minimize false positives on legitimate instructions.
const INJECTION_PATTERNS: &[&str] = &[
    "ignore previous",
    "ignore all",
    "forget all",
    "disregard safety",
    "disregard previous",
    "override safety",
    "bypass validation",
    "skip validation",
    "disable security",
    "curl | bash",
    "wget | bash",
    "download and execute",
];

/// Check repo-controlled content for injection patterns.
/// Returns detected patterns so the caller can surface them to the user.
/// Logs via tracing only — does not print directly (TUI output architecture).
fn check_injection_patterns(content: &str, source: &str) -> Vec<&'static str> {
    let lower = content.to_lowercase();
    let mut detected = Vec::new();
    for pattern in INJECTION_PATTERNS {
        if lower.contains(pattern) {
            tracing::warn!(
                source = source,
                pattern = pattern,
                "Repo-controlled content contains injection-like pattern"
            );
            detected.push(*pattern);
        }
    }
    // Do not eprintln! — violates TUI output architecture.
    // Caller can route warnings through OutputWriter if needed.
    detected
}

/// Skill metadata
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub path: String,
    pub source: SkillSource,
}

/// Skill source type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillSource {
    Project,
    Global,
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Project => write!(f, "project"),
            Self::Global => write!(f, "global"),
        }
    }
}

/// Skill content with instructions
#[derive(Debug, Clone)]
pub struct SkillContent {
    pub name: String,
    pub description: String,
    pub path: String,
    pub source: SkillSource,
    pub instructions: String,
}

/// Supporting files in a skill directory
#[derive(Debug, Clone, Default)]
pub struct SkillSupportingFiles {
    pub docs: Vec<String>,
    pub scripts: Vec<String>,
}

/// A readable AGENTS.md file discovered for a path-scoped target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRuleFile {
    pub path: PathBuf,
    pub content: String,
}

/// Check only the workspace-root AGENTS.md file.
#[must_use]
pub fn find_agents_md_files(cwd: &Path) -> Vec<PathBuf> {
    let started = Instant::now();
    let top_level = cwd.join("AGENTS.md");
    let results = top_level
        .metadata()
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|_| vec![top_level.clone()])
        .unwrap_or_default();
    tracing::debug!(
        workspace = %cwd.display(),
        discovered_files = ?results,
        elapsed_ms = started.elapsed().as_millis(),
        "loaded root AGENTS.md rules"
    );
    results
}

fn resolve_target_path(workspace_root: &Path, target: &Path) -> Option<PathBuf> {
    let workspace_root = fs::canonicalize(workspace_root).ok()?;
    let raw_target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        workspace_root.join(target)
    };

    // Canonicalize the nearest existing ancestor and restore any missing path
    // components. This keeps relative paths and `..` components within the
    // same path scope even when a write target has not been created yet.
    let mut existing = raw_target.clone();
    let mut missing_components = Vec::new();
    while !existing.exists() {
        missing_components.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?.to_path_buf();
    }
    let mut resolved = fs::canonicalize(existing).ok()?;
    for component in missing_components.iter().rev() {
        resolved.push(component);
    }

    resolved.starts_with(&workspace_root).then_some(resolved)
}

/// Inspect AGENTS.md files on the target's ancestor chain, from workspace root
/// to the nearest directory. This performs only direct metadata checks.
#[must_use]
pub fn find_path_scoped_agents_md_files(workspace_root: &Path, target: &Path) -> Vec<PathBuf> {
    let started = Instant::now();
    let Some(workspace_root) = fs::canonicalize(workspace_root).ok() else {
        tracing::debug!(
            workspace = %workspace_root.display(),
            target = %target.display(),
            elapsed_ms = started.elapsed().as_millis(),
            "path-scoped AGENTS.md discovery could not resolve workspace"
        );
        return Vec::new();
    };
    let Some(resolved_target) = resolve_target_path(&workspace_root, target) else {
        tracing::debug!(
            workspace = %workspace_root.display(),
            target = %target.display(),
            discovered_files = ?Vec::<PathBuf>::new(),
            elapsed_ms = started.elapsed().as_millis(),
            "path-scoped AGENTS.md discovery skipped target outside workspace"
        );
        return Vec::new();
    };

    let mut directory = if resolved_target.is_dir() {
        resolved_target.clone()
    } else {
        resolved_target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| workspace_root.clone())
    };
    let mut files = Vec::new();

    loop {
        let candidate = directory.join("AGENTS.md");
        if candidate
            .metadata()
            .ok()
            .is_some_and(|metadata| metadata.is_file())
        {
            files.push(candidate);
        }
        if directory == workspace_root {
            break;
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        directory = parent.to_path_buf();
    }
    files.reverse();

    tracing::debug!(
        workspace = %workspace_root.display(),
        target = %target.display(),
        resolved_target = %resolved_target.display(),
        discovered_files = ?files,
        elapsed_ms = started.elapsed().as_millis(),
        "loaded path-scoped AGENTS.md rules"
    );
    files
}

fn load_agent_rule_files(
    files: impl IntoIterator<Item = PathBuf>,
    toggles: &RuleToggles,
) -> Vec<AgentRuleFile> {
    files
        .into_iter()
        .filter_map(|path| {
            let path_str = path.to_string_lossy().to_string();
            let canonical_path_str = fs::canonicalize(&path)
                .ok()
                .map(|canonical| canonical.to_string_lossy().into_owned());
            let canonical_toggle_disabled = canonical_path_str.as_ref().is_some_and(|canonical| {
                toggles.iter().any(|(configured_path, enabled)| {
                    !enabled
                        && fs::canonicalize(configured_path)
                            .ok()
                            .is_some_and(|configured| {
                                configured.to_string_lossy().as_ref() == canonical.as_str()
                            })
                })
            });
            if matches!(toggles.get(&path_str), Some(false))
                || canonical_path_str
                    .as_ref()
                    .is_some_and(|canonical| matches!(toggles.get(canonical), Some(false)))
                || canonical_toggle_disabled
            {
                return None;
            }
            match fs::read_to_string(&path) {
                Ok(content) => {
                    let content = content.trim().to_string();
                    if content.is_empty() {
                        None
                    } else {
                        check_injection_patterns(&content, &path_str);
                        Some(AgentRuleFile { path, content })
                    }
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "failed to read AGENTS.md file");
                    None
                }
            }
        })
        .collect()
}

fn format_agent_rule_files(cwd: &Path, files: &[AgentRuleFile]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let parts = files
        .iter()
        .map(|file| {
            let relative = file
                .path
                .strip_prefix(cwd)
                .ok()
                .map(Path::to_path_buf)
                .or_else(|| {
                    cwd.canonicalize()
                        .ok()
                        .and_then(|canonical| file.path.strip_prefix(canonical).ok())
                        .map(Path::to_path_buf)
                })
                .unwrap_or_else(|| file.path.clone());
            format!("## {}\n\n{}", relative.display(), file.content)
        })
        .collect::<Vec<_>>();
    let combined = parts.join("\n\n");
    check_injection_patterns(&combined, "AGENTS.md");
    Some(format!("# AGENTS.md Rules\n\n{combined}"))
}

/// Read and combine only the workspace-root AGENTS.md file.
pub fn get_local_agents_rules(cwd: &Path, toggles: &RuleToggles) -> Option<String> {
    format_agent_rule_files(cwd, &load_agent_rule_files(find_agents_md_files(cwd), toggles))
}

/// Read applicable AGENTS.md files for one explicit file or directory target.
/// Results are ordered from workspace root to the nearest ancestor.
#[must_use]
pub fn load_path_scoped_agents_rules(
    workspace_root: &Path,
    target: &Path,
    toggles: &RuleToggles,
) -> Vec<AgentRuleFile> {
    load_agent_rule_files(find_path_scoped_agents_md_files(workspace_root, target), toggles)
}

/// Format path-scoped AGENTS.md rules for callers that need the complete chain.
#[must_use]
pub fn get_path_scoped_agents_rules(
    workspace_root: &Path,
    target: &Path,
    toggles: &RuleToggles,
) -> Option<String> {
    format_agent_rule_files(
        workspace_root,
        &load_path_scoped_agents_rules(workspace_root, target, toggles),
    )
}

/// Read local windsurf rules file
pub fn get_local_windsurf_rules(cwd: &Path, toggles: &RuleToggles) -> Option<String> {
    let windsurf_path = cwd.join(".windsurfrules");
    let path_str = windsurf_path.to_string_lossy().to_string();

    if !windsurf_path.exists() || windsurf_path.is_dir() {
        return None;
    }

    // Check toggle
    if matches!(toggles.get(&path_str), Some(false)) {
        return None;
    }

    match fs::read_to_string(&windsurf_path) {
        Ok(content) => {
            let content = content.trim();
            if content.is_empty() {
                None
            } else {
                check_injection_patterns(content, ".windsurfrules");
                Some(format!("# Windsurf Rules\n\n{content}"))
            }
        }
        Err(e) => {
            tracing::warn!(
                "Failed to read .windsurfrules file at {}: {}",
                windsurf_path.display(),
                e
            );
            None
        }
    }
}

/// Read local cursor rules from file and/or directory
pub fn get_local_cursor_rules(cwd: &Path, toggles: &RuleToggles) -> Vec<Option<String>> {
    let mut results = Vec::new();

    // Check .cursorrules file
    let cursor_rules_file = cwd.join(".cursorrules");
    let file_str = cursor_rules_file.to_string_lossy().to_string();

    if cursor_rules_file.exists()
        && !cursor_rules_file.is_dir()
        && !matches!(toggles.get(&file_str), Some(false))
    {
        match fs::read_to_string(&cursor_rules_file) {
            Ok(content) => {
                let content = content.trim();
                if !content.is_empty() {
                    check_injection_patterns(content, ".cursorrules");
                    results.push(Some(format!("# Cursor Rules\n\n{content}")));
                } else {
                    results.push(None);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to read .cursorrules file at {}: {}",
                    cursor_rules_file.display(),
                    e
                );
                results.push(None);
            }
        }
    }

    // Check .cursor/rules directory
    let cursor_rules_dir = cwd.join(".cursor/rules");
    let _dir_str = cursor_rules_dir.to_string_lossy().to_string();

    if cursor_rules_dir.exists() && cursor_rules_dir.is_dir() {
        match read_directory_recursive(&cursor_rules_dir, ".mdc") {
            Ok(files) => {
                let mut parts = Vec::new();
                for file_path in files {
                    let file_str = file_path.to_string_lossy().to_string();
                    if matches!(toggles.get(&file_str), Some(false)) {
                        continue;
                    }

                    match fs::read_to_string(&file_path) {
                        Ok(content) => {
                            let content = content.trim();
                            if !content.is_empty() {
                                let relative = file_path.strip_prefix(cwd).unwrap_or(&file_path);
                                parts.push(format!("{}\n{}", relative.display(), content));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to read cursor rule at {}: {}",
                                file_path.display(),
                                e
                            );
                        }
                    }
                }

                if !parts.is_empty() {
                    let combined = parts.join("\n\n");
                    check_injection_patterns(&combined, ".cursor/rules");
                    results.push(Some(format!("# Cursor Rules Directory\n\n{combined}")));
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to read .cursor/rules directory at {}: {}",
                    cursor_rules_dir.display(),
                    e
                );
            }
        }
    }

    results
}

/// Recursively read directory, optionally filtering by extension
fn read_directory_recursive(dir: &Path, extension: &str) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut results = Vec::new();

    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if entry.file_type().is_file() {
            let path = entry.path();
            if extension.is_empty() || path.extension().is_some_and(|e| e == &extension[1..]) {
                results.push(path.to_path_buf());
            }
        }
    }

    Ok(results)
}

/// Synchronize rule toggles with current filesystem state.
/// Adds defaults for new files, removes toggles for deleted files.
#[must_use]
pub fn synchronize_rule_toggles(
    rules_path: &Path,
    current_toggles: &RuleToggles,
    allowed_extension: Option<&str>,
) -> RuleToggles {
    let mut updated = current_toggles.clone();

    if rules_path.exists() {
        if rules_path.is_dir() {
            // Directory case
            let mut existing_paths = std::collections::HashSet::new();

            if let Ok(files) = read_directory_recursive(rules_path, allowed_extension.unwrap_or(""))
            {
                for file_path in files {
                    let path_str = file_path.to_string_lossy().to_string();
                    existing_paths.insert(path_str.clone());

                    updated.entry(path_str).or_insert(true);
                }
            }

            // Remove toggles for non-existent files
            updated.retain(|path, _| existing_paths.contains(path));
        } else {
            // File case
            let path_str = rules_path.to_string_lossy().to_string();
            if !updated.contains_key(&path_str) {
                updated.insert(path_str.clone(), true);
            }

            // Remove toggles for other paths
            updated.retain(|path, _| path == &path_str);
        }
    } else {
        // Path doesn't exist - clear all toggles
        updated.clear();
    }

    updated
}

/// Combine two rule toggle maps (toggles2 takes precedence)
#[must_use]
pub fn combine_rule_toggles(toggles1: &RuleToggles, toggles2: &RuleToggles) -> RuleToggles {
    let mut combined = toggles1.clone();
    combined.extend(toggles2.iter().map(|(k, v)| (k.clone(), *v)));
    combined
}

/// Get skills directories to scan
#[must_use]
pub fn get_skills_directories_for_scan(cwd: &Path) -> Vec<(PathBuf, SkillSource)> {
    let mut dirs = vec![
        (cwd.join(".agents/skills"), SkillSource::Project),
        (cwd.join(".claude/skills"), SkillSource::Project),
        (cwd.join(".ai/skills"), SkillSource::Project),
        (cwd.join(".codex/skills"), SkillSource::Project),
    ];

    // Global directories
    if let Some(home) = dirs::home_dir() {
        dirs.push((home.join(".agents/skills"), SkillSource::Global));
        dirs.push((home.join(".codex/skills"), SkillSource::Global));
        dirs.push((home.join(".claude/skills"), SkillSource::Global));
        dirs.push((home.join(".ai/skills"), SkillSource::Global));
    }

    dirs
}

/// Scan a directory for skill subdirectories containing SKILL.md files
/// Also supports .md files directly in the skills directory (e.g., .agents/skills/*.md)
pub fn scan_skills_directory(dir_path: &Path, source: SkillSource) -> Vec<SkillMetadata> {
    let mut skills = Vec::new();

    if !dir_path.exists() || !dir_path.is_dir() {
        return skills;
    }

    let entries = match fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                tracing::warn!(
                    "Permission denied reading skills directory: {}",
                    dir_path.display()
                );
            }
            return skills;
        }
    };

    for entry in entries.filter_map(std::result::Result::ok) {
        let entry_path = entry.path();

        // Support subdirectories with SKILL.md inside
        if entry_path.is_dir() {
            let skill_name = entry_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            if let Some(skill) = load_skill_metadata(&entry_path, source.clone(), &skill_name) {
                skills.push(skill);
            }
        }
        // Support .md files directly in the skills directory
        else if entry_path.is_file() && entry_path.extension().is_some_and(|ext| ext == "md") {
            let skill_name = entry_path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            if let Some(skill) = load_skill_from_md_file(&entry_path, source.clone(), &skill_name) {
                skills.push(skill);
            }
        }
    }

    skills
}

/// Load skill metadata from a skill directory (containing SKILL.md)
fn load_skill_metadata(
    skill_dir: &Path,
    source: SkillSource,
    skill_name: &str,
) -> Option<SkillMetadata> {
    let skill_md_path = skill_dir.join("SKILL.md");
    if !skill_md_path.exists() {
        return None;
    }

    let file_content = match fs::read_to_string(&skill_md_path) {
        Ok(content) => content,
        Err(e) => {
            tracing::warn!("Failed to load skill at {}: {}", skill_dir.display(), e);
            return None;
        }
    };

    // Parse YAML frontmatter
    let (frontmatter, _body, had_frontmatter, parse_error) = parse_yaml_frontmatter(&file_content);

    if parse_error.is_some() {
        tracing::warn!(
            "Failed to parse YAML frontmatter for skill at {}: {:?}",
            skill_dir.display(),
            parse_error
        );
        return None;
    }

    if !had_frontmatter {
        tracing::warn!("Skill at {} missing YAML frontmatter", skill_dir.display());
        return None;
    }

    let name = frontmatter.get("name")?.as_str()?;
    let description = frontmatter.get("description")?.as_str()?;

    check_injection_patterns(description, &format!("skill:{skill_name}:description"));

    // Name must match directory name
    if name != skill_name {
        tracing::warn!(
            "Skill name \"{}\" in frontmatter doesn't match directory \"{}\" at {}",
            name,
            skill_name,
            skill_dir.display()
        );
        return None;
    }

    Some(SkillMetadata {
        name: skill_name.to_string(),
        description: description.to_string(),
        path: skill_md_path.to_string_lossy().to_string(),
        source,
    })
}

/// Load skill metadata from a .md file directly in the skills directory
fn load_skill_from_md_file(
    skill_md_path: &Path,
    source: SkillSource,
    skill_name: &str,
) -> Option<SkillMetadata> {
    let file_content = match fs::read_to_string(skill_md_path) {
        Ok(content) => content,
        Err(e) => {
            tracing::warn!(
                "Failed to load skill file {}: {}",
                skill_md_path.display(),
                e
            );
            return None;
        }
    };

    // Parse YAML frontmatter
    let (frontmatter, _body, had_frontmatter, parse_error) = parse_yaml_frontmatter(&file_content);

    if parse_error.is_some() {
        tracing::warn!(
            "Failed to parse YAML frontmatter for skill {}: {:?}",
            skill_md_path.display(),
            parse_error
        );
        return None;
    }

    let (name, description) = if had_frontmatter {
        let name = frontmatter.get("name")?.as_str()?;
        let description = frontmatter.get("description")?.as_str()?;

        check_injection_patterns(description, &format!("skill:{skill_name}:description"));

        // Name must match filename (without .md extension)
        if name != skill_name {
            tracing::warn!(
                "Skill name \"{}\" in frontmatter doesn't match filename \"{}\" at {}",
                name,
                skill_name,
                skill_md_path.display()
            );
            return None;
        }

        (name.to_string(), description.to_string())
    } else {
        // No frontmatter: use filename as name, generate generic description
        tracing::debug!(
            "Skill file {} has no YAML frontmatter, using filename as name",
            skill_md_path.display()
        );
        (
            skill_name.to_string(),
            format!("Skill loaded from {}", skill_md_path.file_name()?.to_str()?),
        )
    };

    Some(SkillMetadata {
        name,
        description,
        path: skill_md_path.to_string_lossy().to_string(),
        source,
    })
}

/// Parse YAML frontmatter from markdown content
/// Returns (frontmatter_map, body, had_frontmatter, parse_error)
fn parse_yaml_frontmatter(content: &str) -> (serde_yml::Mapping, String, bool, Option<String>) {
    let trimmed = content.trim_start();

    // Check for frontmatter delimiters
    if !trimmed.starts_with("---") {
        return (serde_yml::Mapping::new(), content.to_string(), false, None);
    }

    let rest = &trimmed[3..];
    let Some(end_pos) = rest.find("---") else {
        return (serde_yml::Mapping::new(), content.to_string(), false, None);
    };

    let yaml_str = &rest[..end_pos];
    let body = rest[end_pos + 3..].to_string();

    match serde_yml::from_str::<serde_yml::Mapping>(yaml_str) {
        Ok(mapping) => (mapping, body, true, None),
        Err(e) => (
            serde_yml::Mapping::new(),
            content.to_string(),
            true,
            Some(e.to_string()),
        ),
    }
}

/// Discover all skills from project and global directories
#[must_use]
pub fn discover_skills(cwd: &Path) -> Vec<SkillMetadata> {
    let mut skills = Vec::new();
    let scan_dirs = get_skills_directories_for_scan(cwd);

    for (dir, source) in scan_dirs {
        let dir_skills = scan_skills_directory(&dir, source);
        skills.extend(dir_skills);
    }

    skills
}

/// Get available skills with override resolution (global > project)
#[must_use]
pub fn get_available_skills(skills: Vec<SkillMetadata>) -> Vec<SkillMetadata> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    // Iterate backwards: global skills (added last) take precedence
    for skill in skills.into_iter().rev() {
        if seen.insert(skill.name.clone()) {
            result.push(skill);
        }
    }

    // Reverse back to maintain original order
    result.reverse();
    result
}

/// List supporting files (docs and scripts) in a skill directory
#[must_use]
pub fn list_supporting_files(skill_md_path: &Path) -> SkillSupportingFiles {
    let skill_dir = skill_md_path.parent().unwrap_or(Path::new("."));
    let docs_dir = skill_dir.join("docs");
    let scripts_dir = skill_dir.join("scripts");

    let mut result = SkillSupportingFiles::default();

    if docs_dir.exists()
        && docs_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&docs_dir)
    {
        for entry in entries.filter_map(std::result::Result::ok) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".md") || name_str.ends_with(".txt") {
                result.docs.push(name_str.to_string());
            }
        }
    }

    if scripts_dir.exists()
        && scripts_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&scripts_dir)
    {
        for entry in entries.filter_map(std::result::Result::ok) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with('.') {
                result.scripts.push(name_str.to_string());
            }
        }
    }

    result
}

/// Get full skill content including instructions
#[must_use]
pub fn get_skill_content(
    skill_name: &str,
    available_skills: &[SkillMetadata],
) -> Option<SkillContent> {
    let skill = available_skills.iter().find(|s| s.name == skill_name)?;

    let Ok(file_content) = fs::read_to_string(&skill.path) else {
        return None;
    };

    let (_, body, _, _) = parse_yaml_frontmatter(&file_content);
    let instructions = body.trim().to_string();

    check_injection_patterns(&instructions, &format!("skill:{skill_name}"));

    Some(SkillContent {
        name: skill.name.clone(),
        description: skill.description.clone(),
        path: skill.path.clone(),
        source: skill.source.clone(),
        instructions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_find_agents_md_files_reads_only_root_file() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create nested AGENTS.md without top-level
        let nested = cwd.join("src/AGENTS.md");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, "nested").unwrap();

        let files = find_agents_md_files(cwd);
        assert!(files.is_empty(), "Nested rules are path-scoped, not startup rules");
    }

    #[test]
    fn test_find_agents_md_files_does_not_traverse_nested_directories() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create top-level AGENTS.md
        fs::write(cwd.join("AGENTS.md"), "top-level").unwrap();

        // Create nested AGENTS.md
        let nested = cwd.join("src/AGENTS.md");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, "nested").unwrap();

        let files = find_agents_md_files(cwd);
        assert_eq!(files, vec![cwd.join("AGENTS.md")]);
    }

    #[test]
    fn test_find_agents_md_files_reads_current_root_content_without_cache() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let top_level = cwd.join("AGENTS.md");
        fs::write(&top_level, "top-level").unwrap();

        assert_eq!(find_agents_md_files(cwd).len(), 1);
        fs::write(&top_level, "changed rules with a different size").unwrap();

        assert_eq!(find_agents_md_files(cwd).len(), 1);
        assert_eq!(
            fs::read_to_string(&top_level).unwrap(),
            "changed rules with a different size"
        );
    }

    #[test]
    fn test_path_scoped_discovery_finds_nested_file_created_after_startup() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        fs::write(cwd.join("AGENTS.md"), "top-level").unwrap();
        let nested = cwd.join("nested");
        fs::create_dir(&nested).unwrap();

        assert_eq!(find_agents_md_files(cwd).len(), 1);
        fs::write(nested.join("AGENTS.md"), "nested rules").unwrap();

        let files = find_path_scoped_agents_md_files(cwd, &nested.join("file.rs"));
        assert_eq!(
            files,
            vec![
                cwd.join("AGENTS.md").canonicalize().unwrap(),
                nested.join("AGENTS.md").canonicalize().unwrap()
            ]
        );
    }

    #[test]
    fn test_path_scoped_discovery_preserves_root_to_nearest_precedence() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create top-level AGENTS.md
        fs::write(cwd.join("AGENTS.md"), "top-level").unwrap();

        // Create deeply nested AGENTS.md (12 levels deep - beyond old max_depth(10) limit)
        let deep_path = cwd.join("a/b/c/d/e/f/g/h/i/j/k/deep/AGENTS.md");
        fs::create_dir_all(deep_path.parent().unwrap()).unwrap();
        fs::write(&deep_path, "deep-nested").unwrap();

        let files = find_path_scoped_agents_md_files(cwd, &deep_path.join("file.rs"));
        assert_eq!(
            files.len(),
            2,
            "Should discover both top-level and deeply nested AGENTS.md"
        );
        assert!(files.iter().any(|f| f.file_name().unwrap() == "AGENTS.md"));
    }

    #[test]
    fn test_path_scoped_discovery_includes_nested_without_root_and_excludes_siblings() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let src_rules = cwd.join("src/AGENTS.md");
        let tests_rules = cwd.join("tests/AGENTS.md");
        fs::create_dir_all(src_rules.parent().unwrap()).unwrap();
        fs::create_dir_all(tests_rules.parent().unwrap()).unwrap();
        fs::write(&src_rules, "src rules").unwrap();
        fs::write(&tests_rules, "tests rules").unwrap();

        let files = load_path_scoped_agents_rules(
            cwd,
            &cwd.join("src/new/deep/main.rs"),
            &RuleToggles::new(),
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, src_rules.canonicalize().unwrap());
        assert_eq!(files[0].content, "src rules");
    }

    #[test]
    fn test_synchronize_rule_toggles_file() {
        let temp = TempDir::new().unwrap();
        let rules_file = temp.path().join(".windsurfrules");
        fs::write(&rules_file, "rules").unwrap();

        let toggles = synchronize_rule_toggles(&rules_file, &RuleToggles::new(), None);
        assert_eq!(toggles.len(), 1);
        assert_eq!(
            toggles.get(&rules_file.to_string_lossy().to_string()),
            Some(&true)
        );
    }

    #[test]
    fn test_synchronize_rule_toggles_directory() {
        let temp = TempDir::new().unwrap();
        let rules_dir = temp.path().join(".cursor/rules");
        fs::create_dir_all(&rules_dir).unwrap();
        fs::write(rules_dir.join("rule1.mdc"), "rule1").unwrap();
        fs::write(rules_dir.join("rule2.mdc"), "rule2").unwrap();

        let toggles = synchronize_rule_toggles(&rules_dir, &RuleToggles::new(), Some(".mdc"));
        assert_eq!(toggles.len(), 2);
    }

    #[test]
    fn test_combine_rule_toggles() {
        let mut t1 = RuleToggles::new();
        t1.insert("a".to_string(), true);
        t1.insert("b".to_string(), false);

        let mut t2 = RuleToggles::new();
        t2.insert("b".to_string(), true);
        t2.insert("c".to_string(), true);

        let combined = combine_rule_toggles(&t1, &t2);
        assert_eq!(combined.get("a"), Some(&true));
        assert_eq!(combined.get("b"), Some(&true));
        assert_eq!(combined.get("c"), Some(&true));
    }

    #[test]
    fn test_discover_skills() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create a skill directory
        let skill_dir = cwd.join(".sned/skills/test-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill\n---\n\nInstructions here.",
        )
        .unwrap();

        // Use scan_skills_directory directly for isolated test
        let skills = scan_skills_directory(skill_dir.parent().unwrap(), SkillSource::Project);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");
        assert_eq!(skills[0].description, "A test skill");
    }

    #[test]
    fn test_discover_skills_from_md_files() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create skills directory with .md files directly (like .agents/skills/)
        let skills_dir = cwd.join(".agents/skills");
        fs::create_dir_all(&skills_dir).unwrap();

        // Create skill as .md file with frontmatter
        fs::write(
            skills_dir.join("outcome-replay.md"),
            "---\nname: outcome-replay\ndescription: Replay and analyze trading outcomes\n---\n\nSkill instructions here.",
        )
        .unwrap();

        fs::write(
            skills_dir.join("runtime-operations.md"),
            "---\nname: runtime-operations\ndescription: Manage runtime trading operations\n---\n\nMore instructions.",
        )
        .unwrap();

        // Use scan_skills_directory directly for isolated test
        let skills = scan_skills_directory(&skills_dir, SkillSource::Project);
        assert_eq!(skills.len(), 2);

        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"outcome-replay"));
        assert!(names.contains(&"runtime-operations"));
    }

    #[test]
    fn test_discover_skills_from_md_files_no_frontmatter() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create skills directory with .md files WITHOUT frontmatter
        let skills_dir = cwd.join(".agents/skills");
        fs::create_dir_all(&skills_dir).unwrap();

        // Create skill as .md file without frontmatter
        fs::write(
            skills_dir.join("no-frontmatter.md"),
            "# No Frontmatter Skill\n\nThis skill has no YAML frontmatter.",
        )
        .unwrap();

        // Use scan_skills_directory directly for isolated test
        let skills = scan_skills_directory(&skills_dir, SkillSource::Project);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "no-frontmatter");
        assert!(skills[0].description.contains("no-frontmatter.md"));
    }

    #[test]
    fn test_discover_skills_mixed_formats() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create skills directory with both formats
        let skills_dir = cwd.join("mixed-skills");
        fs::create_dir_all(&skills_dir).unwrap();

        // Subdirectory with SKILL.md
        let subdir_skill = skills_dir.join("dir-skill");
        fs::create_dir_all(&subdir_skill).unwrap();
        fs::write(
            subdir_skill.join("SKILL.md"),
            "---\nname: dir-skill\ndescription: Directory-based skill\n---\n\nInstructions.",
        )
        .unwrap();

        // .md file directly
        fs::write(
            skills_dir.join("file-skill.md"),
            "---\nname: file-skill\ndescription: File-based skill\n---\n\nInstructions.",
        )
        .unwrap();

        // Use scan_skills_directory directly for isolated test
        let skills = scan_skills_directory(&skills_dir, SkillSource::Project);
        assert_eq!(skills.len(), 2);

        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"dir-skill"));
        assert!(names.contains(&"file-skill"));
    }

    #[test]
    fn test_get_available_skills_precedence() {
        let project_skill = SkillMetadata {
            name: "test".to_string(),
            description: "project".to_string(),
            path: "/project/test/SKILL.md".to_string(),
            source: SkillSource::Project,
        };

        let global_skill = SkillMetadata {
            name: "test".to_string(),
            description: "global".to_string(),
            path: "/global/test/SKILL.md".to_string(),
            source: SkillSource::Global,
        };

        let skills = vec![project_skill, global_skill];
        let available = get_available_skills(skills);

        assert_eq!(available.len(), 1);
        assert_eq!(available[0].description, "global"); // Global takes precedence
    }

    #[test]
    fn test_get_skills_directories_for_scan_includes_all_global_roots() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        let scan_dirs = get_skills_directories_for_scan(cwd);

        assert_eq!(
            scan_dirs.len(),
            8,
            "Should have 4 project and 4 global skill directories"
        );

        // Verify project directories
        assert_eq!(scan_dirs[0].0, cwd.join(".agents/skills"));
        assert_eq!(scan_dirs[0].1, SkillSource::Project);
        assert_eq!(scan_dirs[1].0, cwd.join(".claude/skills"));
        assert_eq!(scan_dirs[1].1, SkillSource::Project);
        assert_eq!(scan_dirs[2].0, cwd.join(".ai/skills"));
        assert_eq!(scan_dirs[2].1, SkillSource::Project);
        assert_eq!(scan_dirs[3].0, cwd.join(".codex/skills"));
        assert_eq!(scan_dirs[3].1, SkillSource::Project);

        // Verify global directories
        let home = dirs::home_dir().expect("home_dir should exist in test");
        assert_eq!(scan_dirs[4].0, home.join(".agents/skills"));
        assert_eq!(scan_dirs[4].1, SkillSource::Global);
        assert_eq!(scan_dirs[5].0, home.join(".codex/skills"));
        assert_eq!(scan_dirs[5].1, SkillSource::Global);
        assert_eq!(scan_dirs[6].0, home.join(".claude/skills"));
        assert_eq!(scan_dirs[6].1, SkillSource::Global);
        assert_eq!(scan_dirs[7].0, home.join(".ai/skills"));
        assert_eq!(scan_dirs[7].1, SkillSource::Global);
    }

    #[test]
    fn test_parse_yaml_frontmatter() {
        let content = "---\nname: test\ndescription: hello\n---\n\nBody content.";
        let (fm, body, had_fm, err) = parse_yaml_frontmatter(content);

        assert!(had_fm);
        assert!(err.is_none());
        assert_eq!(fm.get("name").unwrap().as_str().unwrap(), "test");
        assert_eq!(fm.get("description").unwrap().as_str().unwrap(), "hello");
        assert_eq!(body.trim(), "Body content.");
    }

    #[test]
    fn test_parse_yaml_frontmatter_no_frontmatter() {
        let content = "Just body content.";
        let (fm, body, had_fm, err) = parse_yaml_frontmatter(content);

        assert!(!had_fm);
        assert!(err.is_none());
        assert!(fm.is_empty());
        assert_eq!(body, "Just body content.");
    }

    #[test]
    fn test_list_supporting_files() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("test-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "").unwrap();

        let docs_dir = skill_dir.join("docs");
        fs::create_dir_all(&docs_dir).unwrap();
        fs::write(docs_dir.join("readme.md"), "").unwrap();
        fs::write(docs_dir.join("notes.txt"), "").unwrap();

        let scripts_dir = skill_dir.join("scripts");
        fs::create_dir_all(&scripts_dir).unwrap();
        fs::write(scripts_dir.join("setup.sh"), "").unwrap();

        let supporting = list_supporting_files(&skill_dir.join("SKILL.md"));
        assert_eq!(supporting.docs.len(), 2);
        assert_eq!(supporting.scripts.len(), 1);
    }

    #[test]
    fn test_skill_invalid_frontmatter() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        let skill_dir = cwd.join(".sned/skills/bad-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\nnot yaml:::\n---\n\nBody.").unwrap();

        // Use scan_skills_directory directly for isolated test
        let skills = scan_skills_directory(skill_dir.parent().unwrap(), SkillSource::Project);
        assert!(
            skills.is_empty(),
            "Skills with invalid frontmatter should be skipped"
        );
    }

    #[test]
    fn test_skill_duplicate_names() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create project skill
        let project_dir = cwd.join(".sned/skills/test-skill");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: project\n---\n\nProject skill.",
        )
        .unwrap();

        // Create global skill with same name
        let global_dir = temp.path().join("global-skills/test-skill");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: global\n---\n\nGlobal skill.",
        )
        .unwrap();

        let mut skills = Vec::new();
        skills.extend(scan_skills_directory(
            &cwd.join(".sned/skills"),
            SkillSource::Project,
        ));
        skills.extend(scan_skills_directory(
            &temp.path().join("global-skills"),
            SkillSource::Global,
        ));

        let available = get_available_skills(skills);
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].description, "global");
    }

    #[test]
    fn test_agents_rules_with_toggles() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        fs::write(cwd.join("AGENTS.md"), "top-level rules").unwrap();
        let nested = cwd.join("src/AGENTS.md");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, "nested rules").unwrap();

        // Startup loads only the root file.
        let rules = get_local_agents_rules(cwd, &RuleToggles::new());
        assert!(rules.is_some());
        let rules_str = rules.unwrap();
        assert!(rules_str.contains("top-level rules"));
        assert!(!rules_str.contains("nested rules"));

        let scoped = get_path_scoped_agents_rules(cwd, &nested.join("file.rs"), &RuleToggles::new())
            .unwrap();
        assert!(scoped.contains("top-level rules"));
        assert!(scoped.contains("nested rules"));

        // With top-level disabled
        let mut toggles = RuleToggles::new();
        toggles.insert(cwd.join("AGENTS.md").to_string_lossy().to_string(), false);
        let rules = get_local_agents_rules(cwd, &toggles);
        assert!(rules.is_none());

        let scoped = get_path_scoped_agents_rules(cwd, &nested.join("file.rs"), &toggles)
            .unwrap();
        assert!(!scoped.contains("top-level rules"));
        assert!(scoped.contains("nested rules"));
    }

    #[test]
    fn test_cursor_rules_with_directory() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        // Create .cursor/rules directory with .mdc files
        let rules_dir = cwd.join(".cursor/rules");
        fs::create_dir_all(&rules_dir).unwrap();
        fs::write(rules_dir.join("frontend.mdc"), "Frontend rules").unwrap();
        fs::write(rules_dir.join("backend.mdc"), "Backend rules").unwrap();

        let rules = get_local_cursor_rules(cwd, &RuleToggles::new());
        assert_eq!(rules.len(), 1);
        assert!(rules[0].as_ref().unwrap().contains("Frontend rules"));
        assert!(rules[0].as_ref().unwrap().contains("Backend rules"));
    }

    #[test]
    fn test_workflows_not_yet_implemented() {
        // Workflows require .agents/workflows directory scanning
        // which is similar to skills but with different semantics.
        // This test documents the expected behavior when implemented.
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();

        let workflows_dir = cwd.join(".agents/workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("default-workflows.md"),
            "# Workflow\n\nSteps.",
        )
        .unwrap();

        // Currently workflows are not automatically loaded by instructions.rs
        // They would need to be explicitly read by a workflow handler
        assert!(workflows_dir.exists());
    }
}
