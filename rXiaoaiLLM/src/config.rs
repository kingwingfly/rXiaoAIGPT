//! Runtime configuration, read from the environment (or a `.env` file).

use anyhow::{Context as _, Result};
use std::path::PathBuf;

/// Everything the agent needs to know about *this* deployment: which speaker to
/// drive, and where that speaker can reach us.
#[derive(Debug, Clone)]
pub struct Config {
    /// Alias of the speaker, as shown in the Mi Home app. The account must own
    /// the device — being an administrator of it is not enough.
    pub device_alias: String,
    /// Address the *speaker* uses to reach this host: it fetches the audio over
    /// the LAN, so a loopback address will not work.
    pub host_ip: String,
    pub port: u16,
    /// Directory served over HTTP and scanned for audio files.
    pub music_dir: PathBuf,
    /// Where the login result is cached; delete it to force a re-login.
    pub auth_cache: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        Ok(Self {
            device_alias: required("XIAOAI_DEVICE")?,
            host_ip: required("XIAOAI_HOST_IP")?,
            port: match std::env::var("XIAOAI_PORT") {
                Ok(port) => port.parse().context("XIAOAI_PORT is not a port number")?,
                Err(_) => 3000,
            },
            music_dir: optional("XIAOAI_MUSIC_DIR").unwrap_or_else(|| ".".into()),
            auth_cache: optional("XIAOAI_AUTH_CACHE").unwrap_or_else(|| "auth_data.json".into()),
        })
    }

    /// Base URL the speaker will be pointed at.
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host_ip, self.port)
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
