//! Issues: create, discuss, label, close and reopen.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use super::repo::{RepoCtx, load_repo};
use super::{AppError, Ctx, WebResult, redirect, redirect_flash, render};
use crate::db::{self, Issue, IssueEvent, Label};
use crate::markdown;
use crate::state::AppState;

/// Parses the `labels` field of the issue form.
///
/// Labels are comma-separated. Names are trimmed, internal runs of whitespace
/// collapse to one space, and case-insensitive duplicates are dropped. Each
/// name is capped at 50 characters and at most 20 labels are kept.
pub fn parse_labels(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.split(',') {
        let name: String = raw
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(50)
            .collect();
        if name.is_empty() {
            continue;
        }
        if out.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            continue;
        }
        out.push(name);
        if out.len() == 20 {
            break;
        }
    }
    out
}

/// The label names of an issue joined with ", ", for the edit form.
pub fn label_names(issue: &Issue) -> String {
    issue
        .label_list()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Applies a set of label names to an issue, creating missing labels.
async fn set_labels(
    state: &AppState,
    repo_id: i64,
    issue_id: i64,
    names: &[String],
) -> Result<(), AppError> {
    db::clear_issue_labels(&state.db, issue_id).await?;
    for name in names {
        let id = db::ensure_label(&state.db, repo_id, name).await?;
        db::add_issue_label(&state.db, issue_id, id).await?;
    }
    Ok(())
}

/// Whether `user_id` may change an issue's state or text: the author or a
/// member of the repository with write access.
fn can_manage(r: &RepoCtx, issue: &Issue, user_id: i64) -> bool {
    r.access.write || issue.author_id == user_id
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/issues.html")]
struct IssuesPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    issues: Vec<Issue>,
    showing_open: bool,
    open_count: i64,
    closed_count: i64,
    labels: Vec<Label>,
    active_label: Option<String>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    state: Option<String>,
    label: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let showing_open = q.state.as_deref() != Some("closed");
    let active_label = q.label.filter(|l| !l.trim().is_empty());
    let filter = db::IssueFilter {
        open: showing_open,
        label: active_label.clone(),
    };
    let issues = db::list_issues(&state.db, r.repo.id, &filter).await?;
    let (open_count, closed_count) = db::count_issues(&state.db, r.repo.id).await?;
    let labels = db::list_labels(&state.db, r.repo.id).await?;
    render(&IssuesPage {
        ctx,
        r,
        tab: "issues",
        issues,
        showing_open,
        open_count,
        closed_count,
        labels,
        active_label,
    })
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/new_issue.html")]
struct NewIssuePage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    title: String,
    body: String,
    labels: String,
    error: Option<String>,
}

pub async fn new_form(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    ctx.require_user()?;
    render(&NewIssuePage {
        ctx,
        r,
        tab: "issues",
        title: String::new(),
        body: String::new(),
        labels: String::new(),
        error: None,
    })
}

#[derive(Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    labels: String,
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
        return render(&NewIssuePage {
            ctx,
            r,
            tab: "issues",
            title: f.title,
            body: f.body,
            labels: f.labels,
            error: Some("An issue needs a title.".into()),
        });
    }
    let number = db::create_issue(&state.db, r.repo.id, user.id, &title, f.body.trim()).await?;
    let issue = db::issue_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    let names = parse_labels(&f.labels);
    if !names.is_empty() {
        set_labels(&state, r.repo.id, issue.id, &names).await?;
    }
    Ok(redirect(&format!("{}/issues/{number}", r.base())))
}

// ---------------------------------------------------------------------------
// Issue page
// ---------------------------------------------------------------------------

/// The fields an issue template needs, flattened so askama can resolve them
/// inside `{% block %}` overrides (where `{% let %}` bindings are not in scope).
#[derive(Clone)]
pub struct IssueView {
    pub title: String,
    pub number: i64,
    pub author: String,
    pub body: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub closed_by: Option<String>,
    pub closed_at: Option<i64>,
    pub created_at: i64,
    pub comments: i64,
}

impl From<&Issue> for IssueView {
    fn from(i: &Issue) -> Self {
        IssueView {
            title: i.title.clone(),
            number: i.number,
            author: i.author.clone(),
            body: i.body.clone(),
            state: i.state.clone(),
            state_reason: i.state_reason.clone(),
            closed_by: i.closed_by.clone(),
            closed_at: i.closed_at,
            created_at: i.created_at,
            comments: i.comments,
        }
    }
}

pub struct TimelineItem {
    pub event: IssueEvent,
    pub html: String,
}

#[derive(Template)]
#[template(path = "repo/issue.html")]
struct IssuePage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    view: IssueView,
    labels: Vec<(String, String)>,
    body_html: String,
    timeline: Vec<TimelineItem>,
    can_manage: bool,
    label_names: String,
}

pub async fn show(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let issue = db::issue_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    let can_manage = ctx
        .user
        .as_ref()
        .is_some_and(|u| can_manage(&r, &issue, u.id));
    let timeline = db::list_issue_events(&state.db, issue.id)
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
        .collect();
    let body_html = markdown::render(&issue.body, None);
    let labels = issue.label_list();
    let label_names = label_names(&issue);
    render(&IssuePage {
        ctx,
        r,
        tab: "issues",
        view: IssueView::from(&issue),
        labels,
        body_html,
        timeline,
        can_manage,
        label_names,
    })
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CommentForm {
    #[serde(default)]
    body: String,
    /// Present when the form's "Comment and close" button was used.
    close: Option<String>,
    /// Close reason when closed with the comment: `completed` or `not_planned`.
    reason: Option<String>,
}

fn close_reason(raw: Option<&str>) -> Option<&'static str> {
    match raw {
        Some("not_planned") => Some("not_planned"),
        Some("completed") => Some("completed"),
        _ => None,
    }
}

pub async fn comment(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<CommentForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let issue = db::issue_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    let url = format!("{}/issues/{number}", r.base());
    let body = f.body.trim();
    if !body.is_empty() {
        db::add_issue_event(&state.db, issue.id, Some(user.id), "comment", body).await?;
    }
    if f.close.is_some() && issue.is_open() {
        if !can_manage(&r, &issue, user.id) {
            return Err(AppError::Forbidden(
                "Only the author or a collaborator can close this issue.".into(),
            ));
        }
        close_issue_row(&state, &issue, user.id, close_reason(f.reason.as_deref())).await?;
        return Ok(redirect(&url));
    }
    if body.is_empty() {
        return Ok(redirect_flash(&url, "error", "Comment cannot be empty."));
    }
    Ok(redirect(&url))
}

async fn close_issue_row(
    state: &AppState,
    issue: &Issue,
    user_id: i64,
    reason: Option<&str>,
) -> Result<(), AppError> {
    db::close_issue(&state.db, issue.id, user_id, reason).await?;
    db::add_issue_event(
        &state.db,
        issue.id,
        Some(user_id),
        "closed",
        reason.unwrap_or(""),
    )
    .await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct StateForm {
    action: String,
    reason: Option<String>,
}

pub async fn set_state(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<StateForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let issue = db::issue_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    let url = format!("{}/issues/{number}", r.base());
    if !can_manage(&r, &issue, user.id) {
        return Err(AppError::Forbidden(
            "Only the author or a collaborator can change this issue.".into(),
        ));
    }
    match f.action.as_str() {
        "close" if issue.is_open() => {
            close_issue_row(&state, &issue, user.id, close_reason(f.reason.as_deref())).await?;
        }
        "reopen" if !issue.is_open() => {
            db::reopen_issue(&state.db, issue.id).await?;
            db::add_issue_event(&state.db, issue.id, Some(user.id), "reopened", "").await?;
        }
        _ => {}
    }
    Ok(redirect(&url))
}

#[derive(Deserialize)]
pub struct EditForm {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    labels: String,
}

pub async fn edit(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<EditForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let issue = db::issue_by_number(&state.db, r.repo.id, number)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage(&r, &issue, user.id) {
        return Err(AppError::Forbidden(
            "Only the author or a collaborator can edit this issue.".into(),
        ));
    }
    let title = f.title.trim().chars().take(256).collect::<String>();
    if title.is_empty() {
        return Ok(redirect_flash(
            &format!("{}/issues/{number}", r.base()),
            "error",
            "An issue needs a title.",
        ));
    }
    let renamed = title != issue.title;
    db::update_issue_text(&state.db, issue.id, &title, f.body.trim()).await?;
    if renamed {
        db::add_issue_event(&state.db, issue.id, Some(user.id), "renamed", &title).await?;
    }
    set_labels(&state, r.repo.id, issue.id, &parse_labels(&f.labels)).await?;
    Ok(redirect(&format!("{}/issues/{number}", r.base())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_trimmed_and_deduped() {
        assert_eq!(
            parse_labels("bug, help  wanted ,, Bug ,good first issue"),
            vec!["bug", "help wanted", "good first issue"]
        );
    }

    #[test]
    fn labels_are_capped() {
        let many: Vec<String> = (0..30).map(|i| format!("l{i}")).collect();
        assert_eq!(parse_labels(&many.join(",")).len(), 20);
    }

    #[test]
    fn long_label_names_are_truncated() {
        let name = "x".repeat(120);
        assert_eq!(parse_labels(&name)[0].len(), 50);
    }

    #[test]
    fn empty_label_input() {
        assert!(parse_labels("   ,  , ").is_empty());
    }

    #[test]
    fn close_reason_only_accepts_known_values() {
        assert_eq!(close_reason(Some("completed")), Some("completed"));
        assert_eq!(close_reason(Some("not_planned")), Some("not_planned"));
        assert_eq!(close_reason(Some("bogus")), None);
        assert_eq!(close_reason(None), None);
    }
}
