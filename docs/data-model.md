# Data model

Openhub keeps two kinds of state:

| Store | Holds | Source of truth for |
| --- | --- | --- |
| SQLite (`<data>/openhub.db`) | Users, sessions, SSH keys, repository metadata, collaborators, pull requests, pull request events, issues, issue events, labels | Who exists, who may do what, and pull request and issue state |
| Bare repositories (`<data>/repos/<id>.git`) | Every commit, tree, blob, branch and tag, plus Openhub's `refs/pull/<n>/head` refs | All code and history |

The two are linked by the repository ID and by branch names and commit SHAs stored in the database. [Invariants](#invariants) lists the rules that keep them consistent.

**Source files:**
- `migrations/0001_initial.sql` holds the schema.
- `src/db.rs` holds the connection setup, row structs and every query.
- `src/git.rs` holds the repository operations.

## Database connection

`db::connect` opens the database with these settings:

| Setting | Value | Why |
| --- | --- | --- |
| `create_if_missing` | true | A fresh data directory needs no setup |
| `journal_mode` | WAL | Readers don't block the writer |
| `synchronous` | NORMAL | Safe with WAL and faster than FULL |
| `foreign_keys` | on | Cascading deletes and referential integrity |
| `busy_timeout` | 5 s | Waits out short write locks instead of failing |
| Pool size | 8 connections | Plenty for a single small server |

Pending migrations are applied before the servers start.

## Conventions

- **Timestamps** are Unix seconds (`INTEGER`) from `db::now()`. They are shown in UTC.
- **Booleans** are `INTEGER` 0 or 1, mapped to `bool` by sqlx.
- **Names compare case-insensitively.** `users.username`, `users.email` and `repositories.name` use `COLLATE NOCASE`. Lookups use the column's collation, so `/Alice/Project` finds `alice/project`. Uniqueness is case-insensitive too.
- **Row structs** (`User`, `Repo`, `SshKey`, `PullRequest`, `PrEvent`) derive `FromRow`. Queries use runtime `sqlx::query_as`, so building needs no database.
- **Shared column lists.** `USER_COLS`, `REPO_SELECT` and `PULL_SELECT` are constants in `db.rs`. Every query for that struct uses them, so each one returns the same columns.

## Tables

### `users`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `username` | TEXT, unique, NOCASE | Validated by `validate::username`. Appears in URLs (`/{owner}`) |
| `email` | TEXT, unique, NOCASE | Used for signing in and as the committer email for web merges |
| `display_name` | TEXT | Optional, up to 100 characters. `User::name()` falls back to the username |
| `bio` | TEXT | Optional, up to 500 characters |
| `password_hash` | TEXT | Argon2id in PHC string format (`$argon2id$v=19$…`) |
| `is_admin` | INTEGER | Site administrator. Set for the first account created |
| `created_at` | INTEGER | |

### `sessions`

| Column | Type | Notes |
| --- | --- | --- |
| `token_hash` | TEXT PK | Hex SHA-256 of the cookie token. The token itself is never stored |
| `user_id` | INTEGER, FK, cascade | |
| `created_at`, `expires_at` | INTEGER | `expires_at` is 30 days after sign-in. Expired rows are purged at each sign-in |

### `ssh_keys`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `user_id` | INTEGER, FK, cascade | |
| `title` | TEXT | Defaults to the key comment, or failing that the key type |
| `fingerprint` | TEXT, unique | `SHA256:…`. Unique across all users, so a key identifies exactly one account |
| `public_key` | TEXT | Normalised OpenSSH line with the comment removed |
| `key_type` | TEXT | For example `ssh-ed25519` |
| `created_at` | INTEGER | |
| `last_used_at` | INTEGER, nullable | Updated on every successful SSH authentication |

### `repositories`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER PK | Also names the directory on disk: `repos/<id>.git` |
| `owner_id` | INTEGER, FK, cascade | |
| `name` | TEXT, NOCASE | Validated by `validate::repo_name`. `UNIQUE (owner_id, name)` |
| `description` | TEXT | Up to 350 characters |
| `is_private` | INTEGER | |
| `default_branch` | TEXT | Defaults to `main`. Kept in step with the bare repository's `HEAD` |
| `created_at`, `updated_at` | INTEGER | `updated_at` changes when settings are saved, and with `pushed_at` |
| `pushed_at` | INTEGER, nullable | Set after each successful push and each web merge. Orders repository lists |

### `collaborators`

| Column | Type | Notes |
| --- | --- | --- |
| `repo_id` | INTEGER, FK, cascade | Primary key together with `user_id` |
| `user_id` | INTEGER, FK, cascade | |
| `created_at` | INTEGER | |

A row grants write access (push, merge, delete branches). There is only one collaborator level.

### `pull_requests`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER PK | Internal ID, used by `pr_events` |
| `repo_id` | INTEGER, FK, cascade | |
| `number` | INTEGER | Per-repository number shown in URLs. `MAX(number) + 1` inside a transaction. `UNIQUE (repo_id, number)` |
| `title`, `body` | TEXT | Title up to 256 characters. The body is Markdown |
| `author_id` | INTEGER, FK | No cascade (see [Deleting rows](#deleting-rows)) |
| `base_branch`, `head_branch` | TEXT | Branch names in the same repository. There are no forks |
| `state` | TEXT | `open`, `closed` or `merged` |
| `head_sha` | TEXT | Last known tip of the head branch, mirrored in `refs/pull/<number>/head` |
| `base_sha` | TEXT | Merge base of base and head. Tracked while open, frozen on close or merge |
| `merge_commit_sha` | TEXT, nullable | New tip of the base branch after a web merge. NULL when the merge was detected after a push |
| `merged_by` | INTEGER, FK, nullable | NULL when Openhub detected the merge on a page view |
| `merged_at`, `closed_at` | INTEGER, nullable | `closed_at` is also set on merge |
| `created_at`, `updated_at` | INTEGER | `updated_at` changes on every event, edit and head update |

[Pull requests](pull-requests.md) explains how these columns change over a pull request's life.

### `pr_events`

The conversation timeline: comments, plus records of state changes.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER PK | Used for `#event-<id>` anchors |
| `pr_id` | INTEGER, FK, cascade | |
| `author_id` | INTEGER, FK, nullable | NULL for events Openhub detected itself |
| `kind` | TEXT | `comment`, `closed`, `reopened`, `merged` or `branch_deleted` |
| `body` | TEXT | Markdown for `comment`, the branch name for `branch_deleted`, empty otherwise |
| `created_at` | INTEGER | |

The pull request's own description is stored in `pull_requests.body`, not as an event.

### `issues`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER PK | Internal ID, used by `issue_events` |
| `repo_id` | INTEGER, FK, cascade | |
| `number` | INTEGER | Shares one number space with `pull_requests.number`. `UNIQUE (repo_id, number)` |
| `title`, `body` | TEXT | Title up to 256 characters. The body is Markdown |
| `author_id` | INTEGER, FK | No cascade (see [Deleting rows](#deleting-rows)) |
| `state` | TEXT | `open` or `closed` |
| `close_reason` | TEXT, nullable | `completed` (resolved) or `not_planned` once closed, NULL while open |
| `closed_by` | INTEGER, FK, nullable | |
| `closed_at` | INTEGER, nullable | |
| `created_at`, `updated_at` | INTEGER | `updated_at` changes on every event and edit |

### `issue_events`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER PK | Used for `#event-<id>` anchors |
| `issue_id` | INTEGER, FK, cascade | |
| `author_id` | INTEGER, FK, nullable | |
| `kind` | TEXT | `comment`, `closed`, `reopened` or `renamed` |
| `body` | TEXT | Markdown for `comment`, the reason for `closed`, the new title for `renamed`, empty otherwise |
| `created_at` | INTEGER | |

### `labels` and `issue_labels`

| Column | Type | Notes |
| --- | --- | --- |
| `labels.id` | INTEGER PK | |
| `labels.repo_id` | INTEGER, FK, cascade | Labels are per repository |
| `labels.name` | TEXT | `COLLATE NOCASE`, `UNIQUE (repo_id, name)` |
| `labels.color` | TEXT | `#rrggbb`, chosen deterministically from the name by `db::label_color` |
| `issue_labels.issue_id`, `.label_id` | INTEGER, FK, cascade | Composite primary key |

See [Issues](issues.md) for the lifecycle and how labels are assigned.

## Relationships

```
users ─┬─< sessions               (cascade)
       ├─< ssh_keys               (cascade)
       ├─< repositories ─┬─< collaborators >─ users   (cascade both ways)
       │                 ├─< pull_requests ─< pr_events   (cascade)
       │                 ├─< issues ─< issue_events       (cascade)
       │                 ├─< issues ─< issue_labels >─ labels (cascade)
       │                 └─< labels                        (cascade)
       ├─< pull_requests.author_id, .merged_by          (no cascade)
       ├─< pr_events.author_id                          (no cascade)
       ├─< issues.author_id, .closed_by                 (no cascade)
       └─< issue_events.author_id                       (no cascade)
```

### Deleting rows

- **Deleting a repository** (`db::delete_repo`) cascades to its collaborators, pull requests and events, and its issues, issue events and labels. The handler then removes `repos/<id>.git`. If that fails, it logs a warning and the directory is left behind.
- **Deleting a user** isn't supported in the interface. Because foreign keys are enforced, `DELETE FROM users` fails for anyone who authored a pull request or event, or merged a pull request, in a repository they don't own. A future "delete user" feature must reassign or nullify those references first. For pull requests, `author_id` is `NOT NULL`, so that needs a placeholder user or a migration.

## Repositories on disk

Each repository is a bare git repository at `Config::repo_path(id)`, which is `<data>/repos/<id>.git`. File paths are never built from owner or repository names.

`git::init_bare` creates it when a repository is created:
- `HEAD` points to `refs/heads/main`.
- `receive.hideRefs` is set to `refs/pull`.
- If the creator ticked "Add a README", `git::initial_commit` writes a single commit containing `README.md`.

| Ref | Written by | Notes |
| --- | --- | --- |
| `HEAD` | `git init`, `git::ensure_head`, `git::set_head` | Symbolic ref to the default branch |
| `refs/heads/*` | Pushes, web merges, `git::delete_branch` | |
| `refs/tags/*` | Pushes | Tags can't be created in the web interface |
| `refs/pull/<n>/head` | `git::set_pull_ref` (create, refresh, merge, reopen) | Hidden from `receive-pack`, so pushes to it are rejected. Fetchable with `git fetch origin pull/<n>/head` |

The objects that web merges create (merge, squash and rebased commits) are written to the same object database. If the final ref update fails, they are left unreferenced, and a later `git gc` removes them. Openhub never runs `git gc` itself (see [Operations](operations.md#repository-maintenance)).

## Invariants

These must stay true. Code that changes either store must preserve them.

1. **Every `repositories` row has a directory.** `repo::create` inserts the row, then initialises `repos/<id>.git`. If initialisation fails, it deletes the row and the directory.
2. **IDs are never reused for a different directory.** SQLite `INTEGER PRIMARY KEY` can reuse the highest ID after a delete. Deleting a repository removes its directory, so a reused ID starts from an empty path. If directory removal failed, a later repository could pick up the stale directory. Remove leftovers by hand if the log warns about it.
3. **`default_branch` names `HEAD`'s target whenever that branch exists.**
   - The settings page moves `HEAD` with `git::set_head` before saving.
   - After a push, `sync::after_push` calls `git::ensure_head`. If the default branch is missing, `HEAD` moves to `main`, `master` or the first branch alphabetically, and the column is updated to match.
4. **Each pull request's `refs/pull/<n>/head` points at its `head_sha`.** Every code path that writes `head_sha` also calls `git::set_pull_ref`.
5. **At most one open pull request exists per (repository, base, head)**, enforced by checks in `pulls::create` and in reopening (`pulls::set_state`). There is no database constraint, so new code paths must check too.
6. **Closed and merged pull requests have a frozen `base_sha` and `head_sha`**, so their commits and diff render the same forever (see [Pull requests](pull-requests.md#which-commits-a-pull-request-shows)).

## Changing the schema

1. **Add a new file** such as `migrations/0002_add_repo_topics.sql`. Never edit an existing migration: sqlx stores a checksum of each applied migration, and the server refuses to start if one changes.
2. **Update the column list constant** (`USER_COLS`, `REPO_SELECT` or `PULL_SELECT`) and the row struct in `src/db.rs`.
3. **Add query functions** to `src/db.rs`. Keep SQL out of handlers.
4. **Prefer `ALTER TABLE … ADD COLUMN` with a default**, so existing rows stay valid. SQLite can't drop or alter constraints in place, so changing one needs the create-copy-rename pattern inside the migration.
5. **Run the smoke test against a fresh data directory, and against a copy of an existing one,** to check the migration applies to both.

Migrations only go forward. Rolling back means restoring a backup taken before the upgrade.

## Inspecting the database

```sh
sqlite3 -readonly data/openhub.db
sqlite> .tables
sqlite> SELECT id, username, is_admin FROM users;
sqlite> SELECT r.id, u.username || '/' || r.name, r.is_private FROM repositories r JOIN users u ON u.id = r.owner_id;
sqlite> SELECT number, state, base_branch, head_branch, substr(head_sha, 1, 7) FROM pull_requests WHERE repo_id = 1;
```

[Operations](operations.md#administration-without-a-ui) has recipes for changing data by hand.

## When you change this

- **New table or column:** update this page, the migration, `src/db.rs`, and any template that should show it.
- **New ref namespace:** add it to the refs table above. If clients must not push to it, add it to `receive.hideRefs` in `git::configure`. Existing repositories only got the setting when they were created, so they need a migration step at startup.
