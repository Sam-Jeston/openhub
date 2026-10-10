//! Per-request context: the signed-in user, flash messages, CSRF protection
//! and error page rendering.

use std::sync::Arc;

use askama::Template;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::{self, HeaderValue};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

use super::error::{AppError, ErrorPage};
use crate::db::{self, User};
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "openhub_session";
const FLASH_COOKIE: &str = "openhub_flash";
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;

#[derive(Debug, Clone)]
pub struct Flash {
    pub kind: String,
    pub message: String,
}

/// Available to every handler and template.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub user: Option<Arc<User>>,
    pub flash: Option<Flash>,
    pub path: String,
    pub session_hash: Option<String>,
}

impl Ctx {
    pub fn require_user(&self) -> Result<&User, AppError> {
        self.user
            .as_deref()
            .ok_or_else(|| AppError::LoginRequired(self.path.clone()))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Ctx {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Ctx>()
            .cloned()
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("request context missing")))
    }
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn session_cookie(token: String, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(time_duration(SESSION_TTL_SECS))
        .build()
}

pub fn clear_session_cookie() -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, ""))
        .path("/")
        .max_age(time_duration(0))
        .build()
}

fn time_duration(secs: i64) -> cookie::time::Duration {
    cookie::time::Duration::seconds(secs)
}

pub fn flash_cookie(kind: &str, message: &str) -> Cookie<'static> {
    let value = URL_SAFE_NO_PAD.encode(format!("{kind}\n{message}"));
    Cookie::build((FLASH_COOKIE, value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .build()
}

fn decode_flash(value: &str) -> Option<Flash> {
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (kind, message) = text.split_once('\n')?;
    let kind = match kind {
        "success" | "error" | "info" => kind.to_string(),
        _ => return None,
    };
    Some(Flash {
        kind,
        message: message.to_string(),
    })
}

fn is_git_request(path: &str) -> bool {
    path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
}

/// Rejects cross-site form posts. SameSite=Lax cookies already prevent
/// authenticated cross-site requests in modern browsers; this is a second
/// line of defence based on the Origin header. Requests are accepted when the
/// origin matches either the configured public URL or the Host header (reverse
/// proxies do not always preserve the latter).
fn cross_origin(req: &Request, public_url: &str) -> bool {
    let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    if origin.eq_ignore_ascii_case(public_url) {
        return false;
    }
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let origin_host = origin.split_once("://").map(|(_, h)| h).unwrap_or(origin);
    origin == "null" || !origin_host.eq_ignore_ascii_case(host)
}

/// `https://git.example.com/sub` → `https://git.example.com`
fn origin_of(url: &str) -> &str {
    let after_scheme = url.find("://").map(|i| i + 3).unwrap_or(0);
    match url[after_scheme..].find('/') {
        Some(i) => &url[..after_scheme + i],
        None => url,
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    ctx: Ctx,
    title: &'static str,
    message: String,
    status: u16,
}

pub async fn middleware(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if path.starts_with("/static/") || path == "/favicon.svg" {
        return next.run(req).await;
    }
    let git = is_git_request(&path);
    if !git
        && !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        && cross_origin(&req, origin_of(&state.config.public_url))
    {
        return (StatusCode::FORBIDDEN, "Cross-origin request rejected").into_response();
    }

    let jar = CookieJar::from_headers(req.headers());
    let mut user = None;
    let mut session_hash = None;
    if !git {
        if let Some(cookie) = jar.get(SESSION_COOKIE) {
            let hash = hash_token(cookie.value());
            match db::user_by_session(&state.db, &hash).await {
                Ok(Some(u)) => {
                    user = Some(Arc::new(u));
                    session_hash = Some(hash);
                }
                Ok(None) => {}
                Err(e) => tracing::error!("session lookup failed: {e}"),
            }
        }
    }
    let flash = jar.get(FLASH_COOKIE).and_then(|c| decode_flash(c.value()));
    let had_flash = jar.get(FLASH_COOKIE).is_some();

    let ctx = Ctx {
        user,
        flash,
        path,
        session_hash,
    };
    req.extensions_mut().insert(ctx.clone());
    let mut resp = next.run(req).await;

    if let Some(page) = resp.extensions_mut().remove::<ErrorPage>() {
        let tpl = ErrorTemplate {
            ctx: ctx.clone(),
            title: page.title,
            message: page.message,
            status: page.status.as_u16(),
        };
        let body = tpl.render().unwrap_or_else(|_| page.title.to_string());
        resp = (page.status, Html(body)).into_response();
    }

    let headers = resp.headers_mut();
    // Consume the flash message unless this response sets a new one.
    let sets_flash = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|v| v.to_str().is_ok_and(|s| s.starts_with(FLASH_COOKIE)));
    if had_flash && !sets_flash {
        let clear = Cookie::build((FLASH_COOKIE, ""))
            .path("/")
            .max_age(time_duration(0))
            .build();
        if let Ok(v) = HeaderValue::from_str(&clear.to_string()) {
            headers.append(header::SET_COOKIE, v);
        }
    }
    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if is_html {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; img-src 'self' data: https:; style-src 'self' 'unsafe-inline'; \
                 script-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'",
            ),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(origin: Option<&str>, host: &str) -> Request {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/x")
            .header(header::HOST, host);
        if let Some(o) = origin {
            builder = builder.header(header::ORIGIN, o);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn origin_checks() {
        let public = origin_of("https://git.example.com/");
        assert_eq!(public, "https://git.example.com");
        assert!(!cross_origin(&post(None, "localhost:3000"), public));
        assert!(!cross_origin(
            &post(Some("http://localhost:3000"), "localhost:3000"),
            public
        ));
        // Behind a proxy that rewrites Host.
        assert!(!cross_origin(
            &post(Some("https://git.example.com"), "127.0.0.1:3000"),
            public
        ));
        assert!(cross_origin(
            &post(Some("https://evil.example"), "localhost:3000"),
            public
        ));
        assert!(cross_origin(&post(Some("null"), "localhost:3000"), public));
    }
}
