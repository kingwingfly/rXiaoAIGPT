//! QR-code login.
//!
//! NetEase's web player logs in by showing a QR code that the phone app scans.
//! There are three steps, of which only two touch the network:
//!
//! 1. [`create`] asks for a `unikey` — a short-lived nonce identifying this
//!    login attempt.
//! 2. The QR code itself is *local*: it encodes `https://music.163.com/login?codekey={unikey}`
//!    and nothing more. See [`qr_url`].
//! 3. [`poll`] asks what has happened to that unikey. Its answer is a
//!    [`QrStatus`], which walks 801 (waiting) → 802 (scanned) → 803 (confirmed).
//!
//! # Two things that will silently break a login
//!
//! - **The session cookies arrive on the 803 response, and 803 is returned
//!   once.** Poll again after it and the server answers 800, with no cookies.
//!   So the poll must read `Set-Cookie` off that very response — which is why
//!   this module drives [`Client::http`] itself instead of going through
//!   [`Client::post_weapi`], which yields only a body.
//! - **Every poll must use the same [`Client`]** (hence the same cookie jar) as
//!   the [`create`] call. The unikey is bound to the anonymous session cookies
//!   NetEase set on the first request; a fresh client polls a unikey the server
//!   does not consider its own.
//!
//! ```no_run
//! # async fn example() -> netease::Result<()> {
//! let client = netease::Client::new()?;          // one client for the whole flow
//! let pending = netease::api::login::create(&client).await?;
//! println!("scan: {}", pending.qr_url());        // render this however you like
//! let session = netease::api::login::wait(
//!     &client,
//!     &pending,
//!     netease::api::login::POLL_INTERVAL,
//!     netease::api::login::POLL_TIMEOUT,
//! )
//! .await?;
//! session.save(netease::session::DEFAULT_SESSION_PATH)?;
//! # Ok(())
//! # }
//! ```

use crate::{
    Client, crypto,
    error::{NeteaseErr, Result},
    session::Session,
};
use serde_json::{Value, json};
use std::time::Duration;

/// weapi path for step 1. Upstream documentation writes these as `/api/...`;
/// the weapi transport replaces that prefix with `/weapi/`, which
/// [`Client::post_weapi`] adds, so the `/api` is dropped here.
const PATH_UNIKEY: &str = "login/qrcode/unikey";
/// weapi path for step 3.
const PATH_POLL: &str = "login/qrcode/client/login";

/// The login type NetEase expects on both calls. `3` means "QR code".
const QR_TYPE: i64 = 3;

/// How long to wait between polls. Faster buys nothing — the phone side is
/// human-paced — and NetEase rate-limits.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long the whole scan is given before giving up. The server expires a
/// unikey at about three minutes anyway, at which point it answers 800.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(180);

/// A login attempt waiting to be scanned.
///
/// Holds the `unikey` and the URL to render as a QR code. Nothing here is
/// secret for long: the unikey is useless once expired or consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLogin {
    unikey: String,
    qr_url: String,
}

impl PendingLogin {
    /// Wrap a unikey obtained elsewhere (a resumed UI session, a test).
    pub fn from_unikey(unikey: impl Into<String>) -> Self {
        let unikey = unikey.into();
        let qr_url = qr_url(&unikey);
        Self { unikey, qr_url }
    }

    /// The nonce identifying this attempt, as sent to [`poll`].
    pub fn unikey(&self) -> &str {
        &self.unikey
    }

    /// The URL to encode into a QR code. Render it as an image, a terminal
    /// block, a link — this crate deliberately does none of that.
    pub fn qr_url(&self) -> &str {
        &self.qr_url
    }
}

/// The URL a QR code for `unikey` must encode.
///
/// Purely local string building: the phone app resolves it, we never fetch it.
/// It always names the real origin, even when the client points at a mock —
/// a test origin would be meaningless to a phone.
pub fn qr_url(unikey: &str) -> String {
    format!("{}/login?codekey={unikey}", crate::client::BASE_URL)
}

/// What the server says about a pending login.
///
/// Modelled as states rather than integers because NetEase reuses the `code`
/// field for both: [`Client::post_weapi`] deliberately does not call
/// `ensure_ok`, since 800/801/802 are ordinary progress here, not failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrStatus {
    /// 800 — the QR code expired, or was already used. Unrecoverable for this
    /// unikey: start again from [`create`].
    Expired,
    /// 801 — the code is live but nobody has scanned it yet.
    WaitingForScan,
    /// 802 — scanned; the user is being asked to confirm on their phone.
    WaitingForConfirmation,
    /// 803 — authorised. Carries the session, because this response is the only
    /// one that ever will: see the module docs.
    Authorized(Box<Session>),
}

impl QrStatus {
    /// The wire code this state came from.
    pub fn code(&self) -> i64 {
        match self {
            Self::Expired => 800,
            Self::WaitingForScan => 801,
            Self::WaitingForConfirmation => 802,
            Self::Authorized(_) => 803,
        }
    }

    /// Whether polling should stop — either because it succeeded or because the
    /// code died.
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Expired | Self::Authorized(_))
    }
}

/// Step 1: ask for a unikey and build the QR URL around it.
///
/// The returned [`PendingLogin`] is only valid for polls made through the same
/// [`Client`].
pub async fn create(client: &Client) -> Result<PendingLogin> {
    let value: Value = client
        .post_weapi(PATH_UNIKEY, &json!({ "type": QR_TYPE }))
        .await?;
    // Here a non-200 code really is a failure — no state is being signalled.
    crate::client::ensure_ok(&value)?;
    let unikey = value
        .get("unikey")
        .and_then(Value::as_str)
        .ok_or_else(|| NeteaseErr::BadRequest(format!("no unikey in response: {value}")))?;
    Ok(PendingLogin::from_unikey(unikey))
}

/// Step 3: ask what has become of `pending`.
///
/// Sends the request by hand rather than via [`Client::post_weapi`] so the
/// `Set-Cookie` headers survive: on 803 they carry `MUSIC_U`, and there is no
/// second chance to read them.
pub async fn poll(client: &Client, pending: &PendingLogin) -> Result<QrStatus> {
    let url = format!("{}/weapi/{PATH_POLL}", client.base_url());
    let body = serde_json::to_string(&json!({ "key": pending.unikey(), "type": QR_TYPE }))?;
    let encrypted = crypto::encrypt(&body);
    let resp = client
        .http()
        .post(&url)
        .header(reqwest::header::REFERER, crate::client::BASE_URL)
        .form(&[
            ("params", encrypted.params),
            ("encSecKey", encrypted.enc_sec_key),
        ])
        .send()
        .await?
        .error_for_status()?;

    // Read the headers before the body: consuming the response moves it.
    let session = Session::from_headers(resp.headers());
    let text = resp.text().await?;
    let value: Value = serde_json::from_str(&text)?;
    let code = value
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| NeteaseErr::BadRequest(format!("no code in poll response: {text}")))?;
    tracing::debug!(code, "qr login poll");

    match code {
        800 => Ok(QrStatus::Expired),
        801 => Ok(QrStatus::WaitingForScan),
        802 => Ok(QrStatus::WaitingForConfirmation),
        803 => {
            let session = session.ok_or_else(|| {
                // Authorised but no cookie: nothing can be salvaged, and the
                // unikey is now spent, so say so loudly rather than loop.
                NeteaseErr::BadRequest(
                    "login authorised (803) but the response carried no MUSIC_U cookie".into(),
                )
            })?;
            Ok(QrStatus::Authorized(Box::new(session)))
        }
        // Anything else is a genuine error envelope (e.g. 400 for a malformed
        // key), not a state of the scan.
        other => Err(NeteaseErr::Api {
            code: other,
            message: value
                .get("message")
                .or_else(|| value.get("msg"))
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
    }
}

/// Convenience over [`poll`]: loop until the code is scanned, expires, or
/// `timeout` elapses.
///
/// Provided only for callers with nothing better to do than wait; a UI wanting
/// to show "scanned, confirm on your phone" should drive [`poll`] itself.
/// Expiry and timeout both surface as [`NeteaseErr::Api`] with code 800, since
/// the remedy is identical: make a new [`PendingLogin`].
pub async fn wait(
    client: &Client,
    pending: &PendingLogin,
    interval: Duration,
    timeout: Duration,
) -> Result<Session> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match poll(client, pending).await? {
            QrStatus::Authorized(session) => return Ok(*session),
            QrStatus::Expired => {
                return Err(NeteaseErr::Api {
                    code: 800,
                    message: Some("qr code expired before it was confirmed".into()),
                });
            }
            _ if std::time::Instant::now() + interval >= deadline => {
                return Err(NeteaseErr::Api {
                    code: 800,
                    message: Some(format!("gave up waiting for the qr scan after {timeout:?}")),
                });
            }
            _ => tokio::time::sleep(interval).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{HeaderMap, HeaderValue, header::SET_COOKIE},
        response::IntoResponse,
        routing::post,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// A mock NetEase that hands out a fixed unikey and walks the poll through
    /// a scripted sequence of codes, setting the session cookies on the 803 —
    /// exactly once, as the real server does.
    async fn mock(codes: Vec<i64>) -> (String, tokio::task::JoinHandle<()>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let codes = Arc::new(codes);
        let app = Router::new()
            .route(
                "/weapi/login/qrcode/unikey",
                post(|| async { axum::Json(json!({ "code": 200, "unikey": "UNIKEY-1" })) }),
            )
            .route(
                "/weapi/login/qrcode/client/login",
                post(move || {
                    let (calls, codes) = (calls.clone(), codes.clone());
                    async move {
                        let i = calls.fetch_add(1, Ordering::SeqCst);
                        let code = *codes.get(i).unwrap_or(codes.last().unwrap());
                        let mut headers = HeaderMap::new();
                        if code == 803 {
                            headers.append(
                                SET_COOKIE,
                                HeaderValue::from_static("MUSIC_U=session-token; Path=/"),
                            );
                            headers.append(
                                SET_COOKIE,
                                HeaderValue::from_static("__csrf=csrf-token; Path=/"),
                            );
                        }
                        (headers, axum::Json(json!({ "code": code }))).into_response()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn qr_url_points_at_the_real_origin() {
        assert_eq!(qr_url("abc"), "https://music.163.com/login?codekey=abc");
        // And is built without any network call.
        assert_eq!(PendingLogin::from_unikey("abc").qr_url(), qr_url("abc"));
    }

    #[tokio::test]
    async fn walks_801_802_803_and_captures_the_cookies() {
        let (base, server) = mock(vec![801, 802, 803]).await;
        let client = Client::with_base_url(&base).unwrap();

        let pending = create(&client).await.unwrap();
        assert_eq!(pending.unikey(), "UNIKEY-1");

        assert_eq!(
            poll(&client, &pending).await.unwrap(),
            QrStatus::WaitingForScan
        );
        let scanned = poll(&client, &pending).await.unwrap();
        assert_eq!(scanned, QrStatus::WaitingForConfirmation);
        assert!(!scanned.is_final());

        let status = poll(&client, &pending).await.unwrap();
        assert_eq!(status.code(), 803);
        assert!(status.is_final());
        let QrStatus::Authorized(session) = status else {
            unreachable!()
        };
        assert_eq!(session.music_u, "session-token");
        assert_eq!(session.csrf, "csrf-token");

        // The cookies also landed in the shared jar, so the very same client is
        // now authenticated.
        let url = base.parse().unwrap();
        let jar = reqwest::cookie::CookieStore::cookies(client.jar().as_ref(), &url).unwrap();
        assert!(jar.to_str().unwrap().contains("MUSIC_U=session-token"));

        server.abort();
    }

    #[tokio::test]
    async fn expired_code_is_a_state_not_an_error() {
        let (base, server) = mock(vec![800]).await;
        let client = Client::with_base_url(&base).unwrap();
        let pending = create(&client).await.unwrap();

        assert_eq!(poll(&client, &pending).await.unwrap(), QrStatus::Expired);

        // `wait` turns it into a failure, since there is nothing left to wait for.
        let err = wait(&client, &pending, Duration::ZERO, POLL_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(err, NeteaseErr::Api { code: 800, .. }));
        server.abort();
    }

    #[tokio::test]
    async fn wait_loops_until_authorised() {
        let (base, server) = mock(vec![801, 801, 802, 803]).await;
        let client = Client::with_base_url(&base).unwrap();
        let pending = create(&client).await.unwrap();

        let session = wait(&client, &pending, Duration::from_millis(1), POLL_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(session.music_u, "session-token");
        server.abort();
    }

    /// A scan that never happens must not hang forever.
    #[tokio::test]
    async fn wait_gives_up_after_the_timeout() {
        let (base, server) = mock(vec![801]).await;
        let client = Client::with_base_url(&base).unwrap();
        let pending = create(&client).await.unwrap();

        let err = wait(
            &client,
            &pending,
            Duration::from_millis(5),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, NeteaseErr::Api { code: 800, .. }));
        server.abort();
    }

    /// An unmapped code is a real error, not an unknown state.
    #[tokio::test]
    async fn unknown_code_is_an_api_error() {
        let (base, server) = mock(vec![400]).await;
        let client = Client::with_base_url(&base).unwrap();
        let pending = create(&client).await.unwrap();

        let err = poll(&client, &pending).await.unwrap_err();
        assert!(matches!(err, NeteaseErr::Api { code: 400, .. }));
        server.abort();
    }
}
