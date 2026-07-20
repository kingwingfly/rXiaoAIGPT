//! The error type the traits are written against.

/// Anything an implementation of one of this crate's traits can fail with.
///
/// The variants are deliberately generic: this crate cannot name
/// `reqwest::Error` or `xiaoai::XiaoaiErr` without acquiring exactly the
/// dependencies it exists to avoid. Implementations map their own errors into
/// [`BrainErr::Backend`], usually via [`BrainErr::backend`].
#[derive(Debug, thiserror::Error)]
pub enum BrainErr {
    /// The underlying device, API, or filesystem failed.
    #[error("{0}")]
    Backend(String),

    /// The operation is meaningless for this implementation — asking a
    /// text-to-speech-only device to play a URL, say.
    ///
    /// Distinct from [`BrainErr::Backend`] because it is permanent: retrying,
    /// or trying a different argument, will not help.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),

    /// A tool was called with arguments it could not make sense of. The message
    /// is fed back to the model, so phrase it as a correction it can act on.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),

    /// The requested track, tool, or device does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// The backend rejected us for lack of (or expiry of) credentials.
    #[error("not authenticated: {0}")]
    Auth(String),

    /// A JSON payload — tool arguments, a config blob — was malformed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl BrainErr {
    /// Wrap any error as a [`BrainErr::Backend`].
    ///
    /// The idiomatic bridge from an implementation's own error type:
    /// `.map_err(BrainErr::backend)?`.
    pub fn backend(e: impl std::fmt::Display) -> Self {
        Self::Backend(e.to_string())
    }
}

/// Convenience alias used throughout the crate and by implementors.
pub type Result<T, E = BrainErr> = std::result::Result<T, E>;
