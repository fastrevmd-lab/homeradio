use crate::cliamp::{Player, PlayerState as CliampState};
use crate::config::Config;
use crate::stations::StationManager;
use crate::title;
use crate::volume;
use crate::yxc::{YxcClient, YxcError};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, RwLock};
use tokio::time::{interval, Duration};
use tracing::warn;

#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub player: PlayerInfo,
    pub receiver: ReceiverInfo,
    pub zones: Option<HashMap<String, ZoneInfo>>,
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerInfo {
    pub state: String,
    pub station: Option<String>,
    pub station_name: Option<String>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReceiverInfo {
    pub ok: bool,
    pub error: Option<String>,
    pub playback: Option<String>,
    pub airplay_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ZoneInfo {
    pub label: String,
    pub power: String,
    pub radio: bool,
    pub input: String,
    pub volume: i32,
    pub db: f64,
    pub mute: bool,
    pub cap_db: f64,
    pub min_db: f64,
    pub step_db: f64,
}

/// How long a just-written zone value masks the receiver's (asynchronously
/// applied) readback. Shorter than the 3 s poll, so the next poll sees real state.
const OVERRIDE_TTL: Duration = Duration::from_millis(2000);

/// A value we just wrote to the receiver, overlaid on refreshed zone state until
/// the receiver has caught up. Each field is `None` when untouched.
#[derive(Debug, Clone, Default)]
pub struct ZoneOverride {
    pub power: Option<String>,
    pub input: Option<String>,
    pub volume: Option<i32>,
    pub mute: Option<bool>,
}

/// A written value and when it was written.
type Stamped<T> = Option<(T, Instant)>;

/// The live power/input of a zone as read from the receiver immediately before a
/// write. Remembered next to the override so it only masks the readback while the
/// readback is still showing that old value.
#[derive(Debug, Clone)]
pub struct ZoneLive {
    pub power: String,
    pub input: String,
}

/// Whether a just-written value should still mask the live readback `live`.
///
/// With a known `prior` (the live value before the write) the override applies
/// only while the readback has not caught up: `live` is still `prior`, or already
/// `written`. Any other value means something else changed the zone in the
/// meantime (a TV switching input, say), and the live value wins. Without a
/// `prior` the override applies unconditionally.
fn override_applies(written: &str, prior: Option<&str>, live: &str) -> bool {
    let Some(prior) = prior else {
        return true;
    };
    live == prior || live == written
}

/// The overrides held for one zone. Each field expires on its own clock, so a
/// run of volume writes cannot keep a stale power or input override alive.
#[derive(Debug, Clone, Default)]
struct ZoneOverrides {
    power: Stamped<String>,
    input: Stamped<String>,
    volume: Stamped<i32>,
    mute: Stamped<bool>,
    /// Live power before the pending power write (see [`override_applies`]).
    power_prior: Option<String>,
    /// Live input before the pending input write.
    input_prior: Option<String>,
}

impl ZoneOverrides {
    fn record(&mut self, change: ZoneOverride, prior: Option<&ZoneLive>, now: Instant) {
        if let Some(power) = change.power {
            self.power = Some((power, now));
            self.power_prior = prior.map(|live| live.power.clone());
        }
        if let Some(input) = change.input {
            self.input = Some((input, now));
            self.input_prior = prior.map(|live| live.input.clone());
        }
        if let Some(volume) = change.volume {
            self.volume = Some((volume, now));
        }
        if let Some(mute) = change.mute {
            self.mute = Some((mute, now));
        }
    }

    /// Drop every field older than the TTL.
    fn expire(&mut self, now: Instant) {
        fn fresh<T>(field: &mut Stamped<T>, now: Instant) {
            if field
                .as_ref()
                .is_some_and(|(_, written_at)| now.duration_since(*written_at) >= OVERRIDE_TTL)
            {
                *field = None;
            }
        }
        fresh(&mut self.power, now);
        fresh(&mut self.input, now);
        fresh(&mut self.volume, now);
        fresh(&mut self.mute, now);
    }

    fn is_empty(&self) -> bool {
        self.power.is_none() && self.input.is_none() && self.volume.is_none() && self.mute.is_none()
    }

    /// Overlay the fields that have not expired onto `zone_info`.
    ///
    /// When `zone_info` holds the receiver's own readback (`guarded`), a power or
    /// input override is skipped once the readback has moved to something other
    /// than the pre-write or written value. Right after a write, `zone_info` is
    /// the published (already overlaid) state instead, so the write is forced.
    fn apply(&self, zone_info: &mut ZoneInfo, now: Instant, guarded: bool) {
        let live = |written_at: &Instant| now.duration_since(*written_at) < OVERRIDE_TTL;
        if let Some((power, _)) = self.power.as_ref().filter(|(_, at)| live(at)) {
            let prior = self.power_prior.as_deref().filter(|_| guarded);
            if override_applies(power, prior, &zone_info.power) {
                zone_info.power = power.clone();
            }
        }
        if let Some((input, _)) = self.input.as_ref().filter(|(_, at)| live(at)) {
            let prior = self.input_prior.as_deref().filter(|_| guarded);
            if override_applies(input, prior, &zone_info.input) {
                zone_info.input = input.clone();
            }
        }
        if let Some((raw, _)) = self.volume.as_ref().filter(|(_, at)| live(at)) {
            zone_info.volume = *raw;
            zone_info.db = volume::raw_to_db(*raw);
        }
        if let Some((mute, _)) = self.mute.as_ref().filter(|(_, at)| live(at)) {
            zone_info.mute = *mute;
        }
        zone_info.radio = zone_info.power == "on" && zone_info.input == "airplay";
    }
}

/// `playback == "play"` and some zone is on+airplay.
fn compute_airplay_active(playback: Option<&str>, zones: Option<&HashMap<String, ZoneInfo>>) -> bool {
    playback == Some("play")
        && zones.is_some_and(|zones| zones.values().any(|zone| zone.radio))
}

pub struct StateManager {
    yxc: Arc<dyn YxcClient>,
    player: Arc<dyn Player>,
    stations: Arc<RwLock<StationManager>>,
    config: Config,
    state_tx: watch::Sender<State>,
    state_rx: watch::Receiver<State>,
    last_played_station: RwLock<Option<String>>,
    overrides: StdMutex<HashMap<String, ZoneOverrides>>,
}

impl StateManager {
    pub fn new(
        yxc: Arc<dyn YxcClient>,
        player: Arc<dyn Player>,
        stations: Arc<RwLock<StationManager>>,
        config: Config,
    ) -> Self {
        let initial_state = State {
            player: PlayerInfo {
                state: "unknown".to_string(),
                station: None,
                station_name: None,
                artist: None,
                title: None,
                error: None,
            },
            receiver: ReceiverInfo {
                ok: false,
                error: Some("Not yet polled".to_string()),
                playback: None,
                airplay_active: false,
            },
            zones: None,
            updated_ms: current_time_ms(),
        };

        let (state_tx, state_rx) = watch::channel(initial_state);

        Self {
            yxc,
            player,
            stations,
            config,
            state_tx,
            state_rx,
            last_played_station: RwLock::new(None),
            overrides: StdMutex::new(HashMap::new()),
        }
    }

    /// Record values just written to a zone and patch them into the published
    /// state. The receiver applies writes asynchronously, so an immediate
    /// readback is stale; the overlay is reapplied by `refresh` for a short TTL,
    /// tracked per field.
    ///
    /// `prior` is the zone's live power/input read just before the write. While
    /// it is known, a power/input override masks the readback only until the
    /// readback shows something other than `prior` or the written value.
    pub fn note_zone_write(&self, zone: &str, change: ZoneOverride, prior: Option<&ZoneLive>) {
        let now = Instant::now();
        self.overrides
            .lock()
            .unwrap()
            .entry(zone.to_string())
            .or_default()
            .record(change, prior, now);

        self.state_tx.send_modify(|state| {
            let overrides = self.overrides.lock().unwrap();
            if let (Some(zones), Some(active)) = (state.zones.as_mut(), overrides.get(zone)) {
                if let Some(zone_info) = zones.get_mut(zone) {
                    active.apply(zone_info, now, false);
                }
            }
            state.receiver.airplay_active =
                compute_airplay_active(state.receiver.playback.as_deref(), state.zones.as_ref());
        });
    }

    /// The fields most recently written to `zone` that the receiver may not have
    /// caught up with yet: every override still inside its TTL, per field.
    pub fn pending_override(&self, zone: &str) -> ZoneOverride {
        let now = Instant::now();
        let overrides = self.overrides.lock().unwrap();
        let Some(active) = overrides.get(zone) else {
            return ZoneOverride::default();
        };
        fn live<T: Clone>(field: &Stamped<T>, now: Instant) -> Option<T> {
            field
                .as_ref()
                .filter(|(_, written_at)| now.duration_since(*written_at) < OVERRIDE_TTL)
                .map(|(value, _)| value.clone())
        }
        ZoneOverride {
            power: live(&active.power, now),
            input: live(&active.input, now),
            volume: live(&active.volume, now),
            mute: live(&active.mute, now),
        }
    }

    /// Like [`pending_override`](Self::pending_override), but a power or input
    /// override is dropped once the zone's `live` readback has moved to a value
    /// that is neither the pre-write nor the written one: the live value wins.
    pub fn pending_override_against(&self, zone: &str, live: &ZoneLive) -> ZoneOverride {
        let mut pending = self.pending_override(zone);
        let overrides = self.overrides.lock().unwrap();
        let Some(active) = overrides.get(zone) else {
            return pending;
        };
        pending.power = pending.power.filter(|written| {
            override_applies(written, active.power_prior.as_deref(), &live.power)
        });
        pending.input = pending.input.filter(|written| {
            override_applies(written, active.input_prior.as_deref(), &live.input)
        });
        pending
    }

    /// The volume (raw) most recently written to `zone`, if still pending.
    pub fn pending_volume(&self, zone: &str) -> Option<i32> {
        self.pending_override(zone).volume
    }

    pub fn subscribe(&self) -> watch::Receiver<State> {
        self.state_rx.clone()
    }

    pub async fn get_state(&self) -> State {
        self.state_rx.borrow().clone()
    }

    pub async fn set_last_played_station(&self, station_id: String) {
        *self.last_played_station.write().await = Some(station_id);
    }

    /// Start background polling task
    pub fn start_polling_task(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(3));

            loop {
                interval.tick().await;
                self.refresh().await;
            }
        });
    }

    /// Start task to watch for player state changes and refresh immediately
    pub fn start_player_watch_task(self: Arc<Self>) {
        let mut player_rx = self.player.subscribe();

        tokio::spawn(async move {
            loop {
                if player_rx.changed().await.is_err() {
                    break;
                }
                // Player state changed, refresh immediately
                self.refresh().await;
            }
        });
    }

    pub async fn refresh(&self) {
        let player_state = self.player.state().await;
        let stations = self.stations.read().await;

        let player_info = self.build_player_info(&player_state, &stations).await;

        let (receiver_info, mut zones) = self.fetch_receiver_state().await;

        let mut receiver_info = receiver_info;
        if let Some(zones) = zones.as_mut() {
            let now = Instant::now();
            let mut overrides = self.overrides.lock().unwrap();
            overrides.retain(|_, active| {
                active.expire(now);
                !active.is_empty()
            });
            for (zone_name, zone_info) in zones.iter_mut() {
                if let Some(active) = overrides.get(zone_name) {
                    active.apply(zone_info, now, true);
                }
            }
            // The overrides can change which zones are radio, so derive the lamp
            // from the final zones, not the raw readback.
            receiver_info.airplay_active =
                compute_airplay_active(receiver_info.playback.as_deref(), Some(zones));
        }

        let state = State {
            player: player_info,
            receiver: receiver_info,
            zones,
            updated_ms: current_time_ms(),
        };

        self.state_tx.send(state).ok();
    }

    async fn build_player_info(
        &self,
        player_state: &CliampState,
        stations: &StationManager,
    ) -> PlayerInfo {
        // Match on the registry identity (`logical_track.path`, falling back to
        // `track.path`); `track.*` is only for now-playing metadata.
        let station_identity = player_state.station_url.as_ref().or(player_state.url.as_ref());
        let station = if let Some(url) = station_identity {
            // Try to match by URL
            let matched = stations
                .get_registry()
                .groups
                .iter()
                .flat_map(|g| &g.stations)
                .find(|s| stations.get_station_url(&s.id) == Some(url))
                .map(|s| s.id.clone());

            if matched.is_some() {
                matched
            } else {
                // Fall back to last played
                self.last_played_station.read().await.clone()
            }
        } else {
            None
        };

        let station_name = station.as_ref().and_then(|id| stations.get_station_name(id));

        // Parse title from cliamp. A stopped player reports no stale metadata, and a
        // title that is just the stream's path tail or URL is not a real title.
        let (artist, title_parsed) = match &player_state.title {
            Some(raw)
                if player_state.state != "stopped"
                    && !title::is_placeholder_title(raw, player_state.url.as_deref())
                    && !title::is_placeholder_title(raw, player_state.station_url.as_deref()) =>
            {
                title::parse_title(raw)
            }
            _ => (None, None),
        };

        PlayerInfo {
            state: player_state.state.clone(),
            station,
            station_name,
            artist,
            title: title_parsed,
            error: if player_state.state == "unknown" {
                Some("Player not responding".to_string())
            } else {
                None
            },
        }
    }

    async fn fetch_receiver_state(&self) -> (ReceiverInfo, Option<HashMap<String, ZoneInfo>>) {
        let play_info_result = self.yxc.get_play_info().await;
        let main_result = self.yxc.get_zone_status("main").await;
        let zone2_result = self.yxc.get_zone_status("zone2").await;

        match (&main_result, &zone2_result) {
            (Err(e), _) | (_, Err(e)) if matches!(e, YxcError::Unreachable) => (
                ReceiverInfo {
                    ok: false,
                    error: Some("Receiver not responding — is it unplugged?".to_string()),
                    playback: None,
                    airplay_active: false,
                },
                None,
            ),
            (Ok(main_status), Ok(zone2_status)) => {
                let playback = play_info_result.ok().map(|p| p.playback.clone());

                let airplay_active = playback.as_deref() == Some("play")
                    && (main_status.input == "airplay" && main_status.power == "on"
                        || zone2_status.input == "airplay" && zone2_status.power == "on");

                let mut zones = HashMap::new();

                for (zone_name, status) in &[("main", main_status), ("zone2", zone2_status)] {
                    if let Some(zone_config) = self.config.zones.get(*zone_name) {
                        let db = status
                            .actual_volume
                            .as_ref()
                            .map(|v| v.value)
                            .unwrap_or_else(|| volume::raw_to_db(status.volume));

                        let radio = status.power == "on" && status.input == "airplay";

                        zones.insert(
                            zone_name.to_string(),
                            ZoneInfo {
                                label: zone_config.label.clone(),
                                power: status.power.clone(),
                                radio,
                                input: status.input.clone(),
                                volume: status.volume,
                                db,
                                mute: status.mute,
                                cap_db: zone_config.cap_db,
                                min_db: -80.5,
                                step_db: 0.5,
                            },
                        );
                    }
                }

                (
                    ReceiverInfo {
                        ok: true,
                        error: None,
                        playback,
                        airplay_active,
                    },
                    Some(zones),
                )
            }
            _ => {
                warn!("Unexpected YXC error pattern");
                (
                    ReceiverInfo {
                        ok: false,
                        error: Some("Receiver communication error".to_string()),
                        playback: None,
                        airplay_active: false,
                    },
                    None,
                )
            }
        }
    }
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
