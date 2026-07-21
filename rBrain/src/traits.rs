//! The traits an assistant is assembled from, plus the values they pass around.
//!
//! All use [`async_trait`](async_trait::async_trait) because all are used as
//! trait objects, which native async-in-trait does not allow.

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where audio and speech come out: a speaker, a sound card, a test fake.
///
/// The methods are coarse and fire-and-forget: a remote speaker over a cloud API
/// cannot offer anything finer reliably.
#[async_trait::async_trait]
pub trait Speaker: Send + Sync {
    /// Say `text` out loud. Returns once the request is accepted, not once the
    /// speech has finished.
    async fn say(&self, text: &str) -> Result<()>;

    /// Start playing `url`. Devices that cannot fetch arbitrary URLs should return
    /// [`crate::BrainErr::Unsupported`].
    async fn play(&self, url: &str) -> Result<()>;

    /// Tell the user what is about to play, then play it.
    ///
    /// A device with a single audio output — where speech *replaces* playback
    /// rather than mixing — should override this to speak the announcement to
    /// completion before starting `url`, so neither cuts the other off. It is one
    /// call so that ordering and single-channel handling live with the device.
    async fn announce_then_play(&self, announcement: &str, url: &str) -> Result<()> {
        self.say(announcement).await?;
        self.play(url).await
    }

    /// Stop playback. Must be safe to call when nothing is playing.
    async fn stop(&self) -> Result<()>;

    /// Set the volume, `0..=100`. Implementations scale to their own range and
    /// clamp rather than erroring.
    async fn set_volume(&self, level: u8) -> Result<()>;

    /// Whether audio is currently coming out. The loop uses this to wait for a
    /// track to end, so a device that cannot report status should return
    /// [`crate::BrainErr::Unsupported`] rather than a guess.
    async fn is_playing(&self) -> Result<bool>;
}

/// Something the user said, with enough identity to tell it apart from the same
/// words said again a minute later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utterance {
    /// Identifier unique within one source — for a polling source, the record's
    /// id or timestamp, which lets a resumed poll skip what it has handled.
    pub id: String,
    pub text: String,
    /// Milliseconds since the Unix epoch — the unit the speaker APIs report.
    pub timestamp_ms: u64,
}

impl Utterance {
    pub fn new(id: impl Into<String>, text: impl Into<String>, timestamp_ms: u64) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            timestamp_ms,
        }
    }
}

/// Where utterances come in from.
///
/// Pull-based so both a polling implementation (XiaoAi has no push API) and a
/// streaming one (a websocket, a microphone) fit. `&mut self` so a poller can
/// remember how far it has read.
#[async_trait::async_trait]
pub trait UtteranceSource: Send {
    /// The next unseen utterance. Resolves only when one is available, so a poller
    /// should loop internally rather than return `None` for "nothing yet". `None`
    /// means the source is exhausted and ends the control loop; a transient
    /// failure should be retried internally, not surfaced.
    async fn next(&mut self) -> Option<Utterance>;
}

/// One piece of music, as far as anything outside its source needs to know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    /// Identifier meaningful only to the [`MusicSource`] that produced it — a
    /// NetEase song id, a file path, a URL. Round-trip it back to play it.
    pub id: String,
    pub title: String,
    /// Best-known artist, empty when the source does not know.
    pub artist: String,
    /// Name of the [`MusicSource`] this came from, so merged results route back.
    pub source: String,
    pub duration_ms: Option<u64>,
}

/// How to actually play a [`Track`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Playable {
    /// A URL *the device* fetches itself — so it must be reachable from the
    /// device's network, which rules out `localhost` for a hardware speaker.
    Url(String),
    /// A file on this machine. A URL-only device needs this served over HTTP first.
    LocalFile(PathBuf),
}

/// Somewhere music can be found: a local library, or a remote API.
#[async_trait::async_trait]
pub trait MusicSource: Send + Sync {
    /// Short identifier, copied into [`Track::source`]. Stable, lowercase, no
    /// spaces — e.g. `"local"`, `"netease"`.
    fn name(&self) -> &str;

    /// Find tracks matching a free-text query, best match first. The query comes
    /// from speech, so it is fuzzy; no matches is an empty `Vec`, not an error.
    async fn search(&self, query: &str) -> Result<Vec<Track>>;

    /// Turn a track into something playable.
    ///
    /// Separate from [`MusicSource::search`] because the answer is often
    /// short-lived: NetEase mints a signed, expiring URL per request. Resolve at
    /// the moment of playing and do not cache. A foreign [`Track::id`] is a
    /// [`crate::BrainErr::NotFound`].
    async fn resolve(&self, track: &Track) -> Result<Playable>;

    /// A random track, optionally restricted by a free-text filter. `Ok(None)`
    /// means nothing matches, which is not an error. No default: one built on
    /// `search` would have to pick the first result, silently the opposite of
    /// random.
    async fn random(&self, filter: Option<&str>) -> Result<Option<Track>>;
}
