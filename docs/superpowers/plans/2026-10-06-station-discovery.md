# Station Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let anyone on the LAN find stations beyond the curated ROCK list through a search drawer, and keep favourites in a shared MY band with a one-tap star.

**Architecture:** A new `radiobrowser` module (behind a trait, so tests use a fake) searches Radio Browser and sanitises its text. A `MyStore` persists the household list atomically and a `SearchCache` remembers recent results. `StationManager` composes both, `api.rs` resolves every `rb-<uuid>` id to a URL server-side (the browser only ever sends ids), and the vanilla JS UI gains a three-way band switch, a LCD star and a search drawer.

**Tech Stack:** Rust (axum 0.8, tokio, reqwest 0.12 with rustls, async-trait, serde, thiserror 2, tokio-stream), `tempfile` as a dev-dependency, vanilla HTML/CSS/JS embedded with rust-embed.

**Spec:** docs/superpowers/specs/2026-10-06-station-discovery-design.md

## Global Constraints

- ids-only invariant: the browser never sends a stream URL; the server resolves every id (ROCK/CLIAMP registry, MY entries, search cache, then Radio Browser `by_uuid`).
- Radio Browser ids are `rb-` plus a lowercase hyphenated UUID; any other `rb-` id is 400 `bad_station`.
- MY cap is 50 entries; the 51st add is 409 `my_full`; adding a station already in MY and removing one not in MY are no-ops.
- Search returns at most 30 results (the request asks Radio Browser for `limit=60&hidebroken=true&order=clickcount&reverse=true`, then filters and truncates).
- Radio Browser requests time out after 4 s; any failure is 503 `search_unavailable` and nothing else depends on Radio Browser.
- Search cache: 10 minutes, 200 entries.
- Codec filter: MP3 and AAC/AAC+, case-insensitive; also `lastcheckok == 1`, `hls == 0`, an http(s) `url_resolved`, and de-duplication by name + url.
- Text caps: station names 80 characters, tags 40; Radio Browser text is trimmed and stripped of control characters.
- Chip list: Rock, Classic Rock, Alt, Jazz, Blues, Country, Oldies, Classical, Lofi, News, Talk; the server maps `Alt` to the tag `alternative` and every other chip to its lowercase label.
- Search needs `q` (at least 2 characters) or `genre` (one chip name), else 400 `bad_query`.
- Every Radio Browser request sends `User-Agent: homeradio/<version>` (the crate version).
- MY file is `<cache_dir>/my-stations.json`; every write is atomic (temp file in the same directory, fsync, rename); a missing or corrupt file loads as an empty list and is logged, never fatal.
- No third-party requests from the page (no station logos or favicons).
- The UI renders untrusted text with `textContent` only, never `innerHTML`.
- `cargo test` and `cargo clippy --all-targets -- -D warnings` must pass at the end of every Rust task.
- New pub Rust fns get doc comments; new JS functions get JSDoc.
- Early returns and descriptive names (no single-letter variables except loop indices); match the surrounding style.
- The repo is NOT rustfmt-formatted: do not run `cargo fmt`.
- Clippy `-D warnings` rejects unused items: each item is introduced in the task that first uses it.
- Keep each commit small; nothing in this plan is pushed.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/radiobrowser.rs` (create) | Radio Browser types, genre chip mapping, id validation, text sanitising, response filter, `RadioBrowser` trait, `HttpRadioBrowser` client |
| `src/my_stations.rs` (create) | `MyStore`: the persisted MY list (refs and Radio Browser entries), cap, atomic writes, tolerant load |
| `src/search_cache.rs` (create) | `SearchCache`: 10 minute, 200 entry cache of recent search results keyed by `rb-` id |
| `src/lib.rs` (modify) | Declare the three new modules |
| `src/stations.rs` (modify) | `StationManager` owns `MyStore` and `SearchCache`; API view types (`RegistryView`); URL and name resolution incl. the station playing now |
| `src/state.rs` (modify) | Map the playing URL back to a station id via `station_id_for_url` |
| `src/api.rs` (modify) | `AppState` gains the Radio Browser client and the `stations` broadcast; `/api/search`, `/api/my`, `/api/my/{id}`, `rb-` ids in `/api/play`, `stations` SSE event |
| `src/main.rs` (modify) | Wire `HttpRadioBrowser` and the broadcast channel into `AppState` |
| `Cargo.toml`, `Cargo.lock` (modify) | `tempfile` dev-dependency |
| `tests/integration_test.rs` (modify) | `FakeRadioBrowser`, tempdir cache dirs, discovery integration tests |
| `docs/API.md`, `README.md`, `deploy/provision.sh` (modify) | Contract, user docs, and creating `/var/lib/home-radio` |
| `web/index.html` (modify) | Three-way band switch, LCD star, search button, drawer markup |
| `web/app.css` (modify) | Segmented switch, star, drawer (bottom sheet up to 900 px, side panel above) |
| `web/app.js` (modify) | Band state `rock` / `cliamp` / `my`, MY rendering, star toggle, `stations` SSE refresh, drawer, debounced search, chips, results |

Task order: Rust modules (1 to 4), wiring (5 to 7), endpoints (8 to 10), docs (11), web (12 to 15). The web tasks have no JS test harness (and this plan does not add one): their test steps are `node --check web/app.js`, grep assertions and a concrete browser checklist.

---

### Task 1: Radio Browser types, id validation and response filter

This task is about 350 lines, roughly half of them tests, all in one new file with one cohesive purpose (pure functions, no I/O). It is kept whole because splitting it would force public-but-unused items to satisfy clippy; it is far below the size that times out the review.

**Files:**
- Create: `src/radiobrowser.rs`
- Modify: `src/lib.rs` (add `pub mod radiobrowser;` after `pub mod policy;`)
- Test: unit tests in `src/radiobrowser.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing (first task). Existing deps `serde`, `serde_json`, `async-trait`, `thiserror`.
- Produces (all in `crate::radiobrowser`):
  - `pub const SEARCH_LIMIT: usize = 30;` and `pub const GENRE_CHIPS: [&str; 11]`
  - `pub fn genre_tag(label: &str) -> Option<String>` (chip label to Radio Browser tag, `Alt` to `alternative`, `None` for unknown labels)
  - `pub struct RbStation { pub uuid: String, pub name: String, pub genre: String, pub country: String, pub bitrate: u32, pub url: String }` with `pub fn id(&self) -> String` (`rb-<uuid>`) and `pub fn short_name(&self) -> String`
  - `pub fn short_name(name: &str) -> String` (12 characters, ellipsis when cut)
  - `pub enum RbError { Unavailable(String) }`
  - `#[async_trait] pub trait RadioBrowser: Send + Sync { async fn search(&self, name: Option<&str>, tag: Option<&str>) -> Result<Vec<RbStation>, RbError>; async fn by_uuid(&self, uuid: &str) -> Result<Option<RbStation>, RbError>; }`
  - `pub fn is_valid_uuid(candidate: &str) -> bool`, `pub fn rb_uuid(id: &str) -> Option<&str>`
  - `pub struct RawStation`, `pub fn parse_raw_stations(entries: Vec<serde_json::Value>) -> Vec<RawStation>`, `pub fn filter_stations(raw: Vec<RawStation>, limit: usize) -> Vec<RbStation>`

- [ ] **Step 1: Write the failing tests**

Declare the module in `src/lib.rs`:

```diff
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -2,6 +2,7 @@
 pub mod cliamp;
 pub mod config;
 pub mod policy;
+pub mod radiobrowser;
 pub mod route;
 pub mod stations;
 pub mod state;
```

Create `src/radiobrowser.rs` containing only the test module:

```rust
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
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --lib radiobrowser`

Expected: the lib test target does not compile, with about 21 errors such as ``error[E0425]: cannot find type `RbStation` in this scope``, ``cannot find value `SEARCH_LIMIT` in this scope`` and ``cannot find value `GENRE_CHIPS` in this scope`` (none of the items exist yet).

- [ ] **Step 3: Implement**

Insert this at the top of `src/radiobrowser.rs`, above the existing `#[cfg(test)]` line:

```rust
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
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --lib radiobrowser && cargo clippy --all-targets -- -D warnings`

Expected: `test result: ok. 10 passed` and clippy finishes with no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/radiobrowser.rs src/lib.rs
git commit -m "$(cat <<'EOF'
radiobrowser: station types, uuid validation and response filter

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Radio Browser HTTP client

**Files:**
- Modify: `src/radiobrowser.rs` (imports at the top; new code between `filter_stations` and `#[cfg(test)]`; new tests at the end of `mod tests`)
- Test: unit tests in `src/radiobrowser.rs`

**Interfaces:**
- Consumes: `RbStation`, `RbError`, `RadioBrowser`, `RawStation`, `parse_raw_stations`, `filter_stations`, `is_valid_uuid`, `SEARCH_LIMIT` from Task 1.
- Produces:
  - `pub const REQUEST_TIMEOUT: Duration` (4 s)
  - `pub(crate) fn search_url(base: &str, name: Option<&str>, tag: Option<&str>) -> Result<reqwest::Url, RbError>`: `GET {base}/json/stations/search?[name=][&tag=]&limit=60&hidebroken=true&order=clickcount&reverse=true`
  - `pub(crate) fn by_uuid_url(base: &str, uuid: &str) -> String`
  - `pub(crate) fn parse_server_names(entries: &[serde_json::Value]) -> Vec<String>` (keeps only `*.radio-browser.info` host names)
  - `pub struct HttpRadioBrowser` with `pub async fn connect() -> Self` (resolves a mirror from `https://all.api.radio-browser.info/json/servers`, falls back to `https://de1.api.radio-browser.info`, never fails) and `pub fn with_base(base: String) -> Self`; implements `RadioBrowser`. Sends `User-Agent: homeradio/<CARGO_PKG_VERSION>` and a 4 s timeout.

- [ ] **Step 1: Write the failing tests**

Append these tests inside `mod tests` in `src/radiobrowser.rs`:

```diff
--- a/src/radiobrowser.rs
+++ b/src/radiobrowser.rs
@@ -346,4 +346,54 @@
         assert_eq!(short_name("Jazz FM"), "Jazz FM");
         assert_eq!(short_name("Radio Paradise Main Mix"), "Radio Parad\u{2026}");
     }
+
+    #[test]
+    fn search_url_encodes_and_omits_absent_parameters() {
+        let by_name = search_url("https://de1.api.radio-browser.info", Some("jazz & blues"), None).unwrap();
+        assert_eq!(
+            by_name.as_str(),
+            "https://de1.api.radio-browser.info/json/stations/search?name=jazz+%26+blues&limit=60&hidebroken=true&order=clickcount&reverse=true"
+        );
+        let by_tag = search_url("https://x.example", None, Some("classic rock")).unwrap();
+        assert!(by_tag.query().unwrap().starts_with("tag=classic+rock&limit=60"));
+        assert!(!by_tag.query().unwrap().contains("name="));
+        let both = search_url("https://x.example", Some("kiss"), Some("rock")).unwrap();
+        assert!(both.query().unwrap().starts_with("name=kiss&tag=rock&limit=60"));
+    }
+
+    #[test]
+    fn by_uuid_url_appends_the_uuid() {
+        assert_eq!(
+            by_uuid_url("https://x.example", UUID_A),
+            format!("https://x.example/json/stations/byuuid/{UUID_A}")
+        );
+    }
+
+    #[test]
+    fn server_names_keep_only_radio_browser_hosts() {
+        let entries = vec![
+            json!({"ip": "1.2.3.4", "name": "de1.api.radio-browser.info"}),
+            json!({"ip": "5.6.7.8", "name": "evil.example.com"}),
+            json!({"ip": "9.9.9.9", "name": "a.radio-browser.info/../x"}),
+            json!({"ip": "9.9.9.9"}),
+            json!({"name": "fi1.api.radio-browser.info"}),
+        ];
+        assert_eq!(
+            parse_server_names(&entries),
+            vec!["de1.api.radio-browser.info".to_string(), "fi1.api.radio-browser.info".to_string()]
+        );
+    }
+
+    #[tokio::test]
+    async fn by_uuid_refuses_a_malformed_uuid_without_a_request() {
+        // Port 9 (discard) is never contacted: the uuid check returns first.
+        let browser = HttpRadioBrowser::with_base("http://127.0.0.1:9".to_string());
+        assert_eq!(browser.by_uuid("../etc/passwd").await.unwrap(), None);
+    }
+
+    #[tokio::test]
+    async fn an_unreachable_server_is_reported_as_unavailable() {
+        let browser = HttpRadioBrowser::with_base("http://127.0.0.1:9".to_string());
+        assert!(matches!(browser.search(Some("jazz"), None).await, Err(RbError::Unavailable(_))));
+    }
 }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --lib radiobrowser`

Expected: compile failure, about 7 errors such as ``error[E0425]: cannot find function `search_url` in this scope`` and ``cannot find type `HttpRadioBrowser` in this scope``.

- [ ] **Step 3: Implement**

Apply this change to `src/radiobrowser.rs` (the `Duration` import moves here because Task 1 had no use for it):

```diff
--- a/src/radiobrowser.rs
+++ b/src/radiobrowser.rs
@@ -5,6 +5,7 @@
 use async_trait::async_trait;
 use serde::Deserialize;
 use std::collections::HashSet;
+use std::time::Duration;
 
 /// Results returned to the caller after filtering.
 pub const SEARCH_LIMIT: usize = 30;
@@ -191,6 +192,132 @@
     stations
 }
 
+const ALL_SERVERS_BASE: &str = "https://all.api.radio-browser.info";
+const FALLBACK_BASE: &str = "https://de1.api.radio-browser.info";
+/// Results fetched before filtering; `SEARCH_LIMIT` survive.
+const FETCH_LIMIT: &str = "60";
+/// Per-request timeout.
+pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
+
+/// The `GET /json/stations/search` URL for a name and/or tag search.
+pub(crate) fn search_url(
+    base: &str,
+    name: Option<&str>,
+    tag: Option<&str>,
+) -> Result<reqwest::Url, RbError> {
+    let mut params: Vec<(&str, &str)> = Vec::new();
+    if let Some(name) = name {
+        params.push(("name", name));
+    }
+    if let Some(tag) = tag {
+        params.push(("tag", tag));
+    }
+    params.extend([
+        ("limit", FETCH_LIMIT),
+        ("hidebroken", "true"),
+        ("order", "clickcount"),
+        ("reverse", "true"),
+    ]);
+    reqwest::Url::parse_with_params(&format!("{base}/json/stations/search"), &params)
+        .map_err(|error| RbError::Unavailable(error.to_string()))
+}
+
+/// The `GET /json/stations/byuuid/<uuid>` URL. The caller has validated `uuid`.
+pub(crate) fn by_uuid_url(base: &str, uuid: &str) -> String {
+    format!("{base}/json/stations/byuuid/{uuid}")
+}
+
+/// Host names from a `/json/servers` response, keeping only Radio Browser hosts.
+pub(crate) fn parse_server_names(entries: &[serde_json::Value]) -> Vec<String> {
+    entries
+        .iter()
+        .filter_map(|entry| entry.get("name")?.as_str())
+        .filter(|name| name.ends_with(".radio-browser.info"))
+        .filter(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
+        .map(str::to_string)
+        .collect()
+}
+
+/// Radio Browser over HTTPS, pinned to one mirror chosen at startup.
+pub struct HttpRadioBrowser {
+    client: reqwest::Client,
+    base: String,
+}
+
+impl HttpRadioBrowser {
+    /// Resolves `all.api.radio-browser.info` to one mirror (falling back to
+    /// `de1`) and builds the client. Never fails: if Radio Browser is down,
+    /// searches fail later and the rest of the app is unaffected.
+    pub async fn connect() -> Self {
+        let client = Self::build_client();
+        let base = match Self::pick_mirror(&client).await {
+            Some(host) => format!("https://{host}"),
+            None => FALLBACK_BASE.to_string(),
+        };
+        tracing::info!("Radio Browser mirror: {}", base);
+        Self { client, base }
+    }
+
+    /// Builds a client for a known base URL (used by tests and tools).
+    pub fn with_base(base: String) -> Self {
+        Self { client: Self::build_client(), base }
+    }
+
+    /// The shared client: 4 s timeout and the `User-Agent` Radio Browser asks for.
+    fn build_client() -> reqwest::Client {
+        reqwest::Client::builder()
+            .timeout(REQUEST_TIMEOUT)
+            .user_agent(format!("homeradio/{}", env!("CARGO_PKG_VERSION")))
+            .build()
+            .expect("static client configuration is valid")
+    }
+
+    async fn pick_mirror(client: &reqwest::Client) -> Option<String> {
+        let url = format!("{ALL_SERVERS_BASE}/json/servers");
+        let response = client.get(url).send().await.ok()?.error_for_status().ok()?;
+        let entries: Vec<serde_json::Value> = response.json().await.ok()?;
+        let names = parse_server_names(&entries);
+        if names.is_empty() {
+            return None;
+        }
+        let nanos = std::time::SystemTime::now()
+            .duration_since(std::time::UNIX_EPOCH)
+            .map(|elapsed| elapsed.subsec_nanos() as usize)
+            .unwrap_or(0);
+        names.get(nanos % names.len()).cloned()
+    }
+
+    async fn get_entries(&self, url: reqwest::Url) -> Result<Vec<serde_json::Value>, RbError> {
+        let unavailable = |error: reqwest::Error| RbError::Unavailable(error.to_string());
+        let response = self.client.get(url).send().await.map_err(unavailable)?;
+        let response = response.error_for_status().map_err(unavailable)?;
+        response.json().await.map_err(unavailable)
+    }
+}
+
+#[async_trait]
+impl RadioBrowser for HttpRadioBrowser {
+    async fn search(
+        &self,
+        name: Option<&str>,
+        tag: Option<&str>,
+    ) -> Result<Vec<RbStation>, RbError> {
+        let url = search_url(&self.base, name, tag)?;
+        let entries = self.get_entries(url).await?;
+        Ok(filter_stations(parse_raw_stations(entries), SEARCH_LIMIT))
+    }
+
+    async fn by_uuid(&self, uuid: &str) -> Result<Option<RbStation>, RbError> {
+        if !is_valid_uuid(uuid) {
+            return Ok(None);
+        }
+        let url = reqwest::Url::parse(&by_uuid_url(&self.base, uuid))
+            .map_err(|error| RbError::Unavailable(error.to_string()))?;
+        let entries = self.get_entries(url).await?;
+        Ok(filter_stations(parse_raw_stations(entries), 1).into_iter().next())
+    }
+}
+
 #[cfg(test)]
 mod tests {
     use super::*;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --lib radiobrowser && cargo clippy --all-targets -- -D warnings`

Expected: `test result: ok. 15 passed` and no clippy warnings. The two client tests that talk to a server use a malformed uuid and an unreachable local port, so no test touches the network.

- [ ] **Step 5: Commit**

```bash
git add src/radiobrowser.rs
git commit -m "$(cat <<'EOF'
radiobrowser: HTTP client with mirror resolution, UA and 4 s timeout

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: MY store

This task is about 365 lines, roughly 150 of them tests, in one new file (the persistence layer). It is kept whole because the cap, the dedupe and the atomic write are one unit of behaviour.

**Files:**
- Create: `src/my_stations.rs`
- Modify: `src/lib.rs` (add `pub mod my_stations;` after `pub mod config;`), `Cargo.toml` (dev-dependency `tempfile = "3"`), `Cargo.lock` (updated by cargo)
- Test: unit tests in `src/my_stations.rs`

**Interfaces:**
- Consumes: `RbStation`, `rb_uuid` from Task 1.
- Produces (in `crate::my_stations`):
  - `pub const MY_CAP: usize = 50;`
  - `pub struct StoredRbStation { pub id: String, pub name: String, pub short: String, pub genre: String, pub url: String }` with `pub fn from_rb(station: &RbStation) -> Self`; serde shape `{"id":"rb-<uuid>","name":..,"short":..,"genre":..,"url":..}`
  - `#[serde(untagged)] pub enum MyEntry { Ref { #[serde(rename = "ref")] station_id: String }, Rb(StoredRbStation) }` with `pub fn id(&self) -> &str`
  - `pub enum MyError { Full, Save(std::io::Error) }`
  - `pub struct MyStore` with `pub fn load(path: PathBuf) -> Self`, `entries(&self) -> &[MyEntry]`, `contains(&self, id: &str) -> bool`, `get_rb(&self, id: &str) -> Option<&StoredRbStation>`, `rb_entries(&self) -> impl Iterator<Item = &StoredRbStation>`, `async fn add(&mut self, entry: MyEntry) -> Result<bool, MyError>` (`Ok(false)` for a duplicate), `async fn remove(&mut self, id: &str) -> Result<bool, MyError>` (`Ok(false)` when absent)
  - File format: a JSON array such as `[{"ref":"big100"},{"id":"rb-...","name":"..","short":"..","genre":"..","url":".."}]`, insertion order, newest last.

- [ ] **Step 1: Write the failing tests**

Add the module and the dev-dependency:

```diff
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -33,3 +33,4 @@
 
 [dev-dependencies]
 axum-test = "21"
+tempfile = "3"
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,6 +1,7 @@
 pub mod api;
 pub mod cliamp;
 pub mod config;
+pub mod my_stations;
 pub mod policy;
 pub mod radiobrowser;
 pub mod route;
```

Cargo.lock will pick up `tempfile` on the next build; commit it with the task.

Create `src/my_stations.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const UUID_A: &str = "11111111-1111-1111-1111-111111111111";

    fn reference(id: &str) -> MyEntry {
        MyEntry::Ref { station_id: id.to_string() }
    }

    fn rb(uuid: &str) -> MyEntry {
        MyEntry::Rb(StoredRbStation {
            id: format!("rb-{uuid}"),
            name: "Jazz FM".to_string(),
            short: "Jazz FM".to_string(),
            genre: "jazz".to_string(),
            url: "https://jazz.example/stream".to_string(),
        })
    }

    fn store_in(dir: &tempfile::TempDir) -> MyStore {
        MyStore::load(dir.path().join("my-stations.json"))
    }

    #[tokio::test]
    async fn adds_in_insertion_order_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        assert!(store.add(reference("big100")).await.unwrap());
        assert!(store.add(rb(UUID_A)).await.unwrap());

        let reloaded = store_in(&dir);
        let ids: Vec<&str> = reloaded.entries().iter().map(MyEntry::id).collect();
        assert_eq!(ids, vec!["big100", &format!("rb-{UUID_A}")[..]]);
        assert_eq!(reloaded.get_rb(&format!("rb-{UUID_A}")).unwrap().url, "https://jazz.example/stream");
        assert!(reloaded.get_rb("big100").is_none());
    }

    #[test]
    fn from_rb_keeps_the_stream_url_and_a_short_label() {
        let station = RbStation {
            uuid: UUID_A.to_string(),
            name: "Radio Paradise Main Mix".to_string(),
            genre: "eclectic".to_string(),
            country: "United States".to_string(),
            bitrate: 320,
            url: "https://stream.example/rp".to_string(),
        };
        let stored = StoredRbStation::from_rb(&station);
        assert_eq!(stored.id, format!("rb-{UUID_A}"));
        assert_eq!(stored.name, "Radio Paradise Main Mix");
        assert_eq!(stored.short, "Radio Parad\u{2026}");
        assert_eq!(stored.url, "https://stream.example/rp");
    }

    #[tokio::test]
    async fn file_format_is_a_list_of_refs_and_stations() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        store.add(reference("big100")).await.unwrap();
        store.add(rb(UUID_A)).await.unwrap();

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("my-stations.json")).unwrap()).unwrap();
        assert_eq!(on_disk[0], serde_json::json!({"ref": "big100"}));
        assert_eq!(on_disk[1]["id"], format!("rb-{UUID_A}"));
        assert_eq!(on_disk[1]["url"], "https://jazz.example/stream");
    }

    #[tokio::test]
    async fn adding_a_duplicate_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        assert!(store.add(reference("big100")).await.unwrap());
        assert!(!store.add(reference("big100")).await.unwrap());
        assert_eq!(store.entries().len(), 1);
    }

    #[tokio::test]
    async fn the_fifty_first_entry_is_refused_but_a_duplicate_is_still_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        for index in 0..MY_CAP {
            assert!(store.add(reference(&format!("station-{index}"))).await.unwrap());
        }
        assert!(matches!(store.add(reference("one-too-many")).await, Err(MyError::Full)));
        assert_eq!(store.entries().len(), MY_CAP);
        assert!(!store.add(reference("station-0")).await.unwrap());
    }

    #[tokio::test]
    async fn removing_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        store.add(reference("big100")).await.unwrap();
        store.add(reference("lofi")).await.unwrap();
        assert!(store.remove("big100").await.unwrap());
        assert!(!store.remove("big100").await.unwrap());
        assert!(!store.remove("never-added").await.unwrap());
        assert_eq!(store_in(&dir).entries(), &[reference("lofi")]);
    }

    #[test]
    fn a_missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(store_in(&dir).entries().is_empty());
    }

    #[test]
    fn a_corrupt_file_loads_empty_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my-stations.json"), "{ this is not json").unwrap();
        assert!(store_in(&dir).entries().is_empty());
    }

    #[test]
    fn malformed_and_duplicate_entries_are_dropped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let json = serde_json::json!([
            {"ref": "big100"},
            {"ref": "big100"},
            {"ref": ""},
            {"id": "rb-not-a-uuid", "name": "x", "short": "x", "genre": "", "url": "https://x.example/s"},
            {"id": format!("rb-{UUID_A}"), "name": "x", "short": "x", "genre": "", "url": "file:///etc/passwd"},
            {"id": format!("rb-{UUID_A}"), "name": "Ok", "short": "Ok", "genre": "", "url": "http://ok.example/s"}
        ]);
        std::fs::write(dir.path().join("my-stations.json"), json.to_string()).unwrap();
        let ids: Vec<String> = store_in(&dir).entries().iter().map(|e| e.id().to_string()).collect();
        assert_eq!(ids, vec!["big100".to_string(), format!("rb-{UUID_A}")]);
    }

    #[tokio::test]
    async fn a_failed_save_leaves_the_list_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        // The parent directory does not exist, so the temp file cannot be created.
        let mut store = MyStore::load(dir.path().join("missing-dir").join("my-stations.json"));
        assert!(matches!(store.add(reference("big100")).await, Err(MyError::Save(_))));
        assert!(store.entries().is_empty());
    }

    #[tokio::test]
    async fn a_save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        store.add(reference("big100")).await.unwrap();
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["my-stations.json".to_string()]);
    }
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --lib my_stations`

Expected: the lib test target does not compile, with about 15 errors such as ``error[E0425]: cannot find type `MyEntry` in this scope`` and ``cannot find struct, variant or union type `StoredRbStation` in this scope``.

- [ ] **Step 3: Implement**

Insert this at the top of `src/my_stations.rs`, above the existing `#[cfg(test)]` line:

```rust
//! The household's shared MY list, persisted as JSON next to the station cache.
//!
//! An entry is either a reference to a curated (ROCK/CLIAMP) station id or a
//! full Radio Browser station whose stream URL is kept server-side only.

use crate::radiobrowser::{rb_uuid, RbStation};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

/// Most entries MY can hold.
pub const MY_CAP: usize = 50;

/// A Radio Browser station saved in MY.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRbStation {
    pub id: String,
    pub name: String,
    pub short: String,
    pub genre: String,
    pub url: String,
}

impl StoredRbStation {
    /// The persisted form of a Radio Browser station.
    pub fn from_rb(station: &RbStation) -> Self {
        Self {
            id: station.id(),
            name: station.name.clone(),
            short: station.short_name(),
            genre: station.genre.clone(),
            url: station.url.clone(),
        }
    }
}

/// One MY entry: `{"ref":"big100"}` or a stored Radio Browser station.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MyEntry {
    Ref {
        #[serde(rename = "ref")]
        station_id: String,
    },
    Rb(StoredRbStation),
}

impl MyEntry {
    /// The station id this entry stands for.
    pub fn id(&self) -> &str {
        match self {
            MyEntry::Ref { station_id } => station_id,
            MyEntry::Rb(station) => &station.id,
        }
    }

    /// An entry read from disk is kept only if it is well formed: a non-empty
    /// ref, or an `rb-<uuid>` id with an http(s) stream URL.
    fn is_well_formed(&self) -> bool {
        match self {
            MyEntry::Ref { station_id } => !station_id.is_empty(),
            MyEntry::Rb(station) => {
                let lowered = station.url.to_ascii_lowercase();
                rb_uuid(&station.id).is_some()
                    && (lowered.starts_with("http://") || lowered.starts_with("https://"))
            }
        }
    }
}

/// Why a MY change was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum MyError {
    #[error("MY is full ({MY_CAP} stations)")]
    Full,
    #[error("could not save MY: {0}")]
    Save(#[from] std::io::Error),
}

/// The MY list and its backing file. Changes are written to disk first and
/// only then applied in memory, so a failed save leaves both unchanged.
pub struct MyStore {
    path: PathBuf,
    entries: Vec<MyEntry>,
}

impl MyStore {
    /// Loads the list from `path`. A missing file is an empty list; a corrupt
    /// one is logged and also loads as empty, never an error.
    pub fn load(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(contents) => Self::parse(&contents, &path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                warn!("Could not read {}: {}; starting with an empty MY list", path.display(), error);
                Vec::new()
            }
        };
        info!("Loaded {} MY stations", entries.len());
        Self { path, entries }
    }

    fn parse(contents: &str, path: &Path) -> Vec<MyEntry> {
        let parsed: Vec<MyEntry> = match serde_json::from_str(contents) {
            Ok(parsed) => parsed,
            Err(error) => {
                warn!("{} is corrupt ({}); starting with an empty MY list", path.display(), error);
                return Vec::new();
            }
        };
        let mut entries: Vec<MyEntry> = Vec::new();
        for entry in parsed {
            if !entry.is_well_formed() {
                warn!("Dropping malformed MY entry {:?}", entry.id());
                continue;
            }
            if entries.iter().any(|kept| kept.id() == entry.id()) {
                continue;
            }
            entries.push(entry);
        }
        entries.truncate(MY_CAP);
        entries
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &[MyEntry] {
        &self.entries
    }

    /// True when `id` is in MY.
    pub fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.id() == id)
    }

    /// The saved Radio Browser station with this id, if any.
    pub fn get_rb(&self, id: &str) -> Option<&StoredRbStation> {
        self.rb_entries().find(|station| station.id == id)
    }

    /// Every saved Radio Browser station.
    pub fn rb_entries(&self) -> impl Iterator<Item = &StoredRbStation> {
        self.entries.iter().filter_map(|entry| match entry {
            MyEntry::Rb(station) => Some(station),
            MyEntry::Ref { .. } => None,
        })
    }

    /// Appends `entry`. Returns `Ok(false)` without writing when it is already
    /// in MY, and `Err(MyError::Full)` when MY already holds `MY_CAP` entries.
    pub async fn add(&mut self, entry: MyEntry) -> Result<bool, MyError> {
        if self.contains(entry.id()) {
            return Ok(false);
        }
        if self.entries.len() >= MY_CAP {
            return Err(MyError::Full);
        }
        let mut updated = self.entries.clone();
        updated.push(entry);
        self.commit(updated).await?;
        Ok(true)
    }

    /// Removes the entry with this id. Returns `Ok(false)` without writing
    /// when it was not in MY.
    pub async fn remove(&mut self, id: &str) -> Result<bool, MyError> {
        if !self.contains(id) {
            return Ok(false);
        }
        let updated: Vec<MyEntry> = self.entries.iter().filter(|entry| entry.id() != id).cloned().collect();
        self.commit(updated).await?;
        Ok(true)
    }

    async fn commit(&mut self, updated: Vec<MyEntry>) -> Result<(), MyError> {
        let bytes = serde_json::to_vec_pretty(&updated).map_err(std::io::Error::other)?;
        write_atomic(&self.path, &bytes).await?;
        self.entries = updated;
        Ok(())
    }
}

/// Writes `bytes` to a temp file in the same directory, fsyncs it, then
/// renames it over `path`, so readers see the old or the new file, never half.
async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "my-stations.json".to_string());
    let temp_path = path.with_file_name(format!(".{file_name}.tmp"));
    let result = write_and_sync(&temp_path, bytes).await;
    if result.is_err() {
        tokio::fs::remove_file(&temp_path).await.ok();
        return result;
    }
    let renamed = tokio::fs::rename(&temp_path, path).await;
    if renamed.is_err() {
        tokio::fs::remove_file(&temp_path).await.ok();
    }
    renamed
}

async fn write_and_sync(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = tokio::fs::File::create(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await
}
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --lib my_stations && cargo clippy --all-targets -- -D warnings`

Expected: `test result: ok. 11 passed` and no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/my_stations.rs src/lib.rs Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
my_stations: shared MY list with a cap of 50 and atomic saves

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Search cache

**Files:**
- Create: `src/search_cache.rs`
- Modify: `src/lib.rs` (add `pub mod search_cache;` after `pub mod route;`)
- Test: unit tests in `src/search_cache.rs`

**Interfaces:**
- Consumes: `RbStation` from Task 1.
- Produces (in `crate::search_cache`):
  - `pub const CACHE_TTL: Duration` (10 minutes) and `pub const CACHE_CAPACITY: usize = 200;`
  - `pub struct SearchCache` with `pub fn new() -> Self`, `pub fn with_limits(ttl: Duration, capacity: usize) -> Self`, `pub fn insert_all(&mut self, stations: &[RbStation], now: Instant)`, `pub fn get(&self, id: &str, now: Instant) -> Option<RbStation>` (id is the `rb-<uuid>` id; expired entries are not returned)
  - `now` is a parameter so tests control time without sleeping.

- [ ] **Step 1: Write the failing tests**

Declare the module:

```diff
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -5,6 +5,7 @@
 pub mod policy;
 pub mod radiobrowser;
 pub mod route;
+pub mod search_cache;
 pub mod stations;
 pub mod state;
 pub mod title;
```

Create `src/search_cache.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn station(index: usize) -> RbStation {
        RbStation {
            uuid: format!("{index:08}-0000-0000-0000-000000000000"),
            name: format!("Station {index}"),
            genre: String::new(),
            country: String::new(),
            bitrate: 128,
            url: format!("https://a.example/{index}"),
        }
    }

    #[test]
    fn returns_a_stored_station_by_id() {
        let mut cache = SearchCache::new();
        let now = Instant::now();
        cache.insert_all(&[station(1)], now);
        assert_eq!(cache.get(&station(1).id(), now).unwrap().name, "Station 1");
        assert!(cache.get(&station(2).id(), now).is_none());
    }

    #[test]
    fn entries_expire_after_the_ttl() {
        let mut cache = SearchCache::with_limits(Duration::from_secs(600), 200);
        let start = Instant::now();
        cache.insert_all(&[station(1)], start);
        assert!(cache.get(&station(1).id(), start + Duration::from_secs(599)).is_some());
        assert!(cache.get(&station(1).id(), start + Duration::from_secs(600)).is_none());
    }

    #[test]
    fn inserting_again_refreshes_the_timestamp() {
        let mut cache = SearchCache::with_limits(Duration::from_secs(600), 200);
        let start = Instant::now();
        cache.insert_all(&[station(1)], start);
        cache.insert_all(&[station(1)], start + Duration::from_secs(500));
        assert!(cache.get(&station(1).id(), start + Duration::from_secs(900)).is_some());
    }

    #[test]
    fn the_oldest_entries_are_evicted_over_capacity() {
        let mut cache = SearchCache::with_limits(Duration::from_secs(600), 3);
        let start = Instant::now();
        cache.insert_all(&[station(1), station(2)], start);
        cache.insert_all(&[station(3), station(4)], start + Duration::from_secs(1));
        let now = start + Duration::from_secs(2);
        assert!(cache.get(&station(3).id(), now).is_some());
        assert!(cache.get(&station(4).id(), now).is_some());
        let survivors = [1, 2].iter().filter(|index| cache.get(&station(**index).id(), now).is_some()).count();
        assert_eq!(survivors, 1, "capacity 3 keeps one of the two oldest");
    }

    #[test]
    fn expired_entries_are_dropped_on_insert() {
        let mut cache = SearchCache::with_limits(Duration::from_secs(10), 200);
        let start = Instant::now();
        cache.insert_all(&[station(1)], start);
        cache.insert_all(&[station(2)], start + Duration::from_secs(20));
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn production_limits_match_the_spec() {
        assert_eq!(CACHE_TTL, Duration::from_secs(600));
        assert_eq!(CACHE_CAPACITY, 200);
    }
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --lib search_cache`

Expected: compile failure, about 26 errors such as ``error[E0425]: cannot find type `RbStation` in this scope`` and ``error[E0433]: cannot find type `Instant` in this scope``.

- [ ] **Step 3: Implement**

Insert this at the top of `src/search_cache.rs`, above the existing `#[cfg(test)]` line:

```rust
//! Short-lived memory of recent search results, so a station the user found in
//! the drawer can be played or kept by id without asking Radio Browser again.

use crate::radiobrowser::RbStation;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

/// How long a search result stays playable by id.
pub const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Most results remembered at once.
pub const CACHE_CAPACITY: usize = 200;

/// Recent search results keyed by `rb-<uuid>` id. Time is passed in so tests
/// need no sleeping.
pub struct SearchCache {
    ttl: Duration,
    capacity: usize,
    entries: HashMap<String, (RbStation, Instant)>,
}

impl SearchCache {
    /// A cache with the production limits (10 minutes, 200 entries).
    pub fn new() -> Self {
        Self::with_limits(CACHE_TTL, CACHE_CAPACITY)
    }

    /// A cache with explicit limits.
    pub fn with_limits(ttl: Duration, capacity: usize) -> Self {
        Self { ttl, capacity, entries: HashMap::new() }
    }

    /// Remembers `stations` as of `now`: drops expired entries, then evicts
    /// the oldest ones beyond the capacity.
    pub fn insert_all(&mut self, stations: &[RbStation], now: Instant) {
        let ttl = self.ttl;
        self.entries.retain(|_, (_, stored_at)| now.saturating_duration_since(*stored_at) < ttl);
        for station in stations {
            self.entries.insert(station.id(), (station.clone(), now));
        }
        while self.entries.len() > self.capacity {
            let Some(oldest_id) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, stored_at))| *stored_at)
                .map(|(id, _)| id.clone())
            else {
                return;
            };
            self.entries.remove(&oldest_id);
        }
    }

    /// The station with this id if it was stored less than the TTL ago.
    pub fn get(&self, id: &str, now: Instant) -> Option<RbStation> {
        let (station, stored_at) = self.entries.get(id)?;
        if now.saturating_duration_since(*stored_at) >= self.ttl {
            return None;
        }
        Some(station.clone())
    }
}

impl Default for SearchCache {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --lib search_cache && cargo clippy --all-targets -- -D warnings`

Expected: `test result: ok. 6 passed` and no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/search_cache.rs src/lib.rs
git commit -m "$(cat <<'EOF'
search_cache: 10 minute, 200 entry cache of recent search results

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: StationManager owns MY and the search cache, and builds the API view

**Files:**
- Modify: `src/stations.rs` (imports at the top; new view types after `StationRegistry`; two new fields on `StationManager`; `StationManager::new`; `get_station_url`; new methods after `get_station_name`; new tests in `mod tests`)
- Test: unit tests in `src/stations.rs`

**Interfaces:**
- Consumes: `MyStore`, `MyEntry`, `MyError` (Task 3); `SearchCache` (Task 4); `RbStation` (Task 1).
- Produces (in `crate::stations`):
  - `pub struct StationView { pub id: String, pub name: String, pub short: String, pub genre: String, pub in_my: bool }` (serialises to JSON with no URL)
  - `pub struct GroupView { pub id: String, pub label: String, pub stations: Vec<StationView> }`
  - `pub struct RegistryView { pub groups: Vec<GroupView> }`: the `groups` of `StationRegistry` (each station flagged `in_my`) plus a final `{"id":"my","label":"MY"}` group in insertion order; a reference to a station that no longer exists is left out of the view but stays in the file
  - `StationManager::registry_view(&self) -> RegistryView`
  - `StationManager::get_station_url(&self, id: &str) -> Option<&str>`: now also resolves a saved Radio Browser station from MY (never an unsaved search result)
  - `pub fn is_curated(&self, id: &str) -> bool`, `pub fn is_in_my(&self, id: &str) -> bool`
  - `pub async fn my_add(&mut self, entry: MyEntry) -> Result<bool, MyError>`, `pub async fn my_remove(&mut self, id: &str) -> Result<bool, MyError>`
  - `pub fn remember_search_results(&mut self, stations: &[RbStation])`, `pub fn cached_search_result(&self, id: &str) -> Option<RbStation>`
  - `StationManager::new` loads `<cache_dir>/my-stations.json`.

- [ ] **Step 1: Write the failing tests**

Apply this change to the `tests` module in `src/stations.rs` (the two new struct fields in the existing `rock_takes_precedence` fixture, plus the new tests):

```diff
--- a/src/stations.rs
+++ b/src/stations.rs
@@ -283,6 +283,8 @@
             station_urls: HashMap::new(),
             cache_path: PathBuf::new(),
             remote_url: String::new(),
+            my: MyStore::load(PathBuf::from("/nonexistent/my-stations.json")),
+            search_cache: SearchCache::new(),
         };
 
         manager.rebuild_url_map();
@@ -290,4 +292,120 @@
         // Rock takes precedence
         assert_eq!(manager.get_station_url("test"), Some("http://rock"));
     }
+
+    const UUID_A: &str = "11111111-1111-1111-1111-111111111111";
+
+    fn curated(id: &str, url: &str) -> Station {
+        Station {
+            id: id.to_string(),
+            name: format!("{id} name"),
+            short: id.to_string(),
+            genre: "Rock".to_string(),
+            url: url.to_string(),
+        }
+    }
+
+    fn rb_station(uuid: &str, url: &str) -> RbStation {
+        RbStation {
+            uuid: uuid.to_string(),
+            name: "Jazz FM".to_string(),
+            genre: "jazz".to_string(),
+            country: "France".to_string(),
+            bitrate: 128,
+            url: url.to_string(),
+        }
+    }
+
+    fn stored(station: &RbStation) -> MyEntry {
+        MyEntry::Rb(crate::my_stations::StoredRbStation::from_rb(station))
+    }
+
+    /// A manager with one ROCK (`big100`) and one CLIAMP (`lofi`) station and
+    /// an empty MY list stored in `dir`.
+    fn manager_in(dir: &Path) -> StationManager {
+        let mut manager = StationManager {
+            rock_stations: vec![curated("big100", "http://rock/big100")],
+            cliamp_stations: vec![curated("lofi", "http://cliamp/lofi")],
+            station_urls: HashMap::new(),
+            cache_path: PathBuf::new(),
+            remote_url: String::new(),
+            my: MyStore::load(dir.join("my-stations.json")),
+            search_cache: SearchCache::new(),
+        };
+        manager.rebuild_url_map();
+        manager
+    }
+
+    fn group<'a>(view: &'a RegistryView, id: &str) -> &'a GroupView {
+        view.groups.iter().find(|group| group.id == id).unwrap()
+    }
+
+    #[tokio::test]
+    async fn registry_view_adds_an_empty_my_group_and_in_my_flags() {
+        let dir = tempfile::tempdir().unwrap();
+        let manager = manager_in(dir.path());
+        let view = manager.registry_view();
+        let ids: Vec<&str> = view.groups.iter().map(|group| group.id.as_str()).collect();
+        assert_eq!(ids, vec!["rock", "cliamp", "my"]);
+        assert_eq!(group(&view, "my").label, "MY");
+        assert!(group(&view, "my").stations.is_empty());
+        assert!(!group(&view, "rock").stations[0].in_my);
+    }
+
+    #[tokio::test]
+    async fn my_group_lists_refs_and_rb_stations_in_insertion_order() {
+        let dir = tempfile::tempdir().unwrap();
+        let mut manager = manager_in(dir.path());
+        let rb = rb_station(UUID_A, "https://jazz.example/stream");
+        manager.my_add(MyEntry::Ref { station_id: "lofi".to_string() }).await.unwrap();
+        manager.my_add(stored(&rb)).await.unwrap();
+        manager.my_add(MyEntry::Ref { station_id: "big100".to_string() }).await.unwrap();
+
+        let view = manager.registry_view();
+        let my_ids: Vec<&str> = group(&view, "my").stations.iter().map(|s| s.id.as_str()).collect();
+        assert_eq!(my_ids, vec!["lofi", &format!("rb-{UUID_A}")[..], "big100"]);
+        assert!(group(&view, "my").stations.iter().all(|s| s.in_my));
+        assert!(group(&view, "rock").stations[0].in_my, "big100 is flagged in its own band");
+        assert!(group(&view, "cliamp").stations[0].in_my);
+
+        let json = serde_json::to_string(&view).unwrap();
+        assert!(!json.contains("jazz.example"), "stream URLs never reach the API: {json}");
+    }
+
+    #[tokio::test]
+    async fn a_dangling_reference_is_hidden_and_reappears_with_its_station() {
+        let dir = tempfile::tempdir().unwrap();
+        let mut manager = manager_in(dir.path());
+        manager.my_add(MyEntry::Ref { station_id: "gone".to_string() }).await.unwrap();
+        assert!(group(&manager.registry_view(), "my").stations.is_empty());
+
+        manager.cliamp_stations.push(curated("gone", "http://cliamp/gone"));
+        manager.rebuild_url_map();
+        let view = manager.registry_view();
+        assert_eq!(group(&view, "my").stations[0].id, "gone");
+    }
+
+    #[tokio::test]
+    async fn urls_resolve_for_curated_and_saved_stations_but_not_for_search_results() {
+        let dir = tempfile::tempdir().unwrap();
+        let mut manager = manager_in(dir.path());
+        let saved = rb_station(UUID_A, "https://jazz.example/stream");
+        let other = rb_station("22222222-2222-2222-2222-222222222222", "https://other.example/stream");
+        manager.my_add(stored(&saved)).await.unwrap();
+        manager.remember_search_results(std::slice::from_ref(&other));
+
+        assert_eq!(manager.get_station_url("big100"), Some("http://rock/big100"));
+        assert_eq!(manager.get_station_url(&saved.id()), Some("https://jazz.example/stream"));
+        assert_eq!(manager.get_station_url(&other.id()), None);
+        assert_eq!(manager.cached_search_result(&other.id()), Some(other));
+    }
+
+    #[tokio::test]
+    async fn curated_ids_are_recognised() {
+        let dir = tempfile::tempdir().unwrap();
+        let manager = manager_in(dir.path());
+        assert!(manager.is_curated("big100"));
+        assert!(manager.is_curated("lofi"));
+        assert!(!manager.is_curated(&format!("rb-{UUID_A}")));
+    }
 }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --lib stations`

Expected: the lib test target does not compile, with about 32 errors such as ``error[E0433]: cannot find type `MyStore` in this scope``, ``error[E0560]: struct `stations::StationManager` has no field named `my` `` and ``error[E0599]: no method named `cached_search_result` found for struct `stations::StationManager` ``.

- [ ] **Step 3: Implement**

```diff
--- a/src/stations.rs
+++ b/src/stations.rs
@@ -1,3 +1,6 @@
+use crate::my_stations::{MyEntry, MyError, MyStore};
+use crate::radiobrowser::RbStation;
+use crate::search_cache::SearchCache;
 use serde::{Deserialize, Serialize};
 use std::collections::{HashMap, HashSet};
 use std::path::{Path, PathBuf};
@@ -27,6 +30,41 @@
     pub groups: Vec<StationGroup>,
 }
 
+/// A station as `GET /api/stations` shows it: no URL, plus whether it is in MY.
+#[derive(Debug, Clone, Serialize, PartialEq)]
+pub struct StationView {
+    pub id: String,
+    pub name: String,
+    pub short: String,
+    pub genre: String,
+    pub in_my: bool,
+}
+
+impl StationView {
+    fn from_station(station: &Station, in_my: bool) -> Self {
+        Self {
+            id: station.id.clone(),
+            name: station.name.clone(),
+            short: station.short.clone(),
+            genre: station.genre.clone(),
+            in_my,
+        }
+    }
+}
+
+#[derive(Debug, Clone, Serialize)]
+pub struct GroupView {
+    pub id: String,
+    pub label: String,
+    pub stations: Vec<StationView>,
+}
+
+/// The `Stations` API shape: the ROCK and CLIAMP groups plus the `my` group.
+#[derive(Debug, Clone, Serialize)]
+pub struct RegistryView {
+    pub groups: Vec<GroupView>,
+}
+
 #[derive(Debug, Deserialize)]
 struct RockStationsFile {
     station: Vec<Station>,
@@ -53,6 +91,10 @@
     station_urls: HashMap<String, String>,
     cache_path: PathBuf,
     remote_url: String,
+    /// The household's MY list, `<cache_dir>/my-stations.json`.
+    my: MyStore,
+    /// Recent search results, so they can be played or kept by id.
+    search_cache: SearchCache,
 }
 
 impl StationManager {
@@ -68,6 +110,7 @@
         tokio::fs::create_dir_all(cache_dir).await.ok();
 
         let cache_path = cache_dir.join("cliamp-stations.json");
+        let my = MyStore::load(cache_dir.join("my-stations.json"));
 
         // Load cliamp stations
         let cliamp_stations = Self::fetch_cliamp_stations(&remote_url, &cache_path).await;
@@ -78,6 +121,8 @@
             station_urls: HashMap::new(),
             cache_path,
             remote_url,
+            my,
+            search_cache: SearchCache::new(),
         };
 
         manager.rebuild_url_map();
@@ -188,8 +233,14 @@
         }
     }
 
+    /// The stream URL for a station id: ROCK/CLIAMP from the registry, then a
+    /// Radio Browser station saved in MY. Unsaved search results are resolved
+    /// by the API layer, never here.
     pub fn get_station_url(&self, id: &str) -> Option<&str> {
-        self.station_urls.get(id).map(|s| s.as_str())
+        if let Some(url) = self.station_urls.get(id) {
+            return Some(url.as_str());
+        }
+        self.my.get_rb(id).map(|station| station.url.as_str())
     }
 
     pub fn get_station_name(&self, id: &str) -> Option<String> {
@@ -200,6 +251,86 @@
             .map(|s| s.name.clone())
     }
 
+    /// True for a ROCK or CLIAMP station id.
+    pub fn is_curated(&self, id: &str) -> bool {
+        self.station_urls.contains_key(id)
+    }
+
+    /// True when `id` is in MY.
+    pub fn is_in_my(&self, id: &str) -> bool {
+        self.my.contains(id)
+    }
+
+    /// Adds an entry to MY; `Ok(false)` when it was already there.
+    pub async fn my_add(&mut self, entry: MyEntry) -> Result<bool, MyError> {
+        self.my.add(entry).await
+    }
+
+    /// Removes an entry from MY; `Ok(false)` when it was not there.
+    pub async fn my_remove(&mut self, id: &str) -> Result<bool, MyError> {
+        self.my.remove(id).await
+    }
+
+    /// Remembers search results so they can be played or kept by id.
+    pub fn remember_search_results(&mut self, stations: &[RbStation]) {
+        self.search_cache.insert_all(stations, time::Instant::now());
+    }
+
+    /// A recent search result by id, if it has not expired.
+    pub fn cached_search_result(&self, id: &str) -> Option<RbStation> {
+        self.search_cache.get(id, time::Instant::now())
+    }
+
+    /// The registry as the API shows it: ROCK and CLIAMP flagged with `in_my`,
+    /// plus the `my` group in insertion order. A reference whose station is
+    /// gone is left out (but stays in the file).
+    pub fn registry_view(&self) -> RegistryView {
+        let registry = self.get_registry();
+
+        let mut my_stations: Vec<StationView> = Vec::new();
+        for entry in self.my.entries() {
+            match entry {
+                MyEntry::Ref { station_id } => {
+                    let found = registry
+                        .groups
+                        .iter()
+                        .flat_map(|group| &group.stations)
+                        .find(|station| &station.id == station_id);
+                    if let Some(station) = found {
+                        my_stations.push(StationView::from_station(station, true));
+                    }
+                }
+                MyEntry::Rb(stored) => my_stations.push(StationView {
+                    id: stored.id.clone(),
+                    name: stored.name.clone(),
+                    short: stored.short.clone(),
+                    genre: stored.genre.clone(),
+                    in_my: true,
+                }),
+            }
+        }
+
+        let mut groups: Vec<GroupView> = registry
+            .groups
+            .iter()
+            .map(|group| GroupView {
+                id: group.id.clone(),
+                label: group.label.clone(),
+                stations: group
+                    .stations
+                    .iter()
+                    .map(|station| StationView::from_station(station, self.my.contains(&station.id)))
+                    .collect(),
+            })
+            .collect();
+        groups.push(GroupView {
+            id: "my".to_string(),
+            label: "MY".to_string(),
+            stations: my_stations,
+        });
+        RegistryView { groups }
+    }
+
     /// Start background refresh task
     pub fn start_refresh_task(manager: std::sync::Arc<tokio::sync::RwLock<Self>>) {
         tokio::spawn(async move {
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --lib stations && cargo clippy --all-targets -- -D warnings`

Expected: all `stations::tests` pass (the 5 new ones plus the existing ones) and no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/stations.rs
git commit -m "$(cat <<'EOF'
stations: own the MY list and search cache, expose a URL-free registry view

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Name the station that is playing, and map a playing URL back to its id

**Files:**
- Modify: `src/stations.rs` (new field `playing_rb`, `get_station_name`, new `station_id_for_url` and `note_playing`, test fixtures), `src/state.rs` (`build_player_info`, around line 393)
- Test: unit tests in `src/stations.rs`

**Interfaces:**
- Consumes: Task 5's `StationManager`, `StoredRbStation` (Task 3).
- Produces:
  - `StationManager::get_station_name(&self, id: &str) -> Option<String>`: order is ROCK/CLIAMP, then MY, then the station playing now, then the search cache
  - `pub fn station_id_for_url(&self, url: &str) -> Option<String>`: ROCK before CLIAMP, then MY, then the Radio Browser station playing now
  - `pub fn note_playing(&mut self, id: &str, discovered: Option<&RbStation>)`: records the Radio Browser station that just started (`discovered`), or the MY entry with this `id`, or nothing (so a ROCK/CLIAMP station clears the previous one). This keeps the LCD name correct after the cache expires or the station leaves MY.
  - `build_player_info` in `src/state.rs` uses `station_id_for_url`, so `State.player.station_id` and `station_name` are right for `rb-` stations.

- [ ] **Step 1: Write the failing tests**

```diff
--- a/src/stations.rs
+++ b/src/stations.rs
@@ -416,6 +416,7 @@
             remote_url: String::new(),
             my: MyStore::load(PathBuf::from("/nonexistent/my-stations.json")),
             search_cache: SearchCache::new(),
+            playing_rb: None,
         };
 
         manager.rebuild_url_map();
@@ -462,6 +463,7 @@
             remote_url: String::new(),
             my: MyStore::load(dir.join("my-stations.json")),
             search_cache: SearchCache::new(),
+            playing_rb: None,
         };
         manager.rebuild_url_map();
         manager
@@ -532,6 +534,51 @@
     }
 
     #[tokio::test]
+    async fn names_come_from_the_registry_my_the_playing_station_or_the_cache() {
+        let dir = tempfile::tempdir().unwrap();
+        let mut manager = manager_in(dir.path());
+        let saved = rb_station(UUID_A, "https://jazz.example/stream");
+        let playing = rb_station("22222222-2222-2222-2222-222222222222", "https://p.example/s");
+        let cached = rb_station("33333333-3333-3333-3333-333333333333", "https://c.example/s");
+        manager.my_add(stored(&saved)).await.unwrap();
+        manager.note_playing(&playing.id(), Some(&playing));
+        manager.remember_search_results(std::slice::from_ref(&cached));
+
+        assert_eq!(manager.get_station_name("big100").as_deref(), Some("big100 name"));
+        assert_eq!(manager.get_station_name(&saved.id()).as_deref(), Some("Jazz FM"));
+        assert_eq!(manager.get_station_name(&playing.id()).as_deref(), Some("Jazz FM"));
+        assert_eq!(manager.get_station_name(&cached.id()).as_deref(), Some("Jazz FM"));
+        assert_eq!(manager.get_station_name("rb-44444444-4444-4444-4444-444444444444"), None);
+
+        manager.note_playing("big100", None);
+        assert_eq!(manager.get_station_name(&playing.id()), None);
+
+        manager.note_playing(&saved.id(), None);
+        manager.my_remove(&saved.id()).await.unwrap();
+        assert_eq!(
+            manager.get_station_name(&saved.id()).as_deref(),
+            Some("Jazz FM"),
+            "a station removed from MY keeps its name while it plays"
+        );
+    }
+
+    #[tokio::test]
+    async fn a_playing_url_maps_back_to_its_station_id() {
+        let dir = tempfile::tempdir().unwrap();
+        let mut manager = manager_in(dir.path());
+        let saved = rb_station(UUID_A, "https://jazz.example/stream");
+        let playing = rb_station("22222222-2222-2222-2222-222222222222", "https://p.example/s");
+        manager.my_add(stored(&saved)).await.unwrap();
+        manager.note_playing(&playing.id(), Some(&playing));
+
+        assert_eq!(manager.station_id_for_url("http://cliamp/lofi").as_deref(), Some("lofi"));
+        assert_eq!(manager.station_id_for_url("http://rock/big100").as_deref(), Some("big100"));
+        assert_eq!(manager.station_id_for_url("https://jazz.example/stream"), Some(saved.id()));
+        assert_eq!(manager.station_id_for_url("https://p.example/s"), Some(playing.id()));
+        assert_eq!(manager.station_id_for_url("https://nowhere.example/"), None);
+    }
+
+    #[tokio::test]
     async fn curated_ids_are_recognised() {
         let dir = tempfile::tempdir().unwrap();
         let manager = manager_in(dir.path());
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --lib stations`

Expected: compile failure, 11 errors: ``error[E0560]: struct `stations::StationManager` has no field named `playing_rb` ``, ``error[E0599]: no method named `note_playing` found`` and ``no method named `station_id_for_url` found``.

- [ ] **Step 3: Implement**

```diff
--- a/src/state.rs
+++ b/src/state.rs
@@ -393,13 +393,7 @@
         let station_identity = player_state.station_url.as_ref().or(player_state.url.as_ref());
         let station = if let Some(url) = station_identity {
             // Try to match by URL
-            let matched = stations
-                .get_registry()
-                .groups
-                .iter()
-                .flat_map(|g| &g.stations)
-                .find(|s| stations.get_station_url(&s.id) == Some(url))
-                .map(|s| s.id.clone());
+            let matched = stations.station_id_for_url(url);
 
             if matched.is_some() {
                 matched
--- a/src/stations.rs
+++ b/src/stations.rs
@@ -1,4 +1,4 @@
-use crate::my_stations::{MyEntry, MyError, MyStore};
+use crate::my_stations::{MyEntry, MyError, MyStore, StoredRbStation};
 use crate::radiobrowser::RbStation;
 use crate::search_cache::SearchCache;
 use serde::{Deserialize, Serialize};
@@ -95,6 +95,9 @@
     my: MyStore,
     /// Recent search results, so they can be played or kept by id.
     search_cache: SearchCache,
+    /// The Radio Browser station that is playing now, kept so the LCD name
+    /// survives the search cache expiring (or the station leaving MY) mid-listen.
+    playing_rb: Option<StoredRbStation>,
 }
 
 impl StationManager {
@@ -123,6 +126,7 @@
             remote_url,
             my,
             search_cache: SearchCache::new(),
+            playing_rb: None,
         };
 
         manager.rebuild_url_map();
@@ -243,12 +247,41 @@
         self.my.get_rb(id).map(|station| station.url.as_str())
     }
 
+    /// The display name for a station id, from the registry, MY, the station
+    /// playing now, or the search cache.
     pub fn get_station_name(&self, id: &str) -> Option<String> {
-        self.rock_stations
+        let curated = self
+            .rock_stations
             .iter()
             .chain(self.cliamp_stations.iter())
-            .find(|s| s.id == id)
-            .map(|s| s.name.clone())
+            .find(|s| s.id == id);
+        if let Some(station) = curated {
+            return Some(station.name.clone());
+        }
+        if let Some(stored) = self.my.get_rb(id) {
+            return Some(stored.name.clone());
+        }
+        if let Some(playing) = self.playing_rb.as_ref().filter(|rb| rb.id == id) {
+            return Some(playing.name.clone());
+        }
+        self.search_cache.get(id, time::Instant::now()).map(|rb| rb.name)
+    }
+
+    /// The id of the station that streams `url`: curated stations first (ROCK
+    /// before CLIAMP), then MY, then the Radio Browser station playing now.
+    pub fn station_id_for_url(&self, url: &str) -> Option<String> {
+        let curated = self
+            .rock_stations
+            .iter()
+            .chain(self.cliamp_stations.iter())
+            .find(|s| self.station_urls.get(&s.id).map(String::as_str) == Some(url));
+        if let Some(station) = curated {
+            return Some(station.id.clone());
+        }
+        if let Some(stored) = self.my.rb_entries().find(|stored| stored.url == url) {
+            return Some(stored.id.clone());
+        }
+        self.playing_rb.as_ref().filter(|rb| rb.url == url).map(|rb| rb.id.clone())
     }
 
     /// True for a ROCK or CLIAMP station id.
@@ -281,6 +314,16 @@
         self.search_cache.get(id, time::Instant::now())
     }
 
+    /// Records what is playing now: the freshly discovered Radio Browser
+    /// station if there is one, else the MY entry with this id (if any), so
+    /// a ROCK/CLIAMP station clears the previous Radio Browser one.
+    pub fn note_playing(&mut self, id: &str, discovered: Option<&RbStation>) {
+        self.playing_rb = match discovered {
+            Some(station) => Some(StoredRbStation::from_rb(station)),
+            None => self.my.get_rb(id).cloned(),
+        };
+    }
+
     /// The registry as the API shows it: ROCK and CLIAMP flagged with `in_my`,
     /// plus the `my` group in insertion order. A reference whose station is
     /// gone is left out (but stays in the file).
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`

Expected: everything passes, no clippy warnings. The full suite is run because `state.rs` changed.

- [ ] **Step 5: Commit**

```bash
git add src/stations.rs src/state.rs
git commit -m "$(cat <<'EOF'
stations: name the playing Radio Browser station and map playing URLs back to ids

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: AppState carries Radio Browser and the stations broadcast; /api/stations shows MY

**Files:**
- Modify: `src/api.rs` (imports; `AppState` fields; `get_stations`), `src/main.rs` (wiring in the `AppState` literal), `tests/integration_test.rs` (harness: tempdir cache dir, `FakeRadioBrowser`, `discovery_app`, first test)
- Test: `tests/integration_test.rs`

**Interfaces:**
- Consumes: `RadioBrowser`, `RbStation`, `RbError` (Task 1); `RegistryView` (Task 5); `HttpRadioBrowser::connect` (Task 2).
- Produces:
  - `AppState { radio_browser: Arc<dyn RadioBrowser>, stations_tx: broadcast::Sender<String>, .. }`
  - `GET /api/stations` returns `RegistryView` JSON: `{"groups":[{"id":"rock","label":"..","stations":[{"id","name","short","genre","in_my"}]}, .., {"id":"my","label":"MY","stations":[..]}]}`
  - Test harness: `TestApp { _cache_dir: Option<tempfile::TempDir>, radio_browser: Arc<FakeRadioBrowser>, .. }`, `FakeRadioBrowser::new(Vec<RbStation>)`, `fake_stations()` (`JAZZ_UUID` `11111111-1111-1111-1111-111111111111` "Smooth Jazz Radio" `http://jazz.example.com/stream`; `BLUES_UUID` `22222222-2222-2222-2222-222222222222` "Blues Highway" `https://blues.example.com/live`), `build_app_in(dir, yxc, timing, grab_zones)`, `discovery_app()`. Each test now gets a tempdir cache dir, so MY state never leaks between tests.

- [ ] **Step 1: Write the failing test and harness**

Apply this change to `tests/integration_test.rs`. It adds the fake, moves the cache dir to a tempdir (needed so MY files do not leak between tests) and adds the first test. `FakeRadioBrowser::set_down` and `by_uuid_call_count` are not added yet because nothing uses them (clippy rejects dead code); Tasks 8 and 10 add them.

```diff
--- a/tests/integration_test.rs
+++ b/tests/integration_test.rs
@@ -386,6 +386,84 @@
     route: Arc<MockAudioRoute>,
     player: Arc<MockPlayer>,
     events: Events,
+    /// Owns the cache dir (and `my-stations.json`); gone when the test ends.
+    _cache_dir: Option<tempfile::TempDir>,
+}
+
+/// Radio Browser stand-in: serves a fixed list, can be switched off, and
+/// records what it was asked.
+struct FakeRadioBrowser {
+    stations: Vec<radiobrowser::RbStation>,
+    down: std::sync::atomic::AtomicBool,
+    by_uuid_calls: std::sync::atomic::AtomicUsize,
+    searches: std::sync::Mutex<Vec<(Option<String>, Option<String>)>>,
+}
+
+impl FakeRadioBrowser {
+    fn new(stations: Vec<radiobrowser::RbStation>) -> Self {
+        Self {
+            stations,
+            down: std::sync::atomic::AtomicBool::new(false),
+            by_uuid_calls: std::sync::atomic::AtomicUsize::new(0),
+            searches: std::sync::Mutex::new(Vec::new()),
+        }
+    }
+
+    fn is_down(&self) -> bool {
+        self.down.load(std::sync::atomic::Ordering::SeqCst)
+    }
+}
+
+#[async_trait::async_trait]
+impl radiobrowser::RadioBrowser for FakeRadioBrowser {
+    async fn search(
+        &self,
+        name: Option<&str>,
+        tag: Option<&str>,
+    ) -> Result<Vec<radiobrowser::RbStation>, radiobrowser::RbError> {
+        self.searches
+            .lock()
+            .unwrap()
+            .push((name.map(str::to_string), tag.map(str::to_string)));
+        if self.is_down() {
+            return Err(radiobrowser::RbError::Unavailable("fake outage".to_string()));
+        }
+        Ok(self.stations.clone())
+    }
+
+    async fn by_uuid(&self, uuid: &str) -> Result<Option<radiobrowser::RbStation>, radiobrowser::RbError> {
+        self.by_uuid_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
+        if self.is_down() {
+            return Err(radiobrowser::RbError::Unavailable("fake outage".to_string()));
+        }
+        Ok(self.stations.iter().find(|station| station.uuid == uuid).cloned())
+    }
+}
+
+/// UUID of the first fake station.
+const JAZZ_UUID: &str = "11111111-1111-1111-1111-111111111111";
+/// UUID of the second fake station.
+const BLUES_UUID: &str = "22222222-2222-2222-2222-222222222222";
+
+fn fake_stations() -> Vec<radiobrowser::RbStation> {
+    vec![
+        radiobrowser::RbStation {
+            uuid: JAZZ_UUID.to_string(),
+            name: "Smooth Jazz Radio".to_string(),
+            genre: "jazz".to_string(),
+            country: "Canada".to_string(),
+            bitrate: 128,
+            url: "http://jazz.example.com/stream".to_string(),
+        },
+        radiobrowser::RbStation {
+            uuid: BLUES_UUID.to_string(),
+            name: "Blues Highway".to_string(),
+            genre: "blues".to_string(),
+            country: "USA".to_string(),
+            bitrate: 64,
+            url: "https://blues.example.com/live".to_string(),
+        },
+    ]
 }
 
 // Helper to create test app
@@ -405,6 +483,20 @@
     policy_timing: api::PolicyTiming,
     grab_zones: Option<Arc<Mutex<HashMap<String, yxc::ZoneStatus>>>>,
 ) -> TestApp {
+    let cache_dir = tempfile::tempdir().unwrap();
+    let mut app = build_app_in(cache_dir.path(), yxc, policy_timing, grab_zones).await;
+    app._cache_dir = Some(cache_dir);
+    app
+}
+
+/// Like `build_app`, but on a caller-owned cache dir, so a test can start a
+/// second app on the same `my-stations.json` (a restart).
+async fn build_app_in(
+    test_dir: &std::path::Path,
+    yxc: Arc<dyn yxc::YxcClient>,
+    policy_timing: api::PolicyTiming,
+    grab_zones: Option<Arc<Mutex<HashMap<String, yxc::ZoneStatus>>>>,
+) -> TestApp {
     let config = config::Config::with_receiver_url("http://192.0.2.10");
     let events: Events = Arc::default();
     let player_mock = Arc::new(MockPlayer::new(events.clone()));
@@ -414,14 +506,7 @@
         ..MockAudioRoute::new(events.clone())
     });
 
-    // Create a minimal stations manager with unique temp directory
-    use std::sync::atomic::{AtomicU64, Ordering};
-    static COUNTER: AtomicU64 = AtomicU64::new(0);
-    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
-
-    let test_dir = std::env::temp_dir().join(format!("radio_test_{}", id));
-    std::fs::create_dir_all(&test_dir).unwrap();
-
+    // Create a minimal stations manager in the given cache dir
     let stations_file = test_dir.join("stations.toml");
     std::fs::write(
         &stations_file,
@@ -430,7 +515,7 @@
     .unwrap();
 
     let stations = Arc::new(RwLock::new(
-        stations::StationManager::new(&stations_file, &test_dir, "http://localhost".to_string())
+        stations::StationManager::new(&stations_file, test_dir, "http://localhost".to_string())
             .await
             .unwrap(),
     ));
@@ -462,6 +547,8 @@
             player_mock.clone() as Arc<dyn cliamp::Player>,
             vis::VisConfig::default(),
         ),
+        radio_browser: Arc::new(FakeRadioBrowser::new(fake_stations())),
+        stations_tx: tokio::sync::broadcast::channel(16).0,
     };
 
     TestApp {
@@ -470,6 +557,7 @@
         route: route_mock,
         player: player_mock,
         events,
+        _cache_dir: None,
     }
 }
 
@@ -2229,3 +2317,26 @@
     assert_eq!(count_calls(&calls, "set_power main false"), 1);
     assert_eq!(count_events(&app.events, "player.stop"), 1);
 }
+
+// ---- Station discovery: search ----
+
+async fn discovery_app() -> TestApp {
+    build_app(Arc::new(MockYxcClient::new()), fast_timing(), None).await
+}
+
+#[tokio::test]
+async fn stations_view_has_an_empty_my_group_and_in_my_flags() {
+    let app = discovery_app().await;
+
+    let response = app.server.get("/api/stations").await;
+    response.assert_status_ok();
+
+    let body: serde_json::Value = response.json();
+    let groups = body["groups"].as_array().unwrap();
+    let my = groups.iter().find(|group| group["id"] == "my").unwrap();
+    assert_eq!(my["label"], "MY");
+    assert_eq!(my["stations"].as_array().unwrap().len(), 0);
+    let first_curated = &groups[0]["stations"][0];
+    assert_eq!(first_curated["id"], "test");
+    assert_eq!(first_curated["in_my"], false);
+}
```

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test --test integration_test stations_view`

Expected: the integration test target does not compile: ``error[E0560]: struct `AppState` has no field named `radio_browser` `` and the same for `stations_tx` (2 errors).

- [ ] **Step 3: Implement**

```diff
--- a/src/api.rs
+++ b/src/api.rs
@@ -3,9 +3,10 @@
 use crate::cliamp::Player;
 use crate::config::Config;
 use crate::policy::{self, ZoneSelection, ZoneSnapshot};
+use crate::radiobrowser::RadioBrowser;
 use crate::route::AudioRoute;
 use crate::state::{State, StateManager, ZoneLive, ZoneOverride};
-use crate::stations::StationManager;
+use crate::stations::{RegistryView, StationManager};
 use crate::vis::VisHub;
 use crate::volume;
 use crate::yxc::{YxcClient, YxcError};
@@ -25,7 +26,7 @@
 use std::sync::atomic::{AtomicU64, Ordering};
 use std::sync::Arc;
 use std::sync::Mutex as StdMutex;
-use tokio::sync::{Mutex, RwLock};
+use tokio::sync::{broadcast, Mutex, RwLock};
 use tokio::time::{Duration, Instant};
 use tokio_stream::wrappers::{ReceiverStream, WatchStream};
 use tokio_stream::{Stream, StreamExt};
@@ -112,6 +113,10 @@
     pub policy_completions: Arc<AtomicU64>,
     /// Shared spectrum feed behind `GET /api/vis`.
     pub vis: Arc<VisHub>,
+    /// Radio Browser, behind a trait so tests can substitute a fake.
+    pub radio_browser: Arc<dyn RadioBrowser>,
+    /// Carries the `Stations` JSON to every SSE client after MY changes.
+    pub stations_tx: broadcast::Sender<String>,
 }
 
 #[derive(Debug, Serialize)]
@@ -182,11 +187,9 @@
     Json(state.state_manager.get_state().await)
 }
 
-async fn get_stations(
-    AxumState(state): AxumState<AppState>,
-) -> Json<crate::stations::StationRegistry> {
+async fn get_stations(AxumState(state): AxumState<AppState>) -> Json<RegistryView> {
     let stations = state.stations.read().await;
-    Json(stations.get_registry())
+    Json(stations.registry_view())
 }
 
 async fn play_station(
--- a/src/main.rs
+++ b/src/main.rs
@@ -108,6 +108,8 @@
         route_tracking: Arc::default(),
         policy_completions: Arc::default(),
         vis,
+        radio_browser: Arc::new(radiobrowser::HttpRadioBrowser::connect().await),
+        stations_tx: tokio::sync::broadcast::channel(16).0,
     };
 
     // Watch for a receiver that stops pulling audio while cliamp plays
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`

Expected: everything passes, including `stations_view_has_an_empty_my_group_and_in_my_flags`, and no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/api.rs src/main.rs tests/integration_test.rs
git commit -m "$(cat <<'EOF'
api: AppState carries Radio Browser and the stations broadcast; /api/stations adds the MY group

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: GET /api/search

**Files:**
- Modify: `src/api.rs` (imports; `router` gets one route; new search handler and types before `play_station`), `tests/integration_test.rs` (`FakeRadioBrowser::set_down` and four tests)
- Test: `tests/integration_test.rs`

**Interfaces:**
- Consumes: `RadioBrowser::search`, `genre_tag` (Task 1); `StationManager::remember_search_results`, `is_in_my` (Task 5); `AppState.radio_browser` (Task 7).
- Produces:
  - `GET /api/search?q=<text>&genre=<chip>`. The name part is `q` trimmed (used only when it has at least 2 characters, cut to 80). `genre` must be one of the chip names or the request is 400 `bad_query`. With neither a usable `q` nor a `genre`: 400 `bad_query`. Radio Browser failure: 503 `search_unavailable`. Success:
    `{"results":[{"id":"rb-<uuid>","name":"..","genre":"..","country":"..","bitrate":128,"in_my":false}]}` (no URL; at most 30 results). Every result is remembered in the search cache.
  - Error body shape is the existing `error_response`: `{"error":"<code>","detail":"<text>"}`.
  - `FakeRadioBrowser::set_down(&self, down: bool)` in the test harness.

- [ ] **Step 1: Write the failing tests**

```diff
--- a/tests/integration_test.rs
+++ b/tests/integration_test.rs
@@ -386,6 +386,7 @@
     route: Arc<MockAudioRoute>,
     player: Arc<MockPlayer>,
     events: Events,
+    radio_browser: Arc<FakeRadioBrowser>,
     /// Owns the cache dir (and `my-stations.json`); gone when the test ends.
     _cache_dir: Option<tempfile::TempDir>,
 }
@@ -409,6 +410,10 @@
         }
     }
 
+    fn set_down(&self, down: bool) {
+        self.down.store(down, std::sync::atomic::Ordering::SeqCst);
+    }
+
     fn is_down(&self) -> bool {
         self.down.load(std::sync::atomic::Ordering::SeqCst)
     }
@@ -530,6 +535,7 @@
     // Manually trigger a refresh so tests don't have to wait
     state_manager.refresh().await;
 
+    let radio_browser = Arc::new(FakeRadioBrowser::new(fake_stations()));
     let app_state = api::AppState {
         yxc,
         player,
@@ -547,7 +553,7 @@
             player_mock.clone() as Arc<dyn cliamp::Player>,
             vis::VisConfig::default(),
         ),
-        radio_browser: Arc::new(FakeRadioBrowser::new(fake_stations())),
+        radio_browser: radio_browser.clone(),
         stations_tx: tokio::sync::broadcast::channel(16).0,
     };
 
@@ -557,6 +563,7 @@
         route: route_mock,
         player: player_mock,
         events,
+        radio_browser,
         _cache_dir: None,
     }
 }
@@ -2325,6 +2332,87 @@
 }
 
 #[tokio::test]
+async fn search_by_name_returns_ids_only_results() {
+    let app = discovery_app().await;
+
+    let response = app.server.get("/api/search").add_query_param("q", "jazz").await;
+    response.assert_status_ok();
+
+    let body: serde_json::Value = response.json();
+    let results = body["results"].as_array().unwrap();
+    assert_eq!(results.len(), 2);
+    assert_eq!(results[0]["id"], format!("rb-{JAZZ_UUID}"));
+    assert_eq!(results[0]["name"], "Smooth Jazz Radio");
+    assert_eq!(results[0]["genre"], "jazz");
+    assert_eq!(results[0]["country"], "Canada");
+    assert_eq!(results[0]["bitrate"], 128);
+    assert_eq!(results[0]["in_my"], false);
+    // The stream URL never leaves the server.
+    assert!(!body.to_string().contains("example.com"));
+}
+
+#[tokio::test]
+async fn search_maps_the_genre_chip_to_a_radio_browser_tag() {
+    let app = discovery_app().await;
+
+    app.server
+        .get("/api/search")
+        .add_query_param("genre", "Alt")
+        .await
+        .assert_status_ok();
+    app.server
+        .get("/api/search")
+        .add_query_param("q", "  radio  ")
+        .add_query_param("genre", "Classic Rock")
+        .await
+        .assert_status_ok();
+
+    let searches = app.radio_browser.searches.lock().unwrap().clone();
+    assert_eq!(
+        searches,
+        vec![
+            (None, Some("alternative".to_string())),
+            (Some("radio".to_string()), Some("classic rock".to_string())),
+        ]
+    );
+}
+
+#[tokio::test]
+async fn search_rejects_bad_queries() {
+    let app = discovery_app().await;
+
+    for query in [
+        vec![],
+        vec![("q", "a")],
+        vec![("q", "   ")],
+        vec![("genre", "Polka")],
+        vec![("q", "jazz"), ("genre", "Polka")],
+    ] {
+        let mut request = app.server.get("/api/search");
+        for (key, value) in query {
+            request = request.add_query_param(key, value);
+        }
+        let response = request.await;
+        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
+        let body: serde_json::Value = response.json();
+        assert_eq!(body["error"], "bad_query");
+    }
+    assert!(app.radio_browser.searches.lock().unwrap().is_empty());
+}
+
+#[tokio::test]
+async fn search_reports_unavailable_when_radio_browser_is_down() {
+    let app = discovery_app().await;
+    app.radio_browser.set_down(true);
+
+    let response = app.server.get("/api/search").add_query_param("q", "jazz").await;
+
+    response.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
+    let body: serde_json::Value = response.json();
+    assert_eq!(body["error"], "search_unavailable");
+}
+
+#[tokio::test]
 async fn stations_view_has_an_empty_my_group_and_in_my_flags() {
     let app = discovery_app().await;
 
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --test integration_test search_`

Expected: 4 tests FAILED (`search_by_name_returns_ids_only_results`, `search_maps_the_genre_chip_to_a_radio_browser_tag`, `search_rejects_bad_queries`, `search_reports_unavailable_when_radio_browser_is_down`), each with ``Expected status code to be 200 (OK), received 404 (Not Found), for request GET http://localhost/api/search...`` (or the 400/503 the test expects).

- [ ] **Step 3: Implement**

```diff
--- a/src/api.rs
+++ b/src/api.rs
@@ -3,7 +3,7 @@
 use crate::cliamp::Player;
 use crate::config::Config;
 use crate::policy::{self, ZoneSelection, ZoneSnapshot};
-use crate::radiobrowser::RadioBrowser;
+use crate::radiobrowser::{self, RadioBrowser};
 use crate::route::AudioRoute;
 use crate::state::{State, StateManager, ZoneLive, ZoneOverride};
 use crate::stations::{RegistryView, StationManager};
@@ -11,7 +11,7 @@
 use crate::volume;
 use crate::yxc::{YxcClient, YxcError};
 use axum::{
-    extract::{Path, State as AxumState},
+    extract::{Path, Query, State as AxumState},
     http::StatusCode,
     response::{
         sse::{Event, KeepAlive},
@@ -171,6 +171,7 @@
     Router::new()
         .route("/api/state", get(get_state))
         .route("/api/stations", get(get_stations))
+        .route("/api/search", get(search_stations))
         .route("/api/play", post(play_station))
         .route("/api/stop", post(stop_player))
         .route("/api/power", post(set_master_power))
@@ -192,6 +193,92 @@
     Json(stations.registry_view())
 }
 
+/// Shortest `q` that searches by name.
+const SEARCH_MIN_QUERY_CHARS: usize = 2;
+/// `q` is cut to this many characters before it is sent on.
+const SEARCH_MAX_QUERY_CHARS: usize = 80;
+
+#[derive(Debug, Deserialize)]
+struct SearchQuery {
+    q: Option<String>,
+    genre: Option<String>,
+}
+
+#[derive(Debug, Serialize)]
+struct SearchResult {
+    id: String,
+    name: String,
+    genre: String,
+    country: String,
+    bitrate: u32,
+    in_my: bool,
+}
+
+#[derive(Debug, Serialize)]
+struct SearchResponse {
+    results: Vec<SearchResult>,
+}
+
+fn bad_query(detail: &str) -> Response {
+    error_response(StatusCode::BAD_REQUEST, "bad_query", detail)
+}
+
+fn search_unavailable() -> Response {
+    error_response(
+        StatusCode::SERVICE_UNAVAILABLE,
+        "search_unavailable",
+        "Station search is unavailable right now",
+    )
+}
+
+/// `GET /api/search?q=…&genre=…`: Radio Browser stations by name and/or genre
+/// chip. The results are remembered briefly so they can be played or kept by id.
+async fn search_stations(
+    AxumState(state): AxumState<AppState>,
+    Query(query): Query<SearchQuery>,
+) -> Result<Json<SearchResponse>, Response> {
+    let name: Option<String> = query
+        .q
+        .as_deref()
+        .map(str::trim)
+        .filter(|text| text.chars().count() >= SEARCH_MIN_QUERY_CHARS)
+        .map(|text| text.chars().take(SEARCH_MAX_QUERY_CHARS).collect());
+    let tag = match query.genre.as_deref().filter(|label| !label.is_empty()) {
+        Some(label) => Some(
+            radiobrowser::genre_tag(label)
+                .ok_or_else(|| bad_query("genre must be one of the genre chips"))?,
+        ),
+        None => None,
+    };
+    if name.is_none() && tag.is_none() {
+        return Err(bad_query("search needs at least 2 characters or a genre"));
+    }
+
+    let found = state
+        .radio_browser
+        .search(name.as_deref(), tag.as_deref())
+        .await
+        .map_err(|e| {
+            warn!("Radio Browser search failed: {}", e);
+            search_unavailable()
+        })?;
+
+    let mut stations = state.stations.write().await;
+    stations.remember_search_results(&found);
+    let results = found
+        .iter()
+        .map(|station| SearchResult {
+            id: station.id(),
+            name: station.name.clone(),
+            genre: station.genre.clone(),
+            country: station.country.clone(),
+            bitrate: station.bitrate,
+            in_my: stations.is_in_my(&station.id()),
+        })
+        .collect();
+    Ok(Json(SearchResponse { results }))
+}
+
 async fn play_station(
     AxumState(state): AxumState<AppState>,
     Json(req): Json<PlayRequest>,
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`

Expected: everything passes (the 4 new search tests included) and no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/api.rs tests/integration_test.rs
git commit -m "$(cat <<'EOF'
api: GET /api/search over Radio Browser with ids-only results

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: POST /api/my, DELETE /api/my/{id} and the `stations` SSE event

**Files:**
- Modify: `src/api.rs` (imports; two routes; `ResolvedStation` and `resolve_station`; the MY handlers; the SSE handler `state_events`), `tests/integration_test.rs` (six tests)
- Test: `tests/integration_test.rs`

**Interfaces:**
- Consumes: `StationManager::my_add`, `my_remove`, `registry_view`, `cached_search_result`, `remember_search_results`, `is_in_my` (Task 5); `StoredRbStation::from_rb`, `MyEntry`, `MyError`, `MY_CAP` (Task 3); `rb_uuid`, `RadioBrowser::by_uuid` (Task 1); `AppState.stations_tx` (Task 7); `search_unavailable` (Task 8).
- Produces:
  - `POST /api/my` body `{"station":"<id>"}`. A ROCK/CLIAMP/MY id keeps a `{"ref":..}` entry; an `rb-<uuid>` id is resolved server-side (MY, then the search cache, then `by_uuid`) and stored with its URL. Unknown id: 400 `unknown_station`; an `rb-` id that is not a lowercase hyphenated uuid: 400 `bad_station`; MY full: 409 `my_full`; save failure: 500 `my_save_failed`; Radio Browser down while resolving: 503 `search_unavailable`. Response: the `Stations` JSON (`RegistryView`). A duplicate add is a no-op and publishes nothing.
  - `DELETE /api/my/{id}`: removes if present, no-op otherwise; response `Stations`.
  - SSE `GET /api/events` gains `event: stations` whose data is the full `Stations` JSON, sent after every change that actually modified MY (`publish_stations`).
  - `enum ResolvedStation { Known, Discovered(RbStation) }` and `async fn resolve_station(state: &AppState, id: &str) -> Result<ResolvedStation, Response>` (private to `api.rs`).

- [ ] **Step 1: Write the failing tests**

```diff
--- a/tests/integration_test.rs
+++ b/tests/integration_test.rs
@@ -2428,3 +2428,136 @@
     assert_eq!(first_curated["id"], "test");
     assert_eq!(first_curated["in_my"], false);
 }
+
+// ---- Station discovery: the MY list ----
+
+/// The ids in the `my` group of a `Stations` body, in order.
+fn my_ids(stations_body: &serde_json::Value) -> Vec<String> {
+    let groups = stations_body["groups"].as_array().unwrap();
+    let my = groups.iter().find(|group| group["id"] == "my").unwrap();
+    my["stations"]
+        .as_array()
+        .unwrap()
+        .iter()
+        .map(|station| station["id"].as_str().unwrap().to_string())
+        .collect()
+}
+
+#[tokio::test]
+async fn my_add_and_remove_a_curated_station() {
+    let app = discovery_app().await;
+
+    let added = app.server.post("/api/my").json(&json!({"station": "test"})).await;
+    added.assert_status_ok();
+    let added_body: serde_json::Value = added.json();
+    assert_eq!(my_ids(&added_body), vec!["test"]);
+    assert_eq!(added_body["groups"][0]["stations"][0]["in_my"], true);
+
+    // Adding again is a no-op.
+    let again: serde_json::Value = app.server.post("/api/my").json(&json!({"station": "test"})).await.json();
+    assert_eq!(my_ids(&again), vec!["test"]);
+
+    let removed = app.server.delete("/api/my/test").await;
+    removed.assert_status_ok();
+    assert!(my_ids(&removed.json::<serde_json::Value>()).is_empty());
+
+    // Removing again is a no-op, not an error.
+    app.server.delete("/api/my/test").await.assert_status_ok();
+}
+
+#[tokio::test]
+async fn my_add_a_search_result_by_id() {
+    let app = discovery_app().await;
+    app.server.get("/api/search").add_query_param("q", "jazz").await.assert_status_ok();
+    let jazz_id = format!("rb-{JAZZ_UUID}");
+
+    let added = app.server.post("/api/my").json(&json!({"station": jazz_id})).await;
+    added.assert_status_ok();
+    let body: serde_json::Value = added.json();
+
+    assert_eq!(my_ids(&body), vec![jazz_id.clone()]);
+    let saved = &body["groups"].as_array().unwrap().iter().find(|group| group["id"] == "my").unwrap()["stations"][0];
+    assert_eq!(saved["name"], "Smooth Jazz Radio");
+    assert_eq!(saved["in_my"], true);
+    assert!(!body.to_string().contains("example.com"));
+
+    let search: serde_json::Value = app.server.get("/api/search").add_query_param("q", "jazz").await.json();
+    assert_eq!(search["results"][0]["in_my"], true);
+    assert_eq!(search["results"][1]["in_my"], false);
+}
+
+#[tokio::test]
+async fn my_add_rejects_unknown_and_malformed_ids() {
+    let app = discovery_app().await;
+
+    let unknown = app.server.post("/api/my").json(&json!({"station": "nonexistent"})).await;
+    unknown.assert_status(axum::http::StatusCode::BAD_REQUEST);
+    assert_eq!(unknown.json::<serde_json::Value>()["error"], "unknown_station");
+
+    let malformed = app.server.post("/api/my").json(&json!({"station": "rb-not-a-uuid"})).await;
+    malformed.assert_status(axum::http::StatusCode::BAD_REQUEST);
+    assert_eq!(malformed.json::<serde_json::Value>()["error"], "bad_station");
+}
+
+#[tokio::test]
+async fn my_is_capped_at_fifty_stations() {
+    let app = discovery_app().await;
+    let mut stations = app.state.stations.write().await;
+    for number in 0..my_stations::MY_CAP {
+        let uuid = format!("00000000-0000-0000-0000-{number:012}");
+        let entry = my_stations::MyEntry::Rb(my_stations::StoredRbStation::from_rb(&radiobrowser::RbStation {
+            uuid,
+            name: format!("Filler {number}"),
+            genre: String::new(),
+            country: String::new(),
+            bitrate: 0,
+            url: "http://filler.example.com/stream".to_string(),
+        }));
+        stations.my_add(entry).await.unwrap();
+    }
+    drop(stations);
+
+    let response = app.server.post("/api/my").json(&json!({"station": "test"})).await;
+
+    response.assert_status(axum::http::StatusCode::CONFLICT);
+    assert_eq!(response.json::<serde_json::Value>()["error"], "my_full");
+}
+
+#[tokio::test]
+async fn my_survives_an_app_restart() {
+    let cache_dir = tempfile::tempdir().unwrap();
+    let first = build_app_in(cache_dir.path(), Arc::new(MockYxcClient::new()), fast_timing(), None).await;
+    first.server.get("/api/search").add_query_param("q", "jazz").await.assert_status_ok();
+    first.server.post("/api/my").json(&json!({"station": format!("rb-{JAZZ_UUID}")})).await.assert_status_ok();
+    first.server.post("/api/my").json(&json!({"station": "test"})).await.assert_status_ok();
+    drop(first);
+
+    let second = build_app_in(cache_dir.path(), Arc::new(MockYxcClient::new()), fast_timing(), None).await;
+    let body: serde_json::Value = second.server.get("/api/stations").await.json();
+
+    assert_eq!(my_ids(&body), vec![format!("rb-{JAZZ_UUID}"), "test".to_string()]);
+}
+
+#[tokio::test]
+async fn my_changes_are_pushed_as_a_stations_event() {
+    let app = discovery_app().await;
+    let state = app.state.clone();
+    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
+    let address = listener.local_addr().unwrap();
+    tokio::spawn(async move {
+        axum::serve(listener, api::create_router(state)).await.unwrap();
+    });
+    let mut events = reqwest::get(format!("http://{address}/api/events")).await.unwrap();
+    read_sse_until(&mut events, "event: state").await;
+
+    reqwest::Client::new()
+        .post(format!("http://{address}/api/my"))
+        .json(&json!({"station": "test"}))
+        .send()
+        .await
+        .unwrap();
+
+    let received = read_sse_until(&mut events, "event: stations").await;
+    assert!(received.contains(r#""id":"my""#), "{received:?}");
+    assert!(received.contains(r#""in_my":true"#), "{received:?}");
+}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --test integration_test my_`

Expected: 6 tests FAILED (`my_add_and_remove_a_curated_station`, `my_add_a_search_result_by_id`, `my_add_rejects_unknown_and_malformed_ids`, `my_is_capped_at_fifty_stations`, `my_survives_an_app_restart`, `my_changes_are_pushed_as_a_stations_event`), most with ``Expected status code to be 200 (OK), received 404 (Not Found), for request POST http://localhost/api/my``.

- [ ] **Step 3: Implement**

`ResolvedStation::Known` is a unit variant for now; Task 10 gives it the URL once `play_station` needs it (an unread field would fail clippy here).

```diff
--- a/src/api.rs
+++ b/src/api.rs
@@ -3,7 +3,8 @@
 use crate::cliamp::Player;
 use crate::config::Config;
 use crate::policy::{self, ZoneSelection, ZoneSnapshot};
-use crate::radiobrowser::{self, RadioBrowser};
+use crate::my_stations::{MyEntry, MyError, StoredRbStation, MY_CAP};
+use crate::radiobrowser::{self, RadioBrowser, RbStation};
 use crate::route::AudioRoute;
 use crate::state::{State, StateManager, ZoneLive, ZoneOverride};
 use crate::stations::{RegistryView, StationManager};
@@ -17,7 +18,7 @@
         sse::{Event, KeepAlive},
         IntoResponse, Response, Sse,
     },
-    routing::{get, post},
+    routing::{delete, get, post},
     Json, Router,
 };
 use serde::{Deserialize, Serialize};
@@ -28,7 +29,7 @@
 use std::sync::Mutex as StdMutex;
 use tokio::sync::{broadcast, Mutex, RwLock};
 use tokio::time::{Duration, Instant};
-use tokio_stream::wrappers::{ReceiverStream, WatchStream};
+use tokio_stream::wrappers::{BroadcastStream, ReceiverStream, WatchStream};
 use tokio_stream::{Stream, StreamExt};
 use tracing::{error, warn};
 
@@ -172,6 +173,8 @@
         .route("/api/state", get(get_state))
         .route("/api/stations", get(get_stations))
         .route("/api/search", get(search_stations))
+        .route("/api/my", post(add_my_station))
+        .route("/api/my/{id}", delete(remove_my_station))
         .route("/api/play", post(play_station))
         .route("/api/stop", post(stop_player))
         .route("/api/power", post(set_master_power))
@@ -231,6 +234,120 @@
     )
 }
 
+/// A station id after the server has looked up where to find its stream.
+enum ResolvedStation {
+    /// ROCK, CLIAMP or a saved MY station: the URL is already known.
+    Known,
+    /// An unsaved Radio Browser station, from the search cache or `by_uuid`.
+    Discovered(RbStation),
+}
+
+/// Resolves a client-supplied station id to a stream URL held by the server.
+/// The id is the only thing the client controls; it never supplies a URL.
+async fn resolve_station(state: &AppState, id: &str) -> Result<ResolvedStation, Response> {
+    {
+        let stations = state.stations.read().await;
+        if stations.get_station_url(id).is_some() {
+            return Ok(ResolvedStation::Known);
+        }
+        if let Some(found) = stations.cached_search_result(id) {
+            return Ok(ResolvedStation::Discovered(found));
+        }
+    }
+    let unknown = || error_response(StatusCode::BAD_REQUEST, "unknown_station", "Station not found");
+    if !id.starts_with("rb-") {
+        return Err(unknown());
+    }
+    let uuid = radiobrowser::rb_uuid(id).ok_or_else(|| {
+        error_response(StatusCode::BAD_REQUEST, "bad_station", "Malformed station id")
+    })?;
+    let found = state
+        .radio_browser
+        .by_uuid(uuid)
+        .await
+        .map_err(|e| {
+            warn!("Radio Browser lookup failed: {}", e);
+            search_unavailable()
+        })?
+        .ok_or_else(unknown)?;
+    state
+        .stations
+        .write()
+        .await
+        .remember_search_results(std::slice::from_ref(&found));
+    Ok(ResolvedStation::Discovered(found))
+}
+
+#[derive(Debug, Deserialize)]
+struct MyRequest {
+    station: String,
+}
+
+fn my_error_response(error: MyError) -> Response {
+    match error {
+        MyError::Full => error_response(
+            StatusCode::CONFLICT,
+            "my_full",
+            &format!("MY holds at most {} stations", MY_CAP),
+        ),
+        MyError::Save(io_error) => {
+            error!("Could not save the MY list: {}", io_error);
+            error_response(
+                StatusCode::INTERNAL_SERVER_ERROR,
+                "my_save_failed",
+                "Could not save the MY list",
+            )
+        }
+    }
+}
+
+/// Tells every open browser the MY list changed. Called with the stations lock
+/// still held so events cannot be reordered. Having no listener is not an error.
+fn publish_stations(state: &AppState, view: &RegistryView) {
+    if let Ok(json) = serde_json::to_string(view) {
+        let _ = state.stations_tx.send(json);
+    }
+}
+
+/// `POST /api/my`: keeps a station in MY. Adding one already there is a no-op.
+async fn add_my_station(
+    AxumState(state): AxumState<AppState>,
+    Json(req): Json<MyRequest>,
+) -> Result<Json<RegistryView>, Response> {
+    let entry = match resolve_station(&state, &req.station).await? {
+        ResolvedStation::Known => {
+            let stations = state.stations.read().await;
+            if stations.is_in_my(&req.station) {
+                return Ok(Json(stations.registry_view()));
+            }
+            MyEntry::Ref { station_id: req.station.clone() }
+        }
+        ResolvedStation::Discovered(found) => MyEntry::Rb(StoredRbStation::from_rb(&found)),
+    };
+
+    let mut stations = state.stations.write().await;
+    let changed = stations.my_add(entry).await.map_err(my_error_response)?;
+    let view = stations.registry_view();
+    if changed {
+        publish_stations(&state, &view);
+    }
+    Ok(Json(view))
+}
+
+/// `DELETE /api/my/{id}`: drops a station from MY. Removing one that is not there is a no-op.
+async fn remove_my_station(
+    AxumState(state): AxumState<AppState>,
+    Path(id): Path<String>,
+) -> Result<Json<RegistryView>, Response> {
+    let mut stations = state.stations.write().await;
+    let changed = stations.my_remove(&id).await.map_err(my_error_response)?;
+    let view = stations.registry_view();
+    if changed {
+        publish_stations(&state, &view);
+    }
+    Ok(Json(view))
+}
+
 /// `GET /api/search?q=…&genre=…`: Radio Browser stations by name and/or genre
 /// chip. The results are remembered briefly so they can be played or kept by id.
 async fn search_stations(
@@ -689,13 +806,17 @@
 ) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
     let rx = state.state_manager.subscribe();
 
-    let stream = WatchStream::new(rx).map(|state| {
+    let state_events = WatchStream::new(rx).map(|state| {
         Ok(Event::default()
             .event("state")
             .data(serde_json::to_string(&state).unwrap()))
     });
+    // A lagging receiver drops events; clients refetch /api/stations on reconnect.
+    let stations_events = BroadcastStream::new(state.stations_tx.subscribe())
+        .filter_map(|message| message.ok())
+        .map(|json| Ok(Event::default().event("stations").data(json)));
 
-    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
+    Sse::new(state_events.merge(stations_events)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
 }
 
 /// `GET /api/vis`: SSE stream of spectrum frames (`event: vis`).
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`

Expected: everything passes (including the real-server SSE test) and no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/api.rs tests/integration_test.rs
git commit -m "$(cat <<'EOF'
api: add/remove MY stations and push a stations SSE event

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: Play `rb-` ids, resolving the URL server-side

**Files:**
- Modify: `src/api.rs` (`ResolvedStation::Known` carries the URL; `resolve_station`; `play_station`), `tests/integration_test.rs` (`FakeRadioBrowser::by_uuid_call_count` and four tests)
- Test: `tests/integration_test.rs`

**Interfaces:**
- Consumes: `resolve_station`, `ResolvedStation` (Task 9); `StationManager::note_playing` (Task 6).
- Produces:
  - `enum ResolvedStation { Known { url: String }, Discovered(RbStation) }`
  - `POST /api/play {"station":"<id>"}` (body unchanged) now accepts `rb-` ids: MY first, then the search cache (10 minutes, 200 entries), then Radio Browser `by_uuid`. The lookup happens before `play_mutex` is taken, so a slow lookup never blocks other plays. After a successful start the server calls `note_playing`, so the LCD shows the real name. A malformed `rb-` id is 400 `bad_station`; an unknown id stays 400 `unknown_station`; Radio Browser down for an uncached id is 503 `search_unavailable`; a cached or saved station still plays while Radio Browser is down.
  - The client never supplies a URL: the request type has only the `station` id (a test posts an extra `url` field and asserts it is ignored).

- [ ] **Step 1: Write the failing tests**

```diff
--- a/tests/integration_test.rs
+++ b/tests/integration_test.rs
@@ -417,6 +417,10 @@
     fn is_down(&self) -> bool {
         self.down.load(std::sync::atomic::Ordering::SeqCst)
     }
+
+    fn by_uuid_call_count(&self) -> usize {
+        self.by_uuid_calls.load(std::sync::atomic::Ordering::SeqCst)
+    }
 }
 
 #[async_trait::async_trait]
@@ -2561,3 +2565,107 @@
     assert!(received.contains(r#""id":"my""#), "{received:?}");
     assert!(received.contains(r#""in_my":true"#), "{received:?}");
 }
+
+// ---- Station discovery: playing rb- ids ----
+
+#[tokio::test]
+async fn play_resolves_an_unsaved_station_with_by_uuid_then_the_cache() {
+    let app = discovery_app().await;
+    let jazz_id = format!("rb-{JAZZ_UUID}");
+
+    let first = app.server.post("/api/play").json(&json!({"station": jazz_id})).await;
+    first.assert_status_ok();
+    assert_eq!(app.radio_browser.by_uuid_call_count(), 1);
+    let player_state = cliamp::Player::state(app.player.as_ref()).await;
+    assert_eq!(player_state.url.as_deref(), Some("http://jazz.example.com/stream"));
+    let state: serde_json::Value = first.json();
+    assert_eq!(state["player"]["station_name"], "Smooth Jazz Radio");
+
+    // The result is now cached: playing it again does not ask Radio Browser.
+    app.server.post("/api/play").json(&json!({"station": jazz_id})).await.assert_status_ok();
+    assert_eq!(app.radio_browser.by_uuid_call_count(), 1);
+}
+
+#[tokio::test]
+async fn play_uses_the_search_cache_when_radio_browser_goes_down() {
+    let app = discovery_app().await;
+    app.server.get("/api/search").add_query_param("q", "blues").await.assert_status_ok();
+    app.radio_browser.set_down(true);
+
+    let response = app.server.post("/api/play").json(&json!({"station": format!("rb-{BLUES_UUID}")})).await;
+
+    response.assert_status_ok();
+    assert_eq!(app.radio_browser.by_uuid_call_count(), 0);
+    let player_state = cliamp::Player::state(app.player.as_ref()).await;
+    assert_eq!(player_state.url.as_deref(), Some("https://blues.example.com/live"));
+}
+
+#[tokio::test]
+async fn play_uses_my_after_a_restart_without_asking_radio_browser() {
+    let cache_dir = tempfile::tempdir().unwrap();
+    let first = build_app_in(cache_dir.path(), Arc::new(MockYxcClient::new()), fast_timing(), None).await;
+    first.server.get("/api/search").add_query_param("q", "jazz").await.assert_status_ok();
+    let jazz_id = format!("rb-{JAZZ_UUID}");
+    first.server.post("/api/my").json(&json!({"station": jazz_id})).await.assert_status_ok();
+    drop(first);
+
+    let second = build_app_in(cache_dir.path(), Arc::new(MockYxcClient::new()), fast_timing(), None).await;
+    second.radio_browser.set_down(true);
+    let response = second.server.post("/api/play").json(&json!({"station": jazz_id})).await;
+
+    response.assert_status_ok();
+    assert_eq!(second.radio_browser.by_uuid_call_count(), 0);
+    let player_state = cliamp::Player::state(second.player.as_ref()).await;
+    assert_eq!(player_state.url.as_deref(), Some("http://jazz.example.com/stream"));
+}
+
+#[tokio::test]
+async fn play_rejects_ids_that_are_not_known_stations() {
+    let app = discovery_app().await;
+
+    // A URL in the station field is just an unknown id.
+    let url_as_id = app
+        .server
+        .post("/api/play")
+        .json(&json!({"station": "http://evil.example.com/stream"}))
+        .await;
+    url_as_id.assert_status(axum::http::StatusCode::BAD_REQUEST);
+    assert_eq!(url_as_id.json::<serde_json::Value>()["error"], "unknown_station");
+
+    // A URL smuggled in an extra field is ignored: the id still decides.
+    let extra_field = app
+        .server
+        .post("/api/play")
+        .json(&json!({"station": "nonexistent", "url": "http://evil.example.com/stream"}))
+        .await;
+    extra_field.assert_status(axum::http::StatusCode::BAD_REQUEST);
+
+    // A malformed rb- id never reaches Radio Browser.
+    let malformed = app.server.post("/api/play").json(&json!({"station": "rb-../../etc"})).await;
+    malformed.assert_status(axum::http::StatusCode::BAD_REQUEST);
+    assert_eq!(malformed.json::<serde_json::Value>()["error"], "bad_station");
+
+    // A well-formed rb- id that Radio Browser does not know.
+    let missing = app
+        .server
+        .post("/api/play")
+        .json(&json!({"station": "rb-99999999-9999-9999-9999-999999999999"}))
+        .await;
+    missing.assert_status(axum::http::StatusCode::BAD_REQUEST);
+    assert_eq!(missing.json::<serde_json::Value>()["error"], "unknown_station");
+
+    assert_eq!(app.radio_browser.by_uuid_call_count(), 1);
+    let player_state = cliamp::Player::state(app.player.as_ref()).await;
+    assert_eq!(player_state.url, None);
+}
+
+#[tokio::test]
+async fn play_reports_unavailable_when_an_unsaved_station_cannot_be_looked_up() {
+    let app = discovery_app().await;
+    app.radio_browser.set_down(true);
+
+    let response = app.server.post("/api/play").json(&json!({"station": format!("rb-{JAZZ_UUID}")})).await;
+
+    response.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
+    assert_eq!(response.json::<serde_json::Value>()["error"], "search_unavailable");
+}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --test integration_test play_`

Expected: 4 new tests FAILED (`play_resolves_an_unsaved_station_with_by_uuid_then_the_cache`, `play_uses_the_search_cache_when_radio_browser_goes_down`, `play_reports_unavailable_when_an_unsaved_station_cannot_be_looked_up`, `play_rejects_ids_that_are_not_known_stations`). The first three get ``received 400 (Bad Request) ... "detail": "Station not found"`` where they expect 200 or 503; the last fails its `bad_station` assertion with `left: "unknown_station"`.

- [ ] **Step 3: Implement**

```diff
--- a/src/api.rs
+++ b/src/api.rs
@@ -237,7 +237,7 @@
 /// A station id after the server has looked up where to find its stream.
 enum ResolvedStation {
     /// ROCK, CLIAMP or a saved MY station: the URL is already known.
-    Known,
+    Known { url: String },
     /// An unsaved Radio Browser station, from the search cache or `by_uuid`.
     Discovered(RbStation),
 }
@@ -247,8 +247,8 @@
 async fn resolve_station(state: &AppState, id: &str) -> Result<ResolvedStation, Response> {
     {
         let stations = state.stations.read().await;
-        if stations.get_station_url(id).is_some() {
-            return Ok(ResolvedStation::Known);
+        if let Some(url) = stations.get_station_url(id) {
+            return Ok(ResolvedStation::Known { url: url.to_string() });
         }
         if let Some(found) = stations.cached_search_result(id) {
             return Ok(ResolvedStation::Discovered(found));
@@ -315,7 +315,7 @@
     Json(req): Json<MyRequest>,
 ) -> Result<Json<RegistryView>, Response> {
     let entry = match resolve_station(&state, &req.station).await? {
-        ResolvedStation::Known => {
+        ResolvedStation::Known { .. } => {
             let stations = state.stations.read().await;
             if stations.is_in_my(&req.station) {
                 return Ok(Json(stations.registry_view()));
@@ -400,16 +400,16 @@
     AxumState(state): AxumState<AppState>,
     Json(req): Json<PlayRequest>,
 ) -> Result<Json<State>, Response> {
+    // Resolve the id first: a Radio Browser lookup can take seconds and must not
+    // hold up other plays.
+    let (station_url, discovered) = match resolve_station(&state, &req.station).await? {
+        ResolvedStation::Known { url } => (url, None),
+        ResolvedStation::Discovered(found) => (found.url.clone(), Some(found)),
+    };
+
     // Serialize play operations
     let _guard = state.play_mutex.lock().await;
 
-    let stations = state.stations.read().await;
-    let station_url = stations
-        .get_station_url(&req.station)
-        .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "unknown_station", "Station not found"))?
-        .to_string();
-    drop(stations);
-
     // Snapshot LIVE receiver state (the cache can be seconds old, and a
     // TV that just switched to hdmi1 must be seen as such).
     // Pending writes (power/input) are overlaid, since the receiver's readback lags.
@@ -455,6 +455,11 @@
     })?;
 
     state.state_manager.set_last_played_station(req.station.clone()).await;
+    state
+        .stations
+        .write()
+        .await
+        .note_playing(&req.station, discovered.as_ref());
     {
         let mut tracking = state.route_tracking.lock().unwrap();
         tracking.selection = Some(selected.clone());
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`

Expected: everything passes (85 lib tests and 75 integration tests at this point) and no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/api.rs tests/integration_test.rs
git commit -m "$(cat <<'EOF'
api: play rb- ids, resolving the stream URL on the server

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: Docs, README and provisioning for the MY file

**Files:**
- Modify: `docs/API.md` (endpoint table, a "Station ids" paragraph, the `my` group, a `Search` shape, the `stations` SSE event), `README.md` (feature list), `deploy/provision.sh` (create the default `cache_dir`)
- Test: grep assertions plus `bash -n deploy/provision.sh`

**Interfaces:**
- Consumes: the API behaviour from Tasks 7 to 10 (documents it; no code interface).
- Produces: `/var/lib/home-radio` is created, owned by the `radio` user, by `provision.sh`. Nothing created that directory before, so a fresh host could not have saved `my-stations.json` (`StationManager::new` also ignores a failed `create_dir_all`, which would have made every MY save fail with 500 `my_save_failed` for a non-root service user).

- [ ] **Step 1: Write the failing check**

```bash
# docs-check T11
fail=0
check() { grep -qF -- "$2" "$1" || { echo "FAIL: $1 lacks: $2"; fail=1; }; }
check docs/API.md '/api/search'
check docs/API.md '/api/my/{id}'
check docs/API.md 'my_full'
check docs/API.md 'bad_station'
check docs/API.md 'event: stations'
check docs/API.md '### `Search`'
check README.md 'my-stations.json'
check README.md 'ROCK/CLIAMP/MY'
check deploy/provision.sh '/var/lib/home-radio'
bash -n deploy/provision.sh || fail=1
[ "$fail" = 0 ] && echo PASS
```

- [ ] **Step 2: Run the check to see it fail**

Run the block above from the repo root.

Expected: nine `FAIL: ... lacks: ...` lines (one per `check`) and no `PASS`; `bash -n` is silent because the script is valid.

- [ ] **Step 3: Implement**

````diff
--- a/deploy/provision.sh
+++ b/deploy/provision.sh
@@ -83,6 +83,9 @@
 install -m 0755 radio-web /usr/local/bin/radio-web
 install -m 0644 stations.toml /etc/home-radio/stations.toml
 
+# The default cache_dir: holds the cliamp station cache and my-stations.json.
+install -d -o "$RADIO_USER" -g "$RADIO_USER" -m 0755 /var/lib/home-radio
+
 install -d -o "$RADIO_USER" -g "$RADIO_USER" -m 0755 \
   "$RADIO_HOME/.config" "$RADIO_HOME/.config/cliamp" "$RADIO_HOME/.cache" \
   "$RADIO_HOME/.cache/home-radio" "$RADIO_HOME/.config/pipewire" \
--- a/docs/API.md
+++ b/docs/API.md
@@ -81,7 +81,10 @@
 |---|---|---|---|
 | GET  | `/api/state` | – | `State` |
 | GET  | `/api/stations` | – | `Stations` |
-| POST | `/api/play` | `{"station":"big100","zones":{"main":false,"zone2":true}}`. `zones` is optional. | `State`. Unknown id → 400 `unknown_station`. No zone → 409 |
+| GET  | `/api/search?q=jazz&genre=Jazz` | – | `Search`. Needs `q` (2 to 80 characters) or `genre` (a chip name), else 400 `bad_query`. Radio Browser unreachable → 503 `search_unavailable` |
+| POST | `/api/my` | `{"station":"rb-<uuid>"}` | `Stations`. Adds a station to MY (a no-op if already there). Unknown id → 400 `unknown_station`, malformed `rb-` id → 400 `bad_station`, 51st entry → 409 `my_full`, Radio Browser unreachable → 503 `search_unavailable`, disk write failed → 500 `my_save_failed` |
+| DELETE | `/api/my/{id}` | – | `Stations`. Removes a station from MY (a no-op if it is not there) |
+| POST | `/api/play` | `{"station":"big100","zones":{"main":false,"zone2":true}}`. `zones` is optional. | `State`. Also accepts `rb-<uuid>` ids. Unknown id → 400 `unknown_station`, malformed `rb-` id → 400 `bad_station`. No zone → 409 |
 | POST | `/api/stop` | – | `State`. Stops cliamp only and leaves the zones alone |
 | POST | `/api/power` | `{"on":bool}` | `State`. Master switch. Off stops the radio and puts **both** zones in standby, including a TV on hdmi1. On wakes `main` only and leaves input, volume and selection alone |
 | POST | `/api/zone/{main\|zone2}/power` | `{"on":bool}` | `State` (toggle semantics above) |
@@ -96,6 +99,13 @@
 
 **Request size.** Bodies are limited to 4 KiB.
 
+**Station ids.** ROCK and CLIAMP ids are unchanged. A Radio Browser station is
+`rb-` followed by a lowercase hyphenated UUID. The browser only ever sends ids:
+the server resolves each one to a stream URL itself (the registry, then MY, then
+a 10-minute cache of recent search results with at most 200 entries, then Radio
+Browser's `byuuid` lookup), so no client can make the receiver play an
+arbitrary URL.
+
 ### `State`
 
 ```json
@@ -141,16 +151,39 @@
 }
 ```
 
+Each group also carries `in_my` on every station. A third group, `my`, is the
+household's shared list (label `MY`, insertion order, at most 50 entries):
+
+```json
+{ "id": "my", "label": "MY",
+  "stations": [ { "id": "rb-11111111-1111-1111-1111-111111111111", "name": "Smooth Jazz 24/7",
+                  "short": "Smooth Jazz", "genre": "Jazz", "in_my": true } ] }
+```
+
 - The `rock` group comes from `stations.toml`, in file order. The first 7 stations
   are the preset buttons.
 - The `cliamp` group is fetched from `https://radio.cliamp.stream/stations` at
   startup and every 6 h, and cached on disk. Ids that collide with rock ids are
   dropped, and streams must be `https://` or `http://`.
 
+### `Search`
+
+```json
+{ "results": [ { "id": "rb-11111111-1111-1111-1111-111111111111", "name": "Smooth Jazz 24/7",
+                 "genre": "Jazz", "country": "United States", "bitrate": 128, "in_my": false } ] }
+```
+
+Up to 30 results from Radio Browser, MP3 or AAC only. Text from Radio Browser is
+untrusted: names are capped at 80 characters and tags at 40, and the UI renders
+them with `textContent` only. `genre` is one of Rock, Classic Rock, Alt, Jazz,
+Blues, Country, Oldies, Classical, Lofi, News, Talk.
+
 ### `GET /api/events` (SSE)
 
 - **Updates.** Each state change is sent as `event: state` with `data: <State JSON>`.
   The first event goes out immediately on connect.
+- **MY changes.** After every successful change to MY, an `event: stations` is
+  sent with `data: <Stations JSON>`, so other open browsers refresh their MY band.
 - **Keepalive.** A `: ping` comment is sent every 15 s.
 - **Sources.** Changes come from these places:
   - `cliamp remote events runtime.state`, an NDJSON child process that is restarted
--- a/README.md
+++ b/README.md
@@ -9,8 +9,11 @@
 <p align="center"><img src="docs/screenshot-2026-10-06_12-44-35.png" alt="homeradio: the boom box UI with spectrum analyser, tuning dial, presets and two zone controls" width="760"></p>
 
 - **Two zones.** A MEDIA ROOM / UPSTAIRS (receiver main + zone2) on-off switch and a
-  volume knob per room, 7 ROCK presets, a tuning dial over all stations with a
-  ROCK/CLIAMP band switch, PLAY and STOP.
+  volume knob per room, 7 presets and a tuning dial that follow a
+  ROCK/CLIAMP/MY band switch, PLAY and STOP.
+- **MY stations and search.** The 🔍 button searches the public Radio Browser
+  directory by name or genre. ★ on the display keeps a station in MY, one shared
+  list for the household (up to 50), stored in `cache_dir/my-stations.json`.
 - **Master power.** A power button by the logo: off stops the radio and puts both
   zones in standby (TV included); on wakes the main zone.
 - **Volume caps.** The knobs stop at per-zone caps set in the config, and the server
````

- [ ] **Step 4: Run the check to see it pass**

Run the block from Step 1 again.

Expected: `PASS`, and `git diff --stat` shows only `docs/API.md`, `README.md` and `deploy/provision.sh`.

- [ ] **Step 5: Commit**

```bash
git add docs/API.md README.md deploy/provision.sh
git commit -m "$(cat <<'EOF'
docs: document MY, search and the stations event; create /var/lib/home-radio when provisioning

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 12: Three-way band switch and the MY band

The web tasks have no JS test framework (none exists in the repo and this plan does not add one). Each has a grep check plus `node --check web/app.js` as the "test", and a browser checklist for behaviour. To see the page, run the service against your lab receiver: `cargo run -- --config <your config.toml>` and open the `listen` address (default `http://localhost:8080`). `web/` is embedded at build time (`debug-embed`), so rebuild with `cargo run` after each edit.

**Files:**
- Modify: `web/index.html` (the `.band-switch-container` block, around lines 90 to 100), `web/app.css` (the "Band switch" and preset rules, around lines 780 to 900, and the focus rules near line 1150 to 1210), `web/app.js` (state at the top, `renderPresetButtons`, `renderDialScale`, `followStation`, `setupEventListeners`)
- Test: grep check, `node --check web/app.js`, browser checklist

**Interfaces:**
- Consumes: `GET /api/stations` including the `my` group (Task 7), which `app.js` already stores in `stations`.
- Produces (JS, in `web/app.js`):
  - `const BANDS = ['rock', 'cliamp', 'my']`, `const PRESET_SLOTS = 7`
  - `paintBandSwitch(band, focus = false)`: sets `currentBand`, `#bandSwitch[data-band]`, `aria-checked` and the roving `tabindex` on the three `.band-option` buttons
  - `setBand(band, { focus = false } = {})`: user-initiated switch; re-renders presets and the dial and starts the dial on the playing station when it is in the band
  - `followStation(stationId)`: now searches the current band first, then the others, in `BANDS` order
  - `renderPresetButtons()` always renders 7 keys for the current band; missing slots are blank and `disabled`
  - Markup: `#bandSwitch` is a `role="radiogroup"` with three `role="radio"` buttons `.band-option[data-band]`; the empty MY dial shows `★ a station to keep it here`.

- [ ] **Step 1: Write the failing check**

```bash
# web-check T12
fail=0
check() { grep -qF -- "$2" "$1" || { echo "FAIL: $1 lacks: $2"; fail=1; }; }
check web/index.html 'role="radiogroup"'
check web/index.html 'data-band="my"'
check web/app.js "const BANDS = ['rock', 'cliamp', 'my'];"
check web/app.js 'function setBand('
check web/app.js 'function paintBandSwitch('
check web/app.js 'a station to keep it here'
check web/app.css '.band-switch[data-band="my"]'
check web/app.css '.preset-btn:disabled'
if grep -q 'bandSwitch.checked' web/app.js; then echo "FAIL: app.js still treats #bandSwitch as a checkbox"; fail=1; fi
node --check web/app.js || fail=1
[ "$fail" = 0 ] && echo PASS
```

- [ ] **Step 2: Run the check to see it fail**

Run the block above from the repo root.

Expected: eight `FAIL: ... lacks: ...` lines plus `FAIL: app.js still treats #bandSwitch as a checkbox`, and no `PASS`; `node --check` itself succeeds.

- [ ] **Step 3: Implement**

```diff
--- a/web/app.css
+++ b/web/app.css
@@ -780,6 +780,14 @@
     color: rgba(214, 252, 248, 0.7);
 }
 
+/* The empty MY band: a sentence, not a label */
+.dial-empty.dial-hint {
+    padding: 0 8px;
+    text-align: center;
+    font-size: 11px;
+    letter-spacing: 0.04em;
+}
+
 /* Band switch */
 .band-switch-container {
     display: flex;
@@ -792,18 +800,11 @@
     cursor: pointer;
 }
 
-.band-switch input {
-    position: absolute;
-    opacity: 0;
-    width: 0;
-    height: 0;
-}
-
 .band-switch-slider {
     position: relative;
     display: flex;
     align-items: center;
-    width: 200px;
+    width: 240px;
     padding: 6px;
     border-radius: 999px;
     border: 1px solid var(--border);
@@ -814,8 +815,8 @@
 .band-switch-slider::before {
     content: '';
     position: absolute;
-    left: 5px;
-    width: calc(50% - 5px);
+    left: 6px;
+    width: calc((100% - 12px) / 3);
     height: calc(100% - 10px);
     border-radius: 999px;
     background: linear-gradient(to bottom, var(--teal), var(--teal-deep));
@@ -823,15 +824,24 @@
     transition: transform 0.3s;
 }
 
-.band-switch input:checked ~ .band-switch-slider::before {
-    transform: translateX(calc(100% + 0px));
+.band-switch[data-band="cliamp"] .band-switch-slider::before {
+    transform: translateX(100%);
 }
 
-.band-label-left,
-.band-label-right {
+.band-switch[data-band="my"] .band-switch-slider::before {
+    transform: translateX(200%);
+}
+
+/* One segment of the three-way switch (a radio button in a radiogroup) */
+.band-option {
     position: relative;
     z-index: 2;
     flex: 1;
+    padding: 2px 0;
+    border: 0;
+    border-radius: 999px;
+    background: none;
+    cursor: inherit;
     text-align: center;
     font-family: var(--mono);
     font-size: 11px;
@@ -841,11 +851,15 @@
     transition: color 0.3s;
 }
 
-.band-switch input:not(:checked) ~ .band-switch-slider .band-label-left,
-.band-switch input:checked ~ .band-switch-slider .band-label-right {
+.band-option[aria-checked="true"] {
     color: var(--ink-0);
 }
 
+.band-option:focus-visible {
+    outline: 3px solid var(--focus);
+    outline-offset: 2px;
+}
+
 /* Preset buttons: dark metal keys, teal-lit when selected */
 .preset-buttons {
     display: grid;
@@ -869,6 +883,8 @@
 
 .preset-btn {
     position: relative;
+    min-width: 0;
+    overflow-wrap: anywhere;
     padding: 12px 6px 10px;
     border-radius: 6px;
     font-size: 11px;
@@ -878,6 +894,13 @@
     text-align: center;
 }
 
+/* A band with fewer than 7 stations leaves blank, dead keys */
+.preset-btn:disabled {
+    opacity: 0.35;
+    cursor: default;
+    pointer-events: none;
+}
+
 /* indicator slit on every key */
 .preset-btn::before {
     content: '';
@@ -1152,7 +1175,7 @@
 .zone-toggle:active .toggle-slider::after,
 .preset-btn:focus-visible::after,
 .transport-btn:focus-visible::after,
-.band-switch input:focus-visible ~ .band-switch-slider::after,
+.band-switch:has(.band-option:focus-visible) .band-switch-slider::after,
 .zone-toggle input:focus-visible ~ .toggle-slider::after {
     opacity: 1;
 }
@@ -1199,7 +1222,6 @@
     outline-offset: 3px;
 }
 
-.band-switch input:focus-visible ~ .band-switch-slider,
 .zone-toggle input:focus-visible ~ .toggle-slider {
     outline: 3px solid var(--focus);
     outline-offset: 3px;
--- a/web/app.js
+++ b/web/app.js
@@ -2,6 +2,8 @@
 let stations = { groups: [] };
 let currentState = null;
 let currentBand = 'rock';
+const BANDS = ['rock', 'cliamp', 'my'];
+const PRESET_SLOTS = 7;
 let selectedStationIndex = 0;
 let isDraggingDial = false;
 let isDraggingKnob = false;
@@ -22,6 +24,7 @@
     dialScale: document.getElementById('dialScale'),
     dialControl: document.getElementById('dialControl'),
     bandSwitch: document.getElementById('bandSwitch'),
+    bandOptions: Array.from(document.querySelectorAll('.band-option')),
     presetButtons: document.getElementById('presetButtons'),
     playBtn: document.getElementById('playBtn'),
     stopBtn: document.getElementById('stopBtn'),
@@ -142,22 +145,30 @@
     };
 }
 
-// Render preset buttons (first 7 rock stations) using DOM APIs only (no HTML strings)
+// Render the 7 preset keys for the current band using DOM APIs only (no HTML
+// strings). A band with fewer than 7 stations leaves the extra keys blank and disabled.
 function renderPresetButtons() {
-    const rockGroup = stations.groups.find(g => g.id === 'rock');
-    if (!rockGroup) return;
+    const presetStations = (getCurrentGroup()?.stations ?? []).slice(0, PRESET_SLOTS);
 
-    const buttons = rockGroup.stations.slice(0, 7).map((station, index) => {
+    const buttons = Array.from({ length: PRESET_SLOTS }, (_, index) => {
+        const station = presetStations[index];
         const button = document.createElement('button');
         button.type = 'button';
         button.className = 'preset-btn';
-        button.dataset.station = station.id;
         button.dataset.index = String(index);
+        if (!station) {
+            button.disabled = true;
+            button.setAttribute('aria-label', 'Empty preset');
+            return button;
+        }
+        button.dataset.station = station.id;
         button.textContent = station.short;
+        button.title = station.name;
         button.addEventListener('click', () => playStation(station.id));
         return button;
     });
     elements.presetButtons.replaceChildren(...buttons);
+    updatePresetSelection(currentState?.player.station ?? null);
 }
 
 // Dial label layout (computed in renderDialScale, applied by updateDialLabels)
@@ -180,7 +191,9 @@
         dialLayout = null;
         const empty = document.createElement('div');
         empty.className = 'dial-empty';
-        empty.textContent = 'NO STATIONS';
+        const isEmptyMyBand = currentBand === 'my';
+        empty.classList.toggle('dial-hint', isEmptyMyBand);
+        empty.textContent = isEmptyMyBand ? '\u2605 a station to keep it here' : 'NO STATIONS';
         elements.dialScale.replaceChildren(empty);
         return;
     }
@@ -607,26 +620,61 @@
     positionNeedle();
 }
 
-// Move the dial to a newly playing station, switching band if it lives in the other one
-function followStation(stationId) {
-    const inCurrent = getCurrentGroup()?.stations.findIndex(s => s.id === stationId) ?? -1;
-    if (inCurrent !== -1) {
-        selectedStationIndex = inCurrent;
-        positionNeedle();
-        return;
+/**
+ * Make `band` the current one and repaint the switch (thumb position, aria-checked,
+ * roving tabindex). Does not touch the presets or the dial.
+ * @param {string} band one of BANDS
+ * @param {boolean} [focus] move keyboard focus to the newly checked segment
+ */
+function paintBandSwitch(band, focus = false) {
+    currentBand = band;
+    elements.bandSwitch.dataset.band = band;
+    for (const option of elements.bandOptions) {
+        const isChecked = option.dataset.band === band;
+        option.setAttribute('aria-checked', String(isChecked));
+        option.tabIndex = isChecked ? 0 : -1;
+        if (isChecked && focus) option.focus();
     }
+}
 
-    const otherBand = currentBand === 'rock' ? 'cliamp' : 'rock';
-    const otherGroup = stations.groups.find(g => g.id === otherBand);
-    const inOther = otherGroup ? otherGroup.stations.findIndex(s => s.id === stationId) : -1;
-    if (inOther === -1) return;
-
-    currentBand = otherBand;
-    elements.bandSwitch.checked = otherBand === 'cliamp';
-    selectedStationIndex = inOther;
+/**
+ * Switch band by the user's choice: the presets and dial follow it, and the dial
+ * starts on the playing station when that station is in the band.
+ * @param {string} band one of BANDS
+ * @param {{focus?: boolean}} [options]
+ */
+function setBand(band, { focus = false } = {}) {
+    if (!BANDS.includes(band)) return;
+    paintBandSwitch(band, focus);
+    const playingId = currentState?.player.station;
+    const group = getCurrentGroup();
+    const playingIndex = playingId && group ? group.stations.findIndex(s => s.id === playingId) : -1;
+    selectedStationIndex = playingIndex === -1 ? 0 : playingIndex;
+    renderPresetButtons();
     refreshDial();
 }
 
+// Move the dial to a newly playing station, switching band if it lives in another one.
+// The current band wins when the station is in several (a MY entry that is also a preset).
+function followStation(stationId) {
+    const searchOrder = [currentBand, ...BANDS.filter(band => band !== currentBand)];
+    for (const band of searchOrder) {
+        const group = stations.groups.find(g => g.id === band);
+        const index = group ? group.stations.findIndex(s => s.id === stationId) : -1;
+        if (index === -1) continue;
+        if (band === currentBand) {
+            selectedStationIndex = index;
+            positionNeedle();
+            return;
+        }
+        paintBandSwitch(band);
+        renderPresetButtons();
+        selectedStationIndex = index;
+        refreshDial();
+        return;
+    }
+}
+
 // Update preset button selection
 function updatePresetSelection(stationId) {
     elements.presetButtons.querySelectorAll('.preset-btn').forEach(btn => {
@@ -809,13 +857,17 @@
 // Event listeners
 function setupEventListeners() {
     // Band switch: an explicit choice sticks until the playing station changes
-    elements.bandSwitch.addEventListener('change', (e) => {
-        currentBand = e.target.checked ? 'cliamp' : 'rock';
-        const playingId = currentState?.player.station;
-        const group = getCurrentGroup();
-        const playingIndex = playingId && group ? group.stations.findIndex(s => s.id === playingId) : -1;
-        selectedStationIndex = playingIndex === -1 ? 0 : playingIndex;
-        refreshDial();
+    elements.bandSwitch.addEventListener('click', (event) => {
+        const option = event.target.closest('.band-option');
+        if (option) setBand(option.dataset.band);
+    });
+    // Radiogroup keys: arrows move to the neighbouring band, wrapping round
+    elements.bandSwitch.addEventListener('keydown', (event) => {
+        const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key];
+        if (step === undefined) return;
+        event.preventDefault();
+        const nextIndex = (BANDS.indexOf(currentBand) + step + BANDS.length) % BANDS.length;
+        setBand(BANDS[nextIndex], { focus: true });
     });
 
     // Play button
--- a/web/index.html
+++ b/web/index.html
@@ -89,16 +89,16 @@
 
                 <!-- Band switch -->
                 <div class="band-switch-container">
-                    <label class="band-switch">
-                        <input type="checkbox" id="bandSwitch" aria-label="Band selector">
-                        <span class="band-switch-slider">
-                            <span class="band-label-left">ROCK</span>
-                            <span class="band-label-right">CLIAMP</span>
-                        </span>
-                    </label>
+                    <div class="band-switch" id="bandSwitch" role="radiogroup" aria-label="Band selector" data-band="rock">
+                        <div class="band-switch-slider">
+                            <button type="button" class="band-option" role="radio" aria-checked="true" data-band="rock" tabindex="0">ROCK</button>
+                            <button type="button" class="band-option" role="radio" aria-checked="false" data-band="cliamp" tabindex="-1">CLIAMP</button>
+                            <button type="button" class="band-option" role="radio" aria-checked="false" data-band="my" tabindex="-1">MY</button>
+                        </div>
+                    </div>
                 </div>
 
-                <!-- Preset buttons (first 7 rock stations) -->
+                <!-- Preset buttons (first 7 stations of the selected band) -->
                 <div class="preset-buttons" id="presetButtons">
                     <!-- Populated by JS -->
                 </div>
```

- [ ] **Step 4: Run the check to see it pass, then check in the browser**

Run the block from Step 1 again. Expected: `PASS`.

Browser checklist (phone width and desktop width):
1. The switch reads ROCK | CLIAMP | MY with ROCK lit and 7 ROCK presets.
2. Click CLIAMP: the thumb slides to the middle; the presets and the dial show CLIAMP stations.
3. Click MY with an empty list: all 7 presets are blank and dim and cannot be clicked; the dial shows "★ a station to keep it here".
4. Keyboard: Tab into the switch lands only on the checked segment; ArrowRight and ArrowLeft move the selection and the focus ring and wrap from MY to ROCK.
5. Start a CLIAMP station from another client (`curl -X POST http://<host>:8080/api/play -H 'Content-Type: application/json' -d '{"station":"kexp"}'`) while ROCK is selected: the switch follows to CLIAMP and the presets re-render.
6. The browser console shows no errors.

- [ ] **Step 5: Commit**

```bash
git add web/index.html web/app.css web/app.js
git commit -m "$(cat <<'EOF'
web: three-way ROCK/CLIAMP/MY band switch with blank presets for short bands

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 13: LCD star and the `stations` event

**Files:**
- Modify: `web/index.html` (the `.lcd` block, around line 50), `web/app.css` (`.lcd-content` and new `.lcd-star` rules, around line 496), `web/app.js` (state variables, `elements`, `setupEventSource`, new helpers after it, `updateUI`, `setupEventListeners`)
- Test: grep check, `node --check web/app.js`, browser checklist

**Interfaces:**
- Consumes: `POST /api/my {"station":id}` and `DELETE /api/my/{id}` returning `Stations` (Task 9); SSE `event: stations` with the `Stations` JSON as data (Task 9); `GET /api/stations`; `apiCall(method, path, body)` (existing, returns parsed JSON or `null` after showing the error).
- Produces (JS):
  - `applyStations(data)`: adopts a Stations payload, keeps the dial on the same station and repaints presets, dial and star. Ignores `null` and malformed data.
  - `isInMy(stationId) -> boolean`, `toggleMy(stationId, isSaved)`, `starTargetId() -> ?string` (the current station, else the last played one), `renderStar()`, `refetchStations()`
  - Markup: `<button id="lcdStar" class="lcd-star" aria-pressed>` with `&#9733;`; disabled until a station has been played. `aria-pressed="true"` means the station is in MY.

- [ ] **Step 1: Write the failing check**

```bash
# web-check T13
fail=0
check() { grep -qF -- "$2" "$1" || { echo "FAIL: $1 lacks: $2"; fail=1; }; }
check web/index.html 'id="lcdStar"'
check web/app.js "addEventListener('stations'"
check web/app.js 'function applyStations('
check web/app.js 'function toggleMy('
check web/app.js 'function renderStar('
check web/app.js "apiCall('DELETE'"
check web/app.css '.lcd-star[aria-pressed="true"]'
node --check web/app.js || fail=1
[ "$fail" = 0 ] && echo PASS
```

- [ ] **Step 2: Run the check to see it fail**

Expected: seven `FAIL: ... lacks: ...` lines, no `PASS`.

- [ ] **Step 3: Implement**

```diff
--- a/web/app.css
+++ b/web/app.css
@@ -496,6 +496,45 @@
     display: flex;
     align-items: center;
     height: 100%;
+    min-width: 0;
+    margin-right: 30px; /* room for the star */
+    overflow: hidden;
+}
+
+/* The LCD's star: dim outline when the station is not in MY, lit when it is */
+.lcd-star {
+    position: absolute;
+    top: 50%;
+    right: 6px;
+    transform: translateY(-50%);
+    display: grid;
+    place-items: center;
+    width: 24px;
+    height: 24px;
+    padding: 0;
+    border: 0;
+    border-radius: 4px;
+    background: #052a28;
+    color: rgba(143, 245, 236, 0.35);
+    font-size: 17px;
+    line-height: 1;
+    cursor: pointer;
+    transition: color 0.2s, text-shadow 0.2s;
+}
+
+.lcd-star[aria-pressed="true"] {
+    color: #8ff5ec;
+    text-shadow: 0 0 6px rgba(77, 208, 200, 0.7);
+}
+
+.lcd-star:disabled {
+    opacity: 0.35;
+    cursor: default;
+}
+
+.lcd-star:focus-visible {
+    outline: 3px solid var(--focus);
+    outline-offset: -3px;
 }
 
 .lcd-text {
--- a/web/app.js
+++ b/web/app.js
@@ -10,6 +10,8 @@
 let activeKnob = null;
 let eventSource = null;
 let lastSeenStation = null; // band auto-follow only when this changes
+let lastPlayedId = null; // the star's target while nothing is playing
+let stationsStale = false; // the event stream dropped: refetch stations once it is back
 let pendingFollowStation = null; // station change that arrived mid-drag; applied on drag end
 let lcdHoldActive = false;  // an error message currently owns the LCD
 let lcdHoldTimer = null;
@@ -25,6 +27,7 @@
     dialControl: document.getElementById('dialControl'),
     bandSwitch: document.getElementById('bandSwitch'),
     bandOptions: Array.from(document.querySelectorAll('.band-option')),
+    lcdStar: document.getElementById('lcdStar'),
     presetButtons: document.getElementById('presetButtons'),
     playBtn: document.getElementById('playBtn'),
     stopBtn: document.getElementById('stopBtn'),
@@ -139,12 +142,72 @@
         updateUI(state);
     });
 
+    // Another browser changed MY: the event carries the whole Stations payload
+    eventSource.addEventListener('stations', (event) => {
+        applyStations(JSON.parse(event.data));
+    });
+
     eventSource.onerror = () => {
         showError('Connection lost, reconnecting...');
-        // EventSource will auto-reconnect
+        // EventSource will auto-reconnect; MY events missed meanwhile are refetched then
+        stationsStale = true;
+    };
+
+    eventSource.onopen = () => {
+        if (!stationsStale) return;
+        stationsStale = false;
+        refetchStations();
     };
 }
 
+/** Fetch the station registry again and repaint it (after a dropped event stream). */
+async function refetchStations() {
+    applyStations(await apiCall('GET', '/stations'));
+}
+
+/**
+ * Adopt a new Stations payload (from a MY change here or elsewhere) and repaint
+ * everything that depends on it, keeping the station the dial was on.
+ * @param {?{groups: Array}} data the Stations JSON, or null when a request failed
+ */
+function applyStations(data) {
+    if (!data || !Array.isArray(data.groups)) return;
+    const selectedId = getCurrentGroup()?.stations[selectedStationIndex]?.id;
+    stations = data;
+    const keptIndex = selectedId ? (getCurrentGroup()?.stations.findIndex(s => s.id === selectedId) ?? -1) : -1;
+    if (keptIndex !== -1) selectedStationIndex = keptIndex;
+    renderPresetButtons();
+    refreshDial();
+    renderStar();
+}
+
+/** @returns {boolean} whether the station is in the MY list */
+function isInMy(stationId) {
+    return stations.groups.find(g => g.id === 'my')?.stations.some(s => s.id === stationId) ?? false;
+}
+
+/** Add the station to MY, or remove it when it is already there. */
+async function toggleMy(stationId, isSaved) {
+    const data = isSaved
+        ? await apiCall('DELETE', `/my/${encodeURIComponent(stationId)}`)
+        : await apiCall('POST', '/my', { station: stationId });
+    applyStations(data);
+}
+
+/** @returns {?string} the station the LCD star acts on: the current one, else the last played */
+function starTargetId() {
+    return currentState?.player.station ?? lastPlayedId;
+}
+
+// Paint the LCD star: disabled with no station yet, filled when the station is in MY
+function renderStar() {
+    const targetId = starTargetId();
+    const isSaved = targetId ? isInMy(targetId) : false;
+    elements.lcdStar.disabled = !targetId;
+    elements.lcdStar.setAttribute('aria-pressed', String(isSaved));
+    elements.lcdStar.title = isSaved ? 'Remove from MY' : 'Keep in MY';
+}
+
 // Render the 7 preset keys for the current band using DOM APIs only (no HTML
 // strings). A band with fewer than 7 stations leaves the extra keys blank and disabled.
 function renderPresetButtons() {
@@ -300,8 +363,10 @@
         }
     }
 
-    // Preset button selection
+    // Preset button selection and the LCD star
     updatePresetSelection(stationId);
+    if (stationId) lastPlayedId = stationId;
+    renderStar();
 
     // Zones
     if (state.zones) {
@@ -870,6 +935,12 @@
         setBand(BANDS[nextIndex], { focus: true });
     });
 
+    // LCD star: keep the current station in MY, or take it out again
+    elements.lcdStar.addEventListener('click', () => {
+        const targetId = starTargetId();
+        if (targetId) toggleMy(targetId, isInMy(targetId));
+    });
+
     // Play button
     elements.playBtn.addEventListener('click', () => {
         const group = getCurrentGroup();
--- a/web/index.html
+++ b/web/index.html
@@ -50,6 +50,8 @@
                     <div class="lcd-content" role="status" aria-live="polite" aria-atomic="true">
                         <div class="lcd-text" id="nowPlaying">READY</div>
                     </div>
+                    <!-- Keep the current station in MY (filled when it is there) -->
+                    <button type="button" class="lcd-star" id="lcdStar" aria-label="Keep in MY" aria-pressed="false" title="Keep in MY" disabled>&#9733;</button>
                 </div>
 
                 <!-- Spectrum analyser (decorative) + STEREO signal lamp -->
```

- [ ] **Step 4: Run the check to see it pass, then check in the browser**

Run the block from Step 1 again. Expected: `PASS`.

Browser checklist:
1. Fresh page, nothing played yet: the star at the right edge of the LCD is dim and cannot be clicked.
2. Play a ROCK preset: the star becomes active (outline). Click it: it fills, and the MY band now has that station as preset 1.
3. Click it again: it empties and the MY band is blank again.
4. Press STOP: the star still acts on the last played station.
5. Open the page in a second tab and toggle the star in the first: the second tab's MY band updates without a reload. (`curl -N http://<host>:8080/api/events` shows an `event: stations` line.)
6. Restart `radio-web` while the page is open: after the stream reconnects, the MY band refreshes and the console shows no errors.
7. A long station name does not run under the star.

- [ ] **Step 5: Commit**

```bash
git add web/index.html web/app.css web/app.js
git commit -m "$(cat <<'EOF'
web: LCD star toggles MY and MY changes refresh every open browser

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 14: Search button and drawer shell

**Files:**
- Modify: `web/index.html` (🔍 button in `.band-switch-container`; drawer markup before the `<script>` tag), `web/app.css` (`.band-switch-container`, `.search-btn`, drawer rules appended near the end), `web/app.js` (`elements`, `setupEventListeners`, new open/close/focus-trap functions before `setupDialControl`)
- Test: grep check, `node --check web/app.js`, browser checklist

**Interfaces:**
- Consumes: the `elements` map and `setupEventListeners` (existing).
- Produces (JS): `isSearchOpen() -> boolean`, `openSearch()`, `closeSearch()`, `trapSearchFocus(event)`, `setupSearch()` (called from `setupEventListeners`). Opening sets `inert` on `.boombox`, shows `#searchBackdrop` and `#searchDrawer`, sets `aria-expanded="true"` on `#searchBtn` and focuses `#searchInput`. Closing reverses that and returns focus to `#searchBtn`. Escape, `#searchClose` and the backdrop close it; Tab and Shift+Tab stay inside.
- Produces (markup): `#searchBtn` (`aria-haspopup="dialog"`), `#searchBackdrop[hidden]`, `#searchDrawer[role="dialog"][aria-modal="true"][aria-labelledby="searchTitle"][hidden]` containing `#searchClose`, `#searchInput`, `#searchChips` (empty), `#searchStatus`, `#searchResults`. The drawer is at body level because `.control-panel` has its own stacking context (`position: relative; z-index: 1`). Layout: bottom sheet up to 900 px, 400 px side panel above 900 px.

- [ ] **Step 1: Write the failing check**

```bash
# web-check T14
fail=0
check() { grep -qF -- "$2" "$1" || { echo "FAIL: $1 lacks: $2"; fail=1; }; }
check web/index.html 'id="searchBtn"'
check web/index.html 'role="dialog" aria-modal="true" aria-labelledby="searchTitle"'
check web/app.js 'function openSearch('
check web/app.js 'function closeSearch('
check web/app.js 'function trapSearchFocus('
check web/app.js 'elements.boombox.inert = true;'
check web/app.css '.search-drawer[hidden]'
check web/app.css '@keyframes drawer-in'
node --check web/app.js || fail=1
[ "$fail" = 0 ] && echo PASS
```

- [ ] **Step 2: Run the check to see it fail**

Expected: eight `FAIL: ... lacks: ...` lines and no `PASS`.

- [ ] **Step 3: Implement**

```diff
--- a/web/app.css
+++ b/web/app.css
@@ -830,7 +830,9 @@
 /* Band switch */
 .band-switch-container {
     display: flex;
+    align-items: center;
     justify-content: center;
+    gap: 10px;
 }
 
 .band-switch {
@@ -899,6 +901,27 @@
     outline-offset: 2px;
 }
 
+/* Search button beside the band switch: a small round metal key */
+.search-btn {
+    width: 34px;
+    height: 34px;
+    padding: 0;
+    border-radius: 50%;
+    border: 1px solid #323a47;
+    background: linear-gradient(to bottom, #2b313c, #1a1e27);
+    box-shadow: 0 2px 0 #07090d, inset 0 1px 0 rgba(255, 255, 255, 0.14);
+    font-size: 15px;
+    line-height: 1;
+    cursor: pointer;
+}
+
+.search-btn:hover { border-color: var(--teal-dim); }
+.search-btn:active { transform: translateY(1px); }
+.search-btn:focus-visible {
+    outline: 3px solid var(--focus);
+    outline-offset: 3px;
+}
+
 /* Preset buttons: dark metal keys, teal-lit when selected */
 .preset-buttons {
     display: grid;
@@ -1346,3 +1369,115 @@
     .volume-knob { width: 76px; height: 76px; }
     .knob-indicator { height: 26px; transform-origin: 50% 29px; }
 }
+
+/* Search drawer. It sits at body level, outside .control-panel (which has its own
+   stacking context), so it can cover the whole boom box. */
+.search-backdrop {
+    position: fixed;
+    inset: 0;
+    z-index: 40;
+    background: rgba(0, 0, 0, 0.55);
+}
+
+.search-drawer {
+    position: fixed;
+    z-index: 41;
+    display: flex;
+    flex-direction: column;
+    gap: 10px;
+    padding: 14px 16px 16px;
+    color: var(--text-1);
+    border: 1px solid var(--border);
+    background: linear-gradient(to bottom, #1a1e27, #12151c);
+    box-shadow: 0 -8px 30px rgba(0, 0, 0, 0.6);
+    /* phones: a bottom sheet */
+    left: 0;
+    right: 0;
+    bottom: 0;
+    max-height: 80dvh;
+    border-radius: 14px 14px 0 0;
+}
+
+/* display: flex above would beat the hidden attribute */
+.search-drawer[hidden],
+.search-backdrop[hidden] {
+    display: none;
+}
+
+.search-head {
+    display: flex;
+    align-items: center;
+    justify-content: space-between;
+}
+
+.search-title {
+    font-family: var(--mono);
+    font-size: 12px;
+    font-weight: 600;
+    letter-spacing: 0.14em;
+    text-transform: uppercase;
+    color: var(--teal);
+}
+
+.search-close {
+    width: 32px;
+    height: 32px;
+    border: 1px solid var(--border);
+    border-radius: 50%;
+    background: var(--ink-0);
+    color: var(--text-2);
+    font-size: 20px;
+    line-height: 1;
+    cursor: pointer;
+}
+
+.search-close:focus-visible {
+    outline: 3px solid var(--focus);
+    outline-offset: 2px;
+}
+
+.search-input {
+    width: 100%;
+    padding: 10px 12px;
+    border-radius: 8px;
+    border: 1px solid var(--border);
+    background: var(--ink-0);
+    color: var(--text-1);
+    font-family: var(--sans);
+    font-size: 16px; /* 16px stops iOS zooming the page on focus */
+}
+
+.search-input:focus-visible {
+    outline: 3px solid var(--focus);
+    outline-offset: 1px;
+}
+
+@media (prefers-reduced-motion: no-preference) {
+    .search-drawer { animation: drawer-up 0.22s ease-out; }
+}
+
+@keyframes drawer-up {
+    from { transform: translateY(100%); }
+    to { transform: translateY(0); }
+}
+
+@keyframes drawer-in {
+    from { transform: translateX(100%); }
+    to { transform: translateX(0); }
+}
+
+/* Desktop: a side panel */
+@media (min-width: 901px) {
+    .search-drawer {
+        left: auto;
+        top: 0;
+        width: 400px;
+        max-height: none;
+        border-radius: 0;
+        box-shadow: -8px 0 30px rgba(0, 0, 0, 0.6);
+    }
+}
+
+@media (min-width: 901px) and (prefers-reduced-motion: no-preference) {
+    .search-drawer { animation: drawer-in 0.22s ease-out; }
+}
--- a/web/app.js
+++ b/web/app.js
@@ -28,6 +28,15 @@
     bandSwitch: document.getElementById('bandSwitch'),
     bandOptions: Array.from(document.querySelectorAll('.band-option')),
     lcdStar: document.getElementById('lcdStar'),
+    boombox: document.querySelector('.boombox'),
+    searchBtn: document.getElementById('searchBtn'),
+    searchBackdrop: document.getElementById('searchBackdrop'),
+    searchDrawer: document.getElementById('searchDrawer'),
+    searchClose: document.getElementById('searchClose'),
+    searchInput: document.getElementById('searchInput'),
+    searchChips: document.getElementById('searchChips'),
+    searchStatus: document.getElementById('searchStatus'),
+    searchResults: document.getElementById('searchResults'),
     presetButtons: document.getElementById('presetButtons'),
     playBtn: document.getElementById('playBtn'),
     stopBtn: document.getElementById('stopBtn'),
@@ -967,6 +976,9 @@
     // Dial control
     setupDialControl();
 
+    // Search drawer
+    setupSearch();
+
     // Volume knobs
     setupVolumeKnob('main', elements.mainKnob);
     setupVolumeKnob('zone2', elements.zone2Knob);
@@ -977,6 +989,68 @@
     resizeVisCanvas();
 }
 
+// ---------------------------------------------------------------------------
+// Search drawer: open/close, focus handling
+// ---------------------------------------------------------------------------
+
+/** @returns {boolean} whether the search drawer is showing */
+function isSearchOpen() {
+    return !elements.searchDrawer.hidden;
+}
+
+/** Show the drawer, make the boom box inert behind it and focus the search box. */
+function openSearch() {
+    if (isSearchOpen()) return;
+    elements.searchDrawer.hidden = false;
+    elements.searchBackdrop.hidden = false;
+    elements.boombox.inert = true;
+    elements.searchBtn.setAttribute('aria-expanded', 'true');
+    elements.searchInput.focus();
+}
+
+/** Hide the drawer and hand focus back to the 🔍 button. */
+function closeSearch() {
+    if (!isSearchOpen()) return;
+    elements.searchDrawer.hidden = true;
+    elements.searchBackdrop.hidden = true;
+    elements.boombox.inert = false;
+    elements.searchBtn.setAttribute('aria-expanded', 'false');
+    elements.searchBtn.focus();
+}
+
+/** Keep Tab and Shift+Tab inside the open drawer. */
+function trapSearchFocus(event) {
+    const focusable = Array.from(
+        elements.searchDrawer.querySelectorAll('button:not(:disabled), input:not(:disabled)')
+    );
+    if (focusable.length === 0) return;
+    const first = focusable[0];
+    const last = focusable[focusable.length - 1];
+    if (event.shiftKey && document.activeElement === first) {
+        event.preventDefault();
+        last.focus();
+    } else if (!event.shiftKey && document.activeElement === last) {
+        event.preventDefault();
+        first.focus();
+    }
+}
+
+/** Wire the drawer: 🔍 opens it; Escape, the close button or the backdrop close it. */
+function setupSearch() {
+    elements.searchBtn.addEventListener('click', openSearch);
+    elements.searchClose.addEventListener('click', closeSearch);
+    elements.searchBackdrop.addEventListener('click', closeSearch);
+    document.addEventListener('keydown', (event) => {
+        if (!isSearchOpen()) return;
+        if (event.key === 'Escape') {
+            event.preventDefault();
+            closeSearch();
+        } else if (event.key === 'Tab') {
+            trapSearchFocus(event);
+        }
+    });
+}
+
 // Dial control setup
 function setupDialControl() {
     let startX = 0;
--- a/web/index.html
+++ b/web/index.html
@@ -98,6 +98,7 @@
                             <button type="button" class="band-option" role="radio" aria-checked="false" data-band="my" tabindex="-1">MY</button>
                         </div>
                     </div>
+                    <button type="button" class="search-btn" id="searchBtn" aria-label="Search stations" aria-haspopup="dialog" aria-expanded="false">&#128269;</button>
                 </div>
 
                 <!-- Preset buttons (first 7 stations of the selected band) -->
@@ -184,6 +185,19 @@
         </div>
     </div>
 
+    <!-- Station search: a bottom sheet on phones, a side panel on desktop -->
+    <div class="search-backdrop" id="searchBackdrop" hidden></div>
+    <section class="search-drawer" id="searchDrawer" role="dialog" aria-modal="true" aria-labelledby="searchTitle" hidden>
+        <header class="search-head">
+            <h2 class="search-title" id="searchTitle">Find stations</h2>
+            <button type="button" class="search-close" id="searchClose" aria-label="Close search">&times;</button>
+        </header>
+        <input type="search" class="search-input" id="searchInput" placeholder="Search by name" aria-label="Search stations by name" maxlength="80" autocomplete="off" spellcheck="false">
+        <div class="search-chips" id="searchChips" role="group" aria-label="Genres"></div>
+        <p class="search-status" id="searchStatus" role="status" aria-live="polite"></p>
+        <ul class="search-results" id="searchResults"></ul>
+    </section>
+
     <script src="app.js"></script>
 </body>
 </html>
```

- [ ] **Step 4: Run the check to see it pass, then check in the browser**

Run the block from Step 1 again. Expected: `PASS`.

Browser checklist (phone width 390 px and desktop width 1280 px):
1. A round 🔍 key sits beside the band switch.
2. Click it: on a phone a sheet slides up from the bottom (at most 80% of the screen height); on desktop a 400 px panel slides in from the right. The search box has focus and the boom box behind is dimmed.
3. Tab and Shift+Tab cycle only between the close button and the search box. Clicking the boom box behind does nothing.
4. Escape closes it and focus is back on 🔍. So do the close button and a click on the dimmed backdrop.
5. `aria-expanded` on 🔍 flips between `true` and `false` (DevTools, Elements).
6. With the OS "reduce motion" setting on, the drawer appears without sliding.
7. Reopen after closing: no stray text, nothing broken.

- [ ] **Step 5: Commit**

```bash
git add web/index.html web/app.css web/app.js
git commit -m "$(cat <<'EOF'
web: search button and modal drawer shell

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 15: Search behaviour

**Files:**
- Modify: `web/app.css` (chip and result rules, before the desktop media query at the end), `web/app.js` (search section between `trapSearchFocus` and `setupSearch`; `setupSearch`; `applyStations`)
- Test: grep check, `node --check web/app.js`, browser checklist

**Interfaces:**
- Consumes: `GET /api/search?q=&genre=` returning `{"results":[{"id","name","genre","country","bitrate","in_my"}]}` or 400/503 (Task 8); `playStation(id)` (existing, plays with the current zone selection exactly like a preset); `toggleMy`, `isInMy`, `applyStations` (Task 13); the drawer elements (Task 14); `debounce(func, delay)` (existing).
- Produces (JS):
  - `SEARCH_GENRES` (the 11 chips, same order as the server), `SEARCH_MIN_CHARS = 2`, `SEARCH_DEBOUNCE_MS = 300`
  - `setSearchStatus(text)`, `buildGenreChips()` (a chip toggles; text narrows a chip search), `createResultRow(result) -> HTMLLIElement` (name, `genre · country · N kbps`, ▶ and ★, all untrusted text through `textContent`), `refreshSearchStars()` (repaints ★ from `isInMy`, in place), `renderSearchResults(results)`, `runSearch()` (ignores stale responses via `searchRequestId`), `scheduleSearch` (debounced `runSearch`)
  - States: idle hint "Type a name or pick a genre.", "Searching…", "No stations found", "Search unavailable".
  - Result ★ state comes from the client's MY list, so it stays in step with the LCD star and other tabs.

- [ ] **Step 1: Write the failing check**

```bash
# web-check T15
fail=0
check() { grep -qF -- "$2" "$1" || { echo "FAIL: $1 lacks: $2"; fail=1; }; }
check web/app.js 'async function runSearch('
check web/app.js 'function createResultRow('
check web/app.js 'function buildGenreChips('
check web/app.js 'function refreshSearchStars('
check web/app.js 'const SEARCH_DEBOUNCE_MS = 300;'
check web/app.js "'Search unavailable'"
check web/app.js "'No stations found'"
check web/app.css '.search-row'
if grep -q 'innerHTML' web/app.js; then echo "FAIL: app.js uses innerHTML"; fail=1; fi
node --check web/app.js || fail=1
[ "$fail" = 0 ] && echo PASS
```

- [ ] **Step 2: Run the check to see it fail**

Expected: eight `FAIL: ... lacks: ...` lines and no `PASS` (the `innerHTML` guard stays quiet: nothing uses it yet and nothing may).

- [ ] **Step 3: Implement**

This diff also makes `applyStations` (Task 13) call the new `refreshSearchStars()`, so result stars follow MY changes made anywhere.

```diff
--- a/web/app.css
+++ b/web/app.css
@@ -1466,6 +1466,110 @@
     to { transform: translateX(0); }
 }
 
+.search-chips {
+    display: flex;
+    flex-wrap: wrap;
+    gap: 6px;
+}
+
+.search-chip {
+    padding: 5px 11px;
+    border-radius: 999px;
+    border: 1px solid var(--border);
+    background: var(--ink-0);
+    color: var(--text-2);
+    font-family: var(--mono);
+    font-size: 11px;
+    letter-spacing: 0.04em;
+    cursor: pointer;
+}
+
+.search-chip[aria-pressed="true"] {
+    border-color: var(--teal);
+    background: linear-gradient(to bottom, #6fe6df, var(--teal-deep));
+    color: var(--ink-0);
+}
+
+.search-chip:focus-visible,
+.search-play:focus-visible,
+.search-star:focus-visible {
+    outline: 3px solid var(--focus);
+    outline-offset: 2px;
+}
+
+.search-status {
+    min-height: 1.4em;
+    font-family: var(--mono);
+    font-size: 12px;
+    color: var(--text-3);
+}
+
+.search-results {
+    flex: 1;
+    min-height: 0;
+    display: flex;
+    flex-direction: column;
+    gap: 6px;
+    overflow-y: auto;
+    overscroll-behavior: contain;
+    list-style: none;
+}
+
+.search-row {
+    display: flex;
+    align-items: center;
+    gap: 8px;
+    padding: 8px 10px;
+    border: 1px solid var(--border);
+    border-radius: 8px;
+    background: var(--ink-1);
+}
+
+.search-info {
+    flex: 1;
+    min-width: 0;
+    display: flex;
+    flex-direction: column;
+    gap: 2px;
+}
+
+.search-name {
+    overflow: hidden;
+    text-overflow: ellipsis;
+    white-space: nowrap;
+    font-size: 14px;
+    font-weight: 600;
+}
+
+.search-meta {
+    overflow: hidden;
+    text-overflow: ellipsis;
+    white-space: nowrap;
+    font-family: var(--mono);
+    font-size: 11px;
+    color: var(--text-3);
+}
+
+.search-play,
+.search-star {
+    flex: none;
+    width: 40px;
+    height: 40px;
+    border: 1px solid #323a47;
+    border-radius: 50%;
+    background: linear-gradient(to bottom, #2b313c, #1a1e27);
+    color: var(--text-2);
+    font-size: 16px;
+    line-height: 1;
+    cursor: pointer;
+}
+
+.search-star[aria-pressed="true"] {
+    border-color: var(--teal);
+    color: var(--teal);
+    text-shadow: 0 0 6px rgba(77, 208, 200, 0.6);
+}
+
 /* Desktop: a side panel */
 @media (min-width: 901px) {
     .search-drawer {
--- a/web/app.js
+++ b/web/app.js
@@ -188,6 +188,7 @@
     renderPresetButtons();
     refreshDial();
     renderStar();
+    refreshSearchStars();
 }
 
 /** @returns {boolean} whether the station is in the MY list */
@@ -1035,8 +1036,142 @@
     }
 }
 
+// ---------------------------------------------------------------------------
+// Search drawer: genre chips, debounced search, result rows
+//
+// Everything Radio Browser sends us is untrusted text, so rows are built with
+// createElement and textContent only, never HTML strings.
+// ---------------------------------------------------------------------------
+const SEARCH_GENRES = ['Rock', 'Classic Rock', 'Alt', 'Jazz', 'Blues', 'Country', 'Oldies', 'Classical', 'Lofi', 'News', 'Talk'];
+const SEARCH_MIN_CHARS = 2;
+const SEARCH_DEBOUNCE_MS = 300;
+const SEARCH_IDLE_HINT = 'Type a name or pick a genre.';
+let searchGenre = null; // the selected chip, or null
+let searchRequestId = 0; // a response only counts while its request is the latest
+
+/** Show one line of status text under the chips (empty clears it). */
+function setSearchStatus(text) {
+    elements.searchStatus.textContent = text;
+}
+
+/** Build the genre chips: tapping one selects it, tapping it again clears it. */
+function buildGenreChips() {
+    const chips = SEARCH_GENRES.map((genre) => {
+        const chip = document.createElement('button');
+        chip.type = 'button';
+        chip.className = 'search-chip';
+        chip.textContent = genre;
+        chip.setAttribute('aria-pressed', 'false');
+        chip.addEventListener('click', () => {
+            searchGenre = searchGenre === genre ? null : genre;
+            for (const other of elements.searchChips.children) {
+                other.setAttribute('aria-pressed', String(other.textContent === searchGenre));
+            }
+            runSearch();
+        });
+        return chip;
+    });
+    elements.searchChips.replaceChildren(...chips);
+}
+
+/**
+ * One result row: name, a "genre · country · bitrate" line, then ▶ and ★.
+ * @param {{id: string, name: string, genre: string, country: string, bitrate: number}} result
+ * @returns {HTMLLIElement}
+ */
+function createResultRow(result) {
+    const row = document.createElement('li');
+    row.className = 'search-row';
+
+    const info = document.createElement('div');
+    info.className = 'search-info';
+    const name = document.createElement('span');
+    name.className = 'search-name';
+    name.textContent = result.name;
+    const meta = document.createElement('span');
+    meta.className = 'search-meta';
+    meta.textContent = [result.genre, result.country, result.bitrate ? `${result.bitrate} kbps` : '']
+        .filter(Boolean).join(' \u00b7 ');
+    info.append(name, meta);
+
+    const play = document.createElement('button');
+    play.type = 'button';
+    play.className = 'search-play';
+    play.textContent = '\u25b6';
+    play.setAttribute('aria-label', `Play ${result.name}`);
+    play.addEventListener('click', () => playStation(result.id));
+
+    const star = document.createElement('button');
+    star.type = 'button';
+    star.className = 'search-star';
+    star.textContent = '\u2605';
+    star.dataset.id = result.id;
+    star.setAttribute('aria-label', `Keep ${result.name} in MY`);
+    star.addEventListener('click', () => toggleMy(result.id, isInMy(result.id)));
+
+    row.append(info, play, star);
+    return row;
+}
+
+/** Repaint each result's ★ from the MY list, in place so keyboard focus stays put. */
+function refreshSearchStars() {
+    for (const star of elements.searchResults.querySelectorAll('.search-star')) {
+        star.setAttribute('aria-pressed', String(isInMy(star.dataset.id)));
+    }
+}
+
+/** Replace the result list. */
+function renderSearchResults(results) {
+    elements.searchResults.replaceChildren(...results.map(createResultRow));
+    refreshSearchStars();
+}
+
+/** Run the search for the box text and the selected chip, and show the outcome. */
+async function runSearch() {
+    const requestId = ++searchRequestId;
+    const query = elements.searchInput.value.trim();
+    const useQuery = query.length >= SEARCH_MIN_CHARS;
+    if (!useQuery && !searchGenre) {
+        renderSearchResults([]);
+        setSearchStatus(SEARCH_IDLE_HINT);
+        return;
+    }
+
+    const params = new URLSearchParams();
+    if (useQuery) params.set('q', query);
+    if (searchGenre) params.set('genre', searchGenre);
+    setSearchStatus('Searching\u2026');
+    elements.searchResults.setAttribute('aria-busy', 'true');
+
+    let results = null;
+    try {
+        const response = await fetch(`/api/search?${params}`);
+        if (response.ok) results = (await response.json()).results;
+    } catch (error) {
+        results = null;
+    }
+    if (requestId !== searchRequestId) return; // a newer search took over
+    elements.searchResults.removeAttribute('aria-busy');
+
+    if (!Array.isArray(results)) {
+        renderSearchResults([]);
+        setSearchStatus('Search unavailable');
+        return;
+    }
+    renderSearchResults(results);
+    setSearchStatus(results.length === 0 ? 'No stations found' : '');
+}
+
+const scheduleSearch = debounce(runSearch, SEARCH_DEBOUNCE_MS);
+
 /** Wire the drawer: 🔍 opens it; Escape, the close button or the backdrop close it. */
 function setupSearch() {
+    buildGenreChips();
+    setSearchStatus(SEARCH_IDLE_HINT);
+    elements.searchInput.addEventListener('input', () => {
+        searchRequestId++; // whatever is in flight is for text that is gone
+        scheduleSearch();
+    });
     elements.searchBtn.addEventListener('click', openSearch);
     elements.searchClose.addEventListener('click', closeSearch);
     elements.searchBackdrop.addEventListener('click', closeSearch);
```

- [ ] **Step 4: Run the check to see it pass, then check in the browser**

Run the block from Step 1 again. Expected: `PASS`.

Browser checklist (phone and desktop width):
1. Open the drawer: chips Rock, Classic Rock, Alt, Jazz, Blues, Country, Oldies, Classical, Lofi, News, Talk and the hint "Type a name or pick a genre.".
2. Tap Jazz: "Searching…" then up to 30 rows, each with name, genre, country and kbps, ▶ and ★. Tap Jazz again: it clears and the hint returns.
3. Type `smooth` (at least 2 characters): results appear about 300 ms after the last keystroke, and only one request is made per pause (DevTools, Network). With a chip selected the text narrows that chip's results. One character alone does nothing.
4. ▶ on a row starts that station through the receiver in the zones currently selected, and the LCD shows its real name.
5. ★ on a row fills; the LCD star fills when that station is playing; the station appears in the MY band; ★ again removes it. Toggle the LCD star and the row ★ follows.
6. Search for `zzzzqqqq`: "No stations found".
7. In DevTools, block the request URL `*/api/search*` and search again: "Search unavailable"; unblock it and the next search works.
8. Names with `<b>x</b>` style text show literally (the UI never uses `innerHTML`).
9. Escape closes the drawer, focus returns to 🔍, and a reopened drawer keeps its results.

- [ ] **Step 5: Commit**

```bash
git add web/app.css web/app.js
git commit -m "$(cat <<'EOF'
web: search drawer with genre chips, debounced search and play/keep per result

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
EOF
)"
```
