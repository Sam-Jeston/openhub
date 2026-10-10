CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    email         TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    display_name  TEXT    NOT NULL DEFAULT '',
    bio           TEXT    NOT NULL DEFAULT '',
    password_hash TEXT    NOT NULL,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);

CREATE TABLE sessions (
    token_hash TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_user ON sessions(user_id);

CREATE TABLE ssh_keys (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title        TEXT    NOT NULL,
    fingerprint  TEXT    NOT NULL UNIQUE,
    public_key   TEXT    NOT NULL,
    key_type     TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);
CREATE INDEX ssh_keys_user ON ssh_keys(user_id);

-- Bare repositories live on disk at <data>/repos/<id>.git, so renames only touch this table.
CREATE TABLE repositories (
    id             INTEGER PRIMARY KEY,
    owner_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name           TEXT    NOT NULL COLLATE NOCASE,
    description    TEXT    NOT NULL DEFAULT '',
    is_private     INTEGER NOT NULL DEFAULT 0,
    default_branch TEXT    NOT NULL DEFAULT 'main',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    pushed_at      INTEGER,
    UNIQUE (owner_id, name)
);

CREATE TABLE collaborators (
    repo_id    INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, user_id)
);
CREATE INDEX collaborators_user ON collaborators(user_id);

CREATE TABLE pull_requests (
    id               INTEGER PRIMARY KEY,
    repo_id          INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number           INTEGER NOT NULL,
    title            TEXT    NOT NULL,
    body             TEXT    NOT NULL DEFAULT '',
    author_id        INTEGER NOT NULL REFERENCES users(id),
    base_branch      TEXT    NOT NULL,
    head_branch      TEXT    NOT NULL,
    -- 'open' | 'closed' | 'merged'
    state            TEXT    NOT NULL DEFAULT 'open',
    -- Last known tip of the head branch (also kept at refs/pull/<number>/head).
    head_sha         TEXT,
    -- Merge base of head and base. Tracked while open, frozen once closed or merged.
    base_sha         TEXT,
    merge_commit_sha TEXT,
    merged_by        INTEGER REFERENCES users(id),
    merged_at        INTEGER,
    closed_at        INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (repo_id, number)
);
CREATE INDEX pull_requests_repo_state ON pull_requests(repo_id, state);

-- Conversation timeline: comments plus state changes.
CREATE TABLE pr_events (
    id         INTEGER PRIMARY KEY,
    pr_id      INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    -- NULL when Openhub detected the change itself (e.g. merged by a plain push).
    author_id  INTEGER REFERENCES users(id),
    -- 'comment' | 'closed' | 'reopened' | 'merged' | 'branch_deleted'
    kind       TEXT    NOT NULL,
    body       TEXT    NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE INDEX pr_events_pr ON pr_events(pr_id);
