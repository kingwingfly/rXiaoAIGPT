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

// The two types every caller needs by name; everything else stays namespaced
// under `api::` so sibling endpoint modules cannot collide here.
pub use api::search::{SearchQuery, Song};
pub use api::url::{Level, SongUrlErr};
pub use client::Client;
pub use crypto::WeapiRequest;
pub use error::{NeteaseErr, Result};
