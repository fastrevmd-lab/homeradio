use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
    pub state: String,
    /// The live stream URL (`track.path`, then `logical_track.path`).
    pub url: Option<String>,
    /// The registry identity of the stream (`logical_track.path`, then
    /// `track.path`). Use this, not `url`, to match a station: `track.*` can
    /// carry a redirected or resolved URL.
    pub station_url: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteStateResponse {
    ok: bool,
    snapshot: Option<Snapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    state: String,
    track: Option<Track>,
    logical_track: Option<LogicalTrack>,
}

/// The live track object. For radio streams `stream_title` / `title` carry the
/// ICY/iHeart now-playing text, while `logical_track.title` is just the station id.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Track {
    title: Option<String>,
    path: Option<String>,
    stream_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogicalTrack {
    title: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventMessage {
    event: String,
    data: Snapshot,
}

#[async_trait::async_trait]
pub trait Player: Send + Sync {
    async fn play(&self, url: &str) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn state(&self) -> PlayerState;
    fn subscribe(&self) -> watch::Receiver<PlayerState>;
}

pub struct CliampPlayer {
    cliamp_bin: String,
    state_tx: watch::Sender<PlayerState>,
    state_rx: watch::Receiver<PlayerState>,
}

impl CliampPlayer {
    pub fn new(cliamp_bin: String) -> Self {
        let (state_tx, state_rx) = watch::channel(PlayerState {
            state: "unknown".to_string(),
            url: None,
            station_url: None,
            title: None,
        });

        Self {
            cliamp_bin,
            state_tx,
            state_rx,
        }
    }

    /// Get a watch receiver for player state changes
    pub fn subscribe(&self) -> watch::Receiver<PlayerState> {
        self.state_rx.clone()
    }

    /// Start the events task that monitors cliamp state
    pub fn start_events_task(&self) {
        let cliamp_bin = self.cliamp_bin.clone();
        let state_tx = self.state_tx.clone();

        tokio::spawn(async move {
            Self::events_task(cliamp_bin, state_tx).await;
        });
    }

    async fn events_task(cliamp_bin: String, state_tx: watch::Sender<PlayerState>) {
        let mut backoff = 1;

        loop {
            info!("Starting cliamp events stream");

            match Command::new(&cliamp_bin)
                .args(["remote", "events", "runtime.state"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(stdout) = child.stdout.take() {
                        let reader = BufReader::new(stdout);
                        let mut lines = reader.lines();

                        while let Ok(Some(line)) = lines.next_line().await {
                            if let Ok(event) = serde_json::from_str::<EventMessage>(&line) {
                                let state = Self::parse_state(&event.data);
                                state_tx.send(state).ok();
                            }
                        }
                    }

                    // Process exited - set state to unknown
                    let _ = child.wait().await;
                    state_tx.send(PlayerState {
                        state: "unknown".to_string(),
                        url: None,
                        station_url: None,
                        title: None,
                    }).ok();
                    warn!("cliamp events stream ended, restarting in {} seconds", backoff);
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
                Err(e) => {
                    error!("Failed to spawn cliamp events: {}", e);
                    state_tx.send(PlayerState {
                        state: "unknown".to_string(),
                        url: None,
                        station_url: None,
                        title: None,
                    }).ok();
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    }

    /// Convert a cliamp snapshot into a `PlayerState`.
    ///
    /// Title priority: `track.stream_title`, `track.title`, `logical_track.title`.
    /// URL priority: `track.path`, then `logical_track.path`; the station identity
    /// (`station_url`) prefers `logical_track.path`. A stopped player
    /// reports no title so stale metadata is never surfaced.
    fn parse_state(snapshot: &Snapshot) -> PlayerState {
        let state = snapshot.state.clone();
        let non_empty = |value: &Option<String>| value.clone().filter(|v| !v.trim().is_empty());

        let url = snapshot
            .track
            .as_ref()
            .and_then(|t| non_empty(&t.path))
            .or_else(|| snapshot.logical_track.as_ref().and_then(|t| non_empty(&t.path)));

        let station_url = snapshot
            .logical_track
            .as_ref()
            .and_then(|t| non_empty(&t.path))
            .or_else(|| snapshot.track.as_ref().and_then(|t| non_empty(&t.path)));

        let title = if state == "stopped" {
            None
        } else {
            snapshot
                .track
                .as_ref()
                .and_then(|t| non_empty(&t.stream_title))
                .or_else(|| snapshot.track.as_ref().and_then(|t| non_empty(&t.title)))
                .or_else(|| snapshot.logical_track.as_ref().and_then(|t| non_empty(&t.title)))
        };

        PlayerState {
            state,
            url,
            station_url,
            title,
        }
    }

    async fn get_current_state(&self) -> PlayerState {
        match Command::new(&self.cliamp_bin)
            .args(["remote", "state"])
            .output()
            .await
        {
            Ok(output) if output.status.success() => {
                if let Ok(response) = serde_json::from_slice::<RemoteStateResponse>(&output.stdout)
                {
                    if let Some(snapshot) = response.snapshot {
                        return Self::parse_state(&snapshot);
                    }
                }
            }
            _ => {}
        }

        PlayerState {
            state: "unknown".to_string(),
            url: None,
            station_url: None,
            title: None,
        }
    }
}

#[async_trait::async_trait]
impl Player for CliampPlayer {
    async fn play(&self, url: &str) -> anyhow::Result<()> {
        let cliamp_url = format!("cliamp://play?url={}", percent_encoding::utf8_percent_encode(url, percent_encoding::NON_ALPHANUMERIC));

        let output = Command::new(&self.cliamp_bin)
            .args(["open", &cliamp_url])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("cliamp open failed: {}", stderr);
        }

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let output = Command::new(&self.cliamp_bin)
            .args(["stop"])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("cliamp stop failed: {}", stderr);
        }

        Ok(())
    }

    async fn state(&self) -> PlayerState {
        // First try the watch receiver (updated by events)
        let watched = self.state_rx.borrow().clone();
        if watched.state != "unknown" {
            return watched;
        }

        // Fall back to polling
        self.get_current_state().await
    }

    fn subscribe(&self) -> watch::Receiver<PlayerState> {
        self.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(json: &str) -> PlayerState {
        let snapshot: Snapshot = serde_json::from_str(json).unwrap();
        CliampPlayer::parse_state(&snapshot)
    }

    #[test]
    fn stream_title_wins_and_track_path_is_used() {
        let parsed = snapshot(
            r#"{"state":"playing","track":{"title":"T","path":"https://s/zc2505","stream_title":"Artist - Song"},"logical_track":{"title":"zc2505","path":"https://other/x"}}"#,
        );
        assert_eq!(parsed.title.as_deref(), Some("Artist - Song"));
        assert_eq!(parsed.url.as_deref(), Some("https://s/zc2505"));
    }

    #[test]
    fn station_identity_prefers_logical_track_path() {
        let parsed = snapshot(
            r#"{"state":"playing","track":{"title":"T","path":"https://resolved/redirect"},"logical_track":{"title":"zc2505","path":"https://s/zc2505"}}"#,
        );
        assert_eq!(parsed.station_url.as_deref(), Some("https://s/zc2505"));
        assert_eq!(parsed.url.as_deref(), Some("https://resolved/redirect"));

        let parsed = snapshot(r#"{"state":"playing","track":{"title":"T","path":"https://s/a"}}"#);
        assert_eq!(parsed.station_url.as_deref(), Some("https://s/a"));
    }

    #[test]
    fn falls_back_to_track_title_then_logical_track() {
        let parsed = snapshot(
            r#"{"state":"playing","track":{"title":"T","path":null},"logical_track":{"title":"zc2505","path":"https://s/zc2505"}}"#,
        );
        assert_eq!(parsed.title.as_deref(), Some("T"));
        assert_eq!(parsed.url.as_deref(), Some("https://s/zc2505"));

        let parsed = snapshot(
            r#"{"state":"playing","logical_track":{"title":"zc2505","path":"https://s/zc2505"}}"#,
        );
        assert_eq!(parsed.title.as_deref(), Some("zc2505"));
    }

    #[test]
    fn stopped_reports_no_title() {
        let parsed = snapshot(
            r#"{"state":"stopped","track":{"title":"Old","path":"https://s/a","stream_title":"Old"}}"#,
        );
        assert_eq!(parsed.title, None);
        assert_eq!(parsed.url.as_deref(), Some("https://s/a"));
    }
}
