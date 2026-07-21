//! Endpoint wrappers, one module per NetEase API — typed functions over
//! [`crate::Client::post_weapi`]. Endpoints treating a non-200 envelope `code`
//! as failure call [`crate::client::ensure_ok`] themselves.

pub mod login;
pub mod search;
pub mod url;
