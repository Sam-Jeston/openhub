//! SQLite persistence: connection setup, row types and queries.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};

pub type Db = SqlitePool;
type Result<T> = std::result::Result<T, sqlx::Error>;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn connect(path: &Path) -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

// ---------------------------------------------------------------------------
// Users & sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub display_name: String,
    pub bio: String,
    pub password_hash: String,
    pub is_admin: bool,
    pub created_at: i64,
}

impl User {
    /// Display name if set, otherwise the username.
    pub fn name(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.username
        } else {
            &self.display_name
        }
    }
}

const USER_COLS: &str =
    "id, username, email, display_name, bio, password_hash, is_admin, created_at";

pub async fn count_users(db: &Db) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await
}

pub async fn create_user(
    db: &Db,
    username: &str,
    email: &str,
    password_hash: &str,
    is_admin: bool,
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO users (username, email, password_hash, is_admin, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .bind(is_admin)
    .bind(now())
    .execute(db)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn user_by_id(db: &Db, id: i64) -> Result<Option<User>> {
    sqlx::query_as(&format!("SELECT {USER_COLS} FROM users WHERE id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn user_by_username(db: &Db, username: &str) -> Result<Option<User>> {
    sqlx::query_as(&format!("SELECT {USER_COLS} FROM users WHERE username = ?"))
        .bind(username)
        .fetch_optional(db)
        .await
}

pub async fn user_by_login(db: &Db, login: &str) -> Result<Option<User>> {
    sqlx::query_as(&format!(
        "SELECT {USER_COLS} FROM users WHERE username = ?1 OR email = ?1"
    ))
    .bind(login)
    .fetch_optional(db)
    .await
}

pub async fn email_taken(db: &Db, email: &str, except_user: i64) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE email = ? AND id != ?")
        .bind(email)
        .bind(except_user)
        .fetch_one(db)
        .await?;
    Ok(n > 0)
}

pub async fn update_profile(
    db: &Db,
    id: i64,
    display_name: &str,
    email: &str,
    bio: &str,
) -> Result<()> {
    sqlx::query("UPDATE users SET display_name = ?, email = ?, bio = ? WHERE id = ?")
        .bind(display_name)
        .bind(email)
        .bind(bio)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn update_password(db: &Db, id: i64, password_hash: &str) -> Result<()> {
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(password_hash)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn create_session(db: &Db, token_hash: &str, user_id: i64, ttl_secs: i64) -> Result<()> {
    let now = now();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(token_hash)
    .bind(user_id)
    .bind(now)
    .bind(now + ttl_secs)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn user_by_session(db: &Db, token_hash: &str) -> Result<Option<User>> {
    sqlx::query_as(&format!(
        "SELECT {} FROM users u JOIN sessions s ON s.user_id = u.id \
         WHERE s.token_hash = ? AND s.expires_at > ?",
        USER_COLS
            .split(", ")
            .map(|c| format!("u.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .bind(token_hash)
    .bind(now())
    .fetch_optional(db)
    .await
}

pub async fn delete_session(db: &Db, token_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_other_sessions(db: &Db, user_id: i64, keep_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token_hash != ?")
        .bind(user_id)
        .bind(keep_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn purge_expired_sessions(db: &Db) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now())
        .execute(db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// SSH keys
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
pub struct SshKey {
    pub id: i64,
    pub title: String,
    pub fingerprint: String,
    pub key_type: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

pub async fn list_keys(db: &Db, user_id: i64) -> Result<Vec<SshKey>> {
    sqlx::query_as(
        "SELECT id, title, fingerprint, key_type, created_at, last_used_at FROM ssh_keys \
         WHERE user_id = ? ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
}

pub async fn fingerprint_exists(db: &Db, fingerprint: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ssh_keys WHERE fingerprint = ?")
        .bind(fingerprint)
        .fetch_one(db)
        .await?;
    Ok(n > 0)
}

pub async fn add_key(
    db: &Db,
    user_id: i64,
    title: &str,
    fingerprint: &str,
    public_key: &str,
    key_type: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO ssh_keys (user_id, title, fingerprint, public_key, key_type, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(title)
    .bind(fingerprint)
    .bind(public_key)
    .bind(key_type)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_key(db: &Db, id: i64, user_id: i64) -> Result<bool> {
    let res = sqlx::query("DELETE FROM ssh_keys WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Finds the owner of a public key by its SHA256 fingerprint.
pub async fn user_by_key_fingerprint(db: &Db, fingerprint: &str) -> Result<Option<(User, i64)>> {
    let Some((user_id, key_id)): Option<(i64, i64)> =
        sqlx::query_as("SELECT user_id, id FROM ssh_keys WHERE fingerprint = ?")
            .bind(fingerprint)
            .fetch_optional(db)
            .await?
    else {
        return Ok(None);
    };
    Ok(user_by_id(db, user_id).await?.map(|u| (u, key_id)))
}

pub async fn touch_key(db: &Db, key_id: i64) -> Result<()> {
    sqlx::query("UPDATE ssh_keys SET last_used_at = ? WHERE id = ?")
        .bind(now())
        .bind(key_id)
        .execute(db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Repositories
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
pub struct Repo {
    pub id: i64,
    pub owner_id: i64,
    pub owner: String,
    pub name: String,
    pub description: String,
    pub is_private: bool,
    pub default_branch: String,
    pub created_at: i64,
    pub pushed_at: Option<i64>,
}

const REPO_SELECT: &str = "SELECT r.id, r.owner_id, u.username AS owner, r.name, r.description, \
     r.is_private, r.default_branch, r.created_at, r.pushed_at \
     FROM repositories r JOIN users u ON u.id = r.owner_id";

pub async fn create_repo(
    db: &Db,
    owner_id: i64,
    name: &str,
    description: &str,
    is_private: bool,
    default_branch: &str,
) -> Result<i64> {
    let now = now();
    let res = sqlx::query(
        "INSERT INTO repositories (owner_id, name, description, is_private, default_branch, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(owner_id)
    .bind(name)
    .bind(description)
    .bind(is_private)
    .bind(default_branch)
    .bind(now)
    .bind(now)
    .execute(db)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn repo_by_name(db: &Db, owner: &str, name: &str) -> Result<Option<Repo>> {
    sqlx::query_as(&format!(
        "{REPO_SELECT} WHERE u.username = ? AND r.name = ?"
    ))
    .bind(owner)
    .bind(name)
    .fetch_optional(db)
    .await
}

pub async fn repo_name_taken(db: &Db, owner_id: i64, name: &str, except: i64) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM repositories WHERE owner_id = ? AND name = ? AND id != ?",
    )
    .bind(owner_id)
    .bind(name)
    .bind(except)
    .fetch_one(db)
    .await?;
    Ok(n > 0)
}

pub async fn list_public_repos(db: &Db, limit: i64) -> Result<Vec<Repo>> {
    sqlx::query_as(&format!(
        "{REPO_SELECT} WHERE r.is_private = 0 ORDER BY COALESCE(r.pushed_at, r.updated_at) DESC LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(db)
    .await
}

/// Repositories owned by `owner_id` that `viewer` is allowed to see.
pub async fn list_owner_repos(db: &Db, owner_id: i64, viewer: Option<&User>) -> Result<Vec<Repo>> {
    let (viewer_id, admin) = viewer.map(|v| (v.id, v.is_admin)).unwrap_or((-1, false));
    sqlx::query_as(&format!(
        "{REPO_SELECT} WHERE r.owner_id = ?1 AND (r.is_private = 0 OR ?2 OR r.owner_id = ?3 \
         OR EXISTS (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = ?3)) \
         ORDER BY COALESCE(r.pushed_at, r.updated_at) DESC"
    ))
    .bind(owner_id)
    .bind(admin)
    .bind(viewer_id)
    .fetch_all(db)
    .await
}

/// Repositories a user owns or collaborates on.
pub async fn list_member_repos(db: &Db, user_id: i64) -> Result<Vec<Repo>> {
    sqlx::query_as(&format!(
        "{REPO_SELECT} WHERE r.owner_id = ?1 \
         OR EXISTS (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = ?1) \
         ORDER BY COALESCE(r.pushed_at, r.updated_at) DESC"
    ))
    .bind(user_id)
    .fetch_all(db)
    .await
}

pub async fn update_repo(
    db: &Db,
    id: i64,
    name: &str,
    description: &str,
    is_private: bool,
    default_branch: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE repositories SET name = ?, description = ?, is_private = ?, default_branch = ?, updated_at = ? WHERE id = ?",
    )
    .bind(name)
    .bind(description)
    .bind(is_private)
    .bind(default_branch)
    .bind(now())
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn set_default_branch(db: &Db, id: i64, branch: &str) -> Result<()> {
    sqlx::query("UPDATE repositories SET default_branch = ? WHERE id = ?")
        .bind(branch)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn touch_pushed(db: &Db, id: i64) -> Result<()> {
    let now = now();
    sqlx::query("UPDATE repositories SET pushed_at = ?, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(now)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_repo(db: &Db, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM repositories WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn is_collaborator(db: &Db, repo_id: i64, user_id: i64) -> Result<bool> {
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM collaborators WHERE repo_id = ? AND user_id = ?")
            .bind(repo_id)
            .bind(user_id)
            .fetch_one(db)
            .await?;
    Ok(n > 0)
}

pub async fn list_collaborators(db: &Db, repo_id: i64) -> Result<Vec<User>> {
    sqlx::query_as(
        "SELECT u.id, u.username, u.email, u.display_name, u.bio, u.password_hash, u.is_admin, u.created_at \
         FROM users u JOIN collaborators c ON c.user_id = u.id WHERE c.repo_id = ? ORDER BY u.username",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await
}

pub async fn add_collaborator(db: &Db, repo_id: i64, user_id: i64) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO collaborators (repo_id, user_id, created_at) VALUES (?, ?, ?)",
    )
    .bind(repo_id)
    .bind(user_id)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn remove_collaborator(db: &Db, repo_id: i64, user_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM collaborators WHERE repo_id = ? AND user_id = ?")
        .bind(repo_id)
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pull requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
pub struct PullRequest {
    pub id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author_id: i64,
    pub author: String,
    pub base_branch: String,
    pub head_branch: String,
    pub state: String,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub merged_by: Option<String>,
    pub merged_at: Option<i64>,
    pub closed_at: Option<i64>,
    pub created_at: i64,
    pub comments: i64,
}

impl PullRequest {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

const PULL_SELECT: &str = "SELECT p.id, p.repo_id, p.number, p.title, p.body, p.author_id, \
     a.username AS author, p.base_branch, p.head_branch, p.state, p.head_sha, p.base_sha, \
     p.merge_commit_sha, m.username AS merged_by, p.merged_at, p.closed_at, p.created_at, \
     (SELECT COUNT(*) FROM pr_events e WHERE e.pr_id = p.id AND e.kind = 'comment') AS comments \
     FROM pull_requests p JOIN users a ON a.id = p.author_id LEFT JOIN users m ON m.id = p.merged_by";

#[allow(clippy::too_many_arguments)]
pub async fn create_pull(
    db: &Db,
    repo_id: i64,
    author_id: i64,
    title: &str,
    body: &str,
    base: &str,
    head: &str,
    head_sha: &str,
    base_sha: &str,
) -> Result<i64> {
    let mut tx = db.begin().await?;
    let number: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(number), 0) + 1 FROM pull_requests WHERE repo_id = ?",
    )
    .bind(repo_id)
    .fetch_one(&mut *tx)
    .await?;
    let now = now();
    sqlx::query(
        "INSERT INTO pull_requests (repo_id, number, title, body, author_id, base_branch, head_branch, head_sha, base_sha, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(repo_id)
    .bind(number)
    .bind(title)
    .bind(body)
    .bind(author_id)
    .bind(base)
    .bind(head)
    .bind(head_sha)
    .bind(base_sha)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(number)
}

pub async fn pull_by_number(db: &Db, repo_id: i64, number: i64) -> Result<Option<PullRequest>> {
    sqlx::query_as(&format!(
        "{PULL_SELECT} WHERE p.repo_id = ? AND p.number = ?"
    ))
    .bind(repo_id)
    .bind(number)
    .fetch_optional(db)
    .await
}

/// `open = true` lists open pull requests, otherwise closed and merged ones.
pub async fn list_pulls(db: &Db, repo_id: i64, open: bool) -> Result<Vec<PullRequest>> {
    let cond = if open {
        "p.state = 'open'"
    } else {
        "p.state != 'open'"
    };
    sqlx::query_as(&format!(
        "{PULL_SELECT} WHERE p.repo_id = ? AND {cond} ORDER BY p.number DESC"
    ))
    .bind(repo_id)
    .fetch_all(db)
    .await
}

pub async fn count_pulls(db: &Db, repo_id: i64) -> Result<(i64, i64)> {
    sqlx::query_as(
        "SELECT COALESCE(SUM(state = 'open'), 0), COALESCE(SUM(state != 'open'), 0) \
         FROM pull_requests WHERE repo_id = ?",
    )
    .bind(repo_id)
    .fetch_one(db)
    .await
}

pub async fn find_open_pull(db: &Db, repo_id: i64, base: &str, head: &str) -> Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT number FROM pull_requests WHERE repo_id = ? AND base_branch = ? AND head_branch = ? AND state = 'open'",
    )
    .bind(repo_id)
    .bind(base)
    .bind(head)
    .fetch_optional(db)
    .await
}

pub async fn find_open_pull_by_head(db: &Db, repo_id: i64, head: &str) -> Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT number FROM pull_requests WHERE repo_id = ? AND head_branch = ? AND state = 'open'",
    )
    .bind(repo_id)
    .bind(head)
    .fetch_optional(db)
    .await
}

/// Open pull requests in repositories the user can push to, or that they authored.
pub async fn dashboard_pulls(db: &Db, user_id: i64) -> Result<Vec<(PullRequest, String, String)>> {
    let pulls: Vec<PullRequest> = sqlx::query_as(&format!(
        "{PULL_SELECT} JOIN repositories r ON r.id = p.repo_id WHERE p.state = 'open' AND \
         (p.author_id = ?1 OR r.owner_id = ?1 OR EXISTS \
         (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = ?1)) \
         ORDER BY p.updated_at DESC LIMIT 20"
    ))
    .bind(user_id)
    .fetch_all(db)
    .await?;
    let mut out = Vec::with_capacity(pulls.len());
    for p in pulls {
        let (owner, name): (String, String) = sqlx::query_as(
            "SELECT u.username, r.name FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = ?",
        )
        .bind(p.repo_id)
        .fetch_one(db)
        .await?;
        out.push((p, owner, name));
    }
    Ok(out)
}

pub async fn update_pull_head(
    db: &Db,
    id: i64,
    head_sha: &str,
    base_sha: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE pull_requests SET head_sha = ?, base_sha = ?, updated_at = ? WHERE id = ?")
        .bind(head_sha)
        .bind(base_sha)
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn update_pull_text(db: &Db, id: i64, title: &str, body: &str) -> Result<()> {
    sqlx::query("UPDATE pull_requests SET title = ?, body = ?, updated_at = ? WHERE id = ?")
        .bind(title)
        .bind(body)
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn mark_merged(
    db: &Db,
    id: i64,
    merged_by: Option<i64>,
    merge_commit_sha: Option<&str>,
    head_sha: &str,
    base_sha: &str,
) -> Result<()> {
    let now = now();
    sqlx::query(
        "UPDATE pull_requests SET state = 'merged', merged_by = ?, merged_at = ?, closed_at = ?, \
         merge_commit_sha = ?, head_sha = ?, base_sha = ?, updated_at = ? WHERE id = ?",
    )
    .bind(merged_by)
    .bind(now)
    .bind(now)
    .bind(merge_commit_sha)
    .bind(head_sha)
    .bind(base_sha)
    .bind(now)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn mark_closed(db: &Db, id: i64, head_sha: &str, base_sha: &str) -> Result<()> {
    let now = now();
    sqlx::query(
        "UPDATE pull_requests SET state = 'closed', closed_at = ?, head_sha = ?, base_sha = ?, updated_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(head_sha)
    .bind(base_sha)
    .bind(now)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn mark_reopened(db: &Db, id: i64, head_sha: &str, base_sha: &str) -> Result<()> {
    sqlx::query(
        "UPDATE pull_requests SET state = 'open', closed_at = NULL, head_sha = ?, base_sha = ?, updated_at = ? WHERE id = ?",
    )
    .bind(head_sha)
    .bind(base_sha)
    .bind(now())
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct PrEvent {
    pub id: i64,
    pub author: String,
    pub kind: String,
    pub body: String,
    pub created_at: i64,
}

pub async fn add_event(
    db: &Db,
    pr_id: i64,
    author_id: Option<i64>,
    kind: &str,
    body: &str,
) -> Result<()> {
    let now = now();
    sqlx::query(
        "INSERT INTO pr_events (pr_id, author_id, kind, body, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(pr_id)
    .bind(author_id)
    .bind(kind)
    .bind(body)
    .bind(now)
    .execute(db)
    .await?;
    sqlx::query("UPDATE pull_requests SET updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(pr_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn list_events(db: &Db, pr_id: i64) -> Result<Vec<PrEvent>> {
    sqlx::query_as(
        "SELECT e.id, COALESCE(u.username, '') AS author, e.kind, e.body, e.created_at FROM pr_events e \
         LEFT JOIN users u ON u.id = e.author_id WHERE e.pr_id = ? ORDER BY e.created_at, e.id",
    )
    .bind(pr_id)
    .fetch_all(db)
    .await
}
