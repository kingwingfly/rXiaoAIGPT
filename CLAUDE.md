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
cargo test -p xiaoai <name> -- --nocapture   # run one test
```

**All tests hit live Xiaomi APIs** and require real credentials (`.env` with `ACCOUNT_ID` / `ACCOUNT_PASSWORD`, see `.env.example`) plus an actual device on the account. Do not expect `cargo test` to pass in CI or without hardware; use `cargo check` / `cargo clippy` to validate changes. Tests and the agent also hardcode a device alias (`"哈哈"`) that only exists on the author's account.

## Architecture

### Declarative HTTP via `api_req` derive macros

All HTTP in the `xiaoai` crate is driven by two derive macros from the `api_req` crate:

- `#[derive(ApiCaller)]` on an empty struct (e.g. `OpApi`, `AccountApi`, `RecordApi`) defines an API endpoint group: `base_url`, `default_headers`, redirect policy.
- `#[derive(Payload)]` on a request struct defines one request: `path` (with `{field}` interpolation from the struct's fields), `method`, per-request `headers` (also interpolated — this is how cookies like `userId={user_id}; serviceToken={service_token}` are built), `req = query|form` (serialization target), and an optional `before_deserialize` hook (used to strip Xiaomi's `&&&START&&&` response prefix).

Calling pattern: `let resp: SomeResponse = SomeApi::request(payload).await?`. Fields marked `#[serde(skip_serializing)]` exist only for path/header interpolation and are not sent in the body/query.

### Xiaomi API quirks encoded in the crate

- **Login** (`account.rs`) is a two-step flow against `account.xiaomi.com`: `serviceLogin` (may succeed directly via cached `passToken` cookie) then `serviceLoginAuth2` with the MD5-uppercase-hashed password. A third response shape (`LoginResponse3`) means Xiaomi is demanding interactive confirmation at a `notificationUrl`. The final `serviceToken` is fetched separately by following `location` with a SHA1-based `clientSign`. The result (`AuthData`) is cached in `auth_data.json` by `load_or_login_and_save*` — delete that file to force a re-login.
- **Operations** (`op.rs`) all POST to `/remote/ubus` on `api2.mina.mi.com`; the inner `message` is JSON-serialized *into a string* inside the form (see `serde_to_string`). Symmetrically, responses embed JSON as strings, decoded with `serde_from_string` (also in `record.rs`).
- `DEVICE_ID` is a global `LazyLock`: env var `DEVICE_ID` (must be 16 chars) or randomly generated per process.
- `device_by_alias` only works for the *owner* of the device, not administrators.

### The agent (`rXiaoaiLLM/src/agent.rs`)

Single loop, no LLM yet despite the name:

1. Spawns an axum server on `0.0.0.0:<port>` serving audio files from the current working directory. Routing is regex-based: a request path is URL-decoded and treated as a regex matched against a lazily-built index of audio files (`find_file` middleware); `/random` and `/random/{singer}` redirect to a random match.
2. Polls the speaker's last conversation record every 3 s via `LastAskPayload` and matches the query text against hardcoded Chinese regexes: `嘻嘻`/`不嘻嘻` toggle the agent on/off, `播放…的歌` / `我想听…` / `随机播放` trigger `play_url` pointing back at the local HTTP server, then blocks polling `status()` until playback ends.

The device alias and LAN IP the speaker must reach are hardcoded in `rXiaoaiLLM/src/main.rs` — the IP must be the host's address on the same network as the speaker.
