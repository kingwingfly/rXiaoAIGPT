//! A hardware-agnostic intent framework for voice assistants.
//!
//! This crate is the seam between "an LLM deciding what to do" and "a device
//! doing it". It contains only the contract — four traits and the values they
//! exchange — and no implementation of either side.
//!
//! # The decoupling contract
//!
//! **`brain` must not depend on `xiaoai`, `netease`, `xiaoai_llm`, or any other
//! crate tied to a particular device or content provider.** That is not a
//! stylistic preference: the point of the framework is that the same intent
//! layer can drive a XiaoAi speaker today, a microphone and a local sound card
//! tomorrow, and a webhook in a test — so nothing here may know which it is.
//!
//! Concretely:
//!
//! - Dependencies stay limited to `serde`, `serde_json`, `thiserror` and
//!   `async-trait`. Adding an HTTP client, an audio library, or a device SDK
//!   here is a bug.
//! - Errors are [`BrainErr`], whose variants carry strings rather than foreign
//!   error types; implementations bridge with [`BrainErr::backend`].
//! - Identifiers that only one side understands — [`Track::id`] — are opaque
//!   strings, handed back to their origin unread.
//!
//! The dependency arrow points *inward*: `xiaoai_llm` depends on `brain`,
//! `xiaoai` and `netease`, and implements `brain`'s traits in terms of the other
//! two. `brain` depends on none of them.
//!
//! # The pieces
//!
//! | Trait | Role | Typical implementation |
//! |---|---|---|
//! | [`UtteranceSource`] | input | polls the speaker's conversation history |
//! | [`Speaker`] | output | the speaker's remote-control API |
//! | [`MusicSource`] | content | a local library, or NetEase |
//! | [`Tool`] | capability | one function the model can call |
//!
//! A control loop (not in this crate — see unit U6) pulls an [`Utterance`] from
//! the source, hands it and the registered tools to a model, runs whichever
//! [`Tool`]s the model picks, and feeds their results back until the model
//! stops calling tools.
//!
//! # Adding a capability
//!
//! Two steps, and neither of them touches the loop:
//!
//! 1. Implement [`Tool`] on a struct holding whatever it needs (a
//!    [`Speaker`] handle, a [`MusicSource`], a config value). Write
//!    [`Tool::description`] and [`Tool::parameters`] for the *model* — they are
//!    prompt text, and are the whole of how it learns the capability exists.
//! 2. Register the boxed tool with the agent.
//!
//! Nothing dispatches on tool names, so a new capability is purely additive.

pub mod error;
pub mod traits;

pub use error::{BrainErr, Result};
pub use traits::{MusicSource, Playable, Speaker, Tool, Track, Utterance, UtteranceSource};

/// Re-exported so implementors can write `#[brain::async_trait]` without taking
/// their own `async-trait` dependency — and, more importantly, without risking a
/// version mismatch against the one these traits are declared with.
pub use async_trait::async_trait;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// A tool doing nothing, existing only to prove the trait is object-safe:
    /// a registry has to hold heterogeneous tools behind `dyn Tool`, and losing
    /// that property would not otherwise be caught until U6.
    struct Noop;

    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "Does nothing."
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        async fn call(&self, _args: Value) -> Result<String> {
            Ok("ok".into())
        }
    }

    #[tokio::test]
    async fn tools_are_object_safe() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(Noop)];
        assert_eq!(registry[0].name(), "noop");
        assert_eq!(registry[0].call(json!({})).await.unwrap(), "ok");
    }

    /// The other three traits must be usable as trait objects too — the loop
    /// holds a `Box<dyn Speaker>` it cannot name the concrete type of.
    #[test]
    fn the_other_traits_are_object_safe() {
        fn assert_dyn<T: ?Sized>() {}
        assert_dyn::<dyn Speaker>();
        assert_dyn::<dyn UtteranceSource>();
        assert_dyn::<dyn MusicSource>();
    }

    /// These cross a process boundary (a cached poll cursor, a tool result), so
    /// their serialised form is part of the contract.
    #[test]
    fn values_round_trip_through_json() {
        let utterance = Utterance::new("1700000000000", "播放晴天", 1_700_000_000_000);
        let json = serde_json::to_string(&utterance).unwrap();
        assert_eq!(serde_json::from_str::<Utterance>(&json).unwrap(), utterance);

        let track = Track {
            id: "186016".into(),
            title: "晴天".into(),
            artist: "周杰伦".into(),
            source: "netease".into(),
            duration_ms: Some(269_000),
        };
        let json = serde_json::to_string(&track).unwrap();
        assert_eq!(serde_json::from_str::<Track>(&json).unwrap(), track);

        let playable = Playable::Url("https://example.com/a.mp3".into());
        let json = serde_json::to_string(&playable).unwrap();
        assert_eq!(serde_json::from_str::<Playable>(&json).unwrap(), playable);
    }
}
