use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum YxcError {
    #[error("Receiver not responding — is it unplugged?")]
    Unreachable,
    #[error("YXC error: {0}")]
    ApiError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneStatus {
    pub power: String,
    pub input: String,
    pub mute: bool,
    pub volume: i32,
    pub actual_volume: Option<ActualVolume>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActualVolume {
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayInfo {
    pub playback: String,
}

#[async_trait::async_trait]
pub trait YxcClient: Send + Sync {
    async fn get_zone_status(&self, zone: &str) -> Result<ZoneStatus, YxcError>;
    async fn get_play_info(&self) -> Result<PlayInfo, YxcError>;
    async fn set_power(&self, zone: &str, on: bool) -> Result<(), YxcError>;
    async fn set_volume(&self, zone: &str, raw: i32) -> Result<(), YxcError>;
    async fn set_mute(&self, zone: &str, mute: bool) -> Result<(), YxcError>;
    async fn set_input(&self, zone: &str, input: &str) -> Result<(), YxcError>;
}

/// YXC `response_code` meaning the receiver is busy (e.g. just powered on).
const RESPONSE_CODE_BUSY: i64 = 5;

pub struct HttpYxcClient {
    base_url: String,
    client: reqwest::Client,
    busy_retry_interval: Duration,
    busy_retry_window: Duration,
}

impl HttpYxcClient {
    pub fn new(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url,
            client,
            busy_retry_interval: Duration::from_millis(400),
            busy_retry_window: Duration::from_secs(4),
        }
    }

    /// Override how setter calls retry a "busy" (`response_code` 5) reply.
    pub fn with_busy_retry(mut self, interval: Duration, window: Duration) -> Self {
        self.busy_retry_interval = interval;
        self.busy_retry_window = window;
        self
    }

    /// Issue a setter call. A "busy" reply is retried every `busy_retry_interval`
    /// until `busy_retry_window` has elapsed, then surfaces as an error.
    async fn command(&self, path: &str) -> Result<(), YxcError> {
        let started = std::time::Instant::now();
        loop {
            let data: serde_json::Value = self.request(path).await?;
            let busy = data.get("response_code").and_then(|v| v.as_i64()) == Some(RESPONSE_CODE_BUSY);
            if busy && started.elapsed() < self.busy_retry_window {
                tokio::time::sleep(self.busy_retry_interval).await;
                continue;
            }
            return self.check_response_code(&data);
        }
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, YxcError> {
        let url = format!("{}/YamahaExtendedControl/v1/{}", self.base_url, path);

        let response = self.client
            .get(&url)
            .send()
            .await
            .map_err(|_| YxcError::Unreachable)?;

        if !response.status().is_success() {
            return Err(YxcError::Unreachable);
        }

        let data: T = response
            .json()
            .await
            .map_err(|e| YxcError::ApiError(format!("Failed to parse response: {}", e)))?;

        Ok(data)
    }

    fn check_response_code(&self, data: &serde_json::Value) -> Result<(), YxcError> {
        if let Some(code) = data.get("response_code").and_then(|v| v.as_i64()) {
            if code != 0 {
                return Err(YxcError::ApiError(format!("YXC response_code: {}", code)));
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl YxcClient for HttpYxcClient {
    async fn get_zone_status(&self, zone: &str) -> Result<ZoneStatus, YxcError> {
        let data: serde_json::Value = self.request(&format!("{}/getStatus", zone)).await?;
        self.check_response_code(&data)?;

        serde_json::from_value(data)
            .map_err(|e| YxcError::ApiError(format!("Failed to parse zone status: {}", e)))
    }

    async fn get_play_info(&self) -> Result<PlayInfo, YxcError> {
        let data: serde_json::Value = self.request("netusb/getPlayInfo").await?;
        self.check_response_code(&data)?;

        serde_json::from_value(data)
            .map_err(|e| YxcError::ApiError(format!("Failed to parse play info: {}", e)))
    }

    async fn set_power(&self, zone: &str, on: bool) -> Result<(), YxcError> {
        let power = if on { "on" } else { "standby" };
        self.command(&format!("{}/setPower?power={}", zone, power)).await
    }

    async fn set_volume(&self, zone: &str, raw: i32) -> Result<(), YxcError> {
        self.command(&format!("{}/setVolume?volume={}", zone, raw)).await
    }

    async fn set_mute(&self, zone: &str, mute: bool) -> Result<(), YxcError> {
        self.command(&format!("{}/setMute?enable={}", zone, mute)).await
    }

    async fn set_input(&self, zone: &str, input: &str) -> Result<(), YxcError> {
        self.command(&format!("{}/setInput?input={}", zone, input)).await
    }
}
