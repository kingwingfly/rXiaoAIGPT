//! The binary: assemble a [`brain`] agent out of a XiaoAi speaker, a local
//! music directory and NetEase, and run it until the process is stopped.
//!
//! Everything interesting is in the modules; this file is wiring, plus the two
//! deployment concerns that only exist once the pieces are put together — the
//! audio server's [stream token](check_token) and the fact that a missing API
//! key must be fatal *here*, not three seconds later inside a request.

mod config;
mod gate;
mod music;
mod source;
mod speaker;
mod tools;

use anyhow::{Context as _, Result, bail};
use axum::Router;
use brain::{Agent, ClientConfig, LlmClient, ToolRegistry};
use config::Config;
use music::MusicIndex;
use source::{LocalSource, NeteaseSource};
use speaker::{XiaoaiSource, XiaoaiSpeaker};
use std::sync::Arc;
use tokio::net::TcpListener;
use tools::{PlayMusic, SetVolume, Stop, TellStory};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use xiaoai::{account::load_or_login_and_save_with_env, device_by_alias};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    run(Config::from_env()?).await
}

async fn run(config: Config) -> Result<()> {
    // Checked before anything slow happens. Without a key there is no assistant
    // at all — the regex parser that used to stand in for one is gone — so
    // failing at startup, naming the variable, is far kinder than a 401 from
    // the first thing anyone says.
    let api_key = config.deepseek_api_key.clone().context(
        "DEEPSEEK_API_KEY must be set: the assistant's brain is the DeepSeek API \
         and there is no offline fallback (see .env.example)",
    )?;
    let token = config
        .stream_token
        .as_deref()
        .map(check_token)
        .transpose()?;

    let auth_data = load_or_login_and_save_with_env(&config.auth_cache).await?;
    let device = device_by_alias(&auth_data, &config.device_alias).await?;
    let speaker = Arc::new(XiaoaiSpeaker::new(auth_data.clone(), &device));

    // The URL the *speaker* fetches audio from, token prefix included: it is
    // built from the same `token` the router is mounted under, so the two
    // cannot drift apart and leave the device staring at a 404.
    let base_url = public_base_url(config.base_url(), token);
    // One index, shared: `LocalSource` hands out URLs whose paths the router
    // looks up again, so a second index could disagree about what exists.
    let index = Arc::new(MusicIndex::new(config.music_dir.clone()));
    let local = Arc::new(LocalSource::new(index.clone(), &base_url));
    let netease = Arc::new(
        NeteaseSource::from_session_file(&config.netease_session, &base_url)
            .context("cannot build the NetEase client")?,
    );

    serve(
        &config,
        audio_app(index, netease.router(), token),
        &base_url,
    )
    .await?;

    let registry = ToolRegistry::new()
        .with(PlayMusic::new(speaker.clone(), local).with_netease(netease))
        .with(Stop::new(speaker.clone()))
        .with(SetVolume::new(speaker.clone()))
        .with(TellStory::new());
    info!(
        tools = ?registry.names().collect::<Vec<_>>(),
        model = %config.deepseek_model,
        "agent ready"
    );

    let client =
        LlmClient::with_config(ClientConfig::new(api_key).with_model(&config.deepseek_model));
    let mut source = XiaoaiSource::new(speaker.clone(), auth_data, device);
    let mut agent = Agent::new(client, registry, speaker);

    // `Agent::run` only returns when the source is exhausted, and a live
    // speaker never is — Ctrl-C is the intended way out.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        res = agent.run(&mut source) => res.context("agent stopped"),
    }
}

/// Start the HTTP server the speaker fetches audio from.
///
/// Binds `0.0.0.0` rather than [`Config::host_ip`]: that address is what the
/// *speaker* must use to reach us, which says nothing about which local
/// interface to listen on.
async fn serve(config: &Config, app: Router, base_url: &str) -> Result<()> {
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    info!(
        music_dir = %config.music_dir.display(),
        bind = %addr,
        // The token is kept out of the URL that gets logged: a log file is the
        // likeliest place for a secret to leak to.
        speaker_url = %redact(base_url, config.stream_token.as_deref()),
        stream_token = config.stream_token.is_some(),
        "serving audio"
    );
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "http server stopped");
        }
    });
    Ok(())
}

/// The audio routes: the music directory, plus NetEase's streaming proxy.
///
/// # The stream token
///
/// When one is configured every audio route moves under `/{token}/…`, and
/// nothing is served without it. This is the enforcement half of
/// `XIAOAI_STREAM_TOKEN`, which was previously read and never checked.
///
/// A path prefix rather than a header or a query parameter, because of who the
/// client is: the speaker is handed one URL and fetches it itself, with no way
/// to be told to add a header. The deployment this protects publishes the audio
/// path through Cloudflare Access on a **Bypass** policy — the speaker cannot
/// authenticate to Access either — which leaves the origin as the only thing
/// between the internet and the music. An unguessable prefix is the check that
/// restores.
///
/// With no token configured the routes stay where they were, so a LAN-only
/// deployment is unaffected.
fn audio_app(index: Arc<MusicIndex>, netease: Router, token: Option<&str>) -> Router {
    // `NeteaseSource::router` adds only `GET /netease/{id}` and sets no
    // fallback, so it merges into the music router's pattern fallback without
    // conflict.
    let audio = music::router_with(index).merge(netease);
    match token {
        Some(token) => Router::new().nest(&format!("/{token}"), audio),
        None => audio,
    }
}

/// Reject a token that would not survive being put in a URL path.
///
/// A token containing `/` or `?` would silently change the routing rather than
/// protect it, and one needing percent-encoding is a trap: it would be written
/// one way in the config and another in the URL. Both are configuration
/// mistakes worth failing on at startup, while there is still someone watching.
fn check_token(token: &str) -> Result<&str> {
    if token.is_empty()
        || !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
    {
        bail!(
            "XIAOAI_STREAM_TOKEN must be a non-empty URL-safe string \
             (letters, digits, and any of -_.~): it becomes a path prefix"
        );
    }
    Ok(token)
}

/// The speaker-facing base URL, with the token prefix if there is one. Handed
/// to both music sources, since both mint URLs the speaker fetches.
fn public_base_url(base: &str, token: Option<&str>) -> String {
    match token {
        Some(token) => format!("{base}/{token}"),
        None => base.to_string(),
    }
}

/// Keep the token out of the logs while still showing the shape of the URL.
fn redact(url: &str, token: Option<&str>) -> String {
    match token {
        Some(token) => url.replace(token, "<token>"),
        None => url.to_string(),
    }
}

/// Log at `info` for our own crates and `warn` for everything else (reqwest and
/// hyper are extremely chatty at `info`). Override wholesale with `RUST_LOG`,
/// e.g. `RUST_LOG=xiaoai=debug` to see the raw Xiaomi login exchanges.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,xiaoai_llm=info,xiaoai=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt as _;

    /// The audio router over a music directory holding one track.
    fn app(dir: &tempfile::TempDir, token: Option<&str>) -> Router {
        std::fs::write(dir.path().join("晴天.mp3"), b"audio").unwrap();
        let index = Arc::new(MusicIndex::new(dir.path().to_path_buf()));
        let netease = NeteaseSource::from_session_file(dir.path().join("absent.json"), "http://x")
            .expect("a missing session must not be an error");
        audio_app(index, netease.router(), token)
    }

    async fn status(app: Router, uri: &str) -> StatusCode {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// Unset means "as before": a plain LAN deployment must keep working.
    #[tokio::test]
    async fn without_a_token_the_audio_paths_are_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            status(app(&dir, None), "/random").await,
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            status(
                app(&dir, None),
                &format!("/{}", urlencoding::encode(".*晴天.*"))
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            public_base_url("http://host:3000", None),
            "http://host:3000"
        );
    }

    /// Set means required — the whole point, since the audio path is otherwise
    /// published to the internet on a Cloudflare Access *Bypass* policy.
    #[tokio::test]
    async fn with_a_token_every_audio_path_needs_it() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "s3cret-token";

        for unauthorised in ["/random", "/netease/186016", "/.*", "/wrong-token/random"] {
            assert_eq!(
                status(app(&dir, Some(secret)), unauthorised).await,
                StatusCode::NOT_FOUND,
                "{unauthorised} must not be served without the token"
            );
        }

        assert_eq!(
            status(app(&dir, Some(secret)), &format!("/{secret}/random")).await,
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            status(
                app(&dir, Some(secret)),
                &format!("/{secret}/{}", urlencoding::encode(".*晴天.*"))
            )
            .await,
            StatusCode::OK
        );
    }

    /// The URL handed to the speaker must carry the prefix the router moved to,
    /// or the device gets a 404 for every track.
    #[test]
    fn the_speaker_facing_url_carries_the_prefix() {
        assert_eq!(
            public_base_url("http://host:3000", Some("s3cret")),
            "http://host:3000/s3cret"
        );
    }

    #[test]
    fn an_unusable_token_is_a_startup_error() {
        for bad in ["", "has/slash", "has space", "有中文", "a?b"] {
            assert!(check_token(bad).is_err(), "{bad:?}");
        }
        assert!(check_token("A-Za-z_0.9~").is_ok());
    }

    #[test]
    fn the_token_is_not_logged() {
        assert_eq!(
            redact("http://host:3000/s3cret", Some("s3cret")),
            "http://host:3000/<token>"
        );
    }
}
