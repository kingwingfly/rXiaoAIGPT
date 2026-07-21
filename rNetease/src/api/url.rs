//! Song URL resolution — `/weapi/song/enhance/player/url/v1`.
//!
//! Returns a plain `mp3`/`flac` CDN URL, **not** an `.ncm` (that container is
//! written client-side by the desktop app; no endpoint serves one — nothing to
//! decrypt here). The URL's `expi` is a **TTL in seconds** (~1200): resolve
//! just-in-time and never cache it; cache the song id instead. `url` is `null`
//! for tracks the session may not stream (VIP/region/withdrawn), which
//! [`song_url`] turns into [`SongUrlErr::Unavailable`] rather than a `None`.

use crate::{
    Client,
    client::ensure_ok,
    error::{NeteaseErr, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// The weapi path, i.e. what is appended to `{base}/weapi/`.
const PATH: &str = "song/enhance/player/url/v1";

/// Audio quality. Asking for more than the account is entitled to does not fail
/// — NetEase quietly downgrades, or returns a `null` url.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Level {
    /// 128 kbps mp3.
    Standard,
    /// 192 kbps mp3.
    Higher,
    /// 320 kbps mp3 — the default: the best a plain VIP reliably gets.
    #[default]
    Exhigh,
    /// FLAC.
    Lossless,
    /// 24-bit/96 kHz FLAC.
    Hires,
    /// 高清环绕声.
    Jyeffect,
    /// 沉浸环绕声. Requires SVIP, and forces `immerseType` on the request.
    Sky,
    /// 超清母带. Requires SVIP.
    Jymaster,
}

impl Level {
    /// The string NetEase expects in the `level` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Higher => "higher",
            Self::Exhigh => "exhigh",
            Self::Lossless => "lossless",
            Self::Hires => "hires",
            Self::Jyeffect => "jyeffect",
            Self::Sky => "sky",
            Self::Jymaster => "jymaster",
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Build the request body for `ids` at `level`. Two server quirks: `ids` is a
/// JSON array serialized into a *string* (`"[186016]"`) — a real array returns
/// empty `data` — and `sky` additionally requires `immerseType: "c51"`.
pub fn payload(ids: &[u64], level: Level) -> Value {
    let ids = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let mut body = json!({
        "ids": format!("[{ids}]"),
        "level": level.as_str(),
        // Lets the server hand back a flac url when the level allows; harmless
        // otherwise.
        "encodeType": "flac",
    });
    if level == Level::Sky {
        body["immerseType"] = json!("c51");
    }
    body
}

/// One entry of the response's `data[]`. The permissive shape: `url` may be
/// `null` (use [`song_url`] to avoid handling that).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SongUrlInfo {
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub id: u64,
    /// `null` when the track is not streamable for this session; expires after
    /// [`SongUrlInfo::ttl`].
    pub url: Option<String>,
    /// Bitrate in bits per second (`320000`, not `320`).
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub br: u32,
    /// Size of the file in bytes.
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub size: u64,
    pub md5: Option<String>,
    /// Container: `"mp3"`, `"flac"`, …
    #[serde(rename = "type")]
    pub format: Option<String>,
    /// **TTL in seconds**, not an absolute timestamp. See the module docs.
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub expi: u64,
    /// ReplayGain adjustment in dB, to be applied by the player.
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub gain: f64,
    /// `0` free, `1` VIP-only, `4` album-purchase, `8` freely playable for VIPs.
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub fee: i64,
    /// The quality actually served, which may be below the one requested.
    pub level: Option<String>,
    /// Present when only a preview clip is available; untyped, its shape varies.
    #[serde(rename = "freeTrialInfo")]
    pub free_trial_info: Option<Value>,
}

impl SongUrlInfo {
    /// Whether a full-length stream is available.
    pub fn is_available(&self) -> bool {
        self.url.as_deref().is_some_and(|u| !u.is_empty())
    }

    /// Whether a preview clip was offered — a sign the track is paid, not missing.
    pub fn has_free_trial(&self) -> bool {
        self.free_trial_info.as_ref().is_some_and(|v| !v.is_null())
    }

    /// How long the URL stays valid, counted from when the response arrived.
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.expi)
    }

    /// The playable URL, or a typed reason why there is none.
    pub fn playable(&self) -> std::result::Result<&str, SongUrlErr> {
        match self.url.as_deref() {
            Some(url) if !url.is_empty() => Ok(url),
            _ => Err(SongUrlErr::Unavailable {
                id: self.id,
                fee: self.fee,
                free_trial: self.has_free_trial(),
            }),
        }
    }
}

/// The response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SongUrlResponse {
    pub code: i64,
    #[serde(deserialize_with = "crate::serde_util::null_to_default")]
    pub data: Vec<SongUrlInfo>,
}

/// Why a song could not be turned into a playable URL. Separate from
/// [`NeteaseErr`] because "VIP-only" is an ordinary outcome a caller handles by
/// trying the next hit, not a transport failure.
#[derive(Debug, thiserror::Error)]
pub enum SongUrlErr {
    /// The request itself failed.
    #[error(transparent)]
    Netease(#[from] NeteaseErr),
    /// The request succeeded but `data` held no entry — usually a bad id.
    #[error("netease returned no url entry for song {id}")]
    NotFound { id: u64 },
    /// `url` was `null`: VIP-gated, region-locked or withdrawn.
    #[error("song {id} is not streamable (fee={fee}, preview_only={free_trial})")]
    Unavailable {
        id: u64,
        /// The `fee` NetEase reported: `1` VIP-only, `4` album purchase, …
        fee: i64,
        /// A preview clip was offered, so the track exists but is paid.
        free_trial: bool,
    },
}

/// Resolve one song id to a playable URL, immediately before playback. Returns
/// [`SongUrlErr::Unavailable`] for a gated track (never a `None` to unwrap); on
/// `Ok`, [`SongUrlInfo::playable`] cannot fail.
pub async fn song_url(
    client: &Client,
    id: u64,
    level: Level,
) -> std::result::Result<SongUrlInfo, SongUrlErr> {
    let info = song_urls(client, &[id], level)
        .await?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or(SongUrlErr::NotFound { id })?;
    info.playable()?;
    Ok(info)
}

/// Resolve several ids at once, including entries with a `null` url. Order is
/// NetEase's, not `ids`', so match on [`SongUrlInfo::id`].
pub async fn song_urls(client: &Client, ids: &[u64], level: Level) -> Result<Vec<SongUrlInfo>> {
    if ids.is_empty() {
        return Err(NeteaseErr::BadRequest("no song ids given".into()));
    }
    let value = client.post_weapi_value(PATH, &payload(ids, level)).await?;
    ensure_ok(&value)?;
    let parsed: SongUrlResponse = serde_json::from_value(value)?;
    Ok(parsed.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use serde_json::json;

    async fn mock(body: Value) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/weapi/song/enhance/player/url/v1",
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
    fn ids_are_a_stringified_array() {
        let body = payload(&[33894312], Level::Exhigh);
        assert_eq!(body["ids"], "[33894312]");
        assert_eq!(body["level"], "exhigh");
        assert_eq!(body["encodeType"], "flac");
        assert!(body.get("immerseType").is_none());

        assert_eq!(payload(&[1, 2, 3], Level::Standard)["ids"], "[1,2,3]");
    }

    #[test]
    fn sky_carries_immerse_type() {
        assert_eq!(payload(&[1], Level::Sky)["immerseType"], "c51");
        assert_eq!(payload(&[1], Level::Jymaster).get("immerseType"), None);
    }

    #[tokio::test]
    async fn resolves_a_playable_song() {
        let (base, server) = mock(json!({
            "code": 200,
            "data": [{
                "id": 33894312,
                "url": "http://m10.music.126.net/2018/930a9.mp3",
                "br": 320000,
                "size": 10691439,
                "md5": "a877",
                "type": "mp3",
                "expi": 1200,
                "gain": -2.0E-4,
                "fee": 0,
                "somethingNewNetEaseAdded": 42
            }]
        }))
        .await;
        let client = Client::with_base_url(&base).unwrap();

        let info = song_url(&client, 33894312, Level::Exhigh).await.unwrap();

        assert_eq!(
            info.playable().unwrap(),
            "http://m10.music.126.net/2018/930a9.mp3"
        );
        assert_eq!(info.br, 320000);
        assert_eq!(info.format.as_deref(), Some("mp3"));
        assert_eq!(info.ttl(), Duration::from_secs(1200)); // a TTL, not an absolute time

        server.abort();
    }

    /// The VIP case must be a typed error, never a panic.
    #[tokio::test]
    async fn vip_gated_song_is_a_typed_error() {
        let (base, server) = mock(json!({
            "code": 200,
            "data": [{
                "id": 1824045033,
                "url": null,
                "br": 0,
                "size": 0,
                "md5": null,
                "type": null,
                "expi": 0,
                "gain": 0.0,
                "fee": 1,
                "freeTrialInfo": { "start": 0, "end": 30 }
            }]
        }))
        .await;
        let client = Client::with_base_url(&base).unwrap();

        let err = song_url(&client, 1824045033, Level::Lossless)
            .await
            .unwrap_err();

        match err {
            SongUrlErr::Unavailable {
                id,
                fee,
                free_trial,
            } => {
                assert_eq!(id, 1824045033);
                assert_eq!(fee, 1);
                assert!(free_trial);
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }

        // The batch call reports it rather than dropping it.
        let all = song_urls(&client, &[1824045033], Level::Lossless)
            .await
            .unwrap();
        assert!(!all[0].is_available());
        assert!(all[0].has_free_trial());
        server.abort();
    }

    #[tokio::test]
    async fn missing_entry_is_not_found() {
        let (base, server) = mock(json!({ "code": 200, "data": [] })).await;
        let client = Client::with_base_url(&base).unwrap();
        let err = song_url(&client, 7, Level::Exhigh).await.unwrap_err();
        assert!(matches!(err, SongUrlErr::NotFound { id: 7 }));
        server.abort();
    }

    #[tokio::test]
    async fn non_200_code_propagates() {
        let (base, server) = mock(json!({ "code": 400, "message": "参数错误" })).await;
        let client = Client::with_base_url(&base).unwrap();
        let err = song_urls(&client, &[1], Level::Exhigh).await.unwrap_err();
        assert!(matches!(err, NeteaseErr::Api { code: 400, .. }));
        server.abort();
    }

    #[tokio::test]
    async fn empty_id_list_is_rejected_before_sending() {
        let client = Client::with_base_url("http://127.0.0.1:1").unwrap();
        let err = song_urls(&client, &[], Level::Exhigh).await.unwrap_err();
        assert!(matches!(err, NeteaseErr::BadRequest(_)));
    }
}
