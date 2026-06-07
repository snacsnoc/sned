//! Lightweight ANSI syntax highlighting for rendered code blocks.

use crate::cli::colors::style;
use tree_sitter::{Language, Node, Parser};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TokenStyle {
    Keyword,
    String,
    Number,
    Comment,
    Type,
    Function,
}

impl TokenStyle {
    fn ansi(self) -> String {
        match self {
            Self::Keyword => format!("{}{}", style::BOLD, style::CYAN),
            Self::String => style::GREEN.to_string(),
            Self::Number => style::YELLOW.to_string(),
            Self::Comment => format!("{}{}", style::DIM, style::GRAY),
            Self::Type => style::YELLOW.to_string(),
            Self::Function => format!("{}{}", style::BOLD, style::WHITE),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HighlightSpan {
    start: usize,
    end: usize,
    style: TokenStyle,
}

/// Highlight a code block with ANSI color sequences when a matching grammar is available.
///
/// Unsupported languages, disabled grammar features, parse failures, and `NO_COLOR` all fall back
/// to returning the original code unchanged.
pub fn highlight_code(code: &str, lang: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() || code.is_empty() {
        return code.to_string();
    }

    let Some(language) = language_for(lang) else {
        return code.to_string();
    };

    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return code.to_string();
    }

    let Some(tree) = parser.parse(code, None) else {
        return code.to_string();
    };

    let mut spans = Vec::new();
    collect_spans(tree.root_node(), code.as_bytes(), &mut spans);
    render_highlights(code, &spans).unwrap_or_else(|| render_lexical_highlights(code))
}

#[cfg(feature = "lang-rust")]
fn rust_language() -> Language {
    Language::new(tree_sitter_rust::LANGUAGE)
}

#[cfg(feature = "lang-python")]
fn python_language() -> Language {
    Language::new(tree_sitter_python::LANGUAGE)
}

#[cfg(feature = "lang-javascript")]
fn javascript_language() -> Language {
    Language::new(tree_sitter_javascript::LANGUAGE)
}

#[cfg(feature = "lang-typescript")]
fn typescript_language() -> Language {
    Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
}

#[cfg(feature = "lang-go")]
fn go_language() -> Language {
    Language::new(tree_sitter_go::LANGUAGE)
}

fn language_for(lang: &str) -> Option<Language> {
    match normalize_lang(lang).as_str() {
        #[cfg(feature = "lang-rust")]
        "rust" | "rs" => Some(rust_language()),
        #[cfg(feature = "lang-python")]
        "python" | "py" => Some(python_language()),
        #[cfg(feature = "lang-javascript")]
        "javascript" | "js" | "jsx" | "node" => Some(javascript_language()),
        #[cfg(feature = "lang-typescript")]
        "typescript" | "ts" | "tsx" => Some(typescript_language()),
        #[cfg(feature = "lang-go")]
        "go" | "golang" => Some(go_language()),
        _ => None,
    }
}

fn normalize_lang(lang: &str) -> String {
    lang.trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .replace(['_', ' '], "-")
}

fn collect_spans(node: Node<'_>, source: &[u8], spans: &mut Vec<HighlightSpan>) {
    if let Some(style) = classify_node(node) {
        spans.push(HighlightSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            style,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() < child.end_byte() && child.end_byte() <= source.len() {
            collect_spans(child, source, spans);
        }
    }
}

fn classify_node(node: Node<'_>) -> Option<TokenStyle> {
    let kind = node.kind();

    if kind.contains("comment") {
        return Some(TokenStyle::Comment);
    }

    if kind.contains("string") || kind.contains("char") || kind == "raw_string_literal" {
        return Some(TokenStyle::String);
    }

    if kind.contains("integer")
        || kind.contains("float")
        || kind.contains("number")
        || kind == "interpreted_string_literal"
        || kind == "raw_string_literal"
    {
        return Some(TokenStyle::Number);
    }

    if matches!(
        kind,
        "primitive_type"
            | "type_identifier"
            | "predefined_type"
            | "builtin_type"
            | "interface_type"
    ) {
        return Some(TokenStyle::Type);
    }

    if matches!(
        kind,
        "function_identifier" | "field_identifier" | "method_identifier"
    ) {
        return Some(TokenStyle::Function);
    }

    if is_keyword(kind) {
        return Some(TokenStyle::Keyword);
    }

    None
}

fn is_keyword(kind: &str) -> bool {
    matches!(
        kind,
        "as" | "async"
            | "await"
            | "break"
            | "case"
            | "class"
            | "const"
            | "continue"
            | "defer"
            | "def"
            | "default"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "fn"
            | "for"
            | "from"
            | "func"
            | "function"
            | "go"
            | "if"
            | "impl"
            | "import"
            | "in"
            | "interface"
            | "let"
            | "match"
            | "mod"
            | "mut"
            | "package"
            | "pub"
            | "range"
            | "return"
            | "self"
            | "struct"
            | "switch"
            | "trait"
            | "true"
            | "type"
            | "use"
            | "var"
            | "where"
            | "while"
            | "yield"
    )
}

fn render_highlights(code: &str, spans: &[HighlightSpan]) -> Option<String> {
    let mut spans = spans
        .iter()
        .copied()
        .filter(|span| span.start < span.end && span.end <= code.len())
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| (span.start, span.end));

    let mut rendered = String::with_capacity(code.len() + spans.len() * 8);
    let mut cursor = 0;
    let mut emitted = false;

    for span in spans {
        if span.start < cursor
            || !code.is_char_boundary(span.start)
            || !code.is_char_boundary(span.end)
        {
            continue;
        }

        rendered.push_str(&code[cursor..span.start]);
        rendered.push_str(&span.style.ansi());
        rendered.push_str(&code[span.start..span.end]);
        rendered.push_str(style::RESET);
        cursor = span.end;
        emitted = true;
    }

    rendered.push_str(&code[cursor..]);
    emitted.then_some(rendered)
}

fn render_lexical_highlights(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + 32);
    let mut chars = code.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        if ch == '"' || ch == '\'' || ch == '`' {
            let quote = ch;
            let mut end = start + ch.len_utf8();
            let mut escaped = false;
            for (idx, next) in chars.by_ref() {
                end = idx + next.len_utf8();
                if escaped {
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == quote {
                    break;
                }
            }
            push_styled(&mut out, &code[start..end], TokenStyle::String);
            continue;
        }

        if ch == '/' && chars.peek().is_some_and(|(_, next)| *next == '/') {
            let end = code[start..]
                .find('\n')
                .map(|offset| start + offset)
                .unwrap_or(code.len());
            push_styled(&mut out, &code[start..end], TokenStyle::Comment);
            if end < code.len() {
                out.push('\n');
                while let Some((idx, _)) = chars.peek() {
                    if *idx <= end {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            continue;
        }

        if ch == '#' {
            let end = code[start..]
                .find('\n')
                .map(|offset| start + offset)
                .unwrap_or(code.len());
            push_styled(&mut out, &code[start..end], TokenStyle::Comment);
            if end < code.len() {
                out.push('\n');
                while let Some((idx, _)) = chars.peek() {
                    if *idx <= end {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            continue;
        }

        if ch.is_ascii_digit() {
            let mut end = start + ch.len_utf8();
            while let Some((idx, next)) = chars.peek() {
                if next.is_ascii_digit() || *next == '.' || *next == '_' {
                    end = *idx + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            push_styled(&mut out, &code[start..end], TokenStyle::Number);
            continue;
        }

        if ch == '_' || ch.is_ascii_alphabetic() {
            let mut end = start + ch.len_utf8();
            while let Some((idx, next)) = chars.peek() {
                if *next == '_' || next.is_ascii_alphanumeric() {
                    end = *idx + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }

            let word = &code[start..end];
            if is_keyword(word) {
                push_styled(&mut out, word, TokenStyle::Keyword);
            } else if next_non_space_starts_call(code, end) {
                push_styled(&mut out, word, TokenStyle::Function);
            } else if word
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_uppercase())
                || matches!(
                    word,
                    "str" | "string" | "int" | "bool" | "usize" | "i32" | "u64"
                )
            {
                push_styled(&mut out, word, TokenStyle::Type);
            } else {
                out.push_str(word);
            }
            continue;
        }

        out.push(ch);
    }

    out
}

fn next_non_space_starts_call(code: &str, offset: usize) -> bool {
    code[offset..]
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| ch == '(')
}

fn push_styled(out: &mut String, text: &str, token_style: TokenStyle) {
    out.push_str(&token_style.ansi());
    out.push_str(text);
    out.push_str(style::RESET);
}

#[cfg(test)]
mod tests {
    use super::highlight_code;
    use crate::test_support::env_lock;

    fn assert_colored(code: &str, lang: &str) {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let previous = std::env::var_os("NO_COLOR");
        // SAFETY: env mutation guarded by mutex; no other thread reads NO_COLOR concurrently
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        let highlighted = highlight_code(code, lang);
        // SAFETY: env mutation guarded by mutex; restoring previous value
        unsafe {
            match previous {
                Some(value) => std::env::set_var("NO_COLOR", value),
                None => std::env::remove_var("NO_COLOR"),
            }
        }
        assert!(
            highlighted.contains("\x1b["),
            "expected ANSI color for {lang}, got: {highlighted:?}"
        );
        assert_ne!(highlighted, code);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn highlights_rust() {
        assert_colored("pub fn main() { let count = 42; }", "rust");
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn highlights_python() {
        assert_colored("def main():\n    return \"ok\"\n", "python");
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn highlights_javascript() {
        assert_colored(
            "function main() { const count = 42; return count; }",
            "javascript",
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn highlights_typescript() {
        assert_colored(
            "interface User { name: string }\nconst user: User = { name: \"Ada\" };",
            "ts",
        );
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn highlights_go() {
        assert_colored("package main\nfunc main() { var count = 42 }\n", "go");
    }

    #[test]
    fn unsupported_language_returns_uncolored_code() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let code = "SELECT * FROM users";
        assert_eq!(highlight_code(code, "sql"), code);
    }

    #[test]
    fn no_color_returns_uncolored_code() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let code = "fn main() { let count = 42; }";
        let previous = std::env::var_os("NO_COLOR");
        // SAFETY: env mutation guarded by mutex; no other thread reads NO_COLOR concurrently
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }
        assert_eq!(highlight_code(code, "rust"), code);
        // SAFETY: env mutation guarded by mutex; restoring previous value
        unsafe {
            match previous {
                Some(value) => std::env::set_var("NO_COLOR", value),
                None => std::env::remove_var("NO_COLOR"),
            }
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn highlights_typical_block_under_five_ms() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let previous = std::env::var_os("NO_COLOR");
        // SAFETY: env mutation guarded by mutex; no other thread reads NO_COLOR concurrently
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        let code = r#"
pub struct User {
    name: String,
}

impl User {
    pub fn greet(&self) -> String {
        format!("hello {}", self.name)
    }
}
"#;

        let start = std::time::Instant::now();
        let highlighted = highlight_code(code, "rust");
        let elapsed = start.elapsed();

        // SAFETY: env mutation guarded by mutex; restoring previous value
        unsafe {
            match previous {
                Some(value) => std::env::set_var("NO_COLOR", value),
                None => std::env::remove_var("NO_COLOR"),
            }
        }

        assert!(highlighted.contains("\x1b["));
        assert!(
            elapsed < std::time::Duration::from_millis(5),
            "highlighting took {:?}",
            elapsed
        );
    }
}
