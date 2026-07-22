//! A hardware-agnostic intent framework for voice assistants.
//!
//! `brain` is the seam between an LLM deciding what to do and a device doing it.
//! It must not depend on `xiaoai`, `netease`, or any other device/content crate,
//! so the same intent layer can drive a speaker today and a microphone tomorrow.
//!
//! It offers three traits — [`UtteranceSource`] (input), [`Speaker`] (output),
//! [`MusicSource`] (content) — plus [`LlmClient`] (an OpenAI-compatible client,
//! defaulting to DeepSeek) and [`Agent`] (the loop). Capabilities the model can
//! call are not defined here: [`Agent::connect`] takes an MCP transport and talks
//! to a tool server over it, so adding a capability never touches this crate.

pub mod client;
pub mod error;
pub mod run;
pub mod traits;

pub use client::{
    ChatMessage, ChatResponse, ClientConfig, DEFAULT_API_BASE, DEFAULT_MODEL, LlmClient, ToolCall,
};
pub use error::{BrainErr, Result};
pub use run::{Agent, AgentConfig, DEFAULT_SYSTEM_PROMPT, run};
pub use traits::{
    DynMusicSource, DynSpeaker, MusicSource, Playable, Speaker, Track, Utterance, UtteranceSource,
};
