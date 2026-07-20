//! A chat-completions client for DeepSeek (and any other OpenAI-compatible
//! endpoint).
//!
//! # Why a wrapper rather than `async-openai` directly
//!
//! [`async_openai`] models the whole of OpenAI's surface, with a type per
//! message variant, per content variant and per tool variant. The control loop
//! needs about a tenth of that, and — more importantly — units that register
//! tools or implement devices should not have to depend on `async-openai`'s
//! exact version to talk to this crate. So the public vocabulary here is four
//! small owned types ([`ChatMessage`], [`ToolCall`], [`ChatResponse`],
//! [`ClientConfig`]) and the provider crate stays an implementation detail.
//!
//! # DeepSeek specifics
//!
//! - The API base is [`DEFAULT_API_BASE`] and the model [`DEFAULT_MODEL`]
//!   (`deepseek-v4-flash`: 1M context, 384K max output). `deepseek-chat` and
//!   `deepseek-reasoner` were **deprecated on 2026-07-24**; if either appears
//!   anywhere in this repo it is stale.
//! - Function calling is byte-compatible with OpenAI's: a `tools` array of
//!   `{"type":"function","function":{name, description, parameters}}` in, and
//!   `choices[0].message.tool_calls` with `finish_reason: "tool_calls"` out.
//!   Each call's `arguments` is a **JSON string**, not an object, and DeepSeek
//!   is measurably worse than OpenAI at making it parse — which is why
//!   [`ToolCall::arguments`] stays a `String` here and is parsed (fallibly) by
//!   the caller rather than being silently `unwrap`ped in the middle of a
//!   deserialisation.
//!
//! The API key is a constructor argument, never an environment read: `brain` is
//! a library and the process that owns it decides where secrets come from.

use crate::error::{BrainErr, Result};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionTool, ChatCompletionTools,
    CreateChatCompletionRequest, FunctionCall, FunctionObject,
};
use serde::{Deserialize, Serialize};

/// DeepSeek's OpenAI-compatible endpoint root.
pub const DEFAULT_API_BASE: &str = "https://api.deepseek.com";

/// The model used unless overridden: 1M context, 384K max output.
///
/// Deliberately *not* `deepseek-chat` or `deepseek-reasoner`, both retired on
/// 2026-07-24.
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

/// One function call the model asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id. Must be echoed back on the matching tool result
    /// message or the model cannot pair them up.
    pub id: String,
    /// Which [`crate::Tool`] to run.
    pub name: String,
    /// The arguments, **as the raw JSON string the model emitted**.
    ///
    /// Left unparsed on purpose: a model that emits `{"query": "晴天"` (note the
    /// missing brace) should produce a tool-error message the model can recover
    /// from, not a failed response deserialisation that loses the whole turn.
    pub arguments: String,
}

/// One message in a conversation, in the shapes this crate actually uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatMessage {
    /// The standing instructions. Exactly one, first, by convention.
    System(String),
    /// Something the user said.
    User(String),
    /// What the model replied: prose, tool calls, or both.
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    /// The result of running one [`ToolCall`], keyed back to it by id.
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl ChatMessage {
    /// Convenience constructors, mostly for tests and callers assembling
    /// history.
    pub fn system(content: impl Into<String>) -> Self {
        Self::System(content.into())
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User(content.into())
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: Some(content.into()),
            tool_calls: Vec::new(),
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        }
    }
}

/// What came back from one completion request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatResponse {
    /// The model's prose, when it produced any. `None` (or empty) alongside a
    /// non-empty [`ChatResponse::tool_calls`] is the normal shape of a
    /// tool-calling turn.
    pub content: Option<String>,
    /// Tools the model wants run before it will answer. Empty means the turn is
    /// finished — the common case for "tell me a story", which needs no tools at
    /// all.
    pub tool_calls: Vec<ToolCall>,
}

impl ChatResponse {
    /// Whether the model is waiting on tool results.
    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// The prose, or `""` — saves callers an `unwrap_or_default` dance.
    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or_default()
    }
}

/// How to reach the model.
///
/// `api_base` is overridable for two reasons that matter: tests point it at a
/// local mock (this crate never calls the real API from a test), and a
/// deployment may sit behind a proxy or a different OpenAI-compatible vendor.
#[derive(Clone)]
pub struct ClientConfig {
    pub api_key: String,
    pub api_base: String,
    pub model: String,
    /// `None` leaves the provider default. Low values suit an assistant that
    /// should follow instructions rather than free-associate.
    pub temperature: Option<f32>,
    /// Cap on generated tokens per turn. `None` leaves the provider default —
    /// which for `deepseek-v4-flash` is far more than a spoken reply needs.
    pub max_tokens: Option<u32>,
}

impl ClientConfig {
    /// Defaults pointing at DeepSeek with [`DEFAULT_MODEL`].
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            api_base: DEFAULT_API_BASE.to_string(),
            model: DEFAULT_MODEL.to_string(),
            temperature: None,
            max_tokens: None,
        }
    }

    /// Point at a different OpenAI-compatible host — a mock, a proxy, a
    /// self-hosted model.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
}

impl std::fmt::Debug for ClientConfig {
    /// Hand-written so the API key cannot reach a log line. A derived `Debug` on
    /// a struct holding a secret is a leak waiting for the first
    /// `tracing::debug!(?config)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("api_key", &"<redacted>")
            .field("api_base", &self.api_base)
            .field("model", &self.model)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

/// A chat-completions client.
///
/// Cheap to clone (the underlying HTTP client is refcounted), `Send + Sync`, and
/// safe to share between tasks.
#[derive(Debug, Clone)]
pub struct LlmClient {
    inner: Client<OpenAIConfig>,
    model: String,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
}

impl LlmClient {
    /// Talk to DeepSeek with the default model.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_config(ClientConfig::new(api_key))
    }

    /// Talk to whatever [`ClientConfig`] describes.
    pub fn with_config(config: ClientConfig) -> Self {
        // `OpenAIConfig::new()` seeds itself from OPENAI_* environment
        // variables. Every field it can pick up is overwritten here — including
        // the org and project ids, which would otherwise be sent as headers
        // DeepSeek does not expect — so that a stray OPENAI_API_KEY in the
        // environment cannot change this client's behaviour. `brain` reads no
        // environment of its own.
        let openai = OpenAIConfig::new()
            .with_api_base(config.api_base)
            .with_api_key(config.api_key)
            .with_org_id("")
            .with_project_id("");

        Self {
            inner: Client::with_config(openai),
            model: config.model,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
        }
    }

    /// The model id every request is sent with.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// One completion round trip.
    ///
    /// `tools` is the OpenAI `tools` array, normally
    /// [`crate::ToolRegistry::schemas`]; pass an empty slice to forbid tool use
    /// for this call. The `tools` key is then omitted entirely rather than sent
    /// as `[]`, which some OpenAI-compatible servers reject.
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<ChatResponse> {
        // `max_tokens` is deprecated *by OpenAI* in favour of
        // `max_completion_tokens`, but DeepSeek documents and accepts only
        // `max_tokens`. Following the deprecation would silently stop capping
        // output on the provider we actually target.
        #[allow(deprecated)]
        let request = CreateChatCompletionRequest {
            model: self.model.clone(),
            messages: messages.iter().map(to_openai_message).collect(),
            tools: if tools.is_empty() {
                None
            } else {
                Some(
                    tools
                        .iter()
                        .map(to_openai_tool)
                        .collect::<Result<Vec<_>>>()?,
                )
            },
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            ..Default::default()
        };

        tracing::debug!(
            model = %self.model,
            messages = messages.len(),
            tools = tools.len(),
            "requesting completion"
        );

        let response = self
            .inner
            .chat()
            .create(request)
            .await
            .map_err(BrainErr::backend)?;

        // An empty `choices` is not something a well-behaved server produces,
        // but a proxy or an error page dressed up as JSON can, and indexing
        // would panic in the middle of the control loop.
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| BrainErr::Backend("model returned no choices".into()))?;

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .filter_map(|call| match call {
                ChatCompletionMessageToolCalls::Function(f) => Some(ToolCall {
                    id: f.id,
                    name: f.function.name,
                    arguments: f.function.arguments,
                }),
                // "Custom" tools are a free-form-text variant this crate never
                // advertises, so receiving one means the server is confused.
                // Dropping it is better than aborting the turn: the model still
                // gets to answer with whatever else it produced.
                ChatCompletionMessageToolCalls::Custom(c) => {
                    tracing::warn!(id = %c.id, "ignoring custom (non-function) tool call");
                    None
                }
            })
            .collect();

        Ok(ChatResponse {
            content: choice.message.content,
            tool_calls,
        })
    }
}

/// Translate one of our messages into `async-openai`'s representation.
fn to_openai_message(message: &ChatMessage) -> ChatCompletionRequestMessage {
    match message {
        ChatMessage::System(content) => {
            ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                content: ChatCompletionRequestSystemMessageContent::Text(content.clone()),
                name: None,
            })
        }
        ChatMessage::User(content) => {
            ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(content.clone()),
                name: None,
            })
        }
        ChatMessage::Assistant {
            content,
            tool_calls,
        } => {
            let calls: Vec<_> = tool_calls
                .iter()
                .map(|call| {
                    ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                        id: call.id.clone(),
                        function: FunctionCall {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        },
                    })
                })
                .collect();

            ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                content: content
                    .clone()
                    .map(ChatCompletionRequestAssistantMessageContent::Text),
                // Sending `tool_calls: []` rather than omitting it makes some
                // servers treat the message as malformed, so an empty list
                // becomes `None`.
                tool_calls: (!calls.is_empty()).then_some(calls),
                ..Default::default()
            })
        }
        ChatMessage::Tool {
            tool_call_id,
            content,
        } => ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
            content: ChatCompletionRequestToolMessageContent::Text(content.clone()),
            tool_call_id: tool_call_id.clone(),
        }),
    }
}

/// Turn one `{"type":"function","function":{…}}` value from the registry into
/// `async-openai`'s typed form.
///
/// Going through JSON rather than building the typed value in the registry is
/// deliberate: it keeps `async-openai` out of [`crate::Tool`]'s vocabulary, so a
/// tool author writes plain [`serde_json::Value`] schemas and never sees the
/// provider crate.
fn to_openai_tool(tool: &serde_json::Value) -> Result<ChatCompletionTools> {
    let function = tool.get("function").ok_or_else(|| {
        BrainErr::InvalidArguments("tool schema is missing a `function` object".into())
    })?;

    let name = function
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| BrainErr::InvalidArguments("tool schema is missing `function.name`".into()))?
        .to_string();

    Ok(ChatCompletionTools::Function(ChatCompletionTool {
        function: FunctionObject {
            name,
            description: function
                .get("description")
                .and_then(|d| d.as_str())
                .map(str::to_string),
            parameters: function.get("parameters").cloned(),
            strict: None,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_point_at_deepseek_and_a_current_model() {
        let config = ClientConfig::new("k");
        assert_eq!(config.api_base, "https://api.deepseek.com");
        assert_eq!(config.model, "deepseek-v4-flash");
        // The key must never be printable.
        assert!(format!("{:?}", ClientConfig::new("sk-secret")).contains("<redacted>"));
        assert!(!format!("{:?}", ClientConfig::new("sk-secret")).contains("sk-secret"));
        // Guard against a copy-paste resurrection of the retired models.
        assert_ne!(config.model, "deepseek-chat");
        assert_ne!(config.model, "deepseek-reasoner");
    }

    #[test]
    fn the_base_url_is_overridable() {
        let client = LlmClient::with_config(
            ClientConfig::new("k")
                .with_api_base("http://127.0.0.1:9/v1")
                .with_model("mock"),
        );
        assert_eq!(client.model(), "mock");
    }

    /// The wire shape is the contract with the provider, so pin it rather than
    /// trusting `async-openai`'s field names to stay put.
    #[test]
    fn messages_serialise_to_the_openai_wire_shape() {
        let messages = [
            ChatMessage::system("你是小助手"),
            ChatMessage::user("放首歌"),
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "play_music".into(),
                    arguments: r#"{"query":"晴天"}"#.into(),
                }],
            },
            ChatMessage::tool("call_1", "playing 晴天"),
        ];

        let wire: Vec<_> = messages
            .iter()
            .map(|m| serde_json::to_value(to_openai_message(m)).unwrap())
            .collect();

        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[0]["content"], "你是小助手");
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[2]["role"], "assistant");
        assert_eq!(wire[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(wire[2]["tool_calls"][0]["type"], "function");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "play_music");
        // `arguments` is a JSON *string*, not an object.
        assert_eq!(
            wire[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"query":"晴天"}"#
        );
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "call_1");
    }

    #[test]
    fn an_assistant_message_without_calls_omits_tool_calls() {
        let wire =
            serde_json::to_value(to_openai_message(&ChatMessage::assistant("好的"))).unwrap();
        assert_eq!(wire["role"], "assistant");
        assert!(wire.get("tool_calls").is_none());
    }

    #[test]
    fn tool_schemas_round_trip_into_the_typed_form() {
        let schema = json!({
            "type": "function",
            "function": {
                "name": "play_music",
                "description": "Play a song.",
                "parameters": { "type": "object", "properties": {} }
            }
        });

        let wire = serde_json::to_value(to_openai_tool(&schema).unwrap()).unwrap();
        assert_eq!(wire["type"], "function");
        assert_eq!(wire["function"]["name"], "play_music");
        assert_eq!(wire["function"]["description"], "Play a song.");
        assert_eq!(wire["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn a_malformed_tool_schema_is_an_error_not_a_panic() {
        assert!(to_openai_tool(&json!({"type": "function"})).is_err());
        assert!(to_openai_tool(&json!({"function": {"description": "no name"}})).is_err());
    }
}
