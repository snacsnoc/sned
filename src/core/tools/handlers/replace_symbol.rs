use crate::core::agent_loop::TaskState;
use crate::core::file_editor::{FileTextFormat, normalize_file_content, restore_file_content};
use crate::core::hash_utils::strip_hashes;
use crate::core::tools::handlers::error_guidance;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use crate::services::symbol_index::SymbolIndexService;
use crate::services::tree_sitter::{SymbolRange, get_symbol_range, load_required_language_parsers};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;

struct FileBatch {
    absolute_path: String,
    display_path: String,
    replacements: Vec<Replacement>,
}

pub(crate) struct PendingSymbolWrite {
    pub(crate) path: PathBuf,
    pub(crate) original_content: String,
    pub(crate) final_content: String,
}

struct PreparedFileBatch {
    write: PendingSymbolWrite,
    result: FileResult,
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
    async fn increment_mistakes(state: &Arc<Mutex<TaskState>>) -> u32 {
        let mut state = state.lock().await;
        state.consecutive_mistakes += 1;
        state.consecutive_mistakes
    }

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
        self.execute_with_path_roots(state, params, workspace_root, &[])
            .await
    }

    async fn execute_with_path_roots(
        &self,
        state: &mut TaskState,
        params: serde_json::Value,
        workspace_root: &Path,
        allowed_external_roots: &[std::path::PathBuf],
    ) -> Result<String, ToolError> {
        let shared_state = Arc::new(Mutex::new(std::mem::take(state)));
        let result = self
            .execute_with_shared_state(
                shared_state.clone(),
                params,
                workspace_root,
                allowed_external_roots,
                None,
            )
            .await;
        *state = Arc::try_unwrap(shared_state)
            .expect("replace symbol state should have no remaining owners")
            .into_inner();
        result
    }

    async fn execute_with_shared_state(
        &self,
        state: Arc<Mutex<TaskState>>,
        params: serde_json::Value,
        workspace_root: &Path,
        allowed_external_roots: &[std::path::PathBuf],
        edit_context: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        let replacements = read_replacements(&params);
        if replacements.is_empty() {
            let consecutive_mistakes = Self::increment_mistakes(&state).await;
            tracing::warn!(
                consecutive_mistakes,
                "replace_symbol: no replacements provided"
            );
            return Err(ToolError::InvalidInput(error_guidance::missing_parameter(
                "replacements",
                consecutive_mistakes,
            )));
        }

        let batches = group_replacements_by_file_with_allowed_roots(
            replacements,
            workspace_root,
            allowed_external_roots,
        )?;

        let mut prepared = Vec::with_capacity(batches.len());
        for batch in batches.values() {
            prepared.push(prepare_batch(batch, self.symbol_index_service.as_ref(), &state).await?);
        }

        let writes = prepared
            .iter()
            .map(|batch| &batch.write)
            .collect::<Vec<_>>();
        if let Err(error) = commit_symbol_writes_atomically(&writes).await {
            if let Some(ctx) = edit_context {
                for write in &writes {
                    ctx.invalidate_edit_context(&write.path).await;
                }
            }
            return Err(error);
        }

        let mut file_results = Vec::with_capacity(prepared.len());
        for prepared_batch in prepared {
            if let Some(ctx) = edit_context {
                ctx.invalidate_edit_context(&prepared_batch.write.path)
                    .await;
            }
            state
                .lock()
                .await
                .file_context_tracker
                .mark_file_as_edited_by_sned(&prepared_batch.write.path);
            file_results.push(prepared_batch.result);
        }

        if file_results.is_empty() {
            let consecutive_mistakes = Self::increment_mistakes(&state).await;
            tracing::warn!(consecutive_mistakes, "replace_symbol: no files processed");
            return Err(ToolError::ExecutionFailed(
                "No replacements could be processed".to_string(),
            ));
        }

        let total_applied: usize = file_results.iter().map(|r| r.replacements_applied).sum();
        let total_failed: usize = file_results.iter().map(|r| r.replacements_failed).sum();

        if total_failed > 0 {
            let consecutive_mistakes = Self::increment_mistakes(&state).await;
            tracing::warn!(
                consecutive_mistakes,
                total_failed = total_failed,
                total_applied = total_applied,
                "replace_symbol: replacements failed"
            );
        } else if total_applied > 0 {
            state.lock().await.consecutive_mistakes = 0;
        }

        let summaries: Vec<String> = file_results
            .into_iter()
            .map(|fr| {
                let symbol_list = fr.symbols.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
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
            ToolError::ExecutionFailed(format!("Failed to get current directory: {e}"))
        })?;
        self.execute_with_workspace_root(state, params, &workspace_root)
            .await
    }

    #[must_use]
    pub fn description(&self, _params: &serde_json::Value) -> String {
        "[replace_symbol]".to_string()
    }
}

impl ToolHandler for ReplaceSymbolHandler {
    fn execute(
        &self,
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let handler = self;
        let ctx = ctx.clone();
        Box::pin(async move {
            let batches = group_replacements_by_file_with_allowed_roots(
                read_replacements(&params),
                &ctx.workspace_root,
                &ctx.allowed_external_roots,
            )?;
            let _file_locks = ctx
                .lock_file_paths(
                    &batches
                        .values()
                        .map(|batch| std::path::PathBuf::from(&batch.absolute_path))
                        .collect::<Vec<_>>(),
                )
                .await;
            let result = handler
                .execute_with_shared_state(
                    ctx.state.clone(),
                    params,
                    ctx.workspace_root.as_path(),
                    &ctx.allowed_external_roots,
                    Some(&ctx),
                )
                .await;
            result.map(serde_json::Value::String)
        })
    }

    fn description(&self, params: &serde_json::Value) -> String {
        Self::description(self, params)
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

#[cfg(test)]
fn group_replacements_by_file(
    replacements: Vec<Replacement>,
    workspace_root: &Path,
) -> Result<BTreeMap<String, FileBatch>, ToolError> {
    group_replacements_by_file_with_allowed_roots(replacements, workspace_root, &[])
}

fn group_replacements_by_file_with_allowed_roots(
    replacements: Vec<Replacement>,
    workspace_root: &Path,
    allowed_external_roots: &[std::path::PathBuf],
) -> Result<BTreeMap<String, FileBatch>, ToolError> {
    let mut batches: BTreeMap<String, FileBatch> = BTreeMap::new();

    for r in replacements {
        let absolute_path = crate::core::tools::resolve_authorized_path(
            workspace_root,
            allowed_external_roots,
            &r.path,
        )?
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

async fn prepare_batch(
    batch: &FileBatch,
    symbol_index_service: Option<&Arc<std::sync::Mutex<SymbolIndexService>>>,
    state: &Arc<Mutex<TaskState>>,
) -> Result<PreparedFileBatch, ToolError> {
    let original_content = fs::read_to_string(&batch.absolute_path)
        .await
        .map_err(|e| {
            ToolError::ExecutionFailed(format!("Error reading file {}: {}", batch.absolute_path, e))
        })?;

    let language_parsers = load_required_language_parsers(&[batch.absolute_path.as_str()])
        .map_err(|e| ToolError::ExecutionFailed(format!("Failed to load language parsers: {e}")))?;

    let mut resolved_replacements: Vec<(Replacement, SymbolRange)> = Vec::new();

    for r in &batch.replacements {
        let resolved_range = match symbol_index_service {
            Some(mutex) => {
                let locations = {
                    let index_service = mutex
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    index_service.get_definitions(&r.symbol, None)
                };
                let mut result = None;
                let project_root = {
                    let index_service = mutex
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    index_service.get_project_root().to_string()
                };
                for loc in locations {
                    if let Some(loc_path) = &loc.path {
                        let abs_loc_path = std::path::Path::new(&project_root)
                            .join(loc_path)
                            .to_string_lossy()
                            .into_owned();
                        if abs_loc_path == batch.absolute_path
                            || abs_loc_path.starts_with(&format!("{}/", batch.absolute_path))
                        {
                            let Some(start_index) = calculate_byte_offset(
                                &original_content,
                                loc.start_line,
                                loc.start_column,
                            ) else {
                                continue;
                            };
                            let Some(end_index) = calculate_byte_offset(
                                &original_content,
                                loc.end_line,
                                loc.end_column,
                            ) else {
                                continue;
                            };
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
                // Fall back to tree-sitter if index found no match
                result.or_else(|| {
                    find_symbol_via_tree_sitter(
                        r,
                        &batch.absolute_path,
                        &original_content,
                        &language_parsers,
                    )
                    .ok()
                    .flatten()
                })
            }
            None => find_symbol_via_tree_sitter(
                r,
                &batch.absolute_path,
                &original_content,
                &language_parsers,
            )?,
        };

        if let Some(range) = resolved_range {
            resolved_replacements.push((r.clone(), range));
        } else {
            let consecutive_mistakes = ReplaceSymbolHandler::increment_mistakes(state).await;
            return Err(ToolError::ExecutionFailed(
                error_guidance::symbol_not_found(&r.symbol, &r.path, consecutive_mistakes),
            ));
        }
    }

    resolved_replacements.sort_by_key(|a| a.1.start_index);

    for i in 0..resolved_replacements.len().saturating_sub(1) {
        if resolved_replacements[i].1.end_index > resolved_replacements[i + 1].1.start_index {
            let consecutive_mistakes = ReplaceSymbolHandler::increment_mistakes(state).await;
            let symbols = vec![
                resolved_replacements[i].0.symbol.as_str(),
                resolved_replacements[i + 1].0.symbol.as_str(),
            ];
            return Err(ToolError::ExecutionFailed(
                error_guidance::overlapping_replacements(
                    &symbols,
                    &batch.display_path,
                    consecutive_mistakes,
                ),
            ));
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
                    new_text.clone()
                }
            } else {
                new_text.clone()
            }
        } else {
            new_text.clone()
        };

        let (_, file_format) = normalize_file_content(&current_content);
        let (normalized_new_text, _) = normalize_file_content(&adjusted_new_text);
        let replacement_format = FileTextFormat {
            line_ending: file_format.line_ending,
            has_utf8_bom: false,
        };
        let adjusted_new_text = restore_file_content(&normalized_new_text, replacement_format);

        current_content = format!(
            "{}{}{}",
            &current_content[..range.start_index],
            adjusted_new_text,
            &current_content[range.end_index..]
        );

        symbols_applied.push(replacement.symbol);
    }

    Ok(PreparedFileBatch {
        write: PendingSymbolWrite {
            path: PathBuf::from(&batch.absolute_path),
            original_content,
            final_content: current_content,
        },
        result: FileResult {
            display_path: batch.display_path.clone(),
            replacements_applied: symbols_applied.len(),
            replacements_failed: 0,
            symbols: symbols_applied,
            new_problems_message: String::new(),
        },
    })
}

pub(crate) async fn commit_symbol_writes_atomically(
    writes: &[&PendingSymbolWrite],
) -> Result<(), ToolError> {
    let mut written: Vec<&PendingSymbolWrite> = Vec::with_capacity(writes.len());
    for write in writes {
        match fs::read_to_string(&write.path).await {
            Ok(current) if current == write.original_content => {}
            Ok(_) => {
                let detail = rollback_symbol_writes(&written).await;
                return Err(ToolError::ExecutionFailed(format!(
                    "File changed while preparing symbol edits: {}. No symbol edits were applied.{detail}",
                    write.path.display()
                )));
            }
            Err(error) => {
                let detail = rollback_symbol_writes(&written).await;
                return Err(ToolError::ExecutionFailed(format!(
                    "Failed to verify file {} before writing: {error}.{detail}",
                    write.path.display()
                )));
            }
        }
        if let Err(error) =
            crate::storage::disk::atomic_write_file_async(&write.path, &write.final_content).await
        {
            let rollback_detail = rollback_symbol_writes(&written).await;
            return Err(ToolError::ExecutionFailed(format!(
                "Failed to write file {}: {error}.{rollback_detail}",
                write.path.display()
            )));
        }
        written.push(*write);
    }
    Ok(())
}

async fn rollback_symbol_writes(written: &[&PendingSymbolWrite]) -> String {
    let mut failures = Vec::new();
    for previous in written.iter().rev() {
        match fs::read_to_string(&previous.path).await {
            Ok(current) if current == previous.final_content => {
                if let Err(error) = crate::storage::disk::atomic_write_file_async(
                    &previous.path,
                    &previous.original_content,
                )
                .await
                {
                    failures.push(format!("{}: {error}", previous.path.display()));
                }
            }
            Ok(_) => failures.push(format!(
                "{} changed after this request and was preserved",
                previous.path.display()
            )),
            Err(error) => failures.push(format!(
                "{} could not be checked before rollback: {error}",
                previous.path.display()
            )),
        }
    }
    if failures.is_empty() {
        String::new()
    } else {
        format!(" Rollback issues: {}.", failures.join("; "))
    }
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
            "Error finding symbol: {e}"
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

fn calculate_byte_offset(content: &str, line: usize, column: usize) -> Option<usize> {
    let mut byte_offset = 0;

    for (current_line, line_str) in content.split_inclusive('\n').enumerate() {
        if current_line == line {
            let logical_line = line_str.trim_end_matches(['\r', '\n']);
            return (column <= logical_line.len() && logical_line.is_char_boundary(column))
                .then_some(byte_offset + column);
        }
        byte_offset += line_str.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_offsets_preserve_crlf_and_reject_invalid_positions() {
        let text = "a\r\n雪x\r\nz";
        assert_eq!(calculate_byte_offset(text, 1, 3), Some(6));
        assert_eq!(calculate_byte_offset(text, 2, 0), Some(9));
        assert_eq!(calculate_byte_offset(text, 1, 1), None);
        assert_eq!(calculate_byte_offset(text, 1, 5), None);
        assert_eq!(calculate_byte_offset(text, 3, 0), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symbol_write_failure_restores_earlier_file() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().unwrap();
        let writable = workspace.path().join("writable");
        let blocked = workspace.path().join("blocked");
        std::fs::create_dir_all(&writable).unwrap();
        std::fs::create_dir_all(&blocked).unwrap();
        let first = writable.join("first.rs");
        let second = blocked.join("second.rs");
        std::fs::write(&first, "fn old_first() {}\n").unwrap();
        std::fs::write(&second, "fn old_second() {}\n").unwrap();

        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let writes = [
            PendingSymbolWrite {
                path: first.clone(),
                original_content: "fn old_first() {}\n".to_string(),
                final_content: "fn new_first() {}\n".to_string(),
            },
            PendingSymbolWrite {
                path: second,
                original_content: "fn old_second() {}\n".to_string(),
                final_content: "fn new_second() {}\n".to_string(),
            },
        ];
        let write_refs = writes.iter().collect::<Vec<_>>();
        let result = commit_symbol_writes_atomically(&write_refs).await;
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(first).unwrap(),
            "fn old_first() {}\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replace_symbol_rolls_back_all_files_when_a_later_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().unwrap();
        let writable = workspace.path().join("a_writable");
        let blocked = workspace.path().join("z_blocked");
        std::fs::create_dir_all(&writable).unwrap();
        std::fs::create_dir_all(&blocked).unwrap();
        let first = writable.join("first.rs");
        let second = blocked.join("second.rs");
        std::fs::write(&first, "fn first() {}\n").unwrap();
        std::fs::write(&second, "fn second() {}\n").unwrap();

        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let mut state = TaskState::default();
        let result = ReplaceSymbolHandler::new()
            .execute_with_workspace_root(
                &mut state,
                serde_json::json!({
                    "replacements": [
                        {"path": first, "symbol": "first", "text": "fn first() { updated(); }"},
                        {"path": second, "symbol": "second", "text": "fn second() { updated(); }"}
                    ]
                }),
                workspace.path(),
            )
            .await;
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(first).unwrap(), "fn first() {}\n");
    }

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

    #[tokio::test]
    async fn replace_symbol_preserves_crlf_for_multiline_replacements() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("test.rs");
        std::fs::write(
            &path,
            "fn foo() {\r\n    let value = 1;\r\n}\r\nfn bar() {}\r\n",
        )
        .unwrap();

        let handler = ReplaceSymbolHandler::new();
        let mut state = TaskState::default();
        handler
            .execute_with_workspace_root(
                &mut state,
                serde_json::json!({
                    "replacements": [{
                        "path": "test.rs",
                        "symbol": "foo",
                        "text": "fn foo() {\n    let value = 2;\n    let other = 3;\n}"
                    }]
                }),
                workspace.path(),
            )
            .await
            .unwrap();

        let updated = std::fs::read(&path).unwrap();
        assert_eq!(
            updated,
            b"fn foo() {\r\n    let value = 2;\r\n    let other = 3;\r\n}\r\nfn bar() {}\r\n"
        );
    }

    #[tokio::test]
    async fn test_failed_batch_does_not_mark_file_as_sned_edited() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace_root = temp_dir.path();
        let path = workspace_root.join("test.rs");
        std::fs::write(&path, "fn foo() {}\n").unwrap();

        let handler = ReplaceSymbolHandler::new();
        let mut state = TaskState::default();
        state.file_context_tracker.track_file_read(&path);
        let params = serde_json::json!({
            "replacements": [
                {"path": "test.rs", "symbol": "missing", "text": "MISSING"}
            ]
        });

        assert!(
            handler
                .execute_with_workspace_root(&mut state, params, workspace_root)
                .await
                .is_err()
        );

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        std::fs::write(&path, "fn changed() {}\n").unwrap();
        let warning = state.file_context_tracker.check_stale(&path).await;
        assert!(
            warning.is_some(),
            "a failed batch must not suppress later external modification detection"
        );
    }

    #[tokio::test]
    async fn replace_symbol_preflights_all_files_before_writing_any() {
        let workspace = tempfile::tempdir().unwrap();
        let first = workspace.path().join("first.rs");
        let second = workspace.path().join("second.rs");
        std::fs::write(&first, "fn first() {}\n").unwrap();
        std::fs::write(&second, "fn second() {}\n").unwrap();

        let handler = ReplaceSymbolHandler::new();
        let mut state = TaskState::default();
        let result = handler
            .execute_with_workspace_root(
                &mut state,
                serde_json::json!({
                    "replacements": [
                        {"path": "first.rs", "symbol": "first", "text": "fn renamed() {}"},
                        {"path": "second.rs", "symbol": "missing", "text": "fn missing() {}"}
                    ]
                }),
                workspace.path(),
            )
            .await;

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "fn first() {}\n");
        assert_eq!(
            std::fs::read_to_string(&second).unwrap(),
            "fn second() {}\n"
        );
    }
}
