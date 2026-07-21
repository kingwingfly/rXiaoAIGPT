//! Search — `/weapi/cloudsearch/pc`, the rich endpoint (`ar[]`/`al{}`, `dt` in
//! ms, a `privilege` block), not the thin legacy `search/get`.
//!
//! ```no_run
//! # async fn example() -> Result<(), netease::NeteaseErr> {
//! use netease::api::search::{SearchQuery, search_songs};
//! let client = netease::Client::new()?;
//! let songs = search_songs(&client, &SearchQuery::new("晴天").limit(5)).await?;
//! for song in &songs {
//!     println!("{} - {}", song.artist_names(), song.name);
//! }
//! # Ok(())
//! # }
//! ```

use crate::{
    Client,
    client::ensure_ok,
    error::{NeteaseErr, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// The weapi path, i.e. what is appended to `{base}/weapi/`.
const PATH: &str = "cloudsearch/pc";

/// What a query is searching *for* — the integer NetEase calls `type`. Only
/// [`SearchType::Song`] has typed result structs; the rest hold the magic numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SearchType {
    /// Songs — the only variant with a typed result today.
    #[default]
    Song,
    Album,
    Artist,
    Playlist,
    User,
    Mv,
    /// Lyrics: matches inside lyric text, but still returns song objects.
    Lyrics,
    /// Radio / podcast channels (电台).
    Radio,
    Video,
    /// 综合 — the "everything" tab of the web player; results are a mixed bag.
    Comprehensive,
}

impl SearchType {
    /// The integer NetEase expects in the `type` field.
    pub fn code(self) -> u32 {
        match self {
            Self::Song => 1,
            Self::Album => 10,
            Self::Artist => 100,
            Self::Playlist => 1000,
            Self::User => 1002,
            Self::Mv => 1004,
            Self::Lyrics => 1006,
            Self::Radio => 1009,
            Self::Video => 1014,
            Self::Comprehensive => 1018,
        }
    }
}

/// One search request, with the web player's defaults (songs, 30 results).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// Free text, matched against title, artist and album at once.
    pub keywords: String,
    pub kind: SearchType,
    /// NetEase silently clamps this; 30 is the web player's page size.
    pub limit: u32,
    pub offset: u32,
}

impl SearchQuery {
    /// A song search for `keywords` with the web player's defaults.
    pub fn new(keywords: impl Into<String>) -> Self {
        Self {
            keywords: keywords.into(),
            kind: SearchType::default(),
            limit: 30,
            offset: 0,
        }
    }

    /// Search for something other than songs.
    pub fn kind(mut self, kind: SearchType) -> Self {
        self.kind = kind;
        self
    }

    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    pub fn offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }

    /// The request body, before weapi encryption.
    pub fn payload(&self) -> Value {
        json!({
            "s": self.keywords,
            "type": self.kind.code(),
            "limit": self.limit,
            "offset": self.offset,
            // Without `total` the response omits the `*Count` fields.
            "total": true,
        })
    }
}

/// An artist as it appears inside a song's `ar[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Artist {
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub id: u64,
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub name: String,
}

/// A song's album (`al`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Album {
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub id: u64,
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub name: String,
    /// Cover art. Append `?param=200y200` to have NetEase resize it server-side.
    #[serde(rename = "picUrl")]
    pub pic_url: Option<String>,
}

/// A song in a search result — a subset of the object. Unknown fields are
/// ignored: NetEase adds and removes them without notice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Song {
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub id: u64,
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub name: String,
    /// `ar` — every credited artist, in billing order.
    #[serde(rename = "ar", deserialize_with = "crate::serde_util::null_to_default")]
    pub artists: Vec<Artist>,
    /// `al` — `Option` because it is absent on the odd malformed entry.
    #[serde(rename = "al")]
    pub album: Option<Album>,
    /// `dt` — duration in **milliseconds** (the legacy endpoint's `duration`).
    #[serde(rename = "dt", deserialize_with = "crate::serde_util::null_to_default")]
    pub duration_ms: u64,
    /// Alias id: nonzero when this is a cloud-disk copy of another song.
    #[serde(
        rename = "pst",
        deserialize_with = "crate::serde_util::null_to_default"
    )]
    pub pst: i64,
}

impl Song {
    /// Artists joined the way NetEase's own UI joins them.
    pub fn artist_names(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// Duration as a [`Duration`], since `dt`'s unit is easy to get wrong.
    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }
}

/// The `result` object of a song search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SongSearchResult {
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub songs: Vec<Song>,
    /// Total matches across all pages — only present when the request asked for
    /// `total: true`, which [`SearchQuery::payload`] always does.
    #[serde(
        rename = "songCount",
        deserialize_with = "crate::serde_util::null_to_default"
    )]
    pub song_count: u64,
}

/// The full envelope of a song search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SongSearchResponse {
    pub code: i64,
    /// Absent — not empty — when nothing matched at all.
    pub result: Option<SongSearchResult>,
}

/// Run `query` and return the matching songs. No matches is an empty `Vec`; a
/// non-200 envelope `code` is a [`NeteaseErr::Api`].
pub async fn search_songs(client: &Client, query: &SearchQuery) -> Result<Vec<Song>> {
    Ok(search_songs_full(client, query).await?.songs)
}

/// As [`search_songs`], but keeping `songCount` so a caller can page.
pub async fn search_songs_full(client: &Client, query: &SearchQuery) -> Result<SongSearchResult> {
    if query.kind != SearchType::Song && query.kind != SearchType::Lyrics {
        // Lyrics also returns song objects; other types return a different shape
        // that would silently deserialize as empty.
        return Err(NeteaseErr::BadRequest(format!(
            "search type {:?} does not return songs; use search_raw",
            query.kind
        )));
    }
    let value = search_raw(client, query).await?;
    let parsed: SongSearchResponse = serde_json::from_value(value)?;
    Ok(parsed.result.unwrap_or_default())
}

/// The untyped response, for search types with no result struct yet. The
/// envelope `code` is still checked.
pub async fn search_raw(client: &Client, query: &SearchQuery) -> Result<Value> {
    let value = client.post_weapi_value(PATH, &query.payload()).await?;
    ensure_ok(&value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use serde_json::json;

    /// Serve one canned JSON body at the search path.
    async fn mock(body: Value) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/weapi/cloudsearch/pc",
            post(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn payload_matches_the_web_player() {
        let query = SearchQuery::new("晴天").limit(5).offset(10);
        assert_eq!(
            query.payload(),
            json!({ "s": "晴天", "type": 1, "limit": 5, "offset": 10, "total": true })
        );
        let artists = SearchQuery::new("x").kind(SearchType::Artist);
        assert_eq!(artists.payload()["type"], 100);
    }

    #[tokio::test]
    async fn deserializes_a_normal_song_search() {
        let (base, server) = mock(json!({
            "code": 200,
            "result": {
                "songCount": 2,
                "songs": [{
                    "id": 186016,
                    "name": "晴天",
                    "ar": [{ "id": 6452, "name": "周杰伦" }],
                    "al": { "id": 18877, "name": "叶惠美", "picUrl": "https://p1.music.126.net/x.jpg" },
                    "dt": 269146,
                    "pst": 0
                }]
            }
        }))
        .await;
        let client = Client::with_base_url(&base).unwrap();

        let result = search_songs_full(&client, &SearchQuery::new("晴天"))
            .await
            .unwrap();

        assert_eq!(result.song_count, 2);
        let song = &result.songs[0];
        assert_eq!(song.id, 186016);
        assert_eq!(song.artist_names(), "周杰伦");
        assert_eq!(song.album.as_ref().unwrap().name, "叶惠美");
        assert_eq!(song.duration(), Duration::from_millis(269146));
        server.abort();
    }

    /// New or missing fields must not break a search.
    #[tokio::test]
    async fn tolerates_unknown_and_missing_fields() {
        let (base, server) = mock(json!({
            "code": 200,
            "result": {
                "songCount": 1,
                "brandNewTopLevelKey": { "nested": [1, 2, 3] },
                "songs": [{
                    "id": 1,
                    "name": "无名",
                    "ar": [{ "id": 2, "name": "甲", "tns": ["A"] }, { "id": 3, "name": "乙" }],
                    "privilege": { "fee": 1, "somethingNew": true },
                    "dt": 1000
                }]
            }
        }))
        .await;
        let client = Client::with_base_url(&base).unwrap();

        let songs = search_songs(&client, &SearchQuery::new("无名"))
            .await
            .unwrap();

        assert_eq!(songs.len(), 1);
        assert!(songs[0].album.is_none()); // `al` absent, not fatal
        assert_eq!(songs[0].artist_names(), "甲 / 乙");
        server.abort();
    }

    /// NetEase sends explicit `null`, which `#[serde(default)]` alone (missing
    /// keys only) does not cover; one null must not abort the search.
    #[tokio::test]
    async fn tolerates_explicit_nulls() {
        let (base, server) = mock(json!({
            "code": 200,
            "result": {
                "songCount": null,
                "songs": [{
                    "id": 1,
                    "name": null,
                    "ar": [{ "id": 2, "name": null }],
                    "al": null,
                    "dt": null
                }]
            }
        }))
        .await;
        let client = Client::with_base_url(&base).unwrap();

        let songs = search_songs(&client, &SearchQuery::new("x"))
            .await
            .expect("a null field must not fail the parse");
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].name, "");
        assert_eq!(songs[0].artists[0].name, "");
        assert_eq!(songs[0].duration_ms, 0);
        assert!(songs[0].album.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn no_match_is_an_empty_list_not_an_error() {
        let (base, server) = mock(json!({ "code": 200, "result": {} })).await;
        let client = Client::with_base_url(&base).unwrap();
        assert!(
            search_songs(&client, &SearchQuery::new("zzz"))
                .await
                .unwrap()
                .is_empty()
        );
        server.abort();
    }

    #[tokio::test]
    async fn non_200_code_is_an_api_error() {
        let (base, server) = mock(json!({ "code": 301, "msg": "需要登录" })).await;
        let client = Client::with_base_url(&base).unwrap();
        let err = search_songs(&client, &SearchQuery::new("x"))
            .await
            .unwrap_err();
        assert!(matches!(err, NeteaseErr::Api { code: 301, .. }));
        server.abort();
    }

    #[tokio::test]
    async fn typed_helper_refuses_non_song_types() {
        let client = Client::with_base_url("http://127.0.0.1:1").unwrap();
        let query = SearchQuery::new("x").kind(SearchType::Playlist);
        // Rejected before any request, so the unusable port is fine.
        let err = search_songs(&client, &query).await.unwrap_err();
        assert!(matches!(err, NeteaseErr::BadRequest(_)));
    }
}
