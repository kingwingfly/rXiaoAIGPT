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

## Logging in

A session is nothing but the `MUSIC_U` and `__csrf` cookies. Two ways to get them:

- **QR login** — drive `api::login`: `create` returns a `PendingLogin`,
  `PendingLogin::qr_url` is the string to render as a QR code (rendering it is
  your problem, not this crate's), and `wait` polls until the phone confirms.
- **Copy them from a logged-in browser** — at <https://music.163.com>, read the
  `MUSIC_U` cookie (and optionally `__csrf`) from DevTools → Cookies, then build
  the session by hand and persist it:

  ```rust ignore
  netease::Session::new(music_u, csrf).save("netease_session.json")?;
  ```

  Reads (search, URL resolution) only need `MUSIC_U`; `__csrf` may be `""` and is
  used only for writes.

## Three things that will cost you a day

### `.ncm` has nothing to do with this API

`/api/song/enhance/player/url/v1` returns **a plain mp3/flac CDN URL** any player
can stream. No endpoint serves `.ncm` — that container is written *client-side*
by the desktop app when it caches a download, and decrypting it is unrelated to
this crate. (The `xiaoai_llm` binary decrypts pre-existing `.ncm` files on disk
with `ncmc_lib`; that path never touches `netease`.)

### Resolved URLs expire in about 20 minutes

The response's `expi` field is a **TTL in seconds** (typically `1200`), counted
from when the response arrived. So resolve just-in-time, right before playback,
and never cache the URL — cache the song *id* instead, which is stable forever. A
URL stored in a playlist works in testing and fails on the twenty-first minute;
because that is the dominant failure mode, `stream` reports a CDN 403/404 as
`NeteaseErr::UrlExpired` rather than a generic HTTP error.

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
