//! Serving local audio to the speaker. A request path is treated as a regex and
//! matched against an index of the music directory; the first match is served.
//!
//! `.ncm` (NetEase's encrypted container) is indexed like any track but
//! decrypted streaming **on the way out**, so no plaintext hits the disk;
//! [`ServeDir`] would hand the speaker raw ciphertext, hence the separate branch.

use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, Path as UrlPath, Request, State},
    http::{StatusCode, Uri, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse as _, Redirect, Response},
    routing::get,
};
use bytes::Bytes;
use mime_guess::MimeGuess;
use ncmc_lib::NcmFile;
use rand::seq::IteratorRandom as _;
use regex::Regex;
use std::{
    collections::HashSet,
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;
use tower::Layer as _;
use tower_http::services::ServeDir;

/// A regex longer than this is assumed junk, not a song name.
const MAX_PATTERN_LEN: usize = 64;

/// Bytes buffered between the blocking decrypter and the response body.
const DECRYPT_BUFFER: usize = 64 * 1024;

/// The audio files under [`MusicIndex::root`], as relative paths. Cached and
/// rebuilt only on a miss, which also picks up files added since startup.
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

    /// The directory the indexed paths are relative to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Rebuild the index from the filesystem, relative to [`Self::root`].
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

    /// Whether `rel` is an indexed file, rescanning once on a miss. Also the only
    /// sanctioned way to turn caller text into a path — index membership proves it
    /// is a real file under the root, not `../../etc/passwd`.
    pub async fn contains(&self, rel: &str) -> bool {
        if self.paths.read().await.contains(rel) {
            return true;
        }
        self.rescan().await;
        self.paths.read().await.contains(rel)
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

    /// Up to `limit` indexed paths matching `pattern`, in arbitrary order.
    ///
    /// Always rescans first (unlike the cached [`Self::find`] on the serving hot
    /// path): this backs [`brain::MusicSource::search`], so a newly-added track
    /// must show up even when the stale index already matches the pattern.
    pub async fn find_all(&self, pattern: &Regex, limit: usize) -> Vec<String> {
        self.rescan().await;
        self.collect_cached(pattern, limit).await
    }

    async fn collect_cached(&self, pattern: &Regex, limit: usize) -> Vec<String> {
        self.paths
            .read()
            .await
            .iter()
            .filter(|path| pattern.is_match(path))
            .take(limit)
            .cloned()
            .collect()
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

/// Test-only convenience: the binary always shares its [`Arc<MusicIndex>`] with
/// [`crate::source::LocalSource`] via [`router_with`].
#[cfg(test)]
pub fn router(music_dir: PathBuf) -> Router {
    router_with(Arc::new(MusicIndex::new(music_dir)))
}

/// [`router`] over an index the caller already holds — shared with a
/// [`crate::source::LocalSource`] so both agree on what exists.
pub fn router_with(index: Arc<MusicIndex>) -> Router {
    let music_dir = index.root().to_path_buf();
    Router::new()
        .route("/random", get(random))
        .route("/random/{artist}", get(random_by_artist))
        // Only the fallback is wrapped: `/random*` are real paths, not patterns.
        .fallback_service(
            from_fn_with_state(index.clone(), resolve_pattern).layer(ServeDir::new(music_dir)),
        )
        .with_state(index)
}

/// Rewrite the request path (a regex) to the file it matches, then let
/// [`ServeDir`] serve it.
#[cfg_attr(debug_assertions, axum::debug_middleware)]
async fn resolve_pattern(
    State(index): State<Arc<MusicIndex>>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let Ok(wanted) = urlencoding::decode(path.strip_prefix('/').unwrap_or(path)) else {
        return (StatusCode::BAD_REQUEST, "Path is not valid UTF-8").into_response();
    };
    let hit = match resolve(&index, &wanted).await {
        Ok(hit) => hit,
        Err(response) => return response,
    };
    tracing::debug!(%wanted, %hit, "request resolved");
    // `ServeDir` would serve an `.ncm` as undecodable ciphertext; decrypt instead.
    if is_ncm(Path::new(&hit)) {
        return serve_ncm(index.root().join(&hit)).await;
    }
    match file_uri(&hit) {
        Some(uri) => {
            *req.uri_mut() = uri;
            next.run(req).await
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "Unservable path").into_response(),
    }
}

/// The indexed file `wanted` refers to, or a response explaining why there is
/// none. An exact index hit wins first (a real filename like `Song (Live).mp3`
/// is a valid regex meaning something else, and often longer than a spoken
/// title); only otherwise is `wanted` compiled as a speech-derived pattern.
async fn resolve(index: &MusicIndex, wanted: &str) -> Result<String, Response> {
    if index.contains(wanted).await {
        return Ok(wanted.to_string());
    }
    // Characters, not bytes, so a Chinese title is not penalised.
    if wanted.chars().count() > MAX_PATTERN_LEN {
        return Err((StatusCode::BAD_REQUEST, "Pattern too long").into_response());
    }
    // An invalid regex from speech is routine, not an error.
    let Ok(pattern) = Regex::new(wanted) else {
        return Err((StatusCode::BAD_REQUEST, "Not a valid pattern").into_response());
    };
    index
        .find(&pattern)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "No music matches").into_response())
}

#[cfg_attr(debug_assertions, axum::debug_handler)]
async fn random(
    State(index): State<Arc<MusicIndex>>,
    OriginalUri(original): OriginalUri,
    inner: Uri,
) -> Response {
    redirect_to(
        index.choose(None).await,
        mount_prefix(&original, &inner),
        "No music found",
    )
}

#[cfg_attr(debug_assertions, axum::debug_handler)]
async fn random_by_artist(
    State(index): State<Arc<MusicIndex>>,
    OriginalUri(original): OriginalUri,
    inner: Uri,
    UrlPath(artist): UrlPath<String>,
) -> Response {
    let Ok(pattern) = Regex::new(&regex::escape(&artist)) else {
        return (StatusCode::BAD_REQUEST, "Not a valid artist").into_response();
    };
    redirect_to(
        index.choose(Some(&pattern)).await,
        mount_prefix(&original, &inner),
        "No music found for that artist",
    )
}

/// The prefix the audio router is mounted under (empty, or `/{token}` under a
/// stream token). A `/random` redirect must carry it: `nest` hides the prefix
/// from the handler, so a bare `Location: /{file}` would point outside the mount.
fn mount_prefix<'a>(original: &'a Uri, inner: &Uri) -> &'a str {
    original.path().strip_suffix(inner.path()).unwrap_or("")
}

fn redirect_to(hit: Option<String>, prefix: &str, not_found: &'static str) -> Response {
    match hit {
        Some(hit) => {
            Redirect::to(&format!("{prefix}/{}", urlencoding::encode(&hit))).into_response()
        }
        None => (StatusCode::NOT_FOUND, not_found).into_response(),
    }
}

fn file_uri(path: &str) -> Option<Uri> {
    format!("/{}", urlencoding::encode(path)).parse().ok()
}

/// Stream `path` decrypted. `ncmc_lib`'s [`NcmFile`] is a synchronous
/// [`std::io::Read`] whose [`NcmFile::open`] eagerly parses header/key/metadata,
/// so both run on [`spawn_blocking`](tokio::task::spawn_blocking) and the bytes
/// cross back through a channel — a 40 MB FLAC never sits in memory at once.
///
/// No `Content-Length`: the plaintext length is the file size minus a header
/// `ncmc_lib` does not report, and a wrong one is worse than none. This also
/// means no byte-range support. A read failure mid-stream becomes an error item
/// in the body so hyper aborts the connection rather than ending it as a clean
/// (silently truncated) short track.
async fn serve_ncm(path: PathBuf) -> Response {
    let shown = path.display().to_string();
    // Sending blocks when this fills, so the decrypter cannot outrun a slow speaker.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    // `open`'s result is awaited before the response is built, so a corrupt file
    // is a 500 rather than a 200 that turns out empty.
    let (opened, ready) = tokio::sync::oneshot::channel();

    tokio::task::spawn_blocking(move || {
        let mut ncm = match NcmFile::open(&path) {
            Ok(ncm) => ncm,
            Err(e) => {
                let _ = opened.send(Err(e.to_string()));
                return;
            }
        };
        if opened.send(Ok(ncm.meta().format.clone())).is_err() {
            return;
        }
        let mut buf = vec![0u8; DECRYPT_BUFFER];
        loop {
            match ncm.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // A send error is the speaker hanging up — the ordinary end.
                    if tx
                        .blocking_send(Ok(Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    // A genuine failure: push it so the body aborts, not truncates.
                    tracing::warn!(path = %shown, error = %e, "ncm read failed mid-stream");
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });

    let format = match ready.await {
        Ok(Ok(format)) => format,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "cannot decrypt ncm");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Cannot decrypt").into_response();
        }
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Decrypt task died").into_response(),
    };

    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    ([(header::CONTENT_TYPE, content_type(&format))], body).into_response()
}

/// The `.ncm` extension says nothing (NetEase stores both MP3 and FLAC in it);
/// the metadata's format field does.
fn content_type(format: &str) -> &'static str {
    match format {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

pub(crate) fn is_ncm(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ncm"))
}

/// `.ncm` is unknown to `mime_guess`, so name it explicitly or the encrypted
/// half of the library would never be indexed.
fn is_audio(path: &Path) -> bool {
    is_ncm(path) || MimeGuess::from_path(path).first_or_octet_stream().type_() == "audio"
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

    /// Speech produces text that is not a valid regex; that is a 400, not a panic.
    #[tokio::test]
    async fn invalid_pattern_is_rejected() {
        let dir = library();
        let resp = get(&dir, &format!("/{}", urlencoding::encode("*["))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = get(&dir, &format!("/{}", urlencoding::encode(&"x".repeat(100)))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// An exact indexed path (from `/random` or `LocalSource::resolve`) is looked
    /// up as a path, not compiled — even when it exceeds the pattern length limit.
    #[tokio::test]
    async fn an_exact_path_is_served_verbatim() {
        let name = "Song (Live) [Remastered 2011] - A Very Long English Title Indeed.mp3";
        assert!(name.chars().count() > MAX_PATTERN_LEN);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), b"audio").unwrap();

        let resp = get(&dir, &format!("/{}", urlencoding::encode(name))).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The exact-path shortcut must not escape the music directory.
    #[tokio::test]
    async fn traversal_is_not_an_exact_path() {
        let dir = library();
        let resp = get(
            &dir,
            &format!("/{}", urlencoding::encode("../../etc/passwd")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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

    /// Nested under `/{token}`, a `/random` redirect must still land inside the
    /// mount — a bare `Location: /{file}` would 404.
    #[tokio::test]
    async fn random_redirect_keeps_the_mount_prefix() {
        let dir = library();
        let app = Router::new().nest("/s3cret", router(dir.path().to_path_buf()));

        let follow = |uri: &'static str| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::SEE_OTHER, "{uri}");
                let location = resp
                    .headers()
                    .get(header::LOCATION)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                assert!(
                    location.starts_with("/s3cret/"),
                    "{uri} redirected outside the mount: {location}"
                );
            }
        };
        follow("/s3cret/random").await;
        follow("/s3cret/random/周杰伦").await;
    }

    // --- `.ncm` -----------------------------------------------------------
    //
    // No sample file can be checked in (it is copyrighted music), so `ncm` below
    // builds one as the inverse of `ncmc_lib`'s reader — a wrong layout makes the
    // real decoder reject the fixture.

    use aes::cipher::{BlockModeEncrypt as _, KeyInit as _, block_padding::Pkcs7};
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    use http_body_util::BodyExt as _;

    const CORE_KEY: &[u8; 16] = b"hzHRAmso5kInbaxW";
    const META_KEY: &[u8; 16] = br#"#14ljk_!\]&0U<'("#;

    fn aes_ecb(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        ecb::Encryptor::<aes::Aes128>::new_from_slice(key)
            .unwrap()
            .encrypt_padded_vec::<Pkcs7>(plain)
    }

    /// A length-prefixed, XOR-masked block — the shape both the key and the
    /// metadata are stored in.
    fn block(mask: u8, body: &[u8]) -> Vec<u8> {
        let mut out = (body.len() as u32).to_le_bytes().to_vec();
        out.extend(body.iter().map(|b| b ^ mask));
        out
    }

    /// The RC4-like key schedule `ncmc_lib` derives from the decrypted key, and
    /// the keystream it then XORs the audio with.
    fn keystream(key_data: &[u8], len: usize) -> Vec<u8> {
        let mut key_box: [u8; 256] = core::array::from_fn(|i| i as u8);
        let (mut last, mut offset) = (0u8, 0usize);
        for i in 0..256 {
            let c = key_box[i].wrapping_add(last).wrapping_add(key_data[offset]);
            offset = (offset + 1) % key_data.len();
            key_box.swap(i, c as usize);
            last = c;
        }
        (1..=len)
            .map(|i| {
                let i = i as u8;
                let k = key_box[i as usize];
                key_box[k.wrapping_add(key_box[k.wrapping_add(i) as usize]) as usize]
            })
            .collect()
    }

    /// A complete `.ncm` file wrapping `audio`.
    fn ncm(audio: &[u8], format: &str) -> Vec<u8> {
        let key_data = b"a-secret-per-song";
        let mut out = b"CTENFDAM\0\0".to_vec();

        let mut key_plain = b"neteasecloudmusic".to_vec();
        key_plain.extend_from_slice(key_data);
        out.extend(block(0x64, &aes_ecb(CORE_KEY, &key_plain)));

        let json = format!(
            r#"{{"musicName":"晴天","artist":[["周杰伦",6452]],"album":"叶惠美",
               "albumId":32311,"albumPic":"","albumPicDocId":0,"bitrate":320000,
               "duration":269000,"format":"{format}","musicId":186016}}"#
        );
        let mut meta_plain = b"music:".to_vec();
        meta_plain.extend_from_slice(json.as_bytes());
        let mut meta = b"163 key(Don't modify):".to_vec();
        meta.extend_from_slice(
            BASE64_STANDARD
                .encode(aes_ecb(META_KEY, &meta_plain))
                .as_bytes(),
        );
        out.extend(block(0x63, &meta));

        out.extend_from_slice(&[0; 5]); // CRC and gap, never read
        // Cover art: a frame longer than the image it holds, so that the
        // reader's skip-the-remainder step is exercised too.
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&[0xff, 0xd8, 0xff, 0xe0]);
        out.extend_from_slice(&[0; 4]);

        let mask = keystream(key_data, audio.len());
        out.extend(audio.iter().zip(mask).map(|(b, m)| b ^ m));
        out
    }

    fn ncm_library(format: &str, audio: &[u8]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("晴天.ncm"), ncm(audio, format)).unwrap();
        dir
    }

    /// Bytes in, plaintext out, nothing written to disk.
    #[tokio::test]
    async fn ncm_is_decrypted_on_the_way_out() {
        // Longer than one read buffer, to catch a keystream that resets per chunk.
        let audio: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        let dir = ncm_library("mp3", &audio);

        let resp = get(&dir, &format!("/{}", urlencoding::encode(".*晴天.*"))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "audio/mpeg");
        let served = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(served.as_ref(), audio.as_slice());

        // No decrypted copy may be left behind.
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, ["晴天.ncm"]);
    }

    /// The extension says `.ncm` either way; only the metadata knows which.
    #[tokio::test]
    async fn content_type_comes_from_the_metadata() {
        let dir = ncm_library("flac", b"flac frames");
        let resp = get(&dir, &format!("/{}", urlencoding::encode(".*晴天.*"))).await;
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "audio/flac");
    }

    #[tokio::test]
    async fn ncm_files_are_indexed_and_reachable_at_random() {
        let dir = ncm_library("mp3", b"audio");
        let resp = get(&dir, "/random").await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    }

    /// A truncated or renamed file must not take the server down with it.
    #[tokio::test]
    async fn a_corrupt_ncm_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.ncm"), b"not really an ncm file").unwrap();
        let resp = get(&dir, &format!("/{}", urlencoding::encode(".*broken.*"))).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
