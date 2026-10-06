//! Live spectrum feed for the LED visualizer.
//!
//! One shared `cliamp visstream --fps 15` child is fanned out to every
//! `GET /api/vis` subscriber through a [`broadcast`] channel. The child runs
//! only while at least one subscriber is connected AND the player is
//! "playing"; it is killed as soon as either stops being true. While the
//! player is not playing, each subscriber gets a single all-zero frame (on
//! connect, and again whenever the player stops) and then silence.
//!
//! The child is spawned through the [`VisSource`] trait so tests can inject a
//! fake frame source and never start a real process.

use crate::cliamp::Player;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio::time::{interval, sleep_until, Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

/// How many bands cliamp emits per frame.
pub const BAND_COUNT: usize = 10;

/// Frame rate requested from `cliamp visstream`.
pub const SOURCE_FPS: u32 = 15;

/// Capacity of the shared broadcast channel. Kept small on purpose: a lagging
/// subscriber skips ahead to the newest frames instead of replaying a backlog.
const BROADCAST_CAPACITY: usize = 4;

/// One spectrum frame, serialized as `{"bands":[...]}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VisFrame {
    /// Band levels exactly as cliamp emitted them (0..1, already clamped).
    pub bands: Vec<f64>,
}

impl VisFrame {
    /// The all-zero frame sent while nothing is playing.
    pub fn silence() -> Self {
        Self {
            bands: vec![0.0; BAND_COUNT],
        }
    }
}

/// The subset of a `cliamp visstream` NDJSON line that radio-web cares about.
#[derive(Debug, Deserialize)]
struct VisLine {
    ok: Option<bool>,
    bands: Option<Vec<f64>>,
}

/// Parse one `cliamp visstream` line. Returns `None` for anything malformed:
/// invalid JSON, `"ok":false`, a missing `bands` array, or a band count other
/// than [`BAND_COUNT`]. Band values are passed through untouched.
pub fn parse_vis_line(line: &str) -> Option<VisFrame> {
    let parsed: VisLine = serde_json::from_str(line.trim()).ok()?;
    if parsed.ok == Some(false) {
        return None;
    }
    let bands = parsed.bands?;
    if bands.len() != BAND_COUNT {
        return None;
    }
    Some(VisFrame { bands })
}

/// A running frame producer (normally a `cliamp visstream` child).
/// Dropping the feed must stop the underlying producer.
#[async_trait::async_trait]
pub trait VisFeed: Send {
    /// The next raw line, or `None` once the producer has ended.
    /// Must be cancel-safe.
    async fn next_line(&mut self) -> Option<String>;
}

/// Starts frame producers.
pub trait VisSource: Send + Sync {
    /// Start a fresh producer.
    fn spawn(&self) -> anyhow::Result<Box<dyn VisFeed>>;
}

/// [`VisSource`] that runs `cliamp visstream --fps 15`. The child inherits the
/// environment (notably `CLIAMP_CONFIG_DIR`) like every other cliamp call.
pub struct CliampVisSource {
    cliamp_bin: String,
}

impl CliampVisSource {
    /// Create a source that runs `cliamp_bin visstream`.
    pub fn new(cliamp_bin: String) -> Self {
        Self { cliamp_bin }
    }
}

impl VisSource for CliampVisSource {
    fn spawn(&self) -> anyhow::Result<Box<dyn VisFeed>> {
        let mut child = Command::new(&self.cliamp_bin)
            .args(["visstream", "--fps", &SOURCE_FPS.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("cliamp visstream has no stdout"))?;
        Ok(Box::new(CliampVisFeed {
            _child: child,
            lines: BufReader::new(stdout).lines(),
        }))
    }
}

/// A live `cliamp visstream` child. Dropping it kills the process.
struct CliampVisFeed {
    _child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

#[async_trait::async_trait]
impl VisFeed for CliampVisFeed {
    async fn next_line(&mut self) -> Option<String> {
        self.lines.next_line().await.ok().flatten()
    }
}

/// Timing knobs for the hub. Production uses `Default`; tests shrink them.
#[derive(Debug, Clone, Copy)]
pub struct VisConfig {
    /// How often the supervisor re-checks subscribers and player state.
    pub poll_interval: Duration,
    /// First delay before restarting a child that died.
    pub restart_backoff_initial: Duration,
    /// Ceiling for the doubling restart delay.
    pub restart_backoff_max: Duration,
    /// Minimum spacing between two frames sent to one client (15 fps cap).
    pub client_min_interval: Duration,
}

impl Default for VisConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            restart_backoff_initial: Duration::from_secs(1),
            restart_backoff_max: Duration::from_secs(30),
            client_min_interval: Duration::from_micros(1_000_000 / SOURCE_FPS as u64),
        }
    }
}

/// Shared spectrum hub: owns the single child process and the fan-out.
pub struct VisHub {
    source: Box<dyn VisSource>,
    player: Arc<dyn Player>,
    config: VisConfig,
    frames: broadcast::Sender<VisFrame>,
    subscribers: AtomicUsize,
    /// Set by a subscriber that saw "playing" and so was sent no silence frame.
    /// The supervisor consumes it before reading the player: if playback has
    /// stopped by then, it owes that subscriber a silence frame.
    unconfirmed_playing: AtomicBool,
    wake: Notify,
}

/// Keeps the subscriber count honest: decrements and wakes the supervisor on drop.
struct Subscription {
    hub: Arc<VisHub>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.subscribers.fetch_sub(1, Ordering::SeqCst);
        self.hub.wake.notify_one();
    }
}

impl VisHub {
    /// Create the hub and start its supervisor task. Must be called inside a
    /// tokio runtime. No child process starts until someone subscribes while
    /// the player is playing.
    pub fn start(
        source: Box<dyn VisSource>,
        player: Arc<dyn Player>,
        config: VisConfig,
    ) -> Arc<Self> {
        let (frames, _) = broadcast::channel(BROADCAST_CAPACITY);
        let hub = Arc::new(Self {
            source,
            player,
            config,
            frames,
            subscribers: AtomicUsize::new(0),
            unconfirmed_playing: AtomicBool::new(false),
            wake: Notify::new(),
        });
        let supervisor = Supervisor::new(hub.clone());
        tokio::spawn(supervisor.run());
        hub
    }

    /// Number of currently connected subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.load(Ordering::SeqCst)
    }

    /// Subscribe to the feed. The returned receiver yields throttled frames
    /// (at most one per `client_min_interval`) and closes when dropped. If the
    /// player is not playing, the first frame is the all-zero frame.
    pub async fn subscribe(self: &Arc<Self>) -> mpsc::Receiver<VisFrame> {
        // Register before checking the player so a stop between the check and
        // the first frame is not missed.
        self.subscribers.fetch_add(1, Ordering::SeqCst);
        let subscription = Subscription { hub: self.clone() };
        let frames = self.frames.subscribe();
        self.wake.notify_one();

        let playing = self.player.state().await.state == "playing";
        let initial = (!playing).then(VisFrame::silence);
        if playing {
            // Playback may stop before the supervisor's next reconcile, which
            // would see "stopped" with nothing to compare against. Flag it so
            // that reconcile broadcasts silence to this new client too.
            self.unconfirmed_playing.store(true, Ordering::SeqCst);
            self.wake.notify_one();
        }

        let (out, client_frames) = mpsc::channel(1);
        tokio::spawn(run_client(
            frames,
            out,
            initial,
            self.config.client_min_interval,
            subscription,
        ));
        client_frames
    }
}

/// Per-client loop: forwards the newest frame at most once per `min_interval`.
///
/// Broadcast reads never stop, even while the client is backpressured or the
/// rate limit is holding a frame back: every newer frame replaces the pending
/// one, so the newest frame always wins (a silence frame is never displaced by
/// an older one). Ends as soon as the client side of `out` is dropped.
async fn run_client(
    mut frames: broadcast::Receiver<VisFrame>,
    out: mpsc::Sender<VisFrame>,
    initial: Option<VisFrame>,
    min_interval: Duration,
    _subscription: Subscription,
) {
    let mut pending = initial;
    let mut next_allowed = Instant::now();
    loop {
        let may_send = pending.is_some() && Instant::now() >= next_allowed;
        tokio::select! {
            _ = out.closed() => return,
            received = frames.recv() => match received {
                Ok(frame) => pending = Some(frame),
                // Lagging: skip ahead, the next recv returns the oldest kept frame.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            // Waiting for room in `out` does not block the arm above, so a
            // slow consumer cannot pin a stale frame.
            permit = out.reserve(), if may_send => {
                let Ok(permit) = permit else { return };
                if let Some(frame) = pending.take() {
                    permit.send(frame);
                    next_allowed = Instant::now() + min_interval;
                }
            }
            _ = sleep_until(next_allowed), if pending.is_some() && !may_send => {}
        }
    }
}

/// Resolves to the next line of `feed`, or never if there is no feed.
async fn next_feed_line(feed: &mut Option<Box<dyn VisFeed>>) -> Option<String> {
    match feed {
        Some(active) => active.next_line().await,
        None => std::future::pending().await,
    }
}

/// Owns the child process and decides when it should run.
struct Supervisor {
    hub: Arc<VisHub>,
    feed: Option<Box<dyn VisFeed>>,
    backoff: Duration,
    /// Earliest time a dead child may be restarted.
    retry_at: Option<Instant>,
    /// Whether the last reconcile saw the player playing.
    was_playing: bool,
}

impl Supervisor {
    fn new(hub: Arc<VisHub>) -> Self {
        let backoff = hub.config.restart_backoff_initial;
        Self {
            hub,
            feed: None,
            backoff,
            retry_at: None,
            was_playing: false,
        }
    }

    async fn run(mut self) {
        let mut ticker = interval(self.hub.config.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                line = next_feed_line(&mut self.feed) => {
                    self.handle_line(line);
                    continue;
                }
                _ = ticker.tick() => {}
                _ = self.hub.wake.notified() => {}
            }
            self.reconcile().await;
        }
    }

    /// Relay one line from the child, or schedule a restart if it ended.
    fn handle_line(&mut self, line: Option<String>) {
        let Some(line) = line else {
            self.feed = None;
            self.schedule_retry();
            warn!("cliamp visstream ended, restarting in {:?}", self.backoff);
            return;
        };
        let Some(frame) = parse_vis_line(&line) else {
            debug!("skipping malformed visstream line");
            return;
        };
        self.backoff = self.hub.config.restart_backoff_initial;
        // Err just means nobody is listening right now.
        let _ = self.hub.frames.send(frame);
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(self.hub.config.restart_backoff_max);
    }

    fn stop_child(&mut self) {
        if self.feed.take().is_some() {
            info!("stopped cliamp visstream");
        }
        self.retry_at = None;
        self.backoff = self.hub.config.restart_backoff_initial;
    }

    /// Bring the child in line with "subscribers > 0 AND player playing".
    async fn reconcile(&mut self) {
        if self.hub.subscriber_count() == 0 {
            self.stop_child();
            self.was_playing = false;
            // Deliberately leave `unconfirmed_playing` alone: a subscriber may
            // have registered and set it after the count above was read, and
            // clearing it here would lose that subscriber's silence. A stale
            // flag only costs one redundant silence frame.
            return;
        }

        // Consume the flag before reading the player: a subscriber that sets it
        // after this point is handled by the next reconcile, which it wakes.
        let unconfirmed = self.hub.unconfirmed_playing.swap(false, Ordering::SeqCst);
        let playing = self.hub.player.state().await.state == "playing";
        if !playing {
            self.stop_child();
            if self.was_playing || unconfirmed {
                // Child is gone first, so no stale frame can follow this one.
                let _ = self.hub.frames.send(VisFrame::silence());
            }
            self.was_playing = false;
            return;
        }
        self.was_playing = true;

        if self.feed.is_some() {
            return;
        }
        if self.retry_at.is_some_and(|retry_at| Instant::now() < retry_at) {
            return;
        }
        match self.hub.source.spawn() {
            Ok(feed) => {
                info!("started cliamp visstream");
                self.feed = Some(feed);
                self.retry_at = None;
            }
            Err(error) => {
                warn!("failed to start cliamp visstream: {}", error);
                self.schedule_retry();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cliamp::PlayerState;
    use std::sync::Mutex;
    use tokio::sync::watch;
    use tokio::time::timeout;

    /// Player whose state the test sets directly.
    struct FakePlayer {
        state: Mutex<String>,
        watch_tx: watch::Sender<PlayerState>,
    }

    impl FakePlayer {
        fn new(state: &str) -> Arc<Self> {
            let (watch_tx, _) = watch::channel(Self::snapshot(state));
            Arc::new(Self {
                state: Mutex::new(state.to_string()),
                watch_tx,
            })
        }

        fn snapshot(state: &str) -> PlayerState {
            PlayerState {
                state: state.to_string(),
                url: None,
                station_url: None,
                title: None,
            }
        }

        fn set(&self, state: &str) {
            *self.state.lock().unwrap() = state.to_string();
        }
    }

    #[async_trait::async_trait]
    impl Player for FakePlayer {
        async fn play(&self, _url: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn state(&self) -> PlayerState {
            Self::snapshot(&self.state.lock().unwrap())
        }
        fn subscribe(&self) -> watch::Receiver<PlayerState> {
            self.watch_tx.subscribe()
        }
    }

    /// Frame source whose lines the test feeds by hand.
    #[derive(Default)]
    struct FakeSource {
        spawn_count: AtomicUsize,
        active: Arc<AtomicUsize>,
        senders: Mutex<Vec<Option<mpsc::UnboundedSender<String>>>>,
    }

    impl FakeSource {
        fn spawned(&self) -> usize {
            self.spawn_count.load(Ordering::SeqCst)
        }

        fn active(&self) -> usize {
            self.active.load(Ordering::SeqCst)
        }

        /// Feed a raw line to the child with the given spawn index.
        fn send(&self, child: usize, line: &str) {
            let senders = self.senders.lock().unwrap();
            senders[child].as_ref().unwrap().send(line.to_string()).unwrap();
        }

        /// Make the child with the given spawn index end on its own.
        fn end(&self, child: usize) {
            self.senders.lock().unwrap()[child] = None;
        }
    }

    struct FakeFeed {
        lines: mpsc::UnboundedReceiver<String>,
        active: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl VisFeed for FakeFeed {
        async fn next_line(&mut self) -> Option<String> {
            self.lines.recv().await
        }
    }

    impl Drop for FakeFeed {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Lets the hub own a boxed source while the test keeps a handle to it.
    struct SharedSource(Arc<FakeSource>);

    impl VisSource for SharedSource {
        fn spawn(&self) -> anyhow::Result<Box<dyn VisFeed>> {
            let (line_tx, lines) = mpsc::unbounded_channel();
            self.0.senders.lock().unwrap().push(Some(line_tx));
            self.0.spawn_count.fetch_add(1, Ordering::SeqCst);
            self.0.active.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeFeed {
                lines,
                active: self.0.active.clone(),
            }))
        }
    }

    fn fast_config() -> VisConfig {
        VisConfig {
            poll_interval: Duration::from_millis(10),
            restart_backoff_initial: Duration::from_millis(10),
            restart_backoff_max: Duration::from_millis(40),
            client_min_interval: Duration::ZERO,
        }
    }

    fn start_hub(
        player_state: &str,
        config: VisConfig,
    ) -> (Arc<VisHub>, Arc<FakeSource>, Arc<FakePlayer>) {
        let source = Arc::new(FakeSource::default());
        let player = FakePlayer::new(player_state);
        let hub = VisHub::start(
            Box::new(SharedSource(source.clone())),
            player.clone() as Arc<dyn Player>,
            config,
        );
        (hub, source, player)
    }

    async fn wait_until(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn recv_frame(frames: &mut mpsc::Receiver<VisFrame>) -> VisFrame {
        timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("timed out waiting for a frame")
            .expect("frame channel closed")
    }

    async fn assert_no_frame(frames: &mut mpsc::Receiver<VisFrame>, within: Duration) {
        let received = timeout(within, frames.recv()).await;
        assert!(received.is_err(), "unexpected frame: {received:?}");
    }

    const LINE_A: &str =
        r#"{"ok":true,"visualizer":"Bars","bands":[0.32,0.47,0.33,0.35,0.34,0.28,0.16,0,0,0]}"#;
    const BANDS_A: [f64; 10] = [0.32, 0.47, 0.33, 0.35, 0.34, 0.28, 0.16, 0.0, 0.0, 0.0];

    #[test]
    fn parses_a_cliamp_line_exactly() {
        let frame = parse_vis_line(LINE_A).unwrap();
        assert_eq!(frame.bands, BANDS_A);
        assert_eq!(
            serde_json::to_string(&VisFrame::silence()).unwrap(),
            r#"{"bands":[0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}"#
        );
    }

    #[test]
    fn rejects_malformed_lines() {
        for line in [
            "",
            "not json",
            r#"{"ok":false,"bands":[0,0,0,0,0,0,0,0,0,0]}"#,
            r#"{"ok":true}"#,
            r#"{"ok":true,"bands":[0.1,0.2]}"#,
            r#"{"ok":true,"bands":"x"}"#,
            r#"{"ok":true,"bands":[0,0,0,0,0,0,0,0,0,0,0]}"#,
        ] {
            assert!(parse_vis_line(line).is_none(), "accepted {line:?}");
        }
    }

    #[tokio::test]
    async fn stopped_player_gets_one_zero_frame_then_nothing() {
        let (hub, source, _player) = start_hub("stopped", fast_config());
        let mut frames = hub.subscribe().await;

        assert_eq!(recv_frame(&mut frames).await, VisFrame::silence());
        assert_no_frame(&mut frames, Duration::from_millis(150)).await;
        assert_eq!(source.spawned(), 0);
    }

    #[tokio::test]
    async fn frames_are_relayed_while_playing() {
        let (hub, source, _player) = start_hub("playing", fast_config());
        let mut frames = hub.subscribe().await;

        wait_until("child spawn", || source.spawned() == 1).await;
        source.send(0, LINE_A);

        // No leading zero frame while playing: the first frame is the real one.
        assert_eq!(recv_frame(&mut frames).await.bands, BANDS_A);
    }

    #[tokio::test]
    async fn child_is_not_started_without_subscribers() {
        let (hub, source, _player) = start_hub("playing", fast_config());

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(source.spawned(), 0);

        // Starts for a subscriber, and is killed again when the last one leaves.
        let frames = hub.subscribe().await;
        wait_until("child spawn", || source.active() == 1).await;
        drop(frames);
        wait_until("child kill", || source.active() == 0).await;
        assert_eq!(hub.subscriber_count(), 0);
        assert_eq!(source.spawned(), 1);
    }

    #[tokio::test]
    async fn child_is_stopped_when_the_player_stops() {
        let (hub, source, player) = start_hub("playing", fast_config());
        let mut frames = hub.subscribe().await;
        wait_until("child spawn", || source.active() == 1).await;
        source.send(0, LINE_A);
        assert_eq!(recv_frame(&mut frames).await.bands, BANDS_A);

        player.set("stopped");

        wait_until("child kill", || source.active() == 0).await;
        assert_eq!(recv_frame(&mut frames).await, VisFrame::silence());
        assert_no_frame(&mut frames, Duration::from_millis(150)).await;
        assert_eq!(source.spawned(), 1, "must not restart while stopped");
    }

    #[tokio::test]
    async fn malformed_lines_are_skipped() {
        let (hub, source, _player) = start_hub("playing", fast_config());
        let mut frames = hub.subscribe().await;
        wait_until("child spawn", || source.spawned() == 1).await;

        source.send(0, "garbage");
        source.send(0, r#"{"ok":true}"#);
        source.send(0, r#"{"ok":true,"bands":[0.1,0.2]}"#);
        source.send(0, r#"{"ok":false,"bands":[0,0,0,0,0,0,0,0,0,0]}"#);
        source.send(0, LINE_A);

        assert_eq!(recv_frame(&mut frames).await.bands, BANDS_A);
        assert_no_frame(&mut frames, Duration::from_millis(100)).await;
        assert_eq!(source.spawned(), 1, "bad lines must not restart the child");
    }

    #[tokio::test]
    async fn dead_child_is_restarted_while_conditions_hold() {
        let (hub, source, _player) = start_hub("playing", fast_config());
        let mut frames = hub.subscribe().await;
        wait_until("first spawn", || source.spawned() == 1).await;

        source.end(0);

        wait_until("restart", || source.spawned() == 2).await;
        source.send(1, LINE_A);
        assert_eq!(recv_frame(&mut frames).await.bands, BANDS_A);
    }

    #[tokio::test]
    async fn client_rate_is_capped_and_keeps_the_newest_frame() {
        let config = VisConfig {
            client_min_interval: Duration::from_millis(200),
            ..fast_config()
        };
        let (hub, source, _player) = start_hub("playing", config);
        let mut frames = hub.subscribe().await;
        wait_until("child spawn", || source.spawned() == 1).await;

        let line = |level: f64| format!(r#"{{"ok":true,"bands":[{level},0,0,0,0,0,0,0,0,0]}}"#);
        source.send(0, &line(0.1));
        assert_eq!(recv_frame(&mut frames).await.bands[0], 0.1);

        // A burst inside the 200 ms window collapses into its newest frame.
        for level in [0.2, 0.3, 0.4, 0.5] {
            source.send(0, &line(level));
        }
        let started = Instant::now();
        assert_eq!(recv_frame(&mut frames).await.bands[0], 0.5);
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_no_frame(&mut frames, Duration::from_millis(100)).await;
    }

    /// Drain whatever the client has queued, waiting briefly for stragglers.
    async fn drain(frames: &mut mpsc::Receiver<VisFrame>) -> Vec<VisFrame> {
        let mut received = Vec::new();
        while let Ok(Some(frame)) = timeout(Duration::from_millis(150), frames.recv()).await {
            received.push(frame);
        }
        received
    }

    #[tokio::test]
    async fn slow_consumer_never_pins_a_stale_frame_or_loses_the_silence() {
        let (hub, source, player) = start_hub("playing", fast_config());
        let mut frames = hub.subscribe().await;
        wait_until("child spawn", || source.spawned() == 1).await;

        // The client reads nothing while a stream of frames arrives. The first
        // one fills the channel; the rest must keep replacing the pending frame.
        let line = |level: f64| format!(r#"{{"ok":true,"bands":[{level},0,0,0,0,0,0,0,0,0]}}"#);
        for level in [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8] {
            source.send(0, &line(level));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        player.set("stopped");
        wait_until("child kill", || source.active() == 0).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let received = drain(&mut frames).await;
        let levels: Vec<f64> = received.iter().map(|frame| frame.bands[0]).collect();
        assert_eq!(levels.first(), Some(&0.1), "channel held the first frame: {levels:?}");
        assert_eq!(levels.last(), Some(&0.0), "silence must arrive last: {levels:?}");
        assert!(
            received.len() <= 3 && !levels.contains(&0.8),
            "stale frames must be replaced, not queued: {levels:?}"
        );
        assert_eq!(received.last(), Some(&VisFrame::silence()));
    }

    /// Reports "playing" to the first caller only, like a player that stops
    /// between a subscriber's check and the supervisor's first reconcile.
    struct PlaysOnce {
        calls: AtomicUsize,
        watch_tx: watch::Sender<PlayerState>,
    }

    #[async_trait::async_trait]
    impl Player for PlaysOnce {
        async fn play(&self, _url: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn state(&self) -> PlayerState {
            let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            FakePlayer::snapshot(if first { "playing" } else { "stopped" })
        }
        fn subscribe(&self) -> watch::Receiver<PlayerState> {
            self.watch_tx.subscribe()
        }
    }

    #[tokio::test]
    async fn stop_before_the_first_reconcile_still_sends_silence() {
        let source = Arc::new(FakeSource::default());
        let (watch_tx, _) = watch::channel(FakePlayer::snapshot("stopped"));
        let player = Arc::new(PlaysOnce { calls: AtomicUsize::new(0), watch_tx });
        let hub = VisHub::start(
            Box::new(SharedSource(source.clone())),
            player as Arc<dyn Player>,
            fast_config(),
        );

        // The subscriber sees "playing" (no initial silence), then the player
        // is stopped by the time the supervisor first looks.
        let mut frames = hub.subscribe().await;

        assert_eq!(recv_frame(&mut frames).await, VisFrame::silence());
        assert_no_frame(&mut frames, Duration::from_millis(150)).await;
        assert_eq!(source.spawned(), 0, "must not start a child while stopped");
    }

    /// A hub whose supervisor the test drives by hand, one reconcile at a time.
    fn hub_without_supervisor(player: Arc<dyn Player>) -> (Arc<VisHub>, Supervisor) {
        let (frames, _) = broadcast::channel(BROADCAST_CAPACITY);
        let hub = Arc::new(VisHub {
            source: Box::new(SharedSource(Arc::new(FakeSource::default()))),
            player,
            config: fast_config(),
            frames,
            subscribers: AtomicUsize::new(0),
            unconfirmed_playing: AtomicBool::new(false),
            wake: Notify::new(),
        });
        let supervisor = Supervisor::new(hub.clone());
        (hub, supervisor)
    }

    #[tokio::test]
    async fn registration_racing_a_zero_count_reconcile_keeps_its_silence() {
        let (watch_tx, _) = watch::channel(FakePlayer::snapshot("stopped"));
        let player = Arc::new(PlaysOnce { calls: AtomicUsize::new(0), watch_tx });
        let (hub, mut supervisor) = hub_without_supervisor(player as Arc<dyn Player>);

        // Interleaving: the supervisor has read `subscriber_count() == 0` and
        // is about to bail, but a subscriber registers, sees "playing" and sets
        // its flag first. From the supervisor's side that is the zero-count
        // branch running with the flag already set; replay it in that order.
        let mut frames = hub.subscribe().await;
        hub.subscribers.fetch_sub(1, Ordering::SeqCst); // the stale count it read
        assert!(hub.unconfirmed_playing.load(Ordering::SeqCst));
        supervisor.reconcile().await;
        assert!(
            hub.unconfirmed_playing.load(Ordering::SeqCst),
            "a zero-count reconcile must not clear a registered subscriber's flag"
        );

        // The subscriber is really there; the next reconcile sees the stop.
        hub.subscribers.fetch_add(1, Ordering::SeqCst);
        supervisor.reconcile().await;
        assert_eq!(recv_frame(&mut frames).await, VisFrame::silence());
        assert!(!hub.unconfirmed_playing.load(Ordering::SeqCst), "flag is consumed");
    }
}
