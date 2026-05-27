use crate::services::symbol_index::{SymbolLocation, SymbolType};
use rusqlite::{Connection, params};

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT NOT NULL,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    kind TEXT,
    start_line INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    end_column INTEGER NOT NULL,
    FOREIGN KEY (file_path) REFERENCES files(path) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file_path ON symbols(file_path);
";

pub struct SymbolIndexDatabase {
    conn: Connection,
    dirty: bool,
}

impl std::fmt::Debug for SymbolIndexDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolIndexDatabase")
            .field("dirty", &self.dirty)
            .field("conn", &"rusqlite::Connection { ... }")
            .finish()
    }
}

impl SymbolIndexDatabase {
    pub fn open(db_path: &std::path::Path) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, dirty: false })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, dirty: false })
    }

    pub fn update_file_symbols(
        &mut self,
        rel_path: &str,
        mtime: u64,
        size: u64,
        symbols: &[SymbolLocation],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM symbols WHERE file_path = ?1",
            params![rel_path],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO files (path, mtime, size) VALUES (?1, ?2, ?3)",
            params![rel_path, mtime, size],
        )?;

        for sym in symbols {
            let type_str = match sym.symbol_type {
                SymbolType::Definition => "definition",
                SymbolType::Reference => "reference",
            };
            tx.execute(
                "INSERT INTO symbols (file_path, name, type, kind, start_line, start_column, end_line, end_column) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    rel_path,
                    sym.name,
                    type_str,
                    sym.kind,
                    sym.start_line,
                    sym.start_column,
                    sym.end_line,
                    sym.end_column,
                ],
            )?;
        }

        tx.commit()?;
        self.dirty = true;
        Ok(())
    }

    pub fn update_files_symbols_batch(
        &mut self,
        entries: &[(String, u64, u64, Vec<SymbolLocation>)],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;

        for (rel_path, mtime, size, symbols) in entries {
            tx.execute(
                "DELETE FROM symbols WHERE file_path = ?1",
                params![rel_path],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO files (path, mtime, size) VALUES (?1, ?2, ?3)",
                params![rel_path, mtime, size],
            )?;

            for sym in symbols {
                let type_str = match sym.symbol_type {
                    SymbolType::Definition => "definition",
                    SymbolType::Reference => "reference",
                };
                tx.execute(
                    "INSERT INTO symbols (file_path, name, type, kind, start_line, start_column, end_line, end_column) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        rel_path,
                        sym.name,
                        type_str,
                        sym.kind,
                        sym.start_line,
                        sym.start_column,
                        sym.end_line,
                        sym.end_column,
                    ],
                )?;
            }
        }

        tx.commit()?;
        self.dirty = true;
        Ok(())
    }

    pub fn get_symbols_by_name(
        &self,
        name: &str,
        symbol_type: Option<SymbolType>,
        limit: Option<usize>,
    ) -> Vec<SymbolLocation> {
        let mut sql = String::from(
            "SELECT file_path, name, type, kind, start_line, start_column, end_line, end_column FROM symbols WHERE name = ?1",
        );
        let mut param_idx = 2;

        if symbol_type.is_some() {
            sql.push_str(&format!(" AND type = ?{}", param_idx));
            param_idx += 1;
        }
        if limit.is_some() {
            sql.push_str(&format!(" LIMIT ?{}", param_idx));
        }

        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "symbol_index failed to prepare query");
                return Vec::new();
            }
        };

        let type_str = symbol_type.map(|st| match st {
            SymbolType::Definition => "definition",
            SymbolType::Reference => "reference",
        });

        macro_rules! collect_rows {
            ($query:expr) => {
                match $query {
                    Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                    Err(e) => {
                        tracing::warn!(error = %e, "symbol_index query failed");
                        Vec::new()
                    }
                }
            };
        }

        match (symbol_type, limit) {
            (None, None) => collect_rows!(stmt.query_map(params![name], row_to_symbol_location)),
            (Some(_), None) => {
                collect_rows!(stmt.query_map(params![name, type_str.unwrap()], |row| {
                    row_to_symbol_location(row)
                }))
            }
            (None, Some(_)) => {
                collect_rows!(stmt.query_map(params![name, limit.unwrap() as i64], |row| {
                    row_to_symbol_location(row)
                }))
            }
            (Some(_), Some(_)) => collect_rows!(stmt.query_map(
                params![name, type_str.unwrap(), limit.unwrap() as i64],
                row_to_symbol_location,
            )),
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn set_dirty(&mut self, dirty: bool) {
        self.dirty = dirty;
    }
}

fn row_to_symbol_location(row: &rusqlite::Row) -> rusqlite::Result<SymbolLocation> {
    let file_path: String = row.get(0)?;
    let name: String = row.get(1)?;
    let type_str: String = row.get(2)?;
    let kind: Option<String> = row.get(3)?;
    let start_line: usize = row.get(4)?;
    let start_column: usize = row.get(5)?;
    let end_line: usize = row.get(6)?;
    let end_column: usize = row.get(7)?;

    let symbol_type = match type_str.as_str() {
        "definition" => SymbolType::Definition,
        _ => SymbolType::Reference,
    };

    Ok(SymbolLocation {
        path: Some(file_path),
        name,
        start_line,
        start_column,
        end_line,
        end_column,
        symbol_type,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db_open_and_schema() {
        let _db = SymbolIndexDatabase::open_in_memory().unwrap();
    }

    #[test]
    fn test_update_file_symbols() {
        let mut db = SymbolIndexDatabase::open_in_memory().unwrap();

        let symbols = vec![SymbolLocation {
            path: None,
            name: "my_func".to_string(),
            start_line: 5,
            start_column: 0,
            end_line: 5,
            end_column: 7,
            symbol_type: SymbolType::Definition,
            kind: Some("function".to_string()),
        }];

        let _ = db.update_file_symbols("src/main.rs", 123456, 1024, &symbols);
        assert!(db.is_dirty());
    }

    #[test]
    fn test_get_symbols_by_name() {
        let mut db = SymbolIndexDatabase::open_in_memory().unwrap();

        let symbols = vec![
            SymbolLocation {
                path: None,
                name: "foo".to_string(),
                start_line: 1,
                start_column: 0,
                end_line: 1,
                end_column: 3,
                symbol_type: SymbolType::Definition,
                kind: Some("function".to_string()),
            },
            SymbolLocation {
                path: None,
                name: "foo".to_string(),
                start_line: 10,
                start_column: 4,
                end_line: 10,
                end_column: 7,
                symbol_type: SymbolType::Reference,
                kind: None,
            },
        ];

        let _ = db.update_file_symbols("src/lib.rs", 999, 500, &symbols);

        let all = db.get_symbols_by_name("foo", None, None);
        assert_eq!(all.len(), 2);

        let defs = db.get_symbols_by_name("foo", Some(SymbolType::Definition), None);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].start_line, 1);

        let refs = db.get_symbols_by_name("foo", Some(SymbolType::Reference), None);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].start_line, 10);

        let limited = db.get_symbols_by_name("foo", None, Some(1));
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn test_batch_update() {
        let mut db = SymbolIndexDatabase::open_in_memory().unwrap();

        let entries = vec![
            (
                "src/a.rs".to_string(),
                100u64,
                50u64,
                vec![SymbolLocation {
                    path: None,
                    name: "alpha".to_string(),
                    start_line: 1,
                    start_column: 0,
                    end_line: 1,
                    end_column: 5,
                    symbol_type: SymbolType::Definition,
                    kind: None,
                }],
            ),
            (
                "src/b.rs".to_string(),
                200u64,
                60u64,
                vec![SymbolLocation {
                    path: None,
                    name: "beta".to_string(),
                    start_line: 2,
                    start_column: 0,
                    end_line: 2,
                    end_column: 4,
                    symbol_type: SymbolType::Reference,
                    kind: None,
                }],
            ),
        ];

        let _ = db.update_files_symbols_batch(&entries);

        let alpha = db.get_symbols_by_name("alpha", None, None);
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].path.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn test_upsert_replaces_existing() {
        let mut db = SymbolIndexDatabase::open_in_memory().unwrap();

        let symbols_v1 = vec![SymbolLocation {
            path: None,
            name: "old_name".to_string(),
            start_line: 1,
            start_column: 0,
            end_line: 1,
            end_column: 8,
            symbol_type: SymbolType::Definition,
            kind: None,
        }];

        let _ = db.update_file_symbols("src/lib.rs", 100, 50, &symbols_v1);

        let symbols_v2 = vec![SymbolLocation {
            path: None,
            name: "new_name".to_string(),
            start_line: 1,
            start_column: 0,
            end_line: 1,
            end_column: 8,
            symbol_type: SymbolType::Definition,
            kind: None,
        }];

        let _ = db.update_file_symbols("src/lib.rs", 200, 55, &symbols_v2);

        let results = db.get_symbols_by_name("old_name", None, None);
        assert!(results.is_empty());

        let results = db.get_symbols_by_name("new_name", None, None);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_exact_name_match_no_partial() {
        let mut db = SymbolIndexDatabase::open_in_memory().unwrap();

        let symbols = vec![
            SymbolLocation {
                path: None,
                name: "foo".to_string(),
                start_line: 1,
                start_column: 0,
                end_line: 1,
                end_column: 3,
                symbol_type: SymbolType::Definition,
                kind: None,
            },
            SymbolLocation {
                path: None,
                name: "foobar".to_string(),
                start_line: 2,
                start_column: 0,
                end_line: 2,
                end_column: 6,
                symbol_type: SymbolType::Definition,
                kind: None,
            },
        ];

        let _ = db.update_file_symbols("src/lib.rs", 100, 50, &symbols);

        let foo = db.get_symbols_by_name("foo", None, None);
        assert_eq!(foo.len(), 1);

        let foobar = db.get_symbols_by_name("foobar", None, None);
        assert_eq!(foobar.len(), 1);
    }

    #[test]
    fn test_persist_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        {
            let mut db = SymbolIndexDatabase::open(&db_path).unwrap();
            let symbols = vec![SymbolLocation {
                path: None,
                name: "persisted".to_string(),
                start_line: 1,
                start_column: 0,
                end_line: 1,
                end_column: 9,
                symbol_type: SymbolType::Definition,
                kind: None,
            }];
            let _ = db.update_file_symbols("src/lib.rs", 100, 50, &symbols);
        }

        let db = SymbolIndexDatabase::open(&db_path).unwrap();
        let results = db.get_symbols_by_name("persisted", None, None);
        assert_eq!(results.len(), 1);
    }
}
