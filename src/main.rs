mod api;
mod auth;
mod rtmp;
mod state;

use clap::Parser;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "castr", author, version, about = "Castr RTMP & Live Webcam Streaming Engine", long_about = None)]
struct Cli {
    /// Disable registration of new users
    #[arg(short = 'd', long = "disable-registration", alias = "no-registration")]
    disable_registration: bool,

    /// Enable recording of live RTMP streams to .flv files
    #[arg(short = 'r', long = "record")]
    record: bool,

    /// Maximum size of a recorded file before splitting or deleting (e.g. 100MB, 1GB, or bytes)
    #[arg(short = 'm', long = "record-max-size", alias = "max-record-size", default_value = "100MB")]
    record_max_size: String,

    /// Action when recording reaches max size: 'archive' or 'delete'
    #[arg(short = 'o', long = "record-action", alias = "overflow-action", default_value = "archive")]
    record_action: String,

    /// Maximum number of archived recordings kept per stream (0 = unlimited)
    #[arg(short = 'a', long = "max-archived", alias = "record-max-archives", alias = "max-archives", default_value_t = 0)]
    max_archived: usize,
}

fn parse_size_bytes(s: &str) -> u64 {
    let s_trimmed = s.trim().to_uppercase();
    if let Some(num) = s_trimmed.strip_suffix("GB").or_else(|| s_trimmed.strip_suffix("G")) {
        num.parse::<u64>().unwrap_or(1) * 1024 * 1024 * 1024
    } else if let Some(num) = s_trimmed.strip_suffix("MB").or_else(|| s_trimmed.strip_suffix("M")) {
        num.parse::<u64>().unwrap_or(100) * 1024 * 1024
    } else if let Some(num) = s_trimmed.strip_suffix("KB").or_else(|| s_trimmed.strip_suffix("K")) {
        num.parse::<u64>().unwrap_or(100000) * 1024
    } else if let Some(num) = s_trimmed.strip_suffix("B") {
        num.parse::<u64>().unwrap_or(104_857_600)
    } else {
        s_trimmed.parse::<u64>().unwrap_or(104_857_600)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "castr=debug,tower_http=debug,axum=info".into()),
        )
        .init();

    info!("🔥 Initializing Castr RTMP & Live Webcam Streaming Engine...");

    let cli = Cli::parse();
    if cli.disable_registration {
        info!("🔒 New user registration is DISABLED via command-line flag.");
    }

    let overflow_action = match cli.record_action.trim().to_lowercase().as_str() {
        "delete" | "del" => state::RecordOverflowAction::Delete,
        _ => state::RecordOverflowAction::Archive,
    };

    let record_config = state::RecordConfig {
        enabled: cli.record,
        max_size_bytes: parse_size_bytes(&cli.record_max_size),
        overflow_action,
        max_archived_files: cli.max_archived,
    };
    if record_config.enabled {
        info!(
            "📼 Stream recording ENABLED (max_size={} bytes, action={:?}, max_archived={})",
            record_config.max_size_bytes,
            record_config.overflow_action,
            if record_config.max_archived_files == 0 {
                "unlimited".to_string()
            } else {
                record_config.max_archived_files.to_string()
            }
        );
    }

    let state = AppState::new(cli.disable_registration, record_config);

    // Start RTMP Server on Port 1935 in background task
    let rtmp_state = Arc::clone(&state);
    tokio::spawn(async move {
        rtmp::start_rtmp_server(rtmp_state, 1935).await;
    });

    // Configure CORS for web app
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Build Axum router serving API endpoints and embedded Static Web UI / Templates
    let app = api::create_router(Arc::clone(&state))
        .layer(cors);

    let http_port = 3000;
    let addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    info!("🌟 Castr Web Studio & API Server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
