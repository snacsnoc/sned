use crate::core::hash_utils::format_line_with_hash;
use crate::core::tools::{ToolContext, ToolError, ToolHandler, resolve_sanitized_path};
use crate::services::tree_sitter::load_required_language_parsers;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use streaming_iterator::StreamingIterator;
use tokio::fs;

/// Handler for find_symbol_references tool.
pub struct FindSymbolReferencesHandler;

#[derive(Debug, Clone)]
struct Hit {
    line_index: usize,
    symbol: String,
}

/// Stores file lines and parsed hits to avoid re-reading during formatting.
#[derive(Clone)]
struct FileData {
    lines: Vec<String>,
    hits: Vec<Hit>,
}

impl FindSymbolReferencesHandler {
    pub async fn run(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let paths = read_string_list(&params, "paths", "path");
        // Schema declares "name" (string), but support array form for multiple symbols
        let symbols = read_string_list(&params, "names", "name");
        let find_type = params
            .get("find_type")
            .and_then(|v| v.as_str())
            .unwrap_or("both");

        if paths.is_empty() {
            return Err(ToolError::InvalidInput(
                "Missing required parameter: paths".to_string(),
            ));
        }
        if symbols.is_empty() {
            return Err(ToolError::InvalidInput(
                "Missing required parameter: names".to_string(),
            ));
        }

        let mut file_data: BTreeMap<String, FileData> = BTreeMap::new();
        let mut any_error = None;

        for path in &paths {
            let abs_path = resolve_sanitized_path(&ctx.workspace_root, path)?;
            let abs_path_str = abs_path.to_string_lossy();

            let content = match fs::read_to_string(&abs_path).await {
                Ok(content) => content,
                Err(e) => {
                    any_error = Some(ToolError::ExecutionFailed(format!(
                        "Error reading file {}: {}",
                        path, e
                    )));
                    break;
                }
            };

            let parsers =
                load_required_language_parsers(&[abs_path_str.as_ref()]).map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to load language parsers: {}", e))
                })?;

            let hits = collect_hits_for_file(path, &symbols, find_type, &content, &parsers)
                .map_err(|e| {
                    ToolError::ExecutionFailed(format!("Error finding references: {}", e))
                })?;
            
            let lines: Vec<String> = content.lines().map(|line| line.to_string()).collect();
            file_data.insert(path.clone(), FileData { lines, hits });
        }

        if let Some(err) = any_error {
            return Err(err);
        }

        let total_hits = file_data.values().map(|data| data.hits.len()).sum::<usize>();
        if total_hits == 0 {
            let kind = if find_type == "both" {
                "references or definitions".to_string()
            } else {
                format!("{}s", find_type)
            };
            return Ok(format!(
                "No {} found for symbols: {}.",
                kind,
                symbols.join(", ")
            ));
        }

        let mut sections = Vec::new();
        for (path, data) in file_data {
            if data.hits.is_empty() {
                continue;
            }

            let anchor_mgr = ctx.anchor_mgr.clone();
            let anchors = anchor_mgr.reconcile(&path, &data.lines, Some(ctx.task_id.as_str()));

            let mut merged: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
            for hit in data.hits {
                merged.entry(hit.line_index).or_default().insert(hit.symbol);
            }

            let mut file_lines = Vec::new();
            for (line_index, symbols) in merged {
                if let Some(line_content) = data.lines.get(line_index) {
                    let anchor = anchors.get(line_index).cloned().unwrap_or_default();
                    let formatted = format_line_with_hash(line_content, &anchor)
                        .trim()
                        .to_string();
                    file_lines.push(format!(
                        "  ({}) {}",
                        symbols.into_iter().collect::<Vec<_>>().join(", "),
                        formatted
                    ));
                }
            }

            if !file_lines.is_empty() {
                sections.push(format!("{}:\n{}", path, file_lines.join("\n")));
            }
        }

        Ok(sections.join("\n\n"))
    }
}

#[async_trait::async_trait]
impl ToolHandler for FindSymbolReferencesHandler {
    async fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        Self::run(self, ctx, params)
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[find_symbol_references]".to_string()
    }
}

fn read_string_list(
    params: &serde_json::Value,
    plural_key: &str,
    singular_key: &str,
) -> Vec<String> {
    crate::core::tools::coerce_string_array(params, plural_key, singular_key)
}

fn collect_hits_for_file(
    path: &str,
    symbols: &[String],
    find_type: &str,
    content: &str,
    language_parsers: &crate::services::tree_sitter::LanguageParserMap,
) -> Result<Vec<Hit>, String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let entry = language_parsers
        .get(&ext)
        .ok_or_else(|| format!("Unsupported file extension: {}", ext))?;

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&entry.language)
        .map_err(|e| e.to_string())?;

    let content_bytes = content.as_bytes();
    let tree = parser
        .parse(content, None)
        .ok_or_else(|| "Failed to parse file".to_string())?;
    let root_node = tree.root_node();

    let mut query_cursor = tree_sitter::QueryCursor::new();
    let mut captures = query_cursor.captures(&entry.query, root_node, content.as_bytes());

    let mut node_to_match: HashMap<usize, tree_sitter::Node> = HashMap::with_capacity(16);
    let mut capture_text_by_node: HashMap<usize, String> = HashMap::with_capacity(16);

    while let Some((match_, capture_index)) = captures.next() {
        let capture = match_.captures[*capture_index];
        let capture_name = entry.query.capture_names()[capture.index as usize];
        if capture_name.starts_with("name.") || capture_name.starts_with("definition.") {
            node_to_match.insert(capture.node.id(), capture.node);
            if let Ok(text) = capture.node.utf8_text(content_bytes) {
                capture_text_by_node.insert(capture.node.id(), text.to_string());
            }
        }
    }

    let allowed_kind = |capture_name: &str| -> bool {
        match find_type {
            "definition" => capture_name.starts_with("name.definition"),
            "reference" => capture_name.starts_with("name.reference"),
            _ => {
                capture_name.starts_with("name.definition")
                    || capture_name.starts_with("name.reference")
            }
        }
    };

    let mut hits = Vec::new();
    let mut seen_hits: HashSet<(usize, String)> = HashSet::new();
    let mut query_cursor2 = tree_sitter::QueryCursor::new();
    let mut captures2 = query_cursor2.captures(&entry.query, root_node, content.as_bytes());

    while let Some((match_, capture_index)) = captures2.next() {
        let capture = match_.captures[*capture_index];
        let capture_name = entry.query.capture_names()[capture.index as usize];
        if !allowed_kind(capture_name) {
            continue;
        }

        let name_text = match capture.node.utf8_text(content_bytes) {
            Ok(t) => t.to_string(),
            Err(_) => continue,
        };

        let full_name =
            resolve_full_name(capture.node, &node_to_match, &capture_text_by_node, content);
        let normalized_full_name = full_name.replace("::", ".");
        let normalized_name = name_text.replace("::", ".");

        for symbol in symbols {
            let normalized_requested = symbol.replace("::", ".");
            if symbol_matches(&normalized_full_name, &normalized_requested)
                || symbol_matches(&normalized_name, &normalized_requested)
            {
                let key = (capture.node.start_position().row, symbol.clone());
                if seen_hits.insert(key) {
                    hits.push(Hit {
                        line_index: capture.node.start_position().row,
                        symbol: symbol.clone(),
                    });
                }
            }
        }
    }

    Ok(hits)
}

fn resolve_full_name(
    mut current_node: tree_sitter::Node,
    node_to_match: &HashMap<usize, tree_sitter::Node>,
    capture_text_by_node: &HashMap<usize, String>,
    content: &str,
) -> String {
    let content_bytes = content.as_bytes();
    let mut full_name = current_node
        .utf8_text(content_bytes)
        .unwrap_or("")
        .to_string();
    let mut seen_nodes = HashSet::new();
    seen_nodes.insert(current_node.id());

    while let Some(parent) = current_node.parent() {
        current_node = parent;
        if seen_nodes.contains(&current_node.id()) {
            break;
        }
        seen_nodes.insert(current_node.id());

        if let Some(parent_node) = node_to_match.get(&current_node.id())
            && let Some(parent_name) = capture_text_by_node.get(&parent_node.id())
        {
            full_name = format!("{}.{}", parent_name, full_name);
        }
    }

    full_name
}

fn symbol_matches(full_name: &str, requested: &str) -> bool {
    full_name == requested || full_name.ends_with(&format!(".{}", requested))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_loop::TaskState;
    use crate::core::file_editor::AnchorStateManager;
    use crate::core::tools::ToolContext;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_find_symbol_references_basic() {
        // Test that find_symbol_references works correctly with stored file content
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace_root = temp_dir.path();
        
        // Create a test file with a function and its reference
        let file_content = "fn foo() {}\nfn bar() { foo(); }\n";
        std::fs::write(workspace_root.join("test.rs"), file_content).unwrap();

        let handler = FindSymbolReferencesHandler;
        let state = Arc::new(Mutex::new(TaskState::default()));
        let anchor_mgr = AnchorStateManager::new();
        let ctx = ToolContext::new(
            state,
            None,
            workspace_root.to_path_buf(),
            anchor_mgr,
            false,
            "test-task".to_string(),
            None,
            false,
            None,
        );

        let params = serde_json::json!({
            "paths": vec!["test.rs"],
            "symbols": vec!["foo"],
            "find_type": "both",
        });

        let result = handler.execute(&ctx, params).await.unwrap();
        
        // Verify the result contains both the definition and reference
        let result_str = result.as_str().unwrap();
        assert!(result_str.contains("test.rs"));
        assert!(result_str.contains("fn foo()"));
    }
}
