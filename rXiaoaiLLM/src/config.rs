//! Runtime configuration, read from the environment (or a `.env` file).

use anyhow::{Context as _, Result};
use std::path::PathBuf;

/// Everything the agent needs to know about *this* deployment: which speaker to
/// drive, and where that speaker can reach us.
///
/// Note the split between the two "address" notions:
///
/// - [`Config::host_ip`] + [`Config::port`] are the **bind address** — where the
///   HTTP server listens (it actually binds `0.0.0.0:port`; `host_ip` only ever
///   feeds the default speaker-facing URL).
/// - [`Config::public_base_url`] is the **speaker-facing URL** — what we hand to
///   the speaker so it can fetch audio. On a plain LAN deployment the two
///   coincide, but behind a tunnel (Cloudflare Access and friends) the speaker
///   talks to a public hostname that has nothing to do with the bind address,
///   so it must be overridable on its own.
#[derive(Debug, Clone)]
// Several fields are read by nothing yet: they configure the LLM intent layer
// and the NetEase source, which land in later changes. They are here now so that
// deployments can be configured once rather than twice.
#[allow(dead_code)]
pub struct Config {
    /// Alias of the speaker, as shown in the Mi Home app. The account must own
    /// the device — being an administrator of it is not enough.
    pub device_alias: String,
    /// Address the *speaker* uses to reach this host on the LAN: it fetches the
    /// audio itself, so a loopback address will not work. Only used to build the
    /// default [`Config::public_base_url`].
    pub host_ip: String,
    /// Port the HTTP server binds.
    pub port: u16,
    /// Base URL handed to the speaker, without a trailing slash. Defaults to
    /// `http://{host_ip}:{port}`; set `XIAOAI_PUBLIC_BASE_URL` when the speaker
    /// reaches us through a proxy or tunnel instead of directly.
    pub public_base_url: String,
    /// Directory served over HTTP and scanned for audio files.
    pub music_dir: PathBuf,
    /// Where the login result is cached; delete it to force a re-login.
    pub auth_cache: PathBuf,
    /// DeepSeek API key for the (not yet wired up) LLM intent layer. Absent is
    /// not an error: the regex command parser still works without it.
    pub deepseek_api_key: Option<String>,
    /// DeepSeek model name used by the intent layer.
    pub deepseek_model: String,
    /// Where the NetEase Cloud Music login session (cookies) is cached.
    pub netease_session: PathBuf,
    /// Shared secret prefixing the audio paths, so the URLs we hand the speaker
    /// are unguessable.
    ///
    /// The speaker cannot authenticate, so an audio endpoint published through
    /// Cloudflare Access has to sit on a *Bypass* policy — leaving the origin as
    /// the only thing between the internet and the music. Checking this token at
    /// the origin restores that check. Unset means no token check (fine on a
    /// LAN-only deployment).
    pub stream_token: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        let host_ip = required("XIAOAI_HOST_IP")?;
        let port = match std::env::var("XIAOAI_PORT") {
            Ok(port) => port.parse().context("XIAOAI_PORT is not a port number")?,
            Err(_) => 3000,
        };
        Ok(Self {
            device_alias: required("XIAOAI_DEVICE")?,
            public_base_url: public_base_url(
                optional_str("XIAOAI_PUBLIC_BASE_URL"),
                &host_ip,
                port,
            ),
            host_ip,
            port,
            music_dir: optional("XIAOAI_MUSIC_DIR").unwrap_or_else(|| ".".into()),
            auth_cache: optional("XIAOAI_AUTH_CACHE").unwrap_or_else(|| "auth_data.json".into()),
            deepseek_api_key: optional_str("DEEPSEEK_API_KEY"),
            deepseek_model: optional_str("DEEPSEEK_MODEL")
                .unwrap_or_else(|| "deepseek-v4-flash".to_string()),
            netease_session: optional("NETEASE_SESSION")
                .unwrap_or_else(|| "netease_session.json".into()),
            stream_token: optional_str("XIAOAI_STREAM_TOKEN"),
        })
    }

    /// Base URL the speaker will be pointed at. This is *not* where we listen —
    /// see the [`Config`] docs.
    pub fn base_url(&self) -> &str {
        &self.public_base_url
    }
}

fn required(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("{key} must be set (see .env.example)"))
}

fn optional(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(Into::into)
}

fn optional_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The speaker-facing URL: an explicit override wins, otherwise it is derived
/// from the LAN address we assume the speaker can reach us at.
fn public_base_url(explicit: Option<String>, host_ip: &str, port: u16) -> String {
    match explicit {
        // A trailing slash would produce `//path` once a path is appended.
        Some(url) => url.trim_end_matches('/').to_string(),
        None => format!("http://{host_ip}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_the_lan_address() {
        assert_eq!(
            public_base_url(None, "192.168.1.20", 3000),
            "http://192.168.1.20:3000"
        );
    }

    /// Behind a tunnel the speaker talks to a public hostname on port 443, which
    /// the bind address says nothing about.
    #[test]
    fn override_wins_and_loses_its_trailing_slash() {
        assert_eq!(
            public_base_url(Some("https://music.example.com/".into()), "10.0.0.1", 3000),
            "https://music.example.com"
        );
    }
}
