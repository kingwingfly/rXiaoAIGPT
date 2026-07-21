//! Errors this crate can produce.

/// Anything that can go wrong talking to NetEase.
#[derive(Debug, thiserror::Error)]
pub enum NeteaseErr {
    /// The request never completed: DNS, TLS, timeout, connection reset.
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// A response arrived but was not the expected JSON. NetEase changes shapes
    /// without notice, so this is routine.
    #[error("could not decode response: {0}")]
    Decode(#[from] serde_json::Error),
    /// A non-200 `code` in the JSON envelope, independent of the HTTP status.
    #[error("netease returned code {code}{}", .message.as_deref().map(|m| format!(": {m}")).unwrap_or_default())]
    Api { code: i64, message: Option<String> },
    /// The request was malformed before it was sent.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Reading or writing the cached session failed.
    #[error("session io failed: {0}")]
    Io(#[from] std::io::Error),
    /// A CDN audio URL was refused — almost always because it expired (`expi:
    /// 1200`, ~20 min), not because the track is missing. Resolve just-in-time.
    #[error(
        "cdn refused audio url with {status} (most likely expired — netease urls live ~20 min, resolve just before playing): {url}"
    )]
    UrlExpired { status: u16, url: String },
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = NeteaseErr> = std::result::Result<T, E>;
