use axum_test::TestServer;
use home_radio::*;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

// Mock YXC Client
struct MockYxcClient {
    zone_status: Arc<Mutex<HashMap<String, yxc::ZoneStatus>>>,
    play_info: Arc<Mutex<yxc::PlayInfo>>,
    /// When true, writes are accepted but not applied (the receiver applies
    /// them asynchronously, so an immediate readback is stale).
    lagging: bool,
    /// Every call, e.g. "set_volume main 100", in order.
    calls: Arc<std::sync::Mutex<Vec<String>>>,
    /// Simulates AirPlay grabbing both zones after N status polls.
    takeover: Arc<std::sync::Mutex<Takeover>>,
    /// (zone, requested raw, raw actually stored): the receiver overrides the
    /// first matching `set_volume`, as it does right after power-on.
    override_volume_once: Arc<std::sync::Mutex<Option<(String, i32, i32)>>>,
    /// A zone whose `set_power` calls fail (recorded, then rejected as busy).
    fail_set_power_for: Option<String>,
    /// A zone whose `get_zone_status` calls fail as unreachable.
    fail_status_for: Option<String>,
    /// Artificial delay before `get_play_info` answers, in milliseconds.
    play_info_delay_ms: Arc<std::sync::atomic::AtomicU64>,
}

/// Arms a delayed AirPlay takeover: after `polls_left` more `get_zone_status`
/// calls, every zone flips to on+airplay (volume untouched).
#[derive(Default)]
struct Takeover {
    armed: bool,
    polls_left: usize,
}

impl MockYxcClient {
    fn with_zones(zones: &[(&str, &str, &str, i32)]) -> Self {
        let statuses = zones
            .iter()
            .map(|(zone, power, input, volume)| {
                (
                    zone.to_string(),
                    yxc::ZoneStatus {
                        power: power.to_string(),
                        input: input.to_string(),
                        mute: false,
                        volume: *volume,
                        actual_volume: None,
                    },
                )
            })
            .collect();
        Self {
            zone_status: Arc::new(Mutex::new(statuses)),
            ..Self::new()
        }
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn new() -> Self {
        let mut zone_status = HashMap::new();
        zone_status.insert(
            "main".to_string(),
            yxc::ZoneStatus {
                power: "on".to_string(),
                input: "airplay".to_string(),
                mute: false,
                volume: 95,
                actual_volume: Some(yxc::ActualVolume { value: -33.0 }),
            },
        );
        zone_status.insert(
            "zone2".to_string(),
            yxc::ZoneStatus {
                power: "on".to_string(),
                input: "airplay".to_string(),
                mute: false,
                volume: 151,
                actual_volume: Some(yxc::ActualVolume { value: -5.0 }),
            },
        );

        Self {
            zone_status: Arc::new(Mutex::new(zone_status)),
            play_info: Arc::new(Mutex::new(yxc::PlayInfo {
                playback: "play".to_string(),
            })),
            lagging: false,
            calls: Arc::default(),
            takeover: Arc::default(),
            override_volume_once: Arc::default(),
            fail_set_power_for: None,
            fail_status_for: None,
            play_info_delay_ms: Arc::default(),
        }
    }

    fn lagging() -> Self {
        Self {
            lagging: true,
            ..Self::new()
        }
    }

    fn unreachable() -> Self {
        Self {
            zone_status: Arc::new(Mutex::new(HashMap::new())),
            play_info: Arc::new(Mutex::new(yxc::PlayInfo {
                playback: "stop".to_string(),
            })),
            lagging: false,
            calls: Arc::default(),
            takeover: Arc::default(),
            override_volume_once: Arc::default(),
            fail_set_power_for: None,
            fail_status_for: None,
            play_info_delay_ms: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl yxc::YxcClient for MockYxcClient {
    async fn get_zone_status(&self, zone: &str) -> Result<yxc::ZoneStatus, yxc::YxcError> {
        if self.fail_status_for.as_deref() == Some(zone) {
            return Err(yxc::YxcError::Unreachable);
        }
        let mut statuses = self.zone_status.lock().await;
        if statuses.is_empty() {
            return Err(yxc::YxcError::Unreachable);
        }
        let flip_now = {
            let mut takeover = self.takeover.lock().unwrap();
            if takeover.armed && takeover.polls_left > 0 {
                takeover.polls_left -= 1;
            }
            let due = takeover.armed && takeover.polls_left == 0;
            if due {
                takeover.armed = false;
            }
            due
        };
        if flip_now {
            for status in statuses.values_mut() {
                status.power = "on".to_string();
                status.input = "airplay".to_string();
            }
        }
        statuses
            .get(zone)
            .cloned()
            .ok_or(yxc::YxcError::ApiError("Zone not found".to_string()))
    }

    async fn get_play_info(&self) -> Result<yxc::PlayInfo, yxc::YxcError> {
        let delay_ms = self.play_info_delay_ms.load(std::sync::atomic::Ordering::SeqCst);
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        Ok(self.play_info.lock().await.clone())
    }

    async fn set_power(&self, zone: &str, on: bool) -> Result<(), yxc::YxcError> {
        self.record(format!("set_power {zone} {on}"));
        if self.fail_set_power_for.as_deref() == Some(zone) {
            return Err(yxc::YxcError::ApiError("busy".to_string()));
        }
        if self.lagging {
            return Ok(());
        }
        let mut statuses = self.zone_status.lock().await;
        if let Some(status) = statuses.get_mut(zone) {
            status.power = if on { "on".to_string() } else { "standby".to_string() };
        }
        Ok(())
    }

    async fn set_volume(&self, zone: &str, raw: i32) -> Result<(), yxc::YxcError> {
        self.record(format!("set_volume {zone} {raw}"));
        if self.lagging {
            return Ok(());
        }
        let mut statuses = self.zone_status.lock().await;
        let stored = {
            let mut forced = self.override_volume_once.lock().unwrap();
            match forced.as_ref() {
                Some((forced_zone, requested, actual)) if forced_zone == zone && *requested == raw => {
                    let actual = *actual;
                    *forced = None;
                    actual
                }
                _ => raw,
            }
        };
        if let Some(status) = statuses.get_mut(zone) {
            status.volume = stored;
        }
        Ok(())
    }

    async fn set_mute(&self, zone: &str, mute: bool) -> Result<(), yxc::YxcError> {
        self.record(format!("set_mute {zone} {mute}"));
        if self.lagging {
            return Ok(());
        }
        let mut statuses = self.zone_status.lock().await;
        if let Some(status) = statuses.get_mut(zone) {
            status.mute = mute;
        }
        Ok(())
    }

    async fn set_input(&self, zone: &str, input: &str) -> Result<(), yxc::YxcError> {
        self.record(format!("set_input {zone} {input}"));
        if self.lagging {
            return Ok(());
        }
        let mut statuses = self.zone_status.lock().await;
        if let Some(status) = statuses.get_mut(zone) {
            status.input = input.to_string();
        }
        Ok(())
    }
}

/// Ordered log of side effects across mocks ("connect", "player.play", ...).
type Events = Arc<std::sync::Mutex<Vec<String>>>;

fn count_events(events: &Events, event: &str) -> usize {
    events.lock().unwrap().iter().filter(|e| e.as_str() == event).count()
}

// Mock audio route: records calls, and can simulate the receiver grabbing every
// zone when the sink (re)connects.
struct MockAudioRoute {
    events: Events,
    active: std::sync::atomic::AtomicBool,
    fail_connect: std::sync::atomic::AtomicBool,
    fail_disconnect: std::sync::atomic::AtomicBool,
    /// When set, connect/reconnect flip every zone to on+airplay, as the real
    /// receiver does when the RAOP session is established.
    grab_zones: Option<Arc<Mutex<HashMap<String, yxc::ZoneStatus>>>>,
}

impl MockAudioRoute {
    fn new(events: Events) -> Self {
        Self {
            events,
            active: false.into(),
            fail_connect: false.into(),
            fail_disconnect: false.into(),
            grab_zones: None,
        }
    }

    async fn grab(&self) {
        let Some(zones) = &self.grab_zones else { return };
        for status in zones.lock().await.values_mut() {
            status.power = "on".to_string();
            status.input = "airplay".to_string();
        }
    }
}

#[async_trait::async_trait]
impl route::AudioRoute for MockAudioRoute {
    async fn connect(&self) -> anyhow::Result<()> {
        self.events.lock().unwrap().push("connect".to_string());
        if self.fail_connect.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("unit failed");
        }
        self.active.store(true, std::sync::atomic::Ordering::SeqCst);
        self.grab().await;
        Ok(())
    }

    async fn disconnect(&self) -> anyhow::Result<()> {
        self.events.lock().unwrap().push("disconnect".to_string());
        if self.fail_disconnect.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("unit failed to stop");
        }
        self.active.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn reconnect(&self) -> anyhow::Result<()> {
        self.events.lock().unwrap().push("reconnect".to_string());
        self.active.store(true, std::sync::atomic::Ordering::SeqCst);
        self.grab().await;
        Ok(())
    }

    async fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }
}

// Mock Player
struct MockPlayer {
    events: Events,
    state: Arc<Mutex<cliamp::PlayerState>>,
    watch_tx: tokio::sync::watch::Sender<cliamp::PlayerState>,
    watch_rx: tokio::sync::watch::Receiver<cliamp::PlayerState>,
}

impl MockPlayer {
    fn new(events: Events) -> Self {
        let initial_state = cliamp::PlayerState {
            state: "stopped".to_string(),
            url: None,
            station_url: None,
            title: None,
        };
        let (watch_tx, watch_rx) = tokio::sync::watch::channel(initial_state.clone());

        Self {
            events,
            state: Arc::new(Mutex::new(initial_state)),
            watch_tx,
            watch_rx,
        }
    }
}

#[async_trait::async_trait]
impl cliamp::Player for MockPlayer {
    async fn play(&self, url: &str) -> anyhow::Result<()> {
        self.events.lock().unwrap().push("player.play".to_string());
        let mut state = self.state.lock().await;
        state.state = "playing".to_string();
        state.url = Some(url.to_string());
        state.station_url = Some(url.to_string());
        let _ = self.watch_tx.send(state.clone());
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.events.lock().unwrap().push("player.stop".to_string());
        let mut state = self.state.lock().await;
        state.state = "stopped".to_string();
        state.url = None;
        state.station_url = None;
        let _ = self.watch_tx.send(state.clone());
        Ok(())
    }

    async fn state(&self) -> cliamp::PlayerState {
        self.state.lock().await.clone()
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<cliamp::PlayerState> {
        self.watch_rx.clone()
    }
}

/// Spectrum source that replays fixed lines and then idles, so no real
/// `cliamp visstream` child is ever started.
#[derive(Default)]
struct ScriptedVisSource {
    lines: Vec<String>,
}

struct ScriptedVisFeed {
    lines: std::collections::VecDeque<String>,
}

#[async_trait::async_trait]
impl vis::VisFeed for ScriptedVisFeed {
    async fn next_line(&mut self) -> Option<String> {
        match self.lines.pop_front() {
            Some(line) => Some(line),
            None => std::future::pending().await,
        }
    }
}

impl vis::VisSource for ScriptedVisSource {
    fn spawn(&self) -> anyhow::Result<Box<dyn vis::VisFeed>> {
        Ok(Box::new(ScriptedVisFeed {
            lines: self.lines.iter().cloned().collect(),
        }))
    }
}

/// Everything a test needs to drive and inspect the app.
struct TestApp {
    server: TestServer,
    state: api::AppState,
    route: Arc<MockAudioRoute>,
    player: Arc<MockPlayer>,
    events: Events,
    radio_browser: Arc<FakeRadioBrowser>,
    /// Owns the cache dir (and `my-stations.json`); gone when the test ends.
    _cache_dir: Option<tempfile::TempDir>,
}

/// Radio Browser stand-in: serves a fixed list, can be switched off, and
/// records what it was asked.
struct FakeRadioBrowser {
    stations: Vec<radiobrowser::RbStation>,
    down: std::sync::atomic::AtomicBool,
    by_uuid_calls: std::sync::atomic::AtomicUsize,
    /// When set, `by_uuid` answers with the first station whatever uuid was asked for.
    answer_wrong_station: std::sync::atomic::AtomicBool,
    /// Artificial delay before `by_uuid` answers, in milliseconds.
    by_uuid_delay_ms: std::sync::atomic::AtomicU64,
    searches: std::sync::Mutex<Vec<(Option<String>, Option<String>)>>,
}

impl FakeRadioBrowser {
    fn new(stations: Vec<radiobrowser::RbStation>) -> Self {
        Self {
            stations,
            down: std::sync::atomic::AtomicBool::new(false),
            by_uuid_calls: std::sync::atomic::AtomicUsize::new(0),
            answer_wrong_station: std::sync::atomic::AtomicBool::new(false),
            by_uuid_delay_ms: std::sync::atomic::AtomicU64::new(0),
            searches: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_down(&self) -> bool {
        self.down.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set_answer_wrong_station(&self, wrong: bool) {
        self.answer_wrong_station.store(wrong, std::sync::atomic::Ordering::SeqCst);
    }

    fn by_uuid_call_count(&self) -> usize {
        self.by_uuid_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl radiobrowser::RadioBrowser for FakeRadioBrowser {
    async fn search(
        &self,
        name: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Vec<radiobrowser::RbStation>, radiobrowser::RbError> {
        self.searches
            .lock()
            .unwrap()
            .push((name.map(str::to_string), tag.map(str::to_string)));
        if self.is_down() {
            return Err(radiobrowser::RbError::Unavailable("fake outage".to_string()));
        }
        Ok(self.stations.clone())
    }

    async fn by_uuid(&self, uuid: &str) -> Result<Option<radiobrowser::RbStation>, radiobrowser::RbError> {
        self.by_uuid_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let delay_ms = self.by_uuid_delay_ms.load(std::sync::atomic::Ordering::SeqCst);
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        if self.is_down() {
            return Err(radiobrowser::RbError::Unavailable("fake outage".to_string()));
        }
        if self.answer_wrong_station.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(self.stations.first().cloned());
        }
        Ok(self.stations.iter().find(|station| station.uuid == uuid).cloned())
    }
}

/// UUID of the first fake station.
const JAZZ_UUID: &str = "11111111-1111-1111-1111-111111111111";
/// UUID of the second fake station.
const BLUES_UUID: &str = "22222222-2222-2222-2222-222222222222";

fn fake_stations() -> Vec<radiobrowser::RbStation> {
    vec![
        radiobrowser::RbStation {
            uuid: JAZZ_UUID.to_string(),
            name: "Smooth Jazz Radio".to_string(),
            genre: "jazz".to_string(),
            country: "Canada".to_string(),
            bitrate: 128,
            url: "http://jazz.example.com/stream".to_string(),
        },
        radiobrowser::RbStation {
            uuid: BLUES_UUID.to_string(),
            name: "Blues Highway".to_string(),
            genre: "blues".to_string(),
            country: "USA".to_string(),
            bitrate: 64,
            url: "https://blues.example.com/live".to_string(),
        },
    ]
}

// Helper to create test app
async fn create_test_app(yxc: Arc<dyn yxc::YxcClient>) -> TestServer {
    create_test_app_with_timing(yxc, api::PolicyTiming::default()).await
}

async fn create_test_app_with_timing(
    yxc: Arc<dyn yxc::YxcClient>,
    policy_timing: api::PolicyTiming,
) -> TestServer {
    build_app(yxc, policy_timing, None).await.server
}

async fn build_app(
    yxc: Arc<dyn yxc::YxcClient>,
    policy_timing: api::PolicyTiming,
    grab_zones: Option<Arc<Mutex<HashMap<String, yxc::ZoneStatus>>>>,
) -> TestApp {
    let cache_dir = tempfile::tempdir().unwrap();
    let mut app = build_app_in(cache_dir.path(), yxc, policy_timing, grab_zones).await;
    app._cache_dir = Some(cache_dir);
    app
}

/// Like `build_app`, but on a caller-owned cache dir, so a test can start a
/// second app on the same `my-stations.json` (a restart).
async fn build_app_in(
    test_dir: &std::path::Path,
    yxc: Arc<dyn yxc::YxcClient>,
    policy_timing: api::PolicyTiming,
    grab_zones: Option<Arc<Mutex<HashMap<String, yxc::ZoneStatus>>>>,
) -> TestApp {
    let config = config::Config::with_receiver_url("http://192.0.2.10");
    let events: Events = Arc::default();
    let player_mock = Arc::new(MockPlayer::new(events.clone()));
    let player = player_mock.clone() as Arc<dyn cliamp::Player>;
    let route_mock = Arc::new(MockAudioRoute {
        grab_zones,
        ..MockAudioRoute::new(events.clone())
    });

    // Create a minimal stations manager in the given cache dir
    let stations_file = test_dir.join("stations.toml");
    std::fs::write(
        &stations_file,
        "[[station]]\nid = \"test\"\nname = \"Test Station\"\nshort = \"TEST\"\ngenre = \"Test\"\nurl = \"https://test.example.com/stream\"\n",
    )
    .unwrap();

    let stations = Arc::new(RwLock::new(
        stations::StationManager::new(&stations_file, test_dir, "http://localhost".to_string())
            .await
            .unwrap(),
    ));

    let state_manager = Arc::new(state::StateManager::new(
        yxc.clone(),
        player.clone(),
        stations.clone(),
        config.clone(),
    ));

    // Manually trigger a refresh so tests don't have to wait
    state_manager.refresh().await;

    let radio_browser = Arc::new(FakeRadioBrowser::new(fake_stations()));
    let app_state = api::AppState {
        yxc,
        player,
        stations,
        state_manager,
        config,
        play_mutex: Arc::new(Mutex::new(())),
        policy_timing,
        generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        route: route_mock.clone(),
        route_tracking: Arc::default(),
        policy_completions: Arc::default(),
        vis: vis::VisHub::start(
            Box::new(ScriptedVisSource::default()),
            player_mock.clone() as Arc<dyn cliamp::Player>,
            vis::VisConfig::default(),
        ),
        radio_browser: radio_browser.clone(),
        stations_tx: tokio::sync::broadcast::channel(16).0,
    };

    TestApp {
        server: TestServer::new(api::create_router(app_state.clone())),
        state: app_state,
        route: route_mock,
        player: player_mock,
        events,
        radio_browser,
        _cache_dir: None,
    }
}

#[tokio::test]
async fn test_get_state() {
    let yxc = Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    let response = server.get("/api/state").await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    assert_eq!(body["receiver"]["ok"], true);
    assert!(body["zones"].is_object());
}

#[tokio::test]
async fn test_volume_clamping_main() {
    let yxc = Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    // Try to set volume above cap (main cap is -15.0 dB = raw 131)
    let response = server
        .post("/api/zone/main/volume")
        .json(&json!({"db": 10.0}))
        .await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    // Should be clamped to 131 (cap for main zone)
    assert_eq!(body["zones"]["main"]["volume"], 131);
}

#[tokio::test]
async fn test_volume_clamping_zone2() {
    let yxc = Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    // Try to set volume above cap (zone2 cap is 0.0 dB = raw 161)
    let response = server
        .post("/api/zone/zone2/volume")
        .json(&json!({"db": 10.0}))
        .await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    // Should be clamped to 161 (cap for zone2)
    assert_eq!(body["zones"]["zone2"]["volume"], 161);
}

#[tokio::test]
async fn test_unknown_zone() {
    let yxc = Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    let response = server
        .post("/api/zone/invalid/power")
        .json(&json!({"on": true}))
        .await;
    response.assert_status(axum::http::StatusCode::NOT_FOUND);

    let body: serde_json::Value = response.json();
    assert_eq!(body["error"], "unknown_zone");
}

#[tokio::test]
async fn test_unknown_station() {
    let yxc = Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    let response = server
        .post("/api/play")
        .json(&json!({"station": "nonexistent"}))
        .await;
    response.assert_status(axum::http::StatusCode::BAD_REQUEST);

    let body: serde_json::Value = response.json();
    assert_eq!(body["error"], "unknown_station");
}

#[tokio::test]
async fn test_receiver_unreachable() {
    let yxc = Arc::new(MockYxcClient::unreachable()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    let response = server.get("/api/state").await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    assert_eq!(body["receiver"]["ok"], false);
    assert_eq!(
        body["receiver"]["error"],
        "Receiver not responding — is it unplugged?"
    );
    assert_eq!(body["zones"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_play_station() {
    let yxc = Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    let response = server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    assert_eq!(body["player"]["state"], "playing");
}

#[tokio::test]
async fn test_power_on_already_on_no_volume_change() {
    // Create mock with zone already on+airplay at -40 dB
    let mut zone_status = HashMap::new();
    zone_status.insert(
        "main".to_string(),
        yxc::ZoneStatus {
            power: "on".to_string(),
            input: "airplay".to_string(),
            mute: false,
            volume: 81,  // -40 dB
            actual_volume: Some(yxc::ActualVolume { value: -40.0 }),
        },
    );
    zone_status.insert(
        "zone2".to_string(),
        yxc::ZoneStatus {
            power: "on".to_string(),
            input: "airplay".to_string(),
            mute: false,
            volume: 151,
            actual_volume: Some(yxc::ActualVolume { value: -5.0 }),
        },
    );

    let yxc_mock = MockYxcClient {
        zone_status: Arc::new(Mutex::new(zone_status)),
        play_info: Arc::new(Mutex::new(yxc::PlayInfo {
            playback: "play".to_string(),
        })),
        lagging: false,
        calls: Arc::default(),
        takeover: Arc::default(),
        override_volume_once: Arc::default(),
        fail_set_power_for: None,
        fail_status_for: None,
        play_info_delay_ms: Arc::default(),
    };

    let yxc = Arc::new(yxc_mock) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    // Toggle on (should be no-op since already on+airplay)
    let response = server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    // Volume should be unchanged at 81 (-40 dB), not reset to start_db (-35 dB = 95)
    assert_eq!(body["zones"]["main"]["volume"], 81);
}

#[tokio::test]
async fn test_volume_response_reflects_write_when_receiver_lags() {
    let yxc = Arc::new(MockYxcClient::lagging()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    let response = server
        .post("/api/zone/main/volume")
        .json(&json!({"db": -34.0}))
        .await;
    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(body["zones"]["main"]["volume"], 93);
    assert_eq!(body["zones"]["main"]["db"], -34.0);

    // The broadcast/GET state shows the written value too, not the stale 95.
    let body: serde_json::Value = server.get("/api/state").await.json();
    assert_eq!(body["zones"]["main"]["volume"], 93);
}

#[tokio::test]
async fn test_mute_and_power_responses_reflect_write_when_receiver_lags() {
    let yxc = Arc::new(MockYxcClient::lagging()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    let body: serde_json::Value = server
        .post("/api/zone/main/mute")
        .json(&json!({"mute": true}))
        .await
        .json();
    assert_eq!(body["zones"]["main"]["mute"], true);

    let body: serde_json::Value = server
        .post("/api/zone/main/power")
        .json(&json!({"on": false}))
        .await
        .json();
    assert_eq!(body["zones"]["main"]["power"], "standby");
    assert_eq!(body["zones"]["main"]["radio"], false);
    // Earlier mute patch survives the later write
    assert_eq!(body["zones"]["main"]["mute"], true);
}

#[tokio::test]
async fn test_volume_step_uses_fresh_base() {
    let mock = MockYxcClient::new();
    let statuses = mock.zone_status.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    // Receiver moved on after our cached state (cached main volume is 95).
    statuses.lock().await.get_mut("main").unwrap().volume = 91;

    let body: serde_json::Value = server
        .post("/api/zone/main/volume")
        .json(&json!({"step": 2}))
        .await
        .json();
    assert_eq!(body["zones"]["main"]["volume"], 93);
}

#[tokio::test]
async fn test_set_then_immediate_step() {
    let yxc = Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>;
    let server = create_test_app(yxc).await;

    server
        .post("/api/zone/main/volume")
        .json(&json!({"db": -34.0}))
        .await
        .assert_status_ok();
    let body: serde_json::Value = server
        .post("/api/zone/main/volume")
        .json(&json!({"step": 2}))
        .await
        .json();
    // -34 dB + 2 raw steps (0.5 dB each) = -33 dB
    assert_eq!(body["zones"]["main"]["volume"], 95);
    assert_eq!(body["zones"]["main"]["db"], -33.0);
}

fn fast_timing() -> api::PolicyTiming {
    api::PolicyTiming {
        poll_interval: std::time::Duration::from_millis(10),
        takeover_timeout: std::time::Duration::from_secs(2),
        reapply_delay: std::time::Duration::from_millis(30),
        power_poll_interval: std::time::Duration::from_millis(10),
        power_wait_timeout: std::time::Duration::from_millis(500),
        volume_verify_delay: std::time::Duration::from_millis(50),
        watchdog_interval: std::time::Duration::from_millis(20),
        watchdog_hold: std::time::Duration::from_millis(100),
        watchdog_settle: std::time::Duration::from_millis(150),
        reconnect_min_interval: std::time::Duration::from_secs(5),
        regrab_wait: std::time::Duration::from_millis(150),
    }
}

/// Wait until `condition` holds, failing the test after 3 s.
async fn wait_for(description: &str, condition: impl Fn() -> bool) {
    for _ in 0..300 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {description}");
}

/// Wait until `runs` zone-policy background tasks have finished, second pass
/// included. Use this instead of sleeping before asserting that something
/// did NOT happen, or happened exactly once.
async fn wait_for_policy_runs(app: &TestApp, runs: u64) {
    wait_for("zone policy task to finish", || {
        app.state.policy_completions.load(std::sync::atomic::Ordering::SeqCst) >= runs
    })
    .await;
}

fn count_calls(calls: &Arc<std::sync::Mutex<Vec<String>>>, call: &str) -> usize {
    calls.lock().unwrap().iter().filter(|c| c.as_str() == call).count()
}

#[tokio::test]
async fn test_delayed_airplay_takeover_restores_unselected_main_to_standby() {
    // Upstairs (zone2) is the selected radio zone; Media Room (main) is in standby.
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 151),
    ]);
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let takeover = mock.takeover.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    let server = &app.server;

    // AirPlay grabs both zones a few polls after play starts.
    *takeover.lock().unwrap() = Takeover { armed: true, polls_left: 6 };

    let response = server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await;
    response.assert_status_ok();

    wait_for("main put back to standby", || count_calls(&calls, "set_power main false") >= 1).await;
    // Let the second pass finish too.
    wait_for_policy_runs(&app, 1).await;

    let statuses = statuses.lock().await;
    assert_eq!(statuses["main"].power, "standby");
    assert_eq!(statuses["zone2"].power, "on");
    assert_eq!(statuses["zone2"].input, "airplay");

    // Zone2 was already on, so it is never powered off or re-volumed.
    assert_eq!(count_calls(&calls, "set_power zone2 false"), 0);
    assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("set_volume zone2")));
    // Main is only ever powered down, never given a volume.
    assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("set_volume main")));
}

#[tokio::test]
async fn test_tv_on_hdmi1_survives_radio_play_end_to_end() {
    // Media Room TV on hdmi1 at raw 100; Upstairs in standby and selected.
    let mock = MockYxcClient::with_zones(&[
        ("main", "on", "hdmi1", 100),
        ("zone2", "standby", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let takeover = mock.takeover.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    let server = &app.server;

    *takeover.lock().unwrap() = Takeover { armed: true, polls_left: 6 };

    let response = server
        .post("/api/play")
        .json(&json!({"station": "test", "zones": {"zone2": true}}))
        .await;
    response.assert_status_ok();

    wait_for("main volume restored", || count_calls(&calls, "set_volume main 100") >= 1).await;
    wait_for("zone2 start volume", || count_calls(&calls, "set_volume zone2 151") >= 1).await;
    // Let the second pass finish too.
    wait_for_policy_runs(&app, 1).await;

    assert!(count_calls(&calls, "set_input main hdmi1") >= 1);
    // The TV is never turned off.
    assert_eq!(count_calls(&calls, "set_power main false"), 0);
    // Zone2 gets its start volume (-5 dB = raw 151) exactly once: the second
    // pass must not re-issue SetVolume for a selected zone.
    assert_eq!(count_calls(&calls, "set_volume zone2 151"), 1);

    let statuses = statuses.lock().await;
    assert_eq!(statuses["main"].power, "on");
    assert_eq!(statuses["main"].input, "hdmi1");
    assert_eq!(statuses["main"].volume, 100);
    assert_eq!(statuses["zone2"].power, "on");
    assert_eq!(statuses["zone2"].input, "airplay");
    assert_eq!(statuses["zone2"].volume, 151);
}

#[tokio::test]
async fn test_play_snapshots_live_receiver_not_stale_cache() {
    // The cache will say main is on airplay; the receiver has since moved to hdmi1.
    let mock = MockYxcClient::with_zones(&[
        ("main", "on", "airplay", 95),
        ("zone2", "standby", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let takeover = mock.takeover.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    let server = &app.server;

    {
        let mut statuses = statuses.lock().await;
        let main = statuses.get_mut("main").unwrap();
        main.input = "hdmi1".to_string();
        main.volume = 100;
    }
    *takeover.lock().unwrap() = Takeover { armed: true, polls_left: 6 };

    server
        .post("/api/play")
        .json(&json!({"station": "test", "zones": {"zone2": true}}))
        .await
        .assert_status_ok();

    wait_for("main restored to hdmi1", || count_calls(&calls, "set_input main hdmi1") >= 1).await;
    wait_for_policy_runs(&app, 1).await;
    assert_eq!(count_calls(&calls, "set_power main false"), 0);
    assert_eq!(statuses.lock().await["main"].input, "hdmi1");
}

#[tokio::test]
async fn test_stop_cancels_background_policy() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 151),
    ]);
    let calls = mock.calls.clone();
    let takeover = mock.takeover.clone();
    let timing = api::PolicyTiming {
        poll_interval: std::time::Duration::from_millis(100),
        takeover_timeout: std::time::Duration::from_secs(2),
        reapply_delay: std::time::Duration::from_millis(30),
        power_poll_interval: std::time::Duration::from_millis(10),
        power_wait_timeout: std::time::Duration::from_millis(500),
        volume_verify_delay: std::time::Duration::from_millis(50),
        ..fast_timing()
    };
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, timing, None).await;
    let server = &app.server;

    *takeover.lock().unwrap() = Takeover { armed: true, polls_left: 6 };
    server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await
        .assert_status_ok();
    // Stop before the background task's first poll tick; it must not act afterwards.
    server.post("/api/stop").await.assert_status_ok();

    // The cancelled task exits at its first poll tick without acting.
    wait_for_policy_runs(&app, 1).await;
    assert!(calls.lock().unwrap().is_empty(), "stale policy task acted: {:?}", calls.lock().unwrap());
}

#[tokio::test]
async fn test_policy_undoes_receiver_volume_bump_once() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "standby", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let takeover = mock.takeover.clone();
    // The receiver forces zone2 to 161 (0 dB) when the start volume 151 is set.
    *mock.override_volume_once.lock().unwrap() = Some(("zone2".to_string(), 151, 161));
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    let server = &app.server;

    *takeover.lock().unwrap() = Takeover { armed: true, polls_left: 6 };
    server
        .post("/api/play")
        .json(&json!({"station": "test", "zones": {"zone2": true}}))
        .await
        .assert_status_ok();

    wait_for("bump undone", || count_calls(&calls, "set_volume zone2 151") >= 2).await;
    wait_for_policy_runs(&app, 1).await;
    assert_eq!(count_calls(&calls, "set_volume zone2 151"), 2);
    assert_eq!(statuses.lock().await["zone2"].volume, 151);
}

// ---- Fake Yamaha receiver (real HTTP) for busy-retry / settle tests ----

#[derive(Default)]
struct FakeReceiver {
    zones: HashMap<String, (String, String, i32)>, // power, input, volume
    /// Setters answer code 5 until this instant (set by power-on).
    busy_until: Option<std::time::Instant>,
    /// Extra setVolume calls that answer code 5, regardless of time.
    busy_volume_calls: usize,
    /// Power reads as standby until this instant after setPower on.
    power_visible_at: Option<std::time::Instant>,
    /// Setters always answer code 5.
    always_busy: bool,
    /// Volume the receiver forces after power-on, and when.
    bump_to: Option<(i32, std::time::Duration)>,
    log: Vec<String>,
}

type SharedFake = Arc<std::sync::Mutex<FakeReceiver>>;

async fn fake_handler(
    axum::extract::State(fake): axum::extract::State<SharedFake>,
    axum::extract::Path((zone, op)): axum::extract::Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    let mut guard = fake.lock().unwrap();
    let now = std::time::Instant::now();
    if op == "getStatus" {
        let (power, input, volume) = guard.zones.get(&zone).cloned().unwrap();
        let power = match guard.power_visible_at {
            Some(at) if power == "on" && now < at => "standby".to_string(),
            _ => power,
        };
        return axum::Json(json!({
            "response_code": 0, "power": power, "input": input,
            "mute": false, "volume": volume,
        }));
    }
    let is_setter = op.starts_with("set");
    let busy = guard.always_busy
        || guard.busy_until.is_some_and(|until| now < until)
        || (op == "setVolume" && guard.busy_volume_calls > 0);
    if op == "setVolume" && guard.busy_volume_calls > 0 {
        guard.busy_volume_calls -= 1;
    }
    guard.log.push(format!("{op} {zone} {} busy={busy}", query.values().next().cloned().unwrap_or_default()));
    if is_setter && busy && op != "setInput" {
        return axum::Json(json!({"response_code": 5}));
    }
    let entry = guard.zones.get_mut(&zone).unwrap();
    match op.as_str() {
        "setPower" => {
            entry.0 = query["power"].clone();
            if query["power"] == "on" {
                guard.busy_until = Some(now + std::time::Duration::from_millis(250));
                guard.power_visible_at = Some(now + std::time::Duration::from_millis(100));
                if let Some((bump, after)) = guard.bump_to {
                    let fake = fake.clone();
                    let zone = zone.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        let mut guard = fake.lock().unwrap();
                        guard.log.push(format!("bump {zone} {bump}"));
                        guard.zones.get_mut(&zone).unwrap().2 = bump;
                    });
                }
            }
        }
        "setInput" => entry.1 = query["input"].clone(),
        "setVolume" => entry.2 = query["volume"].parse().unwrap(),
        _ => {}
    }
    axum::Json(json!({"response_code": 0}))
}

async fn spawn_fake_receiver(fake: SharedFake) -> String {
    let router = axum::Router::new()
        .route("/YamahaExtendedControl/v1/netusb/getPlayInfo", axum::routing::get(|| async {
            axum::Json(json!({"response_code": 0, "playback": "stop"}))
        }))
        .route("/YamahaExtendedControl/v1/{zone}/{op}", axum::routing::get(fake_handler))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

fn fake_with_zones() -> SharedFake {
    let mut fake = FakeReceiver::default();
    fake.zones.insert("main".into(), ("standby".into(), "airplay".into(), 95));
    fake.zones.insert("zone2".into(), ("standby".into(), "airplay".into(), 120));
    Arc::new(std::sync::Mutex::new(fake))
}

fn http_client(base_url: String) -> Arc<dyn yxc::YxcClient> {
    Arc::new(yxc::HttpYxcClient::new(base_url).with_busy_retry(
        std::time::Duration::from_millis(40),
        std::time::Duration::from_secs(2),
    ))
}

#[tokio::test]
async fn test_http_client_retries_busy_setter_then_succeeds() {
    let fake = fake_with_zones();
    fake.lock().unwrap().busy_volume_calls = 3;
    let base_url = spawn_fake_receiver(fake.clone()).await;
    let client = http_client(base_url);

    client.set_volume("zone2", 130).await.unwrap();

    let guard = fake.lock().unwrap();
    assert_eq!(guard.zones["zone2"].2, 130);
    assert_eq!(guard.log.iter().filter(|l| l.starts_with("setVolume")).count(), 4);
}

#[tokio::test]
async fn test_http_client_gives_up_when_receiver_stays_busy() {
    let fake = fake_with_zones();
    {
        let mut guard = fake.lock().unwrap();
        guard.always_busy = true;
        // Powered on, so the volume call reaches the receiver (standby is 409).
        guard.zones.get_mut("zone2").unwrap().0 = "on".to_string();
    }
    let base_url = spawn_fake_receiver(fake.clone()).await;
    let client: Arc<dyn yxc::YxcClient> = Arc::new(
        yxc::HttpYxcClient::new(base_url).with_busy_retry(
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(150),
        ),
    );

    assert!(matches!(client.set_volume("zone2", 130).await, Err(yxc::YxcError::ApiError(_))));

    // Through the API it is a 502 "busy", not a 500.
    let server = create_test_app(client).await;
    let response = server
        .post("/api/zone/zone2/volume")
        .json(&json!({"db": -20.0}))
        .await;
    response.assert_status(axum::http::StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"], "receiver_busy");
    assert_eq!(body["detail"], "Receiver busy — try again");
}

#[tokio::test]
async fn test_power_on_survives_busy_receiver_and_undoes_forced_volume() {
    let fake = fake_with_zones();
    // After power-on the receiver is busy for 250 ms, and forces 161 (0 dB) at 400 ms.
    fake.lock().unwrap().bump_to = Some((161, std::time::Duration::from_millis(400)));
    let base_url = spawn_fake_receiver(fake.clone()).await;
    let client = http_client(base_url);
    let server = create_test_app_with_timing(client, fast_timing_with_verify(900)).await;

    let response = server
        .post("/api/zone/zone2/power")
        .json(&json!({"on": true}))
        .await;
    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    // Start volume applied (-5 dB = raw 151) despite the busy window.
    assert_eq!(body["zones"]["zone2"]["volume"], 151);
    assert_eq!(body["zones"]["zone2"]["power"], "on");

    // The receiver bumps to 161 at 400 ms; the verify pass at ~900 ms puts 151 back.
    for _ in 0..200 {
        let volume = fake.lock().unwrap().zones["zone2"].2;
        let bumped = fake.lock().unwrap().log.iter().any(|l| l.starts_with("bump"));
        if bumped && volume == 151 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("start volume not restored: {:?}", fake.lock().unwrap().log);
}

fn fast_timing_with_verify(verify_ms: u64) -> api::PolicyTiming {
    api::PolicyTiming {
        volume_verify_delay: std::time::Duration::from_millis(verify_ms),
        ..fast_timing()
    }
}

// ---- AirPlay sink lifecycle ----

fn ms(millis: u64) -> std::time::Duration {
    std::time::Duration::from_millis(millis)
}

#[tokio::test]
async fn test_play_snapshots_before_connect_and_connects_before_cliamp() {
    // Media Room TV on hdmi1; Upstairs (selected) in standby. Connecting the sink
    // makes the receiver grab BOTH zones, so a snapshot taken after connect would
    // see the TV as "airplay" and power it off instead of restoring hdmi1.
    let mock = MockYxcClient::with_zones(&[
        ("main", "on", "hdmi1", 100),
        ("zone2", "standby", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let app = build_app(
        Arc::new(mock) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        Some(statuses.clone()),
    )
    .await;

    app.server
        .post("/api/play")
        .json(&json!({"station": "test", "zones": {"zone2": true}}))
        .await
        .assert_status_ok();

    // Connect first, then cliamp.
    assert_eq!(
        *app.events.lock().unwrap(),
        vec!["connect".to_string(), "player.play".to_string()]
    );

    wait_for("TV input restored", || count_calls(&calls, "set_input main hdmi1") >= 1).await;
    wait_for_policy_runs(&app, 1).await;
    assert_eq!(count_calls(&calls, "set_power main false"), 0);
    let statuses = statuses.lock().await;
    assert_eq!(statuses["main"].input, "hdmi1");
    assert_eq!(statuses["main"].volume, 100);
}

#[tokio::test]
async fn test_play_fails_cleanly_when_sink_cannot_start() {
    let app = build_app(
        Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        None,
    )
    .await;
    app.route.fail_connect.store(true, std::sync::atomic::Ordering::SeqCst);

    let response = app.server.post("/api/play").json(&json!({"station": "test"})).await;
    response.assert_status(axum::http::StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"], "audio_route_error");
    // cliamp was never told to play.
    assert_eq!(count_events(&app.events, "player.play"), 0);
}

#[tokio::test]
async fn test_stop_stops_player_then_disconnects_sink() {
    let app = build_app(
        Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        None,
    )
    .await;
    app.server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await
        .assert_status_ok();
    app.events.lock().unwrap().clear();

    app.server.post("/api/stop").await.assert_status_ok();
    assert_eq!(
        *app.events.lock().unwrap(),
        vec!["player.stop".to_string(), "disconnect".to_string()]
    );
}

#[tokio::test]
async fn test_healthz_reports_airplay_connection() {
    let app = build_app(
        Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        None,
    )
    .await;

    let body: serde_json::Value = app.server.get("/healthz").await.json();
    assert_eq!(body["airplay"]["connected"], false);

    app.server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await
        .assert_status_ok();
    let body: serde_json::Value = app.server.get("/healthz").await.json();
    assert_eq!(body["airplay"]["connected"], true);

    app.server.post("/api/stop").await.assert_status_ok();
    let body: serde_json::Value = app.server.get("/healthz").await.json();
    assert_eq!(body["airplay"]["connected"], false);
}

/// Mock receiver whose netusb playback reads `playback`.
async fn set_playback(mock_play_info: &Arc<Mutex<yxc::PlayInfo>>, playback: &str) {
    mock_play_info.lock().await.playback = playback.to_string();
}

#[tokio::test]
async fn test_watchdog_reconnects_when_receiver_stops_playing() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 151),
    ]);
    let play_info = mock.play_info.clone();
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let app = build_app(
        Arc::new(mock) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        Some(statuses.clone()),
    )
    .await;

    // Play Upstairs only; the connect-time grab powers Media Room, which the
    // policy puts back in standby.
    set_playback(&play_info, "play").await;
    app.server
        .post("/api/play")
        .json(&json!({"station": "test", "zones": {"zone2": true}}))
        .await
        .assert_status_ok();
    wait_for("main back in standby", || count_calls(&calls, "set_power main false") >= 1).await;
    tokio::time::sleep(ms(150)).await;

    // The receiver drops the session: cliamp still plays, receiver stopped.
    set_playback(&play_info, "stop").await;
    api::start_watchdog_task(app.state.clone());

    wait_for("watchdog reconnect", || count_events(&app.events, "reconnect") >= 1).await;
    // After the reconnect the re-grab powers Media Room again; the policy, run
    // with the last play's selection (Upstairs only), puts it back in standby.
    wait_for("policy re-applied after reconnect", || {
        count_calls(&calls, "set_power main false") >= 2
    })
    .await;
    assert_eq!(statuses.lock().await["main"].power, "standby");
    assert_eq!(statuses.lock().await["zone2"].power, "on");

    // Rate limit: the stall persists but no second reconnect within the window.
    tokio::time::sleep(ms(500)).await;
    assert_eq!(count_events(&app.events, "reconnect"), 1);
}

#[tokio::test]
async fn test_watchdog_does_not_reconnect_when_receiver_is_playing() {
    // Playback "play" (possibly another AirPlay sender) is never a stall.
    let app = build_app(
        Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        None,
    )
    .await;
    app.server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await
        .assert_status_ok();
    api::start_watchdog_task(app.state.clone());

    tokio::time::sleep(ms(600)).await;
    assert_eq!(count_events(&app.events, "reconnect"), 0);
}

#[tokio::test]
async fn test_watchdog_ignores_stopped_player_and_zones_not_on_radio() {
    // Receiver not playing, but cliamp is stopped: nothing to reconnect.
    let mock = MockYxcClient::new();
    set_playback(&mock.play_info, "stop").await;
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    api::start_watchdog_task(app.state.clone());
    tokio::time::sleep(ms(400)).await;
    assert_eq!(count_events(&app.events, "reconnect"), 0);

    // cliamp playing and receiver idle, but no zone is on the radio input.
    let mock = MockYxcClient::with_zones(&[
        ("main", "on", "hdmi1", 100),
        ("zone2", "standby", "airplay", 120),
    ]);
    set_playback(&mock.play_info, "stop").await;
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    cliamp::Player::play(&*app.player, "https://test.example.com/stream").await.unwrap();
    api::start_watchdog_task(app.state.clone());
    tokio::time::sleep(ms(400)).await;
    assert_eq!(count_events(&app.events, "reconnect"), 0);
}

#[tokio::test]
async fn test_watchdog_waits_for_settle_after_play() {
    let mock = MockYxcClient::new();
    set_playback(&mock.play_info, "stop").await;
    let timing = api::PolicyTiming {
        watchdog_settle: std::time::Duration::from_secs(5),
        ..fast_timing()
    };
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, timing, None).await;
    app.server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await
        .assert_status_ok();
    api::start_watchdog_task(app.state.clone());

    // The stall exceeds the hold time, but the play was too recent to judge.
    tokio::time::sleep(ms(500)).await;
    assert_eq!(count_events(&app.events, "reconnect"), 0);
}

#[tokio::test]
async fn test_power_on_while_playing_reconnects_when_receiver_stays_idle() {
    // Playing on Upstairs; Media Room is in standby. The receiver reads idle.
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "standby", "airplay", 120),
    ]);
    let play_info = mock.play_info.clone();
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    set_playback(&play_info, "stop").await;
    let app = build_app(
        Arc::new(mock) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        Some(statuses.clone()),
    )
    .await;
    cliamp::Player::play(&*app.player, "https://test.example.com/stream").await.unwrap();

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();

    wait_for("re-grab reconnect", || count_events(&app.events, "reconnect") >= 1).await;
    // The re-grab powered Upstairs on too; only Media Room is selected, so the
    // policy puts Upstairs back in standby and leaves Media Room alone.
    wait_for("other zone put back", || count_calls(&calls, "set_power zone2 false") >= 1).await;
    wait_for_policy_runs(&app, 1).await;
    assert_eq!(count_calls(&calls, "set_power main false"), 0);
    assert_eq!(statuses.lock().await["main"].power, "on");
    assert_eq!(statuses.lock().await["zone2"].power, "standby");
}

#[tokio::test]
async fn test_power_on_while_playing_does_not_reconnect_when_receiver_plays() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 151),
    ]);
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    cliamp::Player::play(&*app.player, "https://test.example.com/stream").await.unwrap();

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();

    tokio::time::sleep(ms(500)).await;
    assert_eq!(count_events(&app.events, "reconnect"), 0);
}

#[tokio::test]
async fn test_power_on_while_stopped_never_reconnects() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "standby", "airplay", 120),
    ]);
    set_playback(&mock.play_info, "stop").await;
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();
    tokio::time::sleep(ms(400)).await;
    assert!(app.events.lock().unwrap().is_empty());
}

// ---- Override semantics ----

#[tokio::test]
async fn test_step_after_pending_write_uses_the_written_volume() {
    // The receiver lags: it still reads 95 after we wrote 93. A step must build on
    // the 93 we wrote, not on the stale hardware readback.
    let server = create_test_app(Arc::new(MockYxcClient::lagging()) as Arc<dyn yxc::YxcClient>).await;

    server
        .post("/api/zone/main/volume")
        .json(&json!({"db": -34.0}))
        .await
        .assert_status_ok();
    let body: serde_json::Value = server
        .post("/api/zone/main/volume")
        .json(&json!({"step": 2}))
        .await
        .json();
    assert_eq!(body["zones"]["main"]["volume"], 95);

    let body: serde_json::Value = server
        .post("/api/zone/main/volume")
        .json(&json!({"step": -1}))
        .await
        .json();
    assert_eq!(body["zones"]["main"]["volume"], 94);
}

#[tokio::test]
async fn test_receiver_airplay_active_follows_overridden_zones() {
    // The receiver lags: it still reports both zones on+airplay and playing.
    let server = create_test_app(Arc::new(MockYxcClient::lagging()) as Arc<dyn yxc::YxcClient>).await;
    let body: serde_json::Value = server.get("/api/state").await.json();
    assert_eq!(body["receiver"]["airplay_active"], true);

    server
        .post("/api/zone/main/power")
        .json(&json!({"on": false}))
        .await
        .assert_status_ok();
    let body: serde_json::Value = server
        .post("/api/zone/zone2/power")
        .json(&json!({"on": false}))
        .await
        .json();
    // Both zones are overridden to standby, so no zone is on the radio.
    assert_eq!(body["zones"]["main"]["radio"], false);
    assert_eq!(body["zones"]["zone2"]["radio"], false);
    assert_eq!(body["receiver"]["airplay_active"], false);

    let body: serde_json::Value = server.get("/api/state").await.json();
    assert_eq!(body["receiver"]["airplay_active"], false);
}

#[tokio::test]
async fn test_override_expires_per_field_not_per_zone() {
    // Lagging receiver keeps reading volume 95 / mute false whatever we write.
    let app = build_app(
        Arc::new(MockYxcClient::lagging()) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        None,
    )
    .await;

    app.server
        .post("/api/zone/main/volume")
        .json(&json!({"db": -34.0}))
        .await
        .assert_status_ok();
    tokio::time::sleep(ms(1200)).await;
    // A later mute write must not keep the (older) volume override alive.
    app.server
        .post("/api/zone/main/mute")
        .json(&json!({"mute": true}))
        .await
        .assert_status_ok();
    tokio::time::sleep(ms(1200)).await;

    // Volume is 2.4 s old (expired: receiver's 95 shows); mute is 1.2 s old (alive).
    app.state.state_manager.refresh().await;
    let body: serde_json::Value = app.server.get("/api/state").await.json();
    assert_eq!(body["zones"]["main"]["volume"], 95);
    assert_eq!(body["zones"]["main"]["mute"], true);
}

#[tokio::test]
async fn test_volume_and_mute_on_standby_zone_are_409_zone_off_without_yxc_calls() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 151),
    ]);
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    for (path, body) in [
        ("/api/zone/main/volume", json!({"db": -30.0})),
        ("/api/zone/main/volume", json!({"step": 2})),
        ("/api/zone/main/mute", json!({"mute": true})),
    ] {
        let response = server.post(path).json(&body).await;
        response.assert_status(axum::http::StatusCode::CONFLICT);
        let body: serde_json::Value = response.json();
        assert_eq!(body["error"], "zone_off");
        assert_eq!(body["detail"], "Turn Media Room on first");
    }
    assert!(calls.lock().unwrap().is_empty(), "YXC was called: {:?}", calls.lock().unwrap());

    // A zone that is on is unaffected.
    server
        .post("/api/zone/zone2/volume")
        .json(&json!({"db": -10.0}))
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_volume_and_mute_respect_pending_power_override() {
    // Receiver lags: it still reads "on" right after we power the zone off, but
    // the pending override says standby, so volume/mute are refused.
    let mock = MockYxcClient::lagging();
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    server
        .post("/api/zone/zone2/power")
        .json(&json!({"on": false}))
        .await
        .assert_status_ok();

    let response = server.post("/api/zone/zone2/volume").json(&json!({"db": -10.0})).await;
    response.assert_status(axum::http::StatusCode::CONFLICT);
    let body: serde_json::Value = response.json();
    assert_eq!(body["detail"], "Turn Upstairs on first");
    server
        .post("/api/zone/zone2/mute")
        .json(&json!({"mute": true}))
        .await
        .assert_status(axum::http::StatusCode::CONFLICT);
    assert_eq!(count_calls(&calls, "set_volume zone2 151"), 0);
    assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("set_volume") || c.starts_with("set_mute")));
}

#[tokio::test]
async fn test_station_matches_on_logical_track_path_not_track_path() {
    let app = build_app(Arc::new(MockYxcClient::new()) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    let stream = "https://test.example.com/stream";

    // `track.path` is a resolved/redirected URL; the registry identity is in
    // `logical_track.path`.
    {
        let mut player = app.player.state.lock().await;
        player.state = "playing".to_string();
        player.url = Some("https://cdn.example.net/redirected".to_string());
        player.station_url = Some(stream.to_string());
    }
    app.state.state_manager.refresh().await;
    let body: serde_json::Value = app.server.get("/api/state").await.json();
    assert_eq!(body["player"]["station"], "test");

    // Only `track.path` known: it still identifies the station.
    {
        let mut player = app.player.state.lock().await;
        player.url = Some(stream.to_string());
        player.station_url = None;
    }
    app.state.state_manager.refresh().await;
    let body: serde_json::Value = app.server.get("/api/state").await.json();
    assert_eq!(body["player"]["station"], "test");

    // A matching `track.path` does not override a different registry identity.
    {
        let mut player = app.player.state.lock().await;
        player.url = Some(stream.to_string());
        player.station_url = Some("https://other.example.com/x".to_string());
    }
    app.state.state_manager.refresh().await;
    let body: serde_json::Value = app.server.get("/api/state").await.json();
    assert_eq!(body["player"]["station"], serde_json::Value::Null);
}

// ---- Pending overrides vs. play / power-off / validation ----

#[tokio::test]
async fn test_play_default_selection_honours_pending_power_off() {
    // Lagging receiver: it keeps reading both zones on+airplay after power-off.
    let app = build_app(
        Arc::new(MockYxcClient::lagging()) as Arc<dyn yxc::YxcClient>,
        fast_timing(),
        None,
    )
    .await;

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": false}))
        .await
        .assert_status_ok();

    // Only Upstairs is still a radio zone; Media Room must not be re-selected.
    app.server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await
        .assert_status_ok();
    {
        let tracking = app.state.route_tracking.lock().unwrap();
        let selection = tracking.selection.clone().unwrap();
        assert!(!selection.main && selection.zone2, "selection was {selection:?}");
    }

    // Turn Upstairs off too: now nothing is selectable.
    app.server
        .post("/api/zone/zone2/power")
        .json(&json!({"on": false}))
        .await
        .assert_status_ok();
    let response = app.server.post("/api/play").json(&json!({"station": "test"})).await;
    response.assert_status(axum::http::StatusCode::CONFLICT);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"], "no_zone");
}

#[tokio::test]
async fn test_play_default_selection_honours_pending_power_on() {
    // Both zones in standby; the lagging receiver keeps reading standby after
    // the power-on, so play must see the pending "on" and not answer no_zone.
    let mock = MockYxcClient {
        lagging: true,
        ..MockYxcClient::with_zones(&[
            ("main", "standby", "airplay", 95),
            ("zone2", "standby", "airplay", 120),
        ])
    };
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();
    app.server
        .post("/api/play")
        .json(&json!({"station": "test"}))
        .await
        .assert_status_ok();
    let tracking = app.state.route_tracking.lock().unwrap();
    let selection = tracking.selection.clone().unwrap();
    assert!(selection.main && !selection.zone2, "selection was {selection:?}");
}

#[tokio::test]
async fn test_rejected_play_leaves_running_policy_task_alone() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 151),
    ]);
    let calls = mock.calls.clone();
    let takeover = mock.takeover.clone();
    let timing = api::PolicyTiming {
        poll_interval: ms(100),
        ..fast_timing()
    };
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, timing, None).await;

    *takeover.lock().unwrap() = Takeover { armed: true, polls_left: 6 };
    app.server
        .post("/api/play")
        .json(&json!({"station": "test", "zones": {"zone2": true}}))
        .await
        .assert_status_ok();

    // While the policy task is still waiting for the takeover, a play that is
    // rejected must not cancel it.
    app.server
        .post("/api/play")
        .json(&json!({"station": "nonexistent"}))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    wait_for("policy still ran: main put back to standby", || {
        count_calls(&calls, "set_power main false") >= 1
    })
    .await;
}

#[tokio::test]
async fn test_power_off_after_pending_power_on_powers_the_zone_down() {
    // Media Room starts in standby on hdmi1. The lagging receiver never applies
    // our writes, so the readback still says hdmi1 after the power-on.
    let mock = MockYxcClient {
        lagging: true,
        ..MockYxcClient::with_zones(&[
            ("main", "standby", "hdmi1", 95),
            ("zone2", "standby", "airplay", 120),
        ])
    };
    let calls = mock.calls.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();
    assert!(count_calls(&calls, "set_input main airplay") >= 1);

    // The pending input=airplay write means this is our zone: power it down.
    let body: serde_json::Value = app
        .server
        .post("/api/zone/main/power")
        .json(&json!({"on": false}))
        .await
        .json();
    assert_eq!(count_calls(&calls, "set_power main false"), 1);
    assert_eq!(body["zones"]["main"]["power"], "standby");
}

#[tokio::test]
async fn test_power_off_still_leaves_a_zone_on_another_input_alone() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "on", "hdmi1", 100),
        ("zone2", "standby", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    server
        .post("/api/zone/main/power")
        .json(&json!({"on": false}))
        .await
        .assert_status_ok();
    assert_eq!(count_calls(&calls, "set_power main false"), 0);
}

// ---- Master power ----

#[tokio::test]
async fn test_master_off_powers_down_both_zones_including_tv_and_stops_radio() {
    // The master switch is deliberately broader than the per-zone toggle: it
    // powers down the TV on hdmi1 too.
    let mock = MockYxcClient::with_zones(&[
        ("main", "on", "hdmi1", 100),
        ("zone2", "on", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;

    let body: serde_json::Value = app
        .server
        .post("/api/power")
        .json(&json!({"on": false}))
        .await
        .json();

    assert_eq!(count_calls(&calls, "set_power main false"), 1);
    assert_eq!(count_calls(&calls, "set_power zone2 false"), 1);
    assert_eq!(
        *app.events.lock().unwrap(),
        vec!["player.stop".to_string(), "disconnect".to_string()]
    );
    assert_eq!(body["zones"]["main"]["power"], "standby");
    assert_eq!(body["zones"]["zone2"]["power"], "standby");
}

#[tokio::test]
async fn test_master_off_resends_standby_to_a_zone_that_reads_standby() {
    // The readback lags: a zone that still reads standby may have a power-on
    // from a moment ago in flight, so the command is sent regardless.
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    server
        .post("/api/power")
        .json(&json!({"on": false}))
        .await
        .assert_status_ok();

    assert_eq!(count_calls(&calls, "set_power main false"), 1);
    assert_eq!(count_calls(&calls, "set_power zone2 false"), 1);
}

#[tokio::test]
async fn test_master_on_wakes_only_main_and_leaves_input_and_volume_alone() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "hdmi1", 95),
        ("zone2", "standby", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    let body: serde_json::Value = server
        .post("/api/power")
        .json(&json!({"on": true}))
        .await
        .json();

    // Power on main is the only write: no input, volume or zone2 changes.
    assert_eq!(*calls.lock().unwrap(), vec!["set_power main true".to_string()]);
    assert_eq!(body["zones"]["main"]["power"], "on");
    assert_eq!(body["zones"]["zone2"]["power"], "standby");
}

#[tokio::test]
async fn test_master_on_resends_power_when_main_reads_on() {
    // The readback lags: a main that still reads on may have a standby from a
    // moment ago in flight, so the command is sent regardless.
    let mock = MockYxcClient::with_zones(&[
        ("main", "on", "hdmi1", 100),
        ("zone2", "standby", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    server
        .post("/api/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();

    assert_eq!(count_calls(&calls, "set_power main true"), 1);
}

#[tokio::test]
async fn test_master_off_still_powers_down_zone2_when_main_fails_and_reports_it() {
    let mock = MockYxcClient {
        fail_set_power_for: Some("main".to_string()),
        ..MockYxcClient::with_zones(&[
            ("main", "on", "airplay", 95),
            ("zone2", "on", "airplay", 120),
        ])
    };
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    let response = server.post("/api/power").json(&json!({"on": false})).await;

    response.assert_status(axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(count_calls(&calls, "set_power main false"), 1);
    assert_eq!(count_calls(&calls, "set_power zone2 false"), 1);
}

#[tokio::test]
async fn test_master_off_still_sends_standby_when_the_status_read_fails() {
    let mock = MockYxcClient {
        fail_status_for: Some("main".to_string()),
        ..MockYxcClient::with_zones(&[
            ("main", "on", "airplay", 95),
            ("zone2", "on", "airplay", 120),
        ])
    };
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    server.post("/api/power").json(&json!({"on": false})).await;

    assert_eq!(count_calls(&calls, "set_power main false"), 1);
    assert_eq!(count_calls(&calls, "set_power zone2 false"), 1);
}

#[tokio::test]
async fn test_master_off_powers_down_zones_and_reports_when_the_sink_fails_to_stop() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "on", "airplay", 95),
        ("zone2", "on", "airplay", 120),
    ]);
    let calls = mock.calls.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    app.route.fail_disconnect.store(true, std::sync::atomic::Ordering::SeqCst);

    let response = app.server.post("/api/power").json(&json!({"on": false})).await;

    response.assert_status(axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(count_calls(&calls, "set_power main false"), 1);
    assert_eq!(count_calls(&calls, "set_power zone2 false"), 1);
}

#[tokio::test]
async fn test_master_on_while_playing_reconnects_when_receiver_stays_idle() {
    // Both zones were switched off with their own toggles while cliamp kept
    // playing. Waking main onto AirPlay must get the receiver to pick it up.
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "standby", "airplay", 120),
    ]);
    set_playback(&mock.play_info, "stop").await;
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    cliamp::Player::play(&*app.player, "https://test.example.com/stream").await.unwrap();

    app.server
        .post("/api/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();

    wait_for("re-grab reconnect", || count_events(&app.events, "reconnect") >= 1).await;
}

#[tokio::test]
async fn test_master_on_regrab_backs_off_when_a_tv_takes_main_meanwhile() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "standby", "airplay", 120),
    ]);
    set_playback(&mock.play_info, "stop").await;
    let statuses = mock.zone_status.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    cliamp::Player::play(&*app.player, "https://test.example.com/stream").await.unwrap();

    app.server
        .post("/api/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();
    // HDMI-CEC switches main to the TV while the re-grab is waiting.
    statuses.lock().await.get_mut("main").unwrap().input = "hdmi1".to_string();

    tokio::time::sleep(ms(500)).await;
    assert_eq!(count_events(&app.events, "reconnect"), 0);
}

#[tokio::test]
async fn test_master_on_while_playing_leaves_a_tv_input_alone() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "hdmi1", 95),
        ("zone2", "standby", "airplay", 120),
    ]);
    set_playback(&mock.play_info, "stop").await;
    let calls = mock.calls.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;
    cliamp::Player::play(&*app.player, "https://test.example.com/stream").await.unwrap();

    app.server
        .post("/api/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();

    tokio::time::sleep(ms(500)).await;
    assert_eq!(count_events(&app.events, "reconnect"), 0);
    assert_eq!(*calls.lock().unwrap(), vec!["set_power main true".to_string()]);
}

#[tokio::test]
async fn test_volume_validates_body_before_checking_zone_power() {
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 151),
    ]);
    let calls = mock.calls.clone();
    let server = create_test_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>).await;

    // Empty body to a standby zone is a 400, not a 409.
    let response = server.post("/api/zone/main/volume").json(&json!({})).await;
    response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"], "invalid_request");

    // A valid body still gets the zone_off check.
    server
        .post("/api/zone/main/volume")
        .json(&json!({"db": -30.0}))
        .await
        .assert_status(axum::http::StatusCode::CONFLICT);
    assert!(calls.lock().unwrap().is_empty());
}

/// Read SSE chunks from `response` until `needle` appears, or fail after 3 s.
async fn read_sse_until(response: &mut reqwest::Response, needle: &str) -> String {
    let mut received = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while !received.contains(needle) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let chunk = tokio::time::timeout(remaining, response.chunk())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}, got {received:?}"))
            .unwrap()
            .expect("stream ended");
        received.push_str(&String::from_utf8_lossy(&chunk));
    }
    received
}

#[tokio::test]
async fn vis_endpoint_streams_zero_frame_when_stopped_then_live_frames() {
    let app = build_app(
        Arc::new(MockYxcClient::new()),
        api::PolicyTiming::default(),
        None,
    )
    .await;

    // Serve on a local ephemeral port: axum-test cannot read an endless stream.
    let mut state = app.state.clone();
    state.vis = vis::VisHub::start(
        Box::new(ScriptedVisSource {
            lines: vec![
                "garbage".to_string(),
                r#"{"ok":true,"visualizer":"Bars","bands":[0.5,0.25,0,0,0,0,0,0,0,1]}"#.to_string(),
            ],
        }),
        app.player.clone() as Arc<dyn cliamp::Player>,
        vis::VisConfig {
            poll_interval: std::time::Duration::from_millis(20),
            ..vis::VisConfig::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::create_router(state)).await.unwrap();
    });
    let url = format!("http://{address}/api/vis");

    // Stopped: one all-zero frame, `event: vis`.
    let mut stopped = reqwest::get(&url).await.unwrap();
    assert_eq!(stopped.status(), 200);
    assert_eq!(
        stopped.headers()[reqwest::header::CONTENT_TYPE],
        "text/event-stream"
    );
    let received = read_sse_until(&mut stopped, "\n\n").await;
    assert!(received.contains("event: vis"), "{received:?}");
    assert!(
        received.contains(r#"data: {"bands":[0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}"#),
        "{received:?}"
    );
    drop(stopped);

    // Playing: the scripted frame is relayed, the malformed line is not.
    cliamp::Player::play(app.player.as_ref(), "https://test.example.com/stream")
        .await
        .unwrap();
    let mut playing = reqwest::get(&url).await.unwrap();
    let received = read_sse_until(&mut playing, "0.25").await;
    assert!(
        received.contains(r#"data: {"bands":[0.5,0.25,0.0,0.0,0.0,0.0,0.0,0.0,0.0,1.0]}"#),
        "{received:?}"
    );
    assert!(!received.contains("garbage"));
}

// ---- Pending overrides must never mask a TV that grabbed a zone ----

/// A lagging receiver (it never applies our writes) with main in standby and
/// upstairs on, for the hdmi race tests below.
fn lagging_with_main_in_standby() -> MockYxcClient {
    MockYxcClient {
        lagging: true,
        ..MockYxcClient::with_zones(&[
            ("main", "standby", "airplay", 95),
            ("zone2", "on", "airplay", 151),
        ])
    }
}

/// Simulate the TV switching Media Room on to hdmi1 behind our back.
async fn tv_grabs_main(statuses: &Arc<Mutex<HashMap<String, yxc::ZoneStatus>>>) {
    let mut statuses = statuses.lock().await;
    let main = statuses.get_mut("main").unwrap();
    main.power = "on".to_string();
    main.input = "hdmi1".to_string();
    main.volume = 100;
}

#[tokio::test]
async fn test_play_after_toggle_does_not_power_off_tv_that_grabbed_main() {
    let mock = lagging_with_main_in_standby();
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let timing = api::PolicyTiming {
        takeover_timeout: ms(150),
        ..fast_timing()
    };
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, timing, None).await;

    // Media Room is toggled on (pending: on + airplay)...
    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();
    // ...then the TV switches it to hdmi1 within the override TTL.
    tv_grabs_main(&statuses).await;

    // An Upstairs-only play must see the TV, not the stale override.
    app.server
        .post("/api/play")
        .json(&json!({"station": "test", "zones": {"zone2": true}}))
        .await
        .assert_status_ok();
    wait_for_policy_runs(&app, 1).await;

    assert_eq!(count_calls(&calls, "set_power main false"), 0);
    // The TV is treated as a TV: kept on and its volume put back.
    assert!(count_calls(&calls, "set_volume main 100") >= 1);
    assert_eq!(count_calls(&calls, "set_input main airplay"), 1, "only the toggle itself set airplay");
}

#[tokio::test]
async fn test_power_off_after_toggle_leaves_tv_that_grabbed_main_alone() {
    let mock = lagging_with_main_in_standby();
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();
    tv_grabs_main(&statuses).await;

    let body: serde_json::Value = app
        .server
        .post("/api/zone/main/power")
        .json(&json!({"on": false}))
        .await
        .json();
    assert_eq!(count_calls(&calls, "set_power main false"), 0);
    assert_eq!(body["zones"]["main"]["power"], "on");
    assert_eq!(body["zones"]["main"]["input"], "hdmi1");
    assert_eq!(body["zones"]["main"]["radio"], false);
}

#[tokio::test]
async fn test_override_never_powers_off_a_zone_live_on_another_input() {
    // Media Room is on hdmi1 and the receiver lags. Toggling it on writes
    // input=airplay, which the readback has not caught up with: the override
    // still "applies", but a zone that is live on hdmi1 is never powered off.
    let mock = MockYxcClient {
        lagging: true,
        ..MockYxcClient::with_zones(&[
            ("main", "on", "hdmi1", 100),
            ("zone2", "standby", "airplay", 120),
        ])
    };
    let calls = mock.calls.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": true}))
        .await
        .assert_status_ok();
    assert!(count_calls(&calls, "set_input main airplay") >= 1);

    app.server
        .post("/api/zone/main/power")
        .json(&json!({"on": false}))
        .await
        .assert_status_ok();
    assert_eq!(count_calls(&calls, "set_power main false"), 0);
}

#[tokio::test]
async fn test_policy_never_powers_off_a_zone_live_on_another_source() {
    // Media Room was in standby at play time (so the policy plans to keep it
    // down), but a TV switches it on to hdmi1 before the policy acts.
    let mock = MockYxcClient::with_zones(&[
        ("main", "standby", "airplay", 95),
        ("zone2", "on", "airplay", 151),
    ]);
    let calls = mock.calls.clone();
    let statuses = mock.zone_status.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;

    app.server
        .post("/api/play")
        .json(&json!({"station": "test", "zones": {"zone2": true}}))
        .await
        .assert_status_ok();
    tv_grabs_main(&statuses).await;
    wait_for_policy_runs(&app, 1).await;

    assert_eq!(count_calls(&calls, "set_power main false"), 0);
    assert_eq!(statuses.lock().await["main"].power, "on");
}

// ---- Power, play and stop are serialized ----

#[tokio::test]
async fn test_power_and_stop_wait_for_the_play_mutex() {
    let mock = MockYxcClient::new();
    let calls = mock.calls.clone();
    let app = build_app(Arc::new(mock) as Arc<dyn yxc::YxcClient>, fast_timing(), None).await;

    // While a play holds the mutex, a power call must not touch the receiver or
    // bump the generation; it proceeds once the play is done.
    let play_in_flight = app.state.play_mutex.lock().await;
    let generation_before = app.state.generation.load(std::sync::atomic::Ordering::SeqCst);
    let power_call = async {
        app.server
            .post("/api/zone/main/power")
            .json(&json!({"on": false}))
            .await
            .assert_status_ok();
    };
    let stop_call = async {
        app.server.post("/api/stop").await.assert_status_ok();
    };
    let observer = async {
        tokio::time::sleep(ms(150)).await;
        assert!(calls.lock().unwrap().is_empty(), "power ran during a play: {:?}", calls.lock().unwrap());
        assert_eq!(count_events(&app.events, "player.stop"), 0, "stop ran during a play");
        assert_eq!(
            app.state.generation.load(std::sync::atomic::Ordering::SeqCst),
            generation_before
        );
        drop(play_in_flight);
    };
    tokio::join!(power_call, stop_call, observer);

    assert_eq!(count_calls(&calls, "set_power main false"), 1);
    assert_eq!(count_events(&app.events, "player.stop"), 1);
}

// ---- Station discovery: search ----

async fn discovery_app() -> TestApp {
    build_app(Arc::new(MockYxcClient::new()), fast_timing(), None).await
}

#[tokio::test]
async fn search_by_name_returns_ids_only_results() {
    let app = discovery_app().await;

    let response = app.server.get("/api/search").add_query_param("q", "jazz").await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], format!("rb-{JAZZ_UUID}"));
    assert_eq!(results[0]["name"], "Smooth Jazz Radio");
    assert_eq!(results[0]["genre"], "jazz");
    assert_eq!(results[0]["country"], "Canada");
    assert_eq!(results[0]["bitrate"], 128);
    assert_eq!(results[0]["in_my"], false);
    // The stream URL never leaves the server.
    assert!(!body.to_string().contains("example.com"));
}

#[tokio::test]
async fn search_maps_the_genre_chip_to_a_radio_browser_tag() {
    let app = discovery_app().await;

    app.server
        .get("/api/search")
        .add_query_param("genre", "Alt")
        .await
        .assert_status_ok();
    app.server
        .get("/api/search")
        .add_query_param("q", "  radio  ")
        .add_query_param("genre", "Classic Rock")
        .await
        .assert_status_ok();

    let searches = app.radio_browser.searches.lock().unwrap().clone();
    assert_eq!(
        searches,
        vec![
            (None, Some("alternative".to_string())),
            (Some("radio".to_string()), Some("classic rock".to_string())),
        ]
    );
}

#[tokio::test]
async fn search_rejects_bad_queries() {
    let app = discovery_app().await;

    for query in [
        vec![],
        vec![("q", "a")],
        vec![("q", "   ")],
        vec![("genre", "Polka")],
        vec![("q", "jazz"), ("genre", "Polka")],
    ] {
        let mut request = app.server.get("/api/search");
        for (key, value) in query {
            request = request.add_query_param(key, value);
        }
        let response = request.await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let body: serde_json::Value = response.json();
        assert_eq!(body["error"], "bad_query");
    }
    assert!(app.radio_browser.searches.lock().unwrap().is_empty());
}

#[tokio::test]
async fn search_reports_unavailable_when_radio_browser_is_down() {
    let app = discovery_app().await;
    app.radio_browser.set_down(true);

    let response = app.server.get("/api/search").add_query_param("q", "jazz").await;

    response.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"], "search_unavailable");
}

#[tokio::test]
async fn stations_view_has_an_empty_my_group_and_in_my_flags() {
    let app = discovery_app().await;

    let response = app.server.get("/api/stations").await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    let groups = body["groups"].as_array().unwrap();
    let my = groups.iter().find(|group| group["id"] == "my").unwrap();
    assert_eq!(my["label"], "MY");
    assert_eq!(my["stations"].as_array().unwrap().len(), 0);
    let first_curated = &groups[0]["stations"][0];
    assert_eq!(first_curated["id"], "test");
    assert_eq!(first_curated["in_my"], false);
}

// ---- Station discovery: the MY list ----

/// The ids in the `my` group of a `Stations` body, in order.
fn my_ids(stations_body: &serde_json::Value) -> Vec<String> {
    let groups = stations_body["groups"].as_array().unwrap();
    let my = groups.iter().find(|group| group["id"] == "my").unwrap();
    my["stations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|station| station["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn my_add_and_remove_a_curated_station() {
    let app = discovery_app().await;

    let added = app.server.post("/api/my").json(&json!({"station": "test"})).await;
    added.assert_status_ok();
    let added_body: serde_json::Value = added.json();
    assert_eq!(my_ids(&added_body), vec!["test"]);
    assert_eq!(added_body["groups"][0]["stations"][0]["in_my"], true);

    // Adding again is a no-op.
    let again: serde_json::Value = app.server.post("/api/my").json(&json!({"station": "test"})).await.json();
    assert_eq!(my_ids(&again), vec!["test"]);

    let removed = app.server.delete("/api/my/test").await;
    removed.assert_status_ok();
    assert!(my_ids(&removed.json::<serde_json::Value>()).is_empty());

    // Removing again is a no-op, not an error.
    app.server.delete("/api/my/test").await.assert_status_ok();
}

#[tokio::test]
async fn my_add_a_search_result_by_id() {
    let app = discovery_app().await;
    app.server.get("/api/search").add_query_param("q", "jazz").await.assert_status_ok();
    let jazz_id = format!("rb-{JAZZ_UUID}");

    let added = app.server.post("/api/my").json(&json!({"station": jazz_id})).await;
    added.assert_status_ok();
    let body: serde_json::Value = added.json();

    assert_eq!(my_ids(&body), vec![jazz_id.clone()]);
    let saved = &body["groups"].as_array().unwrap().iter().find(|group| group["id"] == "my").unwrap()["stations"][0];
    assert_eq!(saved["name"], "Smooth Jazz Radio");
    assert_eq!(saved["in_my"], true);
    assert!(!body.to_string().contains("example.com"));

    let search: serde_json::Value = app.server.get("/api/search").add_query_param("q", "jazz").await.json();
    assert_eq!(search["results"][0]["in_my"], true);
    assert_eq!(search["results"][1]["in_my"], false);
}

#[tokio::test]
async fn my_add_rejects_unknown_and_malformed_ids() {
    let app = discovery_app().await;

    let unknown = app.server.post("/api/my").json(&json!({"station": "nonexistent"})).await;
    unknown.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(unknown.json::<serde_json::Value>()["error"], "unknown_station");

    let malformed = app.server.post("/api/my").json(&json!({"station": "rb-not-a-uuid"})).await;
    malformed.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(malformed.json::<serde_json::Value>()["error"], "bad_station");
}

#[tokio::test]
async fn my_is_capped_at_fifty_stations() {
    let app = discovery_app().await;
    let mut stations = app.state.stations.write().await;
    for number in 0..my_stations::MY_CAP {
        let uuid = format!("00000000-0000-0000-0000-{number:012}");
        let entry = my_stations::MyEntry::Rb(my_stations::StoredRbStation::from_rb(&radiobrowser::RbStation {
            uuid,
            name: format!("Filler {number}"),
            genre: String::new(),
            country: String::new(),
            bitrate: 0,
            url: "http://filler.example.com/stream".to_string(),
        }));
        stations.my_add(entry).await.unwrap();
    }
    drop(stations);

    let response = app.server.post("/api/my").json(&json!({"station": "test"})).await;

    response.assert_status(axum::http::StatusCode::CONFLICT);
    assert_eq!(response.json::<serde_json::Value>()["error"], "my_full");
}

#[tokio::test]
async fn my_survives_an_app_restart() {
    let cache_dir = tempfile::tempdir().unwrap();
    let first = build_app_in(cache_dir.path(), Arc::new(MockYxcClient::new()), fast_timing(), None).await;
    first.server.get("/api/search").add_query_param("q", "jazz").await.assert_status_ok();
    first.server.post("/api/my").json(&json!({"station": format!("rb-{JAZZ_UUID}")})).await.assert_status_ok();
    first.server.post("/api/my").json(&json!({"station": "test"})).await.assert_status_ok();
    drop(first);

    let second = build_app_in(cache_dir.path(), Arc::new(MockYxcClient::new()), fast_timing(), None).await;
    let body: serde_json::Value = second.server.get("/api/stations").await.json();

    assert_eq!(my_ids(&body), vec![format!("rb-{JAZZ_UUID}"), "test".to_string()]);
}

#[tokio::test]
async fn my_changes_are_pushed_as_a_stations_event() {
    let app = discovery_app().await;
    let state = app.state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::create_router(state)).await.unwrap();
    });
    let mut events = reqwest::get(format!("http://{address}/api/events")).await.unwrap();
    read_sse_until(&mut events, "event: state").await;

    reqwest::Client::new()
        .post(format!("http://{address}/api/my"))
        .json(&json!({"station": "test"}))
        .send()
        .await
        .unwrap();

    let received = read_sse_until(&mut events, "event: stations").await;
    assert!(received.contains(r#""id":"my""#), "{received:?}");
    assert!(received.contains(r#""in_my":true"#), "{received:?}");
}

// ---- Station discovery: playing rb- ids ----

#[tokio::test]
async fn play_resolves_an_unsaved_station_with_by_uuid_then_the_cache() {
    let app = discovery_app().await;
    let jazz_id = format!("rb-{JAZZ_UUID}");

    let first = app.server.post("/api/play").json(&json!({"station": jazz_id})).await;
    first.assert_status_ok();
    assert_eq!(app.radio_browser.by_uuid_call_count(), 1);
    let player_state = cliamp::Player::state(app.player.as_ref()).await;
    assert_eq!(player_state.url.as_deref(), Some("http://jazz.example.com/stream"));
    let state: serde_json::Value = first.json();
    assert_eq!(state["player"]["station_name"], "Smooth Jazz Radio");

    // The result is now cached: playing it again does not ask Radio Browser.
    app.server.post("/api/play").json(&json!({"station": jazz_id})).await.assert_status_ok();
    assert_eq!(app.radio_browser.by_uuid_call_count(), 1);
}

#[tokio::test]
async fn play_uses_the_search_cache_when_radio_browser_goes_down() {
    let app = discovery_app().await;
    app.server.get("/api/search").add_query_param("q", "blues").await.assert_status_ok();
    app.radio_browser.set_down(true);

    let response = app.server.post("/api/play").json(&json!({"station": format!("rb-{BLUES_UUID}")})).await;

    response.assert_status_ok();
    assert_eq!(app.radio_browser.by_uuid_call_count(), 0);
    let player_state = cliamp::Player::state(app.player.as_ref()).await;
    assert_eq!(player_state.url.as_deref(), Some("https://blues.example.com/live"));
}

#[tokio::test]
async fn play_uses_my_after_a_restart_without_asking_radio_browser() {
    let cache_dir = tempfile::tempdir().unwrap();
    let first = build_app_in(cache_dir.path(), Arc::new(MockYxcClient::new()), fast_timing(), None).await;
    first.server.get("/api/search").add_query_param("q", "jazz").await.assert_status_ok();
    let jazz_id = format!("rb-{JAZZ_UUID}");
    first.server.post("/api/my").json(&json!({"station": jazz_id})).await.assert_status_ok();
    drop(first);

    let second = build_app_in(cache_dir.path(), Arc::new(MockYxcClient::new()), fast_timing(), None).await;
    second.radio_browser.set_down(true);
    let response = second.server.post("/api/play").json(&json!({"station": jazz_id})).await;

    response.assert_status_ok();
    assert_eq!(second.radio_browser.by_uuid_call_count(), 0);
    let player_state = cliamp::Player::state(second.player.as_ref()).await;
    assert_eq!(player_state.url.as_deref(), Some("http://jazz.example.com/stream"));
}

#[tokio::test]
async fn play_rejects_ids_that_are_not_known_stations() {
    let app = discovery_app().await;

    // A URL in the station field is just an unknown id.
    let url_as_id = app
        .server
        .post("/api/play")
        .json(&json!({"station": "http://evil.example.com/stream"}))
        .await;
    url_as_id.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(url_as_id.json::<serde_json::Value>()["error"], "unknown_station");

    // A URL smuggled in an extra field is ignored: the id still decides.
    let extra_field = app
        .server
        .post("/api/play")
        .json(&json!({"station": "nonexistent", "url": "http://evil.example.com/stream"}))
        .await;
    extra_field.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // A malformed rb- id never reaches Radio Browser.
    let malformed = app.server.post("/api/play").json(&json!({"station": "rb-../../etc"})).await;
    malformed.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(malformed.json::<serde_json::Value>()["error"], "bad_station");

    // A well-formed rb- id that Radio Browser does not know.
    let missing = app
        .server
        .post("/api/play")
        .json(&json!({"station": "rb-99999999-9999-9999-9999-999999999999"}))
        .await;
    missing.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(missing.json::<serde_json::Value>()["error"], "unknown_station");

    assert_eq!(app.radio_browser.by_uuid_call_count(), 1);
    let player_state = cliamp::Player::state(app.player.as_ref()).await;
    assert_eq!(player_state.url, None);
}

#[tokio::test]
async fn a_slow_receiver_poll_does_not_block_station_writers() {
    let mock = MockYxcClient::new();
    let delay_ms = mock.play_info_delay_ms.clone();
    let app = build_app(Arc::new(mock), fast_timing(), None).await;
    delay_ms.store(600, std::sync::atomic::Ordering::SeqCst);

    let state_manager = app.state.state_manager.clone();
    let refresh = tokio::spawn(async move { state_manager.refresh().await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let writer = tokio::time::timeout(std::time::Duration::from_millis(200), app.state.stations.write()).await;
    assert!(writer.is_ok(), "refresh held the stations lock across the receiver calls");
    drop(writer);
    refresh.await.unwrap();
}

#[tokio::test]
async fn a_lookup_that_answers_with_another_station_is_unknown_and_never_cached() {
    let app = discovery_app().await;
    app.radio_browser.set_answer_wrong_station(true);
    let blues_id = format!("rb-{BLUES_UUID}");

    let played = app.server.post("/api/play").json(&json!({"station": blues_id})).await;
    played.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(played.json::<serde_json::Value>()["error"], "unknown_station");

    let kept = app.server.post("/api/my").json(&json!({"station": blues_id})).await;
    kept.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(kept.json::<serde_json::Value>()["error"], "unknown_station");

    let stations = app.state.stations.read().await;
    assert!(stations.cached_search_result(&blues_id).is_none());
    assert!(stations.cached_search_result(&format!("rb-{JAZZ_UUID}")).is_none());
    drop(stations);
    let player_state = cliamp::Player::state(app.player.as_ref()).await;
    assert_eq!(player_state.url, None);
    let body: serde_json::Value = app.server.get("/api/stations").await.json();
    assert_eq!(my_ids(&body), Vec::<String>::new());
}

#[tokio::test]
async fn play_reports_unavailable_when_an_unsaved_station_cannot_be_looked_up() {
    let app = discovery_app().await;
    app.radio_browser.set_down(true);

    let response = app.server.post("/api/play").json(&json!({"station": format!("rb-{JAZZ_UUID}")})).await;

    response.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.json::<serde_json::Value>()["error"], "search_unavailable");
}
