# Architecture

Openhub is one Rust process. It serves the web interface and git over HTTP on one port, and git over SSH on a second port. State is kept in two places:

- A SQLite database, for accounts, keys, repository metadata and pull requests.
- A directory of bare git repositories, for all code and history.

There are no background workers, queues or caches. Everything happens in response to a web request or an SSH session.

## Components

```
   browser      git over HTTP             git over SSH
      │           │                        │
      ▼           ▼                        ▼
  ┌───────────────────────────┐   ┌───────────────────────────┐
  │ Web server (axum)         │   │ SSH server (russh)        │
  │ src/web/                  │   │ src/ssh.rs                │
  └─────────────┬─────────────┘   └─────────────┬─────────────┘
                │                               │
                └───────────────┬───────────────┘
                                ▼
  ┌───────────────────────────────────────────────────────────┐
  │ Shared core                                               │
  │   AppState (src/state.rs): Config, SQLite pool, access()  │
  │   libgit2, in process (src/git.rs, src/sync.rs)           │
  │   git child processes: upload-pack, receive-pack, archive │
  └─────────────┬───────────────────────────────┬─────────────┘
                ▼                               ▼
         <data>/openhub.db              <data>/repos/<id>.git
         SQLite database                bare repositories
```

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | Startup and shutdown |
| `src/config.rs` | `Config`: every command-line flag and `OPENHUB_*` variable, plus path and clone-URL helpers |
| `src/state.rs` | `AppState`, shared by both servers, and the permission model `AppState::access` |
| `src/db.rs` | The connection pool, migrations, row types and every SQL query |
| `src/git.rs` | Every libgit2 operation: refs, trees, blobs, history, diffs, mergeability, merging |
| `src/sync.rs` | Keeps the database in step with refs after they change |
| `src/ssh.rs` | The SSH server: key authentication, command parsing, running git, push hints |
| `src/web/` | The HTTP router, middleware and page handlers, plus smart HTTP and static assets |
| `src/highlight.rs` | Syntax highlighting to `tok-*` CSS classes |
| `src/markdown.rs` | Markdown to sanitised HTML, with relative-link rewriting |
| `src/validate.rs` | Rules for usernames, repository names, emails, passwords and SSH public keys |
| `src/icons.rs` | Inline SVG icons |
| `templates/` | askama HTML templates, compiled into the binary |
| `static/` | `style.css`, `app.js` and `favicon.svg`, embedded into the binary with `include_str!` |
| `migrations/` | SQL migrations, embedded and applied at startup |

## Startup

`main` in `src/main.rs`:

1. Starts logging. The level comes from `RUST_LOG` and defaults to `openhub=info`.
2. Parses `Config` from flags and environment variables, then calls `Config::normalize`, which strips trailing slashes from the public URL.
3. Creates `<data>/repos` if it is missing.
4. Runs `git --version`. If `git` can't be run, startup fails.
5. Opens the database with `db::connect`:
   - Creates the database file if needed.
   - Turns on WAL mode, `synchronous=NORMAL` and foreign keys, with a 5-second busy timeout and a pool of up to 8 connections.
   - Applies any pending migrations.
6. Builds `AppState` and runs `web::serve` and `ssh::serve` together with `tokio::select!`.

If either server returns an error, the process exits. Ctrl-C shuts it down. On its first start, the SSH server generates `<data>/ssh_host_ed25519_key` (see [Git transport](git-transport.md#host-key)).

## Data directory

```
<data-dir>/                  default ./data, or /data in Docker
├── openhub.db               SQLite database (plus openhub.db-wal and openhub.db-shm while running)
├── ssh_host_ed25519_key     SSH host private key (mode 0600)
└── repos/
    ├── 1.git/               bare repository for repositories.id = 1
    └── 2.git/
```

A whole installation is this directory plus the binary. Back it up as a unit (see [Operations](operations.md#backups)).

## libgit2 and the git executable

Openhub uses two git implementations, each for the jobs it handles best:

| Work | Implementation | Where |
| --- | --- | --- |
| Serving fetches and clones (SSH and HTTP) | `git upload-pack`, run as a child process | `src/ssh.rs`, `src/web/git_http.rs` |
| Receiving pushes (SSH only) | `git receive-pack`, run as a child process | `src/ssh.rs` |
| ZIP and tar.gz downloads | `git archive`, run as a child process | `code::archive` in `src/web/code.rs` |
| Browsing trees, files, history, branches and tags | libgit2, in process | `src/git.rs` |
| Diffs, mergeability checks and web merges | libgit2, in process | `src/git.rs` |
| Creating repositories and initial commits | libgit2, in process | `git::init_bare`, `git::initial_commit` |
| Updating refs (`HEAD`, `refs/pull/*`, deleting branches) | libgit2, in process | `src/git.rs`, `src/sync.rs` |

The wire protocol is left to the installed `git`, so protocol v2, shallow clones and future protocol changes come from the system package. Everything Openhub needs to inspect or compute is done with libgit2, which avoids parsing command output.

libgit2 is compiled into the binary from bundled C sources (`git2` crate, default features off, so no OpenSSL or libssh2). The `git` executable is a runtime dependency, configurable with `--git-bin`.

## Concurrency model

- **Runtime.** Openhub runs on a multi-threaded tokio runtime (`#[tokio::main]`).
- **libgit2 calls are synchronous.** Web handlers run them inside `web::blocking`, which wraps `spawn_blocking`, so they never stall the async workers. A `git2::Repository` is opened per request and never shared between threads.
- **Database access is async** through the sqlx pool. Handlers normally:
  1. Load the repository and check access (async).
  2. Do the git work (blocking).
  3. Render the page.
- **Each SSH connection** is its own russh handler (`SshSession`). Each git child process gets tasks that copy data between the SSH channel and the process's stdin, stdout and stderr.
- **Child processes** are started with `tokio::process::Command` and `kill_on_drop(true)`, so a dropped connection kills its git process.
- **Concurrent ref updates** are serialised by git's own ref locking. Web merges use a compare-and-swap ref update, so a merge racing a push fails cleanly instead of overwriting it (see [Pull requests](pull-requests.md#merging-in-the-web-interface)).

## How the main flows work

### Viewing a file in the browser

1. The axum router matches `/{owner}/{repo}/blob/{*rest}` and calls `code::blob`.
2. `ctx::middleware` has already loaded the signed-in user from the session cookie into `Ctx`.
3. `repo::load_repo` looks up the repository by owner and name and calls `AppState::access`. If the user can't read it, the handler returns 404.
4. Inside `web::blocking`:
   - `git::resolve` splits `rest` into a ref and a path.
   - `git::read_blob` reads the file.
   - `highlight::highlight_lines` or `markdown::render` turns it into HTML.
5. The askama template renders. The middleware adds the security headers.

### Pushing over SSH

1. russh authenticates the client key against `ssh_keys.fingerprint`.
2. The client runs `git-receive-pack '/alice/project.git'`. `parse_command` extracts the service, owner and name.
3. Openhub looks up the repository and checks write access.
4. It records the branch tips, then spawns `git receive-pack <data>/repos/<id>.git` and relays data between the client and the process.
5. When the process exits successfully:
   - `sync::after_push` updates `pushed_at`, fixes `HEAD` and refreshes open pull requests.
   - Push hints are printed as `remote:` lines.

### Merging a pull request in the browser

1. `pulls::merge` checks write access and re-checks mergeability.
2. `git::merge` builds the new commits in memory with libgit2.
3. It moves the base branch with `reference_matching`, which only succeeds if the branch still points where it did when the merge started.
4. The database records the merge, and `sync::refresh_pulls` re-evaluates the repository's other open pull requests.

## Design decisions

| Decision | Why |
| --- | --- |
| A single binary with SQLite | Openhub targets small, self-hosted installations. One process and one data directory are easy to run, back up and move. It isn't designed for horizontal scaling. |
| A built-in SSH server | No system `sshd`, `authorized_keys` management or `git` Unix user is needed. Keys are managed in the web interface. |
| Pushes over SSH only | Key-based auth is the one credential for writing. HTTP is read-only, so passwords can never push. |
| Bare repositories named by database ID | Renaming a repository or user never touches the filesystem, and no filesystem path is ever built from user input. |
| `refs/pull/<n>/head` | A pull request's commits stay reachable, and its diff can still be shown, after its branch is deleted. |
| No background jobs | State is reconciled when it is next needed: after a push, and on pull request page views. There's nothing to schedule or monitor. |
| Server-rendered HTML, with optional JavaScript | Every page works without JavaScript. `static/app.js` only adds conveniences such as copy buttons and line highlighting. |

## Scope

Openhub is git hosting only: repositories, code browsing, history, SSH keys, collaborators and pull requests. Issues, CI, wikis, releases, forks, organisations, webhooks, line-level review comments and HTTP push are out of scope. Discuss any of these before adding them.

## When you change this

- **New module:** add it to the layout table in [`CLAUDE.md`](../CLAUDE.md) and to the components table above.
- **New runtime dependency** (another external program, say): add it to `check_git`-style startup checks, the `Dockerfile` runtime stage and the README requirements.
