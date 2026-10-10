//! Git "smart HTTP" protocol for cloning and fetching.
//!
//! Public repositories can be cloned anonymously; private ones accept HTTP
//! basic auth with the account password. Pushing is only offered over SSH,
//! where access is controlled by SSH keys.

use std::io::Read;
use std::process::Stdio;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

use super::account::verify_password;
use crate::db::{self, Repo};
use crate::state::AppState;

fn plain(status: StatusCode, message: &str) -> Response {
    (status, format!("{message}\n")).into_response()
}

fn unauthorized() -> Response {
    let mut resp = plain(StatusCode::UNAUTHORIZED, "Authentication required");
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Openhub\""),
    );
    resp
}

/// Resolves the repository and checks read access, returning an HTTP error
/// response (401 or 404) when the request may not proceed.
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<Repo, Response> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    let not_found = || plain(StatusCode::NOT_FOUND, "Repository not found");
    let repo = match db::repo_by_name(&state.db, owner, name).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(not_found()),
        Err(e) => {
            tracing::error!("repository lookup failed: {e}");
            return Err(plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"));
        }
    };
    if !repo.is_private {
        return Ok(repo);
    }
    let Some((login, password)) = basic_auth(headers) else {
        return Err(unauthorized());
    };
    let user = match db::user_by_login(&state.db, &login).await {
        Ok(Some(u)) => u,
        _ => return Err(unauthorized()),
    };
    if !verify_password(password, user.password_hash.clone()).await {
        return Err(unauthorized());
    }
    match state.access(&repo, Some(&user)).await {
        Ok(a) if a.read => Ok(repo),
        _ => Err(not_found()),
    }
}

fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

fn protocol_v2(headers: &HeaderMap) -> Option<String> {
    headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "=:.".contains(c))
        })
        .map(str::to_string)
}

fn pkt_line(s: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", s.len() + 4).into_bytes();
    out.extend_from_slice(s.as_bytes());
    out
}

#[derive(Deserialize)]
pub struct InfoRefsQuery {
    service: Option<String>,
}

pub async fn info_refs(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Response {
    match q.service.as_deref() {
        Some("git-upload-pack") => {}
        Some("git-receive-pack") => {
            return plain(
                StatusCode::FORBIDDEN,
                "Pushing over HTTP is not supported. Add an SSH key in your Openhub settings and push over SSH.",
            );
        }
        _ => {
            return plain(
                StatusCode::FORBIDDEN,
                "Only the smart HTTP protocol is supported",
            );
        }
    }
    let repo = match authorize(&state, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let path = state.config.repo_path(repo.id);
    let protocol = protocol_v2(&headers);
    let mut cmd = Command::new(&state.config.git_bin);
    cmd.args(["upload-pack", "--stateless-rpc", "--advertise-refs"])
        .arg(&path)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(p) = &protocol {
        cmd.env("GIT_PROTOCOL", p);
    }
    let output = match cmd.output().await {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            tracing::error!("git upload-pack --advertise-refs failed: {}", o.status);
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "git failed");
        }
        Err(e) => {
            tracing::error!("could not run git: {e}");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "git is not available");
        }
    };
    let mut body = Vec::with_capacity(output.len() + 64);
    // Protocol v2 responses start directly with the capability advertisement.
    if !protocol.as_deref().is_some_and(|p| p.contains("version=2")) {
        body.extend(pkt_line("# service=git-upload-pack\n"));
        body.extend_from_slice(b"0000");
    }
    body.extend(output);
    (
        [
            (
                header::CONTENT_TYPE,
                "application/x-git-upload-pack-advertisement",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub async fn upload_pack(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let repo = match authorize(&state, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let gzipped = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"));
    let input = if gzipped {
        let mut out = Vec::new();
        if flate2::read::GzDecoder::new(&body[..])
            .read_to_end(&mut out)
            .is_err()
        {
            return plain(StatusCode::BAD_REQUEST, "Invalid gzip body");
        }
        out
    } else {
        body.to_vec()
    };

    let path = state.config.repo_path(repo.id);
    let mut cmd = Command::new(&state.config.git_bin);
    cmd.args(["upload-pack", "--stateless-rpc"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(p) = protocol_v2(&headers) {
        cmd.env("GIT_PROTOCOL", p);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("could not run git: {e}");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "git is not available");
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
        drop(stdin);
        let _ = child.wait().await;
    });
    (
        [
            (header::CONTENT_TYPE, "application/x-git-upload-pack-result"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(ReaderStream::new(stdout)),
    )
        .into_response()
}

pub async fn receive_pack() -> Response {
    plain(
        StatusCode::FORBIDDEN,
        "Pushing over HTTP is not supported. Push over SSH instead.",
    )
}
