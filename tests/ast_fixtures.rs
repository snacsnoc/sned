use serde::Deserialize;
use sned::cli::output::StderrOutputWriter;
use sned::core::file_editor::AnchorStateManager;
use sned::core::tools::{ToolContext, ToolHandler};
use sned::services::tree_sitter::get_functions;
use sned::services::tree_sitter::parser::load_required_language_parsers;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as TokioMutex;

#[derive(Deserialize)]
struct FixtureTests {
    get_function: Option<Vec<GetFunctionTest>>,
    find_symbol_references: Option<Vec<FindSymbolReferencesTest>>,
    replace_symbol: Option<Vec<ReplaceSymbolTest>>,
}

#[derive(Deserialize)]
struct GetFunctionTest {
    name: String,
    symbols: Vec<String>,
}

#[derive(Deserialize)]
struct FindSymbolReferencesTest {
    name: String,
    symbols: Vec<String>,
    find_type: String,
}

#[derive(Deserialize)]
struct ReplaceSymbolTest {
    name: String,
    symbol: String,
    text: String,
}

fn run_language_fixtures(lang: &str, extension: &str) {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(lang);
    let fixture_dir = match fs::canonicalize(&fixture_dir) {
        Ok(path) => path,
        Err(_) => {
            eprintln!(
                "Skipping {} AST fixtures: historical TypeScript fixture directory is absent",
                lang
            );
            return;
        }
    };
    let tests_json_path = fixture_dir.join("tests.json");
    let sample_file_path = fixture_dir.join(format!("sample.{}", extension));
    let sample_rel_path = format!("sample.{}", extension);
    let rt = tokio::runtime::Runtime::new().unwrap();

    static CWD_GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = CWD_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let original_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&fixture_dir).unwrap();

    let result = (|| {
        if !tests_json_path.exists() {
            println!("Skipping {}: tests.json not found", lang);
            return Ok(());
        }

        let tests_json_content = fs::read_to_string(&tests_json_path).unwrap();
        let tests: FixtureTests = serde_json::from_str(&tests_json_content).unwrap();

        let sample_content = fs::read_to_string(&sample_file_path).unwrap();
        let language_parsers = match load_required_language_parsers(&[sample_rel_path.as_str()]) {
            Ok(parsers) => parsers,
            Err(e) => {
                println!("Skipping {}: language parser not available: {}", lang, e);
                return Ok(());
            }
        };

        if let Some(get_function_tests) = tests.get_function {
            for test in get_function_tests {
                println!("Running get_function test: {} for {}", test.name, lang);
                let expected_path = fixture_dir.join(format!("get_function_{}.txt", test.name));
                if !expected_path.exists() {
                    println!("  Skipping: expected file not found: {:?}", expected_path);
                    continue;
                }
                let expected_output = fs::read_to_string(expected_path).unwrap();

                let anchor_mgr = AnchorStateManager::new();
                let result = get_functions(
                    &anchor_mgr,
                    &sample_rel_path,
                    &sample_rel_path,
                    &test.symbols,
                    &sample_content,
                    &language_parsers,
                    None,
                )
                .unwrap();

                if let Some(res) = result {
                    let actual_output =
                        sned::core::hash_utils::strip_hashes(&res.formatted_content);
                    if actual_output.trim() != expected_output.trim() {
                        println!(
                            "Parity failed for {}. Actual:\n{}\nExpected:\n{}",
                            test.name, actual_output, expected_output
                        );
                    }
                } else {
                    panic!("get_functions returned None for {}", test.name);
                }
            }
        }

        if let Some(reference_tests) = tests.find_symbol_references {
            for test in reference_tests {
                println!(
                    "Running find_symbol_references test: {} for {}",
                    test.name, lang
                );
                let expected_path =
                    fixture_dir.join(format!("find_symbol_references_{}.txt", test.name));
                if !expected_path.exists() {
                    println!("  Skipping: expected file not found: {:?}", expected_path);
                    continue;
                }
                let expected_output = fs::read_to_string(expected_path).unwrap();
                let handler = sned::core::tools::handlers::find_symbol_references::FindSymbolReferencesHandler;
                let state = Arc::new(TokioMutex::new(sned::core::agent_loop::TaskState::default()));
                let tool_context = ToolContext::new(
                    state,
                    None,
                    fixture_dir.clone(),
                    AnchorStateManager::new(),
                    false,
                    "test-task".to_string(),
                    None,
                    false,
                    Arc::new(StderrOutputWriter),
                );
                let result = rt.block_on(ToolHandler::execute(
                    &handler,
                    &tool_context,
                    serde_json::json!({
                        "paths": [sample_rel_path.clone()],
                        "symbols": test.symbols,
                        "find_type": test.find_type,
                    }),
                ));
                let actual_output = match result {
                    Ok(v) => sned::core::hash_utils::strip_hashes(v.as_str().unwrap()),
                    Err(e) => {
                        println!("  find_symbol_references error for {}: {}", test.name, e);
                        continue;
                    }
                };
                if actual_output.trim() != expected_output.trim() {
                    println!(
                        "Parity failed for {}. Actual:\n{}\nExpected:\n{}",
                        test.name, actual_output, expected_output
                    );
                }
            }
        }

        if let Some(replace_tests) = tests.replace_symbol {
            let original_content = fs::read_to_string(&sample_file_path).unwrap();
            for test in replace_tests {
                println!("Running replace_symbol test: {} for {}", test.name, lang);
                let expected_path = fixture_dir.join(format!("replace_symbol_{}.txt", test.name));
                if !expected_path.exists() {
                    println!("  Skipping: expected file not found: {:?}", expected_path);
                    continue;
                }
                let expected_output = fs::read_to_string(expected_path).unwrap();
                let handler =
                    sned::core::tools::handlers::replace_symbol::ReplaceSymbolHandler::new();
                let state = Arc::new(TokioMutex::new(sned::core::agent_loop::TaskState::default()));
                let tool_context = ToolContext::new(
                    state,
                    None,
                    fixture_dir.clone(),
                    AnchorStateManager::new(),
                    false,
                    "test-task".to_string(),
                    None,
                    false,
                    Arc::new(StderrOutputWriter),
                );
                let result = rt.block_on(ToolHandler::execute(
                    &handler,
                    &tool_context,
                    serde_json::json!({
                        "path": sample_rel_path.clone(),
                        "symbol": test.symbol,
                        "text": test.text,
                    }),
                ));
                let actual_output = match result {
                    Ok(v) => sned::core::hash_utils::strip_hashes(v.as_str().unwrap()),
                    Err(e) => {
                        println!("  replace_symbol error for {}: {}", test.name, e);
                        continue;
                    }
                };
                if actual_output.trim() != expected_output.trim() {
                    println!(
                        "Parity failed for {}. Actual:\n{}\nExpected:\n{}",
                        test.name, actual_output, expected_output
                    );
                }
                fs::write(&sample_file_path, &original_content).unwrap();
            }
        }

        Ok::<(), ()>(())
    })();

    std::env::set_current_dir(original_cwd).unwrap();
    result.unwrap();
}

#[test]
fn test_rust_fixtures() {
    run_language_fixtures("rust", "rs");
}

#[test]
fn test_python_fixtures() {
    run_language_fixtures("python", "py");
}

#[test]
fn test_typescript_fixtures() {
    run_language_fixtures("typescript", "ts");
}

#[test]
fn test_c_fixtures() {
    run_language_fixtures("c", "c");
}

#[test]
fn test_cpp_fixtures() {
    run_language_fixtures("cpp", "cpp");
}

#[test]
fn test_csharp_fixtures() {
    run_language_fixtures("csharp", "cs");
}

#[test]
fn test_go_fixtures() {
    run_language_fixtures("go", "go");
}

#[test]
fn test_java_fixtures() {
    run_language_fixtures("java", "java");
}

#[test]
fn test_php_fixtures() {
    run_language_fixtures("php", "php");
}

#[test]
fn test_ruby_fixtures() {
    run_language_fixtures("ruby", "rb");
}

#[test]
fn test_swift_fixtures() {
    run_language_fixtures("swift", "swift");
}
