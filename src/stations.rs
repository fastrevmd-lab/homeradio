use crate::my_stations::{MyEntry, MyError, MyStore, StoredRbStation};
use crate::radiobrowser::RbStation;
use crate::search_cache::SearchCache;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Station {
    pub id: String,
    pub name: String,
    pub short: String,
    pub genre: String,
    #[serde(skip_serializing)]
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StationGroup {
    pub id: String,
    pub label: String,
    pub stations: Vec<Station>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StationRegistry {
    pub groups: Vec<StationGroup>,
}

/// A station as `GET /api/stations` shows it: no URL, plus whether it is in MY.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StationView {
    pub id: String,
    pub name: String,
    pub short: String,
    pub genre: String,
    pub in_my: bool,
}

impl StationView {
    fn from_station(station: &Station, in_my: bool) -> Self {
        Self {
            id: station.id.clone(),
            name: station.name.clone(),
            short: station.short.clone(),
            genre: station.genre.clone(),
            in_my,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupView {
    pub id: String,
    pub label: String,
    pub stations: Vec<StationView>,
}

/// The `Stations` API shape: the ROCK and CLIAMP groups plus the `my` group.
#[derive(Debug, Clone, Serialize)]
pub struct RegistryView {
    /// Rises on every MY or CLIAMP change, so a client can discard a snapshot
    /// older than the one it already shows. Starts from the wall clock, so a
    /// restarted server is never behind a page that outlived the old one.
    pub revision: u64,
    pub groups: Vec<GroupView>,
}

/// Where a new manager's revision counter starts: milliseconds since the epoch.
fn initial_revision() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[derive(Debug, Deserialize)]
struct RockStationsFile {
    station: Vec<Station>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RemoteStationsResponse {
    stations: Vec<RemoteStation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RemoteStation {
    id: String,
    name: String,
    genre: String,
    stream: String,
    #[serde(default)]
    description: Option<String>,
}

pub struct StationManager {
    rock_stations: Vec<Station>,
    cliamp_stations: Vec<Station>,
    station_urls: HashMap<String, String>,
    cache_path: PathBuf,
    remote_url: String,
    /// The household's MY list, `<cache_dir>/my-stations.json`.
    my: MyStore,
    /// Recent search results, so they can be played or kept by id.
    search_cache: SearchCache,
    /// The Radio Browser station that is playing now, kept so the LCD name
    /// survives the search cache expiring (or the station leaving MY) mid-listen.
    playing_rb: Option<StoredRbStation>,
    /// Bumped on every change to what `registry_view` shows; see `RegistryView`.
    revision: u64,
}

impl StationManager {
    pub async fn new(
        stations_file: &Path,
        cache_dir: &Path,
        remote_url: String,
    ) -> anyhow::Result<Self> {
        // Load rock stations from TOML
        let rock_stations = Self::load_rock_stations(stations_file)?;

        // Create cache directory if it doesn't exist
        tokio::fs::create_dir_all(cache_dir).await.ok();

        let cache_path = cache_dir.join("cliamp-stations.json");
        let my = MyStore::load(cache_dir.join("my-stations.json"));

        // Load cliamp stations
        let cliamp_stations = Self::fetch_cliamp_stations(&remote_url, &cache_path).await;

        let mut manager = Self {
            rock_stations: rock_stations.clone(),
            cliamp_stations: cliamp_stations.clone(),
            station_urls: HashMap::new(),
            cache_path,
            remote_url,
            my,
            search_cache: SearchCache::new(),
            playing_rb: None,
            revision: initial_revision(),
        };

        manager.rebuild_url_map();

        Ok(manager)
    }

    fn load_rock_stations(path: &Path) -> anyhow::Result<Vec<Station>> {
        let contents = std::fs::read_to_string(path)?;
        let file: RockStationsFile = toml::from_str(&contents)?;
        Ok(file.station)
    }

    async fn fetch_cliamp_stations(remote_url: &str, cache_path: &Path) -> Vec<Station> {
        // Try to fetch from remote
        match Self::fetch_remote(remote_url).await {
            Ok((feed_text, stations)) => {
                // Cache the raw feed (the API-facing `Station` omits URLs when
                // serialized, so it cannot be used as the cache format).
                tokio::fs::write(cache_path, feed_text).await.ok();
                info!("Fetched {} cliamp stations from remote", stations.len());
                stations
            }
            Err(e) => {
                warn!("Failed to fetch remote stations: {}, trying cache", e);
                // Try to load from cache
                if let Ok(cached) = Self::load_from_cache(cache_path).await {
                    info!("Loaded {} cliamp stations from cache", cached.len());
                    cached
                } else {
                    error!("No cached stations available");
                    Vec::new()
                }
            }
        }
    }

    /// Fetch the remote feed; returns the raw feed text (for the disk cache) and
    /// the parsed stations.
    async fn fetch_remote(url: &str) -> anyhow::Result<(String, Vec<Station>)> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let feed_text = client.get(url).send().await?.error_for_status()?.text().await?;
        let stations = Self::stations_from_feed(&feed_text)?;
        Ok((feed_text, stations))
    }

    /// Parse the remote feed JSON into stations, keeping only http(s) streams.
    fn stations_from_feed(feed_text: &str) -> anyhow::Result<Vec<Station>> {
        let response: RemoteStationsResponse = serde_json::from_str(feed_text)?;
        Ok(response
            .stations
            .into_iter()
            .filter(|s| s.stream.starts_with("https://") || s.stream.starts_with("http://"))
            .map(|s| Station {
                id: s.id,
                name: s.name.clone(),
                short: s.name,
                genre: s.genre,
                url: s.stream,
            })
            .collect())
    }

    async fn load_from_cache(path: &Path) -> anyhow::Result<Vec<Station>> {
        let contents = tokio::fs::read_to_string(path).await?;
        Self::stations_from_feed(&contents)
    }

    fn rebuild_url_map(&mut self) {
        self.station_urls.clear();
        for station in &self.rock_stations {
            self.station_urls.insert(station.id.clone(), station.url.clone());
        }
        // Cliamp stations: skip ids that collide with rock
        for station in &self.cliamp_stations {
            if !self.station_urls.contains_key(&station.id) {
                self.station_urls.insert(station.id.clone(), station.url.clone());
            }
        }
    }

    pub fn get_registry(&self) -> StationRegistry {
        // Filter cliamp stations to exclude collisions
        let rock_ids: HashSet<_> = self.rock_stations.iter().map(|s| &s.id).collect();
        let cliamp_filtered: Vec<_> = self
            .cliamp_stations
            .iter()
            .filter(|s| !rock_ids.contains(&s.id))
            .cloned()
            .collect();

        StationRegistry {
            groups: vec![
                StationGroup {
                    id: "rock".to_string(),
                    label: "ROCK".to_string(),
                    stations: self.rock_stations.clone(),
                },
                StationGroup {
                    id: "cliamp".to_string(),
                    label: "CLIAMP".to_string(),
                    stations: cliamp_filtered,
                },
            ],
        }
    }

    /// The stream URL for a station id: ROCK/CLIAMP from the registry, then a
    /// Radio Browser station saved in MY. Unsaved search results are resolved
    /// by the API layer, never here.
    pub fn get_station_url(&self, id: &str) -> Option<&str> {
        if let Some(url) = self.station_urls.get(id) {
            return Some(url.as_str());
        }
        self.my.get_rb(id).map(|station| station.url.as_str())
    }

    /// The display name for a station id, from the registry, MY, the station
    /// playing now, or the search cache.
    pub fn get_station_name(&self, id: &str) -> Option<String> {
        let curated = self
            .rock_stations
            .iter()
            .chain(self.cliamp_stations.iter())
            .find(|s| s.id == id);
        if let Some(station) = curated {
            return Some(station.name.clone());
        }
        if let Some(stored) = self.my.get_rb(id) {
            return Some(stored.name.clone());
        }
        if let Some(playing) = self.playing_rb.as_ref().filter(|rb| rb.id == id) {
            return Some(playing.name.clone());
        }
        self.search_cache.get(id, time::Instant::now()).map(|rb| rb.name)
    }

    /// The id of the station that streams `url`: curated stations first (ROCK
    /// before CLIAMP), then MY, then the Radio Browser station playing now.
    pub fn station_id_for_url(&self, url: &str) -> Option<String> {
        let curated = self
            .rock_stations
            .iter()
            .chain(self.cliamp_stations.iter())
            .find(|s| self.station_urls.get(&s.id).map(String::as_str) == Some(url));
        if let Some(station) = curated {
            return Some(station.id.clone());
        }
        if let Some(stored) = self.my.rb_entries().find(|stored| stored.url == url) {
            return Some(stored.id.clone());
        }
        self.playing_rb.as_ref().filter(|rb| rb.url == url).map(|rb| rb.id.clone())
    }

    /// True for a ROCK or CLIAMP station id.
    pub fn is_curated(&self, id: &str) -> bool {
        self.station_urls.contains_key(id)
    }

    /// True when `id` is in MY.
    pub fn is_in_my(&self, id: &str) -> bool {
        self.my.contains(id)
    }

    /// Adds an entry to MY; `Ok(false)` when it was already there.
    pub async fn my_add(&mut self, entry: MyEntry) -> Result<bool, MyError> {
        let changed = self.my.add(entry).await?;
        if changed {
            self.revision += 1;
        }
        Ok(changed)
    }

    /// Removes an entry from MY; `Ok(false)` when it was not there.
    pub async fn my_remove(&mut self, id: &str) -> Result<bool, MyError> {
        let changed = self.my.remove(id).await?;
        if changed {
            self.revision += 1;
        }
        Ok(changed)
    }

    /// Remembers search results so they can be played or kept by id.
    pub fn remember_search_results(&mut self, stations: &[RbStation]) {
        self.search_cache.insert_all(stations, time::Instant::now());
    }

    /// A recent search result by id, if it has not expired.
    pub fn cached_search_result(&self, id: &str) -> Option<RbStation> {
        self.search_cache.get(id, time::Instant::now())
    }

    /// Records what is playing now: the freshly discovered Radio Browser
    /// station if there is one, else the MY entry with this id (if any), so
    /// a ROCK/CLIAMP station clears the previous Radio Browser one.
    pub fn note_playing(&mut self, id: &str, discovered: Option<&RbStation>) {
        self.playing_rb = match discovered {
            Some(station) => Some(StoredRbStation::from_rb(station)),
            None => self.my.get_rb(id).cloned(),
        };
    }

    /// The registry as the API shows it: ROCK and CLIAMP flagged with `in_my`,
    /// plus the `my` group in insertion order. A reference whose station is
    /// gone is left out (but stays in the file).
    pub fn registry_view(&self) -> RegistryView {
        let registry = self.get_registry();

        let mut my_stations: Vec<StationView> = Vec::new();
        for entry in self.my.entries() {
            match entry {
                MyEntry::Ref { station_id } => {
                    let found = registry
                        .groups
                        .iter()
                        .flat_map(|group| &group.stations)
                        .find(|station| &station.id == station_id);
                    if let Some(station) = found {
                        my_stations.push(StationView::from_station(station, true));
                    }
                }
                MyEntry::Rb(stored) => my_stations.push(StationView {
                    id: stored.id.clone(),
                    name: stored.name.clone(),
                    short: stored.short.clone(),
                    genre: stored.genre.clone(),
                    in_my: true,
                }),
            }
        }

        let mut groups: Vec<GroupView> = registry
            .groups
            .iter()
            .map(|group| GroupView {
                id: group.id.clone(),
                label: group.label.clone(),
                stations: group
                    .stations
                    .iter()
                    .map(|station| StationView::from_station(station, self.my.contains(&station.id)))
                    .collect(),
            })
            .collect();
        groups.push(GroupView {
            id: "my".to_string(),
            label: "MY".to_string(),
            stations: my_stations,
        });
        RegistryView { revision: self.revision, groups }
    }

    /// Start background refresh task
    pub fn start_refresh_task(manager: std::sync::Arc<tokio::sync::RwLock<Self>>) {
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(6 * 3600)); // 6 hours
            interval.tick().await; // Skip first tick

            loop {
                interval.tick().await;
                let mgr = manager.read().await;
                let remote_url = mgr.remote_url.clone();
                let cache_path = mgr.cache_path.clone();
                drop(mgr);

                info!("Refreshing cliamp stations");
                let stations = Self::fetch_cliamp_stations(&remote_url, &cache_path).await;

                let mut mgr = manager.write().await;
                mgr.cliamp_stations = stations;
                mgr.rebuild_url_map();
                mgr.revision += 1;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_load_rock_stations() {
        let stations = StationManager::load_rock_stations(Path::new("config/stations.toml"))
            .expect("Failed to load stations");
        assert!(!stations.is_empty());
        assert_eq!(stations[0].id, "big100");
    }

    #[tokio::test]
    async fn test_cache_round_trip_keeps_urls() {
        let feed = r#"{"stations":[
            {"id":"a","name":"Alpha","genre":"Rock","stream":"https://a.example/stream","description":"x"},
            {"id":"b","name":"Beta","genre":"Pop","stream":"rtsp://b.example/stream"}
        ]}"#;
        let from_feed = StationManager::stations_from_feed(feed).unwrap();
        assert_eq!(from_feed.len(), 1, "non-http stream filtered out");

        let dir = std::env::temp_dir().join(format!("radio_cache_rt_{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let cache_path = dir.join("cliamp-stations.json");
        tokio::fs::write(&cache_path, feed).await.unwrap();

        let cached = StationManager::load_from_cache(&cache_path).await.unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].url, "https://a.example/stream");

        // The API-facing serialization still omits URLs.
        let json = serde_json::to_value(&cached[0]).unwrap();
        assert!(json.get("url").is_none());
    }

    #[test]
    fn test_station_url_deduplication() {
        let rock = vec![Station {
            id: "test".to_string(),
            name: "Test".to_string(),
            short: "Test".to_string(),
            genre: "Test".to_string(),
            url: "http://rock".to_string(),
        }];

        let cliamp = vec![Station {
            id: "test".to_string(),
            name: "Test Cliamp".to_string(),
            short: "Test".to_string(),
            genre: "Test".to_string(),
            url: "http://cliamp".to_string(),
        }];

        let mut manager = StationManager {
            rock_stations: rock,
            cliamp_stations: cliamp,
            station_urls: HashMap::new(),
            cache_path: PathBuf::new(),
            remote_url: String::new(),
            my: MyStore::load(PathBuf::from("/nonexistent/my-stations.json")),
            search_cache: SearchCache::new(),
            playing_rb: None,
            revision: 0,
        };

        manager.rebuild_url_map();

        // Rock takes precedence
        assert_eq!(manager.get_station_url("test"), Some("http://rock"));
    }

    const UUID_A: &str = "11111111-1111-1111-1111-111111111111";

    fn curated(id: &str, url: &str) -> Station {
        Station {
            id: id.to_string(),
            name: format!("{id} name"),
            short: id.to_string(),
            genre: "Rock".to_string(),
            url: url.to_string(),
        }
    }

    fn rb_station(uuid: &str, url: &str) -> RbStation {
        RbStation {
            uuid: uuid.to_string(),
            name: "Jazz FM".to_string(),
            genre: "jazz".to_string(),
            country: "France".to_string(),
            bitrate: 128,
            url: url.to_string(),
        }
    }

    fn stored(station: &RbStation) -> MyEntry {
        MyEntry::Rb(crate::my_stations::StoredRbStation::from_rb(station))
    }

    /// A manager with one ROCK (`big100`) and one CLIAMP (`lofi`) station and
    /// an empty MY list stored in `dir`.
    fn manager_in(dir: &Path) -> StationManager {
        let mut manager = StationManager {
            rock_stations: vec![curated("big100", "http://rock/big100")],
            cliamp_stations: vec![curated("lofi", "http://cliamp/lofi")],
            station_urls: HashMap::new(),
            cache_path: PathBuf::new(),
            remote_url: String::new(),
            my: MyStore::load(dir.join("my-stations.json")),
            search_cache: SearchCache::new(),
            playing_rb: None,
            revision: initial_revision(),
        };
        manager.rebuild_url_map();
        manager
    }

    fn group<'a>(view: &'a RegistryView, id: &str) -> &'a GroupView {
        view.groups.iter().find(|group| group.id == id).unwrap()
    }

    #[tokio::test]
    async fn registry_view_adds_an_empty_my_group_and_in_my_flags() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager_in(dir.path());
        let view = manager.registry_view();
        let ids: Vec<&str> = view.groups.iter().map(|group| group.id.as_str()).collect();
        assert_eq!(ids, vec!["rock", "cliamp", "my"]);
        assert_eq!(group(&view, "my").label, "MY");
        assert!(group(&view, "my").stations.is_empty());
        assert!(!group(&view, "rock").stations[0].in_my);
    }

    #[tokio::test]
    async fn the_revision_rises_on_every_real_my_change_and_only_then() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_in(dir.path());
        let initial = manager.registry_view().revision;

        manager.my_add(MyEntry::Ref { station_id: "lofi".to_string() }).await.unwrap();
        let after_add = manager.registry_view().revision;
        assert!(after_add > initial);

        manager.my_add(MyEntry::Ref { station_id: "lofi".to_string() }).await.unwrap();
        assert_eq!(manager.registry_view().revision, after_add, "a no-op add changes nothing");

        manager.my_remove("lofi").await.unwrap();
        assert!(manager.registry_view().revision > after_add);

        let json = serde_json::to_value(manager.registry_view()).unwrap();
        assert!(json["revision"].is_u64());
    }

    #[tokio::test]
    async fn a_new_manager_starts_above_the_revision_of_an_earlier_run() {
        let dir = tempfile::tempdir().unwrap();
        let earlier = manager_in(dir.path()).registry_view().revision;
        std::thread::sleep(Duration::from_millis(5));
        let later = manager_in(dir.path()).registry_view().revision;
        assert!(later > earlier, "a restarted server must not look older than the page's last snapshot");
    }

    #[tokio::test]
    async fn my_group_lists_refs_and_rb_stations_in_insertion_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_in(dir.path());
        let rb = rb_station(UUID_A, "https://jazz.example/stream");
        manager.my_add(MyEntry::Ref { station_id: "lofi".to_string() }).await.unwrap();
        manager.my_add(stored(&rb)).await.unwrap();
        manager.my_add(MyEntry::Ref { station_id: "big100".to_string() }).await.unwrap();

        let view = manager.registry_view();
        let my_ids: Vec<&str> = group(&view, "my").stations.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(my_ids, vec!["lofi", &format!("rb-{UUID_A}")[..], "big100"]);
        assert!(group(&view, "my").stations.iter().all(|s| s.in_my));
        assert!(group(&view, "rock").stations[0].in_my, "big100 is flagged in its own band");
        assert!(group(&view, "cliamp").stations[0].in_my);

        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("jazz.example"), "stream URLs never reach the API: {json}");
    }

    #[tokio::test]
    async fn a_dangling_reference_is_hidden_and_reappears_with_its_station() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_in(dir.path());
        manager.my_add(MyEntry::Ref { station_id: "gone".to_string() }).await.unwrap();
        assert!(group(&manager.registry_view(), "my").stations.is_empty());

        manager.cliamp_stations.push(curated("gone", "http://cliamp/gone"));
        manager.rebuild_url_map();
        let view = manager.registry_view();
        assert_eq!(group(&view, "my").stations[0].id, "gone");
    }

    #[tokio::test]
    async fn urls_resolve_for_curated_and_saved_stations_but_not_for_search_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_in(dir.path());
        let saved = rb_station(UUID_A, "https://jazz.example/stream");
        let other = rb_station("22222222-2222-2222-2222-222222222222", "https://other.example/stream");
        manager.my_add(stored(&saved)).await.unwrap();
        manager.remember_search_results(std::slice::from_ref(&other));

        assert_eq!(manager.get_station_url("big100"), Some("http://rock/big100"));
        assert_eq!(manager.get_station_url(&saved.id()), Some("https://jazz.example/stream"));
        assert_eq!(manager.get_station_url(&other.id()), None);
        assert_eq!(manager.cached_search_result(&other.id()), Some(other));
    }

    #[tokio::test]
    async fn names_come_from_the_registry_my_the_playing_station_or_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_in(dir.path());
        let saved = rb_station(UUID_A, "https://jazz.example/stream");
        let playing = rb_station("22222222-2222-2222-2222-222222222222", "https://p.example/s");
        let cached = rb_station("33333333-3333-3333-3333-333333333333", "https://c.example/s");
        manager.my_add(stored(&saved)).await.unwrap();
        manager.note_playing(&playing.id(), Some(&playing));
        manager.remember_search_results(std::slice::from_ref(&cached));

        assert_eq!(manager.get_station_name("big100").as_deref(), Some("big100 name"));
        assert_eq!(manager.get_station_name(&saved.id()).as_deref(), Some("Jazz FM"));
        assert_eq!(manager.get_station_name(&playing.id()).as_deref(), Some("Jazz FM"));
        assert_eq!(manager.get_station_name(&cached.id()).as_deref(), Some("Jazz FM"));
        assert_eq!(manager.get_station_name("rb-44444444-4444-4444-4444-444444444444"), None);

        manager.note_playing("big100", None);
        assert_eq!(manager.get_station_name(&playing.id()), None);

        manager.note_playing(&saved.id(), None);
        manager.my_remove(&saved.id()).await.unwrap();
        assert_eq!(
            manager.get_station_name(&saved.id()).as_deref(),
            Some("Jazz FM"),
            "a station removed from MY keeps its name while it plays"
        );
    }

    #[tokio::test]
    async fn a_playing_url_maps_back_to_its_station_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = manager_in(dir.path());
        let saved = rb_station(UUID_A, "https://jazz.example/stream");
        let playing = rb_station("22222222-2222-2222-2222-222222222222", "https://p.example/s");
        manager.my_add(stored(&saved)).await.unwrap();
        manager.note_playing(&playing.id(), Some(&playing));

        assert_eq!(manager.station_id_for_url("http://cliamp/lofi").as_deref(), Some("lofi"));
        assert_eq!(manager.station_id_for_url("http://rock/big100").as_deref(), Some("big100"));
        assert_eq!(manager.station_id_for_url("https://jazz.example/stream"), Some(saved.id()));
        assert_eq!(manager.station_id_for_url("https://p.example/s"), Some(playing.id()));
        assert_eq!(manager.station_id_for_url("https://nowhere.example/"), None);
    }

    #[tokio::test]
    async fn curated_ids_are_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager_in(dir.path());
        assert!(manager.is_curated("big100"));
        assert!(manager.is_curated("lofi"));
        assert!(!manager.is_curated(&format!("rb-{UUID_A}")));
    }
}
