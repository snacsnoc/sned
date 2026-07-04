pub mod parser;

pub use parser::{
    LanguageParserEntry, LanguageParserError, LanguageParserMap, load_required_language_parsers,
};
pub mod queries;
pub mod symbol_context;

use std::collections::{HashMap, HashSet};

use streaming_iterator::StreamingIterator;

use crate::core::file_editor::AnchorStateManager;
use crate::core::hash_utils::{content_hash, format_line_with_hash};

/// A parsed definition from a source file.
///
/// Equivalent to `ParsedDefinition` in `dirac/src/services/tree-sitter/index.ts`.
#[derive(Debug, Clone)]
pub struct ParsedDefinition {
    pub line_index: usize,
    pub text: String,
    pub indentation: String,
}

/// Parses a file and extracts definitions using tree-sitter queries.
///
pub fn parse_file(
    file_path: &str,
    file_content: &str,
    language_parsers: &LanguageParserMap,
) -> Result<Option<Vec<ParsedDefinition>>, LanguageParserError> {
    let ext = get_extension(file_path);

    let Some(entry) = language_parsers.get(&ext) else {
        return Ok(None);
    };

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&entry.language)
        .map_err(|e| LanguageParserError::ParserCreation(e.to_string()))?;

    let tree = match parser.parse(file_content, None) {
        Some(t) => t,
        None => return Ok(None),
    };

    let mut definitions: Vec<ParsedDefinition> = Vec::new();
    let root_node = tree.root_node();

    // Collect name.definition captures and sort by line
    let mut name_captures: Vec<(tree_sitter::Node, &str)> = Vec::new();
    let mut query_cursor2 = tree_sitter::QueryCursor::new();
    let mut captures2 = query_cursor2.captures(&entry.query, root_node, file_content.as_bytes());

    while let Some((match_, capture_index)) = captures2.next() {
        let capture = match_.captures[*capture_index];
        let capture_name = entry.query.capture_names()[capture.index as usize];

        if capture_name.contains("name.definition") {
            name_captures.push((capture.node, capture_name));
        }
    }

    name_captures.sort_by_key(|(node, _)| node.start_position().row);

    let lines: Vec<&str> = file_content.lines().collect();
    let mut last_line_added: i32 = -1;

    for (node, _capture_name) in name_captures {
        let start_line = node.start_position().row;

        if start_line >= lines.len() {
            continue;
        }

        if start_line as i32 > last_line_added {
            let line_text = lines[start_line];
            let indentation = line_text
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();

            let def = ParsedDefinition {
                line_index: start_line,
                text: line_text.to_string(),
                indentation,
            };

            last_line_added = start_line as i32;
            definitions.push(def);
        }
    }

    if definitions.is_empty() {
        Ok(None)
    } else {
        Ok(Some(definitions))
    }
}

/// Gets the file extension in lowercase.
fn get_extension(file_path: &str) -> String {
    std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Gets the file skeleton with canonical anchors.
///
pub fn get_file_skeleton(
    anchor_mgr: &AnchorStateManager,
    absolute_path: &str,
    file_content: &str,
    language_parsers: &LanguageParserMap,
    task_id: Option<&str>,
) -> Result<Option<String>, LanguageParserError> {
    let Some(definitions) = parse_file(absolute_path, file_content, language_parsers)? else {
        return Ok(None);
    };

    let lines: Vec<String> = file_content
        .lines()
        .map(std::string::ToString::to_string)
        .collect();
    let anchors = anchor_mgr.reconcile(absolute_path, &lines, task_id);

    let mut formatted_output = String::new();
    let mut last_line_added: i32 = -1;

    for def in definitions {
        let start_line = def.line_index;

        if last_line_added != -1 && start_line as i32 > last_line_added + 1 {
            formatted_output.push_str("|----\n");
        }

        if start_line as i32 > last_line_added {
            let anchor = anchors.get(start_line).cloned().unwrap_or_default();
            formatted_output.push_str(&format!("│{}\n", format_line_with_hash(&def.text, &anchor)));
            last_line_added = start_line as i32;
        }
    }

    if !formatted_output.is_empty() {
        Ok(Some(format!("|----\n{formatted_output}|----\n")))
    } else {
        Ok(None)
    }
}

/// Result of getting functions from a file.
#[derive(Debug, Clone)]
pub struct GetFunctionsResult {
    pub formatted_content: String,
    pub found_names: Vec<String>,
}

/// Gets specific functions with their context and anchors.
///
pub fn get_functions(
    anchor_mgr: &AnchorStateManager,
    absolute_path: &str,
    rel_path: &str,
    function_names: &[String],
    file_content: &str,
    language_parsers: &LanguageParserMap,
    task_id: Option<&str>,
) -> Result<Option<GetFunctionsResult>, LanguageParserError> {
    let ext = get_extension(absolute_path);

    let Some(entry) = language_parsers.get(&ext) else {
        return Ok(Some(GetFunctionsResult {
            formatted_content: format!("Unsupported file type: {rel_path}"),
            found_names: vec![],
        }));
    };

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&entry.language)
        .map_err(|e| LanguageParserError::ParserCreation(e.to_string()))?;

    let Some(tree) = parser.parse(file_content, None) else {
        return Ok(Some(GetFunctionsResult {
            formatted_content: format!("Could not parse file: {rel_path}"),
            found_names: vec![],
        }));
    };

    let all_lines: Vec<String> = file_content
        .lines()
        .map(std::string::ToString::to_string)
        .collect();
    let all_anchors = anchor_mgr.reconcile(absolute_path, &all_lines, task_id);

    let root_node = tree.root_node();

    // Build mappings for nested name resolution.
    // Each match can have multiple captures. We track which match a node belongs to,
    // and for each match, the name text from its "name." capture.
    // This mirrors the TypeScript logic using node.id() -> match mapping.
    let mut node_to_match_id: HashMap<usize, u32> = HashMap::with_capacity(16);
    let mut match_to_name_text: HashMap<u32, String> = HashMap::with_capacity(16);

    {
        let mut qc = tree_sitter::QueryCursor::new();
        let mut caps = qc.captures(&entry.query, root_node, file_content.as_bytes());
        while let Some((m, ci)) = caps.next() {
            let cap = m.captures[*ci];
            let cap_name = entry.query.capture_names()[cap.index as usize];
            let nid = cap.node.id();
            let mid = m.id();

            if cap_name.starts_with("name.") || cap_name.starts_with("definition.") {
                node_to_match_id.insert(nid, mid);
            }
            if cap_name.starts_with("name.")
                && let Ok(text) = cap.node.utf8_text(file_content.as_bytes())
            {
                match_to_name_text.entry(mid).or_insert(text.to_string());
            }
        }
    }

    let mut file_results: Vec<String> = Vec::new();
    let mut found_names_in_file: HashSet<String> = HashSet::new();
    let mut seen_ranges: HashSet<String> = HashSet::new();

    // Process matches again to find functions
    let mut query_cursor2 = tree_sitter::QueryCursor::new();
    let mut captures2 = query_cursor2.captures(&entry.query, root_node, file_content.as_bytes());

    while let Some((match_, _capture_index)) = captures2.next() {
        let name_capture = match_.captures.iter().find(|c| {
            let name = entry.query.capture_names()[c.index as usize];
            name.contains("name.definition")
        });

        let def_capture = match_
            .captures
            .iter()
            .find(|c| {
                let name = entry.query.capture_names()[c.index as usize];
                name.starts_with("definition.")
            })
            .or_else(|| {
                match_.captures.iter().find(|c| {
                    let name = entry.query.capture_names()[c.index as usize];
                    !name.contains("name")
                })
            });

        if let (Some(name_cap), Some(def_cap)) = (name_capture, def_capture) {
            let name_text: String = match name_cap.node.utf8_text(file_content.as_bytes()) {
                Ok(t) => t.to_string(),
                Err(_) => continue,
            };

            // Calculate full name by walking up the tree.
            // Mirrors TypeScript: use node_to_match_id to find parent matches,
            // then get the name text from the parent's "name." capture.
            let mut full_name = name_text.clone();
            let mut current_node = def_cap.node;
            let mut seen_match_ids: HashSet<u32> = HashSet::new();
            if let Some(mid) = node_to_match_id.get(&def_cap.node.id()) {
                seen_match_ids.insert(*mid);
            }

            while let Some(parent) = current_node.parent() {
                current_node = parent;
                if let Some(parent_nid) = node_to_match_id.get(&current_node.id())
                    && !seen_match_ids.contains(parent_nid)
                {
                    seen_match_ids.insert(*parent_nid);
                    if let Some(parent_name) = match_to_name_text.get(parent_nid) {
                        full_name = format!("{parent_name}.{full_name}");
                    }
                }
            }

            let normalized_full_name = full_name.replace("::", ".");
            let mut matched_req_names: Vec<String> = Vec::new();

            for req_name in function_names {
                let normalized_req_name = req_name.replace("::", ".");
                if normalized_full_name == normalized_req_name
                    || normalized_full_name.ends_with(&format!(".{normalized_req_name}"))
                {
                    matched_req_names.push(req_name.clone());
                }
            }

            if !matched_req_names.is_empty() {
                for req_name in &matched_req_names {
                    found_names_in_file.insert(req_name.clone());
                }

                let range = get_extended_range(def_cap.node);
                let range_key = format!("{}-{}", range.start_index, range.end_index);

                if seen_ranges.contains(&range_key) {
                    continue;
                }
                seen_ranges.insert(range_key);

                let def_text = &file_content[range.start_index..range.end_index];
                let def_lines: Vec<&str> = def_text.lines().collect();
                let start_line = range.start_line;
                let end_line = start_line + def_lines.len();
                let def_anchors = if end_line <= all_anchors.len() {
                    &all_anchors[start_line..end_line]
                } else {
                    &all_anchors[start_line..]
                };

                let formatted: Vec<String> = def_lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let anchor = def_anchors.get(i).cloned().unwrap_or_default();
                        format_line_with_hash(line, &anchor)
                    })
                    .collect();

                // Resolve symbol context (imports, class properties)
                let context = if def_cap.node == name_cap.node {
                    String::new()
                } else {
                    let context_options = symbol_context::SymbolContextOptions {
                        node: def_cap.node,
                        file_content,
                        ext: &ext,
                        anchors: &all_anchors,
                        max_context_lines: Some(30),
                    };
                    match symbol_context::resolve_symbol_context(context_options, entry) {
                        Ok(result) if !result.context.is_empty() => {
                            format!("\n--- Symbol Context ---\n{}", result.context)
                        }
                        _ => String::new(),
                    }
                };

                let func_hash = content_hash(def_text);
                file_results.push(format!(
                    "{}::{}\n[Function Hash: {}]\nAll Hash Anchors provided below are stable and can be used with edit_file directly.\n{}{}",
                    rel_path,
                    full_name,
                    func_hash,
                    formatted.join("\n"),
                    context
                ));
            }
        }
    }

    if file_results.is_empty() {
        let missing_note = format!(
            "\n\nNote: The following functions were not found in any of the provided files: {}",
            function_names.join(", ")
        );
        Ok(Some(GetFunctionsResult {
            formatted_content: format!(
                "None of the requested functions ({}) were found in {}{}",
                function_names.join(", "),
                rel_path,
                missing_note
            ),
            found_names: vec![],
        }))
    } else {
        Ok(Some(GetFunctionsResult {
            formatted_content: file_results.join("\n\n---\n\n"),
            found_names: found_names_in_file.into_iter().collect(),
        }))
    }
}

/// A symbol range for replacement.
#[derive(Debug, Clone)]
pub struct SymbolRange {
    pub start_index: usize,
    pub end_index: usize,
    pub start_line: usize,
    pub name_text: String,
}

/// Gets the range of a specific symbol for replacement.
///
pub fn get_symbol_range(
    absolute_path: &str,
    symbol: &str,
    symbol_type: Option<&str>,
    file_content: &str,
    language_parsers: &LanguageParserMap,
) -> Result<Option<SymbolRange>, LanguageParserError> {
    let ext = get_extension(absolute_path);

    let Some(entry) = language_parsers.get(&ext) else {
        return Ok(None);
    };

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&entry.language)
        .map_err(|e| LanguageParserError::ParserCreation(e.to_string()))?;

    let Some(tree) = parser.parse(file_content, None) else {
        return Ok(None);
    };
    let root_node = tree.root_node();

    // Build mappings for nested name resolution (same as get_functions)
    let mut node_to_match_id: HashMap<usize, u32> = HashMap::with_capacity(16);
    let mut match_to_name_text: HashMap<u32, String> = HashMap::with_capacity(16);

    {
        let mut qc = tree_sitter::QueryCursor::new();
        let mut caps = qc.captures(&entry.query, root_node, file_content.as_bytes());
        while let Some((m, ci)) = caps.next() {
            let cap = m.captures[*ci];
            let cap_name = entry.query.capture_names()[cap.index as usize];
            let nid = cap.node.id();
            let mid = m.id();

            if cap_name.starts_with("name.") || cap_name.starts_with("definition.") {
                node_to_match_id.insert(nid, mid);
            }
            if cap_name.starts_with("name.")
                && let Ok(t) = cap.node.utf8_text(file_content.as_bytes())
            {
                match_to_name_text.entry(mid).or_insert(t.to_string());
            }
        }
    }

    let normalized_requested_symbol = symbol.replace("::", ".");

    let mut query_cursor2 = tree_sitter::QueryCursor::new();
    let mut captures2 = query_cursor2.captures(&entry.query, root_node, file_content.as_bytes());

    while let Some((match_, _capture_index)) = captures2.next() {
        let name_capture = match_.captures.iter().find(|c| {
            let name = entry.query.capture_names()[c.index as usize];
            name.starts_with("name.definition")
        });

        let def_capture = match_
            .captures
            .iter()
            .find(|c| {
                let name = entry.query.capture_names()[c.index as usize];
                name.starts_with("definition.")
            })
            .or_else(|| {
                match_.captures.iter().find(|c| {
                    let name = entry.query.capture_names()[c.index as usize];
                    !name.starts_with("name.")
                })
            });

        if let (Some(name_cap), Some(def_cap)) = (name_capture, def_capture) {
            let name_text: String = match name_cap.node.utf8_text(file_content.as_bytes()) {
                Ok(t) => t.to_string(),
                Err(_) => continue,
            };

            let def_type = entry.query.capture_names()[def_cap.index as usize]
                .split('.')
                .next_back()
                .unwrap_or("")
                .to_string();

            // Calculate full name by walking up the tree.
            // Mirrors TypeScript: use node_to_match_id to find parent matches,
            // then get the name text from the parent's "name." capture.
            let mut full_name = name_text.clone();
            let mut current_node = def_cap.node;
            let mut seen_match_ids: HashSet<u32> = HashSet::new();
            if let Some(mid) = node_to_match_id.get(&def_cap.node.id()) {
                seen_match_ids.insert(*mid);
            }

            while let Some(parent) = current_node.parent() {
                current_node = parent;
                if let Some(parent_nid) = node_to_match_id.get(&current_node.id())
                    && !seen_match_ids.contains(parent_nid)
                {
                    seen_match_ids.insert(*parent_nid);
                    if let Some(parent_name) = match_to_name_text.get(parent_nid) {
                        full_name = format!("{parent_name}.{full_name}");
                    }
                }
            }

            let normalized_full_name = full_name.replace("::", ".");

            if (normalized_full_name == normalized_requested_symbol
                || normalized_full_name.ends_with(&format!(".{normalized_requested_symbol}")))
                && are_types_compatible(&def_type, symbol_type)
            {
                let range = get_extended_range(def_cap.node);
                return Ok(Some(SymbolRange {
                    start_index: range.start_index,
                    end_index: range.end_index,
                    start_line: range.start_line,
                    name_text,
                }));
            }
        }
    }

    Ok(None)
}

/// Checks if two type names are compatible.
fn are_types_compatible(def_type: &str, req_type: Option<&str>) -> bool {
    match req_type {
        None => true,
        Some(req) if def_type == req => true,
        Some(req) => {
            let synonyms = ["function", "method"];
            synonyms.contains(&def_type) && synonyms.contains(&req)
        }
    }
}

/// Extended range including wrapper types and preceding comments.
#[derive(Debug, Clone)]
pub struct ExtendedRange {
    pub start_index: usize,
    pub end_index: usize,
    pub start_line: usize,
}

/// Gets the extended range of a node, including wrapper types and preceding comments.
///
#[must_use]
pub fn get_extended_range(target_node: tree_sitter::Node) -> ExtendedRange {
    let mut start_index = target_node.start_byte();
    let mut end_index = target_node.end_byte();
    let mut start_line = target_node.start_position().row;

    let mut current_node = target_node;
    let wrapper_types = [
        "export_statement",
        "export_declaration",
        "ambient_declaration",
        "decorated_definition",
        "internal_module",
    ];

    while let Some(parent) = current_node.parent() {
        if wrapper_types.contains(&parent.kind()) {
            current_node = parent;
            start_index = current_node.start_byte();
            end_index = current_node.end_byte();
            start_line = current_node.start_position().row;
        } else {
            break;
        }
    }

    while let Some(prev) = current_node.prev_named_sibling() {
        let prev_type = prev.kind();
        if prev_type == "comment"
            || prev_type == "decorator"
            || prev_type == "attribute"
            || prev_type.contains("comment")
        {
            start_index = prev.start_byte();
            start_line = prev.start_position().row;
            current_node = prev;
        } else {
            break;
        }
    }

    ExtendedRange {
        start_index,
        end_index,
        start_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_extension() {
        assert_eq!(get_extension("file.rs"), "rs");
        assert_eq!(get_extension("file.JS"), "js");
    }
}
