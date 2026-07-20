//! Remote control of XiaoAi speakers (小爱音箱) through Xiaomi's cloud APIs.
//!
//! Start with [`load_or_login_and_save_with_env`] to obtain an [`AuthData`],
//! then [`device_by_alias`] to find the speaker. Every operation is built with
//! [`OpPayloadBuilder`] and sent with `OpApi::request`:
//!
//! ```rust ignore
//! let auth_data = load_or_login_and_save_with_env("auth_data.json").await?;
//! let device = device_by_alias(&auth_data, "卧室的小爱").await?;
//! let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).speak("Hello!");
//! let resp: OpResponse = OpApi::request(payload).await?;
//! ```
//!
//! # Module map
//!
//! - [`account`] — the two-step login, its interactive verification flow, and
//!   caching of the result.
//! - [`op`] — operations on a speaker (speak, volume, play/pause, play URL,
//!   status), all of which POST to the same `ubus` endpoint.
//! - [`record`] — reading the speaker's conversation history.
//! - [`sid`] — the Xiaomi service the login is scoped to.

/// Cached credentials at the workspace root. Anchored to the manifest so tests
/// find it regardless of the working directory `cargo test` picks.
#[cfg(test)]
pub(crate) const AUTH_DATA_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../auth_data.json");

pub mod account;
pub mod error;
pub mod op;
pub mod record;
mod serde_util;
pub mod sid;

pub use account::{
    AuthData, LoginFlow, Verification, load_or_login_and_save, load_or_login_and_save_with_env,
    login, login_with_env, refresh, try_login,
};
pub use api_req::{ApiCaller, error::ApiErr};
pub use error::XiaoaiErr;
pub use op::{Device, OpApi, OpPayloadBuilder, OpResponse, XiaoaiStatus, device_by_alias};
pub use record::{
    Answer, Audio, AudioInfo, Data, LastAskPayload, LastAskResponse, Record, RecordApi, Tts,
};
