//! The music directory, as a [`MusicSource`].
//!
//! This is the same index the HTTP server serves from — deliberately the *same*
//! [`Arc`], not a second one: [`LocalSource::resolve`] hands out a URL whose
//! path is looked up again by [`crate::music::router_with`], so the two must
//! agree on what exists.

use crate::music::{MusicIndex, is_ncm};
use brain::{BrainErr, MusicSource, Playable, Result, Track};
use ncmc_lib::NcmFile;
use regex::Regex;
use std::{path::Path, sync::Arc};

/// Name this source is known by, and the value written into [`Track::source`].
const NAME: &str = "local";

/// Cap on how many hits a search reports. The model reads these; a thousand
/// filenames would be neither useful to it nor cheap to describe, since every
/// `.ncm` hit costs a decrypt of its metadata block.
const MAX_RESULTS: usize = 20;

/// A directory of audio files, exposed to the intent layer.
///
/// `base_url` is the *speaker-facing* URL of our own HTTP server (see
/// [`crate::config::Config::base_url`]) — the device fetches the audio itself,
/// so it must be an address the device can reach. It is taken in the
/// constructor rather than derived here because behind a tunnel it has nothing
/// to do with the address we bind.
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

    /// The index this source shares with the HTTP router.
    pub fn index(&self) -> &Arc<MusicIndex> {
        &self.index
    }

    /// Describe an indexed path as a [`Track`].
    ///
    /// `.ncm` files carry NetEase's own metadata, which is far better than
    /// anything a filename can say, so it is read when present; everything else
    /// falls back to the `Artist/Title.ext` layout the library uses.
    async fn describe(&self, rel: String) -> Track {
        if is_ncm(Path::new(&rel))
            && let Some(track) = self.describe_ncm(&rel).await
        {
            return track;
        }
        track_from_path(&rel)
    }

    /// `None` when the file is not readable or not a valid container — a
    /// corrupt `.ncm` should degrade to its filename, not vanish from results.
    async fn describe_ncm(&self, rel: &str) -> Option<Track> {
        let path = self.index.root().join(rel);
        // `NcmFile::open` parses the header, key, metadata *and* embedded cover
        // art before returning, all with blocking file reads.
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
            // NetEase records the duration in milliseconds, which is what
            // `brain` wants; zero means "not recorded".
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

    /// A URL rather than a [`Playable::LocalFile`]: the speaker is on the other
    /// end of a network and cannot see this filesystem, and for an `.ncm` the
    /// file on disk is not even playable — only the server's decrypting handler
    /// makes it so.
    async fn resolve(&self, track: &Track) -> Result<Playable> {
        if track.source != NAME {
            return Err(BrainErr::NotFound(format!(
                "{} is not a local track",
                track.id
            )));
        }
        // The id *is* the indexed path, which the server matches exactly before
        // it considers treating a request path as a pattern — so no escaping is
        // needed and a filename full of regex metacharacters still resolves to
        // itself.
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

/// Turn a spoken phrase into a permissive matcher.
///
/// The query reaches us from speech recognition by way of a model, so it is
/// neither a trustworthy regex nor reliably cased. A literal, case-insensitive
/// match is the honest reading; the regex machinery is only here because the
/// index is keyed by path.
fn loose(query: &str) -> Regex {
    Regex::new(&format!("(?i){}", regex::escape(query.trim())))
        // `escape` guarantees the only metacharacter left is the `(?i)` flag,
        // so this cannot fail — but a panic on user input is never acceptable.
        .unwrap_or_else(|_| Regex::new("$^").expect("the empty matcher is valid"))
}

/// The library is laid out as `Artist/Title.ext`, so the parent directory is
/// the artist and the file stem the title. A track sitting loose at the root
/// simply has no known artist, which [`Track::artist`] documents as empty.
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
        // Reading a duration means decoding the file; nothing needs it badly
        // enough to pay that on every search hit.
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

    /// Casing survives speech-to-text unpredictably, and a stray metacharacter
    /// must be matched literally rather than blowing up the regex.
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

    /// The whole point of the trait: the intent layer holds sources it cannot
    /// name the type of.
    #[test]
    fn is_usable_as_a_trait_object() {
        let dir = library();
        let sources: Vec<Box<dyn MusicSource>> = vec![Box::new(source(dir.path()))];
        assert_eq!(sources[0].name(), "local");
    }
}
