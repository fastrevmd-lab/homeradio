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
