//! Lightweight local syntax highlighting.  It intentionally tolerates broken
//! input and is available before any language server response.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Toml,
    Markdown,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Highlight {
    Plain,
    Keyword,
    Type,
    String,
    Comment,
    Number,
    Function,
    Macro,
    Heading,
    Punctuation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: Highlight,
}

pub fn language_for_path(path: Option<&Path>) -> Language {
    match path.and_then(Path::extension).and_then(|x| x.to_str()) {
        Some("rs") => Language::Rust,
        Some("toml") => Language::Toml,
        Some("md" | "markdown") => Language::Markdown,
        _ => Language::Plain,
    }
}

/// A pathological generated line must not delay a frame to color offscreen text.
/// Text beyond this prefix remains readable with the plain-text style.
pub const MAX_HIGHLIGHT_BYTES: usize = 16 * 1024;

pub fn highlight_line(language: Language, line: &str) -> Vec<Span> {
    let mut end = line.len().min(MAX_HIGHLIGHT_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let line = &line[..end];
    match language {
        Language::Rust => rust(line),
        Language::Toml => toml(line),
        Language::Markdown => markdown(line),
        Language::Plain => Vec::new(),
    }
}

fn rust(line: &str) -> Vec<Span> {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while", "yield",
    ];
    const TYPES: &[&str] = &[
        "bool", "char", "str", "String", "Option", "Result", "Vec", "usize", "isize", "u8", "u16",
        "u32", "u64", "u128", "i8", "i16", "i32", "i64", "i128", "f32", "f64",
    ];
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            spans.push(Span {
                start: index,
                end: bytes.len(),
                kind: Highlight::Comment,
            });
            break;
        }
        let c = bytes[index];
        if c == b'"' || c == b'\'' {
            let quote = c;
            let start = index;
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let current = bytes[index];
                index += 1;
                if current == quote && !escaped {
                    break;
                }
                escaped = current == b'\\' && !escaped;
                if current != b'\\' {
                    escaped = false;
                }
            }
            spans.push(Span {
                start,
                end: index,
                kind: Highlight::String,
            });
        } else if c.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'.'))
            {
                index += 1;
            }
            spans.push(Span {
                start,
                end: index,
                kind: Highlight::Number,
            });
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            let word = &line[start..index];
            let next = bytes.get(index).copied();
            let kind = if KEYWORDS.contains(&word) {
                Highlight::Keyword
            } else if TYPES.contains(&word) || word.chars().next().is_some_and(char::is_uppercase) {
                Highlight::Type
            } else if next == Some(b'!') {
                Highlight::Macro
            } else if next == Some(b'(') {
                Highlight::Function
            } else {
                Highlight::Plain
            };
            if kind != Highlight::Plain {
                spans.push(Span {
                    start,
                    end: index,
                    kind,
                });
            }
        } else {
            index += char_len(bytes[index]);
        }
    }
    spans
}

fn toml(line: &str) -> Vec<Span> {
    let trimmed = line.trim_start();
    let offset = line.len() - trimmed.len();
    if trimmed.starts_with('#') {
        return vec![Span {
            start: offset,
            end: line.len(),
            kind: Highlight::Comment,
        }];
    }
    if trimmed.starts_with('[') {
        return vec![Span {
            start: offset,
            end: line.len(),
            kind: Highlight::Heading,
        }];
    }
    let mut spans = Vec::new();
    if let Some(eq) = line.find('=') {
        spans.push(Span {
            start: offset,
            end: line[..eq].trim_end().len(),
            kind: Highlight::Keyword,
        });
    }
    string_and_comment_spans(line, &mut spans);
    spans
}

fn markdown(line: &str) -> Vec<Span> {
    let trimmed = line.trim_start();
    let offset = line.len() - trimmed.len();
    if trimmed.starts_with('#') {
        return vec![Span {
            start: offset,
            end: line.len(),
            kind: Highlight::Heading,
        }];
    }
    if trimmed.starts_with("```") {
        return vec![Span {
            start: offset,
            end: line.len(),
            kind: Highlight::Keyword,
        }];
    }
    let mut spans = Vec::new();
    let mut start = None;
    for (index, ch) in line.char_indices() {
        if ch == '`' {
            if let Some(open) = start.take() {
                spans.push(Span {
                    start: open,
                    end: index + 1,
                    kind: Highlight::String,
                });
            } else {
                start = Some(index);
            }
        }
    }
    spans
}

fn string_and_comment_spans(line: &str, spans: &mut Vec<Span>) {
    let mut quote = None;
    let mut start = 0;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if let Some(active) = quote {
            if ch == active && !escaped {
                spans.push(Span {
                    start,
                    end: index + ch.len_utf8(),
                    kind: Highlight::String,
                });
                quote = None;
            }
            escaped = ch == '\\' && !escaped;
            if ch != '\\' {
                escaped = false;
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
            start = index;
        } else if ch == '#' {
            spans.push(Span {
                start: index,
                end: line.len(),
                kind: Highlight::Comment,
            });
            return;
        }
    }
    if quote.is_some() {
        spans.push(Span {
            start,
            end: line.len(),
            kind: Highlight::String,
        });
    }
}

fn char_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_highlight_works_on_incomplete_input() {
        let spans = highlight_line(Language::Rust, "pub fn main( { // later");
        assert!(spans.iter().any(|s| s.kind == Highlight::Keyword));
        assert!(spans.iter().any(|s| s.kind == Highlight::Comment));
    }

    #[test]
    fn detects_languages() {
        assert_eq!(
            language_for_path(Some(Path::new("Cargo.toml"))),
            Language::Toml
        );
        assert_eq!(
            language_for_path(Some(Path::new("src/main.rs"))),
            Language::Rust
        );
    }

    #[test]
    fn pathological_lines_have_a_bounded_utf8_safe_highlight_prefix() {
        let line = format!(
            "{}{}let hidden = 1;",
            " ".repeat(MAX_HIGHLIGHT_BYTES - 1),
            "界"
        );
        for language in [Language::Rust, Language::Toml, Language::Markdown] {
            let spans = highlight_line(language, &line);
            assert!(spans.iter().all(|span| span.end <= MAX_HIGHLIGHT_BYTES));
            assert!(spans.iter().all(|span| {
                line.is_char_boundary(span.start) && line.is_char_boundary(span.end)
            }));
        }

        let line = "let value = 1; ".repeat(MAX_HIGHLIGHT_BYTES);
        let spans = highlight_line(Language::Rust, &line);
        assert!(!spans.is_empty());
        assert!(spans.iter().all(|span| span.end <= MAX_HIGHLIGHT_BYTES));
    }
}
