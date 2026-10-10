//! Server-side syntax highlighting.
//!
//! Scopes from syntect grammars are collapsed into a small set of token
//! classes (`tok-k`, `tok-s`, ...) so colours live in our stylesheet and
//! follow the light/dark theme. Each line is rendered self-contained, which
//! lets the code view and diffs place lines in separate table rows.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::LazyLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Files larger than this are shown without highlighting.
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;

const SCOPE_CLASSES: &[(&str, &str)] = &[
    ("comment", "c"),
    ("punctuation.definition.comment", "c"),
    ("string.regexp", "sr"),
    ("constant.character.escape", "e"),
    ("string", "s"),
    ("markup.raw", "s"),
    ("constant.numeric", "n"),
    ("constant", "kc"),
    ("variable.language", "kc"),
    ("support.constant", "kc"),
    ("keyword.operator", "o"),
    ("keyword", "k"),
    ("storage", "k"),
    ("entity.name.function", "f"),
    ("support.function", "f"),
    ("variable.function", "f"),
    ("entity.name.tag", "tag"),
    ("entity.other.attribute-name", "attr"),
    ("support.type.property-name", "attr"),
    ("entity.name.section", "h"),
    ("markup.heading", "h"),
    ("entity.name", "t"),
    ("entity.other.inherited-class", "t"),
    ("support.type", "t"),
    ("support.class", "t"),
    ("meta.annotation", "m"),
    ("meta.attribute", "m"),
    ("variable.parameter", "v"),
    ("markup.bold", "b"),
    ("markup.italic", "i"),
    ("markup.inserted", "ins"),
    ("markup.deleted", "del"),
    ("markup.underline.link", "link"),
];

fn class_for_scope(scope: Scope) -> Option<&'static str> {
    let name = scope.build_string();
    SCOPE_CLASSES
        .iter()
        .find(|(prefix, _)| {
            name == *prefix || (name.starts_with(prefix) && name.as_bytes()[prefix.len()] == b'.')
        })
        .map(|(_, class)| *class)
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_into(&mut out, s);
    out
}

fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

pub fn find_syntax(path: &str, first_line: &str) -> Option<&'static SyntaxReference> {
    let ss = &*SYNTAXES;
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let ext = file_name.rsplit_once('.').map(|(_, e)| e);
    ext.and_then(|e| ss.find_syntax_by_extension(e))
        .or_else(|| ss.find_syntax_by_extension(file_name))
        .or_else(|| ss.find_syntax_by_first_line(first_line))
        .filter(|s| s.name != "Plain Text")
}

/// Human readable language name for a file, if known.
pub fn language_name(path: &str, first_line: &str) -> Option<&'static str> {
    find_syntax(path, first_line).map(|s| s.name.as_str())
}

struct LineHighlighter {
    state: ParseState,
    stack: ScopeStack,
    cache: HashMap<Scope, Option<&'static str>>,
    failed: bool,
}

impl LineHighlighter {
    fn new(syntax: &SyntaxReference) -> Self {
        LineHighlighter {
            state: ParseState::new(syntax),
            stack: ScopeStack::new(),
            cache: HashMap::new(),
            failed: false,
        }
    }

    fn current_class(&mut self) -> Option<&'static str> {
        for scope in self.stack.as_slice().iter().rev() {
            let class = *self
                .cache
                .entry(*scope)
                .or_insert_with(|| class_for_scope(*scope));
            if class.is_some() {
                return class;
            }
        }
        None
    }

    /// Highlights one line (without its trailing newline).
    fn line(&mut self, text: &str) -> String {
        if self.failed {
            return escape(text);
        }
        let with_newline = format!("{text}\n");
        let ops = match self.state.parse_line(&with_newline, &SYNTAXES) {
            Ok(ops) => ops,
            Err(_) => {
                self.failed = true;
                return escape(text);
            }
        };
        let mut out = String::with_capacity(text.len() * 2);
        let mut pending = String::new();
        let mut pending_class: Option<&'static str> = None;
        let mut cursor = 0;

        let flush = |out: &mut String, pending: &mut String, class: Option<&'static str>| {
            if pending.is_empty() {
                return;
            }
            match class {
                Some(c) => {
                    let _ = write!(out, r#"<span class="tok-{c}">"#);
                    escape_into(out, pending);
                    out.push_str("</span>");
                }
                None => escape_into(out, pending),
            }
            pending.clear();
        };

        for (pos, op) in ops {
            let pos = pos.min(text.len());
            if pos > cursor {
                let class = self.current_class();
                if class != pending_class {
                    flush(&mut out, &mut pending, pending_class);
                    pending_class = class;
                }
                pending.push_str(&text[cursor..pos]);
                cursor = pos;
            }
            if self.stack.apply(&op).is_err() {
                self.failed = true;
                break;
            }
        }
        if cursor < text.len() {
            let class = if self.failed {
                None
            } else {
                self.current_class()
            };
            if class != pending_class {
                flush(&mut out, &mut pending, pending_class);
                pending_class = class;
            }
            pending.push_str(&text[cursor..]);
        }
        flush(&mut out, &mut pending, pending_class);
        out
    }
}

/// Highlights a sequence of lines from the file at `path`, returning one
/// HTML fragment per input line.
pub fn highlight_lines(path: &str, lines: &[&str]) -> Vec<String> {
    let total: usize = lines.iter().map(|l| l.len()).sum();
    let syntax = (total <= MAX_HIGHLIGHT_BYTES)
        .then(|| find_syntax(path, lines.first().copied().unwrap_or("")))
        .flatten();
    match syntax {
        Some(syntax) => {
            let mut h = LineHighlighter::new(syntax);
            lines.iter().map(|l| h.line(l)).collect()
        }
        None => lines.iter().map(|l| escape(l)).collect(),
    }
}

/// Highlights a code block in a known language (used for Markdown fences).
pub fn highlight_block(lang: &str, code: &str) -> Option<String> {
    let syntax = SYNTAXES
        .find_syntax_by_token(lang)
        .filter(|s| s.name != "Plain Text")?;
    if code.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let mut h = LineHighlighter::new(syntax);
    let lines: Vec<String> = code.lines().map(|l| h.line(l)).collect();
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_keywords_and_strings() {
        let out = highlight_lines("main.rs", &["fn main() {", "    let s = \"<hi>\";", "}"]);
        assert_eq!(out.len(), 3);
        assert!(
            out[0].contains(r#"<span class="tok-k">fn</span>"#),
            "{}",
            out[0]
        );
        assert!(out[1].contains("&lt;hi&gt;"), "{}", out[1]);
        assert!(out[1].contains("tok-s"), "{}", out[1]);
    }

    #[test]
    fn unknown_files_are_escaped() {
        let out = highlight_lines("notes.unknownext", &["<b>&</b>"]);
        assert_eq!(out, vec!["&lt;b&gt;&amp;&lt;/b&gt;"]);
    }

    #[test]
    fn multiline_comment_classes_every_line() {
        let out = highlight_lines("a.c", &["/* one", "two */ int x;"]);
        assert!(
            out[1].starts_with(r#"<span class="tok-c">two */</span>"#),
            "{}",
            out[1]
        );
    }
}
