# xiaoai_llm

An agent that turns a XiaoAi speaker (小爱音箱) into a player for your own
music library.

Xiaomi offers no push API, so the agent polls the speaker's conversation
history every few seconds and matches what you said against a few patterns. It
also runs a small HTTP server; when a command matches, the speaker is told to
play a URL pointing back at it.

## Commands

| You say | Effect |
| --- | --- |
| 嘻嘻 | Start reacting to the commands below |
| 不嘻嘻 | Stop reacting until 嘻嘻 |
| 播放\<歌手\>的歌 / 我想听\<歌手\>的歌 | A random track by that artist |
| 播放\<歌手\>的\<歌名\> / 我要听\<歌名\> | One specific track |
| 随机播放 / 随便放首歌听 | A random track |

Matching is by regex over the *file paths* under the music directory, so how
well artist and title are found depends on how your files are named.

## Configuration

All configuration is environment variables (a `.env` file is read too); see
[`.env.example`](../.env.example).

| Variable | Default | Meaning |
| --- | --- | --- |
| `ACCOUNT_ID`, `ACCOUNT_PASSWORD` | — | Xiaomi account, required on first login |
| `XIAOAI_DEVICE` | — | Speaker alias, as shown in Mi Home. **Required** |
| `XIAOAI_HOST_IP` | — | Address the speaker reaches this host at. **Required** |
| `XIAOAI_PORT` | `3000` | Port for the music server |
| `XIAOAI_MUSIC_DIR` | `.` | Directory scanned and served |
| `XIAOAI_AUTH_CACHE` | `auth_data.json` | Cached login; delete to re-login |

`XIAOAI_HOST_IP` must be this machine's address *on the speaker's network* —
the speaker fetches the audio itself, so `127.0.0.1` will not work.

```sh
cargo run -p xiaoai_llm
```

## Layout

| File | Responsibility |
| --- | --- |
| `src/config.rs` | Configuration from the environment |
| `src/command.rs` | Turning what the user said into a `Command` |
| `src/music.rs` | The audio-file index and the HTTP server |
| `src/agent.rs` | The poll loop, and driving the speaker |
