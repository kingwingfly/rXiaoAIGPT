# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workspace layout

Cargo workspace with four crates (directory name ≠ crate name):

- `rXiaoai/` — crate **`xiaoai`** (library, published to crates.io): remote control of XiaoAi speakers (小爱音箱) via Xiaomi's cloud APIs — login, TTS speak, volume, play/pause, play URL, status, and chat-history queries.
- `rNetease/` — crate **`netease`** (library): a client for NetEase Cloud Music's private web API — weapi crypto, QR login, search, song-URL resolution, streaming proxy. Depends on nothing else in the workspace.
- `rBrain/` — crate **`brain`** (library): the hardware-agnostic intent framework — the traits, the LLM client, the control loop, and an MCP client for reaching tools.
- `rXiaoaiLLM/` — crate **`xiaoai_llm`** (binary): the agent. The only crate that depends on the other three.

Each crate has its own README (`README.md` at the root is the overview and crate table; `rXiaoai/`, `rNetease/`, `rBrain/`, `rXiaoaiLLM/` each document their own crate). Keep them distinct — they were once byte-identical copies.

The dependency arrow points inward: `xiaoai_llm` → {`brain`, `xiaoai`, `netease`}, and none of those three depend on each other. **`brain` must never depend on `xiaoai`, `netease`, or `xiaoai_llm`** — its whole purpose is that the same intent layer can drive different hardware, so it may not know which hardware it has. Its dependency list (`serde`, `serde_json`, `thiserror`, `async-trait`, `tracing`, `async-openai` — being an LLM client is `brain`'s own job — and `rmcp` with only the `client` feature, since it reaches tools as an MCP client) is the enforcement mechanism; adding a device SDK, an audio library or a content API there is a bug.

## Commands

```sh
cargo build --workspace                      # build everything
cargo run -p xiaoai_llm                      # run the agent binary
cargo test --workspace --exclude xiaoai      # the default: offline, no credentials
cargo clippy --workspace --all-targets       # CI-safe validation, including `xiaoai`
cargo test -p xiaoai <name> -- --nocapture   # one live-API test; needs hardware
```

`xiaoai_llm`, `netease` and `brain` have self-contained tests, safe to run anywhere — anything network-facing uses a local mock. **The `xiaoai` crate's tests hit live Xiaomi APIs**: they need real credentials (`.env`, see `.env.example`) plus an actual device on the account, and hardcode the alias `"哈哈"` that only exists on the author's account. Do not expect them to pass in CI or without hardware; run `cargo test --workspace --exclude xiaoai` and use `cargo clippy --workspace --all-targets` to validate `xiaoai` instead.

### Configuration and logging

Configuration is environment-only, all of it read in `config.rs`; nothing is hardcoded in the binary. See `.env.example` for the full documented list.

Note the deliberate split between two "address" notions: `XIAOAI_HOST_IP`/`XIAOAI_PORT` are the **bind address** (the server actually binds `0.0.0.0`), while `XIAOAI_PUBLIC_BASE_URL` is the **speaker-facing URL** handed to the device. They coincide on a LAN, but behind a tunnel (Cloudflare Access) the speaker talks to a public hostname unrelated to the bind address. Since the speaker cannot authenticate, such a deployment needs a Bypass policy on the audio path — `XIAOAI_STREAM_TOKEN` is the unguessable prefix the origin checks in its place.

Logging is `tracing`, initialised in `main.rs`; `RUST_LOG` overrides the default `warn,xiaoai_llm=info,xiaoai=info`. `RUST_LOG=xiaoai=debug` dumps the raw Xiaomi login exchanges. There is no `XIAOAI_DEBUG`; a doc mentioning one is stale. The one exception to "no `println!`" is the identity-verification prompt in `account.rs`, which is an interactive stdin dialogue rather than a log and must not be silenceable.

`DEEPSEEK_API_KEY` is required — the binary refuses to start without it, since the regex parser that used to stand in for the model is gone. `XIAOAI_STREAM_TOKEN`, when set, mounts the audio routes under a `/{token}/` prefix and builds the speaker's base URL from the same value. The Cloudflare Access deployment shape (Bypass policy on a narrow audio path, origin-side token, since the speaker cannot send `CF-Access-Client-*` headers) is written up in `rXiaoaiLLM/README.md`.

## Architecture

### Declarative HTTP via `api_req` derive macros

All HTTP in the `xiaoai` crate is driven by two derive macros from the `api_req` crate:

- `#[derive(ApiCaller)]` on an empty struct (e.g. `OpApi`, `AccountApi`, `RecordApi`) defines an API endpoint group: `base_url`, `default_headers`, redirect policy.
- `#[derive(Payload)]` on a request struct defines one request: `path` (with `{field}` interpolation from the struct's fields), `method`, per-request `headers` (also interpolated — this is how cookies like `userId={user_id}; serviceToken={service_token}` are built), `req = query|form` (serialization target), and an optional `before_deserialize` hook (used to strip Xiaomi's `&&&START&&&` response prefix).

Calling pattern: `let resp: SomeResponse = SomeApi::request(payload).await?`. Fields marked `#[serde(skip_serializing)]` exist only for path/header interpolation and are not sent in the body/query.

### Xiaomi API quirks encoded in the crate

- **Login** (`account.rs`) is a two-step flow against `account.xiaomi.com`: `serviceLogin` (may succeed directly via cached `passToken` cookie) then `serviceLoginAuth2` with the MD5-uppercase-hashed password. A `notificationUrl` on the `serviceLoginAuth2` response means Xiaomi is demanding identity verification; that becomes a `Verification` (send code → submit code → resume login in the same cookie session), which `login` drives interactively and `try_login` hands back to the caller. The final `serviceToken` is fetched separately by following `location` with a SHA1-based `clientSign`. The result (`AuthData`) is cached in `auth_data.json` by `load_or_login_and_save*` — delete that file to force a re-login.
- **Operations** (`op.rs`) all POST to `/remote/ubus` on `api2.mina.mi.com` and share one envelope (`OpPayloadBuilder::build`); the inner `message` is JSON-serialized *into a string* inside the form. Symmetrically, responses embed JSON as strings. Both directions go through `serde_util`, which also holds the `&&&START&&&` strippers.
- `DEVICE_ID` is a global `LazyLock`: env var `DEVICE_ID` (must be 16 chars) or randomly generated per process.
- `device_by_alias` only works for the *owner* of the device, not administrators.

### The agent (`rXiaoaiLLM/`)

Wiring only: it implements `brain`'s traits over XiaoAi hardware and hands them to `brain::Agent`. Intent recognition is DeepSeek's job — the ordered-regex `Command::parse` that used to do it is gone, with no fallback. One module per concern:

- `config.rs` — `Config::from_env`, the only place deployment values enter.
- `gate.rs` — the cheap local filter in front of the API. The agent sees *every* utterance the speaker hears, polled every 3 s, so sending all of it to DeepSeek would be the dominant cost. `嘻嘻`/`不嘻嘻` still toggle the agent (`不嘻嘻` matched first, being a special case of `嘻嘻`), and obvious noise is dropped. It deliberately does **not** parse intent — that distinction is the point of the refactor.
- `speaker.rs` — `brain::Speaker` and `brain::UtteranceSource` over Xiaomi's cloud APIs. Two invariants live here. A record is marked seen **before** it is acted on, so a command that fails or is ignored is not retried forever. And after starting playback the source blocks until the speaker stops — `Paused` counts as still playing — because otherwise the next poll finds the triggering utterance still at the top of the history and replays it. `UtteranceSource::next` returning `None` would end the loop permanently, so this implementation polls internally and never returns it.
- `tools.rs` — the `Assistant` MCP server: `#[tool]` methods `play_music` (local library first, NetEase on a miss; `source: "netease"` skips local), `stop`, `set_volume`, `tell_story`, with `schemars`-derived argument schemas. Adding a capability is one more `#[tool]` method. `main.rs` runs this as an in-memory MCP server the `brain` agent connects to.
- `music.rs` — `MusicIndex` (a cached set of audio paths relative to the music dir, rescanned on a miss) plus the axum router. A request path is URL-decoded and treated as a **regex** matched against the index, then rewritten so `ServeDir` serves the hit; `/random` and `/random/{artist}` redirect instead. Patterns come from speech, so an invalid or over-long one is a 400, never a panic. Only the fallback is wrapped in the pattern-matching middleware — `/random*` are literal paths.
- `source/` — `brain::MusicSource` implementations. `local.rs` shares the *same* `Arc<MusicIndex>` as the router, so the URLs it hands out resolve against the same file set the server looks them up in; `.ncm` hits are described from NetEase's own embedded metadata, falling back to `Artist/Title.ext`.
- `main.rs` — builds one `Arc<MusicIndex>` shared between the router and `LocalSource` (a second index could disagree about what exists), mounts `NeteaseSource::router()`, spawns the `Assistant` MCP server on one end of a `tokio::io::duplex` pipe, and starts `brain::Agent` connected to the other end. The server's `serve` awaits the client's `initialize`, so it runs concurrently with `Agent::connect` rather than being awaited first. A missing `DEEPSEEK_API_KEY` fails here, by name, rather than as a 401 from the first thing anyone says.

`XIAOAI_HOST_IP` must be the host's address on the speaker's network: the speaker fetches the audio itself. Behind a tunnel, set `XIAOAI_PUBLIC_BASE_URL` instead — the bind address and the URL handed to the speaker are separate concepts.

**`XIAOAI_STREAM_TOKEN`.** When set, the audio routes are mounted under an unguessable `/{token}/…` prefix and the speaker's base URL is built from the *same* value, so the two cannot drift apart. This exists because the intended deployment puts audio behind a Cloudflare Access **Bypass** policy — the speaker cannot send `CF-Access-Client-Id` headers, so the origin has to be what checks the secret. Unset, the routes are served plainly, which is what plain LAN use wants.

**`.ncm`, once and for all.** It is NetEase's encrypted container, written *client-side* by their desktop app when it caches a download. No NetEase endpoint serves one — `/api/song/enhance/player/url/v1` returns a plain mp3/flac CDN URL. So the local library decrypts `.ncm` on the fly (`ncmc_lib`, streaming, via `spawn_blocking` and a duplex pipe, so no plaintext ever hits the disk), while the online path streams plaintext audio and has nothing to decrypt. Known limitation: the `.ncm` response is chunked with **no `Content-Length`** (the plaintext length is the file size minus a header `ncmc_lib` does not report, and a wrong length is worse than none), so byte-range requests are unsupported on those paths. Local mp3/flac via `ServeDir` and NetEase streams — which forward `Range` upstream verbatim — are unaffected.

### `netease`

NetEase publishes no API; what exists is the private one its web player uses, whose request bodies are encrypted by the player's JavaScript under a scheme called **weapi**.

- `crypto.rs` — the scheme. The payload goes through AES-128-CBC twice (first under a key baked into the JavaScript, then under a random per-request 16-char secret) to become `params`; the secret itself goes through *textbook* RSA — no padding, over the reversed secret zero-padded to 128 bytes — to become `encSecKey`. Every constant is fixed by the server and cannot be negotiated, including the shared IV. `encrypt_with_secret` exists so tests can pin the secret and get deterministic output.
- `client.rs` — `reqwest` plus a cookie jar (the session: NetEase auth is entirely the `MUSIC_U` cookie) and `post_weapi`, which hides the encryption so endpoint modules deal only in plain JSON. It deliberately does **not** check the response's `code` field, because some endpoints (QR-login polling) use non-200 codes as ordinary states; call `client::ensure_ok` where a non-200 really is a failure.
- `session.rs` — a login is nothing but the `MUSIC_U` and `__csrf` cookies; there is no token endpoint and no refresh. Persisting a session is persisting those two strings.
- `api/` — one module per endpoint, added as needed. `login` is the QR flow (801 waiting → 802 scanned → 803 confirmed), and 803 is returned **once**, carrying the only `Set-Cookie` there will ever be — hence it drives `Client::http` by hand to read the headers. `search` uses `cloudsearch/pc`, not the thin legacy `search/get`. `url` resolves a song id: `ids` must be a JSON array *serialized into a string*, and the returned URL carries `expi: 1200` — **a TTL in seconds, so resolve just-in-time and never cache the URL**; cache the id instead.
- `stream.rs` — proxies that URL through to our own response without buffering, forwarding `Range` and the upstream `206`/`Content-Range` verbatim. A 403/404 from the CDN becomes `UrlExpired`, because on this path that is almost always what it means.

### `brain`

- `traits.rs` — the contract, no implementations: `Speaker` (output device), `UtteranceSource` (input, pull-based so both polling and streaming fit), `MusicSource` (content), plus `Utterance`, `Track`, `Playable` and `BrainErr`. All use `async_trait` because all are used as trait objects, which native async-in-trait does not allow. There is no `Tool` trait — tools are an MCP server the binary provides.
- `client.rs` — `LlmClient`, a chat-completions client over `async-openai` pointed at DeepSeek's OpenAI-compatible endpoint. Default model `deepseek-v4-flash`; **`deepseek-chat` and `deepseek-reasoner` were retired on 2026-07-24 and must not reappear.** The API base is overridable, which is how tests run against a local mock — `brain` never calls the real API from a test, and reads no environment variables (the binary passes `DEEPSEEK_API_KEY` in). `async-openai`'s types stay internal; the public vocabulary is `ChatMessage`/`ToolCall`/`ChatResponse`.
- `run.rs` — `Agent`/`run`, the control loop, plus the MCP-client bridge. `Agent::connect` connects an rmcp MCP client over a transport; `list_all_tools` (sorted by name, so the prompt stays reproducible) becomes the OpenAI `tools` array, and each tool call becomes an MCP `call_tool` whose `CallToolResult` text is fed back. Model call → tool calls → tool results → repeat, capped by `max_tool_iterations`; a bounded history of user/assistant text pairs (tool messages are deliberately *not* kept, since an assistant message with `tool_calls` is invalid without its replies). Tool `arguments` arrive as a JSON string and DeepSeek does not always close its braces, so parsing is fallible and a failure becomes a tool-error message back to the model. A failed turn is logged and skipped, never fatal.

`brain` reaches tools over MCP, so it needs no `Tool` trait and dispatches on no tool names. Adding a capability is purely a matter of adding a `#[tool]` method to the binary's `Assistant`. Note rmcp's `ToolRouter` cannot be dispatched purely in-process (its `ToolCallContext` needs a live `Peer`), which is why the binary runs a real MCP server over an in-memory `tokio::io::duplex` pipe and `brain` is a real MCP client — no OS process or socket, just serialized JSON over the pipe.
