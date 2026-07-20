//! The control loop: utterances in, tool calls and speech out.
//!
//! # What is deliberately absent
//!
//! Every device-specific idea. The loop knows about [`UtteranceSource`] and
//! [`Speaker`] and nothing else — no polling interval, no cloud API, no XiaoAi.
//! Swapping the speaker for a microphone and a sound card, or for a test fake,
//! is a change of type parameter and nothing more. That is the point of the
//! crate, and any `if` in here that mentions a device is a bug.
//!
//! # The turn
//!
//! One utterance becomes one *turn*, which may take several round trips:
//!
//! 1. Send `[system] + history + [user]` to the model together with the
//!    registry's tool schemas.
//! 2. If the model answered in prose, that is the turn — speak it. This is not a
//!    fallback: "讲个故事", "跟我辩论一下" and most of what people say to a
//!    speaker need no tool at all.
//! 3. Otherwise append the assistant message *including its tool calls*, run
//!    each call, append a `role: "tool"` message per call, and ask again.
//! 4. Repeat up to [`AgentConfig::max_tool_iterations`] times.
//!
//! # Failure is routine
//!
//! Three things go wrong often enough that they are designed for rather than
//! guarded against:
//!
//! - **Malformed tool arguments.** DeepSeek emits `arguments` as a JSON string
//!   and does not always emit valid JSON. That is turned into a tool-result
//!   message describing the problem, which the model can and does recover from.
//!   It is never a panic and never ends the turn.
//! - **Hallucinated tool names.** Same treatment, via
//!   [`ToolRegistry::dispatch`].
//! - **A dead turn.** The speaker being briefly offline, or the API timing out,
//!   fails one utterance. It is logged and the loop moves to the next one; the
//!   process does not exit because a Wi-Fi hiccup ate one command.

use crate::client::{ChatMessage, ChatResponse, LlmClient, ToolCall};
use crate::error::{BrainErr, Result};
use crate::registry::ToolRegistry;
use crate::traits::{Speaker, Utterance, UtteranceSource};
use std::sync::Arc;

/// The default system prompt.
///
/// Written in Chinese because the users speak Chinese to the speaker, and a
/// prompt in the reply language is the cheapest way to keep the replies in it.
/// It says nothing about which tools exist — those come from the registry, so
/// that adding a capability stays a one-file change.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
你是一台智能音箱的助手。用户通过语音和你交流，你的回答会被朗读出来。

规则：
- 用简体中文回答，语气自然口语化，除非用户要求，否则不要超过三句话。
- 回答里不要出现 Markdown、表情符号、编号列表或任何朗读时会很奇怪的排版。
- 需要播放音乐、控制音量等操作时，调用相应的工具，不要假装已经做了。
- 讲故事、聊天、辩论这类请求直接回答即可，不需要调用任何工具。
- 工具返回错误时，把原因用一句话告诉用户，或者换个参数重试一次。";

/// Forwarding impls so a shared or boxed speaker satisfies `S: Speaker`.
///
/// Without these, `Agent<Arc<dyn Speaker>>` does not compile, and the one thing
/// every real wiring needs — the agent and a "set the volume" tool holding the
/// *same* device — becomes awkward. `?Sized` so `dyn Speaker` itself qualifies.
macro_rules! forward_speaker {
    ($ptr:ident) => {
        #[async_trait::async_trait]
        impl<T: Speaker + ?Sized> Speaker for $ptr<T> {
            async fn say(&self, text: &str) -> Result<()> {
                (**self).say(text).await
            }
            async fn play(&self, url: &str) -> Result<()> {
                (**self).play(url).await
            }
            async fn stop(&self) -> Result<()> {
                (**self).stop().await
            }
            async fn set_volume(&self, level: u8) -> Result<()> {
                (**self).set_volume(level).await
            }
            async fn is_playing(&self) -> Result<bool> {
                (**self).is_playing().await
            }
        }
    };
}

forward_speaker!(Arc);
forward_speaker!(Box);

/// Knobs on the loop's behaviour.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Standing instructions, sent first on every turn. Defaults to
    /// [`DEFAULT_SYSTEM_PROMPT`].
    pub system_prompt: String,

    /// How many rounds of tool execution one utterance may take before the turn
    /// is abandoned.
    ///
    /// A model that keeps asking for the same search because it dislikes the
    /// answer would otherwise burn tokens forever. Five is generous for a
    /// speaker: the deepest real chain is search → resolve → play.
    pub max_tool_iterations: usize,

    /// How many past messages to carry into the next turn, so "再来一首" and
    /// "换一个" resolve against what just happened.
    ///
    /// Counted in messages, not turns, and always trimmed from the oldest end.
    /// Bounded because a speaker runs for weeks and an unbounded history would
    /// grow until it is both expensive and, eventually, longer than the context.
    pub max_history_messages: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_tool_iterations: 5,
            max_history_messages: 12,
        }
    }
}

impl AgentConfig {
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_max_tool_iterations(mut self, max: usize) -> Self {
        self.max_tool_iterations = max;
        self
    }

    pub fn with_max_history_messages(mut self, max: usize) -> Self {
        self.max_history_messages = max;
        self
    }
}

/// The assembled assistant: a model, a set of capabilities, and something to
/// speak through.
///
/// Generic over the [`Speaker`] rather than boxing it, so a caller keeps its
/// concrete type and pays no dynamic dispatch for something it already knows.
///
/// The same speaker is usually also held by a tool ("turn it up" is a
/// [`Tool`](crate::Tool)), so the common shape is one `Arc` cloned into both —
/// which works because [`Speaker`] is implemented for `Arc<T>` and `Box<T>`
/// below.
pub struct Agent<S: Speaker> {
    client: LlmClient,
    registry: ToolRegistry,
    speaker: S,
    config: AgentConfig,
    /// User/assistant pairs only. Assistant messages carrying tool calls are
    /// *not* kept: they are only valid next to their `role: "tool"` replies, and
    /// a trim that cut between the two would produce a request the API rejects.
    /// The tool results are already summarised in the assistant's prose, which
    /// is what a follow-up needs anyway.
    history: Vec<ChatMessage>,
}

impl<S: Speaker> Agent<S> {
    /// An agent with [`AgentConfig::default`].
    pub fn new(client: LlmClient, registry: ToolRegistry, speaker: S) -> Self {
        Self::with_config(client, registry, speaker, AgentConfig::default())
    }

    pub fn with_config(
        client: LlmClient,
        registry: ToolRegistry,
        speaker: S,
        config: AgentConfig,
    ) -> Self {
        Self {
            client,
            registry,
            speaker,
            config,
            history: Vec::new(),
        }
    }

    /// The speaker, for callers that also want to drive it directly.
    pub fn speaker(&self) -> &S {
        &self.speaker
    }

    /// Forget the conversation. Useful after a long silence, when a follow-up
    /// pronoun almost certainly no longer refers to what it used to.
    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Consume utterances until the source is exhausted.
    ///
    /// Returns `Ok(())` when [`UtteranceSource::next`] returns `None`, which for
    /// a live source means shutdown and for a scripted one means the script
    /// ended. A failed turn is logged and skipped — see the module docs — so the
    /// only way out is exhaustion.
    pub async fn run<E: UtteranceSource>(&mut self, source: &mut E) -> Result<()> {
        while let Some(utterance) = source.next().await {
            tracing::info!(id = %utterance.id, text = %utterance.text, "handling utterance");

            match self.handle(&utterance).await {
                Ok(reply) => {
                    if reply.trim().is_empty() {
                        // A tool did the work and the model had nothing to add.
                        // Saying "" would make a speaker emit a click.
                        tracing::debug!(id = %utterance.id, "turn produced no speech");
                        continue;
                    }
                    if let Err(e) = self.speaker.say(&reply).await {
                        tracing::warn!(error = %e, "could not speak the reply");
                    }
                }
                Err(e) => {
                    tracing::warn!(id = %utterance.id, error = %e, "turn failed; skipping");
                }
            }
        }

        tracing::info!("utterance source exhausted; stopping");
        Ok(())
    }

    /// Run one turn and return what should be said, without saying it.
    ///
    /// Exposed separately so a caller can drive the agent from something that is
    /// not an [`UtteranceSource`] — an HTTP handler, a REPL, a test.
    pub async fn handle(&mut self, utterance: &Utterance) -> Result<String> {
        let tools = self.registry.schemas();

        let mut messages = Vec::with_capacity(self.history.len() + 2);
        messages.push(ChatMessage::system(&self.config.system_prompt));
        messages.extend(self.history.iter().cloned());
        messages.push(ChatMessage::user(&utterance.text));

        // `max_tool_iterations` rounds of tools means one more model call than
        // that: the last one is the model's chance to answer with what the tools
        // told it.
        for round in 0..=self.config.max_tool_iterations {
            let response: ChatResponse = self.client.chat(&messages, &tools).await?;

            if !response.wants_tools() {
                let reply = response.content.unwrap_or_default();
                self.remember(&utterance.text, &reply);
                return Ok(reply);
            }

            if round == self.config.max_tool_iterations {
                // Do not run this round's calls: we would have no budget left to
                // send their results, so the work would be wasted and, for a
                // tool with side effects, wrong.
                return Err(BrainErr::Backend(format!(
                    "model still requesting tools after {} rounds; giving up on this turn",
                    self.config.max_tool_iterations
                )));
            }

            tracing::debug!(
                round,
                calls = response.tool_calls.len(),
                "running tool calls"
            );

            messages.push(ChatMessage::Assistant {
                content: response.content,
                tool_calls: response.tool_calls.clone(),
            });

            for call in &response.tool_calls {
                let result = self.run_tool_call(call).await;
                messages.push(ChatMessage::tool(&call.id, result));
            }
        }

        unreachable!("the loop returns on its last iteration");
    }

    /// Run one call and render its outcome as the string the model will read.
    ///
    /// Never returns `Err`: at this point every failure — bad JSON, unknown
    /// name, a tool that blew up — is information the model should get so it can
    /// try something else, not a reason to abandon the turn.
    async fn run_tool_call(&self, call: &ToolCall) -> String {
        let args = match parse_arguments(&call.arguments) {
            Ok(args) => args,
            Err(message) => {
                tracing::warn!(
                    tool = %call.name,
                    arguments = %call.arguments,
                    "model produced unparseable tool arguments"
                );
                return format!("error: {message}");
            }
        };

        match self.registry.dispatch(&call.name, args).await {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(tool = %call.name, error = %e, "tool call failed");
                format!("error: {e}")
            }
        }
    }

    /// Append one exchange to the history and trim the oldest.
    fn remember(&mut self, user: &str, assistant: &str) {
        if self.config.max_history_messages == 0 {
            return;
        }

        self.history.push(ChatMessage::user(user));
        self.history.push(ChatMessage::assistant(assistant));

        // Drain from the front so the window slides. The limit is rounded down
        // to an even number first: history is only ever whole user/assistant
        // exchanges, and a window starting on an orphaned assistant reply reads
        // to the model as if it spoke unprompted.
        let keep = self.config.max_history_messages & !1;
        let excess = self.history.len().saturating_sub(keep);
        if excess > 0 {
            self.history.drain(..excess);
        }
    }
}

/// Turn the model's `arguments` string into an object, or say why it cannot be.
///
/// Separate and total so it can be tested directly, because this is the single
/// most failure-prone step in the whole loop. Three tolerances are deliberate:
/// an empty string, a literal `null`, and whitespace all mean "no arguments",
/// which is what a model emits for a zero-parameter tool. Anything else that is
/// not a JSON object is an error message rather than a coerced guess.
fn parse_arguments(arguments: &str) -> std::result::Result<serde_json::Value, String> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }

    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
        format!("tool arguments were not valid JSON ({e}); re-send them as a JSON object")
    })?;

    match value {
        serde_json::Value::Object(_) => Ok(value),
        serde_json::Value::Null => Ok(serde_json::json!({})),
        other => Err(format!(
            "tool arguments must be a JSON object, got {other}; re-send them as a JSON object"
        )),
    }
}

/// Build an [`Agent`] and run it to exhaustion — the one-line entry point.
///
/// ```no_run
/// # use brain::{AgentConfig, LlmClient, Result, Speaker, ToolRegistry, UtteranceSource};
/// # async fn go<S: Speaker, E: UtteranceSource>(speaker: S, mut source: E) -> Result<()> {
/// let client = LlmClient::new(std::env::var("DEEPSEEK_API_KEY").unwrap());
/// brain::run(client, ToolRegistry::new(), speaker, &mut source, AgentConfig::default()).await
/// # }
/// ```
pub async fn run<E: UtteranceSource, S: Speaker>(
    client: LlmClient,
    registry: ToolRegistry,
    speaker: S,
    source: &mut E,
    config: AgentConfig,
) -> Result<()> {
    Agent::with_config(client, registry, speaker, config)
        .run(source)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientConfig;
    use crate::traits::Tool;
    use axum::{Json, Router, extract::State, routing::post};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    // ---------------------------------------------------------------- mocks

    /// A stand-in for the completions endpoint.
    ///
    /// Serves a queue of canned OpenAI-shaped responses and records every
    /// request body, so a test can assert on what the loop *sent back* — which
    /// is the only way to check that tool results and tool errors reached the
    /// model. The real DeepSeek API is never contacted by any test in this
    /// crate.
    #[derive(Clone, Default)]
    struct MockApi {
        /// Popped from the front; the last one repeats once exhausted so a test
        /// about looping does not have to enqueue an infinite list.
        responses: Arc<Mutex<Vec<Value>>>,
        requests: Arc<Mutex<Vec<Value>>>,
    }

    impl MockApi {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<Value> {
            self.requests.lock().unwrap().clone()
        }

        /// Bind on an ephemeral port and return the base URL to point a client
        /// at.
        async fn serve(&self) -> String {
            let app = Router::new()
                .route("/chat/completions", post(handler))
                .with_state(self.clone());

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            format!("http://{addr}")
        }
    }

    async fn handler(State(mock): State<MockApi>, Json(body): Json<Value>) -> Json<Value> {
        mock.requests.lock().unwrap().push(body);

        let mut responses = mock.responses.lock().unwrap();
        let response = if responses.len() > 1 {
            responses.remove(0)
        } else {
            responses.first().cloned().unwrap_or_else(|| text_reply(""))
        };
        Json(response)
    }

    /// A completion whose message is plain prose.
    fn text_reply(content: &str) -> Value {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 0,
            "model": "mock",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }]
        })
    }

    /// A completion asking for one function call with the given raw `arguments`
    /// string — raw so a test can supply invalid JSON.
    fn tool_call_reply(id: &str, name: &str, arguments: &str) -> Value {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 0,
            "model": "mock",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    }

    /// Records what it was asked to do; does nothing else.
    #[derive(Clone, Default)]
    struct FakeSpeaker {
        said: Arc<Mutex<Vec<String>>>,
        played: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Speaker for FakeSpeaker {
        async fn say(&self, text: &str) -> Result<()> {
            self.said.lock().unwrap().push(text.to_string());
            Ok(())
        }
        async fn play(&self, url: &str) -> Result<()> {
            self.played.lock().unwrap().push(url.to_string());
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn set_volume(&self, _level: u8) -> Result<()> {
            Ok(())
        }
        async fn is_playing(&self) -> Result<bool> {
            Ok(false)
        }
    }

    /// A fixed list of utterances, then exhaustion.
    struct Script(std::vec::IntoIter<Utterance>);

    impl Script {
        fn new(texts: &[&str]) -> Self {
            Self(
                texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| Utterance::new(i.to_string(), *t, i as u64))
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        }
    }

    #[async_trait::async_trait]
    impl UtteranceSource for Script {
        async fn next(&mut self) -> Option<Utterance> {
            self.0.next()
        }
    }

    /// Records that it ran, and echoes its `query`.
    #[derive(Clone, Default)]
    struct RecordingTool {
        calls: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait::async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            "play_music"
        }
        fn description(&self) -> &str {
            "Play a song."
        }
        fn parameters(&self) -> Value {
            json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            })
        }
        async fn call(&self, args: Value) -> Result<String> {
            self.calls.lock().unwrap().push(args.clone());
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            Ok(format!("now playing {query}"))
        }
    }

    async fn agent(
        mock: &MockApi,
        registry: ToolRegistry,
        config: AgentConfig,
    ) -> Agent<FakeSpeaker> {
        let base = mock.serve().await;
        let client = LlmClient::with_config(
            ClientConfig::new("test-key")
                .with_api_base(base)
                .with_model("mock"),
        );
        Agent::with_config(client, registry, FakeSpeaker::default(), config)
    }

    /// The tool messages the loop sent back on request `n`.
    fn tool_messages(request: &Value) -> Vec<String> {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["content"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    // ---------------------------------------------------------------- tests

    #[tokio::test]
    async fn a_plain_text_reply_is_spoken() {
        let mock = MockApi::new(vec![text_reply("从前有座山")]);
        let mut agent = agent(&mock, ToolRegistry::new(), AgentConfig::default()).await;

        agent.run(&mut Script::new(&["讲个故事"])).await.unwrap();

        assert_eq!(
            agent.speaker().said.lock().unwrap().as_slice(),
            ["从前有座山"]
        );
        // No tools registered means the `tools` key is omitted entirely.
        assert!(mock.requests()[0].get("tools").is_none());
    }

    #[tokio::test]
    async fn a_tool_call_is_dispatched_and_its_result_fed_back() {
        let tool = RecordingTool::default();
        let mock = MockApi::new(vec![
            tool_call_reply("call_1", "play_music", r#"{"query":"晴天"}"#),
            text_reply("好的，播放晴天"),
        ]);
        let mut agent = agent(
            &mock,
            ToolRegistry::new().with(tool.clone()),
            AgentConfig::default(),
        )
        .await;

        agent.run(&mut Script::new(&["放晴天"])).await.unwrap();

        assert_eq!(
            tool.calls.lock().unwrap().as_slice(),
            [json!({"query": "晴天"})]
        );
        assert_eq!(
            agent.speaker().said.lock().unwrap().as_slice(),
            ["好的，播放晴天"]
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["tools"][0]["function"]["name"], "play_music");
        assert_eq!(tool_messages(&requests[1]), ["now playing 晴天"]);
        // The assistant message carrying the call must be replayed, or the tool
        // message has nothing to attach to.
        let assistant = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    }

    #[tokio::test]
    async fn malformed_arguments_are_reported_to_the_model_not_panicked_on() {
        let tool = RecordingTool::default();
        let mock = MockApi::new(vec![
            // Truncated JSON — exactly what DeepSeek produces when it runs out
            // of patience mid-object.
            tool_call_reply("call_1", "play_music", r#"{"query":"晴天"#),
            text_reply("抱歉，我没听清"),
        ]);
        let mut agent = agent(
            &mock,
            ToolRegistry::new().with(tool.clone()),
            AgentConfig::default(),
        )
        .await;

        agent.run(&mut Script::new(&["放晴天"])).await.unwrap();

        // The tool was never reached, and the model was told why.
        assert!(tool.calls.lock().unwrap().is_empty());
        let messages = tool_messages(&mock.requests()[1]);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("error:"), "{}", messages[0]);
        assert!(messages[0].contains("not valid JSON"), "{}", messages[0]);
        assert_eq!(
            agent.speaker().said.lock().unwrap().as_slice(),
            ["抱歉，我没听清"]
        );
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected_with_an_explanation() {
        assert!(
            parse_arguments("[1,2]")
                .unwrap_err()
                .contains("JSON object")
        );
        assert!(
            parse_arguments("\"晴天\"")
                .unwrap_err()
                .contains("JSON object")
        );
        // The shapes a model uses for "no arguments" all mean the same thing.
        assert_eq!(parse_arguments("").unwrap(), json!({}));
        assert_eq!(parse_arguments("  ").unwrap(), json!({}));
        assert_eq!(parse_arguments("null").unwrap(), json!({}));
        assert_eq!(parse_arguments(r#"{"a":1}"#).unwrap(), json!({"a": 1}));
    }

    #[tokio::test]
    async fn an_unknown_tool_name_errors_cleanly_back_to_the_model() {
        let mock = MockApi::new(vec![
            tool_call_reply("call_1", "launch_missiles", "{}"),
            text_reply("我做不到"),
        ]);
        let mut agent = agent(
            &mock,
            ToolRegistry::new().with(RecordingTool::default()),
            AgentConfig::default(),
        )
        .await;

        agent.run(&mut Script::new(&["发射导弹"])).await.unwrap();

        let messages = tool_messages(&mock.requests()[1]);
        assert!(
            messages[0].contains("no tool named `launch_missiles`"),
            "{}",
            messages[0]
        );
        // The error names the real tools, so the model can pick one.
        assert!(messages[0].contains("play_music"), "{}", messages[0]);
        assert_eq!(
            agent.speaker().said.lock().unwrap().as_slice(),
            ["我做不到"]
        );
    }

    #[tokio::test]
    async fn the_iteration_cap_stops_a_model_that_loops() {
        let tool = RecordingTool::default();
        // The mock repeats its last response forever, so the model never stops
        // asking for tools.
        let mock = MockApi::new(vec![tool_call_reply(
            "call_1",
            "play_music",
            r#"{"query":"晴天"}"#,
        )]);
        let mut agent = agent(
            &mock,
            ToolRegistry::new().with(tool.clone()),
            AgentConfig::default().with_max_tool_iterations(2),
        )
        .await;

        let err = agent
            .handle(&Utterance::new("1", "放晴天", 0))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("after 2 rounds"), "{err}");

        // Two rounds of tools, then one more model call that is not acted on.
        assert_eq!(tool.calls.lock().unwrap().len(), 2);
        assert_eq!(mock.requests().len(), 3);

        // And the loop itself survives the failed turn.
        agent.run(&mut Script::new(&["放晴天"])).await.unwrap();
        assert!(agent.speaker().said.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn history_carries_context_into_the_next_turn_and_stays_bounded() {
        let mock = MockApi::new(vec![text_reply("好")]);
        let mut agent = agent(
            &mock,
            ToolRegistry::new(),
            AgentConfig::default().with_max_history_messages(2),
        )
        .await;

        agent
            .run(&mut Script::new(&["第一句", "第二句", "第三句"]))
            .await
            .unwrap();

        let requests = mock.requests();
        // Turn two sees turn one.
        let second: Vec<_> = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(second.contains(&"第一句".to_string()));
        assert!(second.contains(&"第二句".to_string()));

        // Turn three has dropped turn one: system + one pair + the new user
        // message.
        let third = requests[2]["messages"].as_array().unwrap();
        assert_eq!(third.len(), 4);
        assert_eq!(third[0]["role"], "system");
        assert_eq!(third[1]["content"], "第二句");
        assert_eq!(third[3]["content"], "第三句");
    }

    #[tokio::test]
    async fn an_empty_reply_is_not_spoken() {
        let mock = MockApi::new(vec![text_reply("   ")]);
        let mut agent = agent(&mock, ToolRegistry::new(), AgentConfig::default()).await;

        agent.run(&mut Script::new(&["放歌"])).await.unwrap();

        assert!(agent.speaker().said.lock().unwrap().is_empty());
    }

    /// The wiring U8 will actually write: one device, held by the agent and by
    /// a tool at the same time.
    #[tokio::test]
    async fn a_shared_speaker_satisfies_the_agent_type_parameter() {
        let device = FakeSpeaker::default();
        let shared: Arc<dyn Speaker> = Arc::new(device.clone());
        let boxed: Box<dyn Speaker> = Box::new(device.clone());

        shared.say("through the arc").await.unwrap();
        boxed.say("through the box").await.unwrap();

        let mock = MockApi::new(vec![text_reply("好")]);
        let base = mock.serve().await;
        let client = LlmClient::with_config(ClientConfig::new("k").with_api_base(base));
        let mut agent = Agent::new(client, ToolRegistry::new(), shared);
        agent.run(&mut Script::new(&["你好"])).await.unwrap();

        assert_eq!(
            device.said.lock().unwrap().as_slice(),
            ["through the arc", "through the box", "好"]
        );
    }

    #[tokio::test]
    async fn the_default_system_prompt_leads_every_request() {
        let mock = MockApi::new(vec![text_reply("好")]);
        let mut agent = agent(&mock, ToolRegistry::new(), AgentConfig::default()).await;
        agent.run(&mut Script::new(&["你好"])).await.unwrap();

        let messages = mock.requests()[0]["messages"].as_array().unwrap().clone();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], DEFAULT_SYSTEM_PROMPT);
    }
}
