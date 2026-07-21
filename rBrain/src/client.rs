//! A thin chat-completions client for DeepSeek (and any OpenAI-compatible
//! endpoint), wrapping `async-openai` in four small owned types so the provider
//! crate never leaks into the rest of `brain`.
//!
//! Default model [`DEFAULT_MODEL`] (`deepseek-v4-flash`). `deepseek-chat` and
//! `deepseek-reasoner` were retired on 2026-07-24 and must not reappear. The API
//! key is a constructor argument, never an environment read.

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

/// The model used unless overridden. Not `deepseek-chat`/`deepseek-reasoner`,
/// both retired on 2026-07-24.
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

/// One function call the model asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id, echoed back on the matching tool result message.
    pub id: String,
    /// Which tool to run.
    pub name: String,
    /// The arguments, as the raw JSON string the model emitted. Left unparsed so
    /// that DeepSeek's frequent truncated JSON becomes a recoverable tool error
    /// rather than a failed response deserialisation.
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
    /// The model's prose, if any. `None`/empty alongside non-empty `tool_calls`
    /// is the normal shape of a tool-calling turn.
    pub content: Option<String>,
    /// Tools the model wants run before it answers. Empty means the turn is done.
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

/// How to reach the model. `api_base` is overridable so tests can point at a
/// local mock and a deployment can sit behind a proxy or another vendor.
///
/// Build with [`ClientConfig::new`] for the DeepSeek default, or
/// `ClientConfig::builder()` to override fields — `.api_base(…)`, `.model(…)`,
/// `.temperature(…)`, `.max_tokens(…)`, each optional.
#[derive(Clone, bon::Builder)]
pub struct ClientConfig {
    #[builder(into)]
    pub api_key: String,
    #[builder(into, default = DEFAULT_API_BASE.to_owned())]
    pub api_base: String,
    #[builder(into, default = DEFAULT_MODEL.to_owned())]
    pub model: String,
    /// `None` leaves the provider default.
    pub temperature: Option<f32>,
    /// `None` leaves the provider default.
    pub max_tokens: Option<u32>,
}

impl ClientConfig {
    /// Defaults pointing at DeepSeek with [`DEFAULT_MODEL`].
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::builder().api_key(api_key).build()
    }
}

impl std::fmt::Debug for ClientConfig {
    /// Hand-written so the API key cannot reach a log line.
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

/// A chat-completions client. Cheap to clone and safe to share between tasks.
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
        // Every field `OpenAIConfig::new()` seeds from OPENAI_* env vars is
        // overwritten (org/project ids too, which DeepSeek does not expect), so a
        // stray OPENAI_API_KEY cannot change this client's behaviour.
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

    /// One completion round trip. `tools` is the OpenAI `tools` array; an empty
    /// slice omits the key entirely (some servers reject `[]`).
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<ChatResponse> {
        // DeepSeek accepts only `max_tokens`, not OpenAI's newer
        // `max_completion_tokens`.
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

        // A proxy or an error page dressed up as JSON can return no choices.
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
                // A free-form-text variant we never advertise; drop it rather
                // than abort the turn.
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
                // Some servers reject `tool_calls: []`, so omit it when empty.
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

/// Turn one `{"type":"function","function":{…}}` value into `async-openai`'s
/// typed form, keeping the provider crate out of the tool vocabulary.
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
            ClientConfig::builder()
                .api_key("k")
                .api_base("http://127.0.0.1:9/v1")
                .model("mock")
                .build(),
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
