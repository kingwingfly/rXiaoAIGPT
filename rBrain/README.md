# brain

A hardware-agnostic intent framework for voice assistants, in Rust.

`brain` is the seam between "an LLM deciding what to do" and "a device doing
it". It holds the contract — four traits and the values they exchange — plus the
three concrete pieces that sit on top: an LLM client, a tool registry, and the
control loop. It holds no implementation of either side.

## The decoupling contract

**`brain` must not depend on `xiaoai`, `netease`, `xiaoai_llm`, or any other
crate tied to a particular device or content provider.** The point of the
framework is that the same intent layer can drive a XiaoAi speaker today, a
microphone and a sound card tomorrow, and a stub in a test — so nothing here may
know which it has.

Concretely:

- Dependencies stay at `serde`, `serde_json`, `thiserror`, `async-trait`,
  `tracing` and `async-openai`. The last earns its place because being an LLM
  client *is* this crate's job; an audio library, a device SDK or a content API
  here would be a bug.
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
| `Tool` | capability | one function the model can call |

All four use `async_trait` rather than native async-in-trait: all four are used
as trait objects, and native AFIT is not object-safe. The boxed future per call
is irrelevant next to the network round trips they wrap.

On top of them:

- **`LlmClient`** — chat completions against an OpenAI-compatible endpoint,
  defaulting to DeepSeek (`https://api.deepseek.com`) and the model
  **`deepseek-v4-flash`**. `deepseek-chat` and `deepseek-reasoner` were retired
  on 2026-07-24 and must not reappear anywhere. The API base is overridable,
  which is how the tests run against a local mock. The client reads no
  environment variables — the binary passes the key in.
- **`ToolRegistry`** — a sorted map of tools, so the tool list in the prompt is
  reproducible. `schemas()` emits the OpenAI `tools` array; `dispatch()` routes
  by name, and an unknown name is a `NotFound` listing the real ones rather than
  a panic.
- **`Agent`** — the loop: model call → tool calls → tool results → repeat, up to
  `max_tool_iterations` (5 by default), then speak the answer. It keeps a
  bounded history of user/assistant *text* pairs; tool messages are deliberately
  not retained, since an assistant message carrying `tool_calls` is invalid
  without its replies. A failed turn is logged and skipped, never fatal.

## Adding a capability

Two steps, neither of which touches the loop. Nothing dispatches on tool names,
so a new capability is purely additive.

```rust ignore
use brain::{Agent, LlmClient, Result, ToolRegistry, Tool};
use serde_json::{Value, json};

struct Volume;

#[brain::async_trait]
impl Tool for Volume {
    fn name(&self) -> &str { "set_volume" }

    // Written for the *model*, not for a developer: this is the only thing
    // telling it when this tool is the right one.
    fn description(&self) -> &str {
        "Set the speaker volume. Use when the user asks for it louder or quieter."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "level": { "type": "integer", "minimum": 0, "maximum": 100 } },
            "required": ["level"]
        })
    }

    async fn call(&self, args: Value) -> Result<String> {
        let level = args["level"].as_u64().unwrap_or(50);
        Ok(format!("volume set to {level}"))
    }
}

let client = LlmClient::new(std::env::var("DEEPSEEK_API_KEY")?);
let registry = ToolRegistry::new().with(Volume);
Agent::new(client, registry, speaker).run(&mut source).await
```

A `ToolCall`'s `arguments` arrive from the model as a raw JSON *string*, left
unparsed on purpose: DeepSeek does not always close its braces, and a malformed
call should come back to the model as a tool-error message it can recover from,
not sink the whole turn.

## Tests

```sh
cargo test -p brain
```

Offline and credential-free: the completions endpoint is mocked with a local
`axum` server, and the real DeepSeek API is never called from a test.
