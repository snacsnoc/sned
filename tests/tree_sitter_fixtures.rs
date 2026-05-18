use sned::services::tree_sitter::{ParseOptions, load_required_language_parsers, parse_file};
use std::fs;
use std::path::PathBuf;

fn get_fixtures_dir() -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.push("dirac/src/services/tree-sitter/__tests__/fixtures");
    path.exists().then_some(path)
}

fn fixtures_dir_for(lang: &str) -> Option<PathBuf> {
    let Some(fixtures_dir) = get_fixtures_dir() else {
        eprintln!(
            "Skipping tree-sitter fixtures: historical TypeScript fixture directory is absent"
        );
        return None;
    };
    Some(fixtures_dir.join(lang))
}

#[test]
fn test_rust_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("rust") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.rs");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.rs");

    let parsers = load_required_language_parsers(&["sample.rs"]).expect("Failed to load parsers");

    let options = ParseOptions {
        show_call_graph: true,
    };

    let result = parse_file("sample.rs", &sample_content, &parsers, None, Some(&options))
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("pub struct RustStruct"))
    );
    assert!(definitions.iter().any(|d| d.text.contains("fn get_value")));
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("pub fn rust_main"))
    );
}

#[test]
fn test_python_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("python") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.py");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.py");

    let parsers = load_required_language_parsers(&["sample.py"]).expect("Failed to load parsers");

    let options = ParseOptions {
        show_call_graph: true,
    };

    let result = parse_file("sample.py", &sample_content, &parsers, None, Some(&options))
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("class PythonClass"))
    );
    assert!(definitions.iter().any(|d| d.text.contains("def calculate")));
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("def top_level_func"))
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn test_typescript_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("typescript") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.ts");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.ts");

    let parsers = load_required_language_parsers(&["sample.ts"]).expect("Failed to load parsers");

    let options = ParseOptions {
        show_call_graph: true,
    };

    let result = parse_file("sample.ts", &sample_content, &parsers, None, Some(&options))
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(definitions.iter().any(|d| d.text.contains("class MyClass")));
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("interface MyInterface"))
    );
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("function topLevelFunction"))
    );
}

#[cfg(feature = "lang-go")]
#[test]
fn test_go_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("go") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.go");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.go");

    let parsers = load_required_language_parsers(&["sample.go"]).expect("Failed to load parsers");

    let result = parse_file("sample.go", &sample_content, &parsers, None, None)
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(definitions.iter().any(|d| d.text.contains("package main")));
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("type GoStruct struct"))
    );
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("func (s *GoStruct) GetName()"))
    );
}

#[cfg(feature = "lang-java")]
#[test]
fn test_java_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("java") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.java");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.java");

    let parsers = load_required_language_parsers(&["sample.java"]).expect("Failed to load parsers");

    let result = parse_file("sample.java", &sample_content, &parsers, None, None)
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("public class JavaClass"))
    );
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("public String getName()"))
    );
}

#[cfg(feature = "lang-cpp")]
#[test]
fn test_cpp_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("cpp") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.cpp");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.cpp");

    let parsers = load_required_language_parsers(&["sample.cpp"]).expect("Failed to load parsers");

    let result = parse_file("sample.cpp", &sample_content, &parsers, None, None)
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("class CppClass"))
    );
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("int CppClass::calculate"))
    );
}

#[cfg(feature = "lang-c")]
#[test]
fn test_c_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("c") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.h");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.h");

    let parsers = load_required_language_parsers(&["sample.h"]).expect("Failed to load parsers");

    let result = parse_file("sample.h", &sample_content, &parsers, None, None)
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(definitions.iter().any(|d| d.text.contains("id")));
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("void say_hello"))
    );
}

#[cfg(feature = "lang-c-sharp")]
#[test]
fn test_csharp_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("csharp") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.cs");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.cs");

    let parsers = load_required_language_parsers(&["sample.cs"]).expect("Failed to load parsers");

    let result = parse_file("sample.cs", &sample_content, &parsers, None, None)
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("class CSharpClass"))
    );
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("public string GetName()"))
    );
}

#[cfg(feature = "lang-ruby")]
#[test]
fn test_ruby_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("ruby") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.rb");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.rb");

    let parsers = load_required_language_parsers(&["sample.rb"]).expect("Failed to load parsers");

    let result = parse_file("sample.rb", &sample_content, &parsers, None, None)
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("class RubyClass"))
    );
    assert!(definitions.iter().any(|d| d.text.contains("def get_name")));
}

#[cfg(feature = "lang-php")]
#[test]
fn test_php_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("php") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.php");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.php");

    let parsers = load_required_language_parsers(&["sample.php"]).expect("Failed to load parsers");

    let result = parse_file("sample.php", &sample_content, &parsers, None, None)
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("class PhpClass"))
    );
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("public function getName()"))
    );
}

#[cfg(feature = "lang-swift")]
#[test]
fn test_swift_fixtures() {
    let Some(fixtures_dir) = fixtures_dir_for("swift") else {
        return;
    };
    let sample_path = fixtures_dir.join("sample.swift");
    let sample_content = fs::read_to_string(&sample_path).expect("Failed to read sample.swift");

    let parsers =
        load_required_language_parsers(&["sample.swift"]).expect("Failed to load parsers");

    let result = parse_file("sample.swift", &sample_content, &parsers, None, None)
        .expect("Failed to parse file");

    assert!(result.is_some());
    let definitions = result.unwrap();

    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("class SwiftClass"))
    );
    assert!(
        definitions
            .iter()
            .any(|d| d.text.contains("func getName()"))
    );
}
