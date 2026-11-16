# Web interface

The web interface is server-rendered HTML: axum handlers fill askama templates, and a small optional script adds conveniences. This page covers:

- the routes;
- what happens to every request;
- how handlers are structured;
- how code, history and diffs are turned into pages, including syntax highlighting and Markdown.

**Source files:**
- `src/web/mod.rs`: router and helpers.
- `src/web/ctx.rs`: middleware.
- `src/web/error.rs`: error handling.
- `src/web/{account,home,repo,code,pulls,issues}.rs`: page handlers.
- `src/web/fmt.rs`: template helpers.
- `src/highlight.rs`, `src/markdown.rs`: content rendering.
- `templates/`, `static/`: templates and static assets.

## Routes

All routes are defined in `web::router`. "Read", "write" and "admin" refer to the [permission model](accounts-and-access.md#permission-model). "User" means a signed-in user is required; anonymous visitors are redirected to `/login?next=…`.

### Site and accounts

| Method | Path | Handler | Access |
| --- | --- | --- | --- |
| GET | `/` | `home::index` | Anyone. Landing page when signed out, dashboard when signed in |
| GET | `/explore` | `home::explore` | Anyone. Up to 200 public repositories, most recently pushed first |
| GET, POST | `/login` | `account::login_form`, `account::login` | Anyone |
| GET, POST | `/register` | `account::register_form`, `account::register` | Anyone, while registration is allowed |
| POST | `/logout` | `account::logout` | User |
| GET, POST | `/new` | `repo::new_form`, `repo::create` | User |
| GET | `/settings` | `account::settings_redirect` | Redirects to `/settings/profile` |
| GET, POST | `/settings/profile` | `account::profile_form`, `account::update_profile` | User |
| POST | `/settings/password` | `account::change_password` | User |
| GET, POST | `/settings/keys` | `account::keys`, `account::add_key` | User |
| POST | `/settings/keys/{id}/delete` | `account::delete_key` | User (own keys only) |
| GET | `/static/style.css`, `/static/app.js`, `/favicon.svg` | `assets::*` | Anyone |
| GET | `/{owner}` | `home::profile` | Anyone. Lists the repositories the viewer may see |

### Repository pages

| Method | Path | Handler | Access |
| --- | --- | --- | --- |
| GET | `/{owner}/{repo}` | `code::repo_home` | Read. `/{owner}/{repo}.git` redirects here |
| GET | `/{owner}/{repo}/tree/{ref}/{path…}` | `code::tree` | Read. Redirects to `blob` if the path is a file |
| GET | `/{owner}/{repo}/blob/{ref}/{path…}` | `code::blob` | Read. `?plain=1` shows Markdown as source |
| GET | `/{owner}/{repo}/raw/{ref}/{path…}` | `code::raw` | Read |
| GET | `/{owner}/{repo}/archive/{ref}.zip` or `.tar.gz` | `code::archive` | Read |
| GET | `/{owner}/{repo}/commits` | `code::commits_default` | Read. Redirects to the default branch |
| GET | `/{owner}/{repo}/commits/{ref}/{path…}` | `code::commits` | Read. `?page=N` |
| GET | `/{owner}/{repo}/commit/{sha}` | `code::commit` | Read. 4–40 hex characters |
| GET | `/{owner}/{repo}/branches` | `repo::branches` | Read |
| POST | `/{owner}/{repo}/branches/delete` | `repo::delete_branch` | Write |
| GET | `/{owner}/{repo}/tags` | `repo::tags` | Read |
| GET | `/{owner}/{repo}/compare` and `/compare/{base}...{head}` | `pulls::compare_default`, `pulls::compare` | Read |
| GET, POST | `/{owner}/{repo}/pulls` | `pulls::list`, `pulls::create` | Read; creating needs a user. `?state=closed` lists closed and merged |
| GET | `/{owner}/{repo}/pull/{n}` and `/commits`, `/files` | `pulls::conversation`, `pulls::commits`, `pulls::files` | Read |
| POST | `/{owner}/{repo}/pull/{n}/comment` | `pulls::comment` | User plus read |
| POST | `/{owner}/{repo}/pull/{n}/state` | `pulls::set_state` | Author or write |
| POST | `/{owner}/{repo}/pull/{n}/edit` | `pulls::edit` | Author or write |
| POST | `/{owner}/{repo}/pull/{n}/merge` | `pulls::merge` | Write |
| GET, POST | `/{owner}/{repo}/issues` and `/issues/new` | `issues::list`, `issues::create`, `issues::new_form` | Read; creating needs a user. `?state=closed` and `?label=…` filter |
| GET | `/{owner}/{repo}/issues/{n}` | `issues::show` | Read |
| POST | `/{owner}/{repo}/issues/{n}/comment` | `issues::comment` | User plus read. `close=1` closes with the comment |
| POST | `/{owner}/{repo}/issues/{n}/state` | `issues::set_state` | Author or write |
| POST | `/{owner}/{repo}/issues/{n}/edit` | `issues::edit` | Author or write |
| GET, POST | `/{owner}/{repo}/settings` | `repo::settings`, `repo::update_settings` | Admin |
| POST | `/{owner}/{repo}/settings/collaborators` and `/collaborators/remove` | `repo::add_collaborator`, `repo::remove_collaborator` | Admin |
| POST | `/{owner}/{repo}/settings/delete` | `repo::delete` | Admin |

### Git over HTTP

`/{owner}/{repo}/info/refs`, `/git-upload-pack` and `/git-receive-pack` are described in [Git transport](git-transport.md#smart-http-read-only).

Any other path returns the 404 page (`AppError::NotFound`, via the router's fallback).

## Request pipeline

```
request ─▶ TraceLayer ─▶ ctx::middleware ─▶ route handler ─▶ ctx::middleware (response part) ─▶ client
```

`ctx::middleware` runs for every request. Before the handler, it does the following:

1. **Static assets.** Requests under `/static/` and for `/favicon.svg` pass straight through.
2. **Cross-origin check.** Every request except GET, HEAD and OPTIONS, other than git HTTP paths, must have either:
   - no `Origin` header; or
   - an `Origin` equal to the origin of `--public-url`; or
   - an `Origin` whose host matches the `Host` header.

   Anything else, including `Origin: null`, gets `403 Cross-origin request rejected`. This backs up `SameSite=Lax` cookies against CSRF.
3. **Session.** Hashes the session cookie and loads the user, except on git HTTP paths.
4. **Flash.** Decodes the flash cookie.
5. **Context.** Puts `Ctx { user, flash, path, session_hash }` into the request extensions. Handlers receive it with the `Ctx` extractor.

After the handler, it does the following:

1. **Error pages.** If the response carries an `ErrorPage` extension (see below), the body is replaced with the rendered `error.html`, keeping the status.
2. **Flash clearing.** Clears the flash cookie if one was shown and the response doesn't set a new one.
3. **HTML headers.** On HTML responses, adds `Content-Security-Policy: default-src 'self'; img-src 'self' data: https:; style-src 'self' 'unsafe-inline'; script-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'` and `Cache-Control: no-store`.
4. **Global headers.** On every response, adds `X-Content-Type-Options: nosniff` and `Referrer-Policy: strict-origin-when-cross-origin`.

## Handler shape

```rust
pub async fn branches(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;      // async: DB lookup and access check (404 if unreadable)
    let path = r.path.clone();
    let default = r.repo.default_branch.clone();
    let list = blocking(move || Ok(git::branches(&git::open(&path)?, &default)?)).await?;  // libgit2 work off the async runtime
    /* … more async DB work … */
    render(&BranchesPage { ctx, r, tab: "branches", rows })     // askama template → HTML response
}
```

- `WebResult` is `Result<Response, AppError>`.
- `RepoCtx` (from `load_repo`) carries the repository row, the viewer's `Access`, the path on disk, the open pull request and issue counts and the clone URLs. Every repository template uses it.
- **Mutations** are `POST` forms that end with `redirect` or `redirect_flash` (303 See Other, the post/redirect/get pattern). When validation fails, the handler re-renders the form with its values and an error message instead.

### Errors

| `AppError` | Status | Shown as |
| --- | --- | --- |
| `NotFound` | 404 | "Page not found". Also used for repositories the viewer can't read |
| `Forbidden(msg)` | 403 | "Access denied" with the message |
| `BadRequest(msg)` | 400 | "Bad request" with the message |
| `LoginRequired(path)` | 303 | Redirect to `/login?next=<path>` |
| `Internal(err)` | 500 | "Something went wrong". The error is logged with its full chain |

Any error type that converts to `anyhow::Error`, such as `sqlx::Error` or `git2::Error`, becomes `Internal` through `?`.

## Refs and paths in URLs

URLs such as `/tree/feature/login/src/main.rs` don't mark where the ref ends and the path begins. Branch names can contain `/`. `git::resolve` splits the URL tail as follows:

1. Split the tail on `/`. For prefixes of growing length (`feature`, then `feature/login`, then …):
   - try `refs/heads/<prefix>`, then `refs/tags/<prefix>`;
   - stop at the first one that exists and points to a commit.
2. If none matches, and the first segment is 4–40 hex characters that resolve to a commit, treat it as a commit SHA.
3. Otherwise, return 404.

Consequences:

- **The shortest matching name wins.** With branches `feature` and `feature/login`, `/tree/feature/login` means the `login` path on branch `feature`. git normally refuses to create both of these branches together, so this is rarely a problem.
- **A branch shadows a tag** with the same name.
- **Building URLs.** Always build ref and path URLs with `fmt::urlpath` (or `{{ crate::web::fmt::urlpath(…) }}` in templates). It percent-encodes spaces, `#`, `?` and non-ASCII characters but keeps `/`. Use `fmt::urlencode` for query-string values.

## Code browsing

### Repository home (`code::repo_home`)

- **Empty repositories** (no branches) render `repo/empty.html`, with push instructions.
- **Ref shown.** The page shows the default branch, or the first branch alphabetically if the default doesn't exist.
- **File list.** The root listing shows each entry with the last commit that changed it (see Tree below).
- **README.** The page renders the first file in the directory that matches, case-insensitively, `readme.md`, `readme.markdown`, `readme`, `readme.txt` or `readme.rst`, if it is under 1 MB and not binary. Markdown is rendered; other formats are shown as escaped text.
- **Sidebar counts.**
  - Commits: `git::commit_count`, a full walk of history.
  - Branches and tags.
  - Open pull requests.
- **"Recent pushes" banner.** Offers **Compare & pull request** for the non-default branch with the newest commit, if that commit is less than an hour old, the branch has no open pull request, and the viewer has write access.

### Tree (`code::tree`, `git::list_tree`)

- Directories are listed first, then files, each group sorted case-insensitively.
- Entries are marked as directories, files, symlinks (mode `120000`) or submodules (commit entries).
- **Last commit per entry** (`git::last_commits`): Openhub walks history newest first from the ref. An entry is attributed to the first commit whose entry ID differs from all of its parents'. The walk stops when every entry is found, or after 5,000 commits. Entries not found by then show no commit.
- A parent-directory link and breadcrumbs are shown below the root. The README of the current directory is rendered too.

### File view (`code::blob`)

The content is chosen in this order:

| Condition | Content |
| --- | --- |
| Extension is `.png`, `.jpg`, `.jpeg`, `.gif`, `.webp`, `.bmp`, `.ico` or `.svg` | `<img>` pointing at the raw URL |
| libgit2 says the blob is binary | "Binary file not shown", with a raw link |
| Larger than 1 MB (`MAX_DISPLAY_BYTES`) | "This file is too large to display", with a raw link |
| `.md` or `.markdown`, without `?plain=1` | Rendered Markdown, with relative links rewritten |
| Anything else | Highlighted source with line numbers |

- **Line numbers** are links to `#L<n>`.
- **Line highlighting.** `static/app.js` highlights `#L10` or `#L10-L20`. Shift-click extends the selection.
- **Header.** Shows the size, the line count, the detected language and the last commit that touched the file.

### Raw files (`code::raw`)

The headers depend on the file:

| File | `Content-Type` | `Content-Disposition` |
| --- | --- | --- |
| Image (by extension, via `mime_guess`) | The image's MIME type, including `image/svg+xml` | `inline` |
| Other text | `text/plain; charset=utf-8`, never `text/html` | `inline` |
| Binary | `application/octet-stream` | `attachment` |

Every raw response also sends `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; sandbox`, so even an SVG opened directly can't run script on the Openhub origin.

### History (`code::commits`, `git::log`)

- **Pagination:** 30 commits per page, sorted by commit time. `?page=N` moves through pages, with Newer and Older links.
- **Path filtering.** A path filter keeps only commits whose tree entry at that path differs from every parent's. This resembles `git log -- <path>` with history simplification, but doesn't follow renames.
- **Grouping:** commits are grouped by committer date (UTC).

### Commit page (`code::commit`)

- Shows the commit's metadata and its diff against the first parent. Merge commits therefore show what the merge brought into the first parent's line.
- A root commit is diffed against an empty tree.
- Parent links go to their commit pages.

### Branches and tags

- **Branches** (`git::branches`):
  - The default branch comes first, then branches ordered by newest tip commit.
  - Each branch shows ahead and behind counts against the default branch (`graph_ahead_behind`), its open pull request, and (for writers) a delete button. The default branch can't be deleted.
- **Tags** (`git::tags`):
  - Sorted in natural order, newest version first, so `v1.10` comes before `v1.9`.
  - Each shows the tagged commit, plus the message for annotated tags.

## Diffs

`git::diff_trees` turns a libgit2 tree-to-tree diff into a `DiffSummary`. The commit page, compare page and pull request files tab all use it, and render it with `templates/partials/diff.html`.

- **Context:** three lines. Submodule changes are ignored. Renames are detected (`find_similar`) and shown as `old → new`.
- **Per-file status:** `added`, `deleted`, `renamed`, `copied` or `modified`, with addition and deletion counts and a five-block summary bar (`FileDiff::stat_blocks`).
- **Size caps:**
  - A file with more than 3,000 changed lines isn't rendered.
  - After 20,000 rendered lines in total, later files aren't rendered either.
  - Files skipped by these caps show a "too large" placeholder but still count toward the totals.
- **Binary files** show a placeholder.
- **Highlighting:** each hunk's lines are highlighted as one sequence, using the file's language.

## Syntax highlighting

`src/highlight.rs` uses syntect's parser with the extended syntax set from `two-face`. It doesn't use a syntect theme. Instead:

- **Language detection** (`find_syntax`): by file extension, then the whole file name (for files such as `Makefile` or `Dockerfile`), then the first line (shebangs). "Plain Text" counts as unknown.
- **Scope mapping.** Scope stacks are mapped to a small set of classes, such as `tok-k` for keywords, `tok-s` for strings and `tok-c` for comments (the table is `SCOPE_CLASSES`). Colours are the `--tok-*` CSS tokens in `static/style.css`, so highlighting follows the light and dark themes.
- **Self-contained lines.** Every line is rendered on its own, with all spans closed at the end of the line. Code views and diffs can then put each line in its own table row. Parser state still carries across lines, so multi-line comments and strings are coloured correctly.
- **Limits:**
  - Input over 512 KB (`MAX_HIGHLIGHT_BYTES`) is escaped but not highlighted.
  - If the parser fails, the rest of the file falls back to escaped text.
- **Output safety.** All text is HTML-escaped. Only `<span class="tok-…">` tags are produced.

## Markdown

`markdown::render(source, link_base)` renders untrusted Markdown in two stages:

1. **comrak** renders GitHub-flavoured Markdown: tables, strikethrough, autolinks, task lists and footnotes. Raw HTML is allowed through at this stage. Fenced code blocks with a language are highlighted with `highlight::highlight_block`.
2. **ammonia** sanitises the result. On top of its defaults, it allows:
   - `<input type checked disabled>`, for task lists;
   - `width`, `height` and `align` on images, and `align` everywhere;
   - the `tok-*` classes on `<span>`.

   It strips scripts, event handlers, `style` attributes and `javascript:` URLs. Links get `rel="noopener noreferrer nofollow"`.

When a `LinkBase` is given (READMEs and Markdown file views), relative URLs are rewritten:

| In the document `docs/guide.md` on `main` | Becomes |
| --- | --- |
| `[x](../src/lib.rs#L3)` | `/owner/repo/blob/main/src/lib.rs#L3` |
| `![logo](img/logo.png)` | `/owner/repo/raw/main/docs/img/logo.png` |
| `[root](/LICENSE)` | `/owner/repo/blob/main/LICENSE` |
| `https://…`, `mailto:…`, `#anchor`, `//host/…` | Unchanged |

Pull request descriptions and comments are rendered without a link base.

## Templates and assets

- **Templates.** Layouts, partials, helper calls and askama syntax are documented in [`templates/CLAUDE.md`](../templates/CLAUDE.md). Templates compile into the binary, so a template mistake is a build error.
- **Static assets.**
  - `static/style.css`, `static/app.js` and `static/favicon.svg` are embedded with `include_str!` and served with `Cache-Control: public, max-age=3600`.
  - Pages link them as `?v=<hash>`, where the hash comes from `assets::version()` (a hash of the CSS and JS), so browsers fetch new versions after an upgrade.
  - Editing these files needs a rebuild.
- **JavaScript** is optional. `app.js` attaches behaviour to `data-*` attributes; [`CLAUDE.md`](../CLAUDE.md) lists the hooks. The page CSP rules out inline scripts and `on*=` attributes.
- **Theming.** Light and dark themes follow `prefers-color-scheme`. All colours are CSS custom properties on `:root`.

## When you change this

- **New page:**
  - Add the route in `web::router` and the table above.
  - Load repositories through `load_repo`.
  - Use `blocking` for libgit2 calls.
  - Add a template that extends `base.html` or a layout.
  - Check the page in light and dark themes and at 390 px width.
- **New top-level path segment:** add it to `RESERVED` in `src/validate.rs`.
- **New limit or cap:** add it to this page and to [Security](security.md) if it protects against abuse.
- **New Markdown feature:** check that ammonia's allow-list still strips anything that could run script, and extend the unit tests in `src/markdown.rs`.
