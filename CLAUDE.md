# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workspace layout

Cargo workspace with two crates (directory name ≠ crate name):

- `rXiaoai/` — crate **`xiaoai`** (library, published to crates.io): remote control of XiaoAi speakers (小爱音箱) via Xiaomi's cloud APIs — login, TTS speak, volume, play/pause, play URL, status, and chat-history queries.
- `rXiaoaiLLM/` — crate **`xiaoai_llm`** (binary): an agent built on `xiaoai` that polls the speaker's conversation history and reacts to Chinese voice commands, serving local music files over HTTP for the speaker to play.

## Commands

```sh
cargo build                      # build workspace
cargo run -p xiaoai_llm          # run the agent binary
cargo test -p xiaoai_llm         # offline tests (command parsing + music server)
cargo test -p xiaoai <name> -- --nocapture   # one live-API test
```

`xiaoai_llm`'s tests are self-contained and safe to run anywhere. **The `xiaoai` crate's tests hit live Xiaomi APIs**: they need real credentials (`.env`, see `.env.example`) plus an actual device on the account, and hardcode the alias `"哈哈"` that only exists on the author's account. Do not expect them to pass in CI or without hardware — use `cargo clippy --workspace --all-targets` to validate changes there.

Configuration is environment-only (`XIAOAI_DEVICE`, `XIAOAI_HOST_IP`, `XIAOAI_PORT`, `XIAOAI_MUSIC_DIR`, `XIAOAI_AUTH_CACHE`); nothing is hardcoded in the binary.

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
