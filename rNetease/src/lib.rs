//! A client for NetEase Cloud Music (网易云音乐)'s private web-player API, whose
//! requests are encrypted under the **weapi** scheme ([`crypto`]). It is a plain
//! API client and depends on no other crate in this workspace.
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

pub mod api;
pub mod client;
pub mod crypto;
mod error;
pub mod session;
pub mod stream;

pub(crate) mod serde_util {
    use serde::{Deserialize, Deserializer};

    /// Deserialize `T`, mapping an explicit `null` to `T::default()`. NetEase
    /// returns `null` for fields like an artist's `name`, which `#[serde(default)]`
    /// (missing keys only) does not cover.
    pub(crate) fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de> + Default,
    {
        Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
    }
}

pub use api::search::{SearchQuery, Song};
pub use api::url::{Level, SongUrlErr};
pub use client::Client;
pub use crypto::WeapiRequest;
pub use error::{NeteaseErr, Result};
pub use session::Session;
