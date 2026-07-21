//! The HTTP client every endpoint module goes through: a [`reqwest::Client`]
//! plus a shared cookie jar (the session) and [`Client::post_weapi`], which hides
//! the weapi encryption so endpoints deal only in plain JSON.

use crate::{
    crypto,
    error::{NeteaseErr, Result},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

/// Default origin. Every weapi path is resolved against `{base}/weapi/{path}`.
pub const BASE_URL: &str = "https://music.163.com";

/// NetEase degrades requests that do not look like a browser, so send a desktop
/// Chrome UA rather than reqwest's default.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

/// A NetEase Cloud Music API client. Cheap to clone — clones share one
/// connection pool and one session.
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

    /// A client against an arbitrary origin (tests point it at a mock). A
    /// trailing slash on `base_url` is ignored.
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        Self::with_jar(base_url, Arc::new(reqwest::cookie::Jar::default()))
    }

    /// A client resuming a saved session — a jar holding `MUSIC_U` from an
    /// earlier run needs no login.
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

    /// The shared cookie jar — the session.
    pub fn jar(&self) -> &Arc<reqwest::cookie::Jar> {
        &self.jar
    }

    /// The underlying HTTP client, for the few endpoints that are not weapi (a
    /// QR image, say).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The origin requests are resolved against, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// POST an encrypted weapi request to `path` (without the `/weapi/` prefix)
    /// and deserialize the response into `T`. The response `code` is **not**
    /// checked — some endpoints (QR-login polling) use non-200 codes as ordinary
    /// states; call [`ensure_ok`] where a non-200 really is a failure.
    pub async fn post_weapi<T: DeserializeOwned>(&self, path: &str, payload: &Value) -> Result<T> {
        let text = self.post_weapi_text(path, payload).await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// As [`Client::post_weapi`], but leaving the response an untyped [`Value`].
    pub async fn post_weapi_value(&self, path: &str, payload: &Value) -> Result<Value> {
        self.post_weapi(path, payload).await
    }

    /// The raw response body. NetEase answers a malformed request with an HTML
    /// error page rather than JSON, so the text is what makes a failure legible.
    pub async fn post_weapi_text(&self, path: &str, payload: &Value) -> Result<String> {
        let url = format!("{}/weapi/{}", self.base_url, path.trim_start_matches('/'));
        let body = serde_json::to_string(payload)?;
        let encrypted = crypto::encrypt(&body);
        tracing::debug!(%url, payload = %body, "weapi request");
        let resp = self
            .http
            .post(&url)
            // Requests without a music.163.com referer are refused.
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

/// Fail unless the response carries `"code": 200`. Opt-in (see
/// [`Client::post_weapi`]); a response with no `code` at all is accepted.
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

    /// Echoes the two weapi form fields back as JSON.
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

        // `encSecKey` must be the full-width hex the real server insists on.
        assert!(!resp["params"].as_str().unwrap().is_empty());
        assert_eq!(resp["encSecKey"].as_str().unwrap().len(), 256);
        ensure_ok(&resp).unwrap();
        server.abort();
    }

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
