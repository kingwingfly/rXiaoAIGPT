//! Errors this crate can produce.

/// Anything that can go wrong talking to NetEase.
#[derive(Debug, thiserror::Error)]
pub enum NeteaseErr {
    /// The request never completed: DNS, TLS, timeout, connection reset.
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// A response arrived but was not the JSON we expected. NetEase changes
    /// response shapes without notice, so this is a routine failure mode.
    #[error("could not decode response: {0}")]
    Decode(#[from] serde_json::Error),
    /// NetEase answered with a non-200 `code` in its JSON envelope. Note this is
    /// independent of the HTTP status, which is almost always 200.
    #[error("netease returned code {code}{}", .message.as_deref().map(|m| format!(": {m}")).unwrap_or_default())]
    Api { code: i64, message: Option<String> },
    /// Serialising the request payload failed, or a request was malformed
    /// before it was ever sent.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Reading or writing the cached session failed.
    #[error("session io failed: {0}")]
    Io(#[from] std::io::Error),
    /// A CDN audio URL was refused. Resolved URLs carry `expi: 1200` and stop
    /// working ~20 minutes after resolution, so a `403`/`404` here almost never
    /// means "no such track" — it means the URL was cached or resolved too far
    /// ahead of playback. Resolve just-in-time and retry.
    #[error(
        "cdn refused audio url with {status} (most likely expired — netease urls live ~20 min, resolve just before playing): {url}"
    )]
    UrlExpired { status: u16, url: String },
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = NeteaseErr> = std::result::Result<T, E>;
