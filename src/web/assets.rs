//! Static assets compiled into the binary.

use axum::http::header;
use axum::response::IntoResponse;

const STYLE: &str = include_str!("../../static/style.css");
const SCRIPT: &str = include_str!("../../static/app.js");
const FAVICON: &str = include_str!("../../static/favicon.svg");

const CACHE: &str = "public, max-age=3600";

pub async fn stylesheet() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE),
        ],
        STYLE,
    )
}

pub async fn script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE),
        ],
        SCRIPT,
    )
}

pub async fn favicon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, CACHE),
        ],
        FAVICON,
    )
}

/// Short content hash appended to asset URLs so browsers pick up new versions.
pub fn version() -> &'static str {
    use sha2::{Digest, Sha256};
    use std::sync::LazyLock;
    static VERSION: LazyLock<String> = LazyLock::new(|| {
        let digest = Sha256::new()
            .chain_update(STYLE)
            .chain_update(SCRIPT)
            .finalize();
        hex::encode(&digest[..4])
    });
    &VERSION
}
