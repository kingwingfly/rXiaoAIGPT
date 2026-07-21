//! The capabilities the model may invoke, exposed as one MCP server.
//!
//! Tool descriptions and argument doc-comments are written *for the model*, in
//! Chinese, because the user speaks Chinese and the arguments (song names) are
//! Chinese too. Chatting and answering questions — telling a story included — are
//! deliberately not tools: the model does those by replying in words.
//!
//! A tool returns `Ok` for ordinary outcomes ("no such song", "needs a
//! membership") and `Err` only for genuine failures (the speaker is
//! unreachable); `brain` prefixes `Err` results with `error:` so the model tells
//! the two apart.

use brain::{MusicSource, Playable, Speaker, Track};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};
use std::sync::Arc;

/// Search hits tried before giving up on a source: more than one because the
/// first hit is often a cover or VIP-gated original; few, because every attempt
/// is a round trip and the user is waiting.
const MAX_ATTEMPTS: usize = 3;

/// The assistant's tools, backed by a speaker and one or two music sources.
///
/// Music prefers the local library (the user's own collection, full quality, no
/// membership) and falls back to NetEase — unless the user names a source.
pub struct Assistant {
    speaker: Arc<dyn Speaker>,
    local: Arc<dyn MusicSource>,
    netease: Option<Arc<dyn MusicSource>>,
}

impl Assistant {
    pub fn new(speaker: Arc<dyn Speaker>, local: Arc<dyn MusicSource>) -> Self {
        Self {
            speaker,
            local,
            netease: None,
        }
    }

    #[must_use]
    pub fn with_netease(mut self, netease: Arc<dyn MusicSource>) -> Self {
        self.netease = Some(netease);
        self
    }

    /// The names of the tools this server exposes, for logging.
    pub fn tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    /// The sources to try, in order, plus a note to prefix the answer with when
    /// the request could not be honoured exactly.
    fn plan(&self, requested: Option<Source>) -> (Vec<&Arc<dyn MusicSource>>, &'static str) {
        match requested {
            Some(Source::Netease) => match &self.netease {
                Some(netease) => (vec![netease], ""),
                None => (vec![&self.local], "网易云音乐未配置，"),
            },
            Some(Source::Local) => (vec![&self.local], ""),
            None => {
                let mut order = vec![&self.local];
                order.extend(self.netease.as_ref());
                (order, "")
            }
        }
    }

    /// Candidate tracks from one source, best first.
    async fn candidates(
        source: &Arc<dyn MusicSource>,
        query: Option<&str>,
        random: bool,
    ) -> brain::Result<Vec<Track>> {
        if random {
            return Ok(source.random(query).await?.into_iter().collect());
        }
        let query = query.ok_or_else(|| {
            brain::BrainErr::InvalidArguments("`query` is required unless `random` is true".into())
        })?;
        let mut hits = source.search(query).await?;
        hits.truncate(MAX_ATTEMPTS);
        Ok(hits)
    }
}

#[tool_router]
impl Assistant {
    #[tool(
        description = "播放音乐。用户想听某首歌、某个歌手，或者想随便听点什么时调用。\
         默认先在本地曲库里找，找不到才去网易云音乐；只有用户明确说了「网易云」之类的话，\
         才把 source 设成 netease。调用成功表示音箱已经开始播放，不需要再做别的。"
    )]
    async fn play_music(
        &self,
        Parameters(args): Parameters<PlayMusicArgs>,
    ) -> Result<String, ErrorData> {
        let query = args
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty());
        if query.is_none() && !args.random {
            return Err(ErrorData::invalid_params(
                "要播放什么？请给出 query（歌名或歌手），或者把 random 设为 true",
                None,
            ));
        }
        let (sources, note) = self.plan(args.source);

        // Why each candidate was skipped, so "found it but VIP-only" does not look
        // like "no such song".
        let mut skipped: Vec<String> = Vec::new();
        for source in sources {
            let candidates = match Self::candidates(source, query, args.random).await {
                Ok(candidates) => candidates,
                // One source failing must not sink the request — NetEase being
                // down is no reason not to play a local file.
                Err(e) => {
                    tracing::warn!(source = source.name(), error = %e, "search failed");
                    skipped.push(format!("{}：{e}", source.name()));
                    continue;
                }
            };
            for track in candidates {
                // Resolved and used immediately — a NetEase URL is valid only for
                // minutes.
                let url = match source.resolve(&track).await {
                    Ok(Playable::Url(url)) => url,
                    Ok(Playable::LocalFile(path)) => {
                        tracing::warn!(path = %path.display(), "a speaker cannot play a local path");
                        skipped.push(format!("《{}》无法通过网络播放", track.title));
                        continue;
                    }
                    Err(e) => {
                        tracing::info!(track = %track.id, error = %e, "track did not resolve");
                        skipped.push(e.to_string());
                        continue;
                    }
                };
                // One call so a single-channel device speaks the announcement
                // before the track. A failure here is the device, a genuine error.
                let announcement = format!("{note}正在播放《{}》{}", track.title, artist(&track));
                self.speaker
                    .announce_then_play(&announcement, &url)
                    .await
                    .map_err(to_err)?;
                tracing::info!(track = %track.id, source = source.name(), "playing");
                return Ok(announcement);
            }
        }

        Ok(match (skipped.is_empty(), query) {
            (true, Some(query)) => format!("{note}没有找到和「{query}」有关的歌曲"),
            (true, None) => format!("{note}曲库里没有可播放的歌曲"),
            (false, _) => format!("{note}没能播放：{}", skipped.join("；")),
        })
    }

    #[tool(description = "停止播放。用户说「停」「别放了」「安静」这类话时调用。没有在播放时调用也是安全的。")]
    async fn stop(&self) -> Result<String, ErrorData> {
        self.speaker.stop().await.map_err(to_err)?;
        Ok("已停止播放".into())
    }

    #[tool(
        description = "设置音箱音量，0 到 100。用户说「大声点」「小声点」「音量调到 30」时调用。\
         「大声点」这类相对的说法，自己估一个绝对值（比如比现在高 20）填进去。"
    )]
    async fn set_volume(
        &self,
        Parameters(args): Parameters<SetVolumeArgs>,
    ) -> Result<String, ErrorData> {
        self.speaker.set_volume(args.level).await.map_err(to_err)?;
        Ok(format!("音量已调到 {}", args.level))
    }
}

#[tool_handler]
impl ServerHandler for Assistant {}

/// Which music source to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Source {
    Local,
    Netease,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PlayMusicArgs {
    /// 要找的歌名、歌手名，或者两者一起，例如「周杰伦 晴天」。\
    /// random 为 true 时，这里可以只写歌手名当作筛选条件，也可以不写。
    #[serde(default)]
    query: Option<String>,
    /// 指定曲库来源。不填就是先本地、后网易云；只有用户明确要求时才填。
    #[serde(default)]
    source: Option<Source>,
    /// 用户说「随便放一首」「来点音乐」这类没有具体目标的请求时设为 true。
    #[serde(default)]
    random: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetVolumeArgs {
    /// 目标音量，0 是静音，100 最大。
    #[serde(deserialize_with = "de_level")]
    level: u8,
}

/// Models emit `30`, `30.0` and `"30"` for the same intent; all three mean 30.
/// Out-of-range means "as loud/quiet as it goes", so clamp rather than reject.
fn de_level<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let level = value
        .as_i64()
        .or_else(|| value.as_f64().map(|v| v.round() as i64))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .ok_or_else(|| {
            serde::de::Error::custom(format!("`{value}` 不是一个音量数字，请给 0 到 100"))
        })?;
    Ok(level.clamp(0, 100) as u8)
}

/// `，周杰伦`, or nothing when the source does not know the artist.
fn artist(track: &Track) -> String {
    if track.artist.is_empty() {
        String::new()
    } else {
        format!("，{}", track.artist)
    }
}

fn to_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain::{BrainErr, Result};
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeSpeaker {
        said: Mutex<Vec<String>>,
        played: Mutex<Vec<String>>,
        volumes: Mutex<Vec<u8>>,
        stops: Mutex<usize>,
        broken: bool,
    }

    #[brain::async_trait]
    impl Speaker for FakeSpeaker {
        async fn say(&self, text: &str) -> Result<()> {
            self.said.lock().unwrap().push(text.to_string());
            Ok(())
        }
        async fn play(&self, url: &str) -> Result<()> {
            if self.broken {
                return Err(BrainErr::Backend("speaker offline".into()));
            }
            self.played.lock().unwrap().push(url.to_string());
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            *self.stops.lock().unwrap() += 1;
            Ok(())
        }
        async fn set_volume(&self, level: u8) -> Result<()> {
            self.volumes.lock().unwrap().push(level);
            Ok(())
        }
        async fn is_playing(&self) -> Result<bool> {
            Ok(false)
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Resolves {
        Fine,
        Vip,
    }

    struct FakeSource {
        name: &'static str,
        titles: Vec<&'static str>,
        resolves: Resolves,
        searched: Mutex<Vec<String>>,
        randomed: Mutex<usize>,
    }

    impl FakeSource {
        fn new(name: &'static str, titles: &[&'static str]) -> Arc<Self> {
            Arc::new(Self {
                name,
                titles: titles.to_vec(),
                resolves: Resolves::Fine,
                searched: Mutex::new(Vec::new()),
                randomed: Mutex::new(0),
            })
        }

        fn vip(name: &'static str, titles: &[&'static str]) -> Arc<Self> {
            Arc::new(Self {
                name,
                titles: titles.to_vec(),
                resolves: Resolves::Vip,
                searched: Mutex::new(Vec::new()),
                randomed: Mutex::new(0),
            })
        }

        fn searches(&self) -> Vec<String> {
            self.searched.lock().unwrap().clone()
        }

        fn track(&self, title: &str) -> Track {
            Track {
                id: title.to_string(),
                title: title.to_string(),
                artist: "某歌手".into(),
                source: self.name.to_string(),
                duration_ms: None,
            }
        }
    }

    #[brain::async_trait]
    impl MusicSource for FakeSource {
        fn name(&self) -> &str {
            self.name
        }

        async fn search(&self, query: &str) -> Result<Vec<Track>> {
            self.searched.lock().unwrap().push(query.to_string());
            Ok(self
                .titles
                .iter()
                .filter(|title| title.contains(query))
                .map(|title| self.track(title))
                .collect())
        }

        async fn resolve(&self, track: &Track) -> Result<Playable> {
            match self.resolves {
                Resolves::Fine => Ok(Playable::Url(format!("http://host/{}/{}", self.name, track.id))),
                Resolves::Vip => Err(BrainErr::Backend(format!("《{}》需要会员", track.title))),
            }
        }

        async fn random(&self, filter: Option<&str>) -> Result<Option<Track>> {
            *self.randomed.lock().unwrap() += 1;
            Ok(self
                .titles
                .iter()
                .find(|title| filter.is_none_or(|f| title.contains(f)))
                .map(|title| self.track(title)))
        }
    }

    fn speaker() -> Arc<FakeSpeaker> {
        Arc::new(FakeSpeaker::default())
    }

    /// `play_music` with the given source/random, sharing `speaker`.
    fn assistant(speaker: Arc<FakeSpeaker>, local: Arc<FakeSource>) -> Assistant {
        Assistant::new(speaker, local)
    }

    fn play_args(query: Option<&str>, source: Option<Source>, random: bool) -> PlayMusicArgs {
        PlayMusicArgs {
            query: query.map(str::to_string),
            source,
            random,
        }
    }

    #[tokio::test]
    async fn the_local_library_is_tried_first() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = assistant(speaker.clone(), local).with_netease(netease.clone());

        let out = tool
            .play_music(Parameters(play_args(Some("晴天"), None, false)))
            .await
            .unwrap();
        assert!(out.contains("晴天"), "{out}");
        assert_eq!(*speaker.played.lock().unwrap(), ["http://host/local/晴天"]);
        assert!(
            netease.searches().is_empty(),
            "netease must not be consulted when local has the song"
        );
    }

    #[tokio::test]
    async fn the_track_is_announced_before_it_plays() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let tool = assistant(speaker.clone(), local);

        tool.play_music(Parameters(play_args(Some("晴天"), None, false)))
            .await
            .unwrap();
        let said = speaker.said.lock().unwrap();
        assert_eq!(said.len(), 1);
        assert!(said[0].contains("晴天"), "{said:?}");
        assert_eq!(*speaker.played.lock().unwrap(), ["http://host/local/晴天"]);
    }

    #[tokio::test]
    async fn netease_takes_over_when_the_local_library_misses() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["别的歌"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = assistant(speaker.clone(), local.clone()).with_netease(netease);

        tool.play_music(Parameters(play_args(Some("晴天"), None, false)))
            .await
            .unwrap();
        assert_eq!(local.searches(), ["晴天"]);
        assert_eq!(*speaker.played.lock().unwrap(), ["http://host/netease/晴天"]);
    }

    #[tokio::test]
    async fn an_explicit_netease_request_skips_the_local_library() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = assistant(speaker.clone(), local.clone()).with_netease(netease);

        tool.play_music(Parameters(play_args(Some("晴天"), Some(Source::Netease), false)))
            .await
            .unwrap();
        assert!(local.searches().is_empty());
        assert_eq!(*speaker.played.lock().unwrap(), ["http://host/netease/晴天"]);
    }

    #[tokio::test]
    async fn an_explicit_local_request_never_reaches_netease() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = assistant(speaker.clone(), local).with_netease(netease.clone());

        tool.play_music(Parameters(play_args(Some("晴天"), Some(Source::Local), false)))
            .await
            .unwrap();
        assert!(netease.searches().is_empty());
    }

    #[tokio::test]
    async fn asking_for_an_unconfigured_netease_falls_back_and_says_so() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let tool = assistant(speaker.clone(), local);

        let out = tool
            .play_music(Parameters(play_args(Some("晴天"), Some(Source::Netease), false)))
            .await
            .unwrap();
        assert!(out.contains("网易云音乐未配置"), "{out}");
        assert_eq!(speaker.played.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_search_miss_is_an_ordinary_answer() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let tool = assistant(speaker.clone(), local);

        let out = tool
            .play_music(Parameters(play_args(Some("不存在的歌"), None, false)))
            .await
            .unwrap();
        assert!(out.contains("没有找到"), "{out}");
        assert!(out.contains("不存在的歌"), "{out}");
        assert!(speaker.played.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_vip_gated_track_explains_itself_instead_of_pretending_to_play() {
        let speaker = speaker();
        let local = FakeSource::new("local", &[]);
        let netease = FakeSource::vip("netease", &["晴天"]);
        let tool = assistant(speaker.clone(), local).with_netease(netease);

        let out = tool
            .play_music(Parameters(play_args(Some("晴天"), None, false)))
            .await
            .unwrap();
        assert!(out.contains("需要会员"), "{out}");
        assert!(out.contains("晴天"), "{out}");
        assert!(speaker.played.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_gated_hit_does_not_stop_the_local_library_from_answering() {
        let speaker = speaker();
        let local = FakeSource::vip("local", &["晴天"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = assistant(speaker.clone(), local).with_netease(netease);

        tool.play_music(Parameters(play_args(Some("晴天"), None, false)))
            .await
            .unwrap();
        assert_eq!(*speaker.played.lock().unwrap(), ["http://host/netease/晴天"]);
    }

    #[tokio::test]
    async fn random_uses_the_random_endpoint_not_a_search() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天", "稻香"]);
        let tool = assistant(speaker.clone(), local.clone());

        let out = tool
            .play_music(Parameters(play_args(None, None, true)))
            .await
            .unwrap();
        assert!(out.contains("正在播放"), "{out}");
        assert_eq!(*local.randomed.lock().unwrap(), 1);
        assert!(local.searches().is_empty());

        tool.play_music(Parameters(play_args(Some("稻香"), None, true)))
            .await
            .unwrap();
        assert!(local.searches().is_empty());
        assert!(
            speaker.played.lock().unwrap()[1].contains("稻香"),
            "the filter must restrict the pick"
        );
    }

    #[tokio::test]
    async fn playing_nothing_in_particular_needs_a_query_or_random() {
        let tool = assistant(speaker(), FakeSource::new("local", &["晴天"]));
        assert!(
            tool.play_music(Parameters(play_args(None, None, false)))
                .await
                .is_err()
        );
        assert!(
            tool.play_music(Parameters(play_args(Some("   "), None, false)))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_broken_speaker_is_an_error() {
        let speaker = Arc::new(FakeSpeaker {
            broken: true,
            ..Default::default()
        });
        let tool = assistant(speaker, FakeSource::new("local", &["晴天"]));
        assert!(
            tool.play_music(Parameters(play_args(Some("晴天"), None, false)))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stop_stops() {
        let speaker = speaker();
        let out = Assistant::new(speaker.clone(), FakeSource::new("local", &[]))
            .stop()
            .await
            .unwrap();
        assert!(out.contains("停止"), "{out}");
        assert_eq!(*speaker.stops.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn the_volume_is_clamped_at_both_ends_rather_than_rejected() {
        let speaker = speaker();
        let tool = assistant(speaker.clone(), FakeSource::new("local", &[]));
        for level in [json!(200), json!(-5), json!(30)] {
            let args: SetVolumeArgs = serde_json::from_value(json!({ "level": level })).unwrap();
            tool.set_volume(Parameters(args)).await.unwrap();
        }
        assert_eq!(*speaker.volumes.lock().unwrap(), [100, 0, 30]);
    }

    #[test]
    fn a_volume_may_be_a_float_or_a_string() {
        let float: SetVolumeArgs = serde_json::from_value(json!({ "level": 30.4 })).unwrap();
        assert_eq!(float.level, 30);
        let string: SetVolumeArgs = serde_json::from_value(json!({ "level": "45" })).unwrap();
        assert_eq!(string.level, 45);

        assert!(serde_json::from_value::<SetVolumeArgs>(json!({})).is_err());
        assert!(serde_json::from_value::<SetVolumeArgs>(json!({ "level": "响一点" })).is_err());
    }

    /// The three tools are advertised over MCP with object schemas.
    #[tokio::test]
    async fn the_tools_are_listed_over_mcp() {
        let router = Assistant::tool_router();
        let mut names: Vec<_> = router.list_all().iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(names, ["play_music", "set_volume", "stop"]);
        for tool in router.list_all() {
            assert_eq!(tool.input_schema.get("type").unwrap(), "object");
            assert!(tool.description.as_ref().is_some_and(|d| !d.is_empty()));
        }
    }
}
