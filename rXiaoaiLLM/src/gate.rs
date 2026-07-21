//! The cheap local filter that decides whether an utterance is worth a model
//! call.
//!
//! # Why this exists at all
//!
//! The speaker reports **everything** it hears, and we poll that history every
//! three seconds. A household speaker picks up television, half-sentences,
//! "嗯", and every question the built-in assistant already answered by itself.
//! Sending all of it to DeepSeek would be the dominant cost of running this
//! program, for turns whose right answer is silence.
//!
//! # Why it is not a parser
//!
//! It is tempting to grow this into "recognise 播放 and extract the song name" —
//! that is precisely the regex layer this refactor removed. Understanding what
//! the user *meant* is the model's job now; this file may only answer "is it
//! plausibly for us at all?", and when in doubt it must answer yes. A gate that
//! guesses wrong costs one API call; a gate that parses wrong silently breaks
//! every request its author did not think of.
//!
//! So: no intent, no slots, no ordering subtleties beyond the one toggle pair
//! below — just length, emptiness, and a short list of noises.

use tracing::debug;

/// Utterances at least this long are almost never speech directed at a speaker;
/// they are a television, a phone call, or a transcription of a whole
/// conversation. Counted in characters, so a Chinese sentence is not penalised
/// against an English one.
const MAX_CHARS: usize = 60;

/// Below this, there is nothing for a model to work with — "嗯", "啊", a single
/// stray syllable from the room. [`SHORT_COMMANDS`] are the exceptions.
const MIN_CHARS: usize = 2;

/// One-character utterances that are nonetheless real commands, so the
/// [`MIN_CHARS`] floor must not swallow them. `停` — "stop" — is the obvious
/// one: it is the single most common way to halt playback and the `stop` tool
/// advertises it by name, yet it is one character. This is *not* the gate
/// parsing intent; it still forwards to the model, which decides what `停`
/// means. It only says these short utterances are worth the model's attention.
const SHORT_COMMANDS: &[&str] = &["停", "放"];

/// Turns the agent on again. Also the phrase that must keep working while the
/// agent is off, since it is the only way back.
const ENABLE: &str = "嘻嘻";

/// Turns the agent off. **Checked before [`ENABLE`]**: `不嘻嘻` contains
/// `嘻嘻`, so a scheme loose enough to catch "嘻嘻，你好" also sees the enable
/// phrase inside the disable one. Getting that order wrong makes the agent
/// impossible to switch off, which is why both live in one function.
const DISABLE: &str = "不嘻嘻";

/// Acknowledgements, fillers and bare wake words. Matched **whole**, after
/// trimming: `好的` alone is noise, but `好的，放首周杰伦` is a request.
const NOISE: &[&str] = &[
    "嗯",
    "啊",
    "哦",
    "呃",
    "噢",
    "唉",
    "喂",
    "哈",
    "嗯嗯",
    "哦哦",
    "好",
    "好的",
    "好吧",
    "行",
    "行吧",
    "是",
    "是的",
    "不是",
    "对",
    "对的",
    "不",
    "不用",
    "没事",
    "没有",
    "小爱",
    "小爱同学",
    "小爱小爱",
    "你好",
    "在吗",
    "谢谢",
    "再见",
    "拜拜",
];

/// What to do with one utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Drop it. No model call, no speech, nothing.
    Ignore,
    /// Handled here. Say this and stop — the toggle phrases are the one thing
    /// the gate answers itself, because routing "嘻嘻" through the model would
    /// pay for a round trip to change a local boolean.
    Reply(&'static str),
    /// Hand it to the model.
    Forward,
}

/// The gate's state: whether the agent is currently listening.
#[derive(Debug)]
pub struct Gate {
    /// While `false`, only [`DISABLE`]/[`ENABLE`] are looked at and everything
    /// else is dropped before it can cost anything.
    enabled: bool,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    /// A gate that starts listening, matching the agent's historical behaviour.
    pub fn new() -> Self {
        Self { enabled: true }
    }

    /// Decide what happens to `text`, updating the enabled flag if it is a
    /// toggle.
    pub fn decide(&mut self, text: &str) -> Decision {
        let text = text.trim();

        // The toggles come first and are obeyed in both states: while disabled,
        // the enable phrase is the only way back in.
        if text.starts_with(DISABLE) {
            self.enabled = false;
            return Decision::Reply("奶龙，关闭！");
        }
        if text.starts_with(ENABLE) {
            self.enabled = true;
            return Decision::Reply("奶龙，启动！");
        }
        if !self.enabled {
            debug!(%text, "gate closed");
            return Decision::Ignore;
        }

        match noise_reason(text) {
            Some(reason) => {
                debug!(%text, reason, "utterance filtered out");
                Decision::Ignore
            }
            // Anything left is plausibly for us. Deciding *what* it means is
            // the model's job, not this function's.
            None => Decision::Forward,
        }
    }
}

/// Why `text` is not worth a model call, or `None` if it might be.
///
/// Split out and total so it can be tested on its own, and so that adding a
/// filter is visibly a filter rather than a special case in the control flow.
fn noise_reason(text: &str) -> Option<&'static str> {
    // A real one-character command beats the length floor.
    if SHORT_COMMANDS.contains(&text) {
        return None;
    }
    let chars = text.chars().count();
    if chars < MIN_CHARS {
        return Some("too short");
    }
    if chars > MAX_CHARS {
        return Some("too long to be speech aimed at a speaker");
    }
    // Punctuation, "……", a transcription that captured nothing. `is_alphanumeric`
    // covers Han characters as well as letters and digits.
    if !text.chars().any(char::is_alphanumeric) {
        return Some("no words in it");
    }
    if NOISE.contains(&text) {
        return Some("filler");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toggles_work_and_disable_wins() {
        let mut gate = Gate::new();
        assert_eq!(gate.decide("不嘻嘻"), Decision::Reply("奶龙，关闭！"));
        assert!(!gate.enabled);
        // `不嘻嘻` must never be read as `嘻嘻`.
        assert_eq!(gate.decide("不嘻嘻了"), Decision::Reply("奶龙，关闭！"));
        assert!(!gate.enabled);

        assert_eq!(gate.decide("嘻嘻"), Decision::Reply("奶龙，启动！"));
        assert!(gate.enabled);
        assert_eq!(gate.decide("嘻嘻，你好"), Decision::Reply("奶龙，启动！"));
    }

    /// While off, nothing but the enable phrase gets through — that is what
    /// "off" has to mean, or turning it off would not stop the spending.
    #[test]
    fn while_disabled_only_the_enable_phrase_is_obeyed() {
        let mut gate = Gate::new();
        gate.decide("不嘻嘻");
        for text in ["播放晴天", "讲个故事", "今天天气怎么样"] {
            assert_eq!(gate.decide(text), Decision::Ignore, "{text}");
        }
        assert_eq!(gate.decide("嘻嘻"), Decision::Reply("奶龙，启动！"));
        assert_eq!(gate.decide("播放晴天"), Decision::Forward);
    }

    #[test]
    fn noise_is_dropped() {
        let mut gate = Gate::new();
        for text in [
            "",
            "   ",
            "嗯",
            "好的",
            "小爱同学",
            "……",
            "，。？",
            // A television, or a transcript of half a conversation.
            "然后我就跟他说这个事情其实没有那么复杂你要是早点告诉我的话我们完全可以换一个方案来处理这件事情根本不至于闹成现在这样大家都下不来台",
        ] {
            assert_eq!(gate.decide(text), Decision::Ignore, "{text:?}");
        }
    }

    /// The gate must err towards forwarding: everything the model could
    /// plausibly act on has to reach it, including requests no regex in the old
    /// parser would have recognised. This test is the point of the refactor.
    #[test]
    fn anything_plausibly_for_us_is_forwarded() {
        let mut gate = Gate::new();
        for text in [
            "播放晴天",
            "我想听周杰伦的歌",
            "随便放首歌",
            "讲个故事",
            "声音大一点",
            "别唱了",
            "跟我辩论一下猫和狗哪个更好",
            "今天天气怎么样",
            // A filler word is noise alone but not as part of a sentence.
            "好的那就放稻香吧",
            // Single-character commands must survive the length floor: `停` is
            // exactly what the stop tool tells the model to listen for.
            "停",
            "放",
        ] {
            assert_eq!(gate.decide(text), Decision::Forward, "{text}");
        }
    }
}
