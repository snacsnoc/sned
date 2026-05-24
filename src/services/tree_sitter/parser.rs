use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};
use tree_sitter::{Language, Query};

use crate::services::tree_sitter::queries::*;

/// Error type for language parser operations.
#[derive(Debug, thiserror::Error)]
pub enum LanguageParserError {
    #[error("Unsupported language extension: {0}")]
    UnsupportedExtension(String),
    #[error("Failed to create parser: {0}")]
    ParserCreation(String),
    #[error("Failed to create query: {0}")]
    QueryCreation(String),
}

/// A loaded language and query for a specific language.
/// Language and Query are Arc'd to enable cheap cloning.
pub struct LanguageParserEntry {
    pub language: Arc<Language>,
    pub query: Arc<Query>,
}

/// Global cache for language parsers keyed by extension.
/// Avoids recompiling tree-sitter queries on every handler invocation.
/// Entries are Arc'd since Query is not Clone but can be shared.
/// Uses LRU eviction (max 256 entries) to bound memory growth.
static PARSER_CACHE: LazyLock<Mutex<lru::LruCache<String, Arc<LanguageParserEntry>>>> =
    LazyLock::new(|| Mutex::new(lru::LruCache::new(
        std::num::NonZero::new(256).unwrap()
    )));

/// Maps file extensions to loaded language parsers.
pub type LanguageParserMap = HashMap<String, LanguageParserEntry>;

/// Loads tree-sitter language parsers for the given file paths.
///
/// This is the Rust equivalent of `loadRequiredLanguageParsers` from
/// `dirac/src/services/tree-sitter/languageParser.ts`.
pub fn load_required_language_parsers(
    file_paths: &[impl AsRef<str>],
) -> Result<LanguageParserMap, LanguageParserError> {
    let mut parsers: LanguageParserMap = HashMap::with_capacity(8);

    for file_path in file_paths {
        let ext = get_extension(file_path.as_ref());
        if parsers.contains_key(&ext) {
            continue;
        }

        let entry = load_language_parser(&ext)?;
        parsers.insert(ext, entry);
    }

    Ok(parsers)
}

/// Loads a single language parser for the given extension.
#[cfg(not(any(
    feature = "lang-rust",
    feature = "lang-javascript",
    feature = "lang-python",
    feature = "lang-typescript",
    feature = "lang-go",
    feature = "lang-c",
    feature = "lang-cpp",
    feature = "lang-c-sharp",
    feature = "lang-ruby",
    feature = "lang-java",
    feature = "lang-php",
    feature = "lang-swift"
)))]
fn load_language_parser(_ext: &str) -> Result<LanguageParserEntry, LanguageParserError> {
    Err(LanguageParserError::UnsupportedExtension(
        "no language parsers enabled".to_string(),
    ))
}

#[cfg(any(
    feature = "lang-rust",
    feature = "lang-javascript",
    feature = "lang-python",
    feature = "lang-typescript",
    feature = "lang-go",
    feature = "lang-c",
    feature = "lang-cpp",
    feature = "lang-c-sharp",
    feature = "lang-ruby",
    feature = "lang-java",
    feature = "lang-php",
    feature = "lang-swift"
))]
fn load_language_parser(ext: &str) -> Result<LanguageParserEntry, LanguageParserError> {
    // Check cache first
    {
        let mut cache = PARSER_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(ext) {
            // Return a copy of the cached entry - Arc makes this cheap
            return Ok(LanguageParserEntry {
                language: Arc::clone(&entry.language),
                query: Arc::clone(&entry.query),
            });
        }
    }

    // Not in cache - create new parser
    let (language, query_text) = match ext {
        #[cfg(feature = "lang-javascript")]
        "js" | "jsx" => {
            let lang_fn = tree_sitter_javascript::LANGUAGE;
            (Language::new(lang_fn), JAVASCRIPT_QUERY)
        }
        #[cfg(feature = "lang-typescript")]
        "ts" => {
            let lang_fn = tree_sitter_typescript::LANGUAGE_TYPESCRIPT;
            (Language::new(lang_fn), TYPESCRIPT_QUERY)
        }
        #[cfg(feature = "lang-typescript")]
        "tsx" => {
            let lang_fn = tree_sitter_typescript::LANGUAGE_TSX;
            (Language::new(lang_fn), TYPESCRIPT_QUERY)
        }
        #[cfg(feature = "lang-python")]
        "py" => {
            let lang_fn = tree_sitter_python::LANGUAGE;
            (Language::new(lang_fn), PYTHON_QUERY)
        }
        #[cfg(feature = "lang-rust")]
        "rs" => {
            let lang_fn = tree_sitter_rust::LANGUAGE;
            (Language::new(lang_fn), RUST_QUERY)
        }
        #[cfg(feature = "lang-go")]
        "go" => {
            let lang_fn = tree_sitter_go::LANGUAGE;
            (Language::new(lang_fn), GO_QUERY)
        }
        #[cfg(feature = "lang-c")]
        "c" | "h" => {
            let lang_fn = tree_sitter_c::LANGUAGE;
            (Language::new(lang_fn), C_QUERY)
        }
        #[cfg(feature = "lang-cpp")]
        "cpp" | "hpp" => {
            let lang_fn = tree_sitter_cpp::LANGUAGE;
            (Language::new(lang_fn), CPP_QUERY)
        }
        #[cfg(feature = "lang-c-sharp")]
        "cs" => {
            let lang_fn = tree_sitter_c_sharp::LANGUAGE;
            (Language::new(lang_fn), CSHARP_QUERY)
        }
        #[cfg(feature = "lang-ruby")]
        "rb" => {
            let lang_fn = tree_sitter_ruby::LANGUAGE;
            (Language::new(lang_fn), RUBY_QUERY)
        }
        #[cfg(feature = "lang-java")]
        "java" => {
            let lang_fn = tree_sitter_java::LANGUAGE;
            (Language::new(lang_fn), JAVA_QUERY)
        }
        #[cfg(feature = "lang-php")]
        "php" => {
            let lang_fn = tree_sitter_php::LANGUAGE_PHP;
            (Language::new(lang_fn), PHP_QUERY)
        }
        #[cfg(feature = "lang-swift")]
        "swift" => {
            let lang_fn = tree_sitter_swift::LANGUAGE;
            (Language::new(lang_fn), SWIFT_QUERY)
        }
        _ => return Err(LanguageParserError::UnsupportedExtension(ext.to_string())),
    };

    let query = Query::new(&language, query_text)
        .map_err(|e| LanguageParserError::QueryCreation(e.to_string()))?;

    let entry = LanguageParserEntry {
        language: Arc::new(language),
        query: Arc::new(query),
    };

    // Cache a copy of the newly created parser
    {
        let mut cache = PARSER_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.put(
            ext.to_string(),
            Arc::new(LanguageParserEntry {
                language: Arc::clone(&entry.language),
                query: Arc::clone(&entry.query),
            }),
        );
    }

    Ok(entry)
}

/// Creates a LanguageParserEntry for testing purposes.
#[cfg(test)]
pub fn create_test_parser_entry(ext: &str) -> Result<LanguageParserEntry, LanguageParserError> {
    load_language_parser(ext)
}

/// Extracts the lowercase file extension without the leading dot.
fn get_extension(file_path: &str) -> String {
    Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_extension() {
        assert_eq!(get_extension("file.rs"), "rs");
        assert_eq!(get_extension("file.JS"), "js");
        assert_eq!(get_extension("/path/to/file.py"), "py");
        assert_eq!(get_extension("no_extension"), "");
    }

    #[test]
    fn test_load_rust_parser() {
        let result = load_language_parser("rs");
        assert!(
            result.is_ok(),
            "Failed to load Rust parser: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_load_javascript_parser() {
        let result = load_language_parser("js");
        assert!(
            result.is_ok(),
            "Failed to load JS parser: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_load_python_parser() {
        let result = load_language_parser("py");
        assert!(
            result.is_ok(),
            "Failed to load Python parser: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_unsupported_extension() {
        let result = load_language_parser("xyz");
        assert!(matches!(
            result,
            Err(LanguageParserError::UnsupportedExtension(_))
        ));
    }

    #[test]
    fn test_kotlin_deferred() {
        // Kotlin is explicitly deferred due to tree-sitter version conflict
        let result = load_language_parser("kt");
        assert!(matches!(
            result,
            Err(LanguageParserError::UnsupportedExtension(_))
        ));
    }
}
