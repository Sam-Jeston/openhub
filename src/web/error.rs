use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};

/// Errors returned by page handlers. The context middleware turns these into
/// full HTML error pages.
#[derive(Debug)]
pub enum AppError {
    NotFound,
    Forbidden(String),
    BadRequest(String),
    /// Redirects to the sign-in page, returning to the given path afterwards.
    LoginRequired(String),
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError::Internal(e.into())
    }
}

/// Marker attached to error responses so the middleware can render them.
#[derive(Debug, Clone)]
pub struct ErrorPage {
    pub status: StatusCode,
    pub title: &'static str,
    pub message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, title, message) = match self {
            AppError::LoginRequired(next) => {
                let target = format!("/login?next={}", super::fmt::urlencode(&next));
                return Redirect::to(&target).into_response();
            }
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                "Page not found",
                "The page you are looking for does not exist, or you do not have access to it."
                    .to_string(),
            ),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, "Access denied", msg),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "Bad request", msg),
            AppError::Internal(err) => {
                tracing::error!("internal error: {err:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong",
                    "An unexpected error occurred. Please try again.".to_string(),
                )
            }
        };
        let mut resp = (status, format!("{title}: {message}")).into_response();
        resp.extensions_mut().insert(ErrorPage {
            status,
            title,
            message,
        });
        resp
    }
}
