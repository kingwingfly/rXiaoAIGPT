//! The cheap local filter deciding whether an utterance is worth a model call.
//!
//! The speaker reports everything it hears, polled every three seconds, so
//! sending all of it to DeepSeek would be the dominant cost. This gate drops
//! obvious noise but deliberately does **not** parse intent — understanding what
//! the user meant is the model's job, so when in doubt it forwards.

use tracing::debug;

/// Longer than this is almost never speech aimed at a speaker (a television, a
/// whole conversation). Counted in characters so Chinese is not penalised.
const MAX_CHARS: usize = 60;

/// Below this there is nothing to work with, except [`SHORT_COMMANDS`].
const MIN_CHARS: usize = 2;

/// One-character utterances that are real commands, so the [`MIN_CHARS`] floor
/// must not swallow them — `停` is the commonest way to halt playback. The gate
/// still forwards these; the model decides what they mean.
const SHORT_COMMANDS: &[&str] = &["停", "放"];

/// Turns the agent on. Must keep working while off, being the only way back.
const ENABLE: &str = "嘻嘻";

/// Turns the agent off. Checked before [`ENABLE`] because `不嘻嘻` contains
/// `嘻嘻`; wrong order would make the agent impossible to switch off.
const DISABLE: &str = "不嘻嘻";

/// Fillers and bare wake words, matched whole after trimming: `好的` alone is
/// noise, but `好的，放首周杰伦` is a request.
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
    /// Drop it entirely.
    Ignore,
    /// Handled here — say this and stop. Only the toggle phrases, which would
    /// waste a round trip to flip a local boolean.
    Reply(&'static str),
    /// Hand it to the model.
    Forward,
}

/// The gate's state: whether the agent is currently listening.
#[derive(Debug)]
pub struct Gate {
    /// While `false`, only [`DISABLE`]/[`ENABLE`] are looked at.
    enabled: bool,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    /// A gate that starts listening.
    pub fn new() -> Self {
        Self { enabled: true }
    }

    /// Decide what happens to `text`, flipping the enabled flag on a toggle.
    pub fn decide(&mut self, text: &str) -> Decision {
        let text = text.trim();

        // Toggles first, obeyed in both states — while disabled, enable is the
        // only way back in.
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
            None => Decision::Forward,
        }
    }
}

/// Why `text` is not worth a model call, or `None` if it might be.
fn noise_reason(text: &str) -> Option<&'static str> {
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
    // `is_alphanumeric` covers Han characters — this drops punctuation and "……".
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

    /// While off, only the enable phrase gets through — else "off" would not
    /// stop the spending.
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

    /// The gate must err towards forwarding: anything the model could plausibly
    /// act on has to reach it.
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
