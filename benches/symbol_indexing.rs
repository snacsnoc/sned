use criterion::{Criterion, criterion_group, criterion_main};
use sned::services::symbol_index::{SymbolIndexService, SymbolLocation, SymbolType};
use std::hint::black_box;

const RUST_SAMPLE: &str = r#"
use std::collections::HashMap;

pub struct Config {
    pub name: String,
    pub values: HashMap<String, i32>,
}

impl Config {
    pub fn new(name: String) -> Self {
        Self {
            name,
            values: HashMap::new(),
        }
    }

    pub fn get_value(&self, key: &str) -> Option<i32> {
        self.values.get(key).copied()
    }

    pub fn set_value(&mut self, key: String, value: i32) {
        self.values.insert(key, value);
    }
}

fn helper_function(x: i32) -> i32 {
    x * 2
}

fn another_helper(data: &[usize]) -> usize {
    data.iter().sum()
}

pub trait Processable {
    fn process(&self) -> String;
}

impl Processable for Config {
    fn process(&self) -> String {
        format!("Config: {}", self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config() {
        let config = Config::new("test".to_string());
        assert_eq!(config.name, "test");
    }
}
"#;

const TS_SAMPLE: &str = r#"
interface UserConfig {
    name: string;
    age: number;
    settings: Record<string, boolean>;
}

class UserManager {
    private users: Map<string, UserConfig> = new Map();

    public addUser(id: string, config: UserConfig): void {
        this.users.set(id, config);
    }

    public getUser(id: string): UserConfig | undefined {
        return this.users.get(id);
    }

    public removeUser(id: string): boolean {
        return this.users.delete(id);
    }

    private validateConfig(config: UserConfig): boolean {
        return config.age > 0 && config.name.length > 0;
    }
}

function processUsers(users: UserConfig[]): string[] {
    return users.map(u => u.name);
}

export { UserManager, processUsers };
"#;

fn bench_extract_symbols_rust(c: &mut Criterion) {
    let mut service = SymbolIndexService::new("/tmp".to_string());

    c.bench_function("symbol_index_rust", |b| {
        b.iter(|| {
            service.index_file(
                "test.rs".to_string(),
                0,
                RUST_SAMPLE.len() as u64,
                vec![], // symbols will be extracted internally
            );
            black_box(service.get_project_root());
        })
    });
}

fn bench_extract_symbols_typescript(c: &mut Criterion) {
    let mut service = SymbolIndexService::new("/tmp".to_string());

    c.bench_function("symbol_index_typescript", |b| {
        b.iter(|| {
            service.index_file("test.ts".to_string(), 0, TS_SAMPLE.len() as u64, vec![]);
            black_box(service.get_project_root());
        })
    });
}

fn bench_symbol_lookup(c: &mut Criterion) {
    let mut service = SymbolIndexService::new("/tmp".to_string());

    // Pre-populate with some symbols
    let symbols = vec![
        SymbolLocation {
            path: None,
            name: "Config".to_string(),
            start_line: 3,
            start_column: 0,
            end_line: 3,
            end_column: 6,
            symbol_type: SymbolType::Definition,
            kind: Some("struct".to_string()),
        },
        SymbolLocation {
            path: None,
            name: "new".to_string(),
            start_line: 8,
            start_column: 0,
            end_line: 8,
            end_column: 3,
            symbol_type: SymbolType::Definition,
            kind: Some("method".to_string()),
        },
        SymbolLocation {
            path: None,
            name: "get_value".to_string(),
            start_line: 15,
            start_column: 0,
            end_line: 15,
            end_column: 9,
            symbol_type: SymbolType::Definition,
            kind: Some("method".to_string()),
        },
        SymbolLocation {
            path: None,
            name: "helper_function".to_string(),
            start_line: 27,
            start_column: 0,
            end_line: 27,
            end_column: 15,
            symbol_type: SymbolType::Definition,
            kind: Some("function".to_string()),
        },
    ];

    service.index_file("test.rs".to_string(), 0, 1000, symbols);

    c.bench_function("symbol_lookup_by_name", |b| {
        b.iter(|| {
            let result =
                service.get_symbols(black_box("Config"), Some(SymbolType::Definition), None);
            black_box(result);
        })
    });

    c.bench_function("symbol_lookup_all", |b| {
        b.iter(|| {
            let result = service.get_symbols(black_box("get_value"), None, None);
            black_box(result);
        })
    });
}

criterion_group!(
    benches,
    bench_extract_symbols_rust,
    bench_extract_symbols_typescript,
    bench_symbol_lookup
);
criterion_main!(benches);
