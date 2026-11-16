# Openhub

A minimal, self-hostable GitHub-style git server: push over SSH, browse code and history, and review pull requests. It is a single Rust binary backed by SQLite. User-facing documentation lives in `README.md`. How the system works (architecture, data model, access control, git transport, pull requests, web interface, security, operations) is documented in `docs/`, indexed at `docs/README.md`. This file covers working on the code.

**Scope is deliberate.** Openhub is git hosting with a basic issue tracker. Do not add CI, wikis, releases, forks or organisations unless asked, and keep issues simple (no milestones, assignees or cross-references) unless asked. Pushing is SSH-only by design (HTTP is read-only), so don't add HTTP push without discussing it first.

> The parent directory's `CLAUDE.md` is about Bun and JavaScript projects. Openhub has no JavaScript toolchain: `static/app.js` is hand-written and served as-is. That guidance only applies if a JS build step is ever introduced.

## Commands

```sh
cargo build                              # also compiles every template; template errors are compile errors
cargo run -- --data-dir ./data           # web on :3000, SSH on :2222 (./data is gitignored)
cargo test                               # unit tests (inline #[cfg(test)] modules)
cargo clippy --all-targets               # keep this warning-free
cargo fmt
scripts/smoke-test.sh [path/to/binary]   # end-to-end test against a throwaway instance
docker compose up -d                     # production-style run (data in a named volume)
scripts/deploy-server.sh user@host       # build on a Linux host over ssh and install/upgrade the systemd service
```

- The smoke test needs `git`, `ssh`, `ssh-keygen` and `curl`. It uses ports 38080 and 38022; override them with `HTTP_PORT` and `SSH_PORT`. It creates and deletes its own temp directory.
- When running a dev server for experiments, point `--data-dir` at a scratch directory. Don't reuse `./data`, which may hold someone's real instance.
- The `git` executable is a runtime dependency. It serves fetches and pushes (`upload-pack`, `receive-pack`) and builds archives.

## Toolchain and dependencies

- Edition 2024 with `rust-version = "1.88"`. Cargo's MSRV-aware resolver then picks dependency versions that build on 1.88. Several crates' newest releases need a newer Rust, so to upgrade, bump `rust-version`, the `rust:` image tag in `Dockerfile` and the README together.
- `Cargo.lock` is committed (this is a binary), and the Dockerfile builds with `--locked`.
- libgit2 and SQLite are compiled from bundled C sources, so a C compiler is required. CMake and OpenSSL are not. git2 is built without default features.
- Key crates: axum 0.8, askama 0.16, sqlx 0.8 (SQLite), git2 0.21, russh 0.63, syntect + two-face, comrak + ammonia, clap 4.

## Layout

| Path | What lives there |
| --- | --- |
| `src/main.rs` | Startup: parse config, open DB, run web and SSH servers |
| `src/config.rs` | All flags and `OPENHUB_*` env vars (clap). Clone URL and cookie helpers |
| `src/state.rs` | `AppState` and the permission model (`AppState::access`) |
| `src/db.rs` | Every SQL query and row struct |
| `migrations/` | sqlx migrations, embedded at compile time and applied at startup |
| `src/git.rs` | libgit2 access: ref resolution, trees, history, diffs, mergeability, merging |
| `src/sync.rs` | Bookkeeping after refs change: HEAD/default branch, open pull requests |
| `src/ssh.rs` | SSH server: key auth, command parsing, running git, post-push hints |
| `src/highlight.rs` | Syntax highlighting (syntect scopes mapped to `tok-*` CSS classes) |
| `src/markdown.rs` | Markdown rendering, sanitisation and relative-link rewriting |
| `src/validate.rs` | Usernames, repo names, emails, passwords, SSH public keys |
| `src/icons.rs` | Inline SVG icons (Lucide, ISC). Add an icon by adding a match arm |
| `src/web/mod.rs` | Router and helpers: `render`, `redirect`, `redirect_flash`, `blocking`, `safe_next` |
| `src/web/ctx.rs` | Per-request middleware: session user, flash, Origin check, error pages, security headers |
| `src/web/error.rs` | `AppError` and how it becomes a response |
| `src/web/fmt.rs` | Helpers called from templates (`ago`, `date`, `plural`, `avatar`, `icon`, `urlpath`, …) |
| `src/web/{account,home,repo,code,pulls,issues}.rs` | Page handlers |
| `src/web/git_http.rs` | Smart HTTP (clone/fetch only) |
| `templates/` | askama templates. See `templates/CLAUDE.md` |
| `static/` | `style.css`, `app.js`, `favicon.svg`. Compiled into the binary with `include_str!` |
| `docs/` | System documentation (start at `docs/README.md`) and the README screenshots in `docs/screenshots/` |
| `packaging/linux/` | systemd unit and `install.sh` (snapshot DB, swap binary, health check, roll back). See `docs/operations.md` |
| `scripts/` | `smoke-test.sh` (end-to-end test) and `deploy-server.sh` (build and install over ssh) |

## How things fit together

**Storage.** Bare repositories live at `<data>/repos/<id>.git` (`Config::repo_path`). They are keyed by database ID, never by name, so renames only touch the DB. Never build filesystem paths from user input.

**Permissions.** `AppState::access(repo, user)` returns `Access { read, write, admin }`:
- The owner and site admins (`users.is_admin`, the first account registered) get admin.
- Collaborators get write.
- Anyone gets read on a public repo.

Web handlers must load repositories through `web::repo::load_repo`, which returns 404 when the user can't read the repo. Then call `RepoCtx::require_write()` or `require_admin()` as needed. Private repos must look identical to missing ones on every channel: web 404, HTTP git 404, SSH "Repository not found."

**Request flow.** `ctx::middleware` runs on every request:
- Loads the session user and flash message into a `Ctx` extension.
- Rejects cross-origin POSTs, accepting the origin of `--public-url` or one matching the `Host` header.
- Adds the CSP and security headers.
- Turns any response carrying an `ErrorPage` extension into the full HTML error page.

Handlers take `Ctx` as an extractor and return `WebResult` (`Result<Response, AppError>`). `ctx.require_user()?` redirects anonymous users to sign in. Use `redirect_flash` to show a message after a POST.

**Blocking work.** git2 calls are synchronous. Run them inside `web::blocking(move || …)` (spawn_blocking). DB calls are async and happen outside it. The usual handler shape is: load the repo and check access (async), do the git work (blocking), render.

**Pull requests.**
- State is `open`, `closed` or `merged`.
- `head_sha` is the last known head.
- `base_sha` is the merge base: it is updated while the PR is open and frozen on close or merge, so the commits and diff can still be shown after the branch moves or is deleted.
- Each PR's head is pinned at `refs/pull/<n>/head`. `receive.hideRefs=refs/pull` (set in `git::init_bare`) stops clients pushing there.
- `sync::refresh_pulls` runs after every push and on every open-PR page view. It tracks new heads and marks PRs as merged when their head is already contained in the base (for example, merged on the command line).
- The `pr_events` table is the conversation timeline. `author_id` is NULL for events Openhub detected itself.
- Web merges (`git::merge`) are done in memory and update the base branch with a compare-and-swap (`reference_matching`), so a concurrent push makes the merge fail cleanly.

**Issues.**
- State is `open` or `closed`; `state_reason` records `completed` or `not_planned`.
- Issues and pull requests share one per-repository number space (`db::create_issue` takes the max over both tables).
- The `issue_events` table is the conversation timeline (`comment`, `closed`, `reopened`, `renamed`).
- Labels are per repository (`labels`, `issue_labels`) and created on demand by `db::ensure_label`.
- Author or write access may close, reopen and edit; any signed-in reader may comment. See `docs/issues.md`.

**After a push** (SSH `receive-pack` exits 0): `sync::after_push` updates `pushed_at`, points HEAD at an existing branch (and updates `default_branch` if it had to change), then refreshes PRs. `ssh.rs` then prints the "Create a pull request" / "View pull request" hints.

## Security rules

- askama escapes everything by default. Use `|safe` only on HTML we generate: `fmt::icon`, `fmt::avatar`, `highlight::*` output and `markdown::render` output.
- Any user-supplied Markdown (READMEs, PR bodies, comments) must go through `markdown::render`, which sanitises with ammonia.
- Pages send `script-src 'self'`, so there can be no inline `<script>`, `onclick=` or `javascript:` URLs. Behaviour goes in `static/app.js` and is attached through `data-*` attributes.
- Raw file responses (`code::raw`) keep their sandboxing CSP and serve non-image text as `text/plain`.
- SSH runs only `git-upload-pack` and `git-receive-pack`, after `parse_command` and an access check. Never pass client-provided strings to a shell.
- Passwords use Argon2id. Sessions store only the SHA-256 of the token.

## Gotchas

- **Migrations are append-only.** sqlx stores a checksum of every applied migration and refuses to start if one changes. Existing installs (including any local `./data`) have already applied `0001_initial.sql`, so add `0002_*.sql` instead of editing it.
- **Column lists are shared constants.** Queries use runtime `sqlx::query_as` with `FromRow`, so no `DATABASE_URL` is needed at build time. When adding a column, also update `USER_COLS`, `REPO_SELECT` or `PULL_SELECT` in `db.rs` so every query returns it.
- **git2 0.21** returns `Result<Option<&str>>` (or `Result<&str>`) from many name and message accessors, and reference iterators yield `Result`s.
- **Ref + path URLs** (`/tree/{*rest}`, `/blob/…`, `/commits/…`, `/archive/…`) are split by `git::resolve`. It tries `refs/heads/` then `refs/tags/` for growing prefixes, then a commit SHA. Build these URLs with `fmt::urlpath`, never raw string concatenation, because branch and file names can contain spaces and `#`.
- **russh channel handling.** `ssh.rs` deliberately drops the `Channel` object in `channel_open_session` and relies on the `data` callbacks. Keeping it unread lets russh's per-channel buffer fill and deadlock the session. Client data is forwarded to git through an unbounded queue.
- **Static assets are embedded at compile time.** Changes to `static/` need a rebuild. Asset URLs carry a content hash (`assets::version()`), so browsers pick up changes without stale caches.
- **Smart HTTP request size.** `git-upload-pack` requests can be large, so that route has its own 64 MB body limit. Other routes use axum's 2 MB default.

## Frontend conventions

- **Colours are tokens.** Colours are CSS custom properties on `:root`, redefined in the `prefers-color-scheme: dark` block. Add new colours as tokens in both places rather than raw hex inside components. Syntax colours are the `--tok-*` tokens. Two kinds of raw colour are intentional exceptions: `#fff` text on solid emphasis fills (primary/success/danger buttons, state pills, badges, avatars), and translucent white on the site header, which is dark in both themes.
- **Reuse the existing classes before adding new ones:**
  - Buttons: `btn`, plus `btn-primary`, `btn-success`, `btn-danger`, `btn-ghost`, `btn-sm`, `btn-icon`.
  - Containers: `box`, `box-header`, `box-body`, `box-row`, `box-footer`.
  - Small UI: `label`, `counter`, `branch-label`, `state-{open,closed,merged}`, `flash-{success,error,info}`, `blankslate`, `banner`.
  - Layout pieces: `tabs` / `tab`, `dropdown` (a `<details>` element), `field`, `field-label`, `field-hint`, `input`, `form-actions`.
- **JS hooks (`static/app.js`):**
  - `data-copy="#input"` and `data-copy-text="…"` copy text to the clipboard.
  - `data-confirm="…"` on a `<form>` asks for confirmation before submitting.
  - `data-toggle="element-id"` shows or hides an element.
  - `data-collapse` collapses a diff file.
  - `data-filter` and `data-filter-item` filter a dropdown list.
  - `data-autosubmit` on a `<select>` submits its form on change.
  - `data-clone-tab` and `data-clone-pane` switch the clone URL tabs.
  - Every page must still work without JS.
- **Check visual changes** in light mode, dark mode and at about 390px width. The README screenshots in `docs/screenshots/` come from a dev instance at 1280px: the repo page (light), a PR conversation (light) and a PR "Files changed" tab (dark). Retake them when the UI changes noticeably.

## Testing expectations

- Unit tests sit next to the code they test. Pure logic such as parsing, validation, highlighting, markdown and formatting should have one.
- `scripts/smoke-test.sh` is the end-to-end contract. When adding or changing a user-facing flow (auth, keys, push, PRs, permissions, HTTP git), extend it and run it.
- Before finishing a change, run `cargo fmt`, `cargo clippy --all-targets`, `cargo test` and the smoke test.

## Documentation

- Read the relevant page in `docs/` before changing a subsystem. Each page names its source files and ends with a "When you change this" checklist.
- When a change alters behaviour that `docs/` describes (routes, permissions, schema, limits, PR tracking, security gaps), update that page in the same change. If the docs and the code disagree, the code is right and the docs need fixing.
- Reference code by file and function name in docs, not by line number.
