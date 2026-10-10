//! Browsing code: repository home, trees, files, raw blobs and history.

use std::process::Stdio;

use askama::Template;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::IntoResponse;
use git2::{BranchType, ObjectType, Oid, Repository};
use serde::Deserialize;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

use super::fmt::urlpath;
use super::repo::{RepoCtx, load_repo};
use super::{AppError, Ctx, WebResult, blocking, redirect, render};
use crate::db;
use crate::git::{self, CommitInfo, DiffSummary, RefKind, Resolved, TreeEntry};
use crate::highlight;
use crate::markdown::{self, LinkBase};
use crate::state::AppState;

const COMMITS_PER_PAGE: usize = 30;
const MAX_DISPLAY_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Shared view pieces
// ---------------------------------------------------------------------------

pub struct RefLink {
    pub name: String,
    pub href: String,
    pub selected: bool,
}

/// The branch/tag switcher shown above code listings.
pub struct RefSelector {
    pub current: String,
    pub kind: RefKind,
    pub branches: Vec<RefLink>,
    pub tags: Vec<RefLink>,
}

impl RefSelector {
    /// `view` is `tree`, `blob` or `commits`; `path` is kept when switching.
    fn build(repo: &Repository, base: &str, view: &str, current: &Resolved, path: &str) -> Self {
        let link = |name: &str, selected: bool| {
            let mut href = format!("{base}/{view}/{}", urlpath(name));
            if !path.is_empty() {
                href.push('/');
                href.push_str(&urlpath(path));
            }
            RefLink {
                name: name.to_string(),
                href,
                selected,
            }
        };
        let branches = git::branch_names(repo)
            .unwrap_or_default()
            .into_iter()
            .map(|b| link(&b, current.kind == RefKind::Branch && current.name == b))
            .collect();
        let tags = git::tag_names(repo)
            .unwrap_or_default()
            .into_iter()
            .map(|t| link(&t, current.kind == RefKind::Tag && current.name == t))
            .collect();
        let current_label = match current.kind {
            RefKind::Commit => current.name.chars().take(7).collect(),
            _ => current.name.clone(),
        };
        RefSelector {
            current: current_label,
            kind: current.kind,
            branches,
            tags,
        }
    }

    pub fn icon(&self) -> &'static str {
        match self.kind {
            RefKind::Tag => "tag",
            _ => "git-branch",
        }
    }
}

pub struct Crumb {
    pub name: String,
    pub href: String,
}

fn breadcrumbs(base: &str, refname: &str, path: &str) -> Vec<Crumb> {
    let mut crumbs = Vec::new();
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        crumbs.push(Crumb {
            name: seg.to_string(),
            href: format!("{base}/tree/{}/{}", urlpath(refname), urlpath(&acc)),
        });
    }
    crumbs
}

pub struct EntryRow {
    pub entry: TreeEntry,
    pub href: String,
}

/// Resolves `{ref}/{path}` or returns 404.
fn resolve(repo: &Repository, rest: &str) -> Result<Resolved, AppError> {
    git::resolve(repo, rest).ok_or(AppError::NotFound)
}

fn link_base(base: &str, refname: &str, dir: &str) -> LinkBase {
    LinkBase {
        blob_root: format!("{base}/blob/{}", urlpath(refname)),
        raw_root: format!("{base}/raw/{}", urlpath(refname)),
        dir: dir.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Repository home and trees
// ---------------------------------------------------------------------------

pub struct Readme {
    pub name: String,
    pub html: String,
    pub is_markdown: bool,
}

pub struct TreeView {
    pub refsel: RefSelector,
    pub refname: String,
    pub path: String,
    pub crumbs: Vec<Crumb>,
    pub entries: Vec<EntryRow>,
    pub parent_href: Option<String>,
    pub latest: Option<CommitInfo>,
    pub commit_count: usize,
    pub branch_count: usize,
    pub tag_count: usize,
    pub readme: Option<Readme>,
    pub history_href: String,
    pub recent_branch: Option<(String, i64)>,
}

#[derive(Template)]
#[template(path = "repo/code.html")]
struct CodePage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    view: TreeView,
    is_root: bool,
}

#[derive(Template)]
#[template(path = "repo/empty.html")]
struct EmptyPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
}

fn build_tree_view(
    repo: &Repository,
    base: &str,
    resolved: &Resolved,
    with_stats: bool,
    default_branch: &str,
) -> Result<Option<TreeView>, AppError> {
    let path = resolved.path.trim_matches('/').to_string();
    let Some(entries) = git::list_tree(repo, resolved.commit, &path)? else {
        return Ok(None);
    };
    let refname = resolved.name.clone();
    let entries = entries
        .into_iter()
        .map(|e| {
            let view = if e.is_dir() { "tree" } else { "blob" };
            let href = format!("{base}/{view}/{}/{}", urlpath(&refname), urlpath(&e.path));
            EntryRow { entry: e, href }
        })
        .collect();
    let latest = if path.is_empty() {
        Some(git::commit_info(repo, resolved.commit)?)
    } else {
        git::log(repo, resolved.commit, Some(&path), 0, 1)?
            .0
            .into_iter()
            .next()
    };
    let readme = git::find_readme(repo, resolved.commit, &path)?.map(|(name, text)| {
        let lower = name.to_ascii_lowercase();
        let is_markdown = lower.ends_with(".md") || lower.ends_with(".markdown");
        let html = if is_markdown {
            markdown::render(&text, Some(&link_base(base, &refname, &path)))
        } else {
            highlight::escape(&text)
        };
        Readme {
            name,
            html,
            is_markdown,
        }
    });
    let parent_href = (!path.is_empty()).then(|| match path.rsplit_once('/') {
        Some((parent, _)) => format!("{base}/tree/{}/{}", urlpath(&refname), urlpath(parent)),
        None => format!("{base}/tree/{}", urlpath(&refname)),
    });
    let (commit_count, branch_count, tag_count) = if with_stats {
        (
            git::commit_count(repo, resolved.commit)?,
            git::branch_names(repo)?.len(),
            git::tag_names(repo)?.len(),
        )
    } else {
        (0, 0, 0)
    };
    let recent_branch = if with_stats {
        recently_pushed_branch(repo, default_branch)
    } else {
        None
    };
    let mut history_href = format!("{base}/commits/{}", urlpath(&refname));
    if !path.is_empty() {
        history_href.push('/');
        history_href.push_str(&urlpath(&path));
    }
    Ok(Some(TreeView {
        refsel: RefSelector::build(repo, base, "tree", resolved, &path),
        crumbs: breadcrumbs(base, &refname, &path),
        refname,
        path,
        entries,
        parent_href,
        latest,
        commit_count,
        branch_count,
        tag_count,
        readme,
        history_href,
        recent_branch,
    }))
}

/// The most recently updated non-default branch, if it changed in the last hour.
fn recently_pushed_branch(repo: &Repository, default_branch: &str) -> Option<(String, i64)> {
    let cutoff = db::now() - 3600;
    repo.branches(Some(BranchType::Local))
        .ok()?
        .flatten()
        .filter_map(|(b, _)| {
            let name = b.name().ok()??.to_string();
            let time = b.get().peel_to_commit().ok()?.committer().when().seconds();
            (name != default_branch && time > cutoff).then_some((name, time))
        })
        .max_by_key(|(_, t)| *t)
}

pub async fn repo_home(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    if let Some(stripped) = name.strip_suffix(".git") {
        return Ok(redirect(&format!("/{owner}/{stripped}")));
    }
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let (path, base, default) = (r.path.clone(), r.base(), r.repo.default_branch.clone());
    let view = blocking(move || {
        let repo = git::open(&path)?;
        if git::is_empty(&repo) {
            return Ok(None);
        }
        let head = git::resolve(&repo, &default).or_else(|| {
            let branch = git::branch_names(&repo).ok()?.into_iter().next()?;
            git::resolve(&repo, &branch)
        });
        let Some(resolved) = head else {
            return Ok(None);
        };
        build_tree_view(&repo, &base, &resolved, true, &default)
    })
    .await?;
    let Some(mut view) = view else {
        return render(&EmptyPage {
            ctx,
            r,
            tab: "code",
        });
    };
    // Only suggest opening a pull request to people who can push, and only once.
    if let Some((branch, _)) = &view.recent_branch {
        let has_pr = db::list_pulls(&state.db, r.repo.id, true)
            .await?
            .iter()
            .any(|p| &p.head_branch == branch);
        if has_pr || !r.access.write {
            view.recent_branch = None;
        }
    }
    render(&CodePage {
        ctx,
        r,
        tab: "code",
        view,
        is_root: true,
    })
}

pub async fn tree(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let (path, base, default) = (r.path.clone(), r.base(), r.repo.default_branch.clone());
    let view = blocking(move || {
        let repo = git::open(&path)?;
        let resolved = resolve(&repo, &rest)?;
        if let Some(ObjectType::Blob) = git::path_type(&repo, resolved.commit, &resolved.path)? {
            return Ok(Err(format!("{base}/blob/{rest}")));
        }
        let is_root = resolved.path.is_empty();
        Ok(Ok((
            build_tree_view(&repo, &base, &resolved, is_root, &default)?,
            is_root,
        )))
    })
    .await?;
    match view {
        Err(blob_url) => Ok(redirect(&urlpath(&blob_url))),
        Ok((Some(view), is_root)) => render(&CodePage {
            ctx,
            r,
            tab: "code",
            view,
            is_root,
        }),
        Ok((None, _)) => Err(AppError::NotFound),
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

pub enum BlobContent {
    Code { lines: Vec<String> },
    Markdown { html: String },
    Image { src: String },
    Binary,
    TooLarge,
}

pub struct BlobView {
    pub refsel: RefSelector,
    pub refname: String,
    pub path: String,
    pub file_name: String,
    pub crumbs: Vec<Crumb>,
    pub size: usize,
    pub line_count: usize,
    pub language: Option<&'static str>,
    pub content: BlobContent,
    pub latest: Option<CommitInfo>,
    pub raw_href: String,
    pub history_href: String,
    pub is_markdown: bool,
    pub plain: bool,
}

#[derive(Template)]
#[template(path = "repo/blob.html")]
struct BlobPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    view: BlobView,
}

#[derive(Deserialize)]
pub struct BlobQuery {
    plain: Option<String>,
}

fn is_image(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".ico", ".svg",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

pub async fn blob(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Query(q): Query<BlobQuery>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let plain = q.plain.is_some_and(|p| p == "1");
    let (path, base) = (r.path.clone(), r.base());
    let view = blocking(move || {
        let repo = git::open(&path)?;
        let resolved = resolve(&repo, &rest)?;
        let file_path = resolved.path.clone();
        if file_path.is_empty() {
            return Ok(Err(format!("{base}/tree/{}", resolved.name)));
        }
        match git::path_type(&repo, resolved.commit, &file_path)? {
            Some(ObjectType::Tree) => return Ok(Err(format!("{base}/tree/{rest}"))),
            Some(ObjectType::Blob) => {}
            _ => return Err(AppError::NotFound),
        }
        let blob = git::read_blob(&repo, resolved.commit, &file_path)?.ok_or(AppError::NotFound)?;
        let refname = resolved.name.clone();
        let raw_href = format!("{base}/raw/{}/{}", urlpath(&refname), urlpath(&file_path));
        let lower = file_path.to_ascii_lowercase();
        let is_markdown = lower.ends_with(".md") || lower.ends_with(".markdown");
        let dir = file_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

        let mut line_count = 0;
        let mut language = None;
        let content = if is_image(&file_path) {
            BlobContent::Image {
                src: raw_href.clone(),
            }
        } else if blob.is_binary {
            BlobContent::Binary
        } else if blob.size > MAX_DISPLAY_BYTES {
            BlobContent::TooLarge
        } else {
            let text = String::from_utf8_lossy(&blob.data);
            let lines: Vec<&str> = text.lines().collect();
            line_count = lines.len();
            language = highlight::language_name(&file_path, lines.first().copied().unwrap_or(""));
            if is_markdown && !plain {
                BlobContent::Markdown {
                    html: markdown::render(&text, Some(&link_base(&base, &refname, dir))),
                }
            } else {
                BlobContent::Code {
                    lines: highlight::highlight_lines(&file_path, &lines),
                }
            }
        };
        let latest = git::log(&repo, resolved.commit, Some(&file_path), 0, 1)?
            .0
            .into_iter()
            .next();
        Ok(Ok(BlobView {
            refsel: RefSelector::build(&repo, &base, "blob", &resolved, &file_path),
            crumbs: breadcrumbs(&base, &refname, dir),
            file_name: file_path
                .rsplit('/')
                .next()
                .unwrap_or(&file_path)
                .to_string(),
            history_href: format!(
                "{base}/commits/{}/{}",
                urlpath(&refname),
                urlpath(&file_path)
            ),
            refname,
            path: file_path,
            size: blob.size,
            line_count,
            language,
            content,
            latest,
            raw_href,
            is_markdown,
            plain,
        }))
    })
    .await?;
    match view {
        Ok(view) => render(&BlobPage {
            ctx,
            r,
            tab: "code",
            view,
        }),
        Err(url) => Ok(redirect(&urlpath(&url))),
    }
}

pub async fn raw(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let path = r.path.clone();
    let (file_path, blob) = blocking(move || {
        let repo = git::open(&path)?;
        let resolved = resolve(&repo, &rest)?;
        let blob =
            git::read_blob(&repo, resolved.commit, &resolved.path)?.ok_or(AppError::NotFound)?;
        Ok((resolved.path, blob))
    })
    .await?;

    let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
    let content_type = if mime.type_() == "image" {
        mime.essence_str().to_string()
    } else if !blob.is_binary {
        "text/plain; charset=utf-8".to_string()
    } else {
        "application/octet-stream".to_string()
    };
    let file_name = file_path
        .rsplit('/')
        .next()
        .unwrap_or("file")
        .replace('"', "");
    let disposition = if content_type == "application/octet-stream" {
        format!("attachment; filename=\"{file_name}\"")
    } else {
        format!("inline; filename=\"{file_name}\"")
    };
    let mut resp = blob.data.into_response();
    let headers = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    // User content is never allowed to run script on this origin.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    Ok(resp)
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

pub struct CommitGroup {
    pub date: String,
    pub commits: Vec<CommitInfo>,
}

pub struct CommitsView {
    pub refsel: RefSelector,
    pub refname: String,
    pub path: String,
    pub groups: Vec<CommitGroup>,
    pub newer_href: Option<String>,
    pub older_href: Option<String>,
}

#[derive(Template)]
#[template(path = "repo/commits.html")]
struct CommitsPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    view: CommitsView,
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<usize>,
}

pub async fn commits_default(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    Ok(redirect(&format!(
        "{}/commits/{}",
        r.base(),
        urlpath(&r.repo.default_branch)
    )))
}

pub async fn commits(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Query(q): Query<PageQuery>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let page = q.page.unwrap_or(1).max(1);
    let (path, base) = (r.path.clone(), r.base());
    let view = blocking(move || {
        let repo = git::open(&path)?;
        let resolved = resolve(&repo, &rest)?;
        let file_path = resolved.path.trim_matches('/').to_string();
        let (list, more) = git::log(
            &repo,
            resolved.commit,
            Some(&file_path),
            (page - 1) * COMMITS_PER_PAGE,
            COMMITS_PER_PAGE,
        )?;
        let mut groups: Vec<CommitGroup> = Vec::new();
        for c in list {
            let date = super::fmt::date(c.committer_time);
            match groups.last_mut() {
                Some(g) if g.date == date => g.commits.push(c),
                _ => groups.push(CommitGroup {
                    date,
                    commits: vec![c],
                }),
            }
        }
        let mut here = format!("{base}/commits/{}", urlpath(&resolved.name));
        if !file_path.is_empty() {
            here.push('/');
            here.push_str(&urlpath(&file_path));
        }
        Ok(CommitsView {
            refsel: RefSelector::build(&repo, &base, "commits", &resolved, &file_path),
            refname: resolved.name.clone(),
            path: file_path,
            groups,
            newer_href: (page > 1).then(|| format!("{here}?page={}", page - 1)),
            older_href: more.then(|| format!("{here}?page={}", page + 1)),
        })
    })
    .await?;
    render(&CommitsPage {
        ctx,
        r,
        tab: "commits",
        view,
    })
}

#[derive(Template)]
#[template(path = "repo/commit.html")]
struct CommitPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    commit: CommitInfo,
    diff: DiffSummary,
}

pub async fn commit(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, sha)): Path<(String, String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) || sha.len() < 4 || sha.len() > 40 {
        return Err(AppError::NotFound);
    }
    let path = r.path.clone();
    let (commit, diff) = blocking(move || {
        let repo = git::open(&path)?;
        let oid: Oid = repo
            .revparse_single(&sha)
            .and_then(|o| o.peel_to_commit())
            .map_err(|_| AppError::NotFound)?
            .id();
        Ok((git::commit_info(&repo, oid)?, git::commit_diff(&repo, oid)?))
    })
    .await?;
    render(&CommitPage {
        ctx,
        r,
        tab: "commits",
        commit,
        diff,
    })
}

// ---------------------------------------------------------------------------
// Archives
// ---------------------------------------------------------------------------

/// Streams `git archive` output for `{ref}.zip` or `{ref}.tar.gz`.
pub async fn archive(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, file)): Path<(String, String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let (refname, format, content_type) = if let Some(r) = file.strip_suffix(".zip") {
        (r.to_string(), "zip", "application/zip")
    } else if let Some(r) = file.strip_suffix(".tar.gz") {
        (r.to_string(), "tar.gz", "application/gzip")
    } else {
        return Err(AppError::NotFound);
    };
    let path = r.path.clone();
    let lookup = refname.clone();
    let commit = blocking(move || {
        let repo = git::open(&path)?;
        let resolved = resolve(&repo, &lookup)?;
        if !resolved.path.is_empty() {
            return Err(AppError::NotFound);
        }
        Ok(resolved.commit)
    })
    .await?;

    let stem = format!("{}-{}", r.repo.name, refname.replace('/', "-"));
    let mut child = Command::new(&state.config.git_bin)
        .arg("--git-dir")
        .arg(&r.path)
        .arg("archive")
        .arg(format!("--format={format}"))
        .arg(format!("--prefix={stem}/"))
        .arg(commit.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("git archive stdout"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let mut resp = Body::from_stream(ReaderStream::new(stdout)).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}.{format}\"",
        stem.replace('"', "")
    )) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}
