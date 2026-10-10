//! Sign up, sign in, profile settings and SSH key management.

use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use super::ctx::{self, Ctx, SESSION_TTL_SECS};
use super::{AppError, WebResult, redirect, redirect_flash, render, safe_next, with_cookie};
use crate::db::{self, SshKey};
use crate::state::AppState;
use crate::validate;

/// Verified against when an account does not exist, to keep timing uniform.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Argon2::default()
        .hash_password(b"openhub-dummy-password", &salt)
        .map(|h| h.to_string())
        .unwrap_or_default()
});

pub async fn hash_password(password: String) -> Result<String, AppError> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut rand::rngs::OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| AppError::Internal(anyhow::anyhow!("hashing password: {e}")))
    })
    .await?
}

pub async fn verify_password(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|parsed| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

async fn start_session(state: &AppState, user_id: i64, to: &str) -> WebResult {
    let token = ctx::new_token();
    db::create_session(
        &state.db,
        &ctx::hash_token(&token),
        user_id,
        SESSION_TTL_SECS,
    )
    .await?;
    let _ = db::purge_expired_sessions(&state.db).await;
    Ok(with_cookie(
        redirect(to),
        ctx::session_cookie(token, state.config.secure_cookies()),
    ))
}

// ---------------------------------------------------------------------------
// Sign in / sign up
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    ctx: Ctx,
    login: String,
    next: String,
    error: Option<String>,
    can_register: bool,
}

#[derive(Deserialize)]
pub struct NextQuery {
    next: Option<String>,
}

async fn can_register(state: &AppState) -> Result<bool, AppError> {
    Ok(!state.config.disable_registration || db::count_users(&state.db).await? == 0)
}

pub async fn login_form(
    State(state): State<AppState>,
    ctx: Ctx,
    Query(q): Query<NextQuery>,
) -> WebResult {
    if ctx.user.is_some() {
        return Ok(redirect(&safe_next(q.next.as_deref())));
    }
    render(&LoginPage {
        ctx,
        login: String::new(),
        next: safe_next(q.next.as_deref()),
        error: None,
        can_register: can_register(&state).await?,
    })
}

#[derive(Deserialize)]
pub struct LoginForm {
    login: String,
    password: String,
    next: Option<String>,
}

pub async fn login(State(state): State<AppState>, ctx: Ctx, Form(f): Form<LoginForm>) -> WebResult {
    let next = safe_next(f.next.as_deref());
    let user = db::user_by_login(&state.db, f.login.trim()).await?;
    // Always run a verification so response timing does not reveal whether the account exists.
    let hash = user
        .as_ref()
        .map(|u| u.password_hash.clone())
        .unwrap_or_else(|| DUMMY_HASH.clone());
    let ok = verify_password(f.password, hash).await;
    match user {
        Some(u) if ok => start_session(&state, u.id, &next).await,
        _ => render(&LoginPage {
            ctx,
            login: f.login,
            next,
            error: Some("Incorrect username or password.".into()),
            can_register: can_register(&state).await?,
        }),
    }
}

#[derive(Template)]
#[template(path = "register.html")]
struct RegisterPage {
    ctx: Ctx,
    username: String,
    email: String,
    error: Option<String>,
    allowed: bool,
    first_user: bool,
}

pub async fn register_form(State(state): State<AppState>, ctx: Ctx) -> WebResult {
    if ctx.user.is_some() {
        return Ok(redirect("/"));
    }
    let users = db::count_users(&state.db).await?;
    render(&RegisterPage {
        ctx,
        username: String::new(),
        email: String::new(),
        error: None,
        allowed: can_register(&state).await?,
        first_user: users == 0,
    })
}

#[derive(Deserialize)]
pub struct RegisterForm {
    username: String,
    email: String,
    password: String,
}

pub async fn register(
    State(state): State<AppState>,
    ctx: Ctx,
    Form(f): Form<RegisterForm>,
) -> WebResult {
    let users = db::count_users(&state.db).await?;
    if !can_register(&state).await? {
        return Err(AppError::Forbidden(
            "Registration is disabled on this server.".into(),
        ));
    }
    let username = f.username.trim().to_string();
    let email = f.email.trim().to_string();
    let mut error = validate::username(&username)
        .and(validate::email(&email))
        .and(validate::password(&f.password))
        .err()
        .map(str::to_string);
    if error.is_none() && db::user_by_username(&state.db, &username).await?.is_some() {
        error = Some("That username is already taken.".into());
    }
    if error.is_none() && db::email_taken(&state.db, &email, -1).await? {
        error = Some("An account with that email already exists.".into());
    }
    if let Some(error) = error {
        return render(&RegisterPage {
            ctx,
            username,
            email,
            error: Some(error),
            allowed: true,
            first_user: users == 0,
        });
    }
    let hash = hash_password(f.password).await?;
    let id = db::create_user(&state.db, &username, &email, &hash, users == 0).await?;
    let resp = start_session(&state, id, "/settings/keys").await?;
    Ok(with_cookie(
        resp,
        ctx::flash_cookie(
            "success",
            "Welcome to Openhub! Add an SSH key so you can push code.",
        ),
    ))
}

pub async fn logout(State(state): State<AppState>, ctx: Ctx) -> WebResult {
    if let Some(hash) = &ctx.session_hash {
        db::delete_session(&state.db, hash).await?;
    }
    Ok(with_cookie(redirect("/"), ctx::clear_session_cookie()))
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

pub async fn settings_redirect() -> WebResult {
    Ok(redirect("/settings/profile"))
}

#[derive(Template)]
#[template(path = "settings/profile.html")]
struct ProfilePage {
    ctx: Ctx,
    tab: &'static str,
    display_name: String,
    email: String,
    bio: String,
    error: Option<String>,
    password_error: Option<String>,
}

pub async fn profile_form(ctx: Ctx) -> WebResult {
    let user = ctx.require_user()?.clone();
    render(&ProfilePage {
        ctx,
        tab: "profile",
        display_name: user.display_name,
        email: user.email,
        bio: user.bio,
        error: None,
        password_error: None,
    })
}

#[derive(Deserialize)]
pub struct ProfileForm {
    #[serde(default)]
    display_name: String,
    email: String,
    #[serde(default)]
    bio: String,
}

pub async fn update_profile(
    State(state): State<AppState>,
    ctx: Ctx,
    Form(f): Form<ProfileForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let display_name = f.display_name.trim().chars().take(100).collect::<String>();
    let bio = f.bio.trim().chars().take(500).collect::<String>();
    let email = f.email.trim().to_string();
    let mut error = validate::email(&email).err().map(str::to_string);
    if error.is_none() && db::email_taken(&state.db, &email, user.id).await? {
        error = Some("That email is used by another account.".into());
    }
    if let Some(error) = error {
        return render(&ProfilePage {
            ctx,
            tab: "profile",
            display_name,
            email,
            bio,
            error: Some(error),
            password_error: None,
        });
    }
    db::update_profile(&state.db, user.id, &display_name, &email, &bio).await?;
    Ok(redirect_flash(
        "/settings/profile",
        "success",
        "Profile updated.",
    ))
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current: String,
    new: String,
    confirm: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    ctx: Ctx,
    Form(f): Form<PasswordForm>,
) -> WebResult {
    let user = ctx.require_user()?.clone();
    let mut error = None;
    if !verify_password(f.current, user.password_hash.clone()).await {
        error = Some("Current password is incorrect.".to_string());
    } else if let Err(e) = validate::password(&f.new) {
        error = Some(e.to_string());
    } else if f.new != f.confirm {
        error = Some("New passwords do not match.".to_string());
    }
    if let Some(error) = error {
        return render(&ProfilePage {
            ctx,
            tab: "profile",
            display_name: user.display_name,
            email: user.email,
            bio: user.bio,
            error: None,
            password_error: Some(error),
        });
    }
    let hash = hash_password(f.new).await?;
    db::update_password(&state.db, user.id, &hash).await?;
    if let Some(keep) = &ctx.session_hash {
        db::delete_other_sessions(&state.db, user.id, keep).await?;
    }
    Ok(redirect_flash(
        "/settings/profile",
        "success",
        "Password changed. Other sessions have been signed out.",
    ))
}

#[derive(Template)]
#[template(path = "settings/keys.html")]
struct KeysPage {
    ctx: Ctx,
    tab: &'static str,
    keys: Vec<SshKey>,
    test_command: String,
    title: String,
    key: String,
    error: Option<String>,
}

async fn keys_page(
    state: &AppState,
    ctx: Ctx,
    title: String,
    key: String,
    error: Option<String>,
) -> WebResult {
    let user = ctx.require_user()?;
    let keys = db::list_keys(&state.db, user.id).await?;
    render(&KeysPage {
        ctx,
        tab: "keys",
        keys,
        test_command: state.config.ssh_test_command(),
        title,
        key,
        error,
    })
}

pub async fn keys(State(state): State<AppState>, ctx: Ctx) -> WebResult {
    keys_page(&state, ctx, String::new(), String::new(), None).await
}

#[derive(Deserialize)]
pub struct KeyForm {
    #[serde(default)]
    title: String,
    key: String,
}

pub async fn add_key(State(state): State<AppState>, ctx: Ctx, Form(f): Form<KeyForm>) -> WebResult {
    let user = ctx.require_user()?.clone();
    let parsed = match validate::ssh_public_key(&f.key) {
        Ok(p) => p,
        Err(e) => return keys_page(&state, ctx, f.title, f.key, Some(e.into())).await,
    };
    if db::fingerprint_exists(&state.db, &parsed.fingerprint).await? {
        return keys_page(
            &state,
            ctx,
            f.title,
            f.key,
            Some("This key is already in use by an account.".into()),
        )
        .await;
    }
    let title = match f.title.trim() {
        "" if !parsed.comment.is_empty() => parsed.comment.clone(),
        "" => parsed.key_type.clone(),
        t => t.chars().take(100).collect(),
    };
    db::add_key(
        &state.db,
        user.id,
        &title,
        &parsed.fingerprint,
        &parsed.normalized,
        &parsed.key_type,
    )
    .await?;
    Ok(redirect_flash(
        "/settings/keys",
        "success",
        &format!("SSH key \u{201c}{title}\u{201d} added."),
    ))
}

pub async fn delete_key(State(state): State<AppState>, ctx: Ctx, Path(id): Path<i64>) -> WebResult {
    let user = ctx.require_user()?;
    if !db::delete_key(&state.db, id, user.id).await? {
        return Err(AppError::NotFound);
    }
    Ok(redirect_flash(
        "/settings/keys",
        "success",
        "SSH key deleted.",
    ))
}
