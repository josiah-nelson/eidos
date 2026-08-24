//! Web UI delivery: the built `web/dist` embedded in the executable, or a
//! directory on disk for development.
//!
//! The embedded copy makes `eidos.exe` self-contained (a single file to
//! install, sign, or carry on a USB stick). It is captured at compile time;
//! `build.rs` re-runs the build when `web/dist` changes and can require it
//! for packaged builds (`EIDOS_REQUIRE_WEB=1`).

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use std::path::PathBuf;

/// Where the service takes its web UI from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebAssets {
    /// The `web/dist` captured at compile time.
    Embedded,
    /// A directory on disk (development: `--web-dir web/dist`).
    Directory(PathBuf),
    /// API only.
    Disabled,
}

#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Dist;

/// True when the executable carries a usable web UI.
pub fn embedded_available() -> bool {
    Dist::get("index.html").is_some()
}

/// Number of files embedded (zero when built without `web/dist`).
pub fn embedded_file_count() -> usize {
    Dist::iter().count()
}

/// Attach the chosen web UI as the router's fallback service.
pub(crate) fn mount(mut app: Router, web: &WebAssets) -> Router {
    match web {
        WebAssets::Embedded => {
            if embedded_available() {
                app = app.fallback(serve_embedded);
                tracing::info!(files = embedded_file_count(), "serving embedded web UI");
            } else {
                tracing::warn!(
                    "this build embeds no web UI; API only (pass --web-dir to serve one from disk)"
                );
            }
        }
        WebAssets::Directory(dir) => {
            if dir.join("index.html").exists() {
                let index = dir.join("index.html");
                // `fallback` (not `not_found_service`) keeps the 200 status so
                // deep links into the SPA are real pages, not 404s with a body.
                let serve = tower_http::services::ServeDir::new(dir)
                    .fallback(tower_http::services::ServeFile::new(index));
                app = app.fallback_service(serve);
                tracing::info!(dir = %dir.display(), "serving web UI from directory");
            } else {
                tracing::warn!(dir = %dir.display(), "web UI directory has no index.html; API only");
            }
        }
        WebAssets::Disabled => {}
    }
    app
}

/// Serve one embedded file. Unknown paths fall back to `index.html` with 200
/// so client-side routes deep-link; hashed `assets/` are immutable.
async fn serve_embedded(req: Request<Body>) -> Response {
    let uri: &Uri = req.uri();
    let raw = uri.path().trim_start_matches('/');
    // Reject anything that is not a plain relative path. rust-embed keys are
    // forward-slash relative paths, so this is defensive rather than
    // load-bearing: there is no filesystem behind the lookup.
    let path = if raw.is_empty() || raw.contains("..") || raw.contains('\\') {
        "index.html"
    } else {
        raw
    };
    let (name, file) = match Dist::get(path) {
        Some(f) => (path, f),
        None => match Dist::get("index.html") {
            Some(f) => ("index.html", f),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let etag = {
        let mut s = String::with_capacity(66);
        s.push('"');
        for b in &file.metadata.sha256_hash() {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s.push('"');
        s
    };
    if req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mime = file.metadata.mimetype();
    let content_type = if mime.starts_with("text/") || mime == "application/javascript" {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };
    let mut resp = Response::new(Body::from(file.data.into_owned()));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_index_matches_availability() {
        // Whether or not this build has a web UI, the two probes agree.
        assert_eq!(embedded_available(), Dist::get("index.html").is_some());
        assert_eq!(
            embedded_available(),
            embedded_file_count() > 0 || !embedded_available()
        );
    }
}
