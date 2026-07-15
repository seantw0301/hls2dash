//! DASH HTTP egress + channel control APIs.

use super::channel_api;
use crate::channel::ChannelRegistry;
use crate::config::Config;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{FromRef, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub cache_dir: PathBuf,
    pub registry: ChannelRegistry,
}

impl FromRef<AppState> for ChannelRegistry {
    fn from_ref(state: &AppState) -> Self {
        state.registry.clone()
    }
}

#[derive(Serialize)]
struct ChannelsResponse {
    channels: Vec<ChannelInfo>,
}

#[derive(Serialize)]
struct ChannelInfo {
    id: String,
    mpd: String,
}

/// Serve live DASH manifests, media segments, and channel APIs over HTTP.
pub async fn run(cfg: Arc<Config>, registry: ChannelRegistry) -> anyhow::Result<()> {
    let addr = cfg.dash_addr()?;
    let live_root = cfg.cache.dir.join("live");
    std::fs::create_dir_all(&live_root)?;

    let state = AppState {
        cache_dir: cfg.cache.dir.clone(),
        registry,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/channels", get(list_channels_compat))
        .route("/metrics", get(metrics))
        .route("/live/{channel}/index.mpd", get(serve_mpd))
        .route("/live/{channel}/{file}", get(serve_media))
        .route(
            "/api/channels",
            get(channel_api::list_channels).post(channel_api::add_channel),
        )
        .route(
            "/api/channels/{channel_id}",
            put(channel_api::update_channel).delete(channel_api::remove_channel),
        )
        .route(
            "/api/channels/{channel_id}/enable",
            post(channel_api::enable_channel),
        )
        .route(
            "/api/channels/{channel_id}/disable",
            post(channel_api::disable_channel),
        )
        .route(
            "/api/channels/enable-all",
            post(channel_api::enable_all_channels),
        )
        .route(
            "/api/channels/disable-all",
            post(channel_api::disable_all_channels),
        )
        .layer(cors)
        .with_state(state);

    info!("DASH HTTP listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let channels = state.registry.active_count().await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        format!("hls2dash_active_channels {channels}\n"),
    )
}

async fn list_channels_compat(State(state): State<AppState>) -> Json<ChannelsResponse> {
    let channels = state
        .registry
        .list_active_discovery()
        .await
        .into_iter()
        .map(|(id, mpd)| ChannelInfo { id, mpd })
        .collect();
    Json(ChannelsResponse { channels })
}

async fn serve_mpd(State(state): State<AppState>, Path(channel): Path<String>) -> Response {
    if !crate::config::is_safe_channel(&channel) {
        return (StatusCode::BAD_REQUEST, "invalid channel id").into_response();
    }
    let path = state
        .cache_dir
        .join("live")
        .join(&channel)
        .join("index.mpd");
    serve_file_stream(
        &path,
        "application/dash+xml",
        "no-cache, no-store, must-revalidate",
        Some("channel offline or mpd missing"),
    )
    .await
}

async fn serve_media(
    State(state): State<AppState>,
    Path((channel, file)): Path<(String, String)>,
) -> Response {
    if !crate::config::is_safe_channel(&channel) || !is_safe_file(&file) {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    let path = state.cache_dir.join("live").join(&channel).join(&file);
    let content_type = if file.ends_with(".mp4") || file.ends_with(".m4s") {
        "video/mp4"
    } else {
        "application/octet-stream"
    };
    serve_file_stream(&path, content_type, "no-cache", None).await
}

async fn serve_file_stream(
    path: &std::path::Path,
    content_type: &str,
    cache_control: &str,
    not_found_body: Option<&'static str>,
) -> Response {
    match tokio::fs::File::open(path).await {
        Ok(file) => {
            let stream = ReaderStream::new(file);
            let body = Body::from_stream(stream);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CACHE_CONTROL, cache_control)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => match not_found_body {
            Some(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
    }
}

fn is_safe_file(file: &str) -> bool {
    !file.is_empty()
        && !file.contains("..")
        && !file.contains('/')
        && !file.contains('\\')
        && (file == "init.mp4"
            || file.ends_with(".m4s")
            || file.ends_with(".mp4")
            || file.ends_with(".mpd"))
}
