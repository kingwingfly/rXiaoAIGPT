//! The logged-in session: the two cookies that *are* the login, plus the
//! plumbing to move them between disk, a [`Client`]'s cookie jar, and a request.
//!
//! NetEase has no token endpoint and no refresh flow. Authentication is a pair
//! of cookies handed out once, at the end of a QR login:
//!
//! - `MUSIC_U` — the actual credential. Long-lived (months), and the only thing
//!   the server checks.
//! - `__csrf` — echoed back on write requests. weapi wants it in *two* places:
//!   as a `csrf_token` field inside the JSON payload **and** as a `csrf_token`
//!   query parameter on the URL. Endpoints that need it should read it from
//!   [`Session::csrf`] and inject it themselves; this module only stores it.
//!
//! Persisting a session is therefore just persisting those two strings, and
//! resuming one is putting them back into a jar — see [`Session::attach`] and
//! [`Session::client`].

use crate::client::BASE_URL;
use crate::error::{NeteaseErr, Result};
use crate::{Client, api::login};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Where the session is cached when config says nothing else. The `NETEASE_SESSION`
/// environment variable overrides it; reading that variable is the binary's job,
/// this crate only ever takes a path.
pub const DEFAULT_SESSION_PATH: &str = "netease_session.json";

/// Cookie name of the credential proper.
pub const MUSIC_U: &str = "MUSIC_U";
/// Cookie name of the CSRF token.
pub const CSRF: &str = "__csrf";

/// A NetEase login, reduced to what actually has to survive a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// The credential cookie. Treat as a secret: it is a full account bearer.
    pub music_u: String,
    /// The CSRF token accompanying it. Empty is tolerated — reads work without
    /// one, only writes insist.
    #[serde(default)]
    pub csrf: String,
}

impl Session {
    /// Build a session from cookie values already in hand.
    pub fn new(music_u: impl Into<String>, csrf: impl Into<String>) -> Self {
        Self {
            music_u: music_u.into(),
            csrf: csrf.into(),
        }
    }

    /// Pick the session out of a response's `Set-Cookie` headers.
    ///
    /// This exists because the cookies are only ever *seen* once: the QR poll
    /// returns code 803 exactly one time, and that response is the sole carrier
    /// of `MUSIC_U`. A jar stores them, but [`reqwest::cookie::Jar`] cannot be
    /// enumerated, so the headers are parsed as they go past.
    ///
    /// Returns `None` when the response carries no `MUSIC_U` — i.e. it was not
    /// the authorising one.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Option<Self> {
        let mut music_u = None;
        let mut csrf = None;
        for value in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(raw) = value.to_str() else { continue };
            // `NAME=VALUE; Path=/; Max-Age=...` — only the first pair matters.
            let Some((name, val)) = raw
                .split(';')
                .next()
                .and_then(|pair| pair.split_once('='))
                .map(|(n, v)| (n.trim(), v.trim()))
            else {
                continue;
            };
            match name {
                MUSIC_U => music_u = Some(val.to_string()),
                CSRF => csrf = Some(val.to_string()),
                _ => {}
            }
        }
        music_u.map(|music_u| Self::new(music_u, csrf.unwrap_or_default()))
    }

    /// The value for a `Cookie` request header, for callers driving
    /// [`Client::http`] by hand.
    pub fn cookie_header(&self) -> String {
        format!("{MUSIC_U}={}; {CSRF}={}", self.music_u, self.csrf)
    }

    /// Put these cookies into `jar` for `base_url`, making every subsequent
    /// request through a [`Client`] sharing that jar an authenticated one.
    ///
    /// `os=pc` rides along because NetEase gates a few responses (notably song
    /// URLs) on it, and it costs nothing to always send.
    pub fn attach(&self, jar: &reqwest::cookie::Jar, base_url: &str) -> Result<()> {
        let url = base_url
            .parse::<reqwest::Url>()
            .map_err(|e| NeteaseErr::BadRequest(format!("invalid base url {base_url:?}: {e}")))?;
        for cookie in [
            format!("{MUSIC_U}={}; Path=/", self.music_u),
            format!("{CSRF}={}; Path=/", self.csrf),
            "os=pc; Path=/".to_string(),
        ] {
            jar.add_cookie_str(&cookie, &url);
        }
        Ok(())
    }

    /// A ready-to-use client against the real origin, already logged in.
    pub fn client(&self) -> Result<Client> {
        self.client_with_base_url(BASE_URL)
    }

    /// As [`Session::client`], but against an arbitrary origin (tests, mocks).
    pub fn client_with_base_url(&self, base_url: &str) -> Result<Client> {
        let jar = Arc::new(reqwest::cookie::Jar::default());
        self.attach(&jar, base_url)?;
        Client::with_jar(base_url, jar)
    }

    /// Read a session cached by [`Session::save`].
    ///
    /// A missing file is a plain [`NeteaseErr::Io`]; callers wanting
    /// "load or log in" should match [`std::io::ErrorKind::NotFound`] — see
    /// [`Session::load_opt`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// [`Session::load`], but a missing file is `Ok(None)` rather than an error
    /// — the shape wanted by "resume the session, else run a QR login".
    ///
    /// A file that exists but is corrupt still errors: silently discarding it
    /// would turn a typo into a mysterious re-login.
    pub fn load_opt(path: impl AsRef<Path>) -> Result<Option<Self>> {
        match Self::load(path) {
            Ok(session) => Ok(Some(session)),
            Err(NeteaseErr::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Cache the session at `path`, creating parent directories as needed.
    ///
    /// The file holds an account bearer token, so on Unix the mode is set as
    /// the file is created rather than afterwards — a chmod after the write
    /// leaves a window in which the token is world-readable.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        use std::io::Write as _;

        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut file = opts.open(path)?;
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }
}

/// Resume a cached session, or run a QR login and cache the result.
///
/// Rendering the QR code is the caller's problem — hence `on_qr`, which is
/// handed the URL to encode (see [`login::qr_url`]) as soon as it is known.
/// Nothing here prints, so a TUI, a web page and a terminal can all use it.
pub async fn load_or_qr_login<F>(path: impl AsRef<Path>, on_qr: F) -> Result<Session>
where
    F: FnOnce(&str),
{
    let path = path.as_ref();
    if let Some(session) = Session::load_opt(path)? {
        return Ok(session);
    }
    // One client for the whole flow: unikey and every poll must share a jar.
    let client = Client::new()?;
    let pending = login::create(&client).await?;
    on_qr(pending.qr_url());
    let session = login::wait(&client, &pending, login::POLL_INTERVAL, login::POLL_TIMEOUT).await?;
    session.save(path)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};

    #[test]
    fn parses_both_cookies_out_of_set_cookie_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("MUSIC_U=abc123; Path=/; Max-Age=1296000; HTTPOnly"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("__csrf=deadbeef; Path=/"),
        );
        headers.append(SET_COOKIE, HeaderValue::from_static("NMTID=irrelevant"));

        let session = Session::from_headers(&headers).unwrap();
        assert_eq!(session.music_u, "abc123");
        assert_eq!(session.csrf, "deadbeef");
    }

    /// A poll that is merely "waiting" sets no `MUSIC_U`; that must not be
    /// mistaken for a session.
    #[test]
    fn no_music_u_means_no_session() {
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, HeaderValue::from_static("__csrf=x; Path=/"));
        assert!(Session::from_headers(&headers).is_none());
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        // A not-yet-existing nested subdir: `save` must create the parents.
        let path = dir.path().join("nested/session.json");
        let session = Session::new("token", "csrf");
        session.save(&path).unwrap();

        assert_eq!(Session::load(&path).unwrap(), session);
        assert_eq!(Session::load_opt(&path).unwrap(), Some(session));
        // A missing file: absence is not an error for `load_opt`.
        let missing = dir.path().join("does-not-exist.json");
        assert_eq!(Session::load_opt(&missing).unwrap(), None);
    }

    #[test]
    fn attaches_cookies_to_a_client_jar() {
        let client = Session::new("tok", "cs")
            .client_with_base_url("http://127.0.0.1:1")
            .unwrap();
        let url = "http://127.0.0.1:1".parse().unwrap();
        let cookies = reqwest::cookie::CookieStore::cookies(client.jar().as_ref(), &url).unwrap();
        let cookies = cookies.to_str().unwrap();
        assert!(cookies.contains("MUSIC_U=tok"), "{cookies}");
        assert!(cookies.contains("__csrf=cs"), "{cookies}");
    }
}
