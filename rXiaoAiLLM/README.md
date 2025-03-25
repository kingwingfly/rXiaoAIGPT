```rust ignore
use xiaoai::{load_or_login_and_save, device_by_alias, OpPayloadBuilder, OpApi};
use api_req::ApiCaller;

let auth_data = load_or_login_and_save("auth_data.json").await;
let device = device_by_alias(&auth_data, "卧室的小爱/XiaoAi in bedroom").await;
let payload = OpPayloadBuilder::new(auth_data, device.device_id).volume(50, "music");
let resp: serde_json::Value = OpApi::request(payload).await.unwrap();
let payload = OpPayloadBuilder::new(auth_data, device_id).speak("Hello world!");
let resp: serde_json::Value = OpApi::request(payload).await.unwrap();

let payload = LastAskPayload::new(&auth_data, &device, 2);
let resp: LastAskResponse = RecordApi::request(payload).await.unwrap();
```

Currently, account_id and account_password can only be loaded from env var.
```sh
ACCOUNT_ID=
ACCOUNT_PASSWORD=
```

Supported operations:
- speak
- volume
- pause and resume
- play url
- status query
- query chat history or record

# Acknowledgement
- [MiGPT](https://github.com/Afool4U/MIGPT)
- [MiService](https://github.com/Yonsm/MiService)
