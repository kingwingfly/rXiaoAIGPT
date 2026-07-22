//! The XiaoAi speaker behind `brain`'s device traits: [`XiaoaiSpeaker`] is the
//! output ([`Speaker`]), [`XiaoaiSource`] the input ([`UtteranceSource`]).
//!
//! Three facts about the hardware shape everything here:
//! - **Polling only.** Xiaomi has no push API — nothing tells us the user spoke
//!   or a track ended, so we ask again every few seconds.
//! - **Xiaomi's own assistant answers first**, out loud, and its reply is
//!   reported as ordinary playback. We cut it short ([`XiaoaiSpeaker::hush`]) and
//!   wait out only audio *we* started (see [`XiaoaiSpeaker::we_are_playing`]).
//! - **TTS and music share one channel:** speech replaces a song rather than
//!   layering, and Xiaomi does not resume it — so the track is announced *before*
//!   it starts and the loop's after-the-fact confirmation is dropped (see
//!   [`XiaoaiSpeaker::suppress_next_say`]).

use anyhow::{Context as _, Result};
use brain::{BrainErr, Speaker, Utterance, UtteranceSource};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tracing::{debug, info, warn};
use xiaoai::{
    ApiCaller as _, Device, LastAskPayload, LastAskResponse, OpApi, OpPayloadBuilder, OpResponse,
    RecordApi, XiaoaiStatus, account::AuthData,
};

use crate::gate::{Decision, Gate};

/// The query poll adapts between these bounds. Xiaomi has no push API, so we ask
/// for the newest conversation record on a timer, and every poll is a request
/// billed to the account — an idle speaker should be asked less often. Start at
/// [`MIN_POLL_INTERVAL`], add [`POLL_BACKOFF`] after each poll that hears nothing
/// new (up to [`MAX_POLL_INTERVAL`]), and halve back toward the floor the moment a
/// new record appears, since activity predicts more activity. [`MIN_POLL_INTERVAL`]
/// also paces [`wait_while_playing`], where a fixed 3 s recheck is all a
/// track-ended test needs.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(3);
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(10);
const POLL_BACKOFF: Duration = Duration::from_secs(1);

/// The longest [`wait_while_playing`] blocks before resuming. If `play_url` is
/// accepted but the track never starts (a 404, an unreachable host), the device
/// sits in `Paused` forever; without this cap the wait loop would poll
/// `is_playing() == true` for good and the agent would never read history again.
/// The `last_seen` cursor still guards against replaying the original command.
const MAX_PLAYBACK_WAIT: Duration = Duration::from_secs(20 * 60);

/// The speaker. Holds credentials and the device id, not a connection — every
/// op is an independent request to `api2.mina.mi.com` — so it is shared as an
/// `Arc` by the loop, the tools and [`XiaoaiSource`] at once.
#[derive(Debug, Clone)]
pub struct XiaoaiSpeaker {
    auth_data: AuthData,
    device_id: String,
    /// Set whenever *we* put sound on the speaker; read-and-cleared by the poll
    /// loop. The device reports Xiaomi's own spoken reply as "playing" too, so a
    /// wait keyed on status alone would sit through it; this flag limits the wait
    /// to audio we started. `Arc` so every clone shares the one flag.
    we_are_playing: Arc<AtomicBool>,
    /// Set by [`Speaker::play`], consumed by the next [`Speaker::say`], to swallow
    /// the model's spoken confirmation of a track it just started. On this single
    /// channel that `say` would replace the music (which Xiaomi never resumes),
    /// so it is dropped — the music starting is the confirmation. `brain` cannot
    /// know this hardware fact, so the suppression lives here.
    suppress_next_say: Arc<AtomicBool>,
}

impl XiaoaiSpeaker {
    pub fn new(auth_data: AuthData, device: &Device) -> Self {
        Self {
            auth_data,
            device_id: device.device_id.clone(),
            we_are_playing: Arc::new(AtomicBool::new(false)),
            suppress_next_say: Arc::new(AtomicBool::new(false)),
        }
    }

    fn op(&self) -> OpPayloadBuilder {
        OpPayloadBuilder::new(&self.auth_data, &self.device_id)
    }

    /// Record that we started audio the poll loop should wait out.
    fn note_our_playback(&self) {
        self.we_are_playing.store(true, Ordering::SeqCst);
    }

    /// Whether we started audio since last asked, clearing the flag.
    fn took_our_playback(&self) -> bool {
        self.we_are_playing.swap(false, Ordering::SeqCst)
    }

    /// Arm the swallow-the-next-`say` flag. See [`Self::suppress_next_say`].
    fn arm_say_suppression(&self) {
        self.suppress_next_say.store(true, Ordering::SeqCst);
    }

    /// Whether this `say` should be dropped, clearing the flag.
    fn take_say_suppression(&self) -> bool {
        self.suppress_next_say.swap(false, Ordering::SeqCst)
    }

    /// Cancel a say-suppression no `say` consumed — if a play's turn spoke
    /// nothing, the armed flag would otherwise eat the *next* command's reply.
    /// The poll loop calls this when a fresh command is admitted.
    fn discard_suppressed_say(&self) {
        self.suppress_next_say.store(false, Ordering::SeqCst);
    }

    /// Cut off the built-in assistant's spoken reply. It runs through the same
    /// media player as music, so a `pause` silences it (a no-op when idle). Fired
    /// the instant a command is forwarded, so the user hears our answer, not both.
    async fn hush(&self) -> brain::Result<()> {
        let _: OpResponse = OpApi::request(self.op().pause())
            .await
            .map_err(BrainErr::backend)?;
        Ok(())
    }
}

impl Speaker for XiaoaiSpeaker {
    async fn say(&self, text: &str) -> brain::Result<()> {
        // A `say` right after a `play` is the confirmation of the just-started
        // track; speaking it would replace the music, so drop it.
        if self.take_say_suppression() {
            debug!(text, "dropping a reply that would talk over freshly-started music");
            return Ok(());
        }
        let _: OpResponse = OpApi::request(self.op().speak(text))
            .await
            .map_err(BrainErr::backend)?;
        // Our own reply is playback to wait out, or the next poll reads over it.
        self.note_our_playback();
        Ok(())
    }

    /// Point the speaker at `url`, returning once Xiaomi accepts the request;
    /// waiting for the track to finish is [`XiaoaiSource::next`]'s job. The pause
    /// first clears any native reply still playing, into which `play_url` is
    /// unreliable.
    async fn play(&self, url: &str) -> brain::Result<()> {
        let _: OpResponse = OpApi::request(self.op().pause())
            .await
            .map_err(BrainErr::backend)?;
        let _: OpResponse = OpApi::request(self.op().play_url(url))
            .await
            .map_err(BrainErr::backend)?;
        self.note_our_playback();
        self.arm_say_suppression();
        Ok(())
    }

    /// Announce the track, then play it — the single-channel version. Speech and
    /// music share one output and there is no "done speaking" signal (TTS is not
    /// the media player, which `hush` parks in `Paused`), so the only lever is
    /// time: speak, hold the channel for [`estimated_speech`] (generous — a beat
    /// of quiet is fine, a clipped song name is the bug), then start the track.
    /// No `pause` first: the native reply was hushed on forwarding, and the
    /// announcement is what we just waited out.
    async fn announce_then_play(&self, announcement: &str, url: &str) -> brain::Result<()> {
        let _: OpResponse = OpApi::request(self.op().speak(announcement))
            .await
            .map_err(BrainErr::backend)?;
        let hold = estimated_speech(announcement);
        debug!(?hold, announcement, "holding the channel for the announcement before playing");
        tokio::time::sleep(hold).await;
        let _: OpResponse = OpApi::request(self.op().play_url(url))
            .await
            .map_err(BrainErr::backend)?;
        self.note_our_playback();
        self.arm_say_suppression();
        Ok(())
    }

    async fn stop(&self) -> brain::Result<()> {
        // `pause`: the API has no stop opcode, and pausing an idle speaker is the
        // no-op `Speaker::stop` asks for.
        let _: OpResponse = OpApi::request(self.op().pause())
            .await
            .map_err(BrainErr::backend)?;
        Ok(())
    }

    async fn set_volume(&self, level: u8) -> brain::Result<()> {
        // Xiaomi's scale is also 0..=100; nothing to convert.
        let _: OpResponse = OpApi::request(self.op().volume(level as usize))
            .await
            .map_err(BrainErr::backend)?;
        Ok(())
    }

    /// Whether a playback session is in progress. **`Paused` counts as playing**
    /// — the caller is a wait loop, and treating a pause as "finished" would
    /// resume mid-song, the failure it exists to prevent.
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

/// The speaker's conversation history as a stream of utterances: polls the
/// newest record and yields it when [`Gate`] judges it new and worth a call.
pub struct XiaoaiSource {
    /// Shared with the loop and tools, so "is it still playing?" is asked of the
    /// device the tools just started.
    speaker: Arc<XiaoaiSpeaker>,
    auth_data: AuthData,
    device: Device,
    gate: Gate,
    /// Timestamp (ms) of the newest record dealt with. `None` until the first
    /// poll, which only adopts the cursor and acts on nothing — the newest record
    /// at startup may be hours old.
    last_seen: Option<usize>,
    /// Adaptive gap before the next history poll, kept within
    /// `MIN_POLL_INTERVAL..=MAX_POLL_INTERVAL`. Grows while the speaker is idle and
    /// halves back toward the floor on a new record — see [`XiaoaiSource::speed_up`].
    poll_interval: Duration,
}

impl XiaoaiSource {
    pub fn new(speaker: Arc<XiaoaiSpeaker>, auth_data: AuthData, device: Device) -> Self {
        Self {
            speaker,
            auth_data,
            device,
            gate: Gate::new(),
            last_seen: None,
            poll_interval: MIN_POLL_INTERVAL,
        }
    }

    /// A new record showed up: halve the gap toward the floor. Activity predicts
    /// more, so become responsive at once rather than one step at a time.
    fn speed_up(&mut self) {
        self.poll_interval = (self.poll_interval / 2).max(MIN_POLL_INTERVAL);
    }

    /// Nothing new this poll: back off one step, up to the ceiling.
    fn slow_down(&mut self) {
        self.poll_interval = (self.poll_interval + POLL_BACKOFF).min(MAX_POLL_INTERVAL);
    }

    /// The newest conversation record, as `(timestamp_ms, text)`.
    async fn latest(&self) -> Result<Option<(usize, String)>> {
        let payload = LastAskPayload::new(&self.auth_data, &self.device, 1);
        let resp: LastAskResponse = RecordApi::request(payload)
            .await
            .context("cannot read the conversation history")?;
        Ok(resp.first().map(|r| (r.time, r.query.clone())))
    }

    /// Decide what to do with the newest record.
    ///
    /// **Invariant: mark seen before acting.** The cursor advances the instant a
    /// record is recognised as new, not after it is handled — a command that
    /// times out, fails, or is dropped is still behind us, never re-read and
    /// retried forever. Pure and synchronous so this is testable without a device.
    fn admit(&mut self, time: usize, query: &str) -> Decision {
        match self.last_seen {
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

    /// Block until the speaker stops playing.
    ///
    /// **Invariant: never poll over our own playback.** While a track plays, the
    /// utterance that asked for it stays at the top of the history; any utterance
    /// transcribed meanwhile (including our announcements) would otherwise start a
    /// second turn and stop the track. Only entered when
    /// [`XiaoaiSpeaker::we_are_playing`] says the audio is ours.
    async fn wait_until_idle(&self) {
        wait_while_playing(self.speaker.as_ref()).await
    }
}

/// Roughly how long to hold the channel for the speaker to read `text` aloud —
/// a startup delay plus the reading. There is no "done speaking" signal to wait
/// on, so time is the only lever. The startup term is the larger and the reason:
/// the request is accepted well before speech begins, and firing `play_url` into
/// that gap clipped the announcement to one character. Generous and capped, and
/// counted in characters so Chinese is timed by spoken length.
fn estimated_speech(text: &str) -> Duration {
    /// Allowance for the request to land *and speech to begin*; undershooting clips.
    const STARTUP: Duration = Duration::from_millis(2500);
    /// Per character once reading has started; the surplus is margin against clipping.
    const PER_CHAR: Duration = Duration::from_millis(320);
    /// Past here, stop waiting.
    const CAP: Duration = Duration::from_secs(10);
    (STARTUP + PER_CHAR * text.chars().count() as u32).min(CAP)
}

/// The body of [`XiaoaiSource::wait_until_idle`], generic over the trait so it
/// can be tested against a fake. `?Sized` so an erased [`brain::DynSpeaker`] fits
/// too, though the caller passes the concrete speaker.
async fn wait_while_playing(speaker: &(impl Speaker + ?Sized)) {
    let mut waited = Duration::ZERO;
    loop {
        match speaker.is_playing().await {
            Ok(true) if waited < MAX_PLAYBACK_WAIT => {
                tokio::time::sleep(MIN_POLL_INTERVAL).await;
                waited += MIN_POLL_INTERVAL;
            }
            // Wedged past the cap (likely stuck in `Paused`): resume rather than
            // wait forever.
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

impl UtteranceSource for XiaoaiSource {
    /// The next utterance worth handling. Loops internally and never returns
    /// `None`: that would end the control loop permanently, and "nothing said in
    /// three seconds" is not exhaustion. A failed poll is retried, not surfaced.
    async fn next(&mut self) -> Option<Utterance> {
        loop {
            tokio::time::sleep(self.poll_interval).await;
            // Only block on playback *we* started; the native reply we cut short.
            if self.speaker.took_our_playback() {
                self.wait_until_idle().await;
            }

            let (time, query) = match self.latest().await {
                Ok(Some(record)) => record,
                Ok(None) => {
                    // Empty history: set the cursor now (the `0` sentinel is below
                    // any real timestamp) so the first real utterance is a command,
                    // not consumed as cursor init by `admit`.
                    self.last_seen.get_or_insert(0);
                    self.slow_down();
                    continue;
                }
                Err(e) => {
                    // Leave the cadence unchanged: a transient poll failure says
                    // nothing about how talkative the user is.
                    warn!(error = format!("{e:#}"), "poll failed");
                    continue;
                }
            };

            // Adapt before acting: a record past the cursor means the user is
            // active, so poll faster; anything else lets the gap grow.
            let is_new = matches!(self.last_seen, Some(seen) if time > seen);
            let decision = self.admit(time, &query);
            if is_new {
                self.speed_up();
            } else {
                self.slow_down();
            }
            if decision != Decision::Ignore {
                // A new command closes the previous play's confirmation window.
                self.speaker.discard_suppressed_say();
            }
            match decision {
                Decision::Ignore => continue,
                Decision::Reply(text) => {
                    info!(%query, "gate handled the utterance");
                    if let Err(e) = self.speaker.say(text).await {
                        warn!(error = %e, "could not answer the toggle");
                    }
                }
                Decision::Forward => {
                    // Silence Xiaomi's own reply before we take the turn.
                    if let Err(e) = self.speaker.hush().await {
                        warn!(error = %e, "could not hush the native assistant");
                    }
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

    /// A credential-free source: `admit` does no I/O, so it never reaches the
    /// network.
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

    /// The flag separating our audio from the native reply: nothing arms it on
    /// Xiaomi's behalf, so a fresh speaker is not waited on and one arm = one wait.
    #[test]
    fn we_wait_for_our_own_playback_exactly_once() {
        let speaker = source().speaker;
        assert!(!speaker.took_our_playback());
        speaker.note_our_playback();
        assert!(speaker.took_our_playback());
        assert!(!speaker.took_our_playback());
    }

    /// A `play` suppresses exactly one following `say` (its confirmation), and no
    /// more.
    #[test]
    fn a_play_suppresses_exactly_one_following_say() {
        let speaker = source().speaker;
        assert!(!speaker.take_say_suppression());
        speaker.arm_say_suppression();
        assert!(speaker.take_say_suppression());
        assert!(!speaker.take_say_suppression());
    }

    /// A play whose turn speaks nothing must not leave the flag armed to eat the
    /// next command's reply; admitting a new command clears it.
    #[test]
    fn a_play_that_never_spoke_does_not_swallow_the_next_reply() {
        let speaker = source().speaker;
        speaker.arm_say_suppression();
        speaker.discard_suppressed_say();
        assert!(!speaker.take_say_suppression());
    }

    /// Whatever was said before startup is history, not a command.
    #[test]
    fn the_first_poll_only_sets_the_cursor() {
        let mut source = source();
        assert_eq!(source.admit(100, "播放晴天"), Decision::Ignore);
        assert_eq!(source.last_seen, Some(100));
        assert_eq!(source.admit(200, "播放晴天"), Decision::Forward);
    }

    /// The invariant: a record is behind us as soon as it is seen, however it is
    /// then handled.
    #[test]
    fn a_record_is_marked_seen_even_when_it_is_dropped() {
        let mut source = source();
        source.admit(100, "");
        assert_eq!(source.admit(200, "嗯"), Decision::Ignore); // gate-filtered
        assert_eq!(source.last_seen, Some(200));
        assert_eq!(source.admit(200, "播放晴天"), Decision::Ignore);
    }

    /// With an empty history at startup, the cursor is set from the empty poll so
    /// the first real utterance is a command, not cursor setup.
    #[test]
    fn an_empty_history_does_not_eat_the_first_command() {
        let mut source = source();
        source.last_seen.get_or_insert(0);
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

    /// The adaptive cadence: idle polls add a second up to the 10 s ceiling; a new
    /// record halves the gap back toward the 3 s floor.
    #[test]
    fn the_poll_interval_backs_off_when_idle_and_snaps_back_on_activity() {
        let mut source = source();
        assert_eq!(source.poll_interval, Duration::from_secs(3));

        // One idle poll adds one second.
        source.slow_down();
        assert_eq!(source.poll_interval, Duration::from_secs(4));

        // It climbs a second at a time and holds at the ceiling.
        for _ in 0..20 {
            source.slow_down();
        }
        assert_eq!(source.poll_interval, Duration::from_secs(10));

        // Activity halves the gap; 2.5 s is clamped up to the 3 s floor.
        source.speed_up();
        assert_eq!(source.poll_interval, Duration::from_secs(5));
        source.speed_up();
        assert_eq!(source.poll_interval, Duration::from_secs(3));
        source.speed_up();
        assert_eq!(source.poll_interval, Duration::from_secs(3));
    }

    /// A fake device that reports a scripted sequence of playback states.
    struct FakeSpeaker {
        states: Mutex<Vec<bool>>,
        asked: Mutex<usize>,
    }

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

    /// The waiting invariant, against a fake. `start_paused` makes sleeps free.
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
    /// without the [`MAX_PLAYBACK_WAIT`] cap this test would never terminate.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_speaker_does_not_wait_forever() {
        struct Wedged;
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
