//! The control loop: utterances in, tool calls and speech out.
//!
//! Device-agnostic — it knows only [`UtteranceSource`], [`Speaker`] and an MCP
//! tool client, never which hardware is behind them. One utterance is one turn:
//! send `system + history + user` with the tool schemas, speak a prose answer, or
//! run the tool calls and ask again, up to [`AgentConfig::max_tool_iterations`].
//!
//! Failure is routine, not fatal: malformed tool arguments (DeepSeek emits
//! `arguments` as a JSON string and does not always close its braces) and tool
//! errors become tool-result messages the model recovers from, and a dead turn
//! is logged and skipped.

use crate::client::{ChatMessage, LlmClient, ToolCall};
use crate::error::{BrainErr, Result};
use crate::traits::{Speaker, Utterance, UtteranceSource};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::IntoTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Map, Value};
use std::sync::Arc;

pub const DEFAULT_SYSTEM_PROMPT: &str = "\
你是一台智能音箱的助手。用户通过语音和你交流，你的回答会被朗读出来。

规则：
- 用简体中文回答，语气自然口语化，除非用户要求，否则不要超过三句话。
- 回答里不要出现 Markdown、表情符号、编号列表或任何朗读时会很奇怪的排版。
- 需要播放音乐、控制音量等操作时，调用相应的工具，不要假装已经做了。
- 聊天、辩论这类请求直接回答即可，不需要调用任何工具。
- 工具返回错误时，把原因用一句话告诉用户，或者换个参数重试一次。";

/// Forwarding impls so a shared or boxed speaker satisfies `S: Speaker`, which is
/// what lets the agent and a "set the volume" tool hold the same `Arc` device.
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

/// Knobs on the loop's behaviour. `AgentConfig::default()` is every default;
/// `AgentConfig::builder()` overrides individual fields.
#[derive(Debug, Clone, bon::Builder)]
pub struct AgentConfig {
    #[builder(into, default = DEFAULT_SYSTEM_PROMPT.to_owned())]
    pub system_prompt: String,
    /// Rounds of tool execution one utterance may take before the turn is
    /// abandoned. The deepest real chain is search → resolve → play.
    #[builder(default = 5)]
    pub max_tool_iterations: usize,
    /// Past messages carried into the next turn so "再来一首" resolves. Counted in
    /// messages, trimmed from the oldest end; bounded because a speaker runs for
    /// weeks.
    #[builder(default = 12)]
    pub max_history_messages: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// The assembled assistant: a model, an MCP tool client, and something to speak
/// through.
///
/// Generic over the [`Speaker`] so a caller keeps its concrete type; the same
/// speaker is usually also held by a tool, so the common shape is one `Arc`
/// cloned into both.
pub struct Agent<S: Speaker> {
    client: LlmClient,
    mcp: RunningService<RoleClient, ()>,
    /// The tool schemas, in OpenAI `tools`-array shape, listed once at connect.
    tools: Vec<Value>,
    speaker: S,
    config: AgentConfig,
    /// User/assistant pairs only. Assistant messages carrying tool calls are not
    /// kept: they are valid only next to their `role: "tool"` replies, and a trim
    /// between the two would produce a request the API rejects.
    history: Vec<ChatMessage>,
}

impl<S: Speaker> Agent<S> {
    /// Connect to an MCP tool server over `transport` with [`AgentConfig::default`].
    pub async fn connect<T, E, A>(client: LlmClient, transport: T, speaker: S) -> Result<Self>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::connect_with_config(client, transport, speaker, AgentConfig::default()).await
    }

    pub async fn connect_with_config<T, E, A>(
        client: LlmClient,
        transport: T,
        speaker: S,
        config: AgentConfig,
    ) -> Result<Self>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let mcp = ().serve(transport).await.map_err(BrainErr::backend)?;
        let tools = tool_schemas(&mcp).await?;
        Ok(Self {
            client,
            mcp,
            tools,
            speaker,
            config,
            history: Vec::new(),
        })
    }

    /// The speaker, for callers that also want to drive it directly.
    pub fn speaker(&self) -> &S {
        &self.speaker
    }

    /// Consume utterances until the source is exhausted. A failed turn is logged
    /// and skipped, so the only way out is exhaustion.
    pub async fn run<E: UtteranceSource>(&mut self, source: &mut E) -> Result<()> {
        while let Some(utterance) = source.next().await {
            tracing::info!(id = %utterance.id, text = %utterance.text, "handling utterance");

            match self.handle(&utterance).await {
                Ok(reply) => {
                    if reply.trim().is_empty() {
                        // A tool did the work; saying "" would make a speaker click.
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
    pub async fn handle(&mut self, utterance: &Utterance) -> Result<String> {
        let mut messages = Vec::with_capacity(self.history.len() + 2);
        messages.push(ChatMessage::system(&self.config.system_prompt));
        messages.extend(self.history.iter().cloned());
        messages.push(ChatMessage::user(&utterance.text));

        // One more model call than `max_tool_iterations`: the last is the model's
        // chance to answer from what the tools told it.
        for round in 0..=self.config.max_tool_iterations {
            let response = self.client.chat(&messages, &self.tools).await?;

            if !response.wants_tools() {
                let reply = response.content.unwrap_or_default();
                self.remember(&utterance.text, &reply);
                return Ok(reply);
            }

            if round == self.config.max_tool_iterations {
                // No budget left to send these results, so running them would be
                // wasted and, for a tool with side effects, wrong.
                return Err(BrainErr::Backend(format!(
                    "model still requesting tools after {} rounds; giving up on this turn",
                    self.config.max_tool_iterations
                )));
            }

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
    /// Never `Err`: every failure here is information the model should get.
    async fn run_tool_call(&self, call: &ToolCall) -> String {
        let arguments = match parse_arguments(&call.arguments) {
            Ok(arguments) => arguments,
            Err(message) => {
                tracing::warn!(tool = %call.name, arguments = %call.arguments, "unparseable tool arguments");
                return format!("error: {message}");
            }
        };

        tracing::info!(tool = %call.name, "dispatching tool call");
        let mut params = CallToolRequestParams::new(call.name.clone());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }

        match self.mcp.call_tool(params).await {
            Ok(result) => render_result(result),
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

        // Round the limit down to even: history is whole user/assistant pairs, and
        // a window starting on an orphaned assistant reply reads as unprompted.
        let keep = self.config.max_history_messages & !1;
        let excess = self.history.len().saturating_sub(keep);
        if excess > 0 {
            self.history.drain(..excess);
        }
    }
}

/// The tool list, in OpenAI `tools`-array shape. `list_all_tools` returns tools
/// sorted by name, so the prompt stays reproducible.
async fn tool_schemas(mcp: &RunningService<RoleClient, ()>) -> Result<Vec<Value>> {
    let tools = mcp.list_all_tools().await.map_err(BrainErr::backend)?;
    Ok(tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect())
}

/// The text of a tool result. An error result is prefixed `error:` so the model
/// treats a genuine failure differently from an ordinary "no such song" answer.
fn render_result(result: CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if result.is_error.unwrap_or(false) {
        format!("error: {text}")
    } else {
        text
    }
}

/// Turn the model's `arguments` string into an object, or say why it cannot be.
/// Empty, `null` and whitespace all mean "no arguments" — what a model emits for
/// a zero-parameter tool.
fn parse_arguments(arguments: &str) -> std::result::Result<Option<Map<String, Value>>, String> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let value: Value = serde_json::from_str(trimmed).map_err(|e| {
        format!("tool arguments were not valid JSON ({e}); re-send them as a JSON object")
    })?;

    match value {
        Value::Object(map) => Ok(Some(map)),
        Value::Null => Ok(None),
        other => Err(format!(
            "tool arguments must be a JSON object, got {other}; re-send them as a JSON object"
        )),
    }
}

/// Connect an [`Agent`] and run it to exhaustion — the one-line entry point.
pub async fn run<E, S, T, TE, A>(
    client: LlmClient,
    transport: T,
    speaker: S,
    source: &mut E,
    config: AgentConfig,
) -> Result<()>
where
    E: UtteranceSource,
    S: Speaker,
    T: IntoTransport<RoleClient, TE, A>,
    TE: std::error::Error + Send + Sync + 'static,
{
    Agent::connect_with_config(client, transport, speaker, config)
        .await?
        .run(source)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientConfig;
    use axum::{Json, Router, extract::State, routing::post};
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
    use schemars::JsonSchema;
    use serde_json::{Value, json};
    use std::sync::Mutex;

    // ------------------------------------------------------- DeepSeek mock

    /// Serves a queue of canned OpenAI-shaped responses and records every request
    /// body, so a test can assert on what the loop sent back. The real DeepSeek
    /// API is never contacted.
    #[derive(Clone, Default)]
    struct MockApi {
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
        // The last response repeats once exhausted, so a looping test need not
        // enqueue an infinite list.
        let response = if responses.len() > 1 {
            responses.remove(0)
        } else {
            responses.first().cloned().unwrap_or_else(|| text_reply(""))
        };
        Json(response)
    }

    fn text_reply(content: &str) -> Value {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 0, "model": "mock",
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
            "id": "chatcmpl-1", "object": "chat.completion", "created": 0, "model": "mock",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{
                        "id": id, "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    }

    // ------------------------------------------------------- MCP tool server

    #[derive(serde::Deserialize, JsonSchema)]
    struct PlayArgs {
        /// The song to play.
        query: String,
    }

    /// An MCP server with one recording `play_music` tool.
    struct ToolServer {
        calls: Arc<Mutex<Vec<Value>>>,
    }

    #[tool_router]
    impl ToolServer {
        fn new(calls: Arc<Mutex<Vec<Value>>>) -> Self {
            Self { calls }
        }

        #[tool(description = "Play a song.")]
        async fn play_music(&self, Parameters(args): Parameters<PlayArgs>) -> String {
            self.calls.lock().unwrap().push(json!({ "query": args.query }));
            format!("now playing {}", args.query)
        }
    }

    #[tool_handler]
    impl ServerHandler for ToolServer {}

    /// An MCP server with no tools.
    struct EmptyServer;

    #[tool_router]
    impl EmptyServer {
        fn new() -> Self {
            Self
        }
    }

    #[tool_handler]
    impl ServerHandler for EmptyServer {}

    async fn spawn_server(server: impl ServerHandler + 'static) -> tokio::io::DuplexStream {
        let (server_t, client_t) = tokio::io::duplex(4096);
        // The server's `serve` awaits the client's `initialize`, so it must run
        // concurrently with the client connecting — not be awaited first.
        tokio::spawn(async move {
            if let Ok(running) = server.serve(server_t).await {
                let _ = running.waiting().await;
            }
        });
        client_t
    }

    async fn agent_with(
        mock: &MockApi,
        server: impl ServerHandler + 'static,
        config: AgentConfig,
    ) -> Agent<FakeSpeaker> {
        let base = mock.serve().await;
        let client = LlmClient::with_config(
            ClientConfig::builder()
                .api_key("test-key")
                .api_base(base)
                .model("mock")
                .build(),
        );
        let transport = spawn_server(server).await;
        Agent::connect_with_config(client, transport, FakeSpeaker::default(), config)
            .await
            .unwrap()
    }

    // ------------------------------------------------------- other fakes

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

    /// The tool messages the loop sent back on one request.
    fn tool_messages(request: &Value) -> Vec<String> {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["content"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    // ------------------------------------------------------- tests

    #[tokio::test]
    async fn a_plain_text_reply_is_spoken() {
        let mock = MockApi::new(vec![text_reply("从前有座山")]);
        let mut agent = agent_with(&mock, EmptyServer::new(), AgentConfig::default()).await;

        agent.run(&mut Script::new(&["讲个故事"])).await.unwrap();

        assert_eq!(
            agent.speaker().said.lock().unwrap().as_slice(),
            ["从前有座山"]
        );
        // No tools means the `tools` key is omitted entirely.
        assert!(mock.requests()[0].get("tools").is_none());
    }

    #[tokio::test]
    async fn a_tool_call_is_dispatched_and_its_result_fed_back() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock = MockApi::new(vec![
            tool_call_reply("call_1", "play_music", r#"{"query":"晴天"}"#),
            text_reply("好的，播放晴天"),
        ]);
        let mut agent = agent_with(&mock, ToolServer::new(calls.clone()), AgentConfig::default()).await;

        agent.run(&mut Script::new(&["放晴天"])).await.unwrap();

        assert_eq!(calls.lock().unwrap().as_slice(), [json!({"query": "晴天"})]);
        assert_eq!(
            agent.speaker().said.lock().unwrap().as_slice(),
            ["好的，播放晴天"]
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["tools"][0]["function"]["name"], "play_music");
        assert_eq!(tool_messages(&requests[1]), ["now playing 晴天"]);
        // The assistant message carrying the call must be replayed.
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
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock = MockApi::new(vec![
            // Truncated JSON — what DeepSeek produces mid-object.
            tool_call_reply("call_1", "play_music", r#"{"query":"晴天"#),
            text_reply("抱歉，我没听清"),
        ]);
        let mut agent = agent_with(&mock, ToolServer::new(calls.clone()), AgentConfig::default()).await;

        agent.run(&mut Script::new(&["放晴天"])).await.unwrap();

        // The tool was never reached, and the model was told why.
        assert!(calls.lock().unwrap().is_empty());
        let messages = tool_messages(&mock.requests()[1]);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("error:"), "{}", messages[0]);
        assert!(messages[0].contains("not valid JSON"), "{}", messages[0]);
        assert_eq!(
            agent.speaker().said.lock().unwrap().as_slice(),
            ["抱歉，我没听清"]
        );
    }

    #[test]
    fn non_object_arguments_are_rejected_with_an_explanation() {
        assert!(parse_arguments("[1,2]").unwrap_err().contains("JSON object"));
        assert!(parse_arguments("\"晴天\"").unwrap_err().contains("JSON object"));
        // The shapes a model uses for "no arguments" all mean the same thing.
        assert_eq!(parse_arguments("").unwrap(), None);
        assert_eq!(parse_arguments("  ").unwrap(), None);
        assert_eq!(parse_arguments("null").unwrap(), None);
        assert_eq!(
            parse_arguments(r#"{"a":1}"#).unwrap().unwrap()["a"],
            json!(1)
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_name_errors_cleanly_back_to_the_model() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock = MockApi::new(vec![
            tool_call_reply("call_1", "launch_missiles", "{}"),
            text_reply("我做不到"),
        ]);
        let mut agent = agent_with(&mock, ToolServer::new(calls), AgentConfig::default()).await;

        agent.run(&mut Script::new(&["发射导弹"])).await.unwrap();

        // The bad call comes back as an error the model can recover from.
        let messages = tool_messages(&mock.requests()[1]);
        assert!(messages[0].starts_with("error:"), "{}", messages[0]);
        assert_eq!(
            agent.speaker().said.lock().unwrap().as_slice(),
            ["我做不到"]
        );
    }

    #[tokio::test]
    async fn the_iteration_cap_stops_a_model_that_loops() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        // The mock repeats its last response forever, so the model never stops.
        let mock = MockApi::new(vec![tool_call_reply(
            "call_1",
            "play_music",
            r#"{"query":"晴天"}"#,
        )]);
        let mut agent = agent_with(
            &mock,
            ToolServer::new(calls.clone()),
            AgentConfig::builder().max_tool_iterations(2).build(),
        )
        .await;

        let err = agent
            .handle(&Utterance::new("1", "放晴天", 0))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("after 2 rounds"), "{err}");

        // Two rounds of tools, then one more model call that is not acted on.
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(mock.requests().len(), 3);

        // The loop survives the failed turn.
        agent.run(&mut Script::new(&["放晴天"])).await.unwrap();
        assert!(agent.speaker().said.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn history_carries_context_into_the_next_turn_and_stays_bounded() {
        let mock = MockApi::new(vec![text_reply("好")]);
        let mut agent = agent_with(
            &mock,
            EmptyServer::new(),
            AgentConfig::builder().max_history_messages(2).build(),
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

        // Turn three has dropped turn one: system + one pair + the new user message.
        let third = requests[2]["messages"].as_array().unwrap();
        assert_eq!(third.len(), 4);
        assert_eq!(third[0]["role"], "system");
        assert_eq!(third[1]["content"], "第二句");
        assert_eq!(third[3]["content"], "第三句");
    }

    #[tokio::test]
    async fn an_empty_reply_is_not_spoken() {
        let mock = MockApi::new(vec![text_reply("   ")]);
        let mut agent = agent_with(&mock, EmptyServer::new(), AgentConfig::default()).await;

        agent.run(&mut Script::new(&["放歌"])).await.unwrap();

        assert!(agent.speaker().said.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_shared_speaker_satisfies_the_agent_type_parameter() {
        let device = FakeSpeaker::default();
        let shared: Arc<dyn Speaker> = Arc::new(device.clone());

        let mock = MockApi::new(vec![text_reply("好")]);
        let base = mock.serve().await;
        let client = LlmClient::with_config(
            ClientConfig::builder().api_key("k").api_base(base).model("mock").build(),
        );
        let transport = spawn_server(EmptyServer::new()).await;
        let mut agent = Agent::connect(client, transport, shared).await.unwrap();
        agent.run(&mut Script::new(&["你好"])).await.unwrap();

        assert_eq!(device.said.lock().unwrap().as_slice(), ["好"]);
    }

    #[tokio::test]
    async fn the_default_system_prompt_leads_every_request() {
        let mock = MockApi::new(vec![text_reply("好")]);
        let mut agent = agent_with(&mock, EmptyServer::new(), AgentConfig::default()).await;
        agent.run(&mut Script::new(&["你好"])).await.unwrap();

        let messages = mock.requests()[0]["messages"].as_array().unwrap().clone();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], DEFAULT_SYSTEM_PROMPT);
    }
}
