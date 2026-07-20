//! Endpoint wrappers, one module per NetEase API.
//!
//! Each module here exposes plain typed
//! functions taking a [`crate::Client`] — build the request as a
//! [`serde_json::Value`], hand it to [`crate::Client::post_weapi`], and let the
//! client deal with encryption, cookies and transport.
//!
//! Endpoints that treat a non-200 envelope `code` as failure should call
//! [`crate::client::ensure_ok`] themselves; the client does not, because some
//! endpoints (QR-login polling) use those codes as ordinary states.

pub mod login;
pub mod search;
pub mod url;
