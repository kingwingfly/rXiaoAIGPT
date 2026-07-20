# rXiaoAiLLM

Turn a XiaoAi speaker (小爱音箱) into something that plays *your* music, in Rust.

Xiaomi offers no push API and no way to run code on the device, so everything
here works from the outside: poll the speaker's conversation history through
Xiaomi's cloud, decide what the user meant, and tell the speaker to play a URL
that points back at us.

## Crates

| Directory | Crate | What it is |
| --- | --- | --- |
| [`rXiaoai/`](rXiaoai) | `xiaoai` | Library ([crates.io](https://crates.io/crates/xiaoai)): log in to Xiaomi's cloud, then speak, set volume, play/pause, play a URL, read status and conversation history. |
| [`rNetease/`](rNetease) | `netease` | Library: a client for NetEase Cloud Music's private web API — weapi encryption, QR login, search, song-URL resolution, and a streaming proxy. |
| [`rBrain/`](rBrain) | `brain` | Library: the hardware-agnostic intent framework — the `Tool` / `Speaker` / `UtteranceSource` / `MusicSource` traits, a DeepSeek (OpenAI-compatible) client, a tool registry, and the control loop. |
| [`rXiaoaiLLM/`](rXiaoaiLLM) | `xiaoai_llm` | The binary. Wiring only: it implements `brain`'s traits over XiaoAi hardware and the local music library, and serves the audio over HTTP. |

### The dependency rule

```
xiaoai_llm ──┬──> brain
             ├──> netease
             └──> xiaoai
```

`xiaoai_llm` depends on the other three. **Those three depend on none of each
other, and nothing depends on `xiaoai_llm`.** That is not tidiness for its own
sake:

- `brain` describes what an assistant *is* — a model choosing among tools that
  drive a speaker and a music source. If it knew about `xiaoai` it could no
  longer drive anything else, which is the whole reason it is a separate crate.
  Its dependency list (`serde`, `serde_json`, `thiserror`, `async-trait`,
  `tracing`, `async-openai`) is the enforcement mechanism.
- `netease` is a plain API client. Nothing speaker-shaped belongs in it.

A change that adds a workspace dependency along any arrow not drawn above is a
bug, even when it compiles.

## Quick start

```sh
cp .env.example .env    # fill in ACCOUNT_ID / ACCOUNT_PASSWORD / XIAOAI_DEVICE / XIAOAI_HOST_IP
cargo run -p xiaoai_llm
```

`XIAOAI_HOST_IP` must be this machine's address *on the speaker's network*: the
speaker fetches the audio itself, so `127.0.0.1` will never work. Behind a
tunnel or reverse proxy, set `XIAOAI_PUBLIC_BASE_URL` too — see
[the agent's deployment notes](rXiaoaiLLM/README.md#deployment-behind-cloudflare-access).

Per-crate documentation: [`xiaoai`](rXiaoai/README.md),
[`netease`](rNetease/README.md), [`brain`](rBrain/README.md),
[`xiaoai_llm`](rXiaoaiLLM/README.md).

## Tests

```sh
cargo test --workspace --exclude xiaoai      # offline, no credentials — the default
cargo clippy --workspace --all-targets       # CI-safe validation of everything
```

`xiaoai_llm`, `netease` and `brain` are self-contained: everything
network-facing is tested against a local mock, so those tests run anywhere.

**`cargo test -p xiaoai` hits the live Xiaomi APIs.** It needs real credentials
in `.env`, an actual speaker on that account, and it hardcodes the device alias
`"哈哈"`, which exists only on the author's account. It cannot pass in CI or on
anybody else's machine, so a failure there is not evidence of a regression.

## Acknowledgement

- [MiGPT](https://github.com/Afool4U/MIGPT)
- [MiService](https://github.com/Yonsm/MiService)
