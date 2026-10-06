//! Radio Browser (https://www.radio-browser.info) client: station search and
//! lookup by uuid. Everything it returns is untrusted text and is sanitised
//! here, so the rest of the app can treat an `RbStation` as clean.

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;

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
}
