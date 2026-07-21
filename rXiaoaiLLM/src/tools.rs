//! The capabilities the model may invoke — one [`Tool`] impl per capability.
//!
//! Everything the model learns about a capability comes from
//! [`Tool::description`] and [`Tool::parameters`]; there is no place else to
//! say it. Both are therefore written *for the model*, in Chinese, because the
//! user speaks Chinese to the speaker and the arguments the model produces
//! (song names, artists, topics) are Chinese too.
//!
//! # What is deliberately *not* a tool
//!
//! Chatting, arguing about a topic, answering a question — the model already
//! does all of that by replying in words, and the loop speaks whatever it
//! replies. Wrapping that in a tool would add a round trip and a chance to fail
//! in exchange for nothing.
//!
//! [`TellStory`] is the one exception, and only because of a conflict it
//! resolves: the system prompt caps answers at about three sentences so the
//! speaker does not lecture, and a story is exactly the request where that cap
//! is wrong. The tool returns no content of its own — it returns the *brief*
//! that lifts the cap. That is a real thing a plain reply cannot do.
//!
//! # Errors versus unhappy answers
//!
//! `Err` text reaches the model as `error: ...`, so it is reserved for genuine
//! failures — the speaker is unreachable, the arguments are unusable. "No such
//! song", "that one needs a membership" and "NetEase is not configured" are
//! ordinary outcomes: they come back as `Ok` strings, which the model can relay
//! or act on without treating the turn as broken.

use brain::{BrainErr, MusicSource, Playable, Result, Speaker, Tool, Track};
use serde_json::{Value, json};
use std::sync::Arc;

/// How many search hits are tried before giving up on a source.
///
/// More than one because the first hit is regularly a cover, a live version, or
/// a VIP-gated original; few, because every attempt is a round trip and the
/// user is standing there waiting.
const MAX_ATTEMPTS: usize = 3;

/// Play music, preferring the local library.
///
/// The ordering is a product decision, not an optimisation: the local library
/// is the user's own collection, it always plays in full quality, and it never
/// asks for a membership. NetEase is the fallback for what the shelf does not
/// have — unless the user names it, in which case they get it immediately.
pub struct PlayMusic {
    speaker: Arc<dyn Speaker>,
    local: Arc<dyn MusicSource>,
    netease: Option<Arc<dyn MusicSource>>,
}

impl PlayMusic {
    /// Local library only. NetEase needs a session and is optional, so it is
    /// added separately with [`PlayMusic::with_netease`].
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

    /// The sources to try, in order, plus a note to prefix the answer with when
    /// the request could not be honoured exactly.
    fn plan(&self, requested: Option<&str>) -> (Vec<&Arc<dyn MusicSource>>, &'static str) {
        match requested {
            Some("netease") => match &self.netease {
                // Asking for NetEase explicitly means going straight there: a
                // local hit is not what was asked for.
                Some(netease) => (vec![netease], ""),
                // Falling back rather than refusing — the user wants music, and
                // the note lets the model explain why it is not the one asked
                // for.
                None => (vec![&self.local], "网易云音乐未配置，"),
            },
            Some("local") => (vec![&self.local], ""),
            _ => {
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
    ) -> Result<Vec<Track>> {
        if random {
            // `random` is the only way to say "anything at all": a search
            // cannot express "sample the whole library", and picking the first
            // search hit every time is the opposite of random.
            return Ok(source.random(query).await?.into_iter().collect());
        }
        let query = query.ok_or_else(|| {
            BrainErr::InvalidArguments("`query` is required unless `random` is true".into())
        })?;
        let mut hits = source.search(query).await?;
        hits.truncate(MAX_ATTEMPTS);
        Ok(hits)
    }
}

#[brain::async_trait]
impl Tool for PlayMusic {
    fn name(&self) -> &str {
        "play_music"
    }

    fn description(&self) -> &str {
        "播放音乐。用户想听某首歌、某个歌手，或者想随便听点什么时调用。\
         默认先在本地曲库里找，找不到才去网易云音乐；只有用户明确说了「网易云」之类的话，\
         才把 source 设成 netease。调用成功表示音箱已经开始播放，不需要再做别的。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "要找的歌名、歌手名，或者两者一起，例如「周杰伦 晴天」。\
                                    random 为 true 时，这里可以只写歌手名当作筛选条件，也可以不写。"
                },
                "source": {
                    "type": "string",
                    "enum": ["local", "netease"],
                    "description": "指定曲库来源。不填就是先本地、后网易云。\
                                    只有用户明确要求时才填。"
                },
                "random": {
                    "type": "boolean",
                    "description": "用户说「随便放一首」「来点音乐」这类没有具体目标的请求时设为 true。"
                }
            },
            "required": []
        })
    }

    async fn call(&self, args: Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty());
        let random = args.get("random").and_then(Value::as_bool).unwrap_or(false);
        if query.is_none() && !random {
            return Err(BrainErr::InvalidArguments(
                "要播放什么？请给出 query（歌名或歌手），或者把 random 设为 true".into(),
            ));
        }
        let (sources, note) = self.plan(args.get("source").and_then(Value::as_str));

        // Why each candidate was skipped, so that "found it but it is VIP-only"
        // does not come back looking like "no such song".
        let mut skipped: Vec<String> = Vec::new();
        for source in sources {
            let candidates = match Self::candidates(source, query, random).await {
                Ok(candidates) => candidates,
                // One source failing must not sink the whole request: NetEase
                // being down is no reason to refuse to play a local file.
                Err(e) => {
                    tracing::warn!(source = source.name(), error = %e, "search failed");
                    skipped.push(format!("{}：{e}", source.name()));
                    continue;
                }
            };
            for track in candidates {
                // Resolved here and used immediately — a NetEase URL is only
                // valid for minutes, so nothing above may hold on to one.
                let url = match source.resolve(&track).await {
                    Ok(Playable::Url(url)) => url,
                    // Nothing in this binary produces one (both sources serve
                    // through our own HTTP server), but a speaker on the far
                    // end of a network cannot read this filesystem, so it would
                    // be unplayable if one appeared.
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
                // Tell the user what is coming *before* the music starts, then
                // play it — one call, so a single-channel device can speak the
                // announcement to completion first rather than having the track
                // cut it off (or a later confirmation cut the track off). A
                // failure here is the device, not the music: that is a real
                // error and the model should say so rather than try another song.
                let announcement =
                    format!("{note}正在播放《{}》{}", track.title, artist(&track));
                self.speaker.announce_then_play(&announcement, &url).await?;
                tracing::info!(track = %track.id, source = source.name(), "playing");
                return Ok(announcement);
            }
        }

        Ok(match (skipped.is_empty(), query) {
            (true, Some(query)) => format!("{note}没有找到和「{query}」有关的歌曲"),
            (true, None) => format!("{note}曲库里没有可播放的歌曲"),
            // The reasons are the useful part — the model relays one of them.
            (false, _) => format!("{note}没能播放：{}", skipped.join("；")),
        })
    }
}

/// `— 周杰伦`, or nothing when the source does not know the artist.
fn artist(track: &Track) -> String {
    if track.artist.is_empty() {
        String::new()
    } else {
        format!("，{}", track.artist)
    }
}

/// Stop whatever is playing.
pub struct Stop {
    speaker: Arc<dyn Speaker>,
}

impl Stop {
    pub fn new(speaker: Arc<dyn Speaker>) -> Self {
        Self { speaker }
    }
}

#[brain::async_trait]
impl Tool for Stop {
    fn name(&self) -> &str {
        "stop"
    }

    fn description(&self) -> &str {
        "停止播放。用户说「停」「别放了」「安静」这类话时调用。没有在播放时调用也是安全的。"
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, _args: Value) -> Result<String> {
        self.speaker.stop().await?;
        Ok("已停止播放".into())
    }
}

/// Set the speaker volume.
pub struct SetVolume {
    speaker: Arc<dyn Speaker>,
}

impl SetVolume {
    pub fn new(speaker: Arc<dyn Speaker>) -> Self {
        Self { speaker }
    }
}

#[brain::async_trait]
impl Tool for SetVolume {
    fn name(&self) -> &str {
        "set_volume"
    }

    fn description(&self) -> &str {
        "设置音箱音量，0 到 100。用户说「大声点」「小声点」「音量调到 30」时调用。\
         「大声点」这类相对的说法，自己估一个绝对值（比如比现在高 20）填进去。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "level": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 100,
                    "description": "目标音量，0 是静音，100 最大。"
                }
            },
            "required": ["level"]
        })
    }

    async fn call(&self, args: Value) -> Result<String> {
        let level = args.get("level").ok_or_else(|| {
            BrainErr::InvalidArguments("要把音量调到多少？请给出 0 到 100 的 level".into())
        })?;
        // Models emit `30`, `30.0` and `"30"` for the same intent, and the
        // provider only validates as far as it feels like. All three mean 30.
        let level = level
            .as_i64()
            .or_else(|| level.as_f64().map(|v| v.round() as i64))
            .or_else(|| level.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| {
                BrainErr::InvalidArguments(format!("`{level}` 不是一个音量数字，请给 0 到 100"))
            })?;
        // Clamped, never rejected: "把音量调到 200" plainly means "as loud as it
        // goes", and bouncing it back would only waste a turn.
        let clamped = level.clamp(0, 100) as u8;
        if i64::from(clamped) != level {
            tracing::debug!(asked = level, used = clamped, "volume clamped");
        }
        self.speaker.set_volume(clamped).await?;
        Ok(format!("音量已调到 {clamped}"))
    }
}

/// Tell a story.
///
/// The story itself is the *model's* to write — this tool contributes no
/// content. What it contributes is permission: the system prompt keeps answers
/// to about three sentences, which is right for every request except this one.
/// The returned brief lifts that limit for one turn and says how to write for a
/// speaker (spoken rhythm, no markup, no asking whether to begin).
#[derive(Debug, Default, Clone, Copy)]
pub struct TellStory;

impl TellStory {
    pub fn new() -> Self {
        Self
    }
}

#[brain::async_trait]
impl Tool for TellStory {
    fn name(&self) -> &str {
        "tell_story"
    }

    fn description(&self) -> &str {
        "用户想听故事时调用，比如「讲个故事」「讲个关于小狗的故事」。\
         这个工具不会返回故事内容，它返回的是讲故事的要求——拿到之后由你把故事讲出来。\
         闲聊、辩论、回答问题都不要调用它，直接回答即可。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic": {
                    "type": "string",
                    "description": "用户指定的主题、角色或题材。用户没说就不要填。"
                }
            },
            "required": []
        })
    }

    async fn call(&self, args: Value) -> Result<String> {
        let topic = args
            .get("topic")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|topic| !topic.is_empty());
        let subject = match topic {
            Some(topic) => format!("主题是「{topic}」。"),
            None => "题材你自己定，选一个大多数人都会喜欢的。".to_string(),
        };
        Ok(format!(
            "现在直接开始讲故事，{subject}要求：\
             有开头、经过和结尾，一次讲完，不要问用户想不想听；\
             300 到 600 字，这一次不受「不超过三句话」的限制；\
             口语化、适合朗读，不要用 Markdown、编号、括号注释或表情符号。"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what it was told to do, so a test can assert on the *effect* of
    /// a tool rather than only on the string it returned.
    #[derive(Default)]
    struct FakeSpeaker {
        said: Mutex<Vec<String>>,
        played: Mutex<Vec<String>>,
        volumes: Mutex<Vec<u8>>,
        stops: Mutex<usize>,
        /// Every call fails — a speaker that is unplugged or offline.
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

    /// What a source does when asked to resolve one of its own tracks.
    #[derive(Clone, Copy, PartialEq)]
    enum Resolves {
        /// To a URL naming the source, so a test can tell which one played.
        Fine,
        /// VIP-gated: an ordinary outcome carrying a sayable reason.
        Vip,
    }

    struct FakeSource {
        name: &'static str,
        /// Titles this source holds; a query matches if a title contains it.
        titles: Vec<&'static str>,
        resolves: Resolves,
        /// Queries seen, so "was NetEase even consulted?" is answerable.
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
                Resolves::Fine => Ok(Playable::Url(format!(
                    "http://host/{}/{}",
                    self.name, track.id
                ))),
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

    /// The product rule: the user's own collection wins, and NetEase is not
    /// even consulted when it does.
    #[tokio::test]
    async fn the_local_library_is_tried_first() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local.clone()).with_netease(netease.clone());

        let out = tool.call(json!({ "query": "晴天" })).await.unwrap();
        assert!(out.contains("晴天"), "{out}");
        assert_eq!(*speaker.played.lock().unwrap(), ["http://host/local/晴天"]);
        assert!(
            netease.searches().is_empty(),
            "netease must not be consulted when the local library has the song"
        );
    }

    /// The user must be told what is playing, and told *before* it starts — the
    /// announcement is spoken, not left for a confirmation that lands after the
    /// music. The tool routes through `announce_then_play`, so on the real device
    /// the announcement precedes the track; here we check the words were spoken
    /// and name the song.
    #[tokio::test]
    async fn the_track_is_announced_before_it_plays() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local);

        tool.call(json!({ "query": "晴天" })).await.unwrap();
        let said = speaker.said.lock().unwrap();
        assert_eq!(said.len(), 1, "the track is announced exactly once");
        assert!(said[0].contains("晴天"), "the announcement names the song: {said:?}");
        assert_eq!(*speaker.played.lock().unwrap(), ["http://host/local/晴天"]);
    }

    #[tokio::test]
    async fn netease_takes_over_when_the_local_library_misses() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["别的歌"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local.clone()).with_netease(netease.clone());

        tool.call(json!({ "query": "晴天" })).await.unwrap();
        assert_eq!(local.searches(), ["晴天"]);
        assert_eq!(
            *speaker.played.lock().unwrap(),
            ["http://host/netease/晴天"]
        );
    }

    /// A user who names NetEase means it; a local file with the same name is
    /// not what they asked for.
    #[tokio::test]
    async fn an_explicit_netease_request_skips_the_local_library() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local.clone()).with_netease(netease.clone());

        tool.call(json!({ "query": "晴天", "source": "netease" }))
            .await
            .unwrap();
        assert!(local.searches().is_empty(), "local must not be searched");
        assert_eq!(
            *speaker.played.lock().unwrap(),
            ["http://host/netease/晴天"]
        );
    }

    #[tokio::test]
    async fn an_explicit_local_request_never_reaches_netease() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local.clone()).with_netease(netease.clone());

        tool.call(json!({ "query": "晴天", "source": "local" }))
            .await
            .unwrap();
        assert!(netease.searches().is_empty());
    }

    /// Without a NetEase session there is still music to play; the answer says
    /// why it is not the source that was asked for.
    #[tokio::test]
    async fn asking_for_an_unconfigured_netease_falls_back_and_says_so() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local);

        let out = tool
            .call(json!({ "query": "晴天", "source": "netease" }))
            .await
            .unwrap();
        assert!(out.contains("网易云音乐未配置"), "{out}");
        assert_eq!(speaker.played.lock().unwrap().len(), 1);
    }

    /// "Nothing matched" is an answer, not a failure — the model has to be able
    /// to relay it instead of retrying.
    #[tokio::test]
    async fn a_search_miss_is_an_ordinary_answer() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local);

        let out = tool.call(json!({ "query": "不存在的歌" })).await.unwrap();
        assert!(out.contains("没有找到"), "{out}");
        assert!(out.contains("不存在的歌"), "{out}");
        assert!(speaker.played.lock().unwrap().is_empty());
    }

    /// A VIP-gated track must come back as something the speaker can say, and
    /// must not be reported as playing.
    #[tokio::test]
    async fn a_vip_gated_track_explains_itself_instead_of_pretending_to_play() {
        let speaker = speaker();
        let local = FakeSource::new("local", &[]);
        let netease = FakeSource::vip("netease", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local).with_netease(netease);

        let out = tool.call(json!({ "query": "晴天" })).await.unwrap();
        assert!(out.contains("需要会员"), "{out}");
        assert!(out.contains("晴天"), "{out}");
        assert!(
            speaker.played.lock().unwrap().is_empty(),
            "nothing was playable, so nothing may have been played"
        );
    }

    /// A gated first hit is not the end of the search: the next source still
    /// gets its turn.
    #[tokio::test]
    async fn a_gated_hit_does_not_stop_the_local_library_from_answering() {
        let speaker = speaker();
        let local = FakeSource::vip("local", &["晴天"]);
        let netease = FakeSource::new("netease", &["晴天"]);
        let tool = PlayMusic::new(speaker.clone(), local).with_netease(netease);

        tool.call(json!({ "query": "晴天" })).await.unwrap();
        assert_eq!(
            *speaker.played.lock().unwrap(),
            ["http://host/netease/晴天"]
        );
    }

    #[tokio::test]
    async fn random_uses_the_random_endpoint_not_a_search() {
        let speaker = speaker();
        let local = FakeSource::new("local", &["晴天", "稻香"]);
        let tool = PlayMusic::new(speaker.clone(), local.clone());

        let out = tool.call(json!({ "random": true })).await.unwrap();
        assert!(out.contains("正在播放"), "{out}");
        assert_eq!(*local.randomed.lock().unwrap(), 1);
        assert!(local.searches().is_empty());

        // A filter still goes through `random`, not `search`.
        tool.call(json!({ "random": true, "query": "稻香" }))
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
        let tool = PlayMusic::new(speaker(), FakeSource::new("local", &["晴天"]));
        assert!(matches!(
            tool.call(json!({})).await,
            Err(BrainErr::InvalidArguments(_))
        ));
        assert!(matches!(
            tool.call(json!({ "query": "   " })).await,
            Err(BrainErr::InvalidArguments(_))
        ));
    }

    /// A dead speaker is a real failure: the model must not tell the user music
    /// is playing.
    #[tokio::test]
    async fn a_broken_speaker_is_an_error() {
        let speaker = Arc::new(FakeSpeaker {
            broken: true,
            ..Default::default()
        });
        let tool = PlayMusic::new(speaker, FakeSource::new("local", &["晴天"]));
        assert!(tool.call(json!({ "query": "晴天" })).await.is_err());
    }

    #[tokio::test]
    async fn stop_stops() {
        let speaker = speaker();
        let out = Stop::new(speaker.clone()).call(json!({})).await.unwrap();
        assert!(out.contains("停止"), "{out}");
        assert_eq!(*speaker.stops.lock().unwrap(), 1);
    }

    /// "调到 200" means "as loud as it goes", not "that is invalid".
    #[tokio::test]
    async fn the_volume_is_clamped_at_both_ends_rather_than_rejected() {
        let speaker = speaker();
        let tool = SetVolume::new(speaker.clone());

        assert!(tool.call(json!({ "level": 200 })).await.is_ok());
        assert!(tool.call(json!({ "level": -5 })).await.is_ok());
        assert!(tool.call(json!({ "level": 30 })).await.is_ok());
        assert_eq!(*speaker.volumes.lock().unwrap(), [100, 0, 30]);
    }

    /// The same intent arrives typed three different ways depending on the
    /// model's mood.
    #[tokio::test]
    async fn a_volume_may_be_a_float_or_a_string() {
        let speaker = speaker();
        let tool = SetVolume::new(speaker.clone());

        tool.call(json!({ "level": 30.4 })).await.unwrap();
        tool.call(json!({ "level": "45" })).await.unwrap();
        assert_eq!(*speaker.volumes.lock().unwrap(), [30, 45]);

        assert!(matches!(
            tool.call(json!({})).await,
            Err(BrainErr::InvalidArguments(_))
        ));
        assert!(matches!(
            tool.call(json!({ "level": "响一点" })).await,
            Err(BrainErr::InvalidArguments(_))
        ));
    }

    /// The tool's whole purpose is the escape from the three-sentence rule, so
    /// that has to actually be in what it returns.
    #[tokio::test]
    async fn the_story_brief_lifts_the_brevity_rule_and_carries_the_topic() {
        let brief = TellStory::new()
            .call(json!({ "topic": "小狗" }))
            .await
            .unwrap();
        assert!(brief.contains("小狗"), "{brief}");
        assert!(brief.contains("三句话"), "{brief}");

        let brief = TellStory::new().call(json!({})).await.unwrap();
        assert!(brief.contains("自己定"), "{brief}");
    }

    /// Every schema must be an object schema — function calling accepts nothing
    /// else — and every tool must survive being boxed into the registry.
    #[test]
    fn the_tools_are_registrable_and_their_schemas_well_formed() {
        let speaker = speaker();
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(PlayMusic::new(
                speaker.clone(),
                FakeSource::new("local", &[]),
            )),
            Box::new(Stop::new(speaker.clone())),
            Box::new(SetVolume::new(speaker)),
            Box::new(TellStory::new()),
        ];
        let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, ["play_music", "stop", "set_volume", "tell_story"]);
        for tool in &tools {
            assert_eq!(tool.parameters()["type"], "object", "{}", tool.name());
            assert!(tool.parameters()["properties"].is_object());
            assert!(!tool.description().is_empty());
        }
    }
}
