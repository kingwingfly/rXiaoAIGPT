//! Recognising the Chinese voice commands the speaker reports back to us.
//!
//! The speaker answers every utterance itself; we only get to see the
//! transcribed text afterwards, via the conversation history. So the whole
//! "wake word" mechanism is just pattern matching over that text.

use regex::Regex;
use std::sync::LazyLock;

/// A recognised utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 嘻嘻 — start reacting to playback commands.
    Enable,
    /// 不嘻嘻 — stop reacting to everything but [`Command::Enable`].
    Disable,
    /// 播放<歌手>的歌 — a random track by one artist.
    PlayArtist { artist: String },
    /// 播放<歌手>的<歌名> — one specific track.
    PlayTrack {
        artist: Option<String>,
        title: String,
    },
    /// 随机播放 — a random track from the whole library.
    PlayRandom,
}

/// `不嘻嘻` is matched before `嘻嘻`, and the artist-only pattern before the
/// general one, because each is a special case of the pattern that follows it.
static DISABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new("^不嘻嘻").unwrap());
static ENABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new("^嘻嘻").unwrap());
static PLAY_RANDOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^(随机播放|(随便)?放一?首歌听{0,2})$").unwrap());
static PLAY_ARTIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^(播放|我[想要]听)(?<artist>[^的]+)的歌$").unwrap());
static PLAY_TRACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("^(播放|我[想要]听)(?:(?<artist>[^的]+)的)?(?<title>.+)$").unwrap()
});

impl Command {
    /// Returns `None` for anything we do not handle — the vast majority of what
    /// the user says to the speaker.
    pub fn parse(query: &str) -> Option<Self> {
        let query = query.trim();
        if DISABLE.is_match(query) {
            return Some(Self::Disable);
        }
        if ENABLE.is_match(query) {
            return Some(Self::Enable);
        }
        if PLAY_RANDOM.is_match(query) {
            return Some(Self::PlayRandom);
        }
        if let Some(caps) = PLAY_ARTIST.captures(query) {
            return Some(Self::PlayArtist {
                artist: caps["artist"].to_string(),
            });
        }
        if let Some(caps) = PLAY_TRACK.captures(query) {
            return Some(Self::PlayTrack {
                artist: caps.name("artist").map(|m| m.as_str().to_string()),
                title: caps["title"].to_string(),
            });
        }
        None
    }

    /// Whether this command is obeyed while the agent is disabled.
    pub fn is_always_allowed(&self) -> bool {
        matches!(self, Self::Enable | Self::Disable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles() {
        assert_eq!(Command::parse("嘻嘻"), Some(Command::Enable));
        assert_eq!(Command::parse("嘻嘻，你好"), Some(Command::Enable));
        // `不嘻嘻` must not be read as `嘻嘻`
        assert_eq!(Command::parse("不嘻嘻"), Some(Command::Disable));
    }

    #[test]
    fn random() {
        for query in ["随机播放", "放一首歌听", "随便放首歌听听"] {
            assert_eq!(Command::parse(query), Some(Command::PlayRandom), "{query}");
        }
    }

    #[test]
    fn artist_only() {
        assert_eq!(
            Command::parse("播放周杰伦的歌"),
            Some(Command::PlayArtist {
                artist: "周杰伦".to_string()
            })
        );
        assert_eq!(
            Command::parse("我想听周杰伦的歌"),
            Some(Command::PlayArtist {
                artist: "周杰伦".to_string()
            })
        );
    }

    #[test]
    fn specific_track() {
        assert_eq!(
            Command::parse("播放周杰伦的晴天"),
            Some(Command::PlayTrack {
                artist: Some("周杰伦".to_string()),
                title: "晴天".to_string(),
            })
        );
        assert_eq!(
            Command::parse("我要听晴天"),
            Some(Command::PlayTrack {
                artist: None,
                title: "晴天".to_string(),
            })
        );
    }

    #[test]
    fn unrelated_speech_is_ignored() {
        for query in ["今天天气怎么样", "播放", ""] {
            assert_eq!(Command::parse(query), None, "{query}");
        }
    }
}
