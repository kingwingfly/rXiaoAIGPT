//! Streaming a remote audio file straight through to an HTTP response — a pipe,
//! not a policy. [`stream_audio`] returns once the upstream *headers* arrive and
//! forwards bytes as they land, so a multi-megabyte track is never buffered.
//!
//! The URL must be resolved just-in-time (`expi: 1200`, ~20 min), so a `403`/
//! `404` is reported as [`crate::NeteaseErr::UrlExpired`] — on this path it
//! nearly always means "resolved too long ago", not "no such track". `Range` is
//! forwarded verbatim and the upstream status/`Content-Range` propagated back so
//! seeking works. No HTTPS upgrade: some CDN hosts are plain `http://`.

use crate::{
    client::Client,
    error::{NeteaseErr, Result},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use std::pin::Pin;

/// A boxed byte stream, so the web layer need not name `reqwest`'s stream type.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// An upstream audio response, opened but not consumed — metadata for the HTTP
/// layer to build its response, and the body left as a stream.
pub struct AudioStream {
    /// Upstream status, passed through: `206` when a `Range` was honoured.
    /// Normalising to `200` would break seeking.
    pub status: u16,
    pub content_type: Option<String>,
    /// The length of *this* response — the range's length on a `206`, not the track's.
    pub content_length: Option<u64>,
    /// `Content-Range` (`bytes 100-199/4096`), present on a `206`.
    pub content_range: Option<String>,
    /// `Accept-Ranges` — forwarding it is what tells the speaker it may seek.
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

    /// Take the body (once). A failure after the first byte arrives as an `Err`
    /// item, so the web layer can tell a truncated track from a complete one.
    pub fn into_stream(self) -> ByteStream {
        self.body
    }
}

/// Open `url` for streaming, forwarding `range` (a raw header value like
/// `Some("bytes=1000-")`) upstream untouched. The URL must have been resolved
/// moments ago (see module docs).
///
/// # Errors
///
/// [`NeteaseErr::UrlExpired`] for a `403`/`404` (a stale URL),
/// [`NeteaseErr::BadRequest`] for any other non-success status or an empty URL,
/// [`NeteaseErr::Http`] if the request never completed.
pub async fn stream_audio(client: &Client, url: &str, range: Option<&str>) -> Result<AudioStream> {
    stream_audio_with(client.http(), url, range).await
}

/// As [`stream_audio`], but against a bare [`reqwest::Client`] — streaming needs
/// nothing from the NetEase session.
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
    // `416 Range Not Satisfiable` is recoverable: it carries `Content-Range:
    // bytes */N` telling the client the real length to clamp and retry, so relay
    // it rather than collapse it into a 502 that discards that header.
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
    // On a 206 this describes the slice, not the track.
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
            // A range past EOF: 416 with the real length, must be relayed.
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
            // A chunk, then a failure: the CDN dying mid-track.
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

    /// Minimal `bytes=start-end` / `bytes=start-` parser for the mock.
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

    /// An open-ended range (what a reconnecting player sends) runs to the end.
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

    /// A 416 is relayed with its `Content-Range`, not surfaced as an error.
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

    /// A connection dying mid-track must be distinguishable from a clean end.
    #[tokio::test]
    async fn mid_stream_failure_is_an_error_item_not_a_silent_truncation() {
        let (base, server) = mock_cdn().await;
        let client = Client::with_base_url(&base).unwrap();

        // The abort may land before or after the headers, so accept either — what
        // must never happen is a clean, silent end to a cut-short body.
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
