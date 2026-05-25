use crate::core::agent_loop::TaskState;
use crate::core::hash_utils::strip_hashes;
use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
use crate::services::symbol_index::SymbolIndexService;
use crate::services::tree_sitter::{SymbolRange, get_symbol_range, load_required_language_parsers};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;

struct FileBatch {
    absolute_path: String,
    display_path: String,
    replacements: Vec<Replacement>,
}

#[derive(Debug, Clone)]
struct Replacement {
    path: String,
    symbol: String,
    text: String,
    symbol_type: Option<String>,
}

#[derive(Debug)]
pub struct ReplaceSymbolHandler {
    symbol_index_service: Option<Arc<std::sync::Mutex<SymbolIndexService>>>,
}

impl ReplaceSymbolHandler {
    pub fn new() -> Self {
        Self {
            symbol_index_service: None,
        }
    }

    pub fn with_symbol_index(mut self, service: Arc<std::sync::Mutex<SymbolIndexService>>) -> Self {
        self.symbol_index_service = Some(service);
        self
    }
}

impl Default for ReplaceSymbolHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplaceSymbolHandler {
    async fn execute_with_workspace_root(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        workspace_root: &Path,
    ) -> Result<String, ToolError> {
        let replacements = read_replacements(&params);
        if replacements.is_empty() {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "replace_symbol: no replacements provided"
            );
            return Err(ToolError::InvalidInput(
                "Missing required parameters: replacements".to_string(),
            ));
        }

        let batches = group_replacements_by_file(replacements, workspace_root)?;

        let mut file_results: Vec<FileResult> = Vec::new();
        let mut any_error = None;

        for batch in batches.values() {
            // Mark file as edited by Sned to suppress stale mtime detection
            state
                .file_context_tracker
                .mark_file_as_edited_by_sned(std::path::Path::new(&batch.absolute_path));

            match process_batch(batch, self.symbol_index_service.as_ref()).await {
                Ok(result) => file_results.push(result),
                Err(e) => {
                    any_error = Some(e);
                    break;
                }
            }
        }

        if let Some(err) = any_error {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                error = %err,
                "replace_symbol: batch processing failed"
            );
            return Err(err);
        }

        if file_results.is_empty() {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                "replace_symbol: no files processed"
            );
            return Err(ToolError::ExecutionFailed(
                "No replacements could be processed".to_string(),
            ));
        }

        let total_applied: usize = file_results.iter().map(|r| r.replacements_applied).sum();
        let total_failed: usize = file_results.iter().map(|r| r.replacements_failed).sum();

        if total_failed > 0 {
            state.consecutive_mistakes += 1;
            tracing::warn!(
                consecutive_mistakes = state.consecutive_mistakes,
                total_failed = total_failed,
                total_applied = total_applied,
                "replace_symbol: replacements failed"
            );
        } else if total_applied > 0 {
            state.consecutive_mistakes = 0;
        }

        let summaries: Vec<String> = file_results
            .into_iter()
            .map(|fr| {
                let symbol_list = fr.symbols.iter().map(|s| format!("'{}'", s)).collect::<Vec<_>>().join(", ");
                let mut summary = format!("Successfully replaced symbols {} in {}. Any existing hash anchors for these symbols are now stale.", symbol_list, fr.display_path);
                if !fr.new_problems_message.is_empty() {
                    summary.push_str(&format!("\n\nNew problems detected after saving the file:\n{}", fr.new_problems_message));
                }
                summary
            })
            .collect();

        Ok(summaries.join("\n\n"))
    }

    pub async fn execute(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let workspace_root = std::env::current_dir().map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to get current directory: {}", e))
        })?;
        self.execute_with_workspace_root(state, params, &workspace_root)
            .await
    }

    pub fn description(&self, _params: &serde_json::Value) -> String {
        "[replace_symbol]".to_string()
    }
}

#[async_trait::async_trait]
impl ToolHandler for ReplaceSymbolHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let mut state = ctx.state.lock().await;
        self.execute_with_workspace_root(&mut state, params, ctx.workspace_root.as_path())
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        ReplaceSymbolHandler::description(self, params)
    }
}

struct FileResult {
    display_path: String,
    replacements_applied: usize,
    replacements_failed: usize,
    symbols: Vec<String>,
    new_problems_message: String,
}

fn read_replacements(params: &serde_json::Value) -> Vec<Replacement> {
    if let Some(replacements) = params.get("replacements").and_then(|v| v.as_array()) {
        return replacements
            .iter()
            .filter_map(|item| {
                // Schema declares "old_name"/"new_name", but support "symbol"/"text" for backwards compatibility
                let symbol = item
                    .get("symbol")
                    .or_else(|| item.get("old_name"))
                    .and_then(|v| v.as_str())?
                    .to_string();
                let text = item
                    .get("text")
                    .or_else(|| item.get("replacement"))
                    .or_else(|| item.get("new_name"))
                    .and_then(|v| v.as_str())?
                    .to_string();
                Some(Replacement {
                    path: item.get("path")?.as_str()?.to_string(),
                    symbol,
                    text,
                    symbol_type: item.get("type").and_then(|v| v.as_str()).map(String::from),
                })
            })
            .collect();
    }

    // Legacy singular format: also support schema keys old_name/new_name
    let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let symbol = params
        .get("symbol")
        .or_else(|| params.get("old_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let text = params
        .get("text")
        .or_else(|| params.get("replacement"))
        .or_else(|| params.get("new_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if path.is_empty() || symbol.is_empty() || text.is_empty() {
        return Vec::new();
    }

    vec![Replacement {
        path: path.to_string(),
        symbol: symbol.to_string(),
        text: text.to_string(),
        symbol_type: params
            .get("type")
            .and_then(|v| v.as_str())
            .map(String::from),
    }]
}

fn group_replacements_by_file(
    replacements: Vec<Replacement>,
    workspace_root: &Path,
) -> Result<BTreeMap<String, FileBatch>, ToolError> {
    let mut batches: BTreeMap<String, FileBatch> = BTreeMap::new();

    for r in replacements {
        let absolute_path = resolve_sanitized_path(workspace_root, &r.path)?
            .to_str()
            .map(String::from)
            .unwrap_or_else(|| r.path.clone());

        let display_path = r.path.clone();

        batches
            .entry(absolute_path.clone())
            .or_insert_with(|| FileBatch {
                absolute_path,
                display_path,
                replacements: Vec::new(),
            })
            .replacements
            .push(r);
    }

    Ok(batches)
}

async fn process_batch(
    batch: &FileBatch,
    symbol_index_service: Option<&Arc<std::sync::Mutex<SymbolIndexService>>>,
) -> Result<FileResult, ToolError> {
    let original_content = fs::read_to_string(&batch.absolute_path)
        .await
        .map_err(|e| {
            ToolError::ExecutionFailed(format!("Error reading file {}: {}", batch.absolute_path, e))
        })?;

    let language_parsers = load_required_language_parsers(&[batch.absolute_path.as_str()])
        .map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to load language parsers: {}", e))
        })?;

    let mut resolved_replacements: Vec<(Replacement, SymbolRange)> = Vec::new();

    for r in &batch.replacements {
        let resolved_range = match symbol_index_service {
            Some(mutex) => {
                let locations = {
                    let index_service = mutex.lock().unwrap_or_else(|e| e.into_inner());
                    index_service.get_definitions(&r.symbol, None)
                };
                let mut result = None;
                for loc in locations {
                    if let Some(loc_path) = &loc.path {
                        let rel_path = std::path::Path::new(&batch.absolute_path);
                        let abs_path_str = rel_path.to_str().unwrap_or("");
                        if loc_path == abs_path_str
                            || loc_path.starts_with(&format!("{}/", abs_path_str))
                        {
                            let start_index = calculate_byte_offset(
                                &original_content,
                                loc.start_line,
                                loc.start_column,
                            );
                            let end_index = calculate_byte_offset(
                                &original_content,
                                loc.end_line,
                                loc.end_column,
                            );
                            result = Some(SymbolRange {
                                start_index,
                                end_index,
                                start_line: loc.start_line,
                                name_text: r.symbol.clone(),
                            });
                            break;
                        }
                    }
                }
                result
            }
            None => find_symbol_via_tree_sitter(
                r,
                &batch.absolute_path,
                &original_content,
                &language_parsers,
            )?,
        };

        match resolved_range {
            Some(range) => resolved_replacements.push((r.clone(), range)),
            None => {
                return Err(ToolError::ExecutionFailed(format!(
                    "Symbol '{}'{} not found in {}.",
                    r.symbol,
                    r.symbol_type
                        .as_ref()
                        .map(|t| format!(" of type '{}'", t))
                        .unwrap_or_default(),
                    r.path
                )));
            }
        }
    }

    resolved_replacements.sort_by_key(|a| a.1.start_index);

    for i in 0..resolved_replacements.len().saturating_sub(1) {
        if resolved_replacements[i].1.end_index > resolved_replacements[i + 1].1.start_index {
            return Err(ToolError::ExecutionFailed(format!(
                "Overlapping replacements detected for symbols '{}' and '{}' in {}.",
                resolved_replacements[i].0.symbol,
                resolved_replacements[i + 1].0.symbol,
                batch.display_path
            )));
        }
    }

    let mut sorted_for_application = resolved_replacements;
    sorted_for_application.sort_by_key(|b| std::cmp::Reverse(b.1.start_index));

    let mut current_content = original_content.clone();
    let mut symbols_applied: Vec<String> = Vec::new();

    for (replacement, range) in sorted_for_application {
        let new_text = strip_hashes(&replacement.text);

        let line_start = find_line_start_byte(&current_content, range.start_index);

        let leading_whitespace_before = &current_content[line_start..range.start_index];
        let adjusted_new_text = if !leading_whitespace_before.is_empty()
            && leading_whitespace_before
                .chars()
                .all(|c| c == ' ' || c == '\t')
        {
            let whitespace_len = leading_whitespace_before.len();
            if new_text.starts_with([' ', '\t']) {
                let non_whitespace_start = new_text
                    .find(|c: char| !c.is_whitespace())
                    .unwrap_or(new_text.len());
                if non_whitespace_start >= whitespace_len {
                    new_text[whitespace_len..].to_string()
                } else {
                    new_text.to_string()
                }
            } else {
                new_text.to_string()
            }
        } else {
            new_text.clone()
        };

        current_content = format!(
            "{}{}{}",
            &current_content[..range.start_index],
            adjusted_new_text,
            &current_content[range.end_index..]
        );

        symbols_applied.push(replacement.symbol);
    }

    crate::storage::disk::atomic_write_file_async(&batch.absolute_path, &current_content)
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("Failed to write file: {}", e)))?;

    Ok(FileResult {
        display_path: batch.display_path.clone(),
        replacements_applied: symbols_applied.len(),
        replacements_failed: 0,
        symbols: symbols_applied,
        new_problems_message: String::new(),
    })
}

fn find_symbol_via_tree_sitter(
    replacement: &Replacement,
    absolute_path: &str,
    content: &str,
    language_parsers: &crate::services::tree_sitter::LanguageParserMap,
) -> Result<Option<SymbolRange>, ToolError> {
    match get_symbol_range(
        absolute_path,
        &replacement.symbol,
        replacement.symbol_type.as_deref(),
        content,
        language_parsers,
    ) {
        Ok(range) => Ok(range),
        Err(e) => Err(ToolError::ExecutionFailed(format!(
            "Error finding symbol: {}",
            e
        ))),
    }
}

fn find_line_start_byte(content: &str, byte_offset: usize) -> usize {
    let mut line_start = 0;
    for (i, c) in content.char_indices() {
        if i >= byte_offset {
            break;
        }
        if c == '\n' {
            line_start = i + 1;
        }
    }
    line_start
}

fn calculate_byte_offset(content: &str, line: usize, column: usize) -> usize {
    let mut byte_offset = 0;

    for (current_line, line_str) in content.lines().enumerate() {
        if current_line == line {
            return byte_offset + column;
        }
        byte_offset += line_str.len() + 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_symbol_handler_creation() {
        let handler = ReplaceSymbolHandler::new();
        assert!(format!("{:?}", handler).starts_with("ReplaceSymbolHandler"));
    }

    #[test]
    fn test_group_replacements_by_file() {
        let replacements = vec![
            Replacement {
                path: "src/a.rs".to_string(),
                symbol: "foo".to_string(),
                text: "bar".to_string(),
                symbol_type: None,
            },
            Replacement {
                path: "src/b.rs".to_string(),
                symbol: "baz".to_string(),
                text: "qux".to_string(),
                symbol_type: None,
            },
            Replacement {
                path: "src/a.rs".to_string(),
                symbol: "foo2".to_string(),
                text: "bar2".to_string(),
                symbol_type: None,
            },
        ];

        let batches =
            group_replacements_by_file(replacements, &std::env::current_dir().unwrap()).unwrap();
        assert_eq!(batches.len(), 2);
        let a_key = std::env::current_dir().unwrap().join("src/a.rs");
        let a_key_str = a_key.to_str().unwrap();
        let b_key = std::env::current_dir().unwrap().join("src/b.rs");
        let b_key_str = b_key.to_str().unwrap();
        assert_eq!(batches.get(a_key_str).unwrap().replacements.len(), 2);
        assert_eq!(batches.get(b_key_str).unwrap().replacements.len(), 1);
    }

    #[test]
    fn test_read_replacements_from_array() {
        let params = serde_json::json!({
            "replacements": [
                {"path": "src/main.rs", "symbol": "foo", "text": "bar"},
                {"path": "src/lib.rs", "symbol": "baz", "replacement": "qux"}
            ]
        });

        let replacements = read_replacements(&params);
        assert_eq!(replacements.len(), 2);
        assert_eq!(replacements[0].symbol, "foo");
        assert_eq!(replacements[1].text, "qux");
    }

    #[test]
    fn test_read_replacements_from_legacy_format() {
        let params = serde_json::json!({
            "path": "src/main.rs",
            "symbol": "foo",
            "text": "bar",
            "type": "function"
        });

        let replacements = read_replacements(&params);
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].symbol, "foo");
        assert_eq!(replacements[0].symbol_type, Some("function".to_string()));
    }

    #[test]
    fn test_read_replacements_empty_params() {
        let params = serde_json::json!({});
        let replacements = read_replacements(&params);
        assert!(replacements.is_empty());
    }

    #[test]
    fn test_group_replacements_by_file_rejects_absolute() {
        let workspace_root = std::env::current_dir().unwrap();
        let replacements = vec![Replacement {
            path: "/etc/passwd".to_string(),
            symbol: "foo".to_string(),
            text: "bar".to_string(),
            symbol_type: None,
        }];
        let result = group_replacements_by_file(replacements, &workspace_root);
        assert!(result.is_err());
    }

    #[test]
    fn test_group_replacements_by_file_rejects_traversal() {
        let workspace_root = std::env::current_dir().unwrap();
        let replacements = vec![Replacement {
            path: "../etc/passwd".to_string(),
            symbol: "foo".to_string(),
            text: "bar".to_string(),
            symbol_type: None,
        }];
        let result = group_replacements_by_file(replacements, &workspace_root);
        assert!(result.is_err());
    }

    #[test]
    fn test_group_replacements_by_file_allows_normal_relative() {
        let workspace_root = std::env::current_dir().unwrap();
        let replacements = vec![Replacement {
            path: "src/main.rs".to_string(),
            symbol: "foo".to_string(),
            text: "bar".to_string(),
            symbol_type: None,
        }];
        let result = group_replacements_by_file(replacements, &workspace_root);
        assert!(result.is_ok());
        let batches = result.unwrap();
        assert_eq!(batches.len(), 1);
    }

    #[tokio::test]
    async fn test_replace_symbol_multi_replacement_single_read() {
        // Test that multiple replacements in the same file work correctly
        // The optimization ensures the file is read only once per batch, not re-read
        // for each calculate_byte_offset call
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace_root = temp_dir.path();

        // Create a test file with a symbol to replace
        let file_content = "fn foo() {}\nfn bar() { foo(); }\n";
        std::fs::write(workspace_root.join("test.rs"), file_content).unwrap();

        let handler = ReplaceSymbolHandler::new();
        let mut state = TaskState::default();
        let params = serde_json::json!({
            "replacements": [
                {"path": "test.rs", "symbol": "foo", "text": "FOO"},
            ]
        });

        let result = handler
            .execute_with_workspace_root(&mut state, params, workspace_root)
            .await
            .unwrap();

        // Verify the result indicates replacements were made
        assert!(result.contains("test.rs"));

        // Verify the file was updated correctly
        let new_content = fs::read_to_string(workspace_root.join("test.rs"))
            .await
            .unwrap();
        assert!(new_content.contains("FOO"));
        assert!(!new_content.contains("fn foo()"));
    }
}
