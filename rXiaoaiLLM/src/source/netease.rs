//! NetEase Cloud Music, as a [`MusicSource`]. The `netease` crate is trait-free,
//! so this adapter lives in the binary that owns both dependencies.
//!
//! A resolved CDN URL carries `expi: 1200` (dead ~20 minutes after minting), so
//! the speaker is never handed one. [`NeteaseSource::resolve`] hands out a proxy
//! URL on our own server, and [`NeteaseSource::router`] resolves the CDN URL
//! just-in-time when the speaker asks; the expiring URL is never stored.
//! `resolve` still calls NetEase once, only to turn a VIP-gated track into a
//! sayable error rather than a silent failure — that URL is discarded.

use axum::{
    Router,
    body::Body,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use brain::{BrainErr, MusicSource, Playable, Result, Track};
use netease::{
    Client,
    api::{
        search::{SearchQuery, search_songs},
        url::{Level, SongUrlErr, song_url},
    },
    stream::stream_audio,
};
use rand::seq::IndexedRandom as _;
use std::path::Path;

/// Name this source is known by, and the value written into [`Track::source`].
const NAME: &str = "netease";

/// Path prefix of the streaming proxy.
const PROXY_PREFIX: &str = "netease";

/// Search hits reported. The model only plays the first one or two; more is
/// prompt weight for nothing.
const MAX_RESULTS: u32 = 10;

/// NetEase, through the account in the session file (or anonymously, at reduced
/// quality). `base_url` is the speaker-facing prefix the router is mounted under.
#[derive(Debug, Clone)]
pub struct NeteaseSource {
    client: Client,
    base_url: String,
    level: Level,
}

impl NeteaseSource {
    /// Wrap an existing client (tests point one at a mock; production wants
    /// [`NeteaseSource::from_session_file`]).
    pub fn new(client: Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            level: Level::default(),
        }
    }

    /// Resume the login cached at `path`, falling back to an anonymous client. A
    /// missing session is not an error: NetEase still answers searches and serves
    /// free tracks, and refusing to start over a logged-out music service is worse.
    pub fn from_session_file(
        path: impl AsRef<Path>,
        base_url: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let client = match netease::Session::load_opt(path) {
            Ok(Some(session)) => session.client()?,
            Ok(None) => {
                tracing::warn!(path = %path.display(), "no netease session cached; going anonymous");
                Client::new()?
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "unreadable netease session; going anonymous");
                Client::new()?
            }
        };
        Ok(Self::new(client, base_url))
    }

    /// Ask for a different audio quality — above `exhigh` needs VIP, and asking
    /// for more than the session allows means fewer playable tracks, not better.
    // No config knob wires this; kept because changing quality is a one-liner here.
    #[allow(dead_code)]
    #[must_use]
    pub fn with_level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// The streaming proxy, merged into the binary's router. Serves
    /// `GET /netease/{id}`, tolerating a trailing extension (`186016.mp3`).
    pub fn router(&self) -> Router {
        Router::new()
            .route(&format!("/{PROXY_PREFIX}/{{id}}"), get(proxy))
            .with_state(Proxy {
                client: self.client.clone(),
                level: self.level,
            })
    }

    /// The URL the speaker is handed for `id`.
    fn proxy_url(&self, id: u64) -> String {
        format!("{}/{PROXY_PREFIX}/{id}", self.base_url)
    }
}

#[brain::async_trait]
impl MusicSource for NeteaseSource {
    fn name(&self) -> &str {
        NAME
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(BrainErr::InvalidArguments(
                "a search needs something to search for".into(),
            ));
        }
        let songs = search_songs(&self.client, &SearchQuery::new(query).limit(MAX_RESULTS))
            .await
            .map_err(BrainErr::backend)?;
        Ok(songs
            .iter()
            .map(|song| Track {
                // The id (stable), not the title: it is what `resolve` needs.
                id: song.id.to_string(),
                title: song.name.clone(),
                artist: song.artist_names(),
                source: NAME.to_string(),
                duration_ms: (song.duration_ms > 0).then_some(song.duration_ms),
            })
            .collect())
    }

    async fn resolve(&self, track: &Track) -> Result<Playable> {
        if track.source != NAME {
            return Err(BrainErr::NotFound(format!(
                "{} is not a netease track",
                track.id
            )));
        }
        let id: u64 = track.id.parse().map_err(|_| {
            BrainErr::InvalidArguments(format!("`{}` is not a netease song id", track.id))
        })?;

        // Availability check only; the URL is discarded (see module docs).
        match song_url(&self.client, id, self.level).await {
            Ok(info) => {
                tracing::debug!(id, ttl = ?info.ttl(), "netease track is playable");
                Ok(Playable::Url(self.proxy_url(id)))
            }
            // Phrased for the model: "needs a membership", not a `fee` code.
            Err(SongUrlErr::Unavailable {
                fee, free_trial, ..
            }) => Err(BrainErr::Backend(unavailable_message(
                &track.title,
                fee,
                free_trial,
            ))),
            Err(SongUrlErr::NotFound { id }) => {
                Err(BrainErr::NotFound(format!("网易云找不到编号 {id} 的歌曲")))
            }
            Err(SongUrlErr::Netease(e)) => Err(BrainErr::backend(e)),
        }
    }

    /// NetEase has no "any song" endpoint, so a random pick needs a filter to
    /// search and choose from; without one, `Ok(None)` lets the caller fall
    /// through to a library that can sample its whole index.
    async fn random(&self, filter: Option<&str>) -> Result<Option<Track>> {
        let Some(filter) = filter.map(str::trim).filter(|f| !f.is_empty()) else {
            tracing::debug!("netease cannot pick at random without a filter");
            return Ok(None);
        };
        let tracks = self.search(filter).await?;
        Ok(tracks.choose(&mut rand::rng()).cloned())
    }
}

/// Why a track would not resolve, in a sentence a speaker can say.
fn unavailable_message(title: &str, fee: i64, free_trial: bool) -> String {
    let reason = match (fee, free_trial) {
        // A preview clip on offer means it is paid, not missing.
        (_, true) => "只能试听，需要会员",
        (1 | 8, _) => "需要会员",
        (4, _) => "需要购买专辑",
        _ => "在网易云上无法播放（可能是版权下架或地区限制）",
    };
    format!("《{title}》{reason}")
}

/// The proxy route's state: a client to resolve and stream with.
#[derive(Clone)]
struct Proxy {
    client: Client,
    level: Level,
}

/// Resolve `id` now and stream it through. The `Range` header and the upstream's
/// status/range headers pass verbatim — answering `200` to a ranged request
/// makes seeking silently do nothing.
#[cfg_attr(debug_assertions, axum::debug_handler)]
async fn proxy(
    State(proxy): State<Proxy>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    // `186016.mp3` and `186016` mean the same track.
    let id = id.rsplit_once('.').map_or(id.as_str(), |(stem, _)| stem);
    let Ok(id) = id.parse::<u64>() else {
        return (StatusCode::BAD_REQUEST, "Not a netease song id").into_response();
    };

    let info = match song_url(&proxy.client, id, proxy.level).await {
        Ok(info) => info,
        Err(SongUrlErr::NotFound { .. }) => {
            return (StatusCode::NOT_FOUND, "No such song").into_response();
        }
        Err(SongUrlErr::Unavailable { .. }) => {
            // The session can expire or the track be withdrawn between `resolve`
            // and the speaker asking.
            return (StatusCode::FORBIDDEN, "Song is not streamable").into_response();
        }
        Err(e) => {
            tracing::warn!(id, error = %e, "cannot resolve netease song");
            return (StatusCode::BAD_GATEWAY, "Cannot resolve song").into_response();
        }
    };
    // The CDN URL never leaves this scope.
    let Ok(url) = info.playable() else {
        return (StatusCode::FORBIDDEN, "Song is not streamable").into_response();
    };

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let audio = match stream_audio(&proxy.client, url, range).await {
        Ok(audio) => audio,
        Err(e) => {
            tracing::warn!(id, error = %e, "cannot open netease stream");
            return (StatusCode::BAD_GATEWAY, "Cannot open audio stream").into_response();
        }
    };

    let mut builder = Response::builder()
        .status(StatusCode::from_u16(audio.status).unwrap_or(StatusCode::OK))
        // Fallback content type: a speaker given none guesses, and the free tier
        // is mp3.
        .header(
            header::CONTENT_TYPE,
            audio.content_type.as_deref().unwrap_or("audio/mpeg"),
        );
    // Length is never rewritten: on a `206` it describes the slice.
    if let Some(length) = audio.content_length {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }
    if let Some(content_range) = &audio.content_range {
        builder = builder.header(header::CONTENT_RANGE, content_range);
    }
    if let Some(accept_ranges) = &audio.accept_ranges {
        builder = builder.header(header::ACCEPT_RANGES, accept_ranges);
    }
    builder
        .body(Body::from_stream(audio.into_stream()))
        .unwrap_or_else(|e| {
            tracing::warn!(id, error = %e, "cannot build proxy response");
            (StatusCode::INTERNAL_SERVER_ERROR, "Cannot build response").into_response()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, http::Request, routing::post};
    use http_body_util::BodyExt as _;
    use serde_json::{Value, json};
    use tower::ServiceExt as _;

    /// A few KB of deterministic "audio", long enough to span several chunks.
    fn fake_audio() -> Vec<u8> {
        (0..4096u32).map(|i| (i % 251) as u8).collect()
    }

    /// A stand-in NetEase: the two weapi endpoints plus a "CDN" path. weapi
    /// responses are plain JSON, so the mock only answers, never decrypts.
    async fn mock(search: Value, url: Value) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        // Point the url fixture back at this server, whose address is only now known.
        let url = serde_json::from_str::<Value>(&url.to_string().replace("{base}", &base)).unwrap();
        let app = Router::new()
            .route(
                "/weapi/cloudsearch/pc",
                post(move || {
                    let search = search.clone();
                    async move { Json(search) }
                }),
            )
            .route(
                "/weapi/song/enhance/player/url/v1",
                post(move || {
                    let url = url.clone();
                    async move { Json(url) }
                }),
            )
            .route(
                "/cdn/song.mp3",
                get(|headers: HeaderMap| async move {
                    let audio = fake_audio();
                    match headers.get(header::RANGE) {
                        Some(_) => (
                            StatusCode::PARTIAL_CONTENT,
                            [
                                (header::CONTENT_TYPE, "audio/mpeg".to_string()),
                                (header::CONTENT_RANGE, format!("bytes 0-99/{}", audio.len())),
                                (header::ACCEPT_RANGES, "bytes".to_string()),
                            ],
                            audio[..100].to_vec(),
                        )
                            .into_response(),
                        None => (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "audio/mpeg")],
                            audio,
                        )
                            .into_response(),
                    }
                }),
            );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, handle)
    }

    fn search_hit() -> Value {
        json!({
            "code": 200,
            "result": {
                "songCount": 1,
                "songs": [{
                    "id": 186016,
                    "name": "晴天",
                    "ar": [{ "id": 6452, "name": "周杰伦" }],
                    "al": { "id": 32311, "name": "叶惠美" },
                    "dt": 269000
                }]
            }
        })
    }

    fn playable() -> Value {
        json!({
            "code": 200,
            "data": [{
                "id": 186016, "url": "{base}/cdn/song.mp3", "br": 320000,
                "size": 4096, "type": "mp3", "expi": 1200, "fee": 0
            }]
        })
    }

    fn vip_only() -> Value {
        json!({
            "code": 200,
            "data": [{
                "id": 186016, "url": null, "br": 0, "size": 0, "expi": 0,
                "fee": 1, "freeTrialInfo": { "start": 0, "end": 30 }
            }]
        })
    }

    async fn source(base: &str) -> NeteaseSource {
        NeteaseSource::new(
            Client::with_base_url(base).unwrap(),
            "http://10.0.0.1:3000/",
        )
    }

    #[tokio::test]
    async fn search_maps_songs_onto_tracks() {
        let (base, server) = mock(search_hit(), playable()).await;
        let source = source(&base).await;

        let hits = source.search("晴天").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "186016");
        assert_eq!(hits[0].title, "晴天");
        assert_eq!(hits[0].artist, "周杰伦");
        assert_eq!(hits[0].source, "netease");
        assert_eq!(hits[0].duration_ms, Some(269_000));

        assert!(matches!(
            source.search("  ").await,
            Err(BrainErr::InvalidArguments(_))
        ));
        server.abort();
    }

    /// The speaker is pointed at us, never the CDN (whose URL expires).
    #[tokio::test]
    async fn resolve_points_at_our_own_proxy_not_the_cdn() {
        let (base, server) = mock(search_hit(), playable()).await;
        let source = source(&base).await;

        let track = source.search("晴天").await.unwrap().remove(0);
        let Playable::Url(url) = source.resolve(&track).await.unwrap() else {
            panic!("netease tracks must resolve to a URL the speaker can fetch");
        };
        assert_eq!(url, "http://10.0.0.1:3000/netease/186016");
        assert!(
            !url.contains("cdn"),
            "a CDN url must never reach the speaker"
        );

        let foreign = Track {
            source: "local".into(),
            ..track
        };
        assert!(matches!(
            source.resolve(&foreign).await,
            Err(BrainErr::NotFound(_))
        ));
        server.abort();
    }

    /// The VIP case arrives as something sayable, not a panic or opaque code.
    #[tokio::test]
    async fn a_vip_track_explains_itself() {
        let (base, server) = mock(search_hit(), vip_only()).await;
        let source = source(&base).await;

        let track = source.search("晴天").await.unwrap().remove(0);
        let err = source.resolve(&track).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("会员"), "{message}");
        assert!(message.contains("晴天"), "{message}");
        server.abort();
    }

    #[test]
    fn unavailable_messages_name_the_actual_obstacle() {
        assert!(unavailable_message("x", 1, false).contains("需要会员"));
        assert!(unavailable_message("x", 1, true).contains("试听"));
        assert!(unavailable_message("x", 4, false).contains("专辑"));
        assert!(unavailable_message("x", 0, false).contains("无法播放"));
    }

    #[tokio::test]
    async fn random_needs_a_filter_to_have_anything_to_pick_from() {
        let (base, server) = mock(search_hit(), playable()).await;
        let source = source(&base).await;

        assert_eq!(
            source.random(Some("周杰伦")).await.unwrap().unwrap().title,
            "晴天"
        );
        // Not an error: the caller falls through to a source that can sample.
        assert!(source.random(None).await.unwrap().is_none());
        assert!(source.random(Some(" ")).await.unwrap().is_none());
        server.abort();
    }

    async fn proxy_get(source: &NeteaseSource, uri: &str, range: Option<&str>) -> Response {
        let mut req = Request::builder().uri(uri);
        if let Some(range) = range {
            req = req.header(header::RANGE, range);
        }
        source
            .router()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// The proxy resolves just-in-time and streams the bytes through unchanged.
    #[tokio::test]
    async fn the_proxy_resolves_just_in_time_and_streams_through() {
        let (base, server) = mock(search_hit(), playable()).await;
        let source = source(&base).await;

        let resp = proxy_get(&source, "/netease/186016", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "audio/mpeg");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), fake_audio().as_slice());

        // A trailing extension is tolerated.
        let resp = proxy_get(&source, "/netease/186016.mp3", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        server.abort();
    }

    /// A ranged request is forwarded and the `206` propagated, so seeking works.
    #[tokio::test]
    async fn a_range_request_is_forwarded_and_206_propagated() {
        let (base, server) = mock(search_hit(), playable()).await;
        let source = source(&base).await;

        let resp = proxy_get(&source, "/netease/186016", Some("bytes=0-99")).await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 0-99/4096");
        assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.len(), 100);
        server.abort();
    }

    #[tokio::test]
    async fn the_proxy_refuses_nonsense_and_gated_tracks_without_panicking() {
        let (base, server) = mock(search_hit(), vip_only()).await;
        let source = source(&base).await;

        let resp = proxy_get(&source, "/netease/nonsense", None).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = proxy_get(&source, "/netease/186016", None).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        server.abort();
    }
}
