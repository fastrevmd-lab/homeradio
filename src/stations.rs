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

        // Load cliamp stations
        let cliamp_stations = Self::fetch_cliamp_stations(&remote_url, &cache_path).await;

        let mut manager = Self {
            rock_stations: rock_stations.clone(),
            cliamp_stations: cliamp_stations.clone(),
            station_urls: HashMap::new(),
            cache_path,
            remote_url,
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

    pub fn get_station_url(&self, id: &str) -> Option<&str> {
        self.station_urls.get(id).map(|s| s.as_str())
    }

    pub fn get_station_name(&self, id: &str) -> Option<String> {
        self.rock_stations
            .iter()
            .chain(self.cliamp_stations.iter())
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
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
        };

        manager.rebuild_url_map();

        // Rock takes precedence
        assert_eq!(manager.get_station_url("test"), Some("http://rock"));
    }
}
