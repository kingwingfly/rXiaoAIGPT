# xiaoai

Remote control of XiaoAi speakers (小爱音箱) through Xiaomi's cloud APIs;
远程操作小爱同学（小爱音箱）.

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
non-interactive callers. Set `XIAOAI_DEBUG=1` to trace the raw exchanges.

## Tests

The tests hit the live Xiaomi APIs and need real credentials plus a device on
the account, so they do not run unattended:

```sh
cargo test -p xiaoai <name> -- --nocapture
```

## Acknowledgement

- [MiGPT](https://github.com/Afool4U/MIGPT)
- [MiService](https://github.com/Yonsm/MiService)
