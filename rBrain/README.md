# brain

A hardware-agnostic intent framework for voice assistants, in Rust.

`brain` is the seam between "an LLM deciding what to do" and "a device doing
it". It holds the contract — three traits and the values they exchange — plus the
concrete pieces on top: an LLM client, an MCP client for reaching tools, and the
control loop. It holds no implementation of either side.

## The decoupling contract

**`brain` must not depend on `xiaoai`, `netease`, `xiaoai_llm`, or any other
crate tied to a particular device or content provider.** The point of the
framework is that the same intent layer can drive a XiaoAi speaker today, a
microphone and a sound card tomorrow, and a stub in a test — so nothing here may
know which it has.

Concretely:

- Dependencies stay at `serde`, `serde_json`, `thiserror`, `async-trait`,
  `tracing`, `async-openai` (being an LLM client *is* this crate's job) and
  `rmcp` with only the `client` feature (reaching tools over MCP is too); an
  audio library, a device SDK or a content API here would be a bug.
- Errors are `BrainErr`, whose variants carry strings rather than foreign error
  types. Implementations bridge with `BrainErr::backend`.
- Identifiers only one side understands (`Track::id`) are opaque strings, handed
  back to their origin unread.

## The pieces

| Trait | Role | Typical implementation |
| --- | --- | --- |
| `UtteranceSource` | input | polls the speaker's conversation history |
| `Speaker` | output | the speaker's remote-control API |
| `MusicSource` | content | a local library, or NetEase |

They use `async_trait` rather than native async-in-trait: all are used as trait
objects, and native AFIT is not object-safe. The boxed future per call is
irrelevant next to the network round trips they wrap. Capabilities the model can
call are *not* a trait here — they come from an MCP tool server the caller
connects to (see below).

On top of them:

- **`LlmClient`** — chat completions against an OpenAI-compatible endpoint,
  defaulting to DeepSeek (`https://api.deepseek.com`) and the model
  **`deepseek-v4-flash`**. `deepseek-chat` and `deepseek-reasoner` were retired
  on 2026-07-24 and must not reappear anywhere. The API base is overridable,
  which is how the tests run against a local mock. The client reads no
  environment variables — the binary passes the key in.
- **MCP tool client** — `Agent::connect` connects to an MCP tool server over a
  transport; its `list_all_tools` (sorted by name, so the prompt is reproducible)
  becomes the OpenAI `tools` array, and each tool call becomes an MCP `call_tool`.
- **`Agent`** — the loop: model call → tool calls → tool results → repeat, up to
  `max_tool_iterations` (5 by default), then speak the answer. It keeps a
  bounded history of user/assistant *text* pairs; tool messages are deliberately
  not retained, since an assistant message carrying `tool_calls` is invalid
  without its replies. A failed turn is logged and skipped, never fatal.

## Adding a capability

Tools live in a **caller-provided MCP server**, defined with `rmcp`'s `#[tool]`
macros; `brain` connects to it as an MCP client. Adding a capability is one more
`#[tool]` method — nothing in `brain` dispatches on tool names.

```rust ignore
use rmcp::{ServerHandler, ServiceExt, handler::server::wrapper::Parameters,
           tool, tool_handler, tool_router};
use schemars::JsonSchema;

#[derive(serde::Deserialize, JsonSchema)]
struct VolumeArgs {
    /// Target volume, 0 to 100.
    level: u8,
}

#[derive(Clone)]
struct Assistant;

#[tool_router]
impl Assistant {
    // The description and the field doc-comments are written for the *model* —
    // they become the tool's schema.
    #[tool(description = "Set the speaker volume, louder or quieter.")]
    async fn set_volume(&self, Parameters(args): Parameters<VolumeArgs>) -> String {
        format!("volume set to {}", args.level)
    }
}

#[tool_handler]
impl ServerHandler for Assistant {}

// Run the tools as an in-memory MCP server and connect the agent to it. The
// server's `serve` awaits the client's `initialize`, so it must run concurrently.
let (server_t, client_t) = tokio::io::duplex(64 * 1024);
tokio::spawn(async move {
    if let Ok(server) = Assistant.serve(server_t).await {
        let _ = server.waiting().await;
    }
});

let client = brain::LlmClient::new(std::env::var("DEEPSEEK_API_KEY")?);
brain::Agent::connect(client, client_t, speaker).await?.run(&mut source).await
```

A tool call's `arguments` arrive from the model as a raw JSON *string*, parsed
into the MCP `call_tool` params by `brain`: DeepSeek does not always close its
braces, so a malformed call comes back as a tool-error message it can recover
from rather than sinking the turn.

## Tests

```sh
cargo test -p brain
```

Offline and credential-free: the completions endpoint is mocked with a local
`axum` server, and the real DeepSeek API is never called from a test.
