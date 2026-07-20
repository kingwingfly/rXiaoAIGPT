# netease

A client for NetEase Cloud Music (网易云音乐), in Rust.

NetEase publishes no API. What exists is the private one its own web player
uses, whose request bodies are encrypted by the player's JavaScript under a
scheme called **weapi**. This crate reimplements that scheme and wraps it in an
HTTP client that carries a session, so endpoint modules deal only in plain JSON.

The crate is deliberately standalone: it knows nothing about speakers, agents or
intent parsing, and depends on no other crate in this workspace.

## What is here

| Module | Responsibility |
| --- | --- |
| `crypto` | The weapi envelope: `params` + `encSecKey` |
| `client` | `reqwest` plus a cookie jar, and `post_weapi` |
| `session` | The two cookies that *are* a login, and their persistence |
| `api::login` | QR-code login (unikey → scan → 803) |
| `api::search` | `cloudsearch/pc` — songs, with artists, album and privilege |
| `api::url` | Resolving a song id to a playable CDN URL |
| `stream` | Proxying that URL straight through to an HTTP response |

## Example

```rust ignore
use netease::{Level, SearchQuery, Session};
use netease::api::{search::search_songs, url::song_url};

// Resume a saved login; `Client::new()` alone is anonymous and resolves little.
let session = Session::load("netease_session.json")?;
let client = session.client()?;

let songs = search_songs(&client, &SearchQuery::new("晴天").limit(5)).await?;
let song = &songs[0];
println!("{} - {}", song.artist_names(), song.name);

// Resolve *now*, play *now*: see "URLs expire" below.
let info = song_url(&client, song.id, Level::Exhigh).await?;
let stream = netease::stream::stream_audio(&client, info.playable()?, None).await?;
```

To log in the first time, drive `api::login`: `create` gives a `PendingLogin`,
`PendingLogin::qr_url` is the string to render as a QR code (rendering it is
your problem, not this crate's), and `wait` polls until the phone confirms.

## Three things that will cost you a day

### `.ncm` has nothing to do with this API

`/api/song/enhance/player/url/v1` returns **a plain mp3 or flac CDN URL** — an
ordinary HTTP file any player can stream, a XiaoAi speaker included. No NetEase
endpoint serves `.ncm`. That container is written *client-side* by the official
desktop app when it caches a download, and its encryption is unrelated to
anything here. If you arrive expecting to decrypt a response, there is nothing
to decrypt.

(`.ncm` files that already exist on disk are a separate matter: the
`xiaoai_llm` binary decrypts those on the fly with `ncmc_lib` while serving its
local library. That path never touches this crate.)

### Resolved URLs expire in about 20 minutes

The response's `expi` field is a **TTL in seconds**, typically `1200`, counted
from when the response arrived — not an absolute timestamp. After that the CDN
starts refusing the URL.

So: resolve just-in-time, immediately before playback, and never cache or
persist the result. Cache the song *id*, which is stable forever. A URL stored
in a playlist, a queue or a "recently played" table works perfectly in testing
and fails on the twenty-first minute. Because that is the dominant failure mode,
`stream` reports a 403/404 from the CDN as `NeteaseErr::UrlExpired` rather than
as a generic HTTP error.

### Not everything is playable

`url` comes back `null` for VIP-only, region-locked or withdrawn tracks;
`song_url` turns that into `SongUrlErr::Unavailable` (carrying `fee` and whether
a preview clip was offered) rather than a `None` waiting to be unwrapped. Handle
it by moving to the next search hit. Roughly, `standard`…`jyeffect` need VIP and
`sky`/`jymaster` need SVIP; without a logged-in session most tracks resolve at
low bitrate or not at all.

## Tests

```sh
cargo test -p netease
```

Offline and credential-free: every test runs against a local `axum` mock. The
real NetEase API is never called.
