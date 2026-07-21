# xiaoai_llm

The binary: an agent that turns a XiaoAi speaker (小爱音箱) into a player for
your own music library.

Xiaomi offers no push API, so the agent polls the speaker's conversation history
every 3 seconds and looks at what you said. It also runs a small HTTP server;
when a command matches, the speaker is told to play a URL pointing back at that
server, and the agent waits for playback to end before polling again — otherwise
the next poll would see the triggering utterance still at the top of the history
and replay it.

This crate is **wiring**. The reusable parts live elsewhere: speaker control in
[`xiaoai`](../rXiaoai), the intent framework in [`brain`](../rBrain), NetEase in
[`netease`](../rNetease). What is here is the implementation of `brain`'s traits
over that hardware, plus the HTTP server for the local library.

Intent recognition is DeepSeek's. An utterance passes a cheap local gate — is
this addressed to us at all? — and anything that survives goes to the model,
which decides what was meant and which tool to call. The ordered-regex parser
that used to do this is gone, with no fallback, so `DEEPSEEK_API_KEY` is
required and the binary says so at startup rather than failing on first use.

## Commands

| You say | Effect |
| --- | --- |
| 嘻嘻 | Start reacting to the commands below |
| 不嘻嘻 | Stop reacting until 嘻嘻 |
| 播放\<歌手\>的歌 / 我想听\<歌手\>的歌 | A random track by that artist |
| 播放\<歌手\>的\<歌名\> / 我要听\<歌名\> | One specific track |
| 随机播放 / 随便放首歌听 | A random track |

Matching is regex over the *file paths* under the music directory, so how well
artist and title are found depends on how your files are named
(`Artist/Title.ext` is what the metadata fallback assumes).

## Configuration

Configuration is environment-only — `config.rs` is the single place deployment
values enter, and nothing is hardcoded. A `.env` file is read if present; see
[`.env.example`](../.env.example) for the annotated list.

| Variable | Default | Meaning |
| --- | --- | --- |
| `ACCOUNT_ID`, `ACCOUNT_PASSWORD` | — | Xiaomi account. Required on first login |
| `XIAOAI_DEVICE` | — | Speaker alias as shown in Mi Home. **Required** |
| `XIAOAI_HOST_IP` | — | Address the speaker reaches this host at. **Required** |
| `XIAOAI_PORT` | `3000` | Port the HTTP server binds |
| `XIAOAI_PUBLIC_BASE_URL` | `http://$XIAOAI_HOST_IP:$XIAOAI_PORT` | URL handed to the speaker |
| `XIAOAI_MUSIC_DIR` | `.` | Directory scanned and served |
| `XIAOAI_AUTH_CACHE` | `auth_data.json` | Cached Xiaomi login; delete to re-login |
| `XIAOAI_STREAM_TOKEN` | — | Unguessable prefix the origin checks (see deployment) |
| `NETEASE_SESSION` | `netease_session.json` | Cached NetEase cookies |
| `DEEPSEEK_API_KEY` | — | Key for the LLM intent layer |
| `DEEPSEEK_MODEL` | `deepseek-v4-flash` | Model name |
| `DEVICE_ID` | random per process | Fixed 16-character device id, read by `xiaoai` |
| `RUST_LOG` | `warn,xiaoai_llm=info,xiaoai=info` | Log filter |

### Bind address vs speaker-facing URL

These are two different things, and conflating them is the usual reason a
tunnelled deployment fails:

- The server always binds **`0.0.0.0:$XIAOAI_PORT`**. `XIAOAI_HOST_IP` is not a
  bind address; it only feeds the default of the next item.
- **`XIAOAI_PUBLIC_BASE_URL`** is what the speaker is told to fetch. On a LAN
  the two coincide (`http://192.168.1.20:3000`); behind a tunnel or reverse
  proxy the speaker talks to a public hostname on port 443 that says nothing
  about where we listen.

Either way, the address must be reachable *by the speaker* — it downloads the
audio itself, so `127.0.0.1` never works.

### Logging

Logging is [`tracing`], with an `EnvFilter`. `RUST_LOG` overrides the default
wholesale; `RUST_LOG=xiaoai=debug` dumps the raw Xiaomi login exchanges. (There
is no `XIAOAI_DEBUG` — if you find one mentioned anywhere, it is stale.)

[`tracing`]: https://docs.rs/tracing

## Running

```sh
cp ../.env.example ../.env   # then fill it in
cargo run -p xiaoai_llm
```

## The music server

A request path is URL-decoded and, if it is not an exact indexed path, compiled
as a **regex** and matched against the index of audio files under the music
directory; the first hit is what gets served. Patterns come from speech
recognition, so an invalid or over-long (>64 character) one is a `400`, never a
panic. `/random` and `/random/{artist}` are literal routes that redirect to a
chosen track rather than pattern-matching.

The index is cached and rescanned on a miss, which also picks up files added
since startup.

### `.ncm`

`.ncm` is NetEase Cloud Music's encrypted container, **written by its desktop
client** when it caches a download. No NetEase API ever serves one —
`/api/song/enhance/player/url/v1` returns a plain mp3/flac CDN URL. So there are
two entirely separate paths here:

- **Local `.ncm` files** are indexed like any other track and decrypted *on the
  way out*, streaming, via `ncmc_lib`. No plaintext copy touches the disk. They
  need their own handler because `ServeDir` would hand the speaker raw
  ciphertext. Their NetEase metadata is also read for track titles, which beats
  anything a filename can say.
- **Online NetEase tracks** are already plain audio; there is nothing to
  decrypt. Their URLs expire in about 20 minutes and must be resolved
  just-in-time — see [`netease`](../rNetease/README.md).

### Known limitation: no seeking within `.ncm`

The `.ncm` response is chunked with **no `Content-Length`**: the plaintext
length is the file size minus a header whose size `ncmc_lib` does not report,
and a wrong length is worse than none. Consequently **byte-range requests are
not supported on `.ncm` paths** — a speaker that seeks or reconnects mid-track
restarts it. Local mp3/flac (served by `ServeDir`) and NetEase streams (which
forward `Range` upstream verbatim) are unaffected. In practice the speaker plays
tracks start to finish, so this has not been worth working around.

## Deployment behind Cloudflare Access

Running this on a public server gated by Cloudflare Access hits one hard
constraint:

> **The speaker fetches the audio itself and cannot send
> `CF-Access-Client-Id` / `CF-Access-Client-Secret` headers.**

It is a consumer appliance with no place to configure them. So a service token
solves nothing for the audio path, no matter how the policy is written.

Cloudflare Access policy actions are `Allow`, `Block`, `Bypass` and
`Service Auth`, and **`Bypass` and `Service Auth` are evaluated before
`Allow`/`Block`**. That ordering is what makes the following work:

1. Put the audio endpoint on a **narrow, unguessable path** — that is what
   `XIAOAI_STREAM_TOKEN` is for — and cover exactly that path with a **`Bypass`**
   policy (Include → Everyone). Bypass wins the evaluation, so the speaker's
   unauthenticated `GET` goes straight through.
2. Have the **origin check the token itself**. Under Bypass, Cloudflare stops
   being the gate; the secret in the URL is what is left between the internet
   and your library. Unset means no check, which is fine on a LAN and not fine
   here. **Caveat as of today:** `XIAOAI_STREAM_TOKEN` is read by `config.rs`
   but nothing else consumes it yet — the router does not enforce a prefix. Until
   it does, a Bypass policy leaves the audio path genuinely open, so do not
   deploy that way and assume the token is protecting you.
3. Keep every other path behind the normal Access policies (or `Service Auth`
   for machine callers). Only the audio path is exposed.
4. Set `XIAOAI_PUBLIC_BASE_URL` to the public hostname, e.g.
   `https://music.example.com`. The server still binds `0.0.0.0:$XIAOAI_PORT`
   behind the tunnel.

Cloudflare's own caveat applies and is worth repeating: under a Bypass policy
the zone's security settings revert to their defaults for that path, and
Cloudflare does not recommend Bypass as permanent access to an internal
application. Treat the unguessable path plus the origin-side token as the real
control, and keep the exposed surface to the audio endpoint alone.

## Layout

| File | Responsibility |
| --- | --- |
| `src/config.rs` | Configuration from the environment; the only entry point for deployment values |
| `src/gate.rs` | The cheap local filter deciding what is worth an API call — not a parser |
| `src/speaker.rs` | `brain::Speaker` and `brain::UtteranceSource` over the Xiaomi cloud APIs |
| `src/tools.rs` | The `Assistant` MCP server: `#[tool]` methods `play_music`, `stop`, `set_volume`, `tell_story` |
| `src/music.rs` | The audio-file index, the HTTP router, and `.ncm` decryption |
| `src/source/` | `brain::MusicSource` implementations — `local` and `netease` |
| `src/main.rs` | Wiring: config, shared index, router, in-memory MCP server, `brain::Agent` |

## Tests

```sh
cargo test -p xiaoai_llm
```

Self-contained: command parsing, config, and the music server against a
temporary directory (including a synthetic `.ncm` fixture the real decoder
accepts). No credentials, no network.
