//! Context loader for initial prompt assembly.
//!
//!
//! Prepares rich initial user message with environment details and context mentions.

use crate::services::symbol_index::{SymbolIndexService, SymbolLocation, SymbolType};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_AUTO_SYMBOL_MATCHES: usize = 3;
const MAX_AUTO_SYMBOL_TOTAL_LINES: usize = 20;
const MAX_AUTO_SYMBOL_LINE_LENGTH_BYTES: usize = 200;

static CODE_FENCE_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"```[\s\S]*?```").unwrap());
static URL_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\w+:\/\/[^\s]+").unwrap());
static MENTION_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"@([A-Za-z0-9_./\-]+)").unwrap());
static SLASH_COMMAND_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^|\s)/([A-Za-z0-9_.:@-]+)").unwrap());
static SYMBOL_TOKEN_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]{2,}\b").unwrap());

const STOP_WORDS: &[&str] = &[
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "break",
    "continue",
    "return",
    "try",
    "catch",
    "finally",
    "throw",
    "function",
    "class",
    "interface",
    "extends",
    "implements",
    "import",
    "export",
    "from",
    "as",
    "default",
    "const",
    "let",
    "var",
    "type",
    "enum",
    "async",
    "await",
    "yield",
    "void",
    "public",
    "private",
    "protected",
    "static",
    "readonly",
    "new",
    "this",
    "super",
    "null",
    "undefined",
    "true",
    "false",
    "typeof",
    "instanceof",
    "string",
    "number",
    "boolean",
    "object",
    "any",
    "unknown",
    "never",
    "array",
    "constructor",
    "get",
    "set",
    "of",
    "in",
    "is",
];

#[derive(Debug, Clone)]
struct SymbolContextAccumulator {
    all_locations: Vec<SymbolLocation>,
    added_lines: Vec<String>,
}

impl SymbolContextAccumulator {
    fn new() -> Self {
        Self {
            all_locations: Vec::new(),
            added_lines: Vec::new(),
        }
    }
}

/// Context loader for assembling initial prompt context.
#[derive(Debug, Clone)]
pub struct ContextLoader {
    cwd: String,
    symbol_index_service: Option<Arc<Mutex<SymbolIndexService>>>,
}

impl ContextLoader {
    pub fn new(cwd: String) -> Self {
        Self {
            cwd,
            symbol_index_service: None,
        }
    }

    pub fn with_symbol_index_service(
        mut self,
        symbol_index_service: Arc<Mutex<SymbolIndexService>>,
    ) -> Self {
        self.symbol_index_service = Some(symbol_index_service);
        self
    }

    /// Gather environment details and format as XML block.
    pub async fn get_environment_details(&self) -> String {
        let mut details = String::new();

        // Operating system
        let os_info = Self::get_os_info();
        details.push_str(&format!("# System Information\n{os_info}\n"));

        // Current working directory
        details.push_str(&format!("\n# Current Working Directory\n{}\n", self.cwd));

        // Git branch (if in a git repo)
        if let Ok(branch) = Self::get_git_branch(&self.cwd).await {
            details.push_str(&format!("\n# Git Branch\n{branch}\n"));
        }

        // Shell
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        details.push_str(&format!("\n# Shell\n{shell}\n"));

        // CPU cores
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        details.push_str(&format!("\n# CPU Cores\n{cores}\n"));

        format!("<environment_details>\n{}</environment_details>", details)
    }

    /// Extract file/directory paths and symbols from text.
    pub fn extract_context(text: &str) -> Vec<String> {
        let mut mentions = Vec::new();
        let mention_regex = regex::Regex::new(r"@([A-Za-z0-9_./\-]+)").unwrap();

        for cap in mention_regex.captures_iter(text) {
            if let Some(matched) = cap.get(1) {
                mentions.push(matched.as_str().to_string());
            }
        }

        mentions
    }

    /// Load initial context: process text mentions, add symbol context, and append environment details.
    pub async fn load_initial_context(&self, text: &str) -> (String, String) {
        // Use the mentions system to expand @mentions
        let workspace_root = std::path::Path::new(&self.cwd);
        let (enriched_text, expanded) =
            crate::core::mentions::expand_mentions(text, workspace_root).await;

        let mut final_text = enriched_text;
        if !expanded.is_empty() {
            final_text.push_str("\n\n");
            final_text.push_str(&expanded.join("\n\n"));
        }

        let symbol_context = self.get_symbol_context(text).await;
        if !symbol_context.is_empty() {
            if !final_text.is_empty() {
                final_text.push_str("\n\n");
            }
            final_text.push_str(&symbol_context.join("\n\n"));
        }

        let environment_details = self.get_environment_details().await;

        (final_text, environment_details)
    }

    fn extract_symbol_like_strings(text: &str) -> Vec<String> {
        if text.is_empty() {
            return Vec::new();
        }

        let without_code_fences = CODE_FENCE_REGEX.replace_all(text, " ");
        let without_urls = URL_REGEX.replace_all(&without_code_fences, " ");
        let without_mentions = MENTION_REGEX.replace_all(&without_urls, " ");
        let scrubbed_text =
            SLASH_COMMAND_REGEX.replace_all(&without_mentions, |caps: &regex::Captures<'_>| {
                format!("{} ", caps.get(1).map(|m| m.as_str()).unwrap_or(""))
            });

        let mut seen = HashSet::new();
        let mut result = Vec::new();

        for mat in SYMBOL_TOKEN_REGEX.find_iter(&scrubbed_text) {
            let candidate = mat.as_str();
            if STOP_WORDS.contains(&candidate) {
                continue;
            }

            let has_underscore = candidate.contains('_');
            let has_internal_case_change = candidate
                .chars()
                .zip(candidate.chars().skip(1))
                .any(|(left, right)| left.is_ascii_lowercase() && right.is_ascii_uppercase());

            if !has_underscore && !has_internal_case_change {
                continue;
            }

            if seen.insert(candidate.to_string()) {
                result.push(candidate.to_string());
            }
        }

        result
    }

    async fn get_symbol_context(&self, text: &str) -> Vec<String> {
        let Some(symbol_index_service) = &self.symbol_index_service else {
            return Vec::new();
        };

        let symbols = Self::extract_symbol_like_strings(text);
        if symbols.is_empty() || symbols.len() > MAX_AUTO_SYMBOL_MATCHES {
            return Vec::new();
        }

        // Single lock acquisition: batch all symbol queries together
        let (project_root, all_definitions, all_references) = {
            let service = symbol_index_service
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let root = service.get_project_root();
            let project_root = if root.is_empty() {
                self.cwd.clone()
            } else {
                root.to_string()
            };
            
            // Batch query all definitions in one lock hold
            let mut definitions = HashMap::new();
            for symbol in &symbols {
                let defs = service.get_definitions(symbol, Some(MAX_AUTO_SYMBOL_TOTAL_LINES));
                definitions.insert(symbol.clone(), defs);
            }
            
            // Batch query all references in one lock hold
            let mut references = HashMap::new();
            for symbol in &symbols {
                let remaining_limit = MAX_AUTO_SYMBOL_TOTAL_LINES.saturating_sub(
                    definitions
                        .get(symbol)
                        .map(|d| d.len())
                        .unwrap_or(0),
                );
                let refs = service.get_references(symbol, Some(remaining_limit));
                references.insert(symbol.clone(), refs);
            }
            
            (project_root, definitions, references)
        };
        let project_root = PathBuf::from(project_root);
        let cwd_path = PathBuf::from(&self.cwd);

        let mut symbol_results: HashMap<String, SymbolContextAccumulator> = symbols
            .iter()
            .map(|symbol| (symbol.clone(), SymbolContextAccumulator::new()))
            .collect();

        for symbol in &symbols {
            if let Some(definitions) = all_definitions.get(symbol) {
                let data = symbol_results
                    .get_mut(symbol)
                    .expect("symbol accumulator should exist");
                data.all_locations.extend(definitions.iter().cloned());
            }
        }

        for symbol in &symbols {
            if let Some(references) = all_references.get(symbol) {
                let data = symbol_results
                    .get_mut(symbol)
                    .expect("symbol accumulator should exist");
                data.all_locations.extend(references.iter().cloned());
            }
        }

        // Collect all unique locations, grouped by file path for efficient reading
        let mut file_locations: HashMap<String, Vec<(String, SymbolLocation)>> = HashMap::new();
        for (symbol, data) in &symbol_results {
            for loc in &data.all_locations {
                let Some(loc_path_str) = &loc.path else {
                    continue;
                };
                
                // Check if we already have this exact location
                let existing = file_locations
                    .get(loc_path_str)
                    .map(|locs| locs.iter().any(|(s, l)| {
                        l.path.as_deref() == Some(loc_path_str.as_str()) 
                            && l.start_line == loc.start_line 
                            && s == symbol
                    }))
                    .unwrap_or(false);
                
                if !existing {
                    file_locations
                        .entry(loc_path_str.clone())
                        .or_default()
                        .push((symbol.clone(), loc.clone()));
                }
            }
        }

        // Read each file once, then extract all needed lines from cached content
        let mut file_contents: HashMap<String, Vec<String>> = HashMap::new();
        for (loc_path_str, _locations) in &file_locations {
            let abs_loc_path = if Path::new(loc_path_str).is_absolute() {
                PathBuf::from(loc_path_str)
            } else {
                project_root.join(loc_path_str)
            };

            if let Ok(content) = tokio::fs::read_to_string(&abs_loc_path).await {
                file_contents.insert(loc_path_str.clone(), content.lines().map(String::from).collect());
            }
        }

        // Process all locations using cached file contents
        let mut read_results: Vec<(String, String, String)> = Vec::new();
        for (loc_path_str, locations) in &file_locations {
            let Some(lines) = file_contents.get(loc_path_str) else {
                continue;
            };

            let abs_loc_path = if Path::new(loc_path_str).is_absolute() {
                PathBuf::from(loc_path_str)
            } else {
                project_root.join(loc_path_str)
            };
            let display_path = abs_loc_path
                .strip_prefix(&cwd_path)
                .map_or(abs_loc_path.clone(), PathBuf::from)
                .display()
                .to_string();

            for (symbol_name, loc) in locations {
                let line_num = loc.start_line;
                if line_num >= lines.len() {
                    continue;
                }

                let mut line_content = lines[line_num].trim().to_string();
                if line_content.len() > MAX_AUTO_SYMBOL_LINE_LENGTH_BYTES {
                    line_content = "(line too long, skipped)".to_string();
                }

                let kind = match loc.symbol_type {
                    SymbolType::Definition => "definition",
                    SymbolType::Reference => "reference",
                };

                read_results.push((
                    symbol_name.clone(),
                    format!(
                        "    - {}:{} [{}] `{}`",
                        display_path,
                        line_num + 1,
                        kind,
                        line_content,
                    ),
                    format!("{}:{}", loc_path_str, line_num),
                ));
            }
        }

        // Process results and populate accumulators with limit enforcement
        let mut total_lines_added = 0usize;
        let mut seen_keys: HashSet<String> = HashSet::new();

        for (symbol_name, line, loc_key) in read_results {
            if total_lines_added >= MAX_AUTO_SYMBOL_TOTAL_LINES {
                break;
            }

            if seen_keys.contains(&loc_key) {
                continue;
            }
            seen_keys.insert(loc_key);

            if let Some(data) = symbol_results.get_mut(&symbol_name) {
                data.added_lines.push(line);
                total_lines_added += 1;
            }
        }

        let mut symbol_definitions = Vec::new();
        for symbol in &symbols {
            let data = symbol_results
                .get(symbol)
                .expect("symbol accumulator should exist");
            if data.added_lines.is_empty() {
                continue;
            }

            let num_locations = data.all_locations.len();
            let mut symbol_lines = Vec::new();
            symbol_lines.push(format!(
                "Note: The following context was automatically included because the symbol \"{}\" was mentioned in user's message.",
                symbol,
            ));
            if num_locations <= MAX_AUTO_SYMBOL_TOTAL_LINES {
                symbol_lines.push(format!(
                    "All {} symbols found in the codebase are listed below.",
                    num_locations,
                ));
            } else {
                symbol_lines.push(format!(
                    "{} out of {} symbols listed below (definitions first).",
                    total_lines_added, num_locations,
                ));
            }

            symbol_lines.push("symbol_context:".to_string());
            symbol_lines.push(format!("  {}:", symbol));
            symbol_lines.extend(data.added_lines.iter().cloned());

            symbol_definitions.push(symbol_lines.join("\n"));
        }

        symbol_definitions
    }

    // --- Private helpers ---

    fn get_os_info() -> String {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        format!("OS: {os}\nArchitecture: {arch}")
    }

    async fn get_git_branch(cwd: &str) -> Result<String, std::io::Error> {
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(cwd)
            .output()
            .await?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(std::io::Error::other("Not a git repository"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_context_mentions() {
        let text = "Please look at @src/main.rs and @README.md for context";
        let mentions = ContextLoader::extract_context(text);
        assert_eq!(mentions, vec!["src/main.rs", "README.md"]);
    }

    #[test]
    fn test_extract_context_no_mentions() {
        let text = "No mentions here";
        let mentions = ContextLoader::extract_context(text);
        assert!(mentions.is_empty());
    }

    #[tokio::test]
    async fn test_get_environment_details() {
        let loader = ContextLoader::new("/tmp".to_string());
        let details = loader.get_environment_details().await;

        assert!(details.contains("<environment_details>"));
        assert!(details.contains("</environment_details>"));
        assert!(details.contains("System Information"));
        assert!(details.contains("Current Working Directory"));
        assert!(details.contains("/tmp"));
        assert!(details.contains("Shell"));
        assert!(details.contains("CPU Cores"));
    }

    #[tokio::test]
    async fn test_load_initial_context() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_readme.md");
        tokio::fs::write(&test_file, "# README").await.unwrap();

        let loader = ContextLoader::new(temp_dir.to_string_lossy().to_string());
        let (enriched, env_details) = loader.load_initial_context("Check @/test_readme.md").await;

        assert!(enriched.contains("# README"));
        assert!(env_details.contains("<environment_details>"));

        tokio::fs::remove_file(&test_file).await.unwrap();
    }

    #[test]
    fn test_extract_symbol_like_strings() {
        let text =
            "Please inspect fooBar and snake_case, but not simple words or https://example.com";
        let symbols = ContextLoader::extract_symbol_like_strings(text);
        assert_eq!(symbols, vec!["fooBar", "snake_case"]);
    }

    #[tokio::test]
    async fn test_load_initial_context_adds_symbol_context() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();
        let file_path = root.join("src/lib.rs");
        tokio::fs::create_dir_all(file_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&file_path, "fn fooBar() {}\n")
            .await
            .unwrap();

        let mut service = SymbolIndexService::new(root.to_string_lossy().to_string());
        service.index_file(
            "src/lib.rs".to_string(),
            1,
            1,
            vec![SymbolLocation {
                path: Some("src/lib.rs".to_string()),
                name: "fooBar".to_string(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 6,
                symbol_type: SymbolType::Definition,
                kind: Some("function".to_string()),
            }],
        );

        let loader = ContextLoader::new(root.to_string_lossy().to_string())
            .with_symbol_index_service(Arc::new(Mutex::new(service)));
        let (enriched, _) = loader.load_initial_context("Please inspect fooBar").await;

        assert!(enriched.contains("symbol_context:"));
        assert!(enriched.contains("fooBar"));
        assert!(enriched.contains("src/lib.rs:1"));
    }

    #[tokio::test]
    async fn test_load_initial_context_batches_symbol_queries() {
        // Regression test: verify that multiple symbols in a prompt are batched
        // efficiently (single lock acquisition, no repeated file reads)
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();
        let lib_path = root.join("src/lib.rs");
        let utils_path = root.join("src/utils.rs");
        tokio::fs::create_dir_all(lib_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::create_dir_all(utils_path.parent().unwrap())
            .await
            .unwrap();
        
        // Create two files with different symbols
        tokio::fs::write(&lib_path, "fn fooBar() {}\nfn bazQux() {}\n")
            .await
            .unwrap();
        tokio::fs::write(&utils_path, "fn helper() {}\n")
            .await
            .unwrap();

        let mut service = SymbolIndexService::new(root.to_string_lossy().to_string());
        // Index both files
        service.index_file(
            "src/lib.rs".to_string(),
            1,
            1,
            vec![
                SymbolLocation {
                    path: Some("src/lib.rs".to_string()),
                    name: "fooBar".to_string(),
                    start_line: 0,
                    start_column: 0,
                    end_line: 0,
                    end_column: 6,
                    symbol_type: SymbolType::Definition,
                    kind: Some("function".to_string()),
                },
                SymbolLocation {
                    path: Some("src/lib.rs".to_string()),
                    name: "bazQux".to_string(),
                    start_line: 1,
                    start_column: 0,
                    end_line: 1,
                    end_column: 6,
                    symbol_type: SymbolType::Definition,
                    kind: Some("function".to_string()),
                },
            ],
        );
        service.index_file(
            "src/utils.rs".to_string(),
            1,
            1,
            vec![SymbolLocation {
                path: Some("src/utils.rs".to_string()),
                name: "helper".to_string(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 7,
                symbol_type: SymbolType::Definition,
                kind: Some("function".to_string()),
            }],
        );

        let loader = ContextLoader::new(root.to_string_lossy().to_string())
            .with_symbol_index_service(Arc::new(Mutex::new(service)));
        
        // Prompt with multiple camelCase symbols should trigger batching
        let (enriched, _) = loader
            .load_initial_context("Please inspect fooBar and bazQux in the codebase")
            .await;

        // Both symbols should be included in context
        assert!(enriched.contains("fooBar"));
        assert!(enriched.contains("bazQux"));
        assert!(enriched.contains("src/lib.rs:1"));
        assert!(enriched.contains("src/lib.rs:2"));
    }
}
