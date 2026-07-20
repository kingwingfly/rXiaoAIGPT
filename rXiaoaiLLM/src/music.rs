//! Serving local audio files to the speaker.
//!
//! The speaker can only be told to play a URL, and we only know what the user
//! *said* — not which file they meant. So a request path is treated as a regex
//! and matched against an index of the audio files under the music directory;
//! the first match is what gets served.
//!
//! # `.ncm`
//!
//! Part of the library is `.ncm`: NetEase Cloud Music's encrypted container,
//! which its desktop client writes and nothing else can play. Those are indexed
//! like any other track and decrypted **on the way out**, streaming, so no
//! plaintext copy ever touches the disk — the point is transparent playback,
//! not conversion. They need their own handler branch because [`ServeDir`]
//! would hand the speaker the raw ciphertext.

use axum::{
    Router,
    body::Body,
    extract::{Path as UrlPath, Request, State},
    http::{StatusCode, Uri, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse as _, Redirect, Response},
    routing::get,
};
use mime_guess::MimeGuess;
use ncmc_lib::NcmFile;
use rand::seq::IteratorRandom as _;
use regex::Regex;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;
use tokio_util::io::{ReaderStream, SyncIoBridge};
use tower::Layer as _;
use tower_http::services::ServeDir;

/// A regex longer than this is assumed to be junk rather than a song name.
const MAX_PATTERN_LEN: usize = 64;

/// Bytes buffered between the blocking decrypter and the response body. Large
/// enough that the decrypting thread is not woken per kilobyte, small enough
/// that a track is never held in memory.
const DECRYPT_BUFFER: usize = 64 * 1024;

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

    /// The directory the indexed paths are relative to. Joining an indexed path
    /// onto it is the only supported way to get an absolute path: the index is
    /// built by walking the tree, so its entries can never escape the root.
    pub fn root(&self) -> &Path {
        &self.root
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

    /// Whether `rel` is an indexed file, rescanning once if the cached index
    /// says no.
    ///
    /// This is also the *only* sanctioned way to turn caller-supplied text into
    /// a path: membership of the index is what proves it names a real file
    /// under the root rather than `../../etc/passwd`.
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

    /// Up to `limit` indexed paths matching `pattern`, rescanning once if the
    /// cached index yields nothing.
    ///
    /// Ordering is the [`HashSet`]'s, i.e. arbitrary — "best match first" is not
    /// something a regex over file paths can express, and pretending otherwise
    /// would be a lie to [`brain::MusicSource::search`]'s caller.
    pub async fn find_all(&self, pattern: &Regex, limit: usize) -> Vec<String> {
        let hits = self.collect_cached(pattern, limit).await;
        if !hits.is_empty() {
            return hits;
        }
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

/// Router serving `music_dir`, with `/random` and `/random/{artist}` shortcuts.
///
/// Test-only: the binary owns an [`Arc<MusicIndex>`] it shares with
/// [`crate::source::LocalSource`] and therefore always builds the router with
/// [`router_with`]. Keeping the convenience form for the tests below costs
/// nothing; keeping it in the binary would be one more way for the two halves
/// to end up with different indexes.
#[cfg(test)]
pub fn router(music_dir: PathBuf) -> Router {
    router_with(Arc::new(MusicIndex::new(music_dir)))
}

/// [`router`] over an index the caller already holds.
///
/// Sharing one index with a [`crate::source::LocalSource`] is not just an
/// optimisation: the source hands out URLs whose paths must resolve against the
/// same set of files the server will look them up in.
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

/// Rewrite the request path — a regex — to the file it matches, then let
/// [`ServeDir`] serve that file.
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
    // `ServeDir` would happily serve an `.ncm`, but as ciphertext the speaker
    // cannot decode — those get decrypted here instead of being passed on.
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

/// The indexed file `wanted` refers to, or the response explaining why there is
/// none.
///
/// Two ways in, because two very different callers share this endpoint:
///
/// - Something that already knows the exact path — a `/random` redirect, or
///   [`crate::source::LocalSource::resolve`] handing the speaker a URL. That is
///   an *exact* lookup, and must not be reinterpreted: a real filename like
///   `Song (Live).mp3` is a valid regex meaning something else entirely, and is
///   easily longer than a plausible spoken title.
/// - Speech, which is a pattern and nothing more precise. Only that path is
///   held to the length limit and the cost of compiling a regex.
async fn resolve(index: &MusicIndex, wanted: &str) -> Result<String, Response> {
    if index.contains(wanted).await {
        return Ok(wanted.to_string());
    }
    if wanted.chars().count() > MAX_PATTERN_LEN {
        // Characters, not bytes: counting bytes would give a Chinese title a
        // third of the budget an English one gets.
        return Err((StatusCode::BAD_REQUEST, "Pattern too long").into_response());
    }
    // The pattern comes from speech recognition, so an invalid regex is routine.
    let Ok(pattern) = Regex::new(wanted) else {
        return Err((StatusCode::BAD_REQUEST, "Not a valid pattern").into_response());
    };
    index
        .find(&pattern)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "No music matches").into_response())
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

/// Stream `path` decrypted.
///
/// Two things make this awkward and both are forced by `ncmc_lib`: [`NcmFile`]
/// is a *synchronous* [`std::io::Read`], and [`NcmFile::open`] eagerly parses
/// the header, key, metadata and embedded cover before a single audio byte is
/// available. Both therefore run on [`tokio::task::spawn_blocking`], and the
/// decrypted bytes cross back into async through a duplex pipe rather than a
/// buffer, so a 40 MB FLAC never exists in memory at once.
///
/// No `Content-Length` is sent: the plaintext length is the ciphertext length
/// minus a header whose size `ncmc_lib` does not report, and a wrong one is
/// worse than none. The response is chunked, which also means byte-range
/// requests are unsupported — the speaker plays tracks start to finish anyway.
async fn serve_ncm(path: PathBuf) -> Response {
    let (reader, writer) = tokio::io::duplex(DECRYPT_BUFFER);
    // Captured out here: the bridge needs a runtime handle, and taking it on
    // the blocking thread relies on ambient state we would rather not assume.
    let handle = tokio::runtime::Handle::current();
    let shown = path.display().to_string();

    // `open` is done inside the same blocking task as the copy, but we wait for
    // its result before answering: a corrupt file must be a 500, not a
    // successful response that turns out to be empty.
    let (opened, ready) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let mut ncm = match NcmFile::open(&path) {
            Ok(ncm) => ncm,
            Err(e) => {
                // A failed send just means the client hung up: nothing to do.
                let _ = opened.send(Err(e.to_string()));
                return;
            }
        };
        let format = ncm.meta().format.clone();
        if opened.send(Ok(format)).is_err() {
            return;
        }
        let mut out = SyncIoBridge::new_with_handle(writer, handle);
        // A broken pipe here is the ordinary end of playback (the speaker
        // stopped), so it is logged at debug rather than warn.
        if let Err(e) = std::io::copy(&mut ncm, &mut out) {
            tracing::debug!(path = %shown, error = %e, "ncm stream ended early");
        }
    });

    let format = match ready.await {
        Ok(Ok(format)) => format,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "cannot decrypt ncm");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Cannot decrypt").into_response();
        }
        // The blocking task cannot panic short of an allocator failure, but a
        // dropped sender must not become a hang.
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Decrypt task died").into_response(),
    };

    (
        [(header::CONTENT_TYPE, content_type(&format))],
        Body::from_stream(ReaderStream::new(reader)),
    )
        .into_response()
}

/// The container's own idea of what it holds — the `.ncm` extension says
/// nothing, and NetEase stores both MP3 and FLAC in it.
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

/// `.ncm` is unknown to `mime_guess` (it is NetEase's own container, not a
/// registered type), so it needs saying explicitly or the library's encrypted
/// half would never be indexed.
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

    /// A real filename is a valid regex meaning something else, and is often
    /// longer than a plausible spoken title — so the exact path a `/random`
    /// redirect (or `LocalSource::resolve`) hands back must be looked up as a
    /// path, not compiled.
    #[tokio::test]
    async fn an_exact_path_is_served_verbatim() {
        let name = "Song (Live) [Remastered 2011] - A Very Long English Title Indeed.mp3";
        assert!(name.chars().count() > MAX_PATTERN_LEN);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), b"audio").unwrap();

        let resp = get(&dir, &format!("/{}", urlencoding::encode(name))).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The exact-path shortcut must not become a way out of the music
    /// directory: only indexed files are servable.
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

    // --- `.ncm` -----------------------------------------------------------
    //
    // There is no sample file to check in (the container is copyrighted music
    // by construction), so the tests build one. `ncm` below is the inverse of
    // `ncmc_lib`'s reader, written against its source: get the layout wrong and
    // the decoder rejects the fixture, which is exactly the assertion wanted.

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

    /// The whole point: bytes in, plaintext out, and nothing written to disk.
    #[tokio::test]
    async fn ncm_is_decrypted_on_the_way_out() {
        // Longer than one read buffer, so a keystream that resets per chunk
        // instead of running continuously would be caught.
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
