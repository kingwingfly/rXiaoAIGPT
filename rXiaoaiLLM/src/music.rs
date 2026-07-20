//! Serving local audio files to the speaker.
//!
//! The speaker can only be told to play a URL, and we only know what the user
//! *said* — not which file they meant. So a request path is treated as a regex
//! and matched against an index of the audio files under the music directory;
//! the first match is what gets served.

use axum::{
    Router,
    extract::{Path as UrlPath, Request, State},
    http::{StatusCode, Uri},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse as _, Redirect, Response},
    routing::get,
};
use mime_guess::MimeGuess;
use rand::seq::IteratorRandom as _;
use regex::Regex;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;
use tower::Layer as _;
use tower_http::services::ServeDir;

/// A regex longer than this is assumed to be junk rather than a song name.
const MAX_PATTERN_LEN: usize = 64;

/// The audio files under [`MusicIndex::root`], as paths relative to it.
///
/// Scanning the tree is not free, so the result is cached and only rebuilt when
/// a lookup misses — which also picks up files added since startup.
#[derive(Debug)]
pub struct MusicIndex {
    root: PathBuf,
    paths: RwLock<HashSet<String>>,
}

impl MusicIndex {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            paths: RwLock::new(HashSet::new()),
        }
    }

    /// Rebuild the index from the filesystem. Paths are relative to [`Self::root`]
    /// so that they can be handed straight to [`ServeDir`].
    async fn rescan(&self) {
        let root = self.root.clone();
        let found = tokio::task::spawn_blocking(move || {
            walkdir::WalkDir::new(&root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter(|e| is_audio(e.path()))
                .filter_map(|e| Some(e.path().strip_prefix(&root).ok()?.to_str()?.to_string()))
                .collect::<HashSet<_>>()
        })
        .await
        .unwrap_or_default();
        *self.paths.write().await = found;
    }

    /// First indexed path matching `pattern`, rescanning once if nothing matches
    /// the cached index.
    pub async fn find(&self, pattern: &Regex) -> Option<String> {
        if let Some(hit) = self.match_cached(|p| pattern.is_match(p)).await {
            return Some(hit);
        }
        self.rescan().await;
        self.match_cached(|p| pattern.is_match(p)).await
    }

    /// A random indexed path, optionally restricted to those matching `pattern`.
    pub async fn choose(&self, pattern: Option<&Regex>) -> Option<String> {
        self.rescan().await;
        self.paths
            .read()
            .await
            .iter()
            .filter(|path| pattern.is_none_or(|re| re.is_match(path)))
            .choose(&mut rand::rng())
            .cloned()
    }

    async fn match_cached(&self, predicate: impl Fn(&str) -> bool) -> Option<String> {
        self.paths
            .read()
            .await
            .iter()
            .find(|path| predicate(path))
            .cloned()
    }
}

/// Router serving `music_dir`, with `/random` and `/random/{artist}` shortcuts.
pub fn router(music_dir: PathBuf) -> Router {
    let index = Arc::new(MusicIndex::new(music_dir.clone()));
    Router::new()
        .route("/random", get(random))
        .route("/random/{artist}", get(random_by_artist))
        // Only the fallback is wrapped: `/random*` are real paths, not patterns.
        .fallback_service(
            from_fn_with_state(index.clone(), resolve_pattern).layer(ServeDir::new(music_dir)),
        )
        .with_state(index)
}

/// Rewrite the request path — a regex — to the file it matches, then let
/// [`ServeDir`] serve that file.
#[cfg_attr(debug_assertions, axum::debug_middleware)]
async fn resolve_pattern(
    State(index): State<Arc<MusicIndex>>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let Ok(pattern) = urlencoding::decode(path.strip_prefix('/').unwrap_or(path)) else {
        return (StatusCode::BAD_REQUEST, "Path is not valid UTF-8").into_response();
    };
    if pattern.len() > MAX_PATTERN_LEN {
        return (StatusCode::BAD_REQUEST, "Pattern too long").into_response();
    }
    // The pattern comes from speech recognition, so an invalid regex is routine.
    let Ok(pattern) = Regex::new(&pattern) else {
        return (StatusCode::BAD_REQUEST, "Not a valid pattern").into_response();
    };
    let Some(hit) = index.find(&pattern).await else {
        return (StatusCode::NOT_FOUND, "No music matches").into_response();
    };
    println!("{pattern} -> {hit}");
    match file_uri(&hit) {
        Some(uri) => {
            *req.uri_mut() = uri;
            next.run(req).await
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "Unservable path").into_response(),
    }
}

#[cfg_attr(debug_assertions, axum::debug_handler)]
async fn random(State(index): State<Arc<MusicIndex>>) -> Response {
    redirect_to(index.choose(None).await, "No music found")
}

#[cfg_attr(debug_assertions, axum::debug_handler)]
async fn random_by_artist(
    State(index): State<Arc<MusicIndex>>,
    UrlPath(artist): UrlPath<String>,
) -> Response {
    let Ok(pattern) = Regex::new(&regex::escape(&artist)) else {
        return (StatusCode::BAD_REQUEST, "Not a valid artist").into_response();
    };
    redirect_to(
        index.choose(Some(&pattern)).await,
        "No music found for that artist",
    )
}

fn redirect_to(hit: Option<String>, not_found: &'static str) -> Response {
    match hit {
        Some(hit) => Redirect::to(&format!("/{}", urlencoding::encode(&hit))).into_response(),
        None => (StatusCode::NOT_FOUND, not_found).into_response(),
    }
}

fn file_uri(path: &str) -> Option<Uri> {
    format!("/{}", urlencoding::encode(path)).parse().ok()
}

fn is_audio(path: &Path) -> bool {
    MimeGuess::from_path(path).first_or_octet_stream().type_() == "audio"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt as _;

    /// A music directory holding one track and one non-audio file.
    fn library() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("周杰伦")).unwrap();
        std::fs::write(dir.path().join("周杰伦/晴天.mp3"), b"audio").unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"not audio").unwrap();
        dir
    }

    async fn get(dir: &tempfile::TempDir, uri: &str) -> Response {
        router(dir.path().to_path_buf())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn serves_the_file_a_pattern_matches() {
        let dir = library();
        let resp = get(&dir, &format!("/{}", urlencoding::encode(".*晴天.*"))).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unmatched_pattern_is_not_found() {
        let dir = library();
        let resp = get(&dir, &format!("/{}", urlencoding::encode(".*不存在.*"))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn non_audio_files_are_not_indexed() {
        let dir = library();
        let resp = get(&dir, &format!("/{}", urlencoding::encode(".*cover.*"))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Speech recognition happily produces text that is not a valid regex; that
    /// must be a 400, not a panic.
    #[tokio::test]
    async fn invalid_pattern_is_rejected() {
        let dir = library();
        let resp = get(&dir, &format!("/{}", urlencoding::encode("*["))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = get(&dir, &format!("/{}", urlencoding::encode(&"x".repeat(100)))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn random_redirects_to_a_track() {
        let dir = library();
        let resp = get(&dir, "/random").await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);

        let resp = get(&dir, "/random/周杰伦").await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);

        let resp = get(&dir, "/random/无此歌手").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
