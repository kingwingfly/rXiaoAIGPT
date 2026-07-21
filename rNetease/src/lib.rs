//! A client for NetEase Cloud Music (网易云音乐).
//!
//! NetEase publishes no API. What exists is the private one its own web player
//! uses, whose requests are encrypted by the player's JavaScript under a scheme
//! known as **weapi**. This crate reimplements that scheme ([`crypto`]) and
//! wraps it in an HTTP client that carries a session ([`client`]), so that
//! endpoint modules under [`api`] can be written in terms of plain JSON.
//!
//! ```no_run
//! # async fn example() -> Result<(), netease::NeteaseErr> {
//! let client = netease::Client::new()?;
//! let resp: serde_json::Value = client
//!     .post_weapi("search/get", &serde_json::json!({ "s": "晴天", "type": 1 }))
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Scope
//!
//! This crate knows nothing about speakers, agents, or intent parsing — it is a
//! plain API client and depends on none of the other crates in this workspace.
//! Anything speaker-shaped belongs behind the traits in the `brain` crate.

pub mod api;
pub mod client;
pub mod crypto;
mod error;
pub mod session;
pub mod stream;

/// Deserialization helpers for NetEase's loose JSON.
pub(crate) mod serde_util {
    use serde::{Deserialize, Deserializer};

    /// Deserialize `T`, mapping an explicit `null` to `T::default()`.
    ///
    /// A container-level `#[serde(default)]` only fills a *missing* key; a key
    /// present with value `null` still fails a non-`Option` field. NetEase does
    /// exactly that — a `"name": null` on one artist row would otherwise abort
    /// the whole search parse — so every non-optional field that could come back
    /// null carries this. Missing keys are still handled by `#[serde(default)]`,
    /// which never calls this.
    pub(crate) fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de> + Default,
    {
        Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
    }
}

// The two types every caller needs by name; everything else stays namespaced
// under `api::` so sibling endpoint modules cannot collide here.
pub use api::search::{SearchQuery, Song};
pub use api::url::{Level, SongUrlErr};
pub use client::Client;
pub use crypto::WeapiRequest;
pub use error::{NeteaseErr, Result};
pub use session::Session;
