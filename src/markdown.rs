//! GitHub-flavoured Markdown rendering with HTML sanitisation.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use comrak::adapters::SyntaxHighlighterAdapter;
use comrak::options::Plugins;
use comrak::{Options, markdown_to_html_with_plugins};

use crate::highlight;

/// Where relative links and images inside a rendered document should point.
#[derive(Debug, Clone)]
pub struct LinkBase {
    /// e.g. `/alice/project/blob/main`
    pub blob_root: String,
    /// e.g. `/alice/project/raw/main`
    pub raw_root: String,
    /// Directory of the document inside the repository, e.g. `docs`.
    pub dir: String,
}

struct Highlighter;

impl SyntaxHighlighterAdapter for Highlighter {
    fn write_highlighted(
        &self,
        output: &mut dyn fmt::Write,
        lang: Option<&str>,
        code: &str,
    ) -> fmt::Result {
        let html = lang
            .and_then(|l| highlight::highlight_block(l, code))
            .unwrap_or_else(|| highlight::escape(code));
        output.write_str(&html)
    }

    fn write_pre_tag(
        &self,
        output: &mut dyn fmt::Write,
        _attributes: HashMap<&'static str, Cow<'_, str>>,
    ) -> fmt::Result {
        output.write_str("<pre>")
    }

    fn write_code_tag(
        &self,
        output: &mut dyn fmt::Write,
        _attributes: HashMap<&'static str, Cow<'_, str>>,
    ) -> fmt::Result {
        output.write_str("<code>")
    }
}

const TOKEN_CLASSES: &[&str] = &[
    "tok-c", "tok-sr", "tok-e", "tok-s", "tok-n", "tok-kc", "tok-o", "tok-k", "tok-f", "tok-tag",
    "tok-attr", "tok-h", "tok-t", "tok-m", "tok-v", "tok-b", "tok-i", "tok-ins", "tok-del",
    "tok-link",
];

/// Renders untrusted Markdown to sanitised HTML.
pub fn render(source: &str, links: Option<&LinkBase>) -> String {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.render.r#unsafe = true; // raw HTML is allowed through, then sanitised below

    let highlighter = Highlighter;
    let mut plugins = Plugins::default();
    plugins.render.codefence_syntax_highlighter = Some(&highlighter);
    let html = markdown_to_html_with_plugins(source, &options, &plugins);

    let mut builder = ammonia::Builder::default();
    builder
        .add_tags(["input"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tag_attributes("img", ["width", "height", "align"])
        .add_generic_attributes(["align"])
        .add_allowed_classes("span", TOKEN_CLASSES)
        .link_rel(Some("noopener noreferrer nofollow"))
        .url_relative(ammonia::UrlRelative::PassThrough);
    if let Some(base) = links.cloned() {
        builder.attribute_filter(
            move |element, attribute, value| match (element, attribute) {
                ("a", "href") => Some(rewrite_relative(value, &base.blob_root, &base.dir)),
                ("img", "src") => Some(rewrite_relative(value, &base.raw_root, &base.dir)),
                _ => Some(Cow::Borrowed(value)),
            },
        );
    }
    builder.clean(&html).to_string()
}

fn rewrite_relative<'u>(value: &'u str, root: &str, dir: &str) -> Cow<'u, str> {
    let is_absolute = value.starts_with('#')
        || value.starts_with("//")
        || value
            .split_once(':')
            .is_some_and(|(scheme, _)| !scheme.contains('/') && !scheme.is_empty());
    if is_absolute || value.is_empty() {
        return Cow::Borrowed(value);
    }
    let (path, suffix) = match value.find(['?', '#']) {
        Some(i) => value.split_at(i),
        None => (value, ""),
    };
    let joined = if let Some(rooted) = path.strip_prefix('/') {
        rooted.to_string()
    } else if dir.is_empty() {
        path.to_string()
    } else {
        format!("{dir}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Cow::Owned(format!("{root}/{}{suffix}", parts.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts() {
        let out = render(
            "hello <script>alert(1)</script> <img src=x onerror=alert(1)>",
            None,
        );
        assert!(!out.contains("<script"));
        assert!(!out.contains("onerror"));
    }

    #[test]
    fn rewrites_relative_links() {
        let base = LinkBase {
            blob_root: "/a/r/blob/main".into(),
            raw_root: "/a/r/raw/main".into(),
            dir: "docs".into(),
        };
        let out = render(
            "[x](../src/lib.rs#L3) ![i](img/logo.png) [y](https://e.com) [z](#top)",
            Some(&base),
        );
        assert!(
            out.contains(r#"href="/a/r/blob/main/src/lib.rs#L3""#),
            "{out}"
        );
        assert!(
            out.contains(r#"src="/a/r/raw/main/docs/img/logo.png""#),
            "{out}"
        );
        assert!(out.contains(r#"href="https://e.com""#), "{out}");
        assert!(out.contains(r##"href="#top""##), "{out}");
    }

    #[test]
    fn highlights_fenced_code() {
        let out = render("```rust\nfn main() {}\n```", None);
        assert!(out.contains(r#"<span class="tok-k">fn</span>"#), "{out}");
    }

    #[test]
    fn renders_task_lists() {
        let out = render("- [x] done\n- [ ] todo", None);
        assert!(out.contains("checkbox"), "{out}");
    }
}
