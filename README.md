# rXiaoAiLLM

Remote control for XiaoAi speakers (小爱音箱), in Rust.

Two crates:

| Directory | Crate | What it is |
| --- | --- | --- |
| [`rXiaoai/`](rXiaoai) | `xiaoai` | Library: log in to Xiaomi's cloud, then speak, set volume, play/pause, play a URL, read status and chat history. |
| [`rXiaoaiLLM/`](rXiaoaiLLM) | `xiaoai_llm` | Binary: an agent that watches what you said to the speaker and plays your local music files in response. |

## Quick start

```sh
cp .env.example .env   # fill in ACCOUNT_ID / ACCOUNT_PASSWORD / XIAOAI_*
cargo run -p xiaoai_llm
```

See [`rXiaoai/README.md`](rXiaoai/README.md) for library usage and
[`rXiaoaiLLM/README.md`](rXiaoaiLLM/README.md) for the agent.

## Acknowledgement

- [MiGPT](https://github.com/Afool4U/MIGPT)
- [MiService](https://github.com/Yonsm/MiService)
