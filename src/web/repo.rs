//! Repository loading, creation, settings, branches and tags.

use std::collections::HashMap;
use std::path::PathBuf;

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use serde::Deserialize;

use super::{AppError, Ctx, WebResult, blocking, redirect, redirect_flash, render, safe_next};
use crate::db::{self, Repo, User};
use crate::git::{self, BranchInfo, TagInfo};
use crate::state::{Access, AppState};
use crate::validate;

/// A repository the current user is allowed to read, with their permissions.
#[derive(Debug, Clone)]
pub struct RepoCtx {
    pub repo: Repo,
    pub access: Access,
    pub path: PathBuf,
    pub open_pulls: i64,
    pub open_issues: i64,
    pub ssh_url: String,
    pub http_url: String,
}

impl RepoCtx {
    /// URL prefix of the repository, e.g. `/alice/project`.
    pub fn base(&self) -> String {
        format!("/{}/{}", self.repo.owner, self.repo.name)
    }

    pub fn pull_url(&self, number: &i64) -> String {
        format!("{}/pull/{number}", self.base())
    }

    pub fn issue_url(&self, number: &i64) -> String {
        format!("{}/issue/{number}", self.base())
    }

    pub fn require_write(&self) -> Result<(), AppError> {
        if self.access.write {
            Ok(())
        } else {
            Err(AppError::Forbidden(
                "You need write access to this repository to do that.".into(),
            ))
        }
    }

    pub fn require_admin(&self) -> Result<(), AppError> {
        if self.access.admin {
            Ok(())
        } else {
            Err(AppError::Forbidden(
                "Only the repository owner can do that.".into(),
            ))
        }
    }
}

pub async fn load_repo(
    state: &AppState,
    ctx: &Ctx,
    owner: &str,
    name: &str,
) -> Result<RepoCtx, AppError> {
    let repo = db::repo_by_name(&state.db, owner, name)
        .await?
        .ok_or(AppError::NotFound)?;
    let access = state.access(&repo, ctx.user.as_deref()).await?;
    if !access.read {
        // Private repositories are indistinguishable from missing ones.
        return Err(AppError::NotFound);
    }
    let (open_pulls, _) = db::count_pulls(&state.db, repo.id).await?;
    let (open_issues, _) = db::count_issues(&state.db, repo.id).await?;
    Ok(RepoCtx {
        path: state.config.repo_path(repo.id),
        ssh_url: state.config.ssh_clone_url(&repo.owner, &repo.name),
        http_url: state.config.http_clone_url(&repo.owner, &repo.name),
        repo,
        access,
        open_pulls,
        open_issues,
    })
}

// ---------------------------------------------------------------------------
// New repository
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "new_repo.html")]
struct NewRepoPage {
    ctx: Ctx,
    name: String,
    description: String,
    private: bool,
    readme: bool,
    error: Option<String>,
}

pub async fn new_form(ctx: Ctx) -> WebResult {
    ctx.require_user()?;
    render(&NewRepoPage {
        ctx,
        name: String::new(),
        description: String::new(),
        private: false,
        readme: false,
        error: None,
    })
}

#[derive(Deserialize)]
pub struct NewRepoForm {
    name: String,
    #[serde(default)]
    description: String,
    visibility: String,
    readme: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    Form(f): Form<NewRepoForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let name = f.name.trim().to_string();
    let description = f.description.trim().chars().take(350).collect::<String>();
    let private = f.visibility == "private";
    let readme = f.readme.is_some();

    let mut error = validate::repo_name(&name).err().map(str::to_string);
    if error.is_none() && db::repo_name_taken(&state.db, user.id, &name, -1).await? {
        error = Some(format!(
            "You already have a repository named \u{201c}{name}\u{201d}."
        ));
    }
    if let Some(error) = error {
        return render(&NewRepoPage {
            ctx,
            name,
            description,
            private,
            readme,
            error: Some(error),
        });
    }

    let id = db::create_repo(&state.db, user.id, &name, &description, private, "main").await?;
    let path = state.config.repo_path(id);
    let (repo_name, desc) = (name.clone(), description.clone());
    let init = blocking(move || {
        let repo = git::init_bare(&path, "main")?;
        if readme {
            let mut content = format!("# {repo_name}\n");
            if !desc.is_empty() {
                content.push_str(&format!("\n{desc}\n"));
            }
            let sig = git2::Signature::now(user.name(), &user.email)?;
            git::initial_commit(&repo, "main", &[("README.md", content.as_bytes())], &sig)?;
        }
        Ok(())
    })
    .await;
    if let Err(e) = init {
        db::delete_repo(&state.db, id).await?;
        let _ = std::fs::remove_dir_all(state.config.repo_path(id));
        return Err(e);
    }
    Ok(redirect(&format!(
        "/{}/{}",
        ctx.require_user()?.username,
        name
    )))
}

// ---------------------------------------------------------------------------
// Branches and tags
// ---------------------------------------------------------------------------

pub struct BranchRow {
    pub branch: BranchInfo,
    pub pull: Option<i64>,
    pub is_default: bool,
}

#[derive(Template)]
#[template(path = "repo/branches.html")]
struct BranchesPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    rows: Vec<BranchRow>,
}

pub async fn branches(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let path = r.path.clone();
    let default = r.repo.default_branch.clone();
    let list = blocking(move || Ok(git::branches(&git::open(&path)?, &default)?)).await?;
    let pulls: HashMap<String, i64> = db::list_pulls(&state.db, r.repo.id, true)
        .await?
        .into_iter()
        .map(|p| (p.head_branch, p.number))
        .collect();
    let mut rows: Vec<BranchRow> = list
        .into_iter()
        .map(|b| BranchRow {
            pull: pulls.get(&b.name).copied(),
            is_default: b.name == r.repo.default_branch,
            branch: b,
        })
        .collect();
    rows.sort_by_key(|row| !row.is_default);
    render(&BranchesPage {
        ctx,
        r,
        tab: "branches",
        rows,
    })
}

#[derive(Deserialize)]
pub struct DeleteBranchForm {
    branch: String,
    return_to: Option<String>,
    pr: Option<i64>,
}

pub async fn delete_branch(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<DeleteBranchForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    r.require_write()?;
    if f.branch == r.repo.default_branch {
        return Err(AppError::BadRequest(
            "The default branch cannot be deleted.".into(),
        ));
    }
    let path = r.path.clone();
    let branch = f.branch.clone();
    blocking(move || {
        let repo = git::open(&path)?;
        if !git::branch_exists(&repo, &branch) {
            return Err(AppError::NotFound);
        }
        git::delete_branch(&repo, &branch)?;
        Ok(())
    })
    .await?;
    if let Some(number) = f.pr {
        if let Some(pr) = db::pull_by_number(&state.db, r.repo.id, number).await? {
            if pr.head_branch == f.branch {
                db::add_event(&state.db, pr.id, Some(user.id), "branch_deleted", &f.branch).await?;
            }
        }
    }
    let to = f
        .return_to
        .as_deref()
        .map(|t| safe_next(Some(t)))
        .unwrap_or_else(|| format!("{}/branches", r.base()));
    Ok(redirect_flash(
        &to,
        "success",
        &format!("Deleted branch {}.", f.branch),
    ))
}

#[derive(Template)]
#[template(path = "repo/tags.html")]
struct TagsPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    tags: Vec<TagInfo>,
}

pub async fn tags(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    let path = r.path.clone();
    let tags = blocking(move || Ok(git::tags(&git::open(&path)?)?)).await?;
    render(&TagsPage {
        ctx,
        r,
        tab: "code",
        tags,
    })
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/settings.html")]
struct SettingsPage {
    ctx: Ctx,
    r: RepoCtx,
    tab: &'static str,
    branches: Vec<String>,
    collaborators: Vec<User>,
    error: Option<String>,
    collab_error: Option<String>,
}

async fn settings_page(
    state: &AppState,
    ctx: Ctx,
    r: RepoCtx,
    error: Option<String>,
    collab_error: Option<String>,
) -> WebResult {
    r.require_admin()?;
    let path = r.path.clone();
    let branches = blocking(move || Ok(git::branch_names(&git::open(&path)?)?)).await?;
    let collaborators = db::list_collaborators(&state.db, r.repo.id).await?;
    render(&SettingsPage {
        ctx,
        r,
        tab: "settings",
        branches,
        collaborators,
        error,
        collab_error,
    })
}

pub async fn settings(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    settings_page(&state, ctx, r, None, None).await
}

#[derive(Deserialize)]
pub struct SettingsForm {
    name: String,
    #[serde(default)]
    description: String,
    visibility: String,
    default_branch: Option<String>,
}

pub async fn update_settings(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<SettingsForm>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    r.require_admin()?;
    let new_name = f.name.trim().to_string();
    let description = f.description.trim().chars().take(350).collect::<String>();
    let mut error = validate::repo_name(&new_name).err().map(str::to_string);
    if error.is_none()
        && db::repo_name_taken(&state.db, r.repo.owner_id, &new_name, r.repo.id).await?
    {
        error = Some("A repository with that name already exists.".into());
    }
    if let Some(error) = error {
        return settings_page(&state, ctx, r, Some(error), None).await;
    }

    let mut default_branch = r.repo.default_branch.clone();
    if let Some(branch) = f.default_branch.filter(|b| *b != default_branch) {
        let path = r.path.clone();
        let b = branch.clone();
        blocking(move || {
            let repo = git::open(&path)?;
            if !git::branch_exists(&repo, &b) {
                return Err(AppError::BadRequest("That branch does not exist.".into()));
            }
            git::set_head(&repo, &b)?;
            Ok(())
        })
        .await?;
        default_branch = branch;
    }
    db::update_repo(
        &state.db,
        r.repo.id,
        &new_name,
        &description,
        f.visibility == "private",
        &default_branch,
    )
    .await?;
    Ok(redirect_flash(
        &format!("/{}/{}/settings", r.repo.owner, new_name),
        "success",
        "Repository settings saved.",
    ))
}

#[derive(Deserialize)]
pub struct CollaboratorForm {
    username: String,
}

pub async fn add_collaborator(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<CollaboratorForm>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    r.require_admin()?;
    let username = f.username.trim();
    let Some(user) = db::user_by_username(&state.db, username).await? else {
        let msg = format!("No user named \u{201c}{username}\u{201d} exists.");
        return settings_page(&state, ctx, r, None, Some(msg)).await;
    };
    if user.id == r.repo.owner_id {
        let msg = "The owner already has full access.".to_string();
        return settings_page(&state, ctx, r, None, Some(msg)).await;
    }
    db::add_collaborator(&state.db, r.repo.id, user.id).await?;
    Ok(redirect_flash(
        &format!("{}/settings", r.base()),
        "success",
        &format!("{} can now push to this repository.", user.username),
    ))
}

pub async fn remove_collaborator(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<CollaboratorForm>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    r.require_admin()?;
    if let Some(user) = db::user_by_username(&state.db, &f.username).await? {
        db::remove_collaborator(&state.db, r.repo.id, user.id).await?;
    }
    Ok(redirect_flash(
        &format!("{}/settings", r.base()),
        "success",
        "Collaborator removed.",
    ))
}

#[derive(Deserialize)]
pub struct DeleteForm {
    confirm: String,
}

pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<DeleteForm>,
) -> WebResult {
    let r = load_repo(&state, &ctx, &owner, &name).await?;
    r.require_admin()?;
    let full_name = format!("{}/{}", r.repo.owner, r.repo.name);
    if f.confirm.trim() != full_name {
        return settings_page(
            &state,
            ctx,
            r,
            Some(format!("Type {full_name} to confirm deletion.")),
            None,
        )
        .await;
    }
    db::delete_repo(&state.db, r.repo.id).await?;
    if let Err(e) = tokio::fs::remove_dir_all(&r.path).await {
        tracing::warn!("could not remove {}: {e}", r.path.display());
    }
    Ok(redirect_flash(
        "/",
        "success",
        &format!("Deleted {full_name}."),
    ))
}
