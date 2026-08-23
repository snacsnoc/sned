//! Symbol index for fast symbol lookup across a codebase.
//!
//! Backed by SQLite for persistence (see `db` module).

pub mod db;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;

/// A symbol location in a file.
#[derive(Debug, Clone)]
pub struct SymbolLocation {
    pub path: Option<String>,
    pub name: String,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub symbol_type: SymbolType,
    pub kind: Option<String>,
}

/// Whether a symbol is a definition or reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolType {
    Definition,
    Reference,
}

/// An entry in the symbol index for a single file.
#[derive(Debug, Clone)]
pub struct FileIndexEntry {
    pub mtime: u64,
    pub size: u64,
    pub symbols: Vec<SymbolLocation>,
}

#[derive(Debug, Clone, Default)]
pub struct SymbolIndexStatus {
    pub indexed_file_count: usize,
    pub workspace_file_count: usize,
    pub initial_walk_complete: bool,
    pub last_indexed_at: Option<SystemTime>,
    pub high_water_mtime: u64,
}

#[derive(Debug, Clone)]
struct IndexCandidate {
    absolute_path: PathBuf,
    relative_path: String,
    mtime: u64,
    size: u64,
}

static SYMBOL_INDEX_REFRESHING: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

const INDEX_BATCH_SIZE: usize = 64;

struct SymbolIndexRefreshGuard {
    project_root: String,
}

impl Drop for SymbolIndexRefreshGuard {
    fn drop(&mut self) {
        SYMBOL_INDEX_REFRESHING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.project_root);
    }
}

/// Symbol index service with optional SQLite persistence.
#[derive(Debug)]
pub struct SymbolIndexService {
    files: HashMap<String, FileIndexEntry>,
    project_root: String,
    /// Optional override for the DB directory. When set, the SQLite database
    /// is stored here instead of `{project_root}/{INDEX_DIR}`. Used to keep
    /// the workspace clean by storing the index in a temp directory.
    index_root: Option<String>,
    db: Option<db::SymbolIndexDatabase>,
    disabled: bool,
    status: SymbolIndexStatus,
}

pub const INDEX_DIR: &str = ".sned-symbol-index";
pub const DB_FILENAME: &str = "data.db";

impl SymbolIndexService {
    #[must_use]
    pub fn new(project_root: String) -> Self {
        Self {
            files: HashMap::with_capacity(1024),
            project_root,
            index_root: None,
            db: None,
            disabled: false,
            status: SymbolIndexStatus::default(),
        }
    }

    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self.db = None;
        self.files.clear();
        self.status.initial_walk_complete = true;
        self
    }

    /// Override the directory used for the SQLite database. When set, the
    /// database is stored at `{index_root}/{INDEX_DIR}` instead of
    /// `{project_root}/{INDEX_DIR}`. The git exclude entry is skipped because
    /// the DB no longer lives inside the project tree.
    #[must_use]
    pub fn with_index_root(mut self, root: String) -> Self {
        self.index_root = Some(root);
        self
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub fn with_persistence(mut self) -> anyhow::Result<Self> {
        if self.disabled {
            return Ok(self);
        }

        // Use index_root override if set, otherwise fall back to project_root.
        let db_base = self.index_root.as_deref().unwrap_or(&self.project_root);
        let db_dir = std::path::Path::new(db_base).join(INDEX_DIR);
        std::fs::create_dir_all(&db_dir)?;

        // Only update the project's git exclude when the DB lives inside the
        // project tree. When index_root is set (e.g. /tmp), the DB is outside
        // the project and no exclude entry is needed.
        if self.index_root.is_none() {
            let git_exclude = std::path::Path::new(&self.project_root)
                .join(".git")
                .join("info")
                .join("exclude");
            if git_exclude.parent().is_some_and(std::path::Path::exists)
                && let Ok(content) = std::fs::read_to_string(&git_exclude)
                && !content.contains(INDEX_DIR)
            {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&git_exclude) {
                    let _ = writeln!(f, "{INDEX_DIR}");
                }
            }
        }

        let db_path = db_dir.join(DB_FILENAME);
        let database = db::SymbolIndexDatabase::open(&db_path)?;
        self.status.indexed_file_count = database.indexed_file_count();
        self.status.high_water_mtime = database.latest_file_mtime().unwrap_or(0);
        self.db = Some(database);
        Ok(self)
    }

    pub fn index_file(
        &mut self,
        rel_path: &str,
        mtime: u64,
        size: u64,
        symbols: &[SymbolLocation],
    ) {
        if self.disabled {
            return;
        }

        self.files.insert(
            rel_path.to_string(),
            FileIndexEntry {
                mtime,
                size,
                symbols: symbols.to_vec(),
            },
        );

        if let Some(ref mut db) = self.db
            && let Err(error) = db.update_file_symbols(rel_path, mtime, size, symbols)
        {
            tracing::warn!(path = %rel_path, %error, "symbol index update failed");
        }
        self.record_index_update(mtime);
    }

    pub fn index_file_safe(
        &mut self,
        rel_path: &str,
        mtime: u64,
        size: u64,
        symbols: &[SymbolLocation],
    ) {
        if self.disabled {
            return;
        }

        if symbols.is_empty() && self.has_symbols_with_metadata(rel_path, mtime, size) {
            return;
        }

        self.index_file(rel_path, mtime, size, symbols);
    }

    pub fn index_files_batch(&mut self, entries: &[(String, u64, u64, Vec<SymbolLocation>)]) {
        if self.disabled || entries.is_empty() {
            return;
        }

        let mut high_water_mtime = self.status.high_water_mtime;
        for (rel_path, mtime, size, symbols) in entries {
            high_water_mtime = high_water_mtime.max(*mtime);
            self.files.insert(
                rel_path.clone(),
                FileIndexEntry {
                    mtime: *mtime,
                    size: *size,
                    symbols: symbols.clone(),
                },
            );
        }

        if let Some(ref mut db) = self.db
            && let Err(error) = db.update_files_symbols_batch(entries)
        {
            tracing::warn!(%error, "symbol index batch update failed");
        }
        self.record_index_update(high_water_mtime);
    }

    fn remove_missing_files(&mut self, existing_paths: &HashSet<String>) {
        self.files.retain(|path, _| existing_paths.contains(path));
        if let Some(ref mut db) = self.db
            && let Err(error) = db.remove_missing_files(existing_paths)
        {
            tracing::warn!(%error, "symbol index failed to remove deleted files");
        }
        self.status.indexed_file_count = self.db.as_ref().map_or(
            self.files.len(),
            db::SymbolIndexDatabase::indexed_file_count,
        );
    }

    pub fn get_symbols(
        &self,
        symbol: &str,
        symbol_type: Option<SymbolType>,
        limit: Option<usize>,
    ) -> Vec<SymbolLocation> {
        if self.disabled {
            return Vec::new();
        }

        if let Some(ref db) = self.db {
            return db.get_symbols_by_name(symbol, symbol_type, limit);
        }

        let mut results = Vec::new();
        for (rel_path, entry) in &self.files {
            for sym in &entry.symbols {
                if sym.name != symbol {
                    continue;
                }
                if let Some(st) = symbol_type
                    && sym.symbol_type != st
                {
                    continue;
                }
                let mut sym_clone = sym.clone();
                sym_clone.path = Some(rel_path.clone());
                results.push(sym_clone);
                if let Some(lim) = limit
                    && results.len() >= lim
                {
                    break;
                }
            }
        }
        results
    }

    pub(crate) fn get_references(&self, symbol: &str, limit: Option<usize>) -> Vec<SymbolLocation> {
        self.get_symbols(symbol, Some(SymbolType::Reference), limit)
    }

    pub(crate) fn get_definitions(
        &self,
        symbol: &str,
        limit: Option<usize>,
    ) -> Vec<SymbolLocation> {
        self.get_symbols(symbol, Some(SymbolType::Definition), limit)
    }

    pub fn get_project_root(&self) -> &str {
        &self.project_root
    }

    #[must_use]
    pub fn status(&self) -> SymbolIndexStatus {
        self.status.clone()
    }

    fn record_index_update(&mut self, mtime: u64) {
        self.status.indexed_file_count = self.db.as_ref().map_or(
            self.files.len(),
            db::SymbolIndexDatabase::indexed_file_count,
        );
        self.status.high_water_mtime = self.status.high_water_mtime.max(mtime);
        self.status.last_indexed_at = Some(SystemTime::now());
    }

    fn has_symbols_with_metadata(&self, rel_path: &str, mtime: u64, size: u64) -> bool {
        if let Some(entry) = self.files.get(rel_path)
            && entry.mtime == mtime
            && entry.size == size
            && !entry.symbols.is_empty()
        {
            return true;
        }

        self.db.as_ref().is_some_and(|db| {
            db.file_metadata(rel_path) == Some((mtime, size)) && db.file_has_symbols(rel_path)
        })
    }

    fn has_matching_metadata(&self, rel_path: &str, mtime: u64, size: u64) -> bool {
        self.files
            .get(rel_path)
            .is_some_and(|entry| entry.mtime == mtime && entry.size == size)
            || self
                .db
                .as_ref()
                .is_some_and(|db| db.file_metadata(rel_path) == Some((mtime, size)))
    }

    fn has_usable_persisted_index(&self) -> bool {
        self.db
            .as_ref()
            .is_some_and(|db| db.indexed_file_count() > 0)
    }

    fn mark_persisted_index_ready(&mut self) {
        self.status.workspace_file_count = self.status.indexed_file_count;
        self.status.initial_walk_complete = true;
    }

    fn begin_initial_walk(&mut self, workspace_file_count: usize) {
        self.status.workspace_file_count = workspace_file_count;
        self.status.indexed_file_count = 0;
        self.status.initial_walk_complete = false;
    }

    pub(crate) fn finish_initial_walk(&mut self, workspace_file_count: usize) {
        self.status.workspace_file_count = workspace_file_count;
        self.status.indexed_file_count = self.db.as_ref().map_or(
            self.files.len(),
            db::SymbolIndexDatabase::indexed_file_count,
        );
        self.status.initial_walk_complete = true;
        self.status.last_indexed_at = Some(SystemTime::now());
    }
}

pub fn start_initial_walk(service: Arc<Mutex<SymbolIndexService>>) {
    let project_root = {
        let service = service
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if service.is_disabled() {
            return;
        }
        service.get_project_root().to_string()
    };

    {
        let mut service = service
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if service.has_usable_persisted_index() {
            service.mark_persisted_index_ready();
            tracing::debug!(
                root = %project_root,
                files = service.status.indexed_file_count,
                "using persisted symbol index; startup refresh is deferred"
            );
        }
    }

    if !SYMBOL_INDEX_REFRESHING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(project_root.clone())
    {
        return;
    }

    let refresh_guard = SymbolIndexRefreshGuard {
        project_root: project_root.clone(),
    };
    let thread_name = format!("sned-symbol-index-{}", sanitize_thread_name(&project_root));
    let refresh_root = project_root.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let _refresh_guard = refresh_guard;
            let started = Instant::now();
            let result = run_initial_walk(&service, Path::new(&refresh_root));
            if let Err(error) = result {
                tracing::warn!(root = %refresh_root, %error, "symbol index initial walk failed");
            }
            let files = service
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .status()
                .indexed_file_count;
            tracing::info!(root = %refresh_root, files, elapsed_ms = started.elapsed().as_millis(), "symbol index initial walk indexed files");
        })
    {
        tracing::warn!(root = %project_root, %error, "symbol index walk thread was not started");
    }
}

fn sanitize_thread_name(project_root: &str) -> String {
    project_root
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .take(48)
        .collect()
}

pub async fn index_file_after_write(
    service: Arc<Mutex<SymbolIndexService>>,
    project_root: &Path,
    rel_path: &str,
    content: &str,
) {
    let project_root = project_root.to_path_buf();
    let rel_path = rel_path.to_string();
    let content = content.to_string();
    let indexed = tokio::task::spawn_blocking(move || {
        prepare_index_entry(&project_root, &rel_path, &content)
    })
    .await;

    match indexed {
        Ok(Ok(Some((rel_path, mtime, size, symbols)))) => {
            service
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .index_file_safe(&rel_path, mtime, size, &symbols);
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => tracing::warn!(%error, "symbol index post-write refresh failed"),
        Err(error) => tracing::warn!(%error, "symbol index post-write task failed"),
    }
}

fn run_initial_walk(
    service: &Arc<Mutex<SymbolIndexService>>,
    project_root: &Path,
) -> anyhow::Result<()> {
    if !is_git_worktree(project_root) {
        service
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish_initial_walk(0);
        return Ok(());
    }

    let persisted_index_ready = service
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .has_usable_persisted_index();

    let candidate_walk_started = Instant::now();
    let mut candidates = collect_index_candidates(project_root);
    let candidate_walk_ms = candidate_walk_started.elapsed().as_millis();
    let parser_load_started = Instant::now();
    let language_parsers = load_parsers_for_candidates(&candidates);
    candidates.retain(|candidate| {
        candidate
            .absolute_path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_lowercase)
            .is_some_and(|extension| language_parsers.contains_key(&extension))
    });
    let parser_load_ms = parser_load_started.elapsed().as_millis();

    {
        let mut index = service
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if persisted_index_ready {
            index.status.workspace_file_count = candidates.len();
        } else {
            index.begin_initial_walk(candidates.len());
        }
    }

    let mut pending = Vec::with_capacity(INDEX_BATCH_SIZE);
    let parse_started = Instant::now();
    let mut flush_elapsed = std::time::Duration::ZERO;
    for candidate in &candidates {
        let unchanged = service
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_matching_metadata(&candidate.relative_path, candidate.mtime, candidate.size);

        if !unchanged {
            match std::fs::read_to_string(&candidate.absolute_path) {
                Ok(content) => match parse_symbols_with_parsers(
                    &candidate.absolute_path,
                    &content,
                    &language_parsers,
                ) {
                    Ok(Some(symbols)) => {
                        pending.push((
                            candidate.relative_path.clone(),
                            candidate.mtime,
                            candidate.size,
                            symbols,
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(path = %candidate.absolute_path.display(), %error, "symbol index skipped unparsable file");
                    }
                },
                Err(error) => {
                    tracing::warn!(path = %candidate.absolute_path.display(), %error, "symbol index skipped unreadable file");
                }
            }
        }

        if pending.len() == INDEX_BATCH_SIZE {
            flush_elapsed += flush_initial_batch(service, &mut pending);
        }
    }
    flush_elapsed += flush_initial_batch(service, &mut pending);
    let candidate_paths = candidates
        .iter()
        .map(|candidate| candidate.relative_path.clone())
        .collect::<HashSet<_>>();
    service
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove_missing_files(&candidate_paths);
    tracing::debug!(
        candidates = candidates.len(),
        candidate_walk_ms,
        parser_load_ms,
        parse_ms = parse_started.elapsed().as_millis(),
        db_flush_ms = flush_elapsed.as_millis(),
        persisted_index_ready,
        "symbol index refresh phases"
    );
    service
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .finish_initial_walk(candidates.len());
    Ok(())
}

fn flush_initial_batch(
    service: &Arc<Mutex<SymbolIndexService>>,
    entries: &mut Vec<(String, u64, u64, Vec<SymbolLocation>)>,
) -> std::time::Duration {
    if entries.is_empty() {
        return std::time::Duration::ZERO;
    }
    let entries = std::mem::take(entries);
    let started = Instant::now();
    service
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .index_files_batch(&entries);
    started.elapsed()
}

fn is_git_worktree(project_root: &Path) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(project_root)
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
        .unwrap_or(false)
}

fn collect_index_candidates(project_root: &Path) -> Vec<IndexCandidate> {
    WalkBuilder::new(project_root)
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') {
                return false;
            }
            !entry.file_type().is_some_and(|file_type| {
                file_type.is_dir() && crate::core::file_search::is_excluded_dir(&name)
            })
        })
        .build()
        .flatten()
        .filter_map(|entry| {
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if metadata.len() > crate::core::tools::handlers::read_file::max_file_read_size() as u64
            {
                return None;
            }
            let path = entry.into_path();
            let relative_path = path
                .strip_prefix(project_root)
                .ok()?
                .to_string_lossy()
                .into_owned();
            let mtime = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_secs());
            Some(IndexCandidate {
                absolute_path: path,
                relative_path,
                mtime,
                size: metadata.len(),
            })
        })
        .collect()
}

fn load_parsers_for_candidates(
    candidates: &[IndexCandidate],
) -> crate::services::tree_sitter::LanguageParserMap {
    let mut parsers = crate::services::tree_sitter::LanguageParserMap::new();
    let mut extensions = std::collections::HashSet::new();

    for candidate in candidates {
        let Some(extension) = candidate
            .absolute_path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_lowercase)
        else {
            continue;
        };
        if !extensions.insert(extension) {
            continue;
        }

        if let Ok(loaded) =
            crate::services::tree_sitter::load_required_language_parsers(&[candidate
                .absolute_path
                .to_string_lossy()
                .as_ref()])
        {
            parsers.extend(loaded);
        }
    }

    parsers
}

fn prepare_index_entry(
    project_root: &Path,
    rel_path: &str,
    content: &str,
) -> anyhow::Result<Option<(String, u64, u64, Vec<SymbolLocation>)>> {
    let absolute_path = project_root.join(rel_path);
    let metadata = std::fs::metadata(&absolute_path)?;
    if metadata.len() > crate::core::tools::handlers::read_file::max_file_read_size() as u64 {
        return Ok(None);
    }
    let Some(symbols) = parse_symbols(&absolute_path, content)? else {
        return Ok(None);
    };
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs());
    Ok(Some((rel_path.to_string(), mtime, metadata.len(), symbols)))
}

fn load_parser_for_path(path: &Path) -> Option<crate::services::tree_sitter::LanguageParserMap> {
    crate::services::tree_sitter::load_required_language_parsers(&[path.to_string_lossy().as_ref()])
        .ok()
}

fn parse_symbols(path: &Path, content: &str) -> anyhow::Result<Option<Vec<SymbolLocation>>> {
    let Some(parsers) = load_parser_for_path(path) else {
        return Ok(None);
    };
    parse_symbols_with_parsers(path, content, &parsers)
}

fn parse_symbols_with_parsers(
    path: &Path,
    content: &str,
    parsers: &crate::services::tree_sitter::LanguageParserMap,
) -> anyhow::Result<Option<Vec<SymbolLocation>>> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if !parsers.contains_key(&extension) {
        return Ok(None);
    }
    extract_symbols_for_indexing(path.to_string_lossy().as_ref(), content, parsers).map(Some)
}

/// Extract symbols from file content for indexing.
/// Exposed for use by tool handlers that need to parse symbols outside the index lock.
pub fn extract_symbols_for_indexing(
    path: &str,
    content: &str,
    language_parsers: &crate::services::tree_sitter::LanguageParserMap,
) -> anyhow::Result<Vec<SymbolLocation>> {
    use streaming_iterator::StreamingIterator;

    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let Some(entry) = language_parsers.get(&ext) else {
        return Ok(Vec::new());
    };

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&entry.language)
        .map_err(|e| anyhow::anyhow!("Language error: {e}"))?;

    let tree = parser
        .parse(content, None)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse"))?;

    let root = tree.root_node();
    let bytes = content.as_bytes();

    let mut symbols = Vec::new();
    let mut query_cursor = tree_sitter::QueryCursor::new();
    let mut captures = query_cursor.captures(&entry.query, root, bytes);

    while let Some((m, ci)) = captures.next() {
        let cap = m.captures[*ci];
        let cap_name = entry.query.capture_names()[cap.index as usize];

        if cap_name.starts_with("name.reference") || cap_name.contains("name.definition") {
            let name_text = match cap.node.utf8_text(bytes) {
                Ok(t) => t.to_string(),
                Err(_) => continue,
            };

            let kind = cap_name.split('.').next_back().map(String::from);
            let symbol_type = if cap_name.contains("name.definition") {
                SymbolType::Definition
            } else {
                SymbolType::Reference
            };

            let start_pos = cap.node.start_position();
            let end_pos = cap.node.end_position();

            symbols.push(SymbolLocation {
                path: None,
                name: name_text,
                start_line: start_pos.row,
                start_column: start_pos.column,
                end_line: end_pos.row,
                end_column: end_pos.column,
                symbol_type,
                kind,
            });
        }
    }

    Ok(symbols)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_symbol(name: &str, line: usize, st: SymbolType) -> SymbolLocation {
        SymbolLocation {
            path: None,
            name: name.to_string(),
            start_line: line,
            start_column: 0,
            end_line: line,
            end_column: name.len(),
            symbol_type: st,
            kind: None,
        }
    }

    #[test]
    fn test_symbol_index_basic() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        let symbols = vec![make_symbol("test_func", 10, SymbolType::Definition)];
        service.index_file("src/main.rs", 1234567890, 1024, &symbols);
        let defs = service.get_definitions("test_func", None);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].start_line, 10);
    }

    #[test]
    fn test_get_symbols_comparison_regression() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        let symbols = vec![
            make_symbol("foo", 1, SymbolType::Definition),
            make_symbol("foobar", 2, SymbolType::Definition),
            make_symbol("foo", 3, SymbolType::Reference),
        ];
        service.index_file("src/main.rs", 1234567890, 1024, &symbols);

        let foo_results = service.get_symbols("foo", None, None);
        assert_eq!(foo_results.len(), 2);

        let foobar_results = service.get_symbols("foobar", None, None);
        assert_eq!(foobar_results.len(), 1);
        assert_eq!(foobar_results[0].start_line, 2);

        let missing = service.get_symbols("nonexistent", None, None);
        assert!(missing.is_empty());

        let empty = service.get_symbols("", None, None);
        assert!(empty.is_empty());
    }

    #[test]
    fn test_get_symbols_with_type_filter() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        let symbols = vec![
            make_symbol("my_symbol", 1, SymbolType::Definition),
            make_symbol("my_symbol", 5, SymbolType::Reference),
        ];
        service.index_file("src/lib.rs", 1234567890, 1024, &symbols);

        let defs = service.get_definitions("my_symbol", None);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].symbol_type, SymbolType::Definition);

        let refs = service.get_references("my_symbol", None);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].symbol_type, SymbolType::Reference);
    }

    #[test]
    fn test_get_symbols_limit() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        let symbols: Vec<_> = (0..10)
            .map(|i| SymbolLocation {
                path: None,
                name: "repeated".to_string(),
                start_line: i,
                start_column: 0,
                end_line: i,
                end_column: 8,
                symbol_type: SymbolType::Reference,
                kind: None,
            })
            .collect();
        service.index_file("src/main.rs", 1234567890, 1024, &symbols);

        let all = service.get_symbols("repeated", None, None);
        assert_eq!(all.len(), 10);

        let limited = service.get_symbols("repeated", None, Some(3));
        assert_eq!(limited.len(), 3);
    }

    #[test]
    fn test_safe_indexing_preserves_matching_known_symbols() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        service.index_file(
            "src/lib.rs",
            10,
            20,
            &[make_symbol("stable_symbol", 1, SymbolType::Definition)],
        );

        service.index_file_safe("src/lib.rs", 10, 20, &[]);

        assert_eq!(service.get_definitions("stable_symbol", None).len(), 1);
    }

    #[tokio::test]
    async fn test_index_file_after_write_refreshes_symbols() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lib.rs");
        let content = "fn refreshed_symbol() {}\n";
        std::fs::write(&path, content).unwrap();
        let service = Arc::new(Mutex::new(SymbolIndexService::new(
            temp.path().to_string_lossy().into_owned(),
        )));

        index_file_after_write(Arc::clone(&service), temp.path(), "lib.rs", content).await;

        assert_eq!(
            service
                .lock()
                .unwrap()
                .get_definitions("refreshed_symbol", None)
                .len(),
            1
        );
    }

    #[test]
    fn test_initial_walk_indexes_supported_files_only() {
        let temp = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::create_dir_all(temp.path().join("target")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "fn indexed_symbol() {}\n").unwrap();
        std::fs::write(
            temp.path().join("target/ignored.rs"),
            "fn ignored_symbol() {}\n",
        )
        .unwrap();
        std::fs::write(temp.path().join(".hidden.rs"), "fn hidden_symbol() {}\n").unwrap();

        let service = Arc::new(Mutex::new(SymbolIndexService::new(
            temp.path().to_string_lossy().into_owned(),
        )));
        run_initial_walk(&service, temp.path()).unwrap();

        let service = service.lock().unwrap();
        assert_eq!(service.get_definitions("indexed_symbol", None).len(), 1);
        assert!(service.get_definitions("ignored_symbol", None).is_empty());
        assert!(service.get_definitions("hidden_symbol", None).is_empty());
        assert!(service.status().initial_walk_complete);
        assert_eq!(service.status().workspace_file_count, 1);
    }

    #[test]
    fn test_persisted_index_reconciles_deleted_workspace_files() {
        let temp = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        let source = temp.path().join("src/lib.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "fn removed_symbol() {}\n").unwrap();

        let first = Arc::new(Mutex::new(
            SymbolIndexService::new(temp.path().to_string_lossy().into_owned())
                .with_persistence()
                .unwrap(),
        ));
        run_initial_walk(&first, temp.path()).unwrap();
        assert_eq!(
            first.lock().unwrap().get_definitions("removed_symbol", None).len(),
            1
        );
        drop(first);

        std::fs::remove_file(source).unwrap();
        let reconciled = Arc::new(Mutex::new(
            SymbolIndexService::new(temp.path().to_string_lossy().into_owned())
                .with_persistence()
                .unwrap(),
        ));
        run_initial_walk(&reconciled, temp.path()).unwrap();

        assert!(
            reconciled
                .lock()
                .unwrap()
                .get_definitions("removed_symbol", None)
                .is_empty(),
            "reconciliation must prune symbols for deleted files"
        );
    }

    #[test]
    fn test_non_git_root_finishes_without_walking() {
        let temp = tempfile::tempdir().unwrap();
        let service = Arc::new(Mutex::new(SymbolIndexService::new(
            temp.path().to_string_lossy().into_owned(),
        )));

        run_initial_walk(&service, temp.path()).unwrap();

        let status = service.lock().unwrap().status();
        assert!(status.initial_walk_complete);
        assert_eq!(status.workspace_file_count, 0);
    }

    #[test]
    fn test_initial_walk_status_counts_indexed_files_separately() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        service.index_file(
            "src/main.rs",
            123,
            10,
            &[make_symbol("main", 0, SymbolType::Definition)],
        );

        service.finish_initial_walk(3);
        let status = service.status();

        assert_eq!(status.workspace_file_count, 3);
        assert_eq!(status.indexed_file_count, 1);
    }

    #[test]
    fn test_persisted_index_is_ready_before_workspace_refresh() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        service.status.indexed_file_count = 42;

        service.mark_persisted_index_ready();

        let status = service.status();
        assert!(status.initial_walk_complete);
        assert_eq!(status.workspace_file_count, 42);
    }

    #[test]
    fn test_get_symbols_across_multiple_files() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        service.index_file(
            "src/a.rs",
            1234567890,
            100,
            &[make_symbol("shared", 1, SymbolType::Definition)],
        );
        service.index_file(
            "src/b.rs",
            1234567891,
            100,
            &[make_symbol("shared", 10, SymbolType::Reference)],
        );

        let results = service.get_symbols("shared", None, None);
        assert_eq!(results.len(), 2);

        let lines: Vec<_> = results.iter().map(|s| s.start_line).collect();
        assert!(lines.contains(&1));
        assert!(lines.contains(&10));
    }

    #[test]
    fn test_db_backed_get_returns_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let mut service = SymbolIndexService::new(root).with_persistence().unwrap();

        service.index_file(
            "src/a.rs",
            100,
            50,
            &[make_symbol("sym_a", 5, SymbolType::Definition)],
        );
        service.index_file(
            "src/b.rs",
            200,
            60,
            &[make_symbol("sym_a", 10, SymbolType::Reference)],
        );

        let results = service.get_symbols("sym_a", None, None);
        assert_eq!(results.len(), 2);
        let paths: Vec<_> = results.iter().filter_map(|r| r.path.clone()).collect();
        assert!(paths.contains(&"src/a.rs".to_string()));
        assert!(paths.contains(&"src/b.rs".to_string()));
    }

    #[test]
    fn test_extract_symbols_rust() {
        let content = "fn hello() {}\nstruct Foo {}\n";
        let parsers =
            crate::services::tree_sitter::load_required_language_parsers(&["test.rs"]).unwrap();
        let symbols = extract_symbols_for_indexing("test.rs", content, &parsers).unwrap();
        assert!(!symbols.is_empty());
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"), "Expected 'hello' in {:?}", names);
    }

    #[test]
    fn test_service_recovers_after_panic_during_index_update() {
        use std::sync::Arc;

        let service = Arc::new(parking_lot::Mutex::new(SymbolIndexService::new(
            "/tmp/test_panic_recovery".to_string(),
        )));

        {
            let mut svc = service.lock();
            svc.index_file(
                "src/existing.rs",
                1234567890,
                1024,
                &[make_symbol("existing_sym", 10, SymbolType::Definition)],
            );
        }

        let service_clone = service.clone();
        let _ = std::thread::spawn(move || {
            let mut svc = service_clone.lock();
            svc.index_file(
                "src/panic.rs",
                0,
                0,
                &[make_symbol("during_panic", 1, SymbolType::Definition)],
            );
            panic!("simulated panic during index update");
        })
        .join();

        let mut svc = service.lock();
        let defs = svc.get_definitions("existing_sym", None);
        assert_eq!(defs.len(), 1, "service should still have pre-panic symbols");
        assert_eq!(defs[0].start_line, 10);

        svc.index_file(
            "src/post_panic.rs",
            1234567891,
            256,
            &[make_symbol("post_panic_sym", 5, SymbolType::Definition)],
        );
        drop(svc);

        let svc = service.lock();
        let post_defs = svc.get_definitions("post_panic_sym", None);
        assert_eq!(
            post_defs.len(),
            1,
            "service should be functional after panic: can add new symbols"
        );
        assert_eq!(post_defs[0].start_line, 5);
    }

    #[test]
    fn test_with_persistence_fallback_on_corrupted_db() {
        use std::fs;
        use std::io::Write;

        let temp_dir = "/tmp/test_corrupted_db_fallback";
        let _ = fs::remove_dir_all(temp_dir);
        fs::create_dir_all(temp_dir).unwrap();

        // Create corrupted DB file (invalid SQLite header)
        let db_dir = std::path::Path::new(temp_dir).join(INDEX_DIR);
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(DB_FILENAME);
        {
            let mut f = fs::File::create(&db_path).unwrap();
            // Write garbage that is not a valid SQLite database
            f.write_all(b"This is not a valid SQLite database file")
                .unwrap();
        }

        // Create service and attempt to open with persistence
        // This should fail and we would normally fall back to memory mode
        let service = SymbolIndexService::new(temp_dir.to_string());
        let result = service.with_persistence();

        // The result should be an error due to corrupted DB
        assert!(
            result.is_err(),
            "with_persistence() should fail on corrupted DB"
        );

        // Clean up
        let _ = fs::remove_dir_all(temp_dir);
    }
}
