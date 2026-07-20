# xiaoai

Remote control of XiaoAi speakers (小爱音箱) through Xiaomi's cloud APIs;
远程操作小爱同学（小爱音箱）.

A standalone library: it depends on nothing else in this workspace, and knows
nothing about agents, intents or music. Published on
[crates.io](https://crates.io/crates/xiaoai).

Supported operations:

- speak (TTS)
- volume
- pause and resume
- play url
- status query
- query chat history / conversation records

```rust ignore
use xiaoai::{
    load_or_login_and_save_with_env, device_by_alias, OpPayloadBuilder, OpResponse, OpApi,
    LastAskPayload, LastAskResponse, RecordApi, ApiCaller as _,
};

// Logs in on first use and caches the result in auth_data.json.
let auth_data = load_or_login_and_save_with_env("auth_data.json").await.unwrap();
// The account must *own* the device; being an administrator is not enough.
let device = device_by_alias(&auth_data, "卧室的小爱/XiaoAi in bedroom").await.unwrap();

let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).volume(50);
let resp: OpResponse = OpApi::request(payload).await.unwrap();
let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).speak("Hello world!");
let resp: OpResponse = OpApi::request(payload).await.unwrap();

// The two most recent things said to the speaker.
let payload = LastAskPayload::new(&auth_data, &device, 2);
let resp: LastAskResponse = RecordApi::request(payload).await.unwrap();
```

## Credentials

`ACCOUNT_ID` and `ACCOUNT_PASSWORD` are read from the environment or a `.env`
file (see [`.env.example`](../.env.example)). On a new device or IP, Xiaomi
usually demands identity verification: `login` handles that interactively on
the terminal, and `try_login` + `Verification` exposes the same flow for
non-interactive callers.

This crate logs through [`tracing`]; install a subscriber to see anything. Set
`RUST_LOG=xiaoai=debug` to trace the raw (undocumented) login exchanges.

[`tracing`]: https://docs.rs/tracing

A fixed `DEVICE_ID` (exactly 16 characters) is worth setting: without one a
fresh id is generated per process, and Xiaomi then asks for identity
verification far more often.

## Quirks worth knowing

Xiaomi documents none of this; the crate encodes what the endpoints actually do.

- **Login is two steps** against `account.xiaomi.com`: `serviceLogin` (which may
  succeed outright from a cached `passToken` cookie) then `serviceLoginAuth2`
  with the password hashed as uppercase MD5. A `notificationUrl` on the second
  response means Xiaomi wants identity verification. The final `serviceToken` is
  fetched separately, by following `location` with a SHA1-based `clientSign`.
- **`device_by_alias` only works for the device's owner**, not for an
  administrator of it — an easy way to get a puzzling "not found".
- **Everything is smuggled through strings.** Operations all POST to
  `/remote/ubus` with one envelope whose inner `message` is JSON serialized
  *into a string*; responses embed JSON as strings the same way, and are
  prefixed with `&&&START&&&`. `serde_util` handles both directions, so callers
  see ordinary structs.
- HTTP itself is declarative: `#[derive(ApiCaller)]` defines an endpoint group
  and `#[derive(Payload)]` one request, both from the `api_req` crate. Adding an
  operation means adding a payload struct, not writing request code.

## Tests

**These tests hit the live Xiaomi APIs.** They need real credentials, an actual
speaker on the account, and they hardcode the alias `"哈哈"` that exists only on
the author's account — so they cannot pass in CI or on another machine, and are
run one at a time by hand:

```sh
cargo test -p xiaoai <name> -- --nocapture
```

To validate a change to this crate without hardware, use
`cargo clippy --workspace --all-targets`.

## Acknowledgement

- [MiGPT](https://github.com/Afool4U/MIGPT)
- [MiService](https://github.com/Yonsm/MiService)
