use crate::core::hash_utils::format_line_with_hash;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use crate::services::symbol_index::{SymbolIndexService, SymbolLocation, SymbolType};
use crate::services::tree_sitter::load_required_language_parsers;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use streaming_iterator::StreamingIterator;
use tokio::fs;

/// Handler for find_symbol_references tool.
pub struct FindSymbolReferencesHandler {
    symbol_index_service: Option<Arc<std::sync::Mutex<SymbolIndexService>>>,
}

#[derive(Debug, Clone)]
struct Hit {
    line_index: usize,
    symbol: String,
    is_definition: bool,
}

/// Stores file lines and parsed hits to avoid re-reading during formatting.
#[derive(Clone)]
struct FileData {
    lines: Vec<String>,
    hits: Vec<Hit>,
}

impl FindSymbolReferencesHandler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            symbol_index_service: None,
        }
    }

    #[must_use]
    pub fn with_symbol_index(mut self, service: Arc<std::sync::Mutex<SymbolIndexService>>) -> Self {
        self.symbol_index_service = Some(service);
        self
    }

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

        if symbols.is_empty() {
            return Err(ToolError::InvalidInput(
                "Missing required parameter: names".to_string(),
            ));
        }

        let mut file_data: BTreeMap<String, FileData> = BTreeMap::new();
        let mut any_error = None;
        let mut index_warnings = Vec::new();

        for path in &paths {
            let abs_path = ctx.resolve_path(path)?;
            let abs_path_str = abs_path.to_string_lossy();

            let content = match fs::read_to_string(&abs_path).await {
                Ok(content) => content,
                Err(e) => {
                    any_error = Some(ToolError::ExecutionFailed(format!(
                        "Error reading file {path}: {e}"
                    )));
                    break;
                }
            };

            let parsers =
                load_required_language_parsers(&[abs_path_str.as_ref()]).map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to load language parsers: {e}"))
                })?;

            let display_path = abs_path
                .strip_prefix(&ctx.workspace_root)
                .unwrap_or(&abs_path)
                .to_string_lossy()
                .into_owned();
            let hits =
                collect_hits_for_file(&display_path, &symbols, find_type, &content, &parsers)
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!("Error finding references: {e}"))
                    })?;

            let lines: Vec<String> = content
                .lines()
                .map(std::string::ToString::to_string)
                .collect();
            file_data.insert(display_path, FileData { lines, hits });
        }

        if let Some(err) = any_error {
            return Err(err);
        }

        let (index_status, indexed_locations, project_root) =
            self.index_locations(&symbols, find_type, paths.is_empty())?;
        if !indexed_locations.is_empty() {
            merge_index_locations(
                &mut file_data,
                &ctx.workspace_root,
                &project_root,
                indexed_locations,
                &mut index_warnings,
            )
            .await;
        }

        for data in file_data.values_mut() {
            let mut seen = HashSet::new();
            data.hits
                .retain(|hit| seen.insert((hit.line_index, hit.symbol.clone(), hit.is_definition)));
        }

        let total_hits = file_data
            .values()
            .map(|data| data.hits.len())
            .sum::<usize>();
        if total_hits == 0 {
            let kind = if find_type == "both" {
                "references or definitions".to_string()
            } else {
                format!("{find_type}s")
            };
            let mut output = Vec::new();
            if let Some(status) = index_status {
                if status.initial_walk_complete {
                    output.push(format!(
                        "Index had 0 hits (workspace scanned {} files).",
                        status.workspace_file_count
                    ));
                } else {
                    output.push(index_status_line(&status));
                }
            }
            output.push(format!(
                "No {} found for symbols: {}.",
                kind,
                symbols.join(", ")
            ));
            output.extend(index_warnings);
            return Ok(output.join("\n"));
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
                    let formatted = format_line_with_hash(line_content, &anchor, &[])
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

        let mut output = Vec::new();
        if let Some(status) = index_status
            && !status.initial_walk_complete
        {
            output.push(index_status_line(&status));
        }
        output.extend(index_warnings);
        output.push(sections.join("\n\n"));
        Ok(output.join("\n\n"))
    }

    fn index_locations(
        &self,
        symbols: &[String],
        find_type: &str,
        paths_are_empty: bool,
    ) -> Result<
        (
            Option<crate::services::symbol_index::SymbolIndexStatus>,
            Vec<SymbolLocation>,
            String,
        ),
        ToolError,
    > {
        let Some(symbol_index_service) = &self.symbol_index_service else {
            if paths_are_empty {
                return Err(ToolError::InvalidInput(
                    "find_symbol_references requires paths when the symbol index is disabled or still warming.".to_string(),
                ));
            }
            return Ok((None, Vec::new(), String::new()));
        };

        let service = symbol_index_service
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if service.is_disabled() {
            if paths_are_empty {
                return Err(ToolError::InvalidInput(
                    "find_symbol_references requires paths when the symbol index is disabled or still warming.".to_string(),
                ));
            }
            return Ok((None, Vec::new(), service.get_project_root().to_string()));
        }

        let status = service.status();
        if paths_are_empty && !status.initial_walk_complete {
            return Err(ToolError::InvalidInput(
                "find_symbol_references requires paths when the symbol index is disabled or still warming.".to_string(),
            ));
        }

        let mut locations = Vec::new();
        for symbol in symbols {
            match find_type {
                "definition" => locations.extend(service.get_definitions(symbol, None)),
                "reference" => locations.extend(service.get_references(symbol, None)),
                _ => {
                    locations.extend(service.get_definitions(symbol, None));
                    locations.extend(service.get_references(symbol, None));
                }
            }
        }
        Ok((
            Some(status),
            locations,
            service.get_project_root().to_string(),
        ))
    }
}

impl Default for FindSymbolReferencesHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolHandler for FindSymbolReferencesHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self;
        let ctx = ctx.clone();
        Box::pin(async move {
            Self::run(handler, &ctx, params)
                .await
                .map(serde_json::Value::String)
        })
    }

    fn description(&self, _params: &serde_json::Value) -> String {
        "[find_symbol_references]".to_string()
    }
}

async fn merge_index_locations(
    file_data: &mut BTreeMap<String, FileData>,
    workspace_root: &Path,
    project_root: &str,
    locations: Vec<SymbolLocation>,
    warnings: &mut Vec<String>,
) {
    let canonical_workspace = match fs::canonicalize(workspace_root).await {
        Ok(path) => path,
        Err(error) => {
            warnings.push(format!(
                "Unable to resolve workspace for index results: {error}"
            ));
            return;
        }
    };

    for location in locations {
        let Some(rel_path) = location.path.as_deref() else {
            continue;
        };
        let absolute_path = Path::new(project_root).join(rel_path);
        let resolved_path = match fs::canonicalize(&absolute_path).await {
            Ok(path) => path,
            Err(error) => {
                warnings.push(format!("Index entry skipped for {rel_path}: {error}"));
                continue;
            }
        };
        if !resolved_path.starts_with(&canonical_workspace) {
            warnings.push(format!("Index entry skipped outside workspace: {rel_path}"));
            continue;
        }
        let display_path = rel_path.to_string();
        if !file_data.contains_key(&display_path) {
            let content = match fs::read_to_string(&resolved_path).await {
                Ok(content) => content,
                Err(error) => {
                    warnings.push(format!("Index entry skipped for {display_path}: {error}"));
                    continue;
                }
            };
            file_data.insert(
                display_path.clone(),
                FileData {
                    lines: content
                        .lines()
                        .map(std::string::ToString::to_string)
                        .collect(),
                    hits: Vec::new(),
                },
            );
        }
        if let Some(data) = file_data.get_mut(&display_path) {
            data.hits.push(Hit {
                line_index: location.start_line,
                symbol: location.name,
                is_definition: location.symbol_type == SymbolType::Definition,
            });
        }
    }
}

fn index_status_line(status: &crate::services::symbol_index::SymbolIndexStatus) -> String {
    format!(
        "[Index: {}/{} files indexed, walk in progress]",
        status.indexed_file_count, status.workspace_file_count
    )
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
        .ok_or_else(|| format!("Unsupported file extension: {ext}"))?;

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
    let mut seen_hits: HashSet<(usize, String, bool)> = HashSet::new();
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
                let is_definition = capture_name.starts_with("name.definition");
                let key = (
                    capture.node.start_position().row,
                    symbol.clone(),
                    is_definition,
                );
                if seen_hits.insert(key) {
                    hits.push(Hit {
                        line_index: capture.node.start_position().row,
                        symbol: symbol.clone(),
                        is_definition,
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
            full_name = format!("{parent_name}.{full_name}");
        }
    }

    full_name
}

fn symbol_matches(full_name: &str, requested: &str) -> bool {
    full_name == requested || full_name.ends_with(&format!(".{requested}"))
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

        let handler = FindSymbolReferencesHandler::new();
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
            Arc::new(crate::cli::output::StderrOutputWriter),
        );

        let params = serde_json::json!({
            "paths": vec!["test.rs"],
            "names": vec!["foo"],
            "find_type": "both",
        });

        let result = handler.execute(&ctx, params).await.unwrap();

        // Verify the result contains both the definition and reference
        let result_str = result.as_str().unwrap();
        assert!(result_str.contains("test.rs"));
        assert!(result_str.contains("fn foo()"));
    }

    #[tokio::test]
    async fn test_find_symbol_references_uses_ready_index_without_paths() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace_root = temp_dir.path();
        std::fs::write(
            workspace_root.join("indexed.rs"),
            "fn indexed_symbol() {}\n",
        )
        .unwrap();

        let symbol_index = Arc::new(std::sync::Mutex::new(SymbolIndexService::new(
            workspace_root.to_string_lossy().into_owned(),
        )));
        {
            let mut service = symbol_index.lock().unwrap();
            service.index_file(
                "indexed.rs",
                1,
                1,
                &[SymbolLocation {
                    path: None,
                    name: "indexed_symbol".to_string(),
                    start_line: 0,
                    start_column: 3,
                    end_line: 0,
                    end_column: 17,
                    symbol_type: SymbolType::Definition,
                    kind: None,
                }],
            );
            service.finish_initial_walk(1);
        }

        let ctx = ToolContext::new(
            Arc::new(Mutex::new(TaskState::default())),
            None,
            workspace_root.to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let handler = FindSymbolReferencesHandler::new().with_symbol_index(symbol_index);

        let result = handler
            .execute(&ctx, serde_json::json!({ "name": "indexed_symbol" }))
            .await
            .unwrap();

        let result = result.as_str().unwrap();
        assert!(result.contains("indexed.rs"));
        assert!(result.contains("fn indexed_symbol"));
    }

    #[tokio::test]
    async fn test_find_symbol_references_requires_paths_while_index_warms() {
        let temp_dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(
            Arc::new(Mutex::new(TaskState::default())),
            None,
            temp_dir.path().to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let handler =
            FindSymbolReferencesHandler::new().with_symbol_index(Arc::new(std::sync::Mutex::new(
                SymbolIndexService::new(temp_dir.path().to_string_lossy().into_owned()),
            )));

        let error = handler
            .execute(&ctx, serde_json::json!({ "name": "missing_symbol" }))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("requires paths"));
    }

    #[tokio::test]
    async fn test_find_symbol_references_merges_paths_with_index_hits() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace_root = temp_dir.path();
        std::fs::write(workspace_root.join("parsed.rs"), "fn shared_symbol() {}\n").unwrap();
        std::fs::write(workspace_root.join("indexed.rs"), "fn shared_symbol() {}\n").unwrap();
        let symbol_index = Arc::new(std::sync::Mutex::new(SymbolIndexService::new(
            workspace_root.to_string_lossy().into_owned(),
        )));
        {
            let mut service = symbol_index.lock().unwrap();
            service.index_file(
                "indexed.rs",
                1,
                1,
                &[SymbolLocation {
                    path: None,
                    name: "shared_symbol".to_string(),
                    start_line: 0,
                    start_column: 3,
                    end_line: 0,
                    end_column: 16,
                    symbol_type: SymbolType::Definition,
                    kind: None,
                }],
            );
            service.finish_initial_walk(2);
        }
        let ctx = ToolContext::new(
            Arc::new(Mutex::new(TaskState::default())),
            None,
            workspace_root.to_path_buf(),
            AnchorStateManager::new(),
            false,
            "test-task".to_string(),
            None,
            false,
            Arc::new(crate::cli::output::StderrOutputWriter),
        );
        let handler = FindSymbolReferencesHandler::new().with_symbol_index(symbol_index);

        let result = handler
            .execute(
                &ctx,
                serde_json::json!({ "path": "parsed.rs", "name": "shared_symbol" }),
            )
            .await
            .unwrap();

        let result = result.as_str().unwrap();
        assert!(result.contains("parsed.rs"));
        assert!(result.contains("indexed.rs"));
    }
}
