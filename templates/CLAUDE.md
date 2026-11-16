# Templates (askama 0.16)

Templates are compiled into the binary at build time, so `cargo build` reports template errors as Rust type errors pointing at the `#[derive(Template)]` struct. Each page's struct sits next to its handler in `src/web/*.rs`.

## Layouts

```
base.html                  blocks: title, page_class, subheader, content
├── layout_repo.html       repository header and tabs; blocks: page_title, content
└── layout_settings.html   account settings side nav; block: settings
```

- Every page struct needs `ctx: Ctx`, because `base.html` reads `ctx.user`, `ctx.flash` and `ctx.path`.
- Repo pages also need `r: RepoCtx` and `tab: &'static str`. `tab` is one of `code`, `commits`, `branches`, `pulls` or `settings` and selects the active tab.
- Settings pages need `tab` (`profile` or `keys`).
- In repo pages, `page_title` is a prefix ending in ` · `, e.g. `{% block page_title %}Branches · {% endblock %}`.
- **Never create a file named `base.html` in a subdirectory.** askama resolves `{% extends %}` relative to the current template's directory first, so `repo/*.html` extending `"base.html"` would pick up `repo/base.html` and fail with a cyclic dependency. That's why section layouts are named `layout_*.html` and kept at the root.

## Partials (`{% include %}`, which share the including template's scope)

| Partial | Variables it expects in scope |
| --- | --- |
| `partials/diff.html` | `diff: DiffSummary`, `file_ref` (commit SHA or URL-encoded ref to link "View file" at), `base` (repo URL) |
| `partials/commit_list.html` | `groups: Vec<CommitGroup>`, `base` |
| `partials/refsel.html` | `refsel: RefSelector` |
| `partials/clone.html` | `r: RepoCtx`, `view` with a `refname` field (used for archive links) |
| `partials/repo_list.html` | `repos: Vec<Repo>` |

Bind them with `{% let %}` before including, e.g. `{% let file_ref = commit.id.clone() %}`, or `{% let repos = recent %}` to reuse the repo list.

## Calling Rust from templates

Helpers live in `src/web/fmt.rs` and are called by full path:

```jinja
{{ crate::web::fmt::icon("git-branch")|safe }}            {# names: match arms in src/icons.rs #}
{{ crate::web::fmt::avatar(user.username, 20)|safe }}
<time title="{{ crate::web::fmt::datetime_full(c.committer_time) }}">{{ crate::web::fmt::ago(c.committer_time) }}</time>
{{ crate::web::fmt::plural(view.issue_count, "issue") }} {# i64 counts: "1 issue", "3 issues" #}
{{ crate::web::fmt::plural_size(view.commit_count, "commit") }} {# usize counts: "1 commit", "2 branches" #}
{{ base }}/tree/{{ crate::web::fmt::urlpath(view.refname) }}/{{ crate::web::fmt::urlpath(entry.path) }}
/login?next={{ crate::web::fmt::urlencode(ctx.path) }}
```

- askama passes struct fields by reference (`&c.committer_time`) but method-call results and literals by value. Numeric helpers therefore take `impl fmt::Num<i64>` / `impl fmt::Num<usize>`, which accepts both. Follow that pattern for new numeric helpers. String helpers take `&str`.
- Methods on view structs are fine too (`r.base()`, `r.pull_url(pr.number)`, `f.display_path()`, `l.class()`).
- Use `urlpath` for ref names and file paths in URL paths (keeps `/`, encodes spaces, `#` and so on). Use `urlencode` for query-string values.
- Only mark output `|safe` when it's HTML we generate: `icon`, `avatar`, highlighted lines, and `markdown::render` output. Never put `|safe` on user-provided strings.

## Syntax that works here

- `{% if let Some(x) = opt %}…{% endif %}`, `{% else if %}`, `{% for (a, b) in pairs %}`, `loop.index`, `loop.last`.
- `{% match value %}{% when crate::web::code::BlobContent::Code with { lines } %}…{% endmatch %}`. Name enum variants by full path.
- Comparing an `Option<enum>`: `data.mergeability == Some(crate::git::Mergeability::Clean)`.
- Comparing a loop item against a field: `{% if *b == view.base %}`.

## Markup conventions

- There is no inline JavaScript (the CSP forbids it). Use the `data-*` hooks from `static/app.js`, which are listed in the root `CLAUDE.md`. Destructive forms get `data-confirm="…"`.
- Dropdowns are `<details class="dropdown">` with a `<summary>` and a `.dropdown-menu`, so they work without JS.
- Show empty states with `<div class="blankslate">` (an icon, an `<h3>` and a `<p>`).
- Show errors on re-rendered forms with `<div class="flash flash-error">`. For messages after a redirect, use `redirect_flash` in the handler instead.
- Forms POST to same-origin paths, and every page must still work with JS disabled.
