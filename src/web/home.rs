//! Dashboard, explore and user profile pages.

use askama::Template;
use axum::extract::{Path, State};

use super::{AppError, Ctx, WebResult, render};
use crate::db::{self, PullRequest, Repo, User};
use crate::state::AppState;

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    ctx: Ctx,
    repos: Vec<Repo>,
    pulls: Vec<(PullRequest, String, String)>,
    recent: Vec<Repo>,
}

#[derive(Template)]
#[template(path = "landing.html")]
struct LandingPage {
    ctx: Ctx,
    repos: Vec<Repo>,
    can_register: bool,
}

pub async fn index(State(state): State<AppState>, ctx: Ctx) -> WebResult {
    match ctx.user.clone() {
        Some(user) => {
            let repos = db::list_member_repos(&state.db, user.id).await?;
            let pulls = db::dashboard_pulls(&state.db, user.id).await?;
            let recent = db::list_public_repos(&state.db, 10)
                .await?
                .into_iter()
                .filter(|r| r.owner_id != user.id)
                .collect();
            render(&DashboardPage {
                ctx,
                repos,
                pulls,
                recent,
            })
        }
        None => {
            let repos = db::list_public_repos(&state.db, 12).await?;
            let can_register =
                !state.config.disable_registration || db::count_users(&state.db).await? == 0;
            render(&LandingPage {
                ctx,
                repos,
                can_register,
            })
        }
    }
}

#[derive(Template)]
#[template(path = "explore.html")]
struct ExplorePage {
    ctx: Ctx,
    repos: Vec<Repo>,
}

pub async fn explore(State(state): State<AppState>, ctx: Ctx) -> WebResult {
    let repos = db::list_public_repos(&state.db, 200).await?;
    render(&ExplorePage { ctx, repos })
}

#[derive(Template)]
#[template(path = "profile.html")]
struct ProfilePage {
    ctx: Ctx,
    profile: User,
    repos: Vec<Repo>,
    is_self: bool,
}

pub async fn profile(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(owner): Path<String>,
) -> WebResult {
    let profile = db::user_by_username(&state.db, &owner)
        .await?
        .ok_or(AppError::NotFound)?;
    let repos = db::list_owner_repos(&state.db, profile.id, ctx.user.as_deref()).await?;
    let is_self = ctx.user.as_ref().is_some_and(|u| u.id == profile.id);
    render(&ProfilePage {
        ctx,
        profile,
        repos,
        is_self,
    })
}
