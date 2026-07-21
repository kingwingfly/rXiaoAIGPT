//! Runtime configuration, read from the environment (or a `.env` file).

use anyhow::{Context as _, Result};
use std::path::PathBuf;

/// This deployment: which speaker to drive and where it can reach us.
///
/// Two distinct "address" notions: the **bind address** ([`Config::port`], since
/// the server binds `0.0.0.0:port`) and [`Config::public_base_url`], the URL
/// handed to the speaker. They coincide on a LAN but diverge behind a tunnel.
/// `XIAOAI_HOST_IP` is consumed only to seed the default `public_base_url`, so it
/// is not kept as a field.
#[derive(Debug, Clone)]
pub struct Config {
    /// Speaker alias as shown in Mi Home. The account must *own* the device;
    /// administrator access is not enough.
    pub device_alias: String,
    /// Port the HTTP server binds.
    pub port: u16,
    /// Base URL handed to the speaker, no trailing slash. Defaults to
    /// `http://{host_ip}:{port}`; set `XIAOAI_PUBLIC_BASE_URL` behind a tunnel.
    pub public_base_url: String,
    /// Directory served over HTTP and scanned for audio.
    pub music_dir: PathBuf,
    /// Where the login result is cached; delete it to force a re-login.
    pub auth_cache: PathBuf,
    /// DeepSeek API key. Optional here, but the binary refuses to start without
    /// one — there is no offline fallback.
    pub deepseek_api_key: Option<String>,
    /// DeepSeek model name.
    pub deepseek_model: String,
    /// Where the NetEase login session (cookies) is cached.
    pub netease_session: PathBuf,
    /// Unguessable prefix on the audio paths. The speaker cannot authenticate, so
    /// an audio endpoint published through Cloudflare Access sits on a *Bypass*
    /// policy and the origin checks this instead. Unset means no check (LAN use).
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

    /// Base URL the speaker is pointed at — *not* where we listen.
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

/// The speaker-facing URL: an explicit override (minus any trailing slash) wins,
/// else it is derived from the LAN address.
fn public_base_url(explicit: Option<String>, host_ip: &str, port: u16) -> String {
    match explicit {
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

    #[test]
    fn override_wins_and_loses_its_trailing_slash() {
        assert_eq!(
            public_base_url(Some("https://music.example.com/".into()), "10.0.0.1", 3000),
            "https://music.example.com"
        );
    }
}
