//! Radio Browser (https://www.radio-browser.info) client: station search and
//! lookup by uuid. Everything it returns is untrusted text and is sanitised
//! here, so the rest of the app can treat an `RbStation` as clean.

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;

/// Results returned to the caller after filtering.
pub const SEARCH_LIMIT: usize = 30;
const NAME_MAX_CHARS: usize = 80;
const TAG_MAX_CHARS: usize = 40;
const COUNTRY_MAX_CHARS: usize = 40;
const SHORT_NAME_MAX_CHARS: usize = 12;

/// The genre chips of the search drawer, in display order.
pub const GENRE_CHIPS: [&str; 11] = [
    "Rock",
    "Classic Rock",
    "Alt",
    "Jazz",
    "Blues",
    "Country",
    "Oldies",
    "Classical",
    "Lofi",
    "News",
    "Talk",
];

/// Maps a genre chip label to its Radio Browser tag: the lowercase label,
/// except `Alt`, which is `alternative`. `None` when the label is not a chip.
pub fn genre_tag(label: &str) -> Option<String> {
    let chip = GENRE_CHIPS.iter().find(|chip| chip.eq_ignore_ascii_case(label))?;
    if *chip == "Alt" {
        return Some("alternative".to_string());
    }
    Some(chip.to_lowercase())
}

/// A station from Radio Browser, already filtered and sanitised.
#[derive(Debug, Clone, PartialEq)]
pub struct RbStation {
    pub uuid: String,
    pub name: String,
    /// First tag, or empty.
    pub genre: String,
    pub country: String,
    /// kbit/s, 0 when unknown.
    pub bitrate: u32,
    /// `url_resolved`: an `http(s)` MP3/AAC stream.
    pub url: String,
}

impl RbStation {
    /// The id the API uses for this station: `rb-<uuid>`.
    pub fn id(&self) -> String {
        format!("rb-{}", self.uuid)
    }

    /// A label short enough for a preset key.
    pub fn short_name(&self) -> String {
        short_name(&self.name)
    }
}

/// `name` cut to 12 characters, ending in an ellipsis when it was longer.
pub fn short_name(name: &str) -> String {
    if name.chars().count() <= SHORT_NAME_MAX_CHARS {
        return name.to_string();
    }
    let head: String = name.chars().take(SHORT_NAME_MAX_CHARS - 1).collect();
    format!("{}\u{2026}", head.trim_end())
}

/// Why a Radio Browser call failed. Callers turn every variant into the same
/// "search unavailable" answer.
#[derive(Debug, thiserror::Error)]
pub enum RbError {
    #[error("radio browser unavailable: {0}")]
    Unavailable(String),
}

/// The Radio Browser operations the app needs, behind a trait so tests can
/// substitute a fake.
#[async_trait]
pub trait RadioBrowser: Send + Sync {
    /// Search by name and/or tag; at most `SEARCH_LIMIT` filtered results.
    async fn search(
        &self,
        name: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Vec<RbStation>, RbError>;

    /// One station by uuid, `None` when unknown or filtered out.
    async fn by_uuid(&self, uuid: &str) -> Result<Option<RbStation>, RbError>;
}

/// True for a lowercase, hyphenated UUID (`8-4-4-4-12` hex digits).
pub fn is_valid_uuid(candidate: &str) -> bool {
    if candidate.len() != 36 {
        return false;
    }
    candidate.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
    })
}

/// The uuid inside a well-formed `rb-<uuid>` id.
pub fn rb_uuid(id: &str) -> Option<&str> {
    let uuid = id.strip_prefix("rb-")?;
    is_valid_uuid(uuid).then_some(uuid)
}

/// Strips control characters, trims, and caps the length in characters.
fn clean_text(raw: &str, max_chars: usize) -> String {
    let stripped: String = raw.chars().filter(|c| !c.is_control()).collect();
    let capped: String = stripped.trim().chars().take(max_chars).collect();
    capped.trim_end().to_string()
}

/// One entry of a Radio Browser response, as far as we use it. Every field is
/// optional because the directory is community-maintained.
#[derive(Debug, Deserialize)]
pub struct RawStation {
    stationuuid: Option<String>,
    name: Option<String>,
    url_resolved: Option<String>,
    tags: Option<String>,
    country: Option<String>,
    codec: Option<String>,
    bitrate: Option<u32>,
    hls: Option<u8>,
    lastcheckok: Option<u8>,
}

/// Decodes a response body entry by entry, so one malformed entry does not
/// discard the rest.
pub fn parse_raw_stations(entries: Vec<serde_json::Value>) -> Vec<RawStation> {
    entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect()
}

/// A playable stream: working, not HLS, MP3 or AAC/AAC+, plain http(s) URL.
fn is_playable(raw: &RawStation, url: &str) -> bool {
    if raw.lastcheckok != Some(1) || raw.hls.unwrap_or(0) != 0 {
        return false;
    }
    let codec = raw.codec.as_deref().unwrap_or("").trim().to_ascii_lowercase();
    if !matches!(codec.as_str(), "mp3" | "aac" | "aac+") {
        return false;
    }
    let lowered = url.to_ascii_lowercase();
    let has_scheme = lowered.starts_with("http://") || lowered.starts_with("https://");
    has_scheme && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Applies the app's playability and sanitising rules, de-duplicates by
/// name + URL (and by uuid) and keeps the first `limit` stations in order.
pub fn filter_stations(raw: Vec<RawStation>, limit: usize) -> Vec<RbStation> {
    let mut seen_streams: HashSet<(String, String)> = HashSet::new();
    let mut seen_uuids: HashSet<String> = HashSet::new();
    let mut stations = Vec::new();

    for entry in raw {
        if stations.len() >= limit {
            break;
        }
        let uuid = entry.stationuuid.as_deref().unwrap_or("").trim().to_string();
        let url = entry.url_resolved.as_deref().unwrap_or("").trim().to_string();
        let name = clean_text(entry.name.as_deref().unwrap_or(""), NAME_MAX_CHARS);
        if !is_valid_uuid(&uuid) || name.is_empty() || !is_playable(&entry, &url) {
            continue;
        }
        if !seen_uuids.insert(uuid.clone()) || !seen_streams.insert((name.clone(), url.clone())) {
            continue;
        }
        let first_tag = entry.tags.as_deref().unwrap_or("").split(',').next().unwrap_or("");
        stations.push(RbStation {
            uuid,
            name,
            genre: clean_text(first_tag, TAG_MAX_CHARS),
            country: clean_text(entry.country.as_deref().unwrap_or(""), COUNTRY_MAX_CHARS),
            bitrate: entry.bitrate.unwrap_or(0),
            url,
        });
    }
    stations
}

const ALL_SERVERS_BASE: &str = "https://all.api.radio-browser.info";
const FALLBACK_BASE: &str = "https://de1.api.radio-browser.info";
/// Results fetched before filtering; `SEARCH_LIMIT` survive.
const FETCH_LIMIT: &str = "60";
/// Per-request timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

/// The `GET /json/stations/search` URL for a name and/or tag search.
pub(crate) fn search_url(
    base: &str,
    name: Option<&str>,
    tag: Option<&str>,
) -> Result<reqwest::Url, RbError> {
    let mut params: Vec<(&str, &str)> = Vec::new();
    if let Some(name) = name {
        params.push(("name", name));
    }
    if let Some(tag) = tag {
        params.push(("tag", tag));
    }
    params.extend([
        ("limit", FETCH_LIMIT),
        ("hidebroken", "true"),
        ("order", "clickcount"),
        ("reverse", "true"),
    ]);
    reqwest::Url::parse_with_params(&format!("{base}/json/stations/search"), &params)
        .map_err(|error| RbError::Unavailable(error.to_string()))
}

/// The `GET /json/stations/byuuid/<uuid>` URL. The caller has validated `uuid`.
pub(crate) fn by_uuid_url(base: &str, uuid: &str) -> String {
    format!("{base}/json/stations/byuuid/{uuid}")
}

/// Host names from a `/json/servers` response, keeping only Radio Browser hosts.
pub(crate) fn parse_server_names(entries: &[serde_json::Value]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| entry.get("name")?.as_str())
        .filter(|name| name.ends_with(".radio-browser.info"))
        .filter(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .map(str::to_string)
        .collect()
}

/// Radio Browser over HTTPS, pinned to one mirror chosen at startup.
pub struct HttpRadioBrowser {
    client: reqwest::Client,
    base: String,
}

impl HttpRadioBrowser {
    /// Resolves `all.api.radio-browser.info` to one mirror (falling back to
    /// `de1`) and builds the client. Never fails: if Radio Browser is down,
    /// searches fail later and the rest of the app is unaffected.
    pub async fn connect() -> Self {
        let client = Self::build_client();
        let base = match Self::pick_mirror(&client).await {
            Some(host) => format!("https://{host}"),
            None => FALLBACK_BASE.to_string(),
        };
        tracing::info!("Radio Browser mirror: {}", base);
        Self { client, base }
    }

    /// Builds a client for a known base URL (used by tests and tools).
    pub fn with_base(base: String) -> Self {
        Self { client: Self::build_client(), base }
    }

    /// The shared client: 4 s timeout and the `User-Agent` Radio Browser asks for.
    fn build_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(format!("homeradio/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("static client configuration is valid")
    }

    async fn pick_mirror(client: &reqwest::Client) -> Option<String> {
        let url = format!("{ALL_SERVERS_BASE}/json/servers");
        let response = client.get(url).send().await.ok()?.error_for_status().ok()?;
        let entries: Vec<serde_json::Value> = response.json().await.ok()?;
        let names = parse_server_names(&entries);
        if names.is_empty() {
            return None;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos() as usize)
            .unwrap_or(0);
        names.get(nanos % names.len()).cloned()
    }

    async fn get_entries(&self, url: reqwest::Url) -> Result<Vec<serde_json::Value>, RbError> {
        let unavailable = |error: reqwest::Error| RbError::Unavailable(error.to_string());
        let response = self.client.get(url).send().await.map_err(unavailable)?;
        let response = response.error_for_status().map_err(unavailable)?;
        response.json().await.map_err(unavailable)
    }
}

#[async_trait]
impl RadioBrowser for HttpRadioBrowser {
    async fn search(
        &self,
        name: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Vec<RbStation>, RbError> {
        let url = search_url(&self.base, name, tag)?;
        let entries = self.get_entries(url).await?;
        Ok(filter_stations(parse_raw_stations(entries), SEARCH_LIMIT))
    }

    async fn by_uuid(&self, uuid: &str) -> Result<Option<RbStation>, RbError> {
        if !is_valid_uuid(uuid) {
            return Ok(None);
        }
        let url = reqwest::Url::parse(&by_uuid_url(&self.base, uuid))
            .map_err(|error| RbError::Unavailable(error.to_string()))?;
        let entries = self.get_entries(url).await?;
        Ok(filter_stations(parse_raw_stations(entries), 1).into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const UUID_A: &str = "11111111-1111-1111-1111-111111111111";
    const UUID_B: &str = "22222222-2222-2222-2222-222222222222";

    /// A raw entry that passes every filter; tests override one field.
    fn good_entry(uuid: &str, name: &str, url: &str) -> serde_json::Value {
        json!({
            "stationuuid": uuid,
            "name": name,
            "url_resolved": url,
            "tags": "classic rock,80s",
            "country": "United States",
            "codec": "MP3",
            "bitrate": 128,
            "hls": 0,
            "lastcheckok": 1
        })
    }

    fn filtered(entries: Vec<serde_json::Value>, limit: usize) -> Vec<RbStation> {
        filter_stations(parse_raw_stations(entries), limit)
    }

    #[test]
    fn keeps_a_good_entry_and_maps_its_fields() {
        let stations = filtered(vec![good_entry(UUID_A, "Rock FM", "https://a.example/stream")], 30);
        assert_eq!(stations.len(), 1);
        let station = &stations[0];
        assert_eq!(station.id(), format!("rb-{UUID_A}"));
        assert_eq!(station.name, "Rock FM");
        assert_eq!(station.genre, "classic rock");
        assert_eq!(station.country, "United States");
        assert_eq!(station.bitrate, 128);
        assert_eq!(station.url, "https://a.example/stream");
    }

    #[test]
    fn rejects_stations_that_are_not_playable() {
        let mut broken = good_entry(UUID_A, "Broken", "https://a.example/s");
        broken["lastcheckok"] = json!(0);
        let mut hls = good_entry(UUID_A, "Hls", "https://a.example/s");
        hls["hls"] = json!(1);
        let mut ogg = good_entry(UUID_A, "Ogg", "https://a.example/s");
        ogg["codec"] = json!("OGG");
        let rtsp = good_entry(UUID_A, "Rtsp", "rtsp://a.example/s");
        let blank_url = good_entry(UUID_A, "Blank", "");
        let spaced_url = good_entry(UUID_A, "Spaced", "https://a.example/a b");
        let bad_uuid = good_entry("not-a-uuid", "Bad uuid", "https://a.example/s");
        let nameless = good_entry(UUID_A, "  \u{7}  ", "https://a.example/s");
        let all = vec![broken, hls, ogg, rtsp, blank_url, spaced_url, bad_uuid, nameless];
        assert!(filtered(all, 30).is_empty());
    }

    #[test]
    fn codec_match_is_case_insensitive_and_accepts_aac_plus() {
        let mut lower = good_entry(UUID_A, "Lower", "https://a.example/1");
        lower["codec"] = json!("mp3");
        let mut aac = good_entry(UUID_B, "Aac", "https://a.example/2");
        aac["codec"] = json!("AAC");
        let mut aac_plus = good_entry("33333333-3333-3333-3333-333333333333", "AacPlus", "http://a.example/3");
        aac_plus["codec"] = json!("AAC+");
        assert_eq!(filtered(vec![lower, aac, aac_plus], 30).len(), 3);
    }

    #[test]
    fn tolerates_missing_fields_and_malformed_entries() {
        let entries = vec![
            json!({"stationuuid": UUID_A}),
            json!("not an object"),
            json!({"stationuuid": UUID_B, "bitrate": "fast"}),
            good_entry("33333333-3333-3333-3333-333333333333", "Fine", "http://a.example/s"),
        ];
        let stations = filtered(entries, 30);
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].name, "Fine");
    }

    #[test]
    fn de_duplicates_by_name_and_url_and_by_uuid() {
        let entries = vec![
            good_entry(UUID_A, "Same", "https://a.example/s"),
            good_entry(UUID_B, "Same", "https://a.example/s"),
            good_entry(UUID_A, "Other name", "https://a.example/other"),
            good_entry("33333333-3333-3333-3333-333333333333", "Same", "https://b.example/s"),
        ];
        let stations = filtered(entries, 30);
        let uuids: Vec<&str> = stations.iter().map(|s| s.uuid.as_str()).collect();
        assert_eq!(uuids, vec![UUID_A, "33333333-3333-3333-3333-333333333333"]);
    }

    #[test]
    fn truncates_to_the_limit_keeping_order() {
        let entries: Vec<_> = (0..40)
            .map(|index| {
                good_entry(
                    &format!("{index:08}-0000-0000-0000-000000000000"),
                    &format!("Station {index}"),
                    &format!("https://a.example/{index}"),
                )
            })
            .collect();
        let stations = filtered(entries, SEARCH_LIMIT);
        assert_eq!(stations.len(), 30);
        assert_eq!(stations[0].name, "Station 0");
        assert_eq!(stations[29].name, "Station 29");
    }

    #[test]
    fn sanitises_text() {
        let long_name = "N".repeat(120);
        let long_tag = "t".repeat(90);
        let mut entry = good_entry(UUID_A, &format!("  \u{1b}[31mEvil\u{0}\n{long_name}  "), "https://a.example/s");
        entry["tags"] = json!(format!("\t{long_tag},other"));
        entry["country"] = json!(format!("C{}", "x".repeat(100)));
        let stations = filtered(vec![entry], 30);
        let station = &stations[0];
        assert!(!station.name.chars().any(|c| c.is_control()));
        assert!(station.name.starts_with("[31mEvil"));
        assert_eq!(station.name.chars().count(), 80);
        assert_eq!(station.genre.chars().count(), 40);
        assert_eq!(station.country.chars().count(), 40);
    }

    #[test]
    fn id_validation_accepts_only_lowercase_hyphenated_uuids() {
        assert_eq!(rb_uuid(&format!("rb-{UUID_A}")), Some(UUID_A));
        assert_eq!(rb_uuid(UUID_A), None, "needs the rb- prefix");
        assert_eq!(rb_uuid("rb-"), None);
        assert_eq!(rb_uuid("rb-11111111111111111111111111111111"), None, "no hyphens");
        assert_eq!(rb_uuid("rb-1111111G-1111-1111-1111-111111111111"), None, "not hex");
        assert_eq!(rb_uuid("rb-AAAAAAAA-1111-1111-1111-111111111111"), None, "uppercase");
        assert_eq!(rb_uuid(&format!("rb-{UUID_A}/../x")), None);
        assert_eq!(rb_uuid("big100"), None);
    }

    #[test]
    fn genre_chips_map_to_tags() {
        assert_eq!(genre_tag("Rock").as_deref(), Some("rock"));
        assert_eq!(genre_tag("Classic Rock").as_deref(), Some("classic rock"));
        assert_eq!(genre_tag("Alt").as_deref(), Some("alternative"));
        assert_eq!(genre_tag("Lofi").as_deref(), Some("lofi"));
        assert_eq!(genre_tag("Polka"), None);
        assert_eq!(genre_tag(""), None);
        assert_eq!(GENRE_CHIPS.len(), 11);
    }

    #[test]
    fn short_name_fits_a_preset_key() {
        assert_eq!(short_name("Jazz FM"), "Jazz FM");
        assert_eq!(short_name("Radio Paradise Main Mix"), "Radio Parad\u{2026}");
    }

    #[test]
    fn search_url_encodes_and_omits_absent_parameters() {
        let by_name = search_url("https://de1.api.radio-browser.info", Some("jazz & blues"), None).unwrap();
        assert_eq!(
            by_name.as_str(),
            "https://de1.api.radio-browser.info/json/stations/search?name=jazz+%26+blues&limit=60&hidebroken=true&order=clickcount&reverse=true"
        );
        let by_tag = search_url("https://x.example", None, Some("classic rock")).unwrap();
        assert!(by_tag.query().unwrap().starts_with("tag=classic+rock&limit=60"));
        assert!(!by_tag.query().unwrap().contains("name="));
        let both = search_url("https://x.example", Some("kiss"), Some("rock")).unwrap();
        assert!(both.query().unwrap().starts_with("name=kiss&tag=rock&limit=60"));
    }

    #[test]
    fn by_uuid_url_appends_the_uuid() {
        assert_eq!(
            by_uuid_url("https://x.example", UUID_A),
            format!("https://x.example/json/stations/byuuid/{UUID_A}")
        );
    }

    #[test]
    fn server_names_keep_only_radio_browser_hosts() {
        let entries = vec![
            json!({"ip": "1.2.3.4", "name": "de1.api.radio-browser.info"}),
            json!({"ip": "5.6.7.8", "name": "evil.example.com"}),
            json!({"ip": "9.9.9.9", "name": "a.radio-browser.info/../x"}),
            json!({"ip": "9.9.9.9"}),
            json!({"name": "fi1.api.radio-browser.info"}),
        ];
        assert_eq!(
            parse_server_names(&entries),
            vec!["de1.api.radio-browser.info".to_string(), "fi1.api.radio-browser.info".to_string()]
        );
    }

    #[tokio::test]
    async fn by_uuid_refuses_a_malformed_uuid_without_a_request() {
        // Port 9 (discard) is never contacted: the uuid check returns first.
        let browser = HttpRadioBrowser::with_base("http://127.0.0.1:9".to_string());
        assert_eq!(browser.by_uuid("../etc/passwd").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_unreachable_server_is_reported_as_unavailable() {
        let browser = HttpRadioBrowser::with_base("http://127.0.0.1:9".to_string());
        assert!(matches!(browser.search(Some("jazz"), None).await, Err(RbError::Unavailable(_))));
    }
}
