# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workspace layout

Cargo workspace with four crates (directory name ≠ crate name):

- `rXiaoai/` — crate **`xiaoai`** (library, published to crates.io): remote control of XiaoAi speakers (小爱音箱) via Xiaomi's cloud APIs — login, TTS speak, volume, play/pause, play URL, status, and chat-history queries.
- `rNetease/` — crate **`netease`** (library): a client for NetEase Cloud Music's private web API. Depends on nothing else in the workspace.
- `rBrain/` — crate **`brain`** (library): the hardware-agnostic intent framework — trait definitions only.
- `rXiaoaiLLM/` — crate **`xiaoai_llm`** (binary): the agent. The only crate that depends on the other three.

The dependency arrow points inward: `xiaoai_llm` → {`brain`, `xiaoai`, `netease`}, and none of those three depend on each other. **`brain` must never depend on `xiaoai`, `netease`, or `xiaoai_llm`** — its whole purpose is that the same intent layer can drive different hardware, so it may not know which hardware it has. Its dependency list (`serde`, `serde_json`, `thiserror`, `async-trait`) is the enforcement mechanism; adding an HTTP client or a device SDK there is a bug.

## Commands

```sh
cargo build                      # build workspace
cargo run -p xiaoai_llm          # run the agent binary
cargo test -p xiaoai_llm         # offline tests (command parsing + music server)
cargo test -p xiaoai <name> -- --nocapture   # one live-API test
```

`xiaoai_llm`, `netease` and `brain` have self-contained tests, safe to run anywhere — anything network-facing uses a local mock. **The `xiaoai` crate's tests hit live Xiaomi APIs**: they need real credentials (`.env`, see `.env.example`) plus an actual device on the account, and hardcode the alias `"哈哈"` that only exists on the author's account. Do not expect them to pass in CI or without hardware; run `cargo test --workspace --exclude xiaoai` and use `cargo clippy --workspace --all-targets` to validate `xiaoai` instead.

### Configuration and logging

Configuration is environment-only, all of it read in `config.rs`; nothing is hardcoded in the binary. See `.env.example` for the full documented list.

Note the deliberate split between two "address" notions: `XIAOAI_HOST_IP`/`XIAOAI_PORT` are the **bind address** (the server actually binds `0.0.0.0`), while `XIAOAI_PUBLIC_BASE_URL` is the **speaker-facing URL** handed to the device. They coincide on a LAN, but behind a tunnel (Cloudflare Access) the speaker talks to a public hostname unrelated to the bind address. Since the speaker cannot authenticate, such a deployment needs a Bypass policy on the audio path — `XIAOAI_STREAM_TOKEN` is the unguessable prefix the origin checks in its place.

Logging is `tracing`, initialised in `main.rs`; `RUST_LOG` overrides the default `warn,xiaoai_llm=info,xiaoai=info`. `RUST_LOG=xiaoai=debug` dumps the raw Xiaomi login exchanges. The one exception to "no `println!`" is the identity-verification prompt in `account.rs`, which is an interactive stdin dialogue rather than a log and must not be silenceable.

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

Still no LLM despite the name. One module per concern:

- `config.rs` — `Config::from_env`, the only place deployment values enter.
- `command.rs` — `Command::parse` maps an utterance to a `Command` enum via ordered Chinese regexes (`不嘻嘻` before `嘻嘻`, artist-only before the general play pattern, since each is a special case of the next). Unit-tested.
- `music.rs` — `MusicIndex` (a cached set of audio paths relative to the music dir, rescanned on a miss) plus the axum router. A request path is URL-decoded and treated as a **regex** matched against the index, then rewritten so `ServeDir` serves the hit; `/random` and `/random/{artist}` redirect instead. Patterns come from speech, so an invalid or over-long one is a 400, never a panic. Only the fallback is wrapped in the pattern-matching middleware — `/random*` are literal paths.
- `agent.rs` — the loop. `tick()` polls the last conversation record every 3 s, marks it seen *before* acting (so a failed or ignored command is not retried forever), and dispatches. `play_and_wait` pauses, points the speaker at our server, then blocks on `status()` until playback ends — otherwise the next poll would see the triggering utterance again.

`XIAOAI_HOST_IP` must be the host's address on the speaker's network: the speaker fetches the audio itself.

### `netease`

NetEase publishes no API; what exists is the private one its web player uses, whose request bodies are encrypted by the player's JavaScript under a scheme called **weapi**.

- `crypto.rs` — the scheme. The payload goes through AES-128-CBC twice (first under a key baked into the JavaScript, then under a random per-request 16-char secret) to become `params`; the secret itself goes through *textbook* RSA — no padding, over the reversed secret zero-padded to 128 bytes — to become `encSecKey`. Every constant is fixed by the server and cannot be negotiated, including the shared IV. `encrypt_with_secret` exists so tests can pin the secret and get deterministic output.
- `client.rs` — `reqwest` plus a cookie jar (the session: NetEase auth is entirely the `MUSIC_U` cookie) and `post_weapi`, which hides the encryption so endpoint modules deal only in plain JSON. It deliberately does **not** check the response's `code` field, because some endpoints (QR-login polling) use non-200 codes as ordinary states; call `client::ensure_ok` where a non-200 really is a failure.
- `api/` — one module per endpoint, added as needed.

### `brain`

Trait definitions only, no implementations: `Tool` (a function the model can call), `Speaker` (output device), `UtteranceSource` (input, pull-based so both polling and streaming fit), `MusicSource` (content), plus `Utterance`, `Track`, `Playable` and `BrainErr`. All four use `async_trait` because all four are used as trait objects, which native async-in-trait does not allow.

Adding a capability means implementing `Tool` and registering it — nothing dispatches on tool names, so it is purely additive. `Tool::description`/`parameters` are prompt text read by the model, not developer documentation.
