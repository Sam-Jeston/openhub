-- Issues: a per-repository tracker modelled on GitHub's. Issue numbers share
-- the pull request number space so that "#12" is unambiguous in a repository.
CREATE TABLE issues (
    id            INTEGER PRIMARY KEY,
    repo_id       INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number        INTEGER NOT NULL,
    title         TEXT    NOT NULL,
    body          TEXT    NOT NULL DEFAULT '',
    author_id     INTEGER NOT NULL REFERENCES users(id),
    -- 'open' | 'closed'
    state         TEXT    NOT NULL DEFAULT 'open',
    -- 'completed' (resolved) | 'not_planned' once closed, NULL otherwise
    close_reason  TEXT,
    closed_by     INTEGER REFERENCES users(id),
    closed_at     INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (repo_id, number)
);
CREATE INDEX issues_repo_state ON issues(repo_id, state);

-- Conversation timeline: comments plus state changes.
CREATE TABLE issue_events (
    id         INTEGER PRIMARY KEY,
    issue_id   INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author_id  INTEGER REFERENCES users(id),
    -- 'comment' | 'closed' | 'reopened' | 'renamed'
    kind       TEXT    NOT NULL,
    body       TEXT    NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE INDEX issue_events_issue ON issue_events(issue_id);

-- Free-form labels, attached to issues. Labels are per repository and are
-- created on demand when an author types a new one on the issue form.
CREATE TABLE labels (
    id      INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name    TEXT    NOT NULL COLLATE NOCASE,
    -- Hex colour such as '#d73a4a'.
    color   TEXT    NOT NULL,
    UNIQUE (repo_id, name)
);

CREATE TABLE issue_labels (
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, label_id)
);
CREATE INDEX issue_labels_label ON issue_labels(label_id);
