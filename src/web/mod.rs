mod account;
pub mod assets;
mod code;
pub mod ctx;
pub mod error;
pub mod fmt;
mod git_http;
mod home;
mod issues;
mod pulls;
mod repo;

use askama::Template;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::Cookie;
use tower_http::trace::TraceLayer;

use crate::state::AppState;
pub use ctx::Ctx;
pub use error::AppError;

pub type WebResult = Result<Response, AppError>;

/// Fetch negotiation requests list every commit the client already has.
const GIT_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home::index))
        .route("/explore", get(home::explore))
        .route("/login", get(account::login_form).post(account::login))
        .route(
            "/register",
            get(account::register_form).post(account::register),
        )
        .route("/logout", post(account::logout))
        .route("/new", get(repo::new_form).post(repo::create))
        .route("/settings", get(account::settings_redirect))
        .route(
            "/settings/profile",
            get(account::profile_form).post(account::update_profile),
        )
        .route("/settings/password", post(account::change_password))
        .route("/settings/keys", get(account::keys).post(account::add_key))
        .route("/settings/keys/{id}/delete", post(account::delete_key))
        .route("/static/style.css", get(assets::stylesheet))
        .route("/static/app.js", get(assets::script))
        .route("/favicon.svg", get(assets::favicon))
        .route("/{owner}", get(home::profile))
        .route("/{owner}/{repo}", get(code::repo_home))
        .route("/{owner}/{repo}/tree/{*rest}", get(code::tree))
        .route("/{owner}/{repo}/blob/{*rest}", get(code::blob))
        .route("/{owner}/{repo}/raw/{*rest}", get(code::raw))
        .route("/{owner}/{repo}/archive/{*file}", get(code::archive))
        .route("/{owner}/{repo}/commits", get(code::commits_default))
        .route("/{owner}/{repo}/commits/{*rest}", get(code::commits))
        .route("/{owner}/{repo}/commit/{sha}", get(code::commit))
        .route("/{owner}/{repo}/branches", get(repo::branches))
        .route("/{owner}/{repo}/branches/delete", post(repo::delete_branch))
        .route("/{owner}/{repo}/tags", get(repo::tags))
        .route("/{owner}/{repo}/compare", get(pulls::compare_default))
        .route("/{owner}/{repo}/compare/{*spec}", get(pulls::compare))
        .route(
            "/{owner}/{repo}/pulls",
            get(pulls::list).post(pulls::create),
        )
        .route("/{owner}/{repo}/pull/{number}", get(pulls::conversation))
        .route("/{owner}/{repo}/pull/{number}/commits", get(pulls::commits))
        .route("/{owner}/{repo}/pull/{number}/files", get(pulls::files))
        .route(
            "/{owner}/{repo}/pull/{number}/comment",
            post(pulls::comment),
        )
        .route("/{owner}/{repo}/pull/{number}/merge", post(pulls::merge))
        .route(
            "/{owner}/{repo}/pull/{number}/state",
            post(pulls::set_state),
        )
        .route("/{owner}/{repo}/pull/{number}/edit", post(pulls::edit))
        .route(
            "/{owner}/{repo}/issues",
            get(issues::list).post(issues::create),
        )
        .route("/{owner}/{repo}/issues/new", get(issues::new_form))
        .route("/{owner}/{repo}/issue/{number}", get(issues::show))
        .route(
            "/{owner}/{repo}/issue/{number}/comment",
            post(issues::comment),
        )
        .route(
            "/{owner}/{repo}/issue/{number}/state",
            post(issues::set_state),
        )
        .route("/{owner}/{repo}/issue/{number}/edit", post(issues::edit))
        .route(
            "/{owner}/{repo}/settings",
            get(repo::settings).post(repo::update_settings),
        )
        .route(
            "/{owner}/{repo}/settings/collaborators",
            post(repo::add_collaborator),
        )
        .route(
            "/{owner}/{repo}/settings/collaborators/remove",
            post(repo::remove_collaborator),
        )
        .route("/{owner}/{repo}/settings/delete", post(repo::delete))
        .route("/{owner}/{repo}/info/refs", get(git_http::info_refs))
        .route(
            "/{owner}/{repo}/git-upload-pack",
            post(git_http::upload_pack).layer(DefaultBodyLimit::max(GIT_BODY_LIMIT)),
        )
        .route(
            "/{owner}/{repo}/git-receive-pack",
            post(git_http::receive_pack),
        )
        .fallback(|| async { AppError::NotFound })
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            ctx::middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let addr = state.config.http_addr;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("web interface listening on http://{addr}");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

pub fn render<T: Template>(template: &T) -> WebResult {
    Ok(Html(template.render()?).into_response())
}

pub fn redirect(to: &str) -> Response {
    let mut resp = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp
}

pub fn with_cookie(mut resp: Response, cookie: Cookie<'_>) -> Response {
    if let Ok(v) = HeaderValue::from_str(&cookie.to_string()) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

/// Redirect that shows a one-off message on the next page.
pub fn redirect_flash(to: &str, kind: &str, message: &str) -> Response {
    with_cookie(redirect(to), ctx::flash_cookie(kind, message))
}

/// Runs blocking repository work off the async runtime.
pub async fn blocking<T, F>(f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
}

/// Returns `next` if it is a safe same-site path, otherwise `/`.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\") => {
            n.to_string()
        }
        _ => "/".to_string(),
    }
}
