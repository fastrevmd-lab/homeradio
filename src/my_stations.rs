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
    /// or unreadable one (including one that is not UTF-8) is preserved, logged
    /// and also loads as empty, never an error.
    pub fn load(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(contents) => Self::parse(&contents, &path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                warn!("Could not read {}: {}; starting with an empty MY list", path.display(), error);
                Self::preserve_corrupt_file(&path);
                Vec::new()
            }
        };
        info!("Loaded {} MY stations", entries.len());
        Self { path, entries }
    }

    /// Parses the file entry by entry, so one bad entry costs only itself. A
    /// file that is not a JSON array at all is renamed to `<name>.corrupt`
    /// before loading as empty, so the next save cannot overwrite the only copy.
    fn parse(contents: &str, path: &Path) -> Vec<MyEntry> {
        let raw_entries: Vec<serde_json::Value> = match serde_json::from_str(contents) {
            Ok(raw_entries) => raw_entries,
            Err(error) => {
                warn!("{} is corrupt ({}); starting with an empty MY list", path.display(), error);
                Self::preserve_corrupt_file(path);
                return Vec::new();
            }
        };
        let mut entries: Vec<MyEntry> = Vec::new();
        for raw_entry in raw_entries {
            let entry: MyEntry = match serde_json::from_value(raw_entry) {
                Ok(entry) => entry,
                Err(error) => {
                    warn!("Dropping unreadable MY entry: {}", error);
                    continue;
                }
            };
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

    /// Renames the unusable file at `path` to the first free `<name>.corrupt`,
    /// `<name>.corrupt.1`, `<name>.corrupt.2`, ... so an earlier preserved copy
    /// is never overwritten.
    fn preserve_corrupt_file(path: &Path) {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "my-stations.json".to_string());
        let corrupt_path = (0u32..)
            .map(|attempt| match attempt {
                0 => path.with_file_name(format!("{file_name}.corrupt")),
                _ => path.with_file_name(format!("{file_name}.corrupt.{attempt}")),
            })
            .find(|candidate| std::fs::symlink_metadata(candidate).is_err())
            .expect("an unbounded range always yields a free name");
        match std::fs::rename(path, &corrupt_path) {
            Ok(()) => warn!("Moved the corrupt MY file to {}", corrupt_path.display()),
            Err(error) => warn!("Could not preserve the corrupt MY file: {}", error),
        }
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

    #[test]
    fn one_bad_entry_does_not_discard_the_good_ones() {
        let dir = tempfile::tempdir().unwrap();
        let json = serde_json::json!([
            {"ref": "big100"},
            {"unexpected": "shape"},
            42,
            {"id": format!("rb-{UUID_A}"), "name": "Ok", "short": "Ok", "genre": "", "url": "http://ok.example/s"}
        ]);
        std::fs::write(dir.path().join("my-stations.json"), json.to_string()).unwrap();
        let ids: Vec<String> = store_in(&dir).entries().iter().map(|e| e.id().to_string()).collect();
        assert_eq!(ids, vec!["big100".to_string(), format!("rb-{UUID_A}")]);
        assert!(!dir.path().join("my-stations.json.corrupt").exists());
    }

    #[tokio::test]
    async fn a_corrupt_file_is_preserved_so_the_next_save_cannot_destroy_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my-stations.json"), "{ this is not json").unwrap();
        let mut store = store_in(&dir);
        assert!(store.entries().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("my-stations.json.corrupt")).unwrap(),
            "{ this is not json"
        );
        store.add(reference("big100")).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("my-stations.json.corrupt")).unwrap(),
            "{ this is not json"
        );
        assert_eq!(store_in(&dir).entries(), &[reference("big100")]);
    }

    #[tokio::test]
    async fn a_non_utf8_file_is_preserved_so_the_next_save_cannot_destroy_it() {
        let dir = tempfile::tempdir().unwrap();
        let latin1_bytes: &[u8] = b"[{\"ref\": \"caf\xe9\"}]";
        std::fs::write(dir.path().join("my-stations.json"), latin1_bytes).unwrap();
        let mut store = store_in(&dir);
        assert!(store.entries().is_empty());
        assert_eq!(std::fs::read(dir.path().join("my-stations.json.corrupt")).unwrap(), latin1_bytes);
        store.add(reference("big100")).await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("my-stations.json.corrupt")).unwrap(), latin1_bytes);
        assert_eq!(store_in(&dir).entries(), &[reference("big100")]);
    }

    #[test]
    fn a_second_corruption_does_not_overwrite_the_first_preserved_copy() {
        let dir = tempfile::tempdir().unwrap();
        let live_path = dir.path().join("my-stations.json");
        for (round, contents) in ["first {", "second {", "third {"].into_iter().enumerate() {
            std::fs::write(&live_path, contents).unwrap();
            assert!(store_in(&dir).entries().is_empty(), "round {round}");
        }
        let preserved = |suffix: &str| std::fs::read_to_string(dir.path().join(format!("my-stations.json{suffix}"))).unwrap();
        assert_eq!(preserved(".corrupt"), "first {");
        assert_eq!(preserved(".corrupt.1"), "second {");
        assert_eq!(preserved(".corrupt.2"), "third {");
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
