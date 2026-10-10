//! Pull requests: compare, open, discuss, review changes, merge and close.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use git2::{Oid, Repository};
use serde::Deserialize;

use super::fmt::urlpath;
use super::repo::{RepoCtx, load_repo};
use super::{AppError, Ctx, WebResult, blocking, redirect, redirect_flash, render};
use crate::db::{self, PrEvent, PullRequest};
use crate::git::{self, CommitInfo, DiffSummary, MergeMethod, MergeRequest, Mergeability};
use crate::markdown;
use crate::state::AppState;
use crate::sync;

// ---------------------------------------------------------------------------
// Compare
// ---------------------------------------------------------------------------

pub struct CompareView {
    pub base: String,
    pub head: String,
    pub branches: Vec<String>,
    pub commits: Vec<CommitInfo>,
    pub diff: Option<DiffSummary>,
    pub mergeability: Option<Mergeability>,
    pub existing: Option<i64>,
    pub title: String,
    pub notice: Option<String>,
}

impl CompareView {
    pub fn can_open(&self) -> bool {
        self.existing.is_none() && !self.commits.is_empty()
    }
}

#[derive(Template)]
#[template(path = "repo/compare.html")]
struct ComparePage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    view: CompareView,
}

#[derive(Deserialize)]
pub struct CompareQuery {
    base: Option<String>,
    head: Option<String>,
}

pub async fn compare_default(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<CompareQuery>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let base = q.base.unwrap_or_else(|| r.repo.default_branch.clone());
    match q.head.filter(|h| !h.is_empty()) {
        Some(head) => Ok(redirect(&format!(
            "{}/compare/{}...{}",
            r.base(),
            urlpath(&base),
            urlpath(&head)
        ))),
        None => compare_page(&state, ctx, r, base, String::new()).await,
    }
}

pub async fn compare(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, spec)): Path<(String, String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let (base, head) = match spec.split_once("...") {
        Some((b, h)) => (b.to_string(), h.to_string()),
        None => (r.repo.default_branch.clone(), spec.clone()),
    };
    compare_page(&state, ctx, r, base, head).await
}

async fn compare_page(
    state: &AppState,
    ctx: Ctx,
    r: RepoCtx,
    base: String,
    head: String,
) -> WebResult {
    let existing = if head.is_empty() {
        None
    } else {
        db::find_open_pull(&state.db, r.repo.id, &base, &head).await?
    };
    let path = r.path.clone();
    let view = blocking(move || {
        let repo = git::open(&path)?;
        let branches = git::branch_names(&repo)?;
        let mut view = CompareView {
            base: base.clone(),
            head: head.clone(),
            branches,
            commits: Vec::new(),
            diff: None,
            mergeability: None,
            existing,
            title: String::new(),
            notice: None,
        };
        if head.is_empty() {
            view.notice = Some("Choose a branch to compare against the base.".into());
            return Ok(view);
        }
        let (Some(base_tip), Some(head_tip)) =
            (git::branch_tip(&repo, &base), git::branch_tip(&repo, &head))
        else {
            view.notice = Some("One of the selected branches does not exist.".into());
            return Ok(view);
        };
        if base == head {
            view.notice = Some("Choose two different branches to see what changed.".into());
            return Ok(view);
        }
        let Some(merge_base) = git::merge_base(&repo, base_tip, head_tip) else {
            view.notice = Some(format!(
                "{base} and {head} have entirely different histories."
            ));
            return Ok(view);
        };
        view.commits = git::commits_between(&repo, base_tip, head_tip)?;
        if view.commits.is_empty() {
            view.notice = Some(format!(
                "{base} is up to date with all commits from {head}."
            ));
            return Ok(view);
        }
        view.title = match view.commits.as_slice() {
            [only] => only.summary.clone(),
            _ => humanize_branch(&head),
        };
        view.mergeability = Some(git::mergeability(&repo, base_tip, head_tip)?);
        view.diff = Some(git::range_diff(&repo, merge_base, head_tip)?);
        Ok(view)
    })
    .await?;
    render(&ComparePage {
        ctx,
        r,
        tab: "pulls",
        view,
    })
}

/// `feature/add-login` → `Feature/add login`
fn humanize_branch(branch: &str) -> String {
    let s = branch.replace(['-', '_'], " ");
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => s,
    }
}

#[derive(Deserialize)]
pub struct CreateForm {
    base: String,
    head: String,
    title: String,
    #[serde(default)]
    body: String,
}

pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<CreateForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let title = f.title.trim().chars().take(256).collect::<String>();
    if title.is_empty() {
        return Err(AppError::BadRequest("A pull request needs a title.".into()));
    }
    if let Some(number) = db::find_open_pull(&state.db, r.repo.id, &f.base, &f.head).await? {
        return Ok(redirect(&format!("{}/pull/{number}", r.base())));
    }
    let path = r.path.clone();
    let (base, head) = (f.base.clone(), f.head.clone());
    let (head_tip, merge_base) = blocking(move || {
        let repo = git::open(&path)?;
        let (Some(base_tip), Some(head_tip)) =
            (git::branch_tip(&repo, &base), git::branch_tip(&repo, &head))
        else {
            return Err(AppError::BadRequest("Both branches must exist.".into()));
        };
        if base == head || git::is_ancestor(&repo, head_tip, base_tip) {
            return Err(AppError::BadRequest(
                "There is nothing to merge between these branches.".into(),
            ));
        }
        let merge_base = git::merge_base(&repo, base_tip, head_tip).ok_or_else(|| {
            AppError::BadRequest("These branches have entirely different histories.".into())
        })?;
        Ok((head_tip, merge_base))
    })
    .await?;
    let number = db::create_pull(
        &state.db,
        r.repo.id,
        user.id,
        &title,
        f.body.trim(),
        &f.base,
        &f.head,
        &head_tip.to_string(),
        &merge_base.to_string(),
    )
    .await?;
    let path = r.path.clone();
    blocking(move || Ok(git::set_pull_ref(&git::open(&path)?, number, head_tip)?)).await?;
    Ok(redirect(&format!("{}/pull/{number}", r.base())))
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/pulls.html")]
struct PullsPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    pulls: Vec<PullRequest>,
    showing_open: bool,
    open_count: i64,
    closed_count: i64,
}

#[derive(Deserialize)]
pub struct ListQuery {
    state: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let showing_open = q.state.as_deref() != Some("closed");
    let pulls = db::list_pulls(&state.db, r.repo.id, showing_open).await?;
    let (open_count, closed_count) = db::count_pulls(&state.db, r.repo.id).await?;
    render(&PullsPage {
        ctx,
        r,
        tab: "pulls",
        pulls,
        showing_open,
        open_count,
        closed_count,
    })
}

// ---------------------------------------------------------------------------
// Pull request pages
// ---------------------------------------------------------------------------

/// Everything derived from git about a pull request.
pub struct PullData {
    pub head_exists: bool,
    pub base_exists: bool,
    pub commits: Vec<CommitInfo>,
    pub mergeability: Option<Mergeability>,
    pub files: usize,
    pub additions: usize,
    pub deletions: usize,
    pub diff: Option<DiffSummary>,
}

fn parse_oid(s: Option<&str>) -> Option<Oid> {
    s.and_then(|s| Oid::from_str(s).ok())
}

/// The commit range a pull request covers: (merge base, head).
fn pull_range(repo: &Repository, pr: &PullRequest) -> (Option<Oid>, Option<Oid>) {
    let stored_head = parse_oid(pr.head_sha.as_deref()).or_else(|| git::pull_ref(repo, pr.number));
    if !pr.is_open() {
        return (parse_oid(pr.base_sha.as_deref()), stored_head);
    }
    let head = git::branch_tip(repo, &pr.head_branch).or(stored_head);
    let base = git::branch_tip(repo, &pr.base_branch);
    match (base, head) {
        (Some(b), Some(h)) => (git::merge_base(repo, b, h), Some(h)),
        _ => (parse_oid(pr.base_sha.as_deref()), head),
    }
}

fn pull_data(repo: &Repository, pr: &PullRequest, with_diff: bool) -> Result<PullData, AppError> {
    let head_tip = git::branch_tip(repo, &pr.head_branch);
    let base_tip = git::branch_tip(repo, &pr.base_branch);
    let (from, to) = pull_range(repo, pr);
    let mut data = PullData {
        head_exists: head_tip.is_some(),
        base_exists: base_tip.is_some(),
        commits: Vec::new(),
        mergeability: None,
        files: 0,
        additions: 0,
        deletions: 0,
        diff: None,
    };
    if let (Some(from), Some(to)) = (from, to) {
        data.commits = git::commits_between(repo, from, to)?;
        (data.files, data.additions, data.deletions) = git::diff_stats(repo, from, to)?;
        if with_diff {
            data.diff = Some(git::range_diff(repo, from, to)?);
        }
    }
    if let (true, Some(base), Some(head)) = (pr.is_open(), base_tip, head_tip) {
        data.mergeability = Some(git::mergeability(repo, base, head)?);
    }
    Ok(data)
}

async fn load_pull(
    state: &AppState,
    r: &RepoCtx,
    number: i64,
    with_diff: bool,
) -> Result<(PullRequest, PullData), AppError> {
    let mut pr = db::pull_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    if pr.is_open() {
        // Pick up pushes and merges that happened since the last sync.
        sync::refresh_pulls(state, r.repo.id, None).await?;
        pr = db::pull_by_number(&state.db, r.repo.id, number)
            .await?
            .ok_or(AppError::NotFound)?;
    }
    let path = r.path.clone();
    let pr_clone = pr.clone();
    let data = blocking(move || pull_data(&git::open(&path)?, &pr_clone, with_diff)).await?;
    Ok((pr, data))
}

pub struct TimelineItem {
    pub event: PrEvent,
    pub html: String,
}

#[derive(Template)]
#[template(path = "repo/pull.html")]
struct PullPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    pr: PullRequest,
    data: PullData,
    pull_tab: &'static str,
    body_html: String,
    timeline: Vec<TimelineItem>,
    can_close: bool,
    can_delete_branch: bool,
}

async fn pull_page(
    state: &AppState,
    ctx: Ctx,
    owner: &str,
    name: &str,
    number: i64,
    pull_tab: &'static str,
) -> WebResult {
    let r = load_repo(state, &ctx, owner, name).await?;
    let (pr, data) = load_pull(state, &r, number, pull_tab == "files").await?;
    let user = ctx.user.as_deref();
    let can_close = user.is_some_and(|u| u.id == pr.author_id) || r.access.write;
    let can_delete_branch = r.access.write
        && !pr.is_open()
        && data.head_exists
        && pr.head_branch != r.repo.default_branch
        && db::find_open_pull_by_head(&state.db, r.repo.id, &pr.head_branch)
            .await?
            .is_none();
    let timeline = if pull_tab == "conversation" {
        db::list_events(&state.db, pr.id)
            .await?
            .into_iter()
            .map(|event| TimelineItem {
                html: if event.kind == "comment" {
                    markdown::render(&event.body, None)
                } else {
                    String::new()
                },
                event,
            })
            .collect()
    } else {
        Vec::new()
    };
    let body_html = markdown::render(&pr.body, None);
    render(&PullPage {
        ctx,
        r,
        tab: "pulls",
        pr,
        data,
        pull_tab,
        body_html,
        timeline,
        can_close,
        can_delete_branch,
    })
}

pub async fn conversation(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> WebResult {
    pull_page(&state, ctx, &owner, &name, number, "conversation").await
}

pub async fn commits(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> WebResult {
    pull_page(&state, ctx, &owner, &name, number, "commits").await
}

pub async fn files(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> WebResult {
    pull_page(&state, ctx, &owner, &name, number, "files").await
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CommentForm {
    #[serde(default)]
    body: String,
    close: Option<String>,
}

pub async fn comment(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<CommentForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let pr = db::pull_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    let body = f.body.trim();
    if !body.is_empty() {
        db::add_event(&state.db, pr.id, Some(user.id), "comment", body).await?;
    }
    let url = format!("{}/pull/{number}", r.base());
    if f.close.is_some() && pr.is_open() {
        return close_pull(&state, &r, &pr, user.id, &url).await;
    }
    if body.is_empty() {
        return Ok(redirect_flash(&url, "error", "Comment cannot be empty."));
    }
    Ok(redirect(&url))
}

async fn close_pull(
    state: &AppState,
    r: &RepoCtx,
    pr: &PullRequest,
    user_id: i64,
    url: &str,
) -> WebResult {
    if !(r.access.write || pr.author_id == user_id) {
        return Err(AppError::Forbidden(
            "Only the author or a collaborator can close this.".into(),
        ));
    }
    let path = r.path.clone();
    let pr_clone = pr.clone();
    let (base, head) = blocking(move || Ok(pull_range(&git::open(&path)?, &pr_clone))).await?;
    let head = head.map(|h| h.to_string()).unwrap_or_default();
    let base = base.map(|b| b.to_string()).unwrap_or_default();
    db::mark_closed(&state.db, pr.id, &head, &base).await?;
    db::add_event(&state.db, pr.id, Some(user_id), "closed", "").await?;
    Ok(redirect(url))
}

#[derive(Deserialize)]
pub struct StateForm {
    action: String,
}

pub async fn set_state(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<StateForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let pr = db::pull_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    let url = format!("{}/pull/{number}", r.base());
    match f.action.as_str() {
        "close" if pr.is_open() => close_pull(&state, &r, &pr, user.id, &url).await,
        "reopen" if pr.state == "closed" => {
            if !(r.access.write || pr.author_id == user.id) {
                return Err(AppError::Forbidden(
                    "Only the author or a collaborator can reopen this.".into(),
                ));
            }
            if db::find_open_pull(&state.db, r.repo.id, &pr.base_branch, &pr.head_branch)
                .await?
                .is_some()
            {
                return Ok(redirect_flash(
                    &url,
                    "error",
                    "Another open pull request already exists for these branches.",
                ));
            }
            let path = r.path.clone();
            let (base_branch, head_branch, n) =
                (pr.base_branch.clone(), pr.head_branch.clone(), pr.number);
            let tips = blocking(move || {
                let repo = git::open(&path)?;
                let (Some(base), Some(head)) = (
                    git::branch_tip(&repo, &base_branch),
                    git::branch_tip(&repo, &head_branch),
                ) else {
                    return Ok(None);
                };
                git::set_pull_ref(&repo, n, head)?;
                Ok(git::merge_base(&repo, base, head).map(|mb| (head, mb)))
            })
            .await?;
            let Some((head, merge_base)) = tips else {
                return Ok(redirect_flash(
                    &url,
                    "error",
                    "This pull request cannot be reopened because a branch no longer exists.",
                ));
            };
            db::mark_reopened(&state.db, pr.id, &head.to_string(), &merge_base.to_string()).await?;
            db::add_event(&state.db, pr.id, Some(user.id), "reopened", "").await?;
            Ok(redirect(&url))
        }
        _ => Ok(redirect(&url)),
    }
}

#[derive(Deserialize)]
pub struct MergeForm {
    method: String,
}

pub async fn merge(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<MergeForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    r.require_write()?;
    let pr = db::pull_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    let url = format!("{}/pull/{number}", r.base());
    if !pr.is_open() {
        return Ok(redirect(&url));
    }
    let method = MergeMethod::parse(&f.method)
        .ok_or_else(|| AppError::BadRequest("Unknown merge method.".into()))?;

    let path = r.path.clone();
    let pr_clone = pr.clone();
    let (committer_name, committer_email) = (user.name().to_string(), user.email.clone());
    let result = blocking(move || {
        let repo = git::open(&path)?;
        let (Some(base), Some(head)) = (
            git::branch_tip(&repo, &pr_clone.base_branch),
            git::branch_tip(&repo, &pr_clone.head_branch),
        ) else {
            return Ok(Err("Both branches must exist to merge.".to_string()));
        };
        match git::mergeability(&repo, base, head)? {
            Mergeability::Clean => {}
            Mergeability::Conflicts => {
                return Ok(Err(
                    "This branch has conflicts that must be resolved first.".into(),
                ));
            }
            Mergeability::UpToDate => return Ok(Err("There is nothing to merge.".into())),
        }
        let merge_base = git::merge_base(&repo, base, head)
            .ok_or_else(|| AppError::BadRequest("Branches have unrelated histories.".into()))?;
        let message = match method {
            MergeMethod::Merge => format!(
                "Merge pull request #{} from {}\n\n{}",
                pr_clone.number, pr_clone.head_branch, pr_clone.title
            ),
            MergeMethod::Squash => {
                let mut m = format!("{} (#{})\n", pr_clone.title, pr_clone.number);
                let commits = git::commits_between(&repo, base, head)?;
                if commits.len() > 1 {
                    m.push('\n');
                    for c in commits {
                        m.push_str(&format!("* {}\n", c.summary));
                    }
                }
                m
            }
            MergeMethod::Rebase => String::new(),
        };
        let sig = git2::Signature::now(&committer_name, &committer_email)?;
        let new_tip = match git::merge(
            &repo,
            &MergeRequest {
                base_branch: &pr_clone.base_branch,
                base,
                head,
                method,
                message: &message,
                committer: &sig,
            },
        ) {
            Ok(tip) => tip,
            Err(e) => return Ok(Err(e.to_string())),
        };
        git::set_pull_ref(&repo, pr_clone.number, head)?;
        Ok(Ok((new_tip, head, merge_base)))
    })
    .await?;

    let (new_tip, head, merge_base) = match result {
        Ok(v) => v,
        Err(msg) => return Ok(redirect_flash(&url, "error", &msg)),
    };
    db::mark_merged(
        &state.db,
        pr.id,
        Some(user.id),
        Some(&new_tip.to_string()),
        &head.to_string(),
        &merge_base.to_string(),
    )
    .await?;
    db::add_event(&state.db, pr.id, Some(user.id), "merged", "").await?;
    db::touch_pushed(&state.db, r.repo.id).await?;
    sync::refresh_pulls(&state, r.repo.id, Some(user.id)).await?;
    Ok(redirect_flash(&url, "success", "Pull request merged."))
}

#[derive(Deserialize)]
pub struct EditForm {
    title: String,
    #[serde(default)]
    body: String,
}

pub async fn edit(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<EditForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let pr = db::pull_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    if !(r.access.write || pr.author_id == user.id) {
        return Err(AppError::Forbidden(
            "Only the author or a collaborator can edit this.".into(),
        ));
    }
    let title = f.title.trim().chars().take(256).collect::<String>();
    if title.is_empty() {
        return Err(AppError::BadRequest("A pull request needs a title.".into()));
    }
    db::update_pull_text(&state.db, pr.id, &title, f.body.trim()).await?;
    Ok(redirect(&format!("{}/pull/{number}", r.base())))
}
