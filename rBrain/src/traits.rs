//! The four traits an assistant is assembled from, plus the values they pass
//! around.
//!
//! Every trait here is written with [`async_trait`](async_trait::async_trait)
//! rather than native `async fn` in traits. That is a deliberate trade: native
//! AFIT is not object-safe, and all four of these are used as trait objects — a
//! registry holds `Box<dyn Tool>`, and the control loop holds a
//! `Box<dyn Speaker>` it cannot know the concrete type of. The boxed future per
//! call is irrelevant next to the network round trips these wrap.

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A capability the model can invoke — one function in the function-calling
/// sense.
///
/// The three metadata methods are what gets serialised into the model's tool
/// list, and [`Tool::call`] is what runs when the model picks it. Keep
/// [`Tool::description`] written for the *model*, not for a developer: it is the
/// only thing telling it when this tool is the right one.
///
/// # Implementing
///
/// ```
/// use brain::{Result, Tool};
/// use serde_json::{Value, json};
///
/// struct Volume;
///
/// #[brain::async_trait]
/// impl Tool for Volume {
///     fn name(&self) -> &str {
///         "set_volume"
///     }
///
///     fn description(&self) -> &str {
///         "Set the speaker volume. Use when the user asks for it louder or quieter."
///     }
///
///     fn parameters(&self) -> Value {
///         json!({
///             "type": "object",
///             "properties": {
///                 "level": { "type": "integer", "minimum": 0, "maximum": 100 }
///             },
///             "required": ["level"]
///         })
///     }
///
///     async fn call(&self, args: Value) -> Result<String> {
///         let level = args["level"].as_u64().unwrap_or(50);
///         Ok(format!("volume set to {level}"))
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Identifier the model calls this tool by. Must be unique within a registry
    /// and stable: it appears in the model's output verbatim.
    fn name(&self) -> &str;

    /// Natural-language description of *when* to use this tool. This is prompt
    /// text, and the single biggest lever on whether the tool gets called
    /// correctly.
    fn description(&self) -> &str;

    /// JSON Schema for [`Tool::call`]'s argument object. Must be a schema of
    /// `"type": "object"`, since that is the only shape function-calling APIs
    /// accept.
    fn parameters(&self) -> serde_json::Value;

    /// Run the tool. `args` is what the model produced, validated against
    /// [`Tool::parameters`] only as far as the provider bothers to — treat it as
    /// untrusted and handle missing or ill-typed fields.
    ///
    /// The returned string goes straight back to the model as the tool result,
    /// so write it for the model to read: a short factual sentence, or the data
    /// it asked for. Returning `Err` is also fine — the loop is expected to feed
    /// the error text back so the model can recover — so reserve it for genuine
    /// failures rather than "no results".
    async fn call(&self, args: serde_json::Value) -> Result<String>;
}

/// Where audio and speech come out: a speaker, a local sound card, a test fake.
///
/// The methods are intentionally coarse and fire-and-forget in spirit. A remote
/// speaker reached over a cloud API cannot offer anything finer reliably, so
/// nothing here assumes low latency or exact state.
#[async_trait::async_trait]
pub trait Speaker: Send + Sync {
    /// Say `text` out loud (text-to-speech). Returns once the request has been
    /// accepted, not necessarily once the speech has finished.
    async fn say(&self, text: &str) -> Result<()>;

    /// Start playing `url`. Implementations that cannot fetch arbitrary URLs
    /// should return [`crate::BrainErr::Unsupported`] rather than silently doing
    /// nothing.
    async fn play(&self, url: &str) -> Result<()>;

    /// Stop playback. Must be safe to call when nothing is playing.
    async fn stop(&self) -> Result<()>;

    /// Set the volume, `0..=100`. Implementations scale to their own range and
    /// clamp rather than erroring on an out-of-range value.
    async fn set_volume(&self, level: u8) -> Result<()>;

    /// Whether audio is currently coming out.
    ///
    /// The control loop uses this to wait for a track to end — a device with no
    /// way to report status should say so with
    /// [`crate::BrainErr::Unsupported`], not return a guess, since a wrong
    /// `false` here ends playback early.
    async fn is_playing(&self) -> Result<bool>;
}

/// Something the user said, with enough identity to tell it apart from the same
/// words said again a minute later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utterance {
    /// Identifier unique within one source. For a polling source this is
    /// typically the record's timestamp or id; it is what lets a resumed poll
    /// skip what it has already handled.
    pub id: String,
    /// What was said, already transcribed.
    pub text: String,
    /// When it was said, in milliseconds since the Unix epoch.
    ///
    /// Milliseconds rather than a `SystemTime` because that is the unit the
    /// speaker APIs report and it survives being written to a state file
    /// unambiguously.
    pub timestamp_ms: u64,
}

impl Utterance {
    /// Convenience constructor.
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
/// Modelled as a pull-based stream so that both shapes fit: a polling
/// implementation (XiaoAi has no push API — its `next` sleeps and re-queries)
/// and a genuinely streaming one (a websocket or microphone — its `next` awaits
/// a channel). Because polling implementations need to remember how far they
/// have read, `next` takes `&mut self`.
#[async_trait::async_trait]
pub trait UtteranceSource: Send {
    /// The next utterance the caller has not seen.
    ///
    /// Resolves only when one is available, so a polling implementation should
    /// loop internally rather than returning `None` for "nothing yet". `None`
    /// means the source is **exhausted** — shut down, disconnected, end of a
    /// scripted test — and ends the control loop.
    ///
    /// A transient failure (one bad poll, one dropped connection) should be
    /// retried internally rather than surfaced, since there is no way to
    /// distinguish it from exhaustion at this signature.
    async fn next(&mut self) -> Option<Utterance>;
}

/// One piece of music, as far as anything outside its source needs to know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    /// Identifier meaningful to the [`MusicSource`] that produced it, and opaque
    /// to everyone else: a numeric NetEase song id, a relative file path, a URL.
    /// Round-trip it back to the same source to play it.
    pub id: String,
    pub title: String,
    /// Best-known artist. Empty when the source does not know — local files
    /// often do not.
    pub artist: String,
    /// Name of the [`MusicSource`] this came from, matching
    /// [`MusicSource::name`]. Lets results from several sources be merged and
    /// still routed back to the right one.
    pub source: String,
    /// Length, when the source knows it.
    pub duration_ms: Option<u64>,
}

/// How to actually play a [`Track`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Playable {
    /// A URL the playback device fetches itself.
    ///
    /// Note *the device* fetches it, not this process: it has to be reachable
    /// from the device's network, which rules out `localhost` for a hardware
    /// speaker.
    Url(String),
    /// A file on this machine. A device that can only play URLs needs this
    /// served over HTTP first.
    LocalFile(PathBuf),
}

/// Somewhere music can be found: a local library, or a remote API.
#[async_trait::async_trait]
pub trait MusicSource: Send + Sync {
    /// Short identifier, copied into [`Track::source`]. Stable, lowercase, no
    /// spaces — e.g. `"local"`, `"netease"`.
    fn name(&self) -> &str;

    /// Find tracks matching a free-text query, best match first.
    ///
    /// The query comes from speech, so it is fuzzy and may be nonsense. No
    /// matches is an empty `Vec`, not an error.
    async fn search(&self, query: &str) -> Result<Vec<Track>>;

    /// Turn a track into something playable.
    ///
    /// Separate from [`MusicSource::search`] because the answer is often
    /// short-lived and expensive: NetEase mints a signed, expiring URL per
    /// request. Resolve at the moment of playing, and do not cache the result.
    ///
    /// `track` should be one this source produced; a foreign [`Track::id`] is a
    /// [`crate::BrainErr::NotFound`].
    async fn resolve(&self, track: &Track) -> Result<Playable>;

    /// A random track, optionally restricted by a free-text filter (an artist
    /// name, a genre — whatever the source can make of it).
    ///
    /// "Play me something" is common enough to deserve its own method rather
    /// than a search the caller then picks from: a local library can sample its
    /// whole index, which searching cannot express. `Ok(None)` means the source
    /// has nothing matching, which is not an error.
    ///
    /// No default implementation on purpose — a default built on
    /// [`MusicSource::search`] would have to pick the first result, which is the
    /// opposite of random and would be wrong silently.
    async fn random(&self, filter: Option<&str>) -> Result<Option<Track>>;
}
