//! The control loop: watch what the user said to the speaker, and react.

use anyhow::{Context as _, Result};
use std::time::Duration;
use tokio::net::TcpListener;
use xiaoai::{
    ApiCaller as _, Device, LastAskPayload, LastAskResponse, OpApi, OpPayloadBuilder, OpResponse,
    RecordApi, XiaoaiStatus, account::AuthData, account::load_or_login_and_save_with_env,
    device_by_alias,
};

use crate::{command::Command, config::Config, music};

/// How often the conversation history and the playback status are polled.
/// Xiaomi has no push API, so everything here is polling.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct Agent {
    config: Config,
    auth_data: AuthData,
    device: Device,
    /// Whether playback commands are currently obeyed; toggled by 嘻嘻/不嘻嘻.
    enabled: bool,
    /// Timestamp of the newest conversation record already handled, so the same
    /// utterance is not acted on twice.
    last_seen: usize,
}

impl Agent {
    pub async fn new(config: Config) -> Result<Self> {
        let auth_data = load_or_login_and_save_with_env(&config.auth_cache).await?;
        let device = device_by_alias(&auth_data, &config.device_alias).await?;
        Ok(Self {
            config,
            auth_data,
            device,
            enabled: true,
            last_seen: 0,
        })
    }

    /// Serve the music directory and poll until Ctrl-C.
    pub async fn run(mut self) -> Result<()> {
        self.serve().await?;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => return Ok(()),
                res = self.tick() => {
                    // One failed poll or operation should not kill the agent:
                    // the speaker may simply be offline for a moment.
                    if let Err(e) = res {
                        eprintln!("error: {e:#}");
                    }
                }
            }
        }
    }

    /// Start the HTTP server the speaker fetches audio from.
    async fn serve(&self) -> Result<()> {
        let addr = format!("0.0.0.0:{}", self.config.port);
        let listener = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("cannot listen on {addr}"))?;
        let app = music::router(self.config.music_dir.clone());
        println!(
            "serving {} at {}",
            self.config.music_dir.display(),
            self.config.base_url()
        );
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("http server stopped: {e}");
            }
        });
        Ok(())
    }

    /// Handle at most one new utterance.
    async fn tick(&mut self) -> Result<()> {
        tokio::time::sleep(POLL_INTERVAL).await;
        let payload = LastAskPayload::new(&self.auth_data, &self.device, 1);
        let resp: LastAskResponse = RecordApi::request(payload).await?;
        let Some(last) = resp.first() else {
            return Ok(());
        };
        if last.time <= self.last_seen {
            return Ok(());
        }
        // Mark it handled up front: a command that fails or is ignored must not
        // be retried on the next poll.
        self.last_seen = last.time;
        let Some(command) = Command::parse(&last.query) else {
            return Ok(());
        };
        println!("{}: {command:?}", last.query);
        if !self.enabled && !command.is_always_allowed() {
            return Ok(());
        }
        self.handle(command).await
    }

    async fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Enable => {
                self.enabled = true;
                self.speak("奶龙，启动！").await
            }
            Command::Disable => {
                self.enabled = false;
                self.speak("奶龙，关闭！").await
            }
            Command::PlayRandom => self.play_and_wait("random").await,
            Command::PlayArtist { artist } => {
                self.play_and_wait(&format!("random/{}", urlencoding::encode(&artist)))
                    .await
            }
            Command::PlayTrack { artist, title } => {
                // The path is the regex the file index is searched with.
                let pattern = match artist {
                    Some(artist) => format!(".*{artist}.*{title}.*"),
                    None => format!(".*{title}.*"),
                };
                self.play_and_wait(&urlencoding::encode(&pattern)).await
            }
        }
    }

    fn op(&self) -> OpPayloadBuilder {
        OpPayloadBuilder::new(&self.auth_data, &self.device.device_id)
    }

    async fn speak(&self, text: &str) -> Result<()> {
        let _: OpResponse = OpApi::request(self.op().speak(text)).await?;
        Ok(())
    }

    /// Point the speaker at `path` on our server, then block until it stops
    /// playing — otherwise the next poll would see the *user's* original
    /// utterance still sitting at the top of the history and replay it.
    async fn play_and_wait(&self, path: &str) -> Result<()> {
        let url = format!("{}/{path}", self.config.base_url());
        println!("playing {url}");
        // The speaker may still be playing its own answer to the utterance.
        let _: OpResponse = OpApi::request(self.op().pause()).await?;
        let _: OpResponse = OpApi::request(self.op().play_url(url)).await?;
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let resp: OpResponse = OpApi::request(self.op().status()).await?;
            if !matches!(resp.status(), XiaoaiStatus::Playing | XiaoaiStatus::Paused) {
                return Ok(());
            }
        }
    }
}
