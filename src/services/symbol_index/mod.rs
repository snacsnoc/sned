//! Symbol index for fast symbol lookup across a codebase.
//!
//! Backed by SQLite for persistence (see `db` module).

pub mod db;

use rayon::prelude::*;
use std::collections::HashMap;

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

/// Symbol index service with optional SQLite persistence.
#[derive(Debug)]
pub struct SymbolIndexService {
    files: HashMap<String, FileIndexEntry>,
    project_root: String,
    db: Option<db::SymbolIndexDatabase>,
    disabled: bool,
}

pub const INDEX_DIR: &str = ".sned-symbol-index";
pub const DB_FILENAME: &str = "data.db";
const FILES_PER_BATCH: usize = 50;

impl SymbolIndexService {
    pub fn new(project_root: String) -> Self {
        Self {
            files: HashMap::new(),
            project_root,
            db: None,
            disabled: false,
        }
    }

    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self.db = None;
        self.files.clear();
        self
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub fn with_persistence(mut self) -> anyhow::Result<Self> {
        if self.disabled {
            return Ok(self);
        }

        let db_dir = std::path::Path::new(&self.project_root).join(INDEX_DIR);
        std::fs::create_dir_all(&db_dir)?;

        let git_exclude = std::path::Path::new(&self.project_root)
            .join(".git")
            .join("info")
            .join("exclude");
        if git_exclude.parent().map(|p| p.exists()).unwrap_or(false)
            && let Ok(content) = std::fs::read_to_string(&git_exclude)
            && !content.contains(INDEX_DIR)
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&git_exclude) {
                let _ = writeln!(f, "{}", INDEX_DIR);
            }
        }

        let db_path = db_dir.join(DB_FILENAME);
        let database = db::SymbolIndexDatabase::open(&db_path)?;
        self.db = Some(database);
        Ok(self)
    }

    pub fn index_file(
        &mut self,
        rel_path: String,
        mtime: u64,
        size: u64,
        symbols: Vec<SymbolLocation>,
    ) {
        if self.disabled {
            return;
        }

        self.store_indexed_files(vec![(rel_path, mtime, size, symbols)]);
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

    pub fn get_references(&self, symbol: &str, limit: Option<usize>) -> Vec<SymbolLocation> {
        self.get_symbols(symbol, Some(SymbolType::Reference), limit)
    }

    pub fn get_definitions(&self, symbol: &str, limit: Option<usize>) -> Vec<SymbolLocation> {
        self.get_symbols(symbol, Some(SymbolType::Definition), limit)
    }

    pub fn remove_file(&mut self, rel_path: &str) {
        if self.disabled {
            return;
        }

        if let Some(ref mut db) = self.db
            && let Err(e) = db.remove_file(rel_path)
        {
            tracing::warn!(error = %e, "symbol_index failed to remove file from DB");
        }
        self.files.remove(rel_path);
    }

    pub fn get_project_root(&self) -> &str {
        &self.project_root
    }

    pub fn get_file_metadata(&self, rel_path: &str) -> Option<(u64, u64)> {
        if self.disabled {
            return None;
        }

        if let Some(ref db) = self.db {
            return db.get_file_metadata(rel_path);
        }
        self.files.get(rel_path).map(|e| (e.mtime, e.size))
    }

    pub fn symbol_count(&self) -> usize {
        if self.disabled {
            return 0;
        }

        if let Some(ref db) = self.db {
            return db.symbol_count();
        }
        self.files.values().map(|e| e.symbols.len()).sum()
    }

    pub fn get_file_symbols(&self, rel_path: &str) -> Vec<SymbolLocation> {
        if self.disabled {
            return Vec::new();
        }

        if let Some(ref db) = self.db {
            return db.get_file_symbols(rel_path);
        }
        self.files
            .get(rel_path)
            .map(|e| e.symbols.clone())
            .unwrap_or_default()
    }

    pub fn file_count(&self) -> usize {
        if self.disabled {
            return 0;
        }

        if let Some(ref db) = self.db {
            return db.file_count();
        }
        self.files.len()
    }

    pub fn vacuum(&self) {
        if self.disabled {
            return;
        }

        if let Some(ref db) = self.db {
            db.vacuum();
        }
    }

    pub fn initialize(&mut self) -> anyhow::Result<()> {
        if self.disabled {
            return Ok(());
        }

        if self.db.is_none() {
            return Ok(());
        }

        let root = std::path::Path::new(&self.project_root);
        let index_dir = root.join(INDEX_DIR);
        let index_dir_str = index_dir.to_str().unwrap_or("");

        let mut files_to_update: Vec<(std::path::PathBuf, String)> = Vec::new();

        for result in ignore::WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build()
        {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let abs_str = match path.to_str() {
                Some(s) => s,
                None => continue,
            };

            if abs_str.starts_with(index_dir_str) {
                continue;
            }

            if !should_index_file(abs_str) {
                continue;
            }

            let rel = path.strip_prefix(root).unwrap_or(path);
            let rel_str = rel.to_str().unwrap_or("").to_string();

            let needs_update = match self.db.as_ref() {
                Some(db) => {
                    let meta = match std::fs::metadata(path) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let size = meta.len();

                    match db.get_file_metadata(&rel_str) {
                        Some((db_mtime, db_size)) => db_mtime != mtime || db_size != size,
                        None => true,
                    }
                }
                None => true,
            };

            if needs_update {
                files_to_update.push((path.to_path_buf(), rel_str));
            }
        }

        for batch in files_to_update.chunks(FILES_PER_BATCH) {
            self.index_files_batch(batch)?;
        }

        if let Some(ref db) = self.db {
            db.vacuum();
        }

        Ok(())
    }

    fn index_files_batch(
        &mut self,
        files_to_update: &[(std::path::PathBuf, String)],
    ) -> anyhow::Result<()> {
        if self.disabled {
            return Ok(());
        }

        if files_to_update.is_empty() {
            return Ok(());
        }

        let absolute_paths: Vec<String> = files_to_update
            .iter()
            .filter_map(|(path, _)| path.to_str().map(|s| s.to_string()))
            .collect();

        let language_parsers =
            match crate::services::tree_sitter::load_required_language_parsers(&absolute_paths) {
                Ok(parsers) => parsers,
                Err(_) => return Ok(()),
            };

        // Use rayon for parallel file indexing with automatic work-stealing
        let indexed_entries: Vec<(usize, String, u64, u64, Vec<SymbolLocation>)> = files_to_update
            .par_iter()
            .enumerate()
            .filter_map(|(index, (absolute_path, rel_path))| {
                let absolute_str = absolute_path.to_str()?;

                let Ok(content) = std::fs::read_to_string(absolute_path) else {
                    return None;
                };

                let Ok(symbols) = extract_symbols(absolute_str, &content, &language_parsers) else {
                    return None;
                };

                let meta = std::fs::metadata(absolute_path).ok();
                let mtime = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size = meta.map(|m| m.len()).unwrap_or(0);

                Some((index, rel_path.clone(), mtime, size, symbols))
            })
            .collect();

        self.store_indexed_files(
            indexed_entries
                .into_iter()
                .map(|(_, rel_path, mtime, size, symbols)| (rel_path, mtime, size, symbols))
                .collect(),
        );
        Ok(())
    }

    fn store_indexed_files(&mut self, entries: Vec<(String, u64, u64, Vec<SymbolLocation>)>) {
        if self.disabled {
            return;
        }

        if entries.is_empty() {
            return;
        }

        if let Some(ref mut db) = self.db
            && let Err(e) = db.update_files_symbols_batch(&entries)
        {
            tracing::warn!(error = %e, "symbol_index failed to batch update symbols");
        }

        for (rel_path, mtime, size, symbols) in entries {
            self.files.insert(
                rel_path,
                FileIndexEntry {
                    mtime,
                    size,
                    symbols,
                },
            );
        }
    }

    pub fn update_file(&mut self, absolute_path: &str) -> anyhow::Result<()> {
        if self.disabled {
            return Ok(());
        }

        let root = std::path::Path::new(&self.project_root);
        let abs_path = std::path::Path::new(absolute_path);
        let rel = abs_path.strip_prefix(root).unwrap_or(abs_path);
        let rel_str = rel.to_str().unwrap_or("").to_string();

        if !should_index_file(absolute_path) {
            return Ok(());
        }

        let content = std::fs::read_to_string(absolute_path)?;
        let parsers =
            crate::services::tree_sitter::load_required_language_parsers(&[absolute_path])?;
        let symbols = extract_symbols(absolute_path, &content, &parsers)?;

        let meta = std::fs::metadata(absolute_path)?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let size = meta.len();

        self.index_file(rel_str, mtime, size, symbols);
        Ok(())
    }
}

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "c", "cpp", "h", "hpp", "cs", "rb", "php",
    "swift",
];

fn should_index_file(path: &str) -> bool {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    SUPPORTED_EXTENSIONS.contains(&ext.as_str())
}

fn extract_symbols(
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

    let entry = match language_parsers.get(&ext) {
        Some(e) => e,
        None => return Ok(Vec::new()),
    };

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&entry.language)
        .map_err(|e| anyhow::anyhow!("Language error: {}", e))?;

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
        service.index_file("src/main.rs".to_string(), 1234567890, 1024, symbols);
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
        service.index_file("src/main.rs".to_string(), 1234567890, 1024, symbols);

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
        service.index_file("src/lib.rs".to_string(), 1234567890, 1024, symbols);

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
        service.index_file("src/main.rs".to_string(), 1234567890, 1024, symbols);

        let all = service.get_symbols("repeated", None, None);
        assert_eq!(all.len(), 10);

        let limited = service.get_symbols("repeated", None, Some(3));
        assert_eq!(limited.len(), 3);
    }

    #[test]
    fn test_get_symbols_across_multiple_files() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        service.index_file(
            "src/a.rs".to_string(),
            1234567890,
            100,
            vec![make_symbol("shared", 1, SymbolType::Definition)],
        );
        service.index_file(
            "src/b.rs".to_string(),
            1234567891,
            100,
            vec![make_symbol("shared", 10, SymbolType::Reference)],
        );

        let results = service.get_symbols("shared", None, None);
        assert_eq!(results.len(), 2);

        let lines: Vec<_> = results.iter().map(|s| s.start_line).collect();
        assert!(lines.contains(&1));
        assert!(lines.contains(&10));
    }

    #[test]
    fn test_symbol_index_remove_file() {
        let mut service = SymbolIndexService::new("/tmp/test".to_string());
        service.index_file(
            "src/main.rs".to_string(),
            1234567890,
            100,
            vec![make_symbol("test", 1, SymbolType::Definition)],
        );
        assert_eq!(service.get_definitions("test", None).len(), 1);
        service.remove_file("src/main.rs");
        assert!(service.get_definitions("test", None).is_empty());
    }

    #[test]
    fn test_with_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let mut service = SymbolIndexService::new(root.clone())
            .with_persistence()
            .unwrap();

        let symbols = vec![make_symbol("persisted", 1, SymbolType::Definition)];
        service.index_file("src/lib.rs".to_string(), 100, 50, symbols);
        assert_eq!(service.symbol_count(), 1);
        assert_eq!(service.file_count(), 1);

        let results = service.get_symbols("persisted", None, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn test_disabled_mode_never_indexes_or_persists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file_path = src_dir.join("lib.rs");
        std::fs::write(&file_path, "fn should_not_be_indexed() {}\n").unwrap();

        let mut service = SymbolIndexService::new(root.to_string_lossy().to_string()).disabled();
        assert!(service.is_disabled());

        service = service.with_persistence().unwrap();
        assert!(!root.join(INDEX_DIR).exists());

        service.initialize().unwrap();
        assert_eq!(service.file_count(), 0);
        assert_eq!(service.symbol_count(), 0);
        assert!(
            service
                .get_definitions("should_not_be_indexed", None)
                .is_empty()
        );
        assert!(service.get_file_symbols("src/lib.rs").is_empty());
        assert_eq!(service.get_file_metadata("src/lib.rs"), None);

        service.index_file(
            "src/manual.rs".to_string(),
            1,
            10,
            vec![make_symbol("manual", 1, SymbolType::Definition)],
        );
        service.update_file("/path/that/does/not/exist.rs").unwrap();
        service.remove_file("src/manual.rs");

        assert_eq!(service.file_count(), 0);
        assert_eq!(service.symbol_count(), 0);
        assert!(service.get_symbols("manual", None, None).is_empty());
        assert!(!root.join(INDEX_DIR).exists());
    }

    #[test]
    fn test_memory_mode_indexes_without_persisting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file_path = src_dir.join("lib.rs");
        std::fs::write(&file_path, "fn in_memory_symbol() {}\n").unwrap();

        let mut service = SymbolIndexService::new(root.to_string_lossy().to_string());
        service.update_file(file_path.to_str().unwrap()).unwrap();

        let defs = service.get_definitions("in_memory_symbol", None);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(service.file_count(), 1);
        assert!(service.symbol_count() >= 1);
        assert!(!root.join(INDEX_DIR).exists());
    }

    #[test]
    fn test_store_indexed_files_batch_updates_cache_and_db() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let mut service = SymbolIndexService::new(root).with_persistence().unwrap();

        let entries = vec![
            (
                "src/a.rs".to_string(),
                100,
                50,
                vec![make_symbol("alpha", 1, SymbolType::Definition)],
            ),
            (
                "src/b.rs".to_string(),
                200,
                60,
                vec![make_symbol("beta", 2, SymbolType::Reference)],
            ),
        ];

        service.store_indexed_files(entries);

        assert_eq!(service.file_count(), 2);
        assert_eq!(service.symbol_count(), 2);

        let alpha = service.get_definitions("alpha", None);
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].path.as_deref(), Some("src/a.rs"));

        let beta = service.get_references("beta", None);
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].path.as_deref(), Some("src/b.rs"));
    }

    #[test]
    fn test_index_files_batch_handles_more_than_one_scan_batch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        let mut service = SymbolIndexService::new(root.to_str().unwrap().to_string())
            .with_persistence()
            .unwrap();

        let mut entries = Vec::new();
        for i in 0..60 {
            let file_path = src_dir.join(format!("file_{i}.rs"));
            std::fs::write(&file_path, format!("fn file_{i}() {{}}\n")).unwrap();
            entries.push((file_path, format!("src/file_{i}.rs")));
        }

        service.index_files_batch(&entries).unwrap();

        assert_eq!(service.file_count(), 60);
        assert_eq!(service.get_definitions("file_0", None).len(), 1);
        assert_eq!(service.get_definitions("file_59", None).len(), 1);
        assert!(service.symbol_count() >= 60);
    }

    #[test]
    fn test_db_backed_get_returns_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let mut service = SymbolIndexService::new(root).with_persistence().unwrap();

        service.index_file(
            "src/a.rs".to_string(),
            100,
            50,
            vec![make_symbol("sym_a", 5, SymbolType::Definition)],
        );
        service.index_file(
            "src/b.rs".to_string(),
            200,
            60,
            vec![make_symbol("sym_a", 10, SymbolType::Reference)],
        );

        let results = service.get_symbols("sym_a", None, None);
        assert_eq!(results.len(), 2);
        let paths: Vec<_> = results.iter().filter_map(|r| r.path.clone()).collect();
        assert!(paths.contains(&"src/a.rs".to_string()));
        assert!(paths.contains(&"src/b.rs".to_string()));
    }

    #[test]
    fn test_should_index_file() {
        assert!(should_index_file("src/main.rs"));
        assert!(should_index_file("lib.ts"));
        assert!(should_index_file("app.py"));
        assert!(!should_index_file("README.md"));
        assert!(!should_index_file("config.json"));
        assert!(!should_index_file("Makefile"));
    }

    #[test]
    fn test_extract_symbols_rust() {
        let content = "fn hello() {}\nstruct Foo {}\n";
        let parsers =
            crate::services::tree_sitter::load_required_language_parsers(&["test.rs"]).unwrap();
        let symbols = extract_symbols("test.rs", content, &parsers).unwrap();
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
                "src/existing.rs".to_string(),
                1234567890,
                1024,
                vec![make_symbol("existing_sym", 10, SymbolType::Definition)],
            );
        }

        let service_clone = service.clone();
        let _ = std::thread::spawn(move || {
            let mut svc = service_clone.lock();
            svc.index_file(
                "src/panic.rs".to_string(),
                0,
                0,
                vec![make_symbol("during_panic", 1, SymbolType::Definition)],
            );
            panic!("simulated panic during index update");
        })
        .join();

        let mut svc = service.lock();
        let defs = svc.get_definitions("existing_sym", None);
        assert_eq!(defs.len(), 1, "service should still have pre-panic symbols");
        assert_eq!(defs[0].start_line, 10);

        svc.index_file(
            "src/post_panic.rs".to_string(),
            1234567891,
            256,
            vec![make_symbol("post_panic_sym", 5, SymbolType::Definition)],
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
    fn test_mtime_comparison_no_spurious_reindex() {
        use crate::services::symbol_index::db::SymbolIndexDatabase;
        use std::fs;

        let temp_dir = "/tmp/test_mtime_comparison";
        let _ = fs::remove_dir_all(temp_dir);
        fs::create_dir_all(temp_dir).unwrap();
        let db_path = std::path::PathBuf::from(format!("{}/symbols.db", temp_dir));

        let mut db = SymbolIndexDatabase::open(&db_path).unwrap();

        let test_mtime: u64 = 1234567890;
        let test_size: u64 = 1024;

        db.update_file_symbols("test.rs", test_mtime, test_size, &[])
            .unwrap();

        let (stored_mtime, stored_size) = db.get_file_metadata("test.rs").unwrap();

        assert_eq!(
            stored_mtime, test_mtime,
            "DB should store mtime in seconds, not milliseconds"
        );
        assert_eq!(stored_size, test_size);

        let needs_update = stored_mtime != test_mtime || stored_size != test_size;
        assert!(
            !needs_update,
            "File with same mtime/size should not need update (no spurious re-index)"
        );

        let needs_update_different = stored_mtime != (test_mtime + 1) || stored_size != test_size;
        assert!(
            needs_update_different,
            "File with different mtime should need update"
        );

        let _ = fs::remove_dir_all(temp_dir);
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
            f.write_all(b"This is not a valid SQLite database file").unwrap();
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
