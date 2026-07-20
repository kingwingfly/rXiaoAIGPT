//! Xiaomi's APIs nest JSON *inside JSON strings* — a request's `message` field
//! and a response's `info`/`data` fields are all documents encoded as strings.
//! These helpers bridge that for `serde`.

use serde::{Deserialize, Serialize, Serializer};

/// Serialize `value` to JSON, then emit that JSON as a string.
pub(crate) fn to_string<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    serde_json::to_string(value)
        .map_err(|e| serde::ser::Error::custom(format!("Failed to serialize message {e}")))?
        .serialize(serializer)
}

/// Read a string, then parse it as JSON.
pub(crate) fn from_string<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let s = String::deserialize(deserializer)?;
    serde_json::from_str(&s).map_err(serde::de::Error::custom)
}

/// Strip the `&&&START&&&` sentinel Xiaomi prefixes its JSON responses with.
pub(crate) fn strip_start(text: &str) -> &str {
    text.strip_prefix("&&&START&&&").unwrap_or(text)
}

/// `before_deserialize` hook for the `api_req` payload macros: `Ok` with the
/// stripped body, or `Err` with the original when the sentinel is absent.
pub(crate) fn strip_start_hook(text: String) -> Result<String, String> {
    match text.strip_prefix("&&&START&&&") {
        Some(rest) => Ok(rest.to_owned()),
        None => Err(text),
    }
}
