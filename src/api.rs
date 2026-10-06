#![allow(clippy::result_large_err)]

use crate::cliamp::Player;
use crate::config::Config;
use crate::policy::{self, ZoneSelection, ZoneSnapshot};
use crate::route::AudioRoute;
use crate::state::{State, StateManager, ZoneLive, ZoneOverride};
use crate::stations::StationManager;
use crate::vis::VisHub;
use crate::volume;
use crate::yxc::{YxcClient, YxcError};
use axum::{
    extract::{Path, State as AxumState},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive},
        IntoResponse, Response, Sse,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, Instant};
use tokio_stream::wrappers::{ReceiverStream, WatchStream};
use tokio_stream::{Stream, StreamExt};
use tracing::{error, warn};

/// Timing of the post-play zone policy. Production uses `Default`; tests shrink
/// it so the background task finishes in milliseconds.
#[derive(Debug, Clone, Copy)]
pub struct PolicyTiming {
    /// Interval between polls while waiting for AirPlay to grab the zones.
    pub poll_interval: Duration,
    /// How long to wait for AirPlay before applying the policy anyway.
    pub takeover_timeout: Duration,
    /// Delay before the second (re-apply) pass.
    pub reapply_delay: Duration,
    /// Interval between polls while waiting for a zone to report `power == on`.
    pub power_poll_interval: Duration,
    /// How long to wait for a zone to report `power == on` after powering it up.
    pub power_wait_timeout: Duration,
    /// How long after setting a start volume to re-read it and undo any bump the
    /// receiver applied on its own.
    pub volume_verify_delay: Duration,
    /// How often the watchdog checks that the receiver is pulling audio.
    pub watchdog_interval: Duration,
    /// How long the stall (playing, radio zone up, receiver not playing) must
    /// have held before the watchdog reconnects.
    pub watchdog_hold: Duration,
    /// Minimum time since the last play/reconnect before the watchdog may act,
    /// so a fresh handshake is given time to finish.
    pub watchdog_settle: Duration,
    /// Minimum time between two watchdog reconnects.
    pub reconnect_min_interval: Duration,
    /// How long after a zone powers on mid-playback to wait for the receiver to
    /// start playing before forcing a reconnect.
    pub regrab_wait: Duration,
}

impl Default for PolicyTiming {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(500),
            takeover_timeout: Duration::from_secs(10),
            reapply_delay: Duration::from_secs(2),
            power_poll_interval: Duration::from_millis(300),
            power_wait_timeout: Duration::from_secs(3),
            volume_verify_delay: Duration::from_millis(1500),
            watchdog_interval: Duration::from_secs(3),
            watchdog_hold: Duration::from_secs(9),
            watchdog_settle: Duration::from_secs(15),
            reconnect_min_interval: Duration::from_secs(30),
            regrab_wait: Duration::from_secs(3),
        }
    }
}

/// What the watchdog and re-grab logic need to remember about the AirPlay route.
#[derive(Debug, Default)]
pub struct RouteTracking {
    /// The zone selection from the last play (kept current by power toggles).
    pub selection: Option<ZoneSelection>,
    /// When audio last started or the route was last reconnected.
    pub last_activity: Option<Instant>,
    /// When the watchdog (or a re-grab) last reconnected the route.
    pub last_reconnect: Option<Instant>,
}

#[derive(Clone)]
pub struct AppState {
    pub yxc: Arc<dyn YxcClient>,
    pub player: Arc<dyn Player>,
    pub stations: Arc<RwLock<StationManager>>,
    pub state_manager: Arc<StateManager>,
    pub config: Config,
    pub play_mutex: Arc<Mutex<()>>,
    pub policy_timing: PolicyTiming,
    /// Bumped on every play, stop and power call. A background policy task exits
    /// as soon as the value it started with is no longer current.
    pub generation: Arc<AtomicU64>,
    /// The AirPlay sink: connected on play, disconnected on stop.
    pub route: Arc<dyn AudioRoute>,
    pub route_tracking: Arc<StdMutex<RouteTracking>>,
    /// Number of zone-policy background tasks that have finished (by any exit
    /// path). Lets tests wait for the whole policy, second pass included.
    pub policy_completions: Arc<AtomicU64>,
    /// Shared spectrum feed behind `GET /api/vis`.
    pub vis: Arc<VisHub>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    detail: String,
}

#[derive(Debug, Deserialize)]
struct PlayRequest {
    station: String,
    zones: Option<HashMap<String, bool>>,
}

#[derive(Debug, Deserialize)]
struct PowerRequest {
    on: bool,
}

#[derive(Debug, Deserialize)]
struct VolumeRequest {
    db: Option<f64>,
    step: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct MuteRequest {
    mute: bool,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    cliamp: ComponentHealth,
    receiver: ComponentHealth,
    airplay: AirplayHealth,
}

/// Whether the AirPlay sink unit is up. Informational: does not affect `ok`.
#[derive(Debug, Serialize)]
struct AirplayHealth {
    connected: bool,
}

#[derive(Debug, Serialize)]
struct ComponentHealth {
    ok: bool,
    error: Option<String>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/stations", get(get_stations))
        .route("/api/play", post(play_station))
        .route("/api/stop", post(stop_player))
        .route("/api/power", post(set_master_power))
        .route("/api/zone/{zone}/power", post(set_zone_power))
        .route("/api/zone/{zone}/volume", post(set_zone_volume))
        .route("/api/zone/{zone}/mute", post(set_zone_mute))
        .route("/api/events", get(sse_handler))
        .route("/api/vis", get(vis_handler))
        .route("/healthz", get(health_check))
        .with_state(state)
}

async fn get_state(AxumState(state): AxumState<AppState>) -> Json<State> {
    Json(state.state_manager.get_state().await)
}

async fn get_stations(
    AxumState(state): AxumState<AppState>,
) -> Json<crate::stations::StationRegistry> {
    let stations = state.stations.read().await;
    Json(stations.get_registry())
}

async fn play_station(
    AxumState(state): AxumState<AppState>,
    Json(req): Json<PlayRequest>,
) -> Result<Json<State>, Response> {
    // Serialize play operations
    let _guard = state.play_mutex.lock().await;

    let stations = state.stations.read().await;
    let station_url = stations
        .get_station_url(&req.station)
        .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "unknown_station", "Station not found"))?
        .to_string();
    drop(stations);

    // Snapshot LIVE receiver state (the cache can be seconds old, and a
    // TV that just switched to hdmi1 must be seen as such).
    // Pending writes (power/input) are overlaid, since the receiver's readback lags.
    let snapshots = live_snapshots(&state.yxc).await?;
    let snapshots = overlay_pending(&state.state_manager, snapshots);

    // Determine selected zones
    let selected = determine_selection(&req.zones, &snapshots);
    if !selected.main && !selected.zone2 {
        return Err(error_response(
            StatusCode::CONFLICT,
            "no_zone",
            "Turn on Media Room or Upstairs first",
        ));
    }

    // Capture player state before play
    let player_state_before = state.player.state().await;

    // The play is now committed: only here is the running policy task superseded,
    // so a rejected play (unknown station, no zone, receiver down) leaves it alone.
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;

    // Step 3: Bring the AirPlay sink up. This MUST come after the snapshot: the
    // receiver grabs both zones as soon as the sink connects.
    state.route.connect().await.map_err(|e| {
        error!("Failed to connect AirPlay sink: {}", e);
        error_response(
            StatusCode::BAD_GATEWAY,
            "audio_route_error",
            &format!("Could not start the AirPlay sink: {}", e),
        )
    })?;

    // Step 4: Play
    state.player.play(&station_url).await.map_err(|e| {
        error!("Failed to play station: {}", e);
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "player_error",
            &format!("Failed to play: {}", e),
        )
    })?;

    state.state_manager.set_last_played_station(req.station.clone()).await;
    {
        let mut tracking = state.route_tracking.lock().unwrap();
        tracking.selection = Some(selected.clone());
        tracking.last_activity = Some(Instant::now());
    }

    // Spawn background task for the zone policy
    let state_clone = state.clone();
    let selected_clone = selected.clone();
    let snapshots_clone = snapshots.clone();
    tokio::spawn(async move {
        apply_zone_policy(
            state_clone,
            selected_clone,
            snapshots_clone,
            player_state_before.state != "playing",
            generation,
        )
        .await;
    });

    // Return immediately
    state.state_manager.refresh().await;
    Ok(Json(state.state_manager.get_state().await))
}

async fn stop_player(AxumState(state): AxumState<AppState>) -> Result<Json<State>, Response> {
    // Serialize with play and power calls: each reads, then bumps the generation.
    let _guard = state.play_mutex.lock().await;
    state.generation.fetch_add(1, Ordering::SeqCst);
    state.player.stop().await.map_err(|e| {
        error!("Failed to stop player: {}", e);
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "player_error",
            &format!("Failed to stop: {}", e),
        )
    })?;

    // Release the receiver: dropping the sink ends the RTSP session.
    state.route.disconnect().await.map_err(|e| {
        error!("Failed to disconnect AirPlay sink: {}", e);
        error_response(
            StatusCode::BAD_GATEWAY,
            "audio_route_error",
            &format!("Could not stop the AirPlay sink: {}", e),
        )
    })?;

    state.state_manager.refresh().await;
    Ok(Json(state.state_manager.get_state().await))
}

/// Master power switch for the whole receiver.
///
/// Off stops the radio and puts BOTH zones in standby, even one live on another
/// source (a TV on hdmi1): unlike the per-zone toggle, this is the off switch for
/// the whole setup. On only wakes the Media Room, leaving input, volume and the
/// remembered selection alone.
async fn set_master_power(
    AxumState(state): AxumState<AppState>,
    Json(req): Json<PowerRequest>,
) -> Result<Json<State>, Response> {
    // Serialize with play, stop and zone toggles, and supersede any running
    // policy task so it cannot undo the power change.
    let _guard = state.play_mutex.lock().await;
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;

    if !req.on {
        // Try every step even if one fails, so a hiccup on one part never leaves
        // the rest running; report the first failure once all have been tried.
        let mut first_error: Option<Response> = None;
        if let Err(e) = state.player.stop().await {
            error!("Failed to stop player during master power off: {}", e);
            first_error.get_or_insert_with(|| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "player_error",
                    &format!("Failed to stop: {}", e),
                )
            });
        }
        if let Err(e) = state.route.disconnect().await {
            error!("Failed to disconnect AirPlay sink during master power off: {}", e);
            first_error.get_or_insert_with(|| {
                error_response(
                    StatusCode::BAD_GATEWAY,
                    "audio_route_error",
                    &format!("Could not stop the AirPlay sink: {}", e),
                )
            });
        }
        for zone in ["main", "zone2"] {
            if let Err(response) = power_off_zone(&state, zone).await {
                first_error.get_or_insert(response);
            }
        }
        if let Some(response) = first_error {
            state.state_manager.refresh().await;
            return Err(response);
        }
    } else {
        // Always send: the readback lags, so a main that still reads "on" may be
        // a standby from a moment ago that has not landed yet.
        let current_status = state.yxc.get_zone_status("main").await.map_err(yxc_error_to_response)?;
        state.yxc.set_power("main", true).await.map_err(yxc_error_to_response)?;
        let woke_into_radio = current_status.input == "airplay";
        let prior = ZoneLive {
            power: current_status.power,
            input: current_status.input,
        };
        state.state_manager.note_zone_write(
            "main",
            ZoneOverride {
                power: Some("on".to_string()),
                ..ZoneOverride::default()
            },
            Some(&prior),
        );

        // Like the zone toggle: if the radio is still playing and main woke up on
        // AirPlay, make sure the receiver actually picks the stream up. A main
        // waking on another input (the TV) is left alone.
        if woke_into_radio && state.player.state().await.state == "playing" {
            let regrab_state = state.clone();
            tokio::spawn(async move {
                regrab_after_power_on(regrab_state, "main".to_string(), generation).await;
            });
        }
    }

    state.state_manager.refresh().await;
    Ok(Json(state.state_manager.get_state().await))
}

/// Put `zone` in standby whatever input it is on, and forget it in the
/// remembered selection. The command is sent even if the zone already reads
/// standby: the readback lags, so that may be a power-on that has not landed yet.
async fn power_off_zone(state: &AppState, zone: &str) -> Result<(), Response> {
    // The status read only feeds the override bookkeeping: a failed read must not
    // stop the standby command from being tried.
    let prior = match state.yxc.get_zone_status(zone).await {
        Ok(status) => Some(ZoneLive {
            power: status.power,
            input: status.input,
        }),
        Err(e) => {
            warn!("Could not read {} before powering it off: {}", zone, e);
            None
        }
    };
    state.yxc.set_power(zone, false).await.map_err(yxc_error_to_response)?;
    set_selected(state, zone, false);
    state.state_manager.note_zone_write(
        zone,
        ZoneOverride {
            power: Some("standby".to_string()),
            ..ZoneOverride::default()
        },
        prior.as_ref(),
    );
    Ok(())
}

async fn set_zone_power(
    AxumState(state): AxumState<AppState>,
    Path(zone): Path<String>,
    Json(req): Json<PowerRequest>,
) -> Result<Json<State>, Response> {
    validate_zone(&zone)?;
    // Serialize with play and stop. A play that snapshotted the receiver before
    // this call could otherwise bump the generation afterwards and let its stale
    // policy undo the toggle.
    let _guard = state.play_mutex.lock().await;
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;

    if req.on {
        // Read fresh status to check current state
        let current_status = state.yxc.get_zone_status(&zone).await.map_err(yxc_error_to_response)?;

        // If already on+airplay, it's a no-op
        if current_status.power == "on" && current_status.input == "airplay" {
            state.state_manager.refresh().await;
            return Ok(Json(state.state_manager.get_state().await));
        }

        // Was in standby or on different input
        let was_standby = current_status.power == "standby";

        // Turning on: set power, wait for the zone to actually report "on" (the
        // receiver rejects or overrides settings while it is still waking), then
        // set the input.
        state.yxc.set_power(&zone, true).await.map_err(yxc_error_to_response)?;
        if was_standby {
            wait_for_power_on(&state, &zone).await;
        }
        state.yxc.set_input(&zone, "airplay").await.map_err(yxc_error_to_response)?;

        let mut written = ZoneOverride {
            power: Some("on".to_string()),
            input: Some("airplay".to_string()),
            ..ZoneOverride::default()
        };

        // Set default start volume ONLY if zone was in standby
        if was_standby {
            if let Some(start_raw) = start_volume_raw(&state.config, &zone) {
                state.yxc.set_volume(&zone, start_raw).await.map_err(yxc_error_to_response)?;
                written.volume = Some(start_raw);

                // The receiver may force its own volume shortly after power-on;
                // verify in the background and put the start volume back.
                let verify_state = state.clone();
                let verify_zone = zone.clone();
                tokio::spawn(async move {
                    verify_start_volume(verify_state, verify_zone, start_raw, generation).await;
                });
            }
        }
        let prior = ZoneLive {
            power: current_status.power.clone(),
            input: current_status.input.clone(),
        };
        state.state_manager.note_zone_write(&zone, written, Some(&prior));

        // Record the zone as part of the selection, and if audio is playing make
        // sure the receiver actually picks the stream up on the newly active zone.
        set_selected(&state, &zone, true);
        if state.player.state().await.state == "playing" {
            let regrab_state = state.clone();
            let regrab_zone = zone.clone();
            tokio::spawn(async move {
                regrab_after_power_on(regrab_state, regrab_zone, generation).await;
            });
        }
    } else {
        // Turning off: only an airplay zone is ours to power down. Read the live
        // input; a TV (or anything else) on another input is left alone.
        let current_status = state.yxc.get_zone_status(&zone).await.map_err(yxc_error_to_response)?;
        let live = ZoneLive {
            power: current_status.power,
            input: current_status.input,
        };
        // A pending input write (e.g. from a power-on a moment ago) beats the
        // lagging readback, but only while the readback has not moved on to
        // something else.
        let pending = state.state_manager.pending_override_against(&zone, &live);
        let input = pending.input.unwrap_or_else(|| live.input.clone());
        // Never power down a zone that is live on another source, whatever an
        // override says: that is someone watching TV.
        if input == "airplay" && !is_live_on_other_source(&live) {
            state.yxc.set_power(&zone, false).await.map_err(yxc_error_to_response)?;
            set_selected(&state, &zone, false);
            state.state_manager.note_zone_write(
                &zone,
                ZoneOverride {
                    power: Some("standby".to_string()),
                    ..ZoneOverride::default()
                },
                Some(&live),
            );
        }
    }

    state.state_manager.refresh().await;
    Ok(Json(state.state_manager.get_state().await))
}

async fn set_zone_volume(
    AxumState(state): AxumState<AppState>,
    Path(zone): Path<String>,
    Json(req): Json<VolumeRequest>,
) -> Result<Json<State>, Response> {
    validate_zone(&zone)?;

    let zone_config = state
        .config
        .zones
        .get(&zone)
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "unknown_zone", "Zone not found"))?;

    if req.db.is_none() && req.step.is_none() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Must provide either db or step",
        ));
    }
    ensure_zone_on(&state, &zone).await?;

    let new_raw = if let Some(db) = req.db {
        volume::db_to_raw(db)
    } else if let Some(step) = req.step {
        // Base the step on the value we just wrote if the receiver may not have
        // caught up with it; otherwise on a fresh readback (cached state can lag).
        let base = match state.state_manager.pending_volume(&zone) {
            Some(pending) => pending,
            None => state.yxc.get_zone_status(&zone).await.map_err(yxc_error_to_response)?.volume,
        };
        base + step
    } else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Must provide either db or step",
        ));
    };

    let clamped = volume::clamp_raw(new_raw, zone_config.cap_db);

    state.yxc.set_volume(&zone, clamped).await.map_err(yxc_error_to_response)?;
    state.state_manager.note_zone_write(
        &zone,
        ZoneOverride {
            volume: Some(clamped),
            ..ZoneOverride::default()
        },
        None,
    );

    state.state_manager.refresh().await;
    Ok(Json(state.state_manager.get_state().await))
}

async fn set_zone_mute(
    AxumState(state): AxumState<AppState>,
    Path(zone): Path<String>,
    Json(req): Json<MuteRequest>,
) -> Result<Json<State>, Response> {
    validate_zone(&zone)?;
    ensure_zone_on(&state, &zone).await?;

    state.yxc.set_mute(&zone, req.mute).await.map_err(yxc_error_to_response)?;
    state.state_manager.note_zone_write(
        &zone,
        ZoneOverride {
            mute: Some(req.mute),
            ..ZoneOverride::default()
        },
        None,
    );

    state.state_manager.refresh().await;
    Ok(Json(state.state_manager.get_state().await))
}

async fn sse_handler(
    AxumState(state): AxumState<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.state_manager.subscribe();

    let stream = WatchStream::new(rx).map(|state| {
        Ok(Event::default()
            .event("state")
            .data(serde_json::to_string(&state).unwrap()))
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// `GET /api/vis`: SSE stream of spectrum frames (`event: vis`).
async fn vis_handler(
    AxumState(state): AxumState<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let frames = state.vis.subscribe().await;

    let stream = ReceiverStream::new(frames).map(|frame| {
        Ok(Event::default()
            .event("vis")
            .data(serde_json::to_string(&frame).unwrap()))
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("ping"))
}

async fn health_check(AxumState(state): AxumState<AppState>) -> (StatusCode, Json<HealthResponse>) {
    let player_state = state.player.state().await;
    let cliamp_ok = player_state.state != "unknown";

    let receiver_status = state.yxc.get_zone_status("main").await;
    let (receiver_ok, receiver_error) = match receiver_status {
        Ok(_) => (true, None),
        Err(YxcError::Unreachable) => (false, Some("Receiver not responding — is it unplugged?".to_string())),
        Err(e) => (false, Some(format!("{}", e))),
    };

    let airplay_connected = state.route.is_active().await;

    let overall_ok = cliamp_ok && receiver_ok;
    let status = if overall_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(HealthResponse {
            ok: overall_ok,
            cliamp: ComponentHealth {
                ok: cliamp_ok,
                error: if cliamp_ok { None } else { Some("cliamp not responding".to_string()) },
            },
            receiver: ComponentHealth {
                ok: receiver_ok,
                error: receiver_error,
            },
            airplay: AirplayHealth {
                connected: airplay_connected,
            },
        }),
    )
}

// Helper functions

fn validate_zone(zone: &str) -> Result<(), Response> {
    if zone != "main" && zone != "zone2" {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "unknown_zone",
            "Zone must be 'main' or 'zone2'",
        ));
    }
    Ok(())
}

fn error_response(status: StatusCode, error: &str, detail: &str) -> Response {
    let err = ErrorResponse {
        error: error.to_string(),
        detail: detail.to_string(),
    };
    (status, Json(err)).into_response()
}

fn yxc_error_to_response(e: YxcError) -> Response {
    match e {
        YxcError::Unreachable => error_response(
            StatusCode::BAD_GATEWAY,
            "receiver_unreachable",
            "Receiver not responding — is it unplugged?",
        ),
        YxcError::ApiError(msg) => {
            error!("YXC API error: {}", msg);
            error_response(
                StatusCode::BAD_GATEWAY,
                "receiver_busy",
                "Receiver busy — try again",
            )
        }
    }
}

/// Poll until `zone` reports `power == "on"`, giving up after the configured
/// timeout (the caller carries on either way; the retrying client covers the rest).
async fn wait_for_power_on(state: &AppState, zone: &str) {
    let timing = state.policy_timing;
    let started = tokio::time::Instant::now();
    while started.elapsed() < timing.power_wait_timeout {
        if let Ok(status) = state.yxc.get_zone_status(zone).await {
            if status.power == "on" {
                return;
            }
        }
        tokio::time::sleep(timing.power_poll_interval).await;
    }
}

/// Re-read a zone's volume after `volume_verify_delay` and, if the receiver bumped
/// it above `start_raw` on its own, set it back. Does nothing if a newer
/// play/stop/power call has superseded `generation`.
async fn verify_start_volume(state: AppState, zone: String, start_raw: i32, generation: u64) {
    tokio::time::sleep(state.policy_timing.volume_verify_delay).await;
    if state.generation.load(Ordering::SeqCst) != generation {
        return;
    }
    let Ok(status) = state.yxc.get_zone_status(&zone).await else {
        return;
    };
    if status.volume > start_raw {
        if let Err(e) = state.yxc.set_volume(&zone, start_raw).await {
            error!("Failed to restore start volume on {}: {}", zone, e);
            return;
        }
        state.state_manager.refresh().await;
    }
}

/// Default start volume for a zone, clamped to the zone's cap.
fn start_volume_raw(config: &Config, zone: &str) -> Option<i32> {
    let zone_config = config.zones.get(zone)?;
    Some(volume::clamp_raw(
        volume::db_to_raw(zone_config.start_db),
        zone_config.cap_db,
    ))
}

/// Read both zones directly from the receiver.
async fn live_snapshots(yxc: &Arc<dyn YxcClient>) -> Result<HashMap<String, ZoneSnapshot>, Response> {
    let mut snapshots = HashMap::new();
    for zone in ["main", "zone2"] {
        let status = yxc.get_zone_status(zone).await.map_err(yxc_error_to_response)?;
        snapshots.insert(
            zone.to_string(),
            ZoneSnapshot {
                power: status.power,
                input: status.input,
                volume: status.volume,
            },
        );
    }
    Ok(snapshots)
}

/// Overlay still-pending power/input writes onto live snapshots: the receiver's
/// readback lags, so a zone just toggled would otherwise look unchanged. A write
/// is skipped once the live readback has moved to something other than the
/// pre-write or written value, so a TV that grabbed the zone is never masked.
fn overlay_pending(
    state_manager: &StateManager,
    mut snapshots: HashMap<String, ZoneSnapshot>,
) -> HashMap<String, ZoneSnapshot> {
    for (zone, snapshot) in snapshots.iter_mut() {
        let live = ZoneLive {
            power: snapshot.power.clone(),
            input: snapshot.input.clone(),
        };
        let pending = state_manager.pending_override_against(zone, &live);
        if let Some(power) = pending.power {
            snapshot.power = power;
        }
        if let Some(input) = pending.input {
            snapshot.input = input;
        }
    }
    snapshots
}

fn extract_snapshots(state: &State) -> Result<HashMap<String, ZoneSnapshot>, Response> {
    let zones = state.zones.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "receiver_unreachable",
            "Receiver not responding — is it unplugged?",
        )
    })?;

    let mut snapshots = HashMap::new();
    for (zone_name, zone_info) in zones {
        snapshots.insert(
            zone_name.clone(),
            ZoneSnapshot {
                power: zone_info.power.clone(),
                input: zone_info.input.clone(),
                volume: zone_info.volume,
            },
        );
    }

    Ok(snapshots)
}

fn determine_selection(
    req_zones: &Option<HashMap<String, bool>>,
    snapshots: &HashMap<String, ZoneSnapshot>,
) -> ZoneSelection {
    if let Some(zones) = req_zones {
        ZoneSelection {
            main: *zones.get("main").unwrap_or(&false),
            zone2: *zones.get("zone2").unwrap_or(&false),
        }
    } else {
        // Default to current "radio" state
        ZoneSelection {
            main: snapshots
                .get("main")
                .map(|s| s.power == "on" && s.input == "airplay")
                .unwrap_or(false),
            zone2: snapshots
                .get("zone2")
                .map(|s| s.power == "on" && s.input == "airplay")
                .unwrap_or(false),
        }
    }
}

/// Counts a finished zone-policy task when dropped, on every exit path.
struct PolicyCompletion(Arc<AtomicU64>);

impl Drop for PolicyCompletion {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn apply_zone_policy(
    state: AppState,
    selected: ZoneSelection,
    snapshots: HashMap<String, ZoneSnapshot>,
    wait_for_takeover: bool,
    generation: u64,
) {
    let _completion = PolicyCompletion(state.policy_completions.clone());
    let is_stale = || state.generation.load(Ordering::SeqCst) != generation;

    // Wait for AirPlay to grab zones (only if audio was not already playing)
    if wait_for_takeover {
        let timing = state.policy_timing;
        let started = tokio::time::Instant::now();
        while started.elapsed() < timing.takeover_timeout {
            tokio::time::sleep(timing.poll_interval).await;
            if is_stale() {
                return;
            }

            // Refresh state to get fresh data
            state.state_manager.refresh().await;
            let current_state = state.state_manager.get_state().await;

            // Break when any zone's (power, input) differs from pre-play snapshot
            if let Some(zones) = &current_state.zones {
                let zone_changed = snapshots.iter().any(|(zone_name, snap)| {
                    zones.get(zone_name).map(|z| {
                        z.power != snap.power || z.input != snap.input
                    }).unwrap_or(false)
                });

                if zone_changed {
                    break;
                }
            }
        }
    }

    // Apply policy
    if is_stale() {
        return;
    }
    state.state_manager.refresh().await;
    let current_state = state.state_manager.get_state().await;
    let current_snapshots = match extract_snapshots(&current_state) {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut start_volumes = HashMap::new();
    for zone_name in state.config.zones.keys() {
        if let Some(start_raw) = start_volume_raw(&state.config, zone_name) {
            start_volumes.insert(zone_name.clone(), start_raw);
        }
    }

    let actions = policy::plan(&snapshots, &selected, &current_snapshots, &start_volumes);

    for action in &actions {
        if is_stale() {
            return;
        }
        if let Err(e) = execute_policy_action(&state, action).await {
            error!("Failed to execute action {:?}: {}", action, e);
            continue;
        }
        // Let a zone that was just powered on settle before the next setting.
        if let policy::Action::SetPower { zone, on: true } = action {
            wait_for_power_on(&state, zone).await;
        }
    }

    // Re-apply after a short delay
    tokio::time::sleep(state.policy_timing.reapply_delay).await;

    if is_stale() {
        return;
    }
    state.state_manager.refresh().await;
    let current_state = state.state_manager.get_state().await;
    let current_snapshots = match extract_snapshots(&current_state) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Second pass: re-apply power/input; selected zones' volume only if bumped
    let actions = policy::plan(&snapshots, &selected, &current_snapshots, &start_volumes);

    for action in &actions {
        if is_stale() {
            return;
        }
        // For a selected zone's start volume, only undo a bump: if the receiver
        // raised the volume above the start volume on its own, set it back. Never
        // re-issue it otherwise (the user may have turned it down since).
        if let policy::Action::SetVolume { zone, raw } = action {
            let selected_zone = (zone == "main" && selected.main) || (zone == "zone2" && selected.zone2);
            if selected_zone {
                let bumped = current_snapshots.get(zone).is_some_and(|snap| snap.volume > *raw);
                if !bumped {
                    continue;
                }
            }
        }
        if let Err(e) = execute_policy_action(&state, action).await {
            error!("Failed to re-execute action {:?}: {}", action, e);
        }
    }

    state.state_manager.refresh().await;
}

/// Run one policy action. A power-off is re-checked against the live receiver
/// first: a zone that is on and playing another source (a TV that switched in
/// since the snapshot, or one an override wrongly painted as airplay) is never
/// powered down by the policy.
async fn execute_policy_action(state: &AppState, action: &policy::Action) -> Result<(), YxcError> {
    if let policy::Action::SetPower { zone, on: false } = action {
        let status = state.yxc.get_zone_status(zone).await?;
        let live = ZoneLive {
            power: status.power,
            input: status.input,
        };
        if is_live_on_other_source(&live) {
            warn!("Not powering off {}: it is live on {}", zone, live.input);
            return Ok(());
        }
    }
    execute_action(&state.yxc, action).await
}

async fn execute_action(yxc: &Arc<dyn YxcClient>, action: &policy::Action) -> Result<(), YxcError> {
    match action {
        policy::Action::SetPower { zone, on } => yxc.set_power(zone, *on).await,
        policy::Action::SetInput { zone, input } => yxc.set_input(zone, input).await,
        policy::Action::SetVolume { zone, raw } => yxc.set_volume(zone, *raw).await,
    }
}


/// Reject volume/mute changes for a zone in standby: the receiver refuses them
/// (code 5) and the retries would surface as a confusing "receiver busy". Uses a
/// just-written power value if one is still pending, else the live status.
async fn ensure_zone_on(state: &AppState, zone: &str) -> Result<(), Response> {
    let status = state.yxc.get_zone_status(zone).await.map_err(yxc_error_to_response)?;
    let live = ZoneLive {
        power: status.power,
        input: status.input,
    };
    let power = state
        .state_manager
        .pending_override_against(zone, &live)
        .power
        .unwrap_or(live.power);
    if power != "standby" {
        return Ok(());
    }
    let label = state
        .config
        .zones
        .get(zone)
        .map(|zone_config| zone_config.label.as_str())
        .unwrap_or(zone);
    Err(error_response(
        StatusCode::CONFLICT,
        "zone_off",
        &format!("Turn {} on first", label),
    ))
}

/// Whether the zone is powered on and playing something other than AirPlay
/// (someone watching TV on hdmi1, a tuner, another network source...).
fn is_live_on_other_source(live: &ZoneLive) -> bool {
    live.power == "on" && live.input != "airplay"
}

/// Mark `zone` as selected (or not) in the remembered selection.
fn set_selected(state: &AppState, zone: &str, selected: bool) {
    let mut tracking = state.route_tracking.lock().unwrap();
    let selection = tracking.selection.get_or_insert(ZoneSelection {
        main: false,
        zone2: false,
    });
    match zone {
        "main" => selection.main = selected,
        "zone2" => selection.zone2 = selected,
        _ => {}
    }
}

/// Zones currently on+airplay in `snapshots`.
fn radio_selection(snapshots: &HashMap<String, ZoneSnapshot>) -> ZoneSelection {
    determine_selection(&None, snapshots)
}

/// After a zone powers on while audio is playing: if the receiver does not start
/// playing within `regrab_wait`, the sink's RTSP session is stale, so reconnect
/// it. The re-grab can power on the other zone too, so the policy runs with the
/// zones currently radio plus this one selected.
async fn regrab_after_power_on(state: AppState, zone: String, generation: u64) {
    let timing = state.policy_timing;
    let started = Instant::now();
    loop {
        if state.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        if state.player.state().await.state != "playing" {
            return;
        }
        if let Ok(info) = state.yxc.get_play_info().await {
            if info.playback == "play" {
                return;
            }
        }
        if started.elapsed() >= timing.regrab_wait {
            break;
        }
        tokio::time::sleep(timing.power_poll_interval).await;
    }

    // Something else (a TV over HDMI-CEC) may have taken the zone while we waited;
    // only reclaim it if it is still on AirPlay.
    let Ok(status) = state.yxc.get_zone_status(&zone).await else {
        return;
    };
    let live = ZoneLive {
        power: status.power,
        input: status.input,
    };
    let pending = state.state_manager.pending_override_against(&zone, &live);
    if pending.input.unwrap_or(live.input) != "airplay" {
        return;
    }

    warn!("Receiver is not playing after powering on {}; reconnecting AirPlay sink", zone);
    let zone_for_selection = zone.clone();
    reconnect_route(&state, move |snapshots| {
        let mut selection = radio_selection(snapshots);
        match zone_for_selection.as_str() {
            "main" => selection.main = true,
            "zone2" => selection.zone2 = true,
            _ => {}
        }
        selection
    })
    .await;
}

/// Reconnect the AirPlay sink and re-run the zone policy once.
///
/// Takes the play mutex (so it cannot interleave with a play), bumps the
/// generation (cancelling older policy tasks), takes a live snapshot BEFORE the
/// reconnect (the re-handshake makes the receiver grab zones again), then
/// reconnects and spawns the policy with the selection `choose_selection`
/// derives from that snapshot. Does nothing if the player is no longer playing.
async fn reconnect_route<F>(state: &AppState, choose_selection: F)
where
    F: FnOnce(&HashMap<String, ZoneSnapshot>) -> ZoneSelection,
{
    let _guard = state.play_mutex.lock().await;
    if state.player.state().await.state != "playing" {
        return;
    }

    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let Ok(snapshots) = live_snapshots(&state.yxc).await else {
        return;
    };
    let snapshots = overlay_pending(&state.state_manager, snapshots);
    let selection = choose_selection(&snapshots);
    {
        let mut tracking = state.route_tracking.lock().unwrap();
        tracking.selection = Some(selection.clone());
        let now = Instant::now();
        tracking.last_activity = Some(now);
        tracking.last_reconnect = Some(now);
    }

    if let Err(e) = state.route.reconnect().await {
        error!("Failed to reconnect AirPlay sink: {}", e);
        return;
    }

    let policy_state = state.clone();
    tokio::spawn(async move {
        apply_zone_policy(policy_state, selection, snapshots, true, generation).await;
    });
}

/// Start the watchdog that notices a receiver that has stopped pulling audio
/// while cliamp still plays, and restarts the AirPlay sink to re-handshake.
pub fn start_watchdog_task(state: AppState) {
    tokio::spawn(async move {
        let mut stalled_since: Option<Instant> = None;
        let mut ticker = tokio::time::interval(state.policy_timing.watchdog_interval);
        loop {
            ticker.tick().await;
            watchdog_tick(&state, &mut stalled_since).await;
        }
    });
}

/// One watchdog check. `stalled_since` tracks when the current stall began.
async fn watchdog_tick(state: &AppState, stalled_since: &mut Option<Instant>) {
    let timing = state.policy_timing;

    // The stall condition: cliamp playing, a zone on radio, receiver not playing.
    // Anything else (including another AirPlay sender owning the receiver, which
    // shows as playback "play") clears it.
    if state.player.state().await.state != "playing" {
        *stalled_since = None;
        return;
    }
    let Ok(play_info) = state.yxc.get_play_info().await else {
        *stalled_since = None;
        return;
    };
    if play_info.playback == "play" {
        *stalled_since = None;
        return;
    }
    let Ok(snapshots) = live_snapshots(&state.yxc).await else {
        *stalled_since = None;
        return;
    };
    if !snapshots.values().any(|zone| zone.power == "on" && zone.input == "airplay") {
        *stalled_since = None;
        return;
    }

    let stalled_for = stalled_since.get_or_insert_with(Instant::now).elapsed();
    if stalled_for < timing.watchdog_hold {
        return;
    }

    let remembered = {
        let tracking = state.route_tracking.lock().unwrap();
        let settled = tracking
            .last_activity
            .is_none_or(|at| at.elapsed() > timing.watchdog_settle);
        let rate_limited = tracking
            .last_reconnect
            .is_some_and(|at| at.elapsed() < timing.reconnect_min_interval);
        if !settled || rate_limited {
            return;
        }
        tracking.selection.clone()
    };

    warn!(
        "Receiver has not been playing for {:.0}s while cliamp plays; reconnecting AirPlay sink",
        stalled_for.as_secs_f64()
    );
    *stalled_since = None;
    reconnect_route(state, move |snapshots| {
        // The last play's selection, plus any zone that is radio right now.
        let current = radio_selection(snapshots);
        match remembered {
            Some(last) => ZoneSelection {
                main: last.main || current.main,
                zone2: last.zone2 || current.zone2,
            },
            None => current,
        }
    })
    .await;
}
