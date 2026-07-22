//! The binary: assemble a [`brain`] agent out of a XiaoAi speaker, a local music
//! directory and NetEase, and run it until stopped. Everything interesting is in
//! the modules; this file is wiring plus two deployment concerns — the audio
//! server's [stream token](check_token) and making a missing API key fatal here.

mod config;
mod gate;
mod music;
mod source;
mod speaker;
mod tools;

use anyhow::{Context as _, Result, bail};
use axum::Router;
use brain::{Agent, ClientConfig, DynMusicSource, DynSpeaker, LlmClient};
use config::Config;
use music::MusicIndex;
use rmcp::ServiceExt as _;
use source::{LocalSource, NeteaseSource};
use speaker::{XiaoaiSource, XiaoaiSpeaker};
use std::sync::Arc;
use tokio::net::TcpListener;
use tools::Assistant;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use xiaoai::{account::load_or_login_and_save_with_env, device_by_alias};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    run(Config::from_env()?).await
}

async fn run(config: Config) -> Result<()> {
    // Fatal at startup, by name, rather than a 401 from the first request: there
    // is no offline fallback for the model.
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

    // The URL the speaker fetches audio from, built from the same `token` the
    // router is mounted under so the two cannot drift apart.
    let base_url = match token {
        Some(token) => format!("{}/{token}", config.base_url()),
        None => config.base_url().to_string(),
    };
    // One index, shared: a second index could disagree about what exists with the
    // URLs `LocalSource` hands out.
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

    // Erase the concrete devices into the `dyn`-style wrappers the tools hold; the
    // `Arc`s still share one instance apiece with the loop and the source.
    let assistant = Assistant::new(
        DynSpeaker::from_arc(speaker.clone()),
        DynMusicSource::from_arc(local),
    )
    .with_netease(DynMusicSource::from_arc(netease));
    info!(
        tools = ?Assistant::tool_names(),
        model = %config.deepseek_model,
        "agent ready"
    );

    // The tools run as an in-memory MCP server; `brain` connects to it as an MCP
    // client over a duplex pipe — no socket, no second process. The server's
    // `serve` awaits the client's `initialize`, so it runs concurrently with the
    // connect below rather than being awaited first.
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        match assistant.serve(server_transport).await {
            Ok(server) => {
                let _ = server.waiting().await;
            }
            Err(e) => error!(error = %e, "MCP tool server failed to start"),
        }
    });

    let client = LlmClient::with_config(
        ClientConfig::builder()
            .api_key(api_key)
            .model(&config.deepseek_model)
            .build(),
    );
    let mut source = XiaoaiSource::new(speaker.clone(), auth_data, device);
    // `Arc` is not itself a `Speaker`, so the agent takes a concrete clone rather
    // than the shared handle — `XiaoaiSpeaker`'s clones share the same playback
    // flags and credentials, so this drives the very same device the tools do.
    let mut agent = Agent::connect(client, client_transport, speaker.as_ref().clone())
        .await
        .context("cannot connect to the MCP tool server")?;

    // `Agent::run` returns only when the source is exhausted, and a live speaker
    // never is — Ctrl-C is the way out.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        res = agent.run(&mut source) => res.context("agent stopped"),
    }
}

/// Start the HTTP server the speaker fetches audio from. Binds `0.0.0.0`: the
/// speaker's route to us says nothing about which local interface to listen on.
async fn serve(config: &Config, app: Router, base_url: &str) -> Result<()> {
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    // Keep the token out of the logs — a log file is a likely place to leak it.
    let logged_url = match config.stream_token.as_deref() {
        Some(token) => base_url.replace(token, "<token>"),
        None => base_url.to_string(),
    };
    info!(
        music_dir = %config.music_dir.display(),
        bind = %addr,
        speaker_url = %logged_url,
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
/// With a stream token every route moves under `/{token}/…` and nothing is
/// served without it. This is a path prefix (not a header) because the speaker
/// fetches one handed-to-it URL and cannot add headers; the intended deployment
/// publishes this path through a Cloudflare Access *Bypass* policy, leaving the
/// origin's prefix check as the only guard. No token means routes stay at the
/// root, so LAN use is unaffected.
fn audio_app(index: Arc<MusicIndex>, netease: Router, token: Option<&str>) -> Router {
    // `NeteaseSource::router` adds only `GET /netease/{id}` and no fallback, so
    // it merges into the music router's pattern fallback without conflict.
    let audio = music::router_with(index).merge(netease);
    match token {
        Some(token) => Router::new().nest(&format!("/{token}"), audio),
        None => audio,
    }
}

/// Reject a token that would not survive being put in a URL path: `/` or `?`
/// would change the routing, and percent-encoding would be written one way in
/// config and another in the URL. Both are startup-worthy config mistakes.
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

/// `info` for our own crates, `warn` for the rest (reqwest/hyper are chatty).
/// `RUST_LOG` overrides, e.g. `RUST_LOG=xiaoai=debug` for the login exchanges.
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
    }

    /// Set means required, since the audio path is otherwise published on a
    /// Cloudflare Access *Bypass* policy.
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

    #[test]
    fn an_unusable_token_is_a_startup_error() {
        for bad in ["", "has/slash", "has space", "有中文", "a?b"] {
            assert!(check_token(bad).is_err(), "{bad:?}");
        }
        assert!(check_token("A-Za-z_0.9~").is_ok());
    }
}
