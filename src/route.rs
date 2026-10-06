//! The audio route between cliamp and the receiver.
//!
//! PipeWire's RAOP sink holds its RTSP session for as long as it is loaded, so
//! the receiver is never released after a stop and never re-handshakes if it
//! drops the session. The sink therefore lives in its own systemd user unit
//! (`raop-sink.service`) that radio-web starts on play, stops on stop and
//! restarts when the receiver stops pulling audio. A silent fallback sink
//! always exists, so cliamp never blocks while the unit is down.

use anyhow::{bail, Context};
use std::time::Duration;
use tokio::process::Command;

/// How long a `systemctl` invocation may take before it counts as failed.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// Controls the AirPlay sink that carries audio to the receiver.
#[async_trait::async_trait]
pub trait AudioRoute: Send + Sync {
    /// Bring the route up. A no-op when it is already active.
    async fn connect(&self) -> anyhow::Result<()>;
    /// Tear the route down, releasing the receiver.
    async fn disconnect(&self) -> anyhow::Result<()>;
    /// Tear the route down and bring it back up, forcing a fresh handshake.
    async fn reconnect(&self) -> anyhow::Result<()>;
    /// Whether the route is currently up.
    async fn is_active(&self) -> bool;
}

/// `AudioRoute` backed by a systemd user unit. radio-web runs inside the same
/// user manager, so plain `systemctl --user` reaches it.
pub struct SystemdAudioRoute {
    unit: String,
}

impl SystemdAudioRoute {
    /// Create a route that drives the systemd user unit `unit`.
    pub fn new(unit: String) -> Self {
        Self { unit }
    }

    /// Run `systemctl --user <verb> <unit>`, failing on a non-zero exit or after
    /// the timeout. Returns the exit status success flag for `is-active`.
    async fn systemctl(&self, verb: &str) -> anyhow::Result<std::process::Output> {
        let child = Command::new("systemctl")
            .args(["--user", verb, &self.unit])
            .kill_on_drop(true)
            .output();
        tokio::time::timeout(SYSTEMCTL_TIMEOUT, child)
            .await
            .with_context(|| format!("systemctl --user {verb} {} timed out", self.unit))?
            .with_context(|| format!("failed to run systemctl --user {verb} {}", self.unit))
    }

    async fn run(&self, verb: &str) -> anyhow::Result<()> {
        let output = self.systemctl(verb).await?;
        if !output.status.success() {
            bail!(
                "systemctl --user {verb} {} failed: {}",
                self.unit,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl AudioRoute for SystemdAudioRoute {
    async fn connect(&self) -> anyhow::Result<()> {
        // `start` on an active unit is a no-op, so this is idempotent.
        self.run("start").await
    }

    async fn disconnect(&self) -> anyhow::Result<()> {
        self.run("stop").await
    }

    async fn reconnect(&self) -> anyhow::Result<()> {
        self.run("restart").await
    }

    async fn is_active(&self) -> bool {
        self.systemctl("is-active")
            .await
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}
