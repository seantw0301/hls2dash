//! HTTP handlers for `/api/channels` CRUD and enable/disable.

use crate::channel::ChannelRegistry;
use crate::config::default_enabled;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

/// Request body for `POST /api/channels`.
#[derive(Debug, Deserialize)]
pub struct AddChannelRequest {
    pub id: String,
    pub hls_url: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Request body for `PUT /api/channels/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateChannelRequest {
    pub hls_url: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
pub struct BatchChannelResult {
    pub affected: usize,
    pub enabled: bool,
}

/// `GET /api/channels`
pub async fn list_channels(State(registry): State<ChannelRegistry>) -> impl IntoResponse {
    Json(registry.list().await)
}

/// `POST /api/channels`
pub async fn add_channel(
    State(registry): State<ChannelRegistry>,
    Json(body): Json<AddChannelRequest>,
) -> Response {
    match registry
        .add_channel(body.id, body.hls_url, body.enabled)
        .await
    {
        Ok(()) => (StatusCode::CREATED, "channel added").into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err}")).into_response(),
    }
}

/// `PUT /api/channels/{id}`
pub async fn update_channel(
    State(registry): State<ChannelRegistry>,
    Path(channel_id): Path<String>,
    Json(body): Json<UpdateChannelRequest>,
) -> Response {
    match registry
        .update_channel(&channel_id, body.hls_url, body.enabled)
        .await
    {
        Ok(()) => (StatusCode::OK, "channel updated").into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err}")).into_response(),
    }
}

/// `DELETE /api/channels/{id}`
pub async fn remove_channel(
    State(registry): State<ChannelRegistry>,
    Path(channel_id): Path<String>,
) -> Response {
    match registry.remove_channel(&channel_id).await {
        Ok(()) => (StatusCode::OK, "channel removed").into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err}")).into_response(),
    }
}

/// `POST /api/channels/{id}/enable`
pub async fn enable_channel(
    State(registry): State<ChannelRegistry>,
    Path(channel_id): Path<String>,
) -> Response {
    match registry.enable_channel(&channel_id).await {
        Ok(()) => (StatusCode::OK, "channel enabled").into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err}")).into_response(),
    }
}

/// `POST /api/channels/{id}/disable`
pub async fn disable_channel(
    State(registry): State<ChannelRegistry>,
    Path(channel_id): Path<String>,
) -> Response {
    match registry.disable_channel(&channel_id).await {
        Ok(()) => (StatusCode::OK, "channel disabled").into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err}")).into_response(),
    }
}

/// `POST /api/channels/enable-all`
pub async fn enable_all_channels(State(registry): State<ChannelRegistry>) -> Response {
    let affected = registry.enable_all().await;
    Json(BatchChannelResult {
        affected,
        enabled: true,
    })
    .into_response()
}

/// `POST /api/channels/disable-all`
pub async fn disable_all_channels(State(registry): State<ChannelRegistry>) -> Response {
    let affected = registry.disable_all().await;
    Json(BatchChannelResult {
        affected,
        enabled: false,
    })
    .into_response()
}
