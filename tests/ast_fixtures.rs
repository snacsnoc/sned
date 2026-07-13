use serde::Deserialize;
use sned::cli::output::StderrOutputWriter;
use sned::core::file_editor::AnchorStateManager;
use sned::core::tools::{ToolContext, ToolHandler};
use sned::services::tree_sitter::get_functions;
use sned::services::tree_sitter::parser::load_required_language_parsers;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as TokioMutex;

struct WorkingDirectoryGuard(PathBuf);

impl Drop for WorkingDirectoryGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

fn strip_reference_anchors(content: &str) -> String {
    content
        .lines()
        .map(|line| {
            line.split_once(") ").map_or_else(
                || line.to_string(),
                |(prefix, anchored)| {
                    format!(
                        "{prefix}) {}",
                        sned::core::hash_utils::strip_hashes(anchored)
                    )
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

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
    let fixture_dir = fs::canonicalize(&fixture_dir)
        .unwrap_or_else(|error| panic!("Missing {lang} fixture directory: {error}"));
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
    let _cwd_guard = WorkingDirectoryGuard(original_cwd);

    let result = (|| {
        let tests_json_content = fs::read_to_string(&tests_json_path)
            .unwrap_or_else(|error| panic!("Missing {}: {error}", tests_json_path.display()));
        let tests: FixtureTests = serde_json::from_str(&tests_json_content)
            .unwrap_or_else(|error| panic!("Invalid {}: {error}", tests_json_path.display()));

        let sample_content = fs::read_to_string(&sample_file_path).unwrap();
        let language_parsers = load_required_language_parsers(&[sample_rel_path.as_str()])
            .unwrap_or_else(|error| panic!("Failed to load {lang} parser: {error}"));

        if let Some(get_function_tests) = tests.get_function {
            for test in get_function_tests {
                println!("Running get_function test: {} for {}", test.name, lang);
                let expected_path = fixture_dir.join(format!("get_function_{}.txt", test.name));
                let expected_output = fs::read_to_string(&expected_path).unwrap_or_else(|error| {
                    panic!(
                        "Missing expected fixture {}: {error}",
                        expected_path.display()
                    )
                });

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
                    assert_eq!(
                        actual_output.trim(),
                        expected_output.trim(),
                        "get_function parity failed for {}",
                        test.name
                    );
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
                let expected_output = fs::read_to_string(&expected_path).unwrap_or_else(|error| {
                    panic!(
                        "Missing expected fixture {}: {error}",
                        expected_path.display()
                    )
                });
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
                        "names": test.symbols,
                        "find_type": test.find_type,
                    }),
                ));
                let value = result.unwrap_or_else(|error| {
                    panic!("find_symbol_references failed for {}: {error}", test.name)
                });
                let actual_output =
                    strip_reference_anchors(value.as_str().expect("tool result must be a string"));
                assert_eq!(
                    actual_output.trim(),
                    expected_output.trim(),
                    "find_symbol_references parity failed for {}",
                    test.name
                );
            }
        }

        if let Some(replace_tests) = tests.replace_symbol {
            let original_content = fs::read_to_string(&sample_file_path).unwrap();
            for test in replace_tests {
                println!("Running replace_symbol test: {} for {}", test.name, lang);
                let expected_path = fixture_dir.join(format!("replace_symbol_{}.txt", test.name));
                let expected_output = fs::read_to_string(&expected_path).unwrap_or_else(|error| {
                    panic!(
                        "Missing expected fixture {}: {error}",
                        expected_path.display()
                    )
                });
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
                let value = result.unwrap_or_else(|error| {
                    panic!("replace_symbol failed for {}: {error}", test.name)
                });
                let actual_output = sned::core::hash_utils::strip_hashes(
                    value.as_str().expect("tool result must be a string"),
                );
                fs::write(&sample_file_path, &original_content).unwrap();
                assert_eq!(
                    actual_output.trim(),
                    expected_output.trim(),
                    "replace_symbol parity failed for {}",
                    test.name
                );
            }
        }

        Ok::<(), ()>(())
    })();

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
