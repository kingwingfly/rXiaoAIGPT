//! The HTTP client every endpoint module goes through.
//!
//! Two things make this more than a bare [`reqwest::Client`]:
//!
//! - **A cookie jar.** NetEase authentication is entirely cookie-based
//!   (`MUSIC_U` after a successful login, plus a `__csrf` token). The jar is
//!   shared and reachable via [`Client::jar`] so a later unit can persist it to
//!   disk and restore a session without logging in again.
//! - **The weapi envelope.** Requests are not JSON bodies but the two encrypted
//!   form fields described in [`crate::crypto`]; [`Client::post_weapi`] hides
//!   that entirely, so endpoint modules only ever deal in plain JSON.

use crate::{
    crypto,
    error::{NeteaseErr, Result},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

/// Default origin. Every weapi path is resolved against `{base}/weapi/{path}`.
pub const BASE_URL: &str = "https://music.163.com";

/// NetEase rejects, or silently degrades, requests that do not look like a
/// browser — hence a desktop Chrome UA rather than reqwest's default.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

/// A NetEase Cloud Music API client.
///
/// Cheap to clone: the underlying [`reqwest::Client`] and the cookie jar are
/// both reference-counted, so clones share one connection pool and one session.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    jar: Arc<reqwest::cookie::Jar>,
    base_url: String,
}

impl Client {
    /// A client against the real NetEase origin, with an empty session.
    pub fn new() -> Result<Self> {
        Self::with_base_url(BASE_URL)
    }

    /// A client against an arbitrary origin. Intended for tests, which point it
    /// at a local mock server; production callers want [`Client::new`].
    ///
    /// A trailing slash on `base_url` is ignored.
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        Self::with_jar(base_url, Arc::new(reqwest::cookie::Jar::default()))
    }

    /// A client resuming a previously saved session.
    ///
    /// Restoring cookies is the whole of "staying logged in" here: hand back a
    /// jar holding the `MUSIC_U` cookie from an earlier run and no login is
    /// needed.
    pub fn with_jar(base_url: impl Into<String>, jar: Arc<reqwest::cookie::Jar>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .cookie_provider(jar.clone())
            .build()?;
        Ok(Self {
            http,
            jar,
            base_url,
        })
    }

    /// The shared cookie jar — the session. Serialise what is in here to keep a
    /// login across restarts.
    pub fn jar(&self) -> &Arc<reqwest::cookie::Jar> {
        &self.jar
    }

    /// The underlying HTTP client, for the handful of NetEase endpoints that are
    /// not weapi at all (fetching a QR image, say).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The origin requests are resolved against, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// POST an encrypted weapi request to `path` (e.g. `"search/get"`, i.e.
    /// without the `/weapi/` prefix) and deserialize the JSON response into `T`.
    ///
    /// The response's own `code` field is **not** checked here: it is not always
    /// an error signal — the QR-login poll, for one, communicates its state
    /// through non-200 codes. Call [`ensure_ok`] explicitly on endpoints where a
    /// non-200 code really does mean failure.
    pub async fn post_weapi<T: DeserializeOwned>(&self, path: &str, payload: &Value) -> Result<T> {
        let text = self.post_weapi_text(path, payload).await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// As [`Client::post_weapi`], but leaving the response as an untyped
    /// [`Value`] — useful while reverse-engineering a new endpoint.
    pub async fn post_weapi_value(&self, path: &str, payload: &Value) -> Result<Value> {
        self.post_weapi(path, payload).await
    }

    /// The raw response body, before any parsing.
    ///
    /// NetEase answers a malformed request with an HTML error page rather than
    /// JSON, so keeping the text around is what makes such a failure legible.
    pub async fn post_weapi_text(&self, path: &str, payload: &Value) -> Result<String> {
        let url = format!("{}/weapi/{}", self.base_url, path.trim_start_matches('/'));
        let body = serde_json::to_string(payload)?;
        let encrypted = crypto::encrypt(&body);
        tracing::debug!(%url, payload = %body, "weapi request");
        let resp = self
            .http
            .post(&url)
            // Requests without a music.163.com referer are refused outright.
            .header(reqwest::header::REFERER, BASE_URL)
            .form(&[
                ("params", encrypted.params),
                ("encSecKey", encrypted.enc_sec_key),
            ])
            .send()
            .await?
            .error_for_status()?;
        let text = resp.text().await?;
        tracing::debug!(%url, response = %text, "weapi response");
        Ok(text)
    }
}

/// Fail unless the response envelope carries `"code": 200`.
///
/// Opt-in rather than automatic — see [`Client::post_weapi`]. A response with no
/// `code` field at all is accepted: some endpoints simply omit it.
pub fn ensure_ok(value: &Value) -> Result<()> {
    match value.get("code").and_then(Value::as_i64) {
        None | Some(200) => Ok(()),
        Some(code) => Err(NeteaseErr::Api {
            code,
            message: value
                .get("message")
                .or_else(|| value.get("msg"))
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::Form, routing::post};
    use serde_json::json;

    /// A stand-in for the weapi endpoint that echoes the two form fields it was
    /// sent back as JSON, so a test can assert the wire format without touching
    /// the network.
    async fn mock_server() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/weapi/echo",
            post(
                |Form(form): Form<std::collections::HashMap<String, String>>| async move {
                    axum::Json(json!({
                        "code": 200,
                        "params": form.get("params").cloned(),
                        "encSecKey": form.get("encSecKey").cloned(),
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn posts_the_two_weapi_form_fields() {
        let (base, server) = mock_server().await;
        let client = Client::with_base_url(&base).unwrap();

        let resp: Value = client
            .post_weapi("echo", &json!({ "s": "晴天" }))
            .await
            .unwrap();

        // Both fields must arrive, and `encSecKey` must be the full-width hex
        // the real server insists on.
        assert!(!resp["params"].as_str().unwrap().is_empty());
        assert_eq!(resp["encSecKey"].as_str().unwrap().len(), 256);
        ensure_ok(&resp).unwrap();
        server.abort();
    }

    /// A leading slash on the path is a natural thing to write and must not
    /// produce `//weapi//echo`.
    #[tokio::test]
    async fn path_and_base_url_slashes_are_normalised() {
        let (base, server) = mock_server().await;
        let client = Client::with_base_url(format!("{base}/")).unwrap();
        assert_eq!(client.base_url(), base);

        let resp: Value = client.post_weapi("/echo", &json!({})).await.unwrap();
        assert_eq!(resp["code"], 200);
        server.abort();
    }

    #[test]
    fn ensure_ok_reports_the_code_and_message() {
        ensure_ok(&json!({ "code": 200 })).unwrap();
        // Not every endpoint sends a code; absence is not failure.
        ensure_ok(&json!({ "result": {} })).unwrap();

        let err = ensure_ok(&json!({ "code": 301, "msg": "需要登录" })).unwrap_err();
        assert!(matches!(err, NeteaseErr::Api { code: 301, .. }));
        assert!(err.to_string().contains("需要登录"));
    }

    /// Clones share the session, so logging in on one clone logs in on all.
    #[test]
    fn clones_share_the_cookie_jar() {
        let client = Client::new().unwrap();
        let clone = client.clone();
        assert!(Arc::ptr_eq(client.jar(), clone.jar()));
    }
}
