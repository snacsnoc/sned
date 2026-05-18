use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use super::{LanguageParserEntry, LanguageParserError};

/// Options for resolving symbol context.
pub struct SymbolContextOptions<'a> {
    pub node: Node<'a>,
    pub file_content: &'a str,
    pub ext: &'a str,
    pub anchors: &'a [String],
    pub max_context_lines: Option<usize>,
}

/// Result of resolving symbol context.
pub struct SymbolContextResult {
    pub context: String,
}

/// Resolves relevant context (imports and class properties) for a given symbol node.
///
pub fn resolve_symbol_context(
    options: SymbolContextOptions,
    language_entry: &LanguageParserEntry,
) -> Result<SymbolContextResult, LanguageParserError> {
    let query_strings = match get_query_strings(options.ext) {
        Some(qs) => qs,
        None => {
            return Ok(SymbolContextResult {
                context: String::new(),
            });
        }
    };

    let query = Query::new(&language_entry.language, query_strings.context_query)
        .map_err(|e| LanguageParserError::QueryCreation(e.to_string()))?;

    let mut query_cursor = QueryCursor::new();

    // Collect captures into a Vec for easier processing
    let mut captures: Vec<(String, usize, usize, String)> = Vec::new();
    {
        let mut cap_iter =
            query_cursor.captures(&query, options.node, options.file_content.as_bytes());
        while let Some((match_, capture_index)) = cap_iter.next() {
            if let Some(capture) = match_.captures.get(*capture_index) {
                let capture_name = query.capture_names()[capture.index as usize];
                let start_byte = capture.node.start_byte();
                let end_byte = capture.node.end_byte();
                let capture_text = options.file_content[start_byte..end_byte].to_string();
                captures.push((
                    capture_name.to_string(),
                    capture.node.start_position().row,
                    capture.node.end_position().row,
                    capture_text,
                ));
            }
        }
    }

    // Build context from captures
    let context = build_context_from_captures(&captures, options.max_context_lines.unwrap_or(30));

    Ok(SymbolContextResult { context })
}

/// Builds a context string from tree-sitter captures.
fn build_context_from_captures(
    captures: &[(String, usize, usize, String)],
    max_context_lines: usize,
) -> String {
    let mut context_parts: Vec<String> = Vec::new();
    let mut lines_count = 0;

    // Group captures by type
    let imports: Vec<&String> = captures
        .iter()
        .filter(|(name, _, _, _)| *name == "import")
        .map(|(_, _, _, text)| text)
        .collect();

    let classes: Vec<&String> = captures
        .iter()
        .filter(|(name, _, _, _)| *name == "class")
        .map(|(_, _, _, text)| text)
        .collect();

    let properties: Vec<&String> = captures
        .iter()
        .filter(|(name, _, _, _)| *name == "property")
        .map(|(_, _, _, text)| text)
        .collect();

    let methods: Vec<&String> = captures
        .iter()
        .filter(|(name, _, _, _)| *name == "method")
        .map(|(_, _, _, text)| text)
        .collect();

    // Add imports section
    if !imports.is_empty() {
        context_parts.push("### Imports".to_string());
        for import in imports {
            let line_count = import.lines().count();
            if lines_count + line_count <= max_context_lines {
                context_parts.push(import.clone());
                lines_count += line_count;
            }
        }
    }

    // Add class definition
    if !classes.is_empty() {
        if !context_parts.is_empty() {
            context_parts.push(String::new());
        }
        context_parts.push("### Class".to_string());
        for class in classes {
            let line_count = class.lines().count();
            if lines_count + line_count <= max_context_lines {
                context_parts.push(class.clone());
                lines_count += line_count;
            }
        }
    }

    // Add properties section
    if !properties.is_empty() {
        if !context_parts.is_empty() {
            context_parts.push(String::new());
        }
        context_parts.push("### Properties".to_string());
        for prop in properties {
            let line_count = prop.lines().count();
            if lines_count + line_count <= max_context_lines {
                context_parts.push(prop.clone());
                lines_count += line_count;
            }
        }
    }

    // Add methods section
    if !methods.is_empty() {
        if !context_parts.is_empty() {
            context_parts.push(String::new());
        }
        context_parts.push("### Methods".to_string());
        for method in methods {
            let line_count = method.lines().count();
            if lines_count + line_count <= max_context_lines {
                context_parts.push(method.clone());
                lines_count += line_count;
            }
        }
    }

    context_parts.join("\n")
}

struct QueryStrings {
    context_query: &'static str,
}

fn get_query_strings(ext: &str) -> Option<QueryStrings> {
    match ext {
        "ts" | "tsx" | "js" | "jsx" => Some(QueryStrings {
            context_query: r#"
                (import_declaration) @import
                (class_declaration) @class
                (class_heritage) @class.heritage
                (public_field_definition) @property
                (private_property_definition) @property
                (method_definition) @method
                (identifier) @ref
                (property_identifier) @ref
            "#,
        }),
        "py" => Some(QueryStrings {
            context_query: r#"
                (import_from_statement) @import
                (import_statement) @import
                (class_definition) @class
                (function_definition) @method
                (assignment left: (attribute object: (identifier) @self attribute: (identifier) @property)) @property
                (identifier) @ref
            "#,
        }),
        "java" => Some(QueryStrings {
            context_query: r#"
                (import_declaration) @import
                (class_declaration) @class
                (field_declaration) @property
                (method_declaration) @method
                (identifier) @ref
            "#,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_symbol_context_python() {
        let code = r#"
from typing import List
import os

class MyClass:
    def __init__(self):
        self.value = 42
    
    def my_method(self):
        return self.value
    
    def another_method(self):
        pass
"#;

        let result = resolve_symbol_context_for_test(code, "py");

        // Should have non-empty context with imports and class
        assert!(
            !result.is_empty(),
            "Context should not be empty for Python code"
        );
        assert!(result.contains("import"), "Should contain imports");
    }

    #[test]
    fn test_resolve_symbol_context_returns_empty_for_unsupported_ext() {
        let result = resolve_symbol_context_for_test("some code", "xyz");
        assert!(
            result.is_empty(),
            "Unsupported ext should return empty context"
        );
    }

    /// Test helper to resolve symbol context without needing a full Node.
    fn resolve_symbol_context_for_test(code: &str, ext: &str) -> String {
        use crate::services::tree_sitter::parser::create_test_parser_entry;

        let entry = match create_test_parser_entry(ext) {
            Ok(e) => e,
            Err(_) => return String::new(),
        };

        let mut parser = tree_sitter::Parser::new();
        let _ = parser.set_language(&entry.language);
        let tree = match parser.parse(code, None) {
            Some(t) => t,
            None => return String::new(),
        };

        let options = SymbolContextOptions {
            node: tree.root_node(),
            file_content: code,
            ext,
            anchors: &[],
            max_context_lines: Some(30),
        };

        match resolve_symbol_context(options, &entry) {
            Ok(result) => result.context,
            Err(_) => String::new(),
        }
    }
}
