//! The XiaoAi speaker behind `brain`'s two device traits.
//!
//! [`brain`] describes a device it may not name; this module is where that
//! description meets Xiaomi's cloud API. [`XiaoaiSpeaker`] is the output side
//! ([`Speaker`]) and [`XiaoaiSource`] the input side ([`UtteranceSource`]).
//! Both are thin — the interesting content is two invariants inherited from the
//! poll loop this replaced, documented on the methods that carry them.
//!
//! # Everything here is polling
//!
//! Xiaomi publishes no push API. There is no way to be told that the user said
//! something, or that a track finished; the only mechanism is asking again in a
//! moment. That single fact shapes both invariants below.

use anyhow::{Context as _, Result};
use brain::{BrainErr, Speaker, Utterance, UtteranceSource};
use std::{sync::Arc, time::Duration};
use tracing::{debug, info, warn};
use xiaoai::{
    ApiCaller as _, Device, LastAskPayload, LastAskResponse, OpApi, OpPayloadBuilder, OpResponse,
    RecordApi, XiaoaiStatus, account::AuthData,
};

use crate::gate::{Decision, Gate};

/// How often the conversation history and the playback status are asked for.
///
/// Three seconds is the compromise the original loop settled on: fast enough
/// that a spoken command does not feel ignored, slow enough that the account is
/// not hammered around the clock.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// The longest [`wait_while_playing`] will block before giving up and resuming.
///
/// A single playback session running past this is unusual; a speaker *wedged*
/// in `Paused` is the common case past it. Both [`Speaker::play`] and
/// [`Speaker::stop`] issue a pause, and if `play_url` is accepted but the track
/// never actually starts — a 404 on the URL, a host the speaker cannot reach —
/// the device sits in `Paused` with nothing playing and nothing to clear it.
/// Without a cap the wait loop below would then poll `is_playing() == true`
/// forever and the agent would never read its history again, unrecoverable
/// short of a restart. Capping trades a rare early resume — harmless, since the
/// `last_seen` cursor still guards against replaying the original command — for
/// never locking the agent out.
const MAX_PLAYBACK_WAIT: Duration = Duration::from_secs(20 * 60);

/// The speaker, as somewhere sound comes out.
///
/// Holds the credentials and the device id rather than a connection: every
/// operation is an independent HTTPS request to `api2.mina.mi.com`, so there is
/// no session to keep alive and the type is trivially shareable. It is normally
/// held as an `Arc` by three things at once — the control loop, the tools, and
/// [`XiaoaiSource`] — which works because `brain` implements [`Speaker`] for
/// `Arc<T>`.
#[derive(Debug, Clone)]
pub struct XiaoaiSpeaker {
    auth_data: AuthData,
    device_id: String,
}

impl XiaoaiSpeaker {
    pub fn new(auth_data: AuthData, device: &Device) -> Self {
        Self {
            auth_data,
            device_id: device.device_id.clone(),
        }
    }

    fn op(&self) -> OpPayloadBuilder {
        OpPayloadBuilder::new(&self.auth_data, &self.device_id)
    }
}

#[brain::async_trait]
impl Speaker for XiaoaiSpeaker {
    async fn say(&self, text: &str) -> brain::Result<()> {
        let _: OpResponse = OpApi::request(self.op().speak(text))
            .await
            .map_err(BrainErr::backend)?;
        Ok(())
    }

    /// Point the speaker at `url`.
    ///
    /// The pause first is not redundant: the speaker answers every utterance
    /// itself, so at the moment we act on "放首歌" it is quite likely still
    /// reading out its own reply, and `play_url` arriving mid-speech is
    /// unreliable.
    ///
    /// This returns as soon as Xiaomi has accepted the request, per
    /// [`Speaker::play`]'s contract. Waiting for the track to *finish* is
    /// [`XiaoaiSource::next`]'s job — see the invariant documented there.
    async fn play(&self, url: &str) -> brain::Result<()> {
        let _: OpResponse = OpApi::request(self.op().pause())
            .await
            .map_err(BrainErr::backend)?;
        let _: OpResponse = OpApi::request(self.op().play_url(url))
            .await
            .map_err(BrainErr::backend)?;
        Ok(())
    }

    async fn stop(&self) -> brain::Result<()> {
        // `pause` rather than a stop opcode: the API has no other, and pausing
        // an idle speaker is a no-op rather than an error, which is what
        // `Speaker::stop` asks for.
        let _: OpResponse = OpApi::request(self.op().pause())
            .await
            .map_err(BrainErr::backend)?;
        Ok(())
    }

    async fn set_volume(&self, level: u8) -> brain::Result<()> {
        // Xiaomi's scale is also 0..=100, so there is nothing to convert, and
        // `u8` cannot exceed it in the first place.
        let _: OpResponse = OpApi::request(self.op().volume(level as usize))
            .await
            .map_err(BrainErr::backend)?;
        Ok(())
    }

    /// Whether a playback session is in progress.
    ///
    /// **`Paused` counts as playing.** A paused track is still the current
    /// track, and the caller of this method is a wait loop: treating a pause as
    /// "finished" would let the loop resume mid-song, which is exactly the
    /// failure it exists to prevent. This mirrors the original `play_and_wait`,
    /// which waited for a status that was neither `Playing` nor `Paused`.
    async fn is_playing(&self) -> brain::Result<bool> {
        let resp: OpResponse = OpApi::request(self.op().status())
            .await
            .map_err(BrainErr::backend)?;
        Ok(matches!(
            resp.status(),
            XiaoaiStatus::Playing | XiaoaiStatus::Paused
        ))
    }
}

/// The speaker's conversation history, as a stream of utterances.
///
/// The device transcribes everything it hears and files it under the account;
/// this polls the newest record and turns it into a [`Utterance`] when it is
/// both new and worth a model call. Filtering is delegated to [`Gate`] — see
/// that module for why a filter, and not a parser, sits here.
pub struct XiaoaiSource {
    /// Shared with the control loop and the tools, so that "is it still
    /// playing?" is asked of the very device the tools just started.
    speaker: Arc<XiaoaiSpeaker>,
    auth_data: AuthData,
    device: Device,
    gate: Gate,
    /// Timestamp (ms) of the newest record already dealt with.
    ///
    /// `None` until the first successful poll: on startup the newest record is
    /// whatever was said to the speaker last — possibly hours ago, by someone
    /// who has since left the room — so the first poll only takes note of where
    /// the history has got to and acts on nothing.
    last_seen: Option<usize>,
}

impl XiaoaiSource {
    pub fn new(speaker: Arc<XiaoaiSpeaker>, auth_data: AuthData, device: Device) -> Self {
        Self {
            speaker,
            auth_data,
            device,
            gate: Gate::new(),
            last_seen: None,
        }
    }

    /// The newest conversation record, as `(timestamp_ms, text)`.
    async fn latest(&self) -> Result<Option<(usize, String)>> {
        let payload = LastAskPayload::new(&self.auth_data, &self.device, 1);
        let resp: LastAskResponse = RecordApi::request(payload)
            .await
            .context("cannot read the conversation history")?;
        Ok(resp.first().map(|r| (r.time, r.query.clone())))
    }

    /// Decide what to do with the newest record, and — crucially — *record that
    /// we have seen it before anything can go wrong*.
    ///
    /// # Invariant: mark seen before acting
    ///
    /// The cursor is advanced the instant a record is recognised as new, not
    /// after it has been handled successfully. If the model call times out, if
    /// a tool fails, if the gate drops it, the record is still behind us. The
    /// alternative shipped once: a command that failed was re-read three
    /// seconds later and failed again, forever, and every retry was a paid API
    /// call.
    ///
    /// Pure and synchronous so the invariant is testable without a speaker.
    fn admit(&mut self, time: usize, query: &str) -> Decision {
        match self.last_seen {
            // First poll: adopt the cursor, act on nothing.
            None => {
                self.last_seen = Some(time);
                debug!(time, "history cursor initialised");
                return Decision::Ignore;
            }
            Some(seen) if time <= seen => return Decision::Ignore,
            Some(_) => {}
        }
        self.last_seen = Some(time);
        self.gate.decide(query)
    }

    /// Block until the speaker has stopped playing.
    ///
    /// # Invariant: never poll over our own playback
    ///
    /// Once a track is playing, the top of the conversation history is still
    /// the utterance that asked for it, and it stays there for the length of
    /// the song. Polling through that is how the original agent got into a
    /// loop, and the `last_seen` cursor alone is not enough insurance: any
    /// utterance the speaker transcribes *while* the music plays (including its
    /// own announcements) would otherwise start a second turn on top of the
    /// first, stopping the track the user just asked for.
    ///
    /// A failure to read the status ends the wait rather than retrying
    /// forever — if the device is unreachable we cannot be interrupting its
    /// playback either.
    async fn wait_until_idle(&self) {
        wait_while_playing(self.speaker.as_ref()).await
    }
}

/// The body of [`XiaoaiSource::wait_until_idle`], over the trait rather than
/// the concrete device, so the invariant can be tested against a fake.
async fn wait_while_playing(speaker: &dyn Speaker) {
    let mut waited = Duration::ZERO;
    loop {
        match speaker.is_playing().await {
            Ok(true) if waited < MAX_PLAYBACK_WAIT => {
                tokio::time::sleep(POLL_INTERVAL).await;
                waited += POLL_INTERVAL;
            }
            // Still "playing" past the cap: treat the device as wedged (most
            // likely stuck in `Paused` after a track that never started) and
            // resume, rather than waiting on it forever.
            Ok(true) => {
                warn!(
                    "speaker still reports playing after {MAX_PLAYBACK_WAIT:?}; \
                     assuming it is wedged and resuming polling"
                );
                return;
            }
            Ok(false) => return,
            Err(e) => {
                warn!(error = %e, "cannot read playback status; assuming idle");
                return;
            }
        }
    }
}

#[brain::async_trait]
impl UtteranceSource for XiaoaiSource {
    /// The next utterance worth handling.
    ///
    /// Loops internally, because [`UtteranceSource::next`] returning `None`
    /// ends the control loop **permanently** and "nothing was said in the last
    /// three seconds" is not the end of anything. A failed poll is likewise
    /// retried rather than surfaced: the speaker being briefly offline, or the
    /// service token needing a refresh, must not shut the agent down. Hence
    /// this implementation never returns `None` at all — the process ends by
    /// being stopped, which is the only genuine exhaustion a live speaker has.
    async fn next(&mut self) -> Option<Utterance> {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            self.wait_until_idle().await;

            let (time, query) = match self.latest().await {
                Ok(Some(record)) => record,
                Ok(None) => {
                    // Empty history. Establish the cursor now, so the *first*
                    // thing said after startup is recognised as new. Otherwise
                    // `admit` would take its `None` branch on that first real
                    // record and consume it as cursor initialisation — dropping
                    // the user's opening command on a fresh or just-cleared
                    // device. (A device that already has history initialises the
                    // cursor in `admit` instead, deliberately skipping whatever
                    // predates startup; the `0` sentinel is below any real
                    // millisecond timestamp, so it never masks a genuine record.)
                    self.last_seen.get_or_insert(0);
                    continue;
                }
                Err(e) => {
                    warn!(error = format!("{e:#}"), "poll failed");
                    continue;
                }
            };

            match self.admit(time, &query) {
                Decision::Ignore => continue,
                Decision::Reply(text) => {
                    info!(%query, "gate handled the utterance");
                    if let Err(e) = self.speaker.say(text).await {
                        warn!(error = %e, "could not answer the toggle");
                    }
                }
                Decision::Forward => {
                    return Some(Utterance::new(time.to_string(), query, time as u64));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `admit` is where the "mark seen before acting" invariant lives, and it
    /// is deliberately free of I/O so it can be checked without a device. A
    /// source with no credentials never reaches the network as long as only
    /// `admit` is called.
    fn source() -> XiaoaiSource {
        let auth_data = AuthData {
            user_id: 0,
            device_id: String::new(),
            ssecurity: String::new(),
            service_token: String::new(),
            pass_token: String::new(),
        };
        let device = Device {
            alias: String::new(),
            device_id: String::new(),
            hardware: String::new(),
            others: serde_json::Value::Null,
        };
        XiaoaiSource::new(
            Arc::new(XiaoaiSpeaker::new(auth_data.clone(), &device)),
            auth_data,
            device,
        )
    }

    /// Whatever was said before the agent started is history, not a command.
    #[test]
    fn the_first_poll_only_sets_the_cursor() {
        let mut source = source();
        assert_eq!(source.admit(100, "播放晴天"), Decision::Ignore);
        assert_eq!(source.last_seen, Some(100));
        assert_eq!(source.admit(200, "播放晴天"), Decision::Forward);
    }

    /// The invariant: a record is behind us as soon as it is seen, whatever
    /// happens to it afterwards.
    #[test]
    fn a_record_is_marked_seen_even_when_it_is_dropped() {
        let mut source = source();
        source.admit(100, "");
        // Filtered out by the gate...
        assert_eq!(source.admit(200, "嗯"), Decision::Ignore);
        // ...and still never offered again.
        assert_eq!(source.last_seen, Some(200));
        assert_eq!(source.admit(200, "播放晴天"), Decision::Ignore);
    }

    /// On a device whose history is empty at startup, the cursor is set from
    /// the empty poll (what `next` does on `Ok(None)`), so the first real
    /// utterance is a command rather than being consumed as cursor setup.
    #[test]
    fn an_empty_history_does_not_eat_the_first_command() {
        let mut source = source();
        // What `next` now does when `latest()` returns nothing:
        source.last_seen.get_or_insert(0);
        // The next thing said is therefore a real command, not cursor init.
        assert_eq!(
            source.admit(1_700_000_000_000, "播放晴天"),
            Decision::Forward
        );
    }

    #[test]
    fn the_same_record_is_never_returned_twice() {
        let mut source = source();
        source.admit(100, "");
        assert_eq!(source.admit(200, "播放晴天"), Decision::Forward);
        assert_eq!(source.admit(200, "播放晴天"), Decision::Ignore);
        assert_eq!(source.admit(150, "播放晴天"), Decision::Ignore);
    }

    /// A fake device that reports a scripted sequence of playback states.
    struct FakeSpeaker {
        states: Mutex<Vec<bool>>,
        asked: Mutex<usize>,
    }

    #[brain::async_trait]
    impl Speaker for FakeSpeaker {
        async fn say(&self, _text: &str) -> brain::Result<()> {
            Ok(())
        }
        async fn play(&self, _url: &str) -> brain::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> brain::Result<()> {
            Ok(())
        }
        async fn set_volume(&self, _level: u8) -> brain::Result<()> {
            Ok(())
        }
        async fn is_playing(&self) -> brain::Result<bool> {
            *self.asked.lock().unwrap() += 1;
            let mut states = self.states.lock().unwrap();
            Ok(if states.is_empty() {
                false
            } else {
                states.remove(0)
            })
        }
    }

    /// The waiting half of the second invariant, checked against the trait
    /// rather than the real device. `start_paused` makes the sleeps free.
    #[tokio::test(start_paused = true)]
    async fn waiting_blocks_until_playback_ends() {
        let speaker = FakeSpeaker {
            states: Mutex::new(vec![true, true, false]),
            asked: Mutex::new(0),
        };
        wait_while_playing(&speaker).await;
        assert_eq!(*speaker.asked.lock().unwrap(), 3);
    }

    /// An unreachable speaker must not become an infinite wait.
    #[tokio::test(start_paused = true)]
    async fn a_broken_status_ends_the_wait() {
        struct Broken;
        #[brain::async_trait]
        impl Speaker for Broken {
            async fn say(&self, _: &str) -> brain::Result<()> {
                Ok(())
            }
            async fn play(&self, _: &str) -> brain::Result<()> {
                Ok(())
            }
            async fn stop(&self) -> brain::Result<()> {
                Ok(())
            }
            async fn set_volume(&self, _: u8) -> brain::Result<()> {
                Ok(())
            }
            async fn is_playing(&self) -> brain::Result<bool> {
                Err(BrainErr::Backend("offline".into()))
            }
        }
        wait_while_playing(&Broken).await;
    }

    /// A speaker wedged reporting "playing" forever must not hang the wait —
    /// otherwise the agent never reads its history again. Without the
    /// [`MAX_PLAYBACK_WAIT`] cap this test would never terminate.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_speaker_does_not_wait_forever() {
        struct Wedged;
        #[brain::async_trait]
        impl Speaker for Wedged {
            async fn say(&self, _: &str) -> brain::Result<()> {
                Ok(())
            }
            async fn play(&self, _: &str) -> brain::Result<()> {
                Ok(())
            }
            async fn stop(&self) -> brain::Result<()> {
                Ok(())
            }
            async fn set_volume(&self, _: u8) -> brain::Result<()> {
                Ok(())
            }
            async fn is_playing(&self) -> brain::Result<bool> {
                Ok(true)
            }
        }
        wait_while_playing(&Wedged).await;
    }
}
