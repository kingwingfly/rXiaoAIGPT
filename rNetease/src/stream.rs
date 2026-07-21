//! Streaming a remote audio file straight through to an HTTP response.
//!
//! # Why a proxy at all
//!
//! The XiaoAi speaker will not play a NetEase CDN URL handed to it directly:
//! the URLs are single-use-ish, expire quickly, and are not something we want
//! the speaker to hold on to. So our own HTTP server stands in front of them —
//! the speaker asks us, and we ask the CDN.
//!
//! # Why a *stream* and not a download
//!
//! A track is several megabytes. Buffering it in memory (or spooling it to
//! disk) before answering would add the whole download to the latency the user
//! perceives as "the speaker is slow", and would make a dozen concurrent
//! requests a memory problem. [`stream_audio`] returns as soon as the upstream
//! *headers* arrive; bytes are then forwarded chunk by chunk as they land.
//!
//! # Why the URL must be resolved just-in-time
//!
//! A resolved CDN URL carries `expi: 1200` — it stops working roughly **20
//! minutes** after resolution. Anything that caches one (a playlist expanded
//! ahead of time, a "recently played" table, a retry that reuses the old URL)
//! will work in testing and fail in the field. Resolve immediately before
//! calling into this module, once per playback attempt, and throw the URL away
//! afterwards.
//!
//! Because that is the dominant failure mode, a `403`/`404` from the CDN is
//! reported as [`crate::NeteaseErr::UrlExpired`] rather than as a generic HTTP
//! error: on this path "forbidden" almost never means "this track does not
//! exist", it means "you resolved this URL too long ago".
//!
//! # Why `Range` is forwarded
//!
//! The speaker seeks, and it reconnects after a network hiccup — in both cases
//! by re-requesting the same resource with a `Range` header. If we ignored the
//! header and always answered `200` with the whole body, seeking would appear
//! to do nothing and any blip mid-track would restart it from the beginning.
//! So the caller's range is passed upstream verbatim and the upstream's
//! `206 Partial Content`, `Content-Range` and status are propagated back
//! unchanged. We are a pipe, not a policy.
//!
//! # Why no HTTPS upgrade
//!
//! Some NetEase CDN hosts are served over plain `http://`. Forcing HTTPS would
//! turn those into connection errors, so the URL is used exactly as resolved.

use crate::{
    client::Client,
    error::{NeteaseErr, Result},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use std::pin::Pin;

/// A boxed byte stream. Boxed because it is stored in a struct field and handed
/// across a crate boundary into a web layer that must not need to name
/// `reqwest`'s concrete stream type.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// An upstream audio response, opened but not yet consumed.
///
/// The metadata fields are exactly what an HTTP layer needs to build its own
/// response; the body is left as a stream so nothing is buffered. This crate
/// deliberately does not depend on `axum` — the binary adapts this into
/// whatever its web framework wants.
pub struct AudioStream {
    /// The upstream status, passed through as-is: `200` for a whole body,
    /// `206` when a `Range` was honoured. Answering `200` to a ranged request
    /// breaks seeking, so do not normalise this.
    pub status: u16,
    /// Upstream `Content-Type`, e.g. `audio/mpeg`. `None` if the CDN omitted it.
    pub content_type: Option<String>,
    /// Upstream `Content-Length`: the length of *this* response, i.e. of the
    /// requested range when the response is a `206`, not of the whole track.
    pub content_length: Option<u64>,
    /// Upstream `Content-Range` (`bytes 100-199/4096`), present on a `206`.
    pub content_range: Option<String>,
    /// Upstream `Accept-Ranges`. Forwarding it is what tells the speaker it may
    /// seek at all.
    pub accept_ranges: Option<String>,
    body: ByteStream,
}

impl std::fmt::Debug for AudioStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioStream")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("content_length", &self.content_length)
            .field("content_range", &self.content_range)
            .field("accept_ranges", &self.accept_ranges)
            .finish_non_exhaustive()
    }
}

impl AudioStream {
    /// True when the upstream honoured a `Range` request.
    pub fn is_partial(&self) -> bool {
        self.status == 206
    }

    /// Take the body. Consuming the struct is deliberate: the stream can only
    /// be read once, and the metadata should have been copied into the outgoing
    /// response headers before this point.
    ///
    /// A failure *after* the first byte (the CDN dropping the connection
    /// mid-track) arrives as an `Err` item in the stream rather than as an
    /// early end, so the web layer can distinguish a truncated track from a
    /// complete one.
    pub fn into_stream(self) -> ByteStream {
        self.body
    }
}

/// Open `url` for streaming, optionally forwarding a `Range` header value.
///
/// `range` is the raw header value as received from the downstream client, e.g.
/// `Some("bytes=1000-")`. It is passed upstream untouched; parsing and
/// satisfying it is the CDN's job, and re-deriving it ourselves would only
/// introduce a way to get it wrong.
///
/// The URL **must have been resolved moments ago** — see the module docs. The
/// `client`'s connection pool is reused, but its cookies are irrelevant here:
/// CDN URLs authenticate through their own signed query string.
///
/// # Errors
///
/// - [`NeteaseErr::UrlExpired`] for a `403` or `404`, the usual symptom of a
///   stale URL.
/// - [`NeteaseErr::BadRequest`] for any other non-success status, and for an
///   empty URL.
/// - [`NeteaseErr::Http`] if the request never completed.
pub async fn stream_audio(client: &Client, url: &str, range: Option<&str>) -> Result<AudioStream> {
    stream_audio_with(client.http(), url, range).await
}

/// As [`stream_audio`], but against a bare [`reqwest::Client`].
///
/// Exists because streaming needs nothing from the NetEase session — a caller
/// that already has its own HTTP client should not have to build a [`Client`]
/// just to proxy bytes.
pub async fn stream_audio_with(
    http: &reqwest::Client,
    url: &str,
    range: Option<&str>,
) -> Result<AudioStream> {
    if url.trim().is_empty() {
        return Err(NeteaseErr::BadRequest(
            "empty audio url; resolve the track id first".into(),
        ));
    }

    let mut req = http.get(url);
    if let Some(range) = range {
        req = req.header(reqwest::header::RANGE, range);
    }
    tracing::debug!(%url, range = ?range, "opening audio stream");
    let resp = req.send().await?;

    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
        return Err(NeteaseErr::UrlExpired {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    // `416 Range Not Satisfiable` is a normal, recoverable answer to an
    // out-of-range request, not a server failure: it carries
    // `Content-Range: bytes */N` telling the client the real length so it can
    // clamp and retry. Relay it like any other response rather than collapsing
    // it into a 502 that discards that header and looks like a broken origin.
    let relayable = status.is_success() || status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE;
    if !relayable {
        return Err(NeteaseErr::BadRequest(format!(
            "cdn answered {status} for {url}"
        )));
    }

    let header = |name: reqwest::header::HeaderName| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let content_type = header(reqwest::header::CONTENT_TYPE);
    let content_range = header(reqwest::header::CONTENT_RANGE);
    let accept_ranges = header(reqwest::header::ACCEPT_RANGES);
    // `Response::content_length` is the parsed header, and is what we want:
    // for a 206 it describes the slice, not the track.
    let content_length = resp.content_length();

    Ok(AudioStream {
        status: status.as_u16(),
        content_type,
        content_length,
        content_range,
        accept_ranges,
        body: Box::pin(
            resp.bytes_stream()
                .map(|chunk| chunk.map_err(NeteaseErr::from)),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::{IntoResponse, Response},
        routing::get,
    };
    use std::sync::Arc;

    /// A few KB of deterministic "audio" — enough to span several chunks.
    fn fake_audio() -> Arc<Vec<u8>> {
        Arc::new((0..8192u32).map(|i| (i % 251) as u8).collect())
    }

    /// A stand-in CDN: serves the bytes, honours a single `bytes=a-b` range,
    /// and has a `/expired` path that answers `403` the way a stale URL does.
    async fn mock_cdn() -> (String, tokio::task::JoinHandle<()>) {
        let audio = fake_audio();
        let app = Router::new()
            .route(
                "/song.mp3",
                get(
                    |State(audio): State<Arc<Vec<u8>>>, headers: HeaderMap| async move {
                        let range = headers
                            .get(header::RANGE)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        serve(&audio, range)
                    },
                ),
            )
            .route("/expired", get(|| async { StatusCode::FORBIDDEN }))
            .route("/boom", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            // A range past EOF: the CDN answers 416 with the real length, which
            // the client needs in order to correct itself. Must be relayed, not
            // turned into a 502.
            .route(
                "/rangetoobig",
                get(|| async {
                    (
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [(header::CONTENT_RANGE, "bytes */8192")],
                    )
                        .into_response()
                }),
            )
            // Sends a chunk, then fails: the CDN dying mid-track. Must surface
            // as an `Err` item downstream, not as a body that merely ends.
            .route(
                "/truncated",
                get(|| async {
                    let chunks: Vec<std::result::Result<Bytes, std::io::Error>> = vec![
                        Ok(Bytes::from_static(&[7u8; 100])),
                        Err(std::io::Error::other("cdn went away")),
                    ];
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from_stream(futures_util::stream::iter(chunks)))
                        .unwrap()
                }),
            )
            .with_state(audio);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    fn serve(audio: &[u8], range: Option<String>) -> Response {
        let total = audio.len();
        match range.as_deref().and_then(parse_range) {
            Some((start, end)) if start <= end && end < total => {
                let slice = audio[start..=end].to_vec();
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_TYPE, "audio/mpeg".to_string()),
                        (header::ACCEPT_RANGES, "bytes".to_string()),
                        (
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{end}/{total}"),
                        ),
                    ],
                    Body::from(slice),
                )
                    .into_response()
            }
            _ => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "audio/mpeg".to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                ],
                Body::from(audio.to_vec()),
            )
                .into_response(),
        }
    }

    /// Minimal `bytes=start-end` / `bytes=start-` parser; the mock only needs
    /// the shapes reqwest will send it.
    fn parse_range(value: &str) -> Option<(usize, usize)> {
        let spec = value.strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        let start: usize = start.trim().parse().ok()?;
        let end = end.trim();
        let end = if end.is_empty() {
            8191
        } else {
            end.parse().ok()?
        };
        Some((start, end))
    }

    async fn collect(stream: AudioStream) -> Vec<u8> {
        let mut body = stream.into_stream();
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn full_body_reassembles_to_the_original_bytes() {
        let (base, server) = mock_cdn().await;
        let client = Client::with_base_url(&base).unwrap();

        let stream = stream_audio(&client, &format!("{base}/song.mp3"), None)
            .await
            .unwrap();
        assert_eq!(stream.status, 200);
        assert!(!stream.is_partial());
        assert_eq!(stream.content_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(stream.accept_ranges.as_deref(), Some("bytes"));
        assert_eq!(stream.content_length, Some(8192));
        assert!(stream.content_range.is_none());

        assert_eq!(collect(stream).await, *fake_audio());
        server.abort();
    }

    #[tokio::test]
    async fn range_is_forwarded_and_206_propagated() {
        let (base, server) = mock_cdn().await;
        let client = Client::with_base_url(&base).unwrap();

        let stream = stream_audio(&client, &format!("{base}/song.mp3"), Some("bytes=100-199"))
            .await
            .unwrap();
        assert_eq!(stream.status, 206);
        assert!(stream.is_partial());
        assert_eq!(stream.content_range.as_deref(), Some("bytes 100-199/8192"));
        assert_eq!(stream.content_length, Some(100));

        let body = collect(stream).await;
        assert_eq!(body.len(), 100);
        assert_eq!(body, fake_audio()[100..200]);
        server.abort();
    }

    /// An open-ended range is what a reconnecting player sends; it must reach
    /// the end of the track rather than being clamped.
    #[tokio::test]
    async fn open_ended_range_runs_to_the_end() {
        let (base, server) = mock_cdn().await;
        let client = Client::with_base_url(&base).unwrap();

        let stream = stream_audio(&client, &format!("{base}/song.mp3"), Some("bytes=8000-"))
            .await
            .unwrap();
        assert_eq!(stream.status, 206);
        assert_eq!(collect(stream).await, fake_audio()[8000..]);
        server.abort();
    }

    #[tokio::test]
    async fn forbidden_maps_to_expired_url() {
        let (base, server) = mock_cdn().await;
        let client = Client::with_base_url(&base).unwrap();

        let err = stream_audio(&client, &format!("{base}/expired"), None)
            .await
            .unwrap_err();
        // The message must point at expiry, since that is nearly always the
        // real cause of a 403 here.
        assert!(err.to_string().contains("expired"), "{err}");
        match err {
            NeteaseErr::UrlExpired { status, url } => {
                assert_eq!(status, 403);
                assert!(url.ends_with("/expired"));
            }
            other => panic!("expected UrlExpired, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn other_failures_are_not_reported_as_expiry() {
        let (base, server) = mock_cdn().await;
        let client = Client::with_base_url(&base).unwrap();

        let err = stream_audio(&client, &format!("{base}/boom"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, NeteaseErr::BadRequest(_)), "got {err:?}");
        server.abort();
    }

    /// A 416 is a recoverable answer, not a failure: it must be relayed with its
    /// `Content-Range` so the speaker can learn the length and retry, rather
    /// than surfacing as a generic error the way a real 4xx/5xx does.
    #[tokio::test]
    async fn range_not_satisfiable_is_relayed_not_an_error() {
        let (base, server) = mock_cdn().await;
        let client = Client::with_base_url(&base).unwrap();

        let audio = stream_audio(
            &client,
            &format!("{base}/rangetoobig"),
            Some("bytes=99999-"),
        )
        .await
        .expect("416 should be relayed, not an error");
        assert_eq!(audio.status, 416);
        assert_eq!(audio.content_range.as_deref(), Some("bytes */8192"));
        server.abort();
    }

    /// A connection that dies mid-track must not look like a track that simply
    /// ended: the caller has to be able to tell "finished" from "cut off".
    #[tokio::test]
    async fn mid_stream_failure_is_an_error_item_not_a_silent_truncation() {
        let (base, server) = mock_cdn().await;
        let client = Client::with_base_url(&base).unwrap();

        // Whether the abort lands before or after the response headers is up to
        // the runtime's scheduling, so accept either — what must never happen
        // is a clean, silent end to a body that was cut short.
        let stream = match stream_audio(&client, &format!("{base}/truncated"), None).await {
            Ok(stream) => stream,
            Err(e) => {
                assert!(matches!(e, NeteaseErr::Http(_)), "got {e:?}");
                server.abort();
                return;
            }
        };
        let mut body = stream.into_stream();
        let mut got = 0usize;
        let mut errored = false;
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => got += bytes.len(),
                Err(e) => {
                    assert!(matches!(e, NeteaseErr::Http(_)), "got {e:?}");
                    errored = true;
                    break;
                }
            }
        }
        assert!(errored, "short body ended silently after {got} bytes");
        server.abort();
    }

    #[tokio::test]
    async fn empty_url_is_rejected_before_any_request() {
        let client = Client::new().unwrap();
        let err = stream_audio(&client, "   ", None).await.unwrap_err();
        assert!(matches!(err, NeteaseErr::BadRequest(_)), "got {err:?}");
    }
}
