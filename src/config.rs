use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Base URL of the receiver's Extended Control API, e.g. `http://192.168.1.50`.
    /// Required: there is no sensible default for a LAN address.
    pub receiver_url: String,
    #[serde(default = "default_cliamp_bin")]
    pub cliamp_bin: String,
    #[serde(default = "default_stations_file")]
    pub stations_file: PathBuf,
    #[serde(default = "default_cache_dir")]
    pub cache_dir: PathBuf,
    #[serde(default = "default_remote_stations_url")]
    pub remote_stations_url: String,
    /// systemd user unit that owns the AirPlay (RAOP) sink.
    #[serde(default = "default_raop_unit")]
    pub raop_unit: String,
    #[serde(default = "default_zones")]
    pub zones: HashMap<String, ZoneConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneConfig {
    pub cap_db: f64,
    pub start_db: f64,
    pub label: String,
}

fn default_listen() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_cliamp_bin() -> String {
    "/usr/local/bin/cliamp".to_string()
}

fn default_stations_file() -> PathBuf {
    PathBuf::from("/etc/home-radio/stations.toml")
}

fn default_cache_dir() -> PathBuf {
    PathBuf::from("/var/lib/home-radio")
}

fn default_raop_unit() -> String {
    "raop-sink.service".to_string()
}

fn default_remote_stations_url() -> String {
    "https://radio.cliamp.stream/stations".to_string()
}

/// The built-in zones, used when a config omits the `[zones.*]` tables.
pub fn default_zones() -> HashMap<String, ZoneConfig> {
    let mut zones = HashMap::new();
    zones.insert(
        "main".to_string(),
        ZoneConfig {
            cap_db: -15.0,
            start_db: -35.0,
            label: "Media Room".to_string(),
        },
    );
    zones.insert(
        "zone2".to_string(),
        ZoneConfig {
            cap_db: 0.0,
            start_db: -5.0,
            label: "Upstairs".to_string(),
        },
    );
    zones
}

impl Config {
    /// A configuration with the given receiver URL and every other field at its
    /// built-in default (what a minimal `receiver_url = "..."` config file gives).
    pub fn with_receiver_url(receiver_url: impl Into<String>) -> Self {
        Self {
            listen: default_listen(),
            receiver_url: receiver_url.into(),
            cliamp_bin: default_cliamp_bin(),
            stations_file: default_stations_file(),
            cache_dir: default_cache_dir(),
            remote_stations_url: default_remote_stations_url(),
            raop_unit: default_raop_unit(),
            zones: default_zones(),
        }
    }

    /// Load configuration from a TOML file. `receiver_url` is required; a file
    /// without it fails here with a message naming the file.
    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)
            .map_err(|err| anyhow::anyhow!("invalid config {}: {}", path.display(), err))?;
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration: both zones must exist, and each must satisfy
    /// `-80.5 <= start_db <= cap_db <= 0.0` (the receiver's volume floor and a
    /// hard ceiling of 0 dB).
    pub fn validate(&self) -> anyhow::Result<()> {
        for required in ["main", "zone2"] {
            if !self.zones.contains_key(required) {
                anyhow::bail!("Zone '{}' is missing from the configuration", required);
            }
        }
        for (zone_name, zone) in &self.zones {
            // Written as a negated chain so NaN fails every comparison.
            let in_range = -80.5 <= zone.start_db && zone.start_db <= zone.cap_db && zone.cap_db <= 0.0;
            if !in_range {
                anyhow::bail!(
                    "Zone '{}' must satisfy -80.5 <= start_db <= cap_db <= 0.0 (start_db = {}, cap_db = {}) - refusing to start for safety",
                    zone_name,
                    zone.start_db,
                    zone.cap_db
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECEIVER: &str = "http://192.0.2.10";

    /// Parse `toml_text` with a `receiver_url` line prepended (it is required).
    fn parse(toml_text: &str) -> anyhow::Result<Config> {
        parse_raw(&format!("receiver_url = \"{RECEIVER}\"\n{toml_text}"))
    }

    fn parse_raw(toml_text: &str) -> anyhow::Result<Config> {
        let config: Config = toml::from_str(toml_text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn defaults_are_valid() {
        Config::with_receiver_url(RECEIVER).validate().unwrap();
    }

    #[test]
    fn receiver_url_is_required() {
        let err = parse_raw("listen = \"127.0.0.1:9\"").unwrap_err().to_string();
        assert!(err.contains("receiver_url"), "unexpected error: {err}");
    }

    #[test]
    fn from_file_names_the_file_when_receiver_url_is_missing() {
        let path = std::env::temp_dir().join(format!("home_radio_cfg_{}.toml", std::process::id()));
        std::fs::write(&path, "listen = \"127.0.0.1:9\"\n").unwrap();
        let err = Config::from_file(&path).unwrap_err().to_string();
        std::fs::remove_file(&path).ok();
        assert!(err.contains("receiver_url") && err.contains("home_radio_cfg_"), "unexpected error: {err}");
    }

    #[test]
    fn raop_unit_defaults_and_overrides() {
        assert_eq!(Config::with_receiver_url(RECEIVER).raop_unit, "raop-sink.service");
        let config = parse("raop_unit = \"custom.service\"").unwrap();
        assert_eq!(config.raop_unit, "custom.service");
        assert_eq!(parse("listen = \"127.0.0.1:9\"").unwrap().raop_unit, "raop-sink.service");
    }

    #[test]
    fn omitted_zone_tables_get_builtin_zones() {
        let config = parse("listen = \"127.0.0.1:9\"").unwrap();
        assert_eq!(config.zones["main"].cap_db, -15.0);
        assert_eq!(config.zones["zone2"].start_db, -5.0);
    }

    #[test]
    fn rejects_cap_above_zero() {
        let text = "[zones.main]\ncap_db = 1.0\nstart_db = -35.0\nlabel = \"M\"\n[zones.zone2]\ncap_db = 0.0\nstart_db = -5.0\nlabel = \"U\"\n";
        assert!(parse(text).is_err());
    }

    #[test]
    fn rejects_start_above_cap_and_below_floor() {
        let above = "[zones.main]\ncap_db = -15.0\nstart_db = -10.0\nlabel = \"M\"\n[zones.zone2]\ncap_db = 0.0\nstart_db = -5.0\nlabel = \"U\"\n";
        assert!(parse(above).is_err());
        let below = "[zones.main]\ncap_db = -15.0\nstart_db = -90.0\nlabel = \"M\"\n[zones.zone2]\ncap_db = 0.0\nstart_db = -5.0\nlabel = \"U\"\n";
        assert!(parse(below).is_err());
    }

    #[test]
    fn rejects_missing_required_zone() {
        let text = "[zones.main]\ncap_db = -15.0\nstart_db = -35.0\nlabel = \"M\"\n";
        assert!(parse(text).is_err());
    }
}
