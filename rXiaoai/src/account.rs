use api_req::{ApiCaller, Method, Payload, RedirectPolicy, header};
use base64::{Engine, prelude::BASE64_STANDARD};
use rand::distr::{Alphanumeric, SampleString as _};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{path::Path, sync::LazyLock};

use crate::error::{Result, XiaoaiErr};
use crate::serde_util::{strip_start, strip_start_hook};
use crate::sid::Sid;

const ACCOUNT_URL: &str = "https://account.xiaomi.com";
const USER_AGENT: &str = "APP/com.xiaomi.mihome APPV/6.0.103 iosPassportSDK/3.9.0 iOS/14.4 miHSTS";

pub static DEVICE_ID: LazyLock<String> = LazyLock::new(|| {
    let id = std::env::var("DEVICE_ID")
        .unwrap_or(Alphanumeric.sample_string(&mut rand::rng(), 16))
        .to_uppercase();
    assert_eq!(id.len(), 16, "DEVICE_ID's length must be 16");
    id
});

/// Load auth data from `path`, or log in (via `ACCOUNT_ID`/`ACCOUNT_PASSWORD`)
/// and save it there.
pub async fn load_or_login_and_save_with_env(path: impl AsRef<Path>) -> Result<AuthData> {
    match load(path.as_ref()) {
        Ok(data) => Ok(data),
        Err(e) => {
            debug(
                "auth-cache",
                &format!("{}: {e}; logging in", path.as_ref().display()),
            );
            let data = login_with_env().await?;
            save(path.as_ref(), &data)?;
            Ok(data)
        }
    }
}

fn load(path: &Path) -> std::io::Result<AuthData> {
    let file = std::fs::File::open(path)?;
    serde_json::from_reader(file).map_err(std::io::Error::other)
}

fn save(path: &Path, data: &AuthData) -> Result<()> {
    let file = std::fs::File::create(path)
        .map_err(|e| XiaoaiErr::Auth(format!("cannot write {}: {e}", path.display())))?;
    serde_json::to_writer(file, data).map_err(|e| XiaoaiErr::Auth(e.to_string()))
}

/// Load auth data from `path`, or log in with `user`/`password` and save it there.
pub async fn load_or_login_and_save(
    user: String,
    password: String,
    path: impl AsRef<Path>,
) -> Result<AuthData> {
    match load(path.as_ref()) {
        Ok(data) => Ok(data),
        Err(e) => {
            debug(
                "auth-cache",
                &format!("{}: {e}; logging in", path.as_ref().display()),
            );
            let data = login(user, password).await?;
            save(path.as_ref(), &data)?;
            Ok(data)
        }
    }
}

/// Log in from `ACCOUNT_ID`/`ACCOUNT_PASSWORD`, without saving.
pub async fn login_with_env() -> Result<AuthData> {
    dotenvy::dotenv().ok();
    login(
        std::env::var("ACCOUNT_ID")
            .map_err(|_| XiaoaiErr::Auth("ACCOUNT_ID env var not found".to_string()))?,
        std::env::var("ACCOUNT_PASSWORD")
            .map_err(|_| XiaoaiErr::Auth("ACCOUNT_PASSWORD env var not found".to_string()))?,
    )
    .await
}

/// Log in, without saving. When Xiaomi demands identity verification (typical for
/// a new device/IP) this reads the code from stdin; for a non-interactive flow
/// use [`try_login`] and [`Verification`] directly.
pub async fn login(user: String, password: String) -> Result<AuthData> {
    match try_login(user, password).await? {
        LoginFlow::Done(data) => Ok(data),
        LoginFlow::NeedVerification(verification) => {
            // These `eprintln!`s are deliberately not `tracing`: they are one half
            // of an interactive stdin dialogue, and `RUST_LOG` must not silence
            // the prompt and hang the login.
            eprintln!("Xiaomi requires identity verification.");
            // Sending the code ourselves binds it to our session, not a browser's.
            match verification.send_ticket().await {
                Ok(sent) => {
                    let how = match sent.contains(&8) {
                        true => "email",
                        false => "SMS",
                    };
                    eprintln!("A verification code has been sent to you by {how}.");
                }
                Err(e) => {
                    eprintln!("Could not send the code automatically ({e}).");
                    eprintln!("Open this URL in a browser and request a code instead:");
                    eprintln!("   {}", verification.url());
                    eprintln!("Do NOT enter the code on Xiaomi's website; enter it below.");
                }
            }
            loop {
                eprintln!("Enter the verification code (empty to abort): ");
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .map_err(|e| XiaoaiErr::Auth(e.to_string()))?;
                let ticket = line.trim();
                if ticket.is_empty() {
                    return Err(XiaoaiErr::Auth("verification aborted".to_string()));
                }
                match verification.submit_ticket(ticket).await {
                    Ok(data) => return Ok(data),
                    Err(e) => eprintln!("{e}; try again"),
                }
            }
        }
    }
}

/// Result of [`try_login`]: either finished auth data, or a pending
/// interactive identity verification.
#[derive(Debug)]
pub enum LoginFlow {
    Done(AuthData),
    NeedVerification(Verification),
}

/// Start a login without any interactivity. Returns
/// [`LoginFlow::NeedVerification`] when Xiaomi demands identity verification;
/// complete it with [`Verification::submit_ticket`].
pub async fn try_login(user: String, password: String) -> Result<LoginFlow> {
    let payload = LoginPayload {
        device_id: DEVICE_ID.clone(),
        ..Default::default()
    };
    let resp: LoginResponse = AccountApi::request(payload)
        .await
        .map_err(|e| XiaoaiErr::Auth(e.to_string()))?;
    if let Some(user_id) = resp.user_id {
        return Ok(LoginFlow::Done(AuthData {
            service_token: resp.service_token().await?,
            user_id,
            device_id: DEVICE_ID.clone(),
            ssecurity: resp
                .ssecurity
                .ok_or(XiaoaiErr::Auth("ssecurity not found in resp".to_string()))?,
            pass_token: resp.pass_token.unwrap_or_default(),
        }));
    }
    let payload2 = LoginPayload2 {
        _json: true,
        user,
        hash: { hex::encode(md5::compute(password).iter()).to_uppercase() },
        ..resp
            .payload2
            .ok_or(XiaoaiErr::Auth("payload2 not found in resp".to_string()))?
    };
    let resp: LoginResponse2 = AccountApi::request(payload2.clone())
        .await
        .map_err(|e| XiaoaiErr::Auth(e.to_string()))?;
    match resp.notification_url {
        Some(url) => Ok(LoginFlow::NeedVerification(
            Verification::start(absolute_url(url), &payload2).await?,
        )),
        None => Ok(LoginFlow::Done(resp.into_auth_data().await?)),
    }
}

/// A pending identity verification (Xiaomi's `identity/authStart` flow). Call
/// [`Verification::send_ticket`] then [`Verification::submit_ticket`]. If sending
/// fails (Xiaomi may demand a captcha), request a code at [`Verification::url`]
/// in a browser — but do not enter it there, or it binds to the browser's session.
#[derive(Debug)]
pub struct Verification {
    /// Cookie session shared across identity/list, verify, and the login resume.
    client: reqwest::Client,
    /// The same jar `client` uses: redirect hops set `serviceToken`/`passToken`
    /// cookies only visible here.
    jar: std::sync::Arc<reqwest::cookie::Jar>,
    verify_url: String,
    /// `identity/list` API URL, derived from `verify_url`.
    list_url: String,
    /// Methods offered: 4 = phone/SMS, 8 = email.
    options: Vec<i64>,
    user: String,
    hash: String,
    sid: String,
}

/// Every `reqwest` failure in the login flow is an auth failure.
fn auth_err(e: reqwest::Error) -> XiaoaiErr {
    XiaoaiErr::Auth(e.to_string())
}

/// Credentials Xiaomi hands back in the clear. `passToken` is the dangerous one:
/// [`refresh`] turns it into a fresh `serviceToken` with no password and no
/// verification code, so it is password-equivalent and long-lived.
const SECRET_KEYS: [&str; 4] = ["passToken", "ssecurity", "serviceToken", "cUserId"];

/// Replace every credential value in `text` with `<redacted>`. A blunt textual
/// pass, not a parse: the bodies that reach an error message are the ones that
/// failed to parse. Handles both JSON (`"passToken":"…"`) and cookie (`=…;`) forms.
fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for key in SECRET_KEYS {
        let mut from = 0;
        while let Some(at) = out.get(from..).and_then(|rest| rest.find(key)) {
            let after_key = from + at + key.len();
            let Some(rest) = out.get(after_key..) else {
                break;
            };
            // Step over whatever separates the name from its value.
            let sep = rest.len() - rest.trim_start_matches(['"', ':', '=', ' ']).len();
            let value = &rest[sep..];
            let end = value
                .find(['"', ';', ',', '}', '\n'])
                .unwrap_or(value.len());
            if end == 0 {
                // A bare mention with no value — skip past it and keep looking.
                from = after_key;
                continue;
            }
            let start = after_key + sep;
            out.replace_range(start..start + end, "<redacted>");
            from = start + "<redacted>".len();
        }
    }
    out
}

/// Bound a body for an error message, credentials removed first.
fn snippet(text: &str) -> String {
    redact(text).chars().take(300).collect()
}

/// Trace the raw Xiaomi exchanges (`RUST_LOG=xiaoai=debug`) — the login APIs are
/// undocumented, so the bodies are the only way to diagnose a broken flow.
/// Redacted because that is exactly when a body carries a fresh `passToken`.
fn debug(step: &str, body: &str) {
    tracing::debug!(step, body = %redact(body), "xiaomi exchange");
}

impl Verification {
    async fn start(verify_url: String, payload2: &LoginPayload2) -> Result<Self> {
        let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
        for domain in [".xiaomi.com", ".mi.com"] {
            for origin in [ACCOUNT_URL, "https://mi.com"] {
                let origin = origin.parse::<reqwest::Url>().unwrap();
                jar.add_cookie_str(
                    &format!("deviceId={}; Domain={domain}; Path=/", &*DEVICE_ID),
                    &origin,
                );
                jar.add_cookie_str(&format!("sdkVersion=3.9; Domain={domain}; Path=/"), &origin);
            }
        }
        let client = reqwest::Client::builder()
            .cookie_provider(jar.clone())
            .user_agent(USER_AGENT)
            .build()
            .map_err(auth_err)?;
        // `verify_url` is the SPA; the JSON API is `/identity/list`, same query.
        let list_url = match verify_url.contains("fe/service/identity/authStart") {
            true => verify_url.replace("fe/service/identity/authStart", "identity/list"),
            false => verify_url.replace("identity/authStart", "identity/list"),
        };
        let mut this = Self {
            client,
            jar,
            verify_url,
            list_url,
            options: vec![],
            user: payload2.user.clone(),
            hash: payload2.hash.clone(),
            sid: payload2.sid.clone(),
        };
        this.options = this.fetch_options().await?;
        Ok(this)
    }

    /// Read a cookie the server set on any hop of a redirect chain.
    fn cookie(&self, url: &reqwest::Url, name: &str) -> Option<String> {
        use reqwest::cookie::CookieStore as _;
        let header = self.jar.cookies(url)?;
        header
            .to_str()
            .ok()?
            .split("; ")
            .find_map(|c| c.strip_prefix(&format!("{name}=")))
            .map(ToOwned::to_owned)
    }

    /// GET `identity/list`: sets the `identity_session` cookie the verify
    /// endpoints need, and returns the offered methods (4 = SMS, 8 = email).
    async fn fetch_options(&self) -> Result<Vec<i64>> {
        let resp = self
            .client
            .get(&self.list_url)
            .send()
            .await
            .map_err(auth_err)?;
        let got_session = resp.cookies().any(|c| c.name() == "identity_session");
        let text = resp.text().await.map_err(auth_err)?;
        debug("identity/list", &text);
        if !got_session {
            return Err(XiaoaiErr::Auth(format!(
                "identity/list did not set identity_session cookie; body: {}",
                snippet(&text)
            )));
        }
        let resp: IdentityListResponse =
            serde_json::from_str(strip_start(&text)).unwrap_or(IdentityListResponse {
                flag: None,
                options: None,
            });
        let mut options = resp.options.unwrap_or_default();
        if options.is_empty() {
            options.push(resp.flag.unwrap_or(4));
        }
        Ok(options)
    }

    /// The URL to open in a browser to request a verification code.
    pub fn url(&self) -> &str {
        &self.verify_url
    }

    async fn post_identity(&self, api: &str, form: &[(&str, &str)]) -> Result<String> {
        let dc = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string();
        let text = self
            .client
            .post(format!("{ACCOUNT_URL}/identity/auth/{api}"))
            .query(&[("_dc", dc.as_str())])
            .form(form)
            .send()
            .await
            .map_err(auth_err)?
            .text()
            .await
            .map_err(auth_err)?;
        debug(api, &text);
        Ok(text)
    }

    /// Ask Xiaomi to send a code **to this session**, so it is bound to the
    /// `identity_session` that will later verify it. Returns the methods it was
    /// sent by (4 = SMS, 8 = email); an error means requesting one in a browser.
    pub async fn send_ticket(&self) -> Result<Vec<i64>> {
        let mut sent = vec![];
        let mut last = "no supported verification method".to_string();
        for flag in &*self.options {
            let api = match flag {
                4 => "sendPhoneTicket",
                8 => "sendEmailTicket",
                _ => continue,
            };
            let text = self
                .post_identity(api, &[("retry", "false"), ("_json", "true")])
                .await?;
            let resp: VerifyTicketResponse = serde_json::from_str(strip_start(&text))
                .map_err(|e| XiaoaiErr::Auth(format!("{api}: {e}; body: {}", snippet(&text))))?;
            match resp.code {
                Some(0) => sent.push(*flag),
                _ => {
                    last = format!(
                        "{api}: code={:?}, desc={:?}",
                        resp.code,
                        resp.desc.clone().or(resp.description.clone())
                    )
                }
            }
        }
        match sent.is_empty() {
            true => Err(XiaoaiErr::Auth(last)),
            false => Ok(sent),
        }
    }

    /// Submit the received code, then resume the login in the same session.
    pub async fn submit_ticket(&self, ticket: impl AsRef<str>) -> Result<AuthData> {
        let ticket = ticket.as_ref().trim();
        let mut last = "no supported verification method".to_string();
        for flag in &*self.options {
            let api = match flag {
                4 => "verifyPhone",
                8 => "verifyEmail",
                _ => continue,
            };
            let text = self
                .post_identity(
                    api,
                    &[
                        ("_flag", &flag.to_string()),
                        ("ticket", ticket),
                        ("trust", "true"),
                        ("_json", "true"),
                    ],
                )
                .await?;
            let resp: VerifyTicketResponse = serde_json::from_str(strip_start(&text))
                .map_err(|e| XiaoaiErr::Auth(format!("{api}: {e}; body: {}", snippet(&text))))?;
            match resp.code {
                Some(0) => {
                    // This redirect chain is what issues passToken.
                    match resp.location.as_deref().filter(|l| !l.is_empty()) {
                        Some(location) => self.follow_verified_location(location).await?,
                        None => debug("verify-location", "none returned"),
                    }
                    return self.resume_login(&text).await;
                }
                _ => {
                    last = format!(
                        "{api} rejected: code={:?}, desc={:?}",
                        resp.code,
                        resp.desc.clone().or(resp.description.clone())
                    )
                }
            }
        }
        Err(XiaoaiErr::Auth(format!("verification failed: {last}")))
    }

    /// Follow the post-verification redirect chain to the callback that issues
    /// `passToken`. Xiaomi interposes a `/fe/service/` interstitial whose `skipUrl`
    /// query parameter is the real continuation, so follow that.
    async fn follow_verified_location(&self, location: &str) -> Result<()> {
        let mut resp = self
            .client
            .get(absolute_url(location.to_owned()))
            .send()
            .await
            .map_err(auth_err)?;
        for _ in 0..5 {
            let url = resp.url().clone();
            debug("verify-location", &format!("{} {url}", resp.status()));
            if !url.path().starts_with("/fe/service/") {
                break;
            }
            let Some(skip) = url
                .query_pairs()
                .find_map(|(k, v)| (k == "skipUrl").then(|| v.into_owned()))
            else {
                break;
            };
            resp = self
                .client
                .get(absolute_url(skip))
                .send()
                .await
                .map_err(auth_err)?;
        }
        let status = resp.status();
        let url = resp.url().clone();
        let body = resp.text().await.map_err(auth_err)?;
        debug(
            "verify-location-final",
            &format!("{status} {url}\n{}", snippet(&body)),
        );
        Ok(())
    }

    /// Resume the login in the verified session. The `qs`/`_sign`/`callback`
    /// captured before verification are bound to the unverified attempt, so
    /// `serviceLogin` is re-run for fresh ones (skipping the password step if it
    /// already returns a `location`).
    async fn resume_login(&self, verify_body: &str) -> Result<AuthData> {
        let text = self
            .client
            .get(format!("{ACCOUNT_URL}/pass/serviceLogin"))
            .query(&[("sid", self.sid.as_str()), ("_json", "true")])
            .send()
            .await
            .map_err(auth_err)?
            .text()
            .await
            .map_err(auth_err)?;
        debug("serviceLogin-resume", &text);
        let step1: LoginResponse = serde_json::from_str(strip_start(&text)).map_err(|e| {
            XiaoaiErr::Auth(format!(
                "serviceLogin resume: {e}; body: {}",
                snippet(&text)
            ))
        })?;
        if let (Some(user_id), Some(location), Some(ssecurity), Some(nonce)) = (
            step1.user_id,
            step1.location.as_deref(),
            step1.ssecurity.as_deref(),
            step1.nonce,
        ) {
            return self
                .finish(
                    location,
                    nonce,
                    ssecurity,
                    user_id,
                    step1.pass_token.clone(),
                )
                .await;
        }
        let payload2 = LoginPayload2 {
            _json: true,
            user: self.user.clone(),
            hash: self.hash.clone(),
            ..step1.payload2.clone().ok_or_else(|| {
                XiaoaiErr::Auth(format!(
                    "serviceLogin after verification returned neither a location nor sign \
                     parameters; body: {}",
                    snippet(&text)
                ))
            })?
        };
        let text2 = self
            .client
            .post(format!("{ACCOUNT_URL}/pass/serviceLoginAuth2"))
            .query(&[("_json", "true")])
            .form(&payload2)
            .send()
            .await
            .map_err(auth_err)?
            .text()
            .await
            .map_err(auth_err)?;
        let step2: LoginResponse2 = serde_json::from_str(strip_start(&text2)).map_err(|e| {
            XiaoaiErr::Auth(format!(
                "serviceLoginAuth2 retry: {e}; body: {}",
                snippet(&text2)
            ))
        })?;
        debug("serviceLoginAuth2-retry", &text2);
        if step2.notification_url.is_some() {
            return Err(XiaoaiErr::Auth(format!(
                "Xiaomi still demands verification after the code was accepted.\n\
                 verify said: {}\nserviceLogin said: {}\n\
                 serviceLoginAuth2 said: {}",
                snippet(verify_body),
                snippet(&text),
                snippet(&text2)
            )));
        }
        match (
            step2.user_id,
            step2.location.as_deref(),
            step2.ssecurity.as_deref(),
            step2.nonce,
        ) {
            (Some(user_id), Some(location), Some(ssecurity), Some(nonce)) => {
                self.finish(
                    location,
                    nonce,
                    ssecurity,
                    user_id,
                    step2.pass_token.clone(),
                )
                .await
            }
            _ => step2.into_auth_data().await,
        }
    }

    /// Exchange `location` for the `serviceToken` cookie, in-session.
    async fn finish(
        &self,
        location: &str,
        nonce: i64,
        ssecurity: &str,
        user_id: i64,
        pass_token: Option<String>,
    ) -> Result<AuthData> {
        let url = reqwest::Url::parse_with_params(
            location,
            &[("clientSign", client_sign(nonce, ssecurity))],
        )
        .map_err(|e| XiaoaiErr::Auth(e.to_string()))?;
        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(auth_err)?;
        let service_token = resp
            .cookies()
            .find(|c| c.name() == "serviceToken")
            .map(|c| c.value().to_owned())
            .or_else(|| self.cookie(&url, "serviceToken"))
            .ok_or_else(|| {
                XiaoaiErr::Auth("serviceToken not found in cookies after verification".to_string())
            })?;
        Ok(AuthData {
            service_token,
            user_id,
            device_id: DEVICE_ID.clone(),
            ssecurity: ssecurity.to_owned(),
            pass_token: pass_token
                .or_else(|| self.cookie(&url, "passToken"))
                .unwrap_or_default(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct IdentityListResponse {
    flag: Option<i64>,
    options: Option<Vec<i64>>,
}

#[derive(Debug, Deserialize)]
struct VerifyTicketResponse {
    code: Option<i64>,
    location: Option<String>,
    desc: Option<String>,
    description: Option<String>,
}

/// Refresh auth data using the cached `passToken` (no password/verification). If
/// Xiaomi rejects the token, do a full [`login`] again.
pub async fn refresh(auth_data: &AuthData) -> Result<AuthData> {
    if auth_data.pass_token.is_empty() {
        return Err(XiaoaiErr::Auth(
            "no cached passToken; do a full login".to_string(),
        ));
    }
    let payload = LoginPayload {
        sid: Sid::default(),
        device_id: auth_data.device_id.clone(),
        user_id: auth_data.user_id,
        pass_token: auth_data.pass_token.clone(),
    };
    let resp: LoginResponse = AccountApi::request(payload)
        .await
        .map_err(|e| XiaoaiErr::Auth(e.to_string()))?;
    match resp.user_id {
        Some(user_id) => Ok(AuthData {
            service_token: resp.service_token().await?,
            user_id,
            device_id: auth_data.device_id.clone(),
            ssecurity: resp
                .ssecurity
                .ok_or(XiaoaiErr::Auth("ssecurity not found in resp".to_string()))?,
            pass_token: resp
                .pass_token
                .unwrap_or_else(|| auth_data.pass_token.clone()),
        }),
        None => Err(XiaoaiErr::Auth(
            "passToken expired or rejected; do a full login".to_string(),
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthData {
    pub user_id: i64,
    /// The `deviceId` this session used; see [`DEVICE_ID`]. `divice_id` was the
    /// (misspelled) 0.1.x name, aliased so older caches still load.
    #[serde(alias = "divice_id")]
    pub device_id: String,
    pub ssecurity: String,
    pub service_token: String,
    /// Long-lived, password-equivalent token [`refresh`] trades for a serviceToken.
    #[serde(default)]
    pub pass_token: String,
}

#[derive(Debug, ApiCaller)]
#[api_req(
    base_url = ACCOUNT_URL,
    default_headers = [(header::USER_AGENT, USER_AGENT)],
    redirect = RedirectPolicy::none(),
)]
pub struct AccountApi {}

#[derive(Debug, Default, Serialize, Payload)]
#[api_req(
    path = "/pass/serviceLogin?sid={sid}&_json=true",
    method = Method::GET,
    headers = [(header::COOKIE, "sdkVersion=3.9; deviceId={device_id}; userId={user_id}; passToken={pass_token}")],
    req = query,
    before_deserialize = |text: String| strip_start_hook(text),
)]
pub struct LoginPayload {
    #[serde(skip_serializing)]
    sid: Sid,
    #[serde(skip_serializing)]
    device_id: String,
    #[serde(skip_serializing)]
    user_id: i64,
    #[serde(skip_serializing)]
    pass_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Payload)]
#[api_req(
    path = "/pass/serviceLoginAuth2",
    method = Method::POST,
    headers = [(header::COOKIE, format!("sdkVersion=3.9; deviceId={}", &*DEVICE_ID))],
    req = form,
    before_deserialize = |text: String| strip_start_hook(text)
)]
pub struct LoginPayload2 {
    #[serde(skip_deserializing)]
    _json: bool,
    qs: String,
    sid: String,
    _sign: String,
    callback: String,
    #[serde(skip_deserializing)]
    user: String,
    #[serde(skip_deserializing)]
    hash: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    #[serde(rename = "userId")]
    pub user_id: Option<i64>,
    #[serde(rename = "passToken")]
    pub pass_token: Option<String>,
    pub location: Option<String>,
    pub nonce: Option<i64>,
    pub ssecurity: Option<String>,
    #[serde(flatten)]
    pub payload2: Option<LoginPayload2>,
}

/// `serviceLoginAuth2` response. All fields optional: on failure Xiaomi returns
/// `code`/`desc` plus possibly `notificationUrl` (interactive verification
/// required) or `captchaUrl` instead of the auth fields.
#[derive(Debug, Deserialize)]
pub struct LoginResponse2 {
    #[serde(rename = "userId")]
    pub user_id: Option<i64>,
    #[serde(rename = "passToken")]
    pub pass_token: Option<String>,
    pub location: Option<String>,
    pub nonce: Option<i64>,
    pub ssecurity: Option<String>,
    pub code: Option<i64>,
    pub desc: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "notificationUrl")]
    pub notification_url: Option<String>,
    #[serde(rename = "captchaUrl")]
    pub captcha_url: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn absolute_url(url: String) -> String {
    match url.starts_with("http") {
        true => url,
        false => format!("{ACCOUNT_URL}{url}"),
    }
}

impl LoginResponse2 {
    async fn into_auth_data(self) -> Result<AuthData> {
        if let Some(url) = self.notification_url {
            return Err(XiaoaiErr::Auth(format!(
                "Xiaomi requires identity verification. Open this URL in a browser, \
                 complete the verification, then login again:\n{}",
                absolute_url(url)
            )));
        }
        if let Some(url) = self.captcha_url {
            return Err(XiaoaiErr::Auth(format!(
                "Xiaomi requires a captcha:\n{}",
                absolute_url(url)
            )));
        }
        match (self.user_id, self.ssecurity, self.nonce, self.location) {
            (Some(user_id), Some(ssecurity), Some(nonce), Some(location)) => Ok(AuthData {
                service_token: service_token(&location, nonce, &ssecurity).await?,
                user_id,
                device_id: DEVICE_ID.clone(),
                ssecurity,
                pass_token: self.pass_token.unwrap_or_default(),
            }),
            _ => Err(XiaoaiErr::Auth(format!(
                "login rejected: code={:?}, desc={:?}, extra={:?}",
                self.code,
                self.desc.or(self.description),
                self.extra
            ))),
        }
    }
}

fn client_sign(nonce: i64, ssecurity: impl AsRef<str>) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("nonce={nonce}&{}", ssecurity.as_ref()));
    BASE64_STANDARD.encode(hasher.finalize())
}

async fn service_token(
    location: impl AsRef<str>,
    nonce: i64,
    ssecurity: impl AsRef<str>,
) -> Result<String> {
    let sig = client_sign(nonce, ssecurity);
    Ok(reqwest::get(
        reqwest::Url::parse_with_params(location.as_ref(), &[("clientSign", sig)])
            .map_err(|e| XiaoaiErr::Auth(e.to_string()))?,
    )
    .await
    .map_err(|e| XiaoaiErr::Auth(e.to_string()))?
    .cookies()
    .find(|c| c.name() == "serviceToken")
    .ok_or(XiaoaiErr::Auth(
        "serviceToken not found in cookies".to_string(),
    ))?
    .value()
    .to_owned())
}

impl LoginResponse {
    pub async fn service_token(&self) -> Result<String> {
        service_token(
            self.location
                .as_deref()
                .ok_or(XiaoaiErr::Auth("location not found in resp".to_string()))?,
            self.nonce
                .ok_or(XiaoaiErr::Auth("nonce not found in resp".to_string()))?,
            self.ssecurity
                .as_deref()
                .ok_or(XiaoaiErr::Auth("ssecurity not found in resp".to_string()))?,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `passToken` is password-equivalent, and the error-message bodies are the
    /// ones carrying a fresh one, so redaction must never let one through.
    #[test]
    fn credentials_never_survive_redaction() {
        let body = r#"{"userId":123,"passToken":"V1:secret-pass","ssecurity":"s3cur1ty","location":"https://example.com/x"}"#;
        let out = snippet(body);
        assert!(!out.contains("secret-pass"), "{out}");
        assert!(!out.contains("s3cur1ty"), "{out}");
        // Non-secret context has to survive, or the message is useless.
        assert!(out.contains("userId"), "{out}");
        assert!(out.contains("https://example.com/x"), "{out}");

        // The same names arrive as cookies, not just JSON fields.
        let cookie = "passToken=V1:abc; serviceToken=xyz; Path=/";
        let out = redact(cookie);
        assert!(!out.contains("V1:abc") && !out.contains("xyz"), "{out}");

        // A malformed body is exactly the case that gets logged, so redaction
        // must not depend on it parsing.
        let broken = r#"{"passToken":"leaked-anyway", "#;
        assert!(!redact(broken).contains("leaked-anyway"));

        // Non-ASCII must not panic on a byte-index slice.
        let unicode = r#"{"desc":"验证码已发送","passToken":"tok"}"#;
        let out = redact(unicode);
        assert!(!out.contains("\"tok\""), "{out}");
        assert!(out.contains("验证码已发送"), "{out}");
    }

    #[tokio::test]
    async fn test_login() {
        login_with_env().await.unwrap();
    }
}
