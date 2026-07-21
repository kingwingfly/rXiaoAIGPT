//! The music directory, as a [`MusicSource`]. It shares the *same* [`Arc`] index
//! the HTTP server serves from, so the URLs it hands out resolve against the
//! same file set the router looks them up in.

use crate::music::{MusicIndex, is_ncm};
use brain::{BrainErr, MusicSource, Playable, Result, Track};
use ncmc_lib::NcmFile;
use regex::Regex;
use std::{path::Path, sync::Arc};

/// Name this source is known by, and the value written into [`Track::source`].
const NAME: &str = "local";

/// Cap on search hits: the model reads these, and every `.ncm` hit costs a
/// decrypt of its metadata block to describe.
const MAX_RESULTS: usize = 20;

/// A directory of audio files. `base_url` is the *speaker-facing* URL of our own
/// HTTP server — the device fetches the audio, so it must reach that address,
/// which behind a tunnel is unrelated to the address we bind.
#[derive(Debug)]
pub struct LocalSource {
    index: Arc<MusicIndex>,
    base_url: String,
}

impl LocalSource {
    pub fn new(index: Arc<MusicIndex>, base_url: impl Into<String>) -> Self {
        Self {
            index,
            base_url: base_url.into(),
        }
    }

    /// Describe an indexed path as a [`Track`], reading `.ncm` embedded metadata
    /// when present and falling back to the `Artist/Title.ext` layout otherwise.
    async fn describe(&self, rel: String) -> Track {
        if is_ncm(Path::new(&rel))
            && let Some(track) = self.describe_ncm(&rel).await
        {
            return track;
        }
        track_from_path(&rel)
    }

    /// `None` when the file is unreadable or not a valid container — a corrupt
    /// `.ncm` degrades to its filename rather than vanishing from results.
    async fn describe_ncm(&self, rel: &str) -> Option<Track> {
        let path = self.index.root().join(rel);
        // `NcmFile::open` does blocking file reads.
        let meta = tokio::task::spawn_blocking(move || {
            NcmFile::open(&path)
                .map(|ncm| ncm.meta().clone())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        let meta = match meta {
            Ok(meta) => meta,
            Err(e) => {
                tracing::debug!(path = %rel, error = %e, "unreadable ncm metadata");
                return None;
            }
        };
        Some(Track {
            id: rel.to_string(),
            title: meta.music_name,
            artist: meta
                .artist
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            source: NAME.to_string(),
            // NetEase records duration in ms (what `brain` wants); zero means unset.
            duration_ms: (meta.duration > 0).then_some(meta.duration as u64),
        })
    }
}

#[brain::async_trait]
impl MusicSource for LocalSource {
    fn name(&self) -> &str {
        NAME
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>> {
        if query.trim().is_empty() {
            return Err(BrainErr::InvalidArguments(
                "a search needs something to search for".into(),
            ));
        }
        let hits = self.index.find_all(&loose(query), MAX_RESULTS).await;
        let mut tracks = Vec::with_capacity(hits.len());
        for hit in hits {
            tracks.push(self.describe(hit).await);
        }
        Ok(tracks)
    }

    /// A URL, not a [`Playable::LocalFile`]: the speaker cannot see this
    /// filesystem, and an `.ncm` is only playable through the decrypting handler.
    async fn resolve(&self, track: &Track) -> Result<Playable> {
        if track.source != NAME {
            return Err(BrainErr::NotFound(format!(
                "{} is not a local track",
                track.id
            )));
        }
        // The id is the indexed path, matched exactly by the server before it
        // treats a request path as a pattern, so no escaping is needed.
        Ok(Playable::Url(format!(
            "{}/{}",
            self.base_url,
            urlencoding::encode(&track.id)
        )))
    }

    async fn random(&self, filter: Option<&str>) -> Result<Option<Track>> {
        let pattern = filter.map(loose);
        let Some(hit) = self.index.choose(pattern.as_ref()).await else {
            return Ok(None);
        };
        Ok(Some(self.describe(hit).await))
    }
}

/// A literal, case-insensitive matcher — the query is from speech, not a
/// trustworthy regex. `escape` leaves only the `(?i)` flag, so this cannot fail,
/// but user input must never panic.
fn loose(query: &str) -> Regex {
    Regex::new(&format!("(?i){}", regex::escape(query.trim())))
        .unwrap_or_else(|_| Regex::new("$^").expect("the empty matcher is valid"))
}

/// The library is laid out as `Artist/Title.ext`; a track loose at the root has
/// no known artist ([`Track::artist`] empty).
fn track_from_path(rel: &str) -> Track {
    let path = Path::new(rel);
    Track {
        id: rel.to_string(),
        title: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(rel)
            .to_string(),
        artist: path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string(),
        source: NAME.to_string(),
        // Reading a duration means decoding the file; not worth it per search hit.
        duration_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(dir: &Path) -> LocalSource {
        LocalSource::new(
            Arc::new(MusicIndex::new(dir.to_path_buf())),
            "http://10.0.0.1:3000",
        )
    }

    fn library() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("周杰伦")).unwrap();
        std::fs::write(dir.path().join("周杰伦/晴天.mp3"), b"audio").unwrap();
        std::fs::write(dir.path().join("Loose.mp3"), b"audio").unwrap();
        dir
    }

    #[tokio::test]
    async fn searches_by_title_and_by_artist() {
        let dir = library();
        let source = source(dir.path());

        let hits = source.search("晴天").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "晴天");
        assert_eq!(hits[0].artist, "周杰伦");
        assert_eq!(hits[0].source, "local");
        assert_eq!(hits[0].id, "周杰伦/晴天.mp3");

        assert_eq!(source.search("周杰伦").await.unwrap().len(), 1);
        assert!(source.search("不存在").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_is_case_insensitive_and_literal() {
        let dir = library();
        let source = source(dir.path());
        assert_eq!(source.search("loose").await.unwrap().len(), 1);
        assert!(source.search("loose(").await.unwrap().is_empty());
        assert!(matches!(
            source.search("  ").await,
            Err(BrainErr::InvalidArguments(_))
        ));
    }

    #[tokio::test]
    async fn a_track_at_the_root_has_no_artist() {
        let dir = library();
        let hits = source(dir.path()).search("Loose").await.unwrap();
        assert_eq!(hits[0].artist, "");
    }

    #[tokio::test]
    async fn resolve_points_at_our_own_server() {
        let dir = library();
        let source = source(dir.path());
        let track = source.search("晴天").await.unwrap().remove(0);
        let Playable::Url(url) = source.resolve(&track).await.unwrap() else {
            panic!("local tracks must resolve to a URL the speaker can fetch");
        };
        assert!(url.starts_with("http://10.0.0.1:3000/"), "{url}");
        assert!(
            url.contains(&urlencoding::encode("晴天").into_owned()),
            "{url}"
        );

        // A track from another source must not be claimed by this one.
        let foreign = Track {
            source: "netease".into(),
            ..track
        };
        assert!(matches!(
            source.resolve(&foreign).await,
            Err(BrainErr::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn random_respects_its_filter() {
        let dir = library();
        let source = source(dir.path());
        assert!(source.random(None).await.unwrap().is_some());
        assert_eq!(
            source.random(Some("周杰伦")).await.unwrap().unwrap().title,
            "晴天"
        );
        assert!(source.random(Some("无此歌手")).await.unwrap().is_none());
    }
}
