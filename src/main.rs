use home_radio::*;

use axum::{
    body::Body,
    http::{header, StatusCode, Uri},
    response::Response,
    routing::get,
    Router,
};
use clap::Parser;
use rust_embed::Embed;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::info;

#[derive(Embed)]
#[folder = "web/"]
struct WebAssets;

#[derive(Parser, Debug)]
#[command(name = "radio-web")]
#[command(about = "Home radio web service")]
struct Args {
    /// Path to configuration file
    #[arg(short, long, env = "RADIO_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "radio_web=info,tower_http=debug".into()),
        )
        .init();

    let args = Args::parse();

    let config_path = args
        .config
        .unwrap_or_else(|| PathBuf::from("/etc/home-radio/config.toml"));

    if !config_path.exists() {
        anyhow::bail!(
            "config file {} not found: copy config/config.example.toml there and set receiver_url",
            config_path.display()
        );
    }
    info!("Loading config from {:?}", config_path);
    let config = config::Config::from_file(&config_path)?;

    info!("Receiver URL: {}", config.receiver_url);
    info!("Cliamp binary: {}", config.cliamp_bin);
    info!("Stations file: {:?}", config.stations_file);
    info!("AirPlay sink unit: {}", config.raop_unit);

    // Initialize components
    let yxc = Arc::new(yxc::HttpYxcClient::new(config.receiver_url.clone())) as Arc<dyn yxc::YxcClient>;

    let cliamp_player = cliamp::CliampPlayer::new(config.cliamp_bin.clone());
    cliamp_player.start_events_task();
    let player = Arc::new(cliamp_player) as Arc<dyn cliamp::Player>;

    let stations = Arc::new(RwLock::new(
        stations::StationManager::new(
            &config.stations_file,
            &config.cache_dir,
            config.remote_stations_url.clone(),
        )
        .await?,
    ));

    // Start station refresh task
    stations::StationManager::start_refresh_task(stations.clone());

    let state_manager = Arc::new(state::StateManager::new(
        yxc.clone(),
        player.clone(),
        stations.clone(),
        config.clone(),
    ));

    // Start state polling task
    state_manager.clone().start_polling_task();

    // Start player watch task for immediate SSE updates
    state_manager.clone().start_player_watch_task();

    let vis = vis::VisHub::start(
        Box::new(vis::CliampVisSource::new(config.cliamp_bin.clone())),
        player.clone(),
        vis::VisConfig::default(),
    );

    let app_state = api::AppState {
        yxc,
        player,
        stations,
        state_manager,
        config: config.clone(),
        play_mutex: Arc::new(tokio::sync::Mutex::new(())),
        policy_timing: api::PolicyTiming::default(),
        generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        route: Arc::new(route::SystemdAudioRoute::new(config.raop_unit.clone())),
        route_tracking: Arc::default(),
        policy_completions: Arc::default(),
        vis,
        radio_browser: Arc::new(radiobrowser::HttpRadioBrowser::connect().await),
        stations_tx: tokio::sync::broadcast::channel(16).0,
    };

    // Watch for a receiver that stops pulling audio while cliamp plays
    api::start_watchdog_task(app_state.clone());

    let api_router = api::create_router(app_state);

    let app = Router::new()
        .merge(api_router)
        .route("/", get(serve_index))
        .route("/{*path}", get(serve_static))
        .layer(RequestBodyLimitLayer::new(4096)); // 4 KiB limit

    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    info!("Listening on {}", config.listen);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn serve_index() -> Response {
    serve_static_file("index.html").await
}

async fn serve_static(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    serve_static_file(path).await
}

async fn serve_static_file(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data))
                .unwrap()
        }
        None => {
            // Try serving index.html for SPA routing
            if let Some(content) = WebAssets::get("index.html") {
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from(content.data))
                    .unwrap()
            } else {
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("Not found"))
                    .unwrap()
            }
        }
    }
}
