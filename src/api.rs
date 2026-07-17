use crate::auth;
use crate::state::{AppState, ChatMessage, MediaPacket, StreamMetadata};
use axum::{
    extract::ws::{Message, WebSocket},
    extract::{Path, Query, State, WebSocketUpgrade},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use chrono::Utc;
use futures::SinkExt;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::sync::Arc;
use tera::{Context, Tera};
use tracing::{info, warn};
use uuid::Uuid;

use rust_embed_for_web::{EmbedableFile, RustEmbed};

#[derive(RustEmbed)]
#[folder = "templates/"]
struct TemplateAssets;

pub static TEMPLATES: Lazy<Tera> = Lazy::new(|| {
    let mut tera = Tera::default();
    let template_paths = [
        "base.html",
        "index.html",
        "partials/stream_cards.html",
        "partials/stats_cards.html",
    ];
    let mut raw_templates = Vec::new();
    for path in template_paths {
        if let Some(file) = TemplateAssets::get(path) {
            let data = file.data();
            if let Ok(content) = std::str::from_utf8(data.as_ref()) {
                raw_templates.push((path.to_string(), content.to_string()));
            }
        } else {
            tracing::error!("Embedded template not found: {}", path);
        }
    }
    if let Err(e) = tera.add_raw_templates(raw_templates.iter().map(|(n, c)| (n.as_str(), c.as_str()))) {
        tracing::error!("Tera template parsing error from embedded assets: {}", e);
    } else {
        tracing::info!("🎨 Successfully loaded {} embedded Tera templates", raw_templates.len());
    }
    tera
});

#[derive(RustEmbed)]
#[folder = "static/"]
struct StaticAssets;

pub async fn static_handler(
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    let mut path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        path = "index.html";
    }

    let Some(file) = StaticAssets::get(path) else {
        return (StatusCode::NOT_FOUND, "404 Not Found").into_response();
    };

    let etag = file.etag();
    let etag_str: &str = etag.as_ref();
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        if let Ok(req_etag) = if_none_match.to_str() {
            if req_etag == etag_str {
                return StatusCode::NOT_MODIFIED.into_response();
            }
        }
    }

    let accept_encoding = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let (data, encoding) = if accept_encoding.contains("br") && file.data_br().is_some() {
        let br_data = file.data_br().unwrap();
        let slice: &[u8] = br_data.as_ref();
        (slice.to_vec(), Some("br"))
    } else if accept_encoding.contains("gzip") && file.data_gzip().is_some() {
        let gz_data = file.data_gzip().unwrap();
        let slice: &[u8] = gz_data.as_ref();
        (slice.to_vec(), Some("gzip"))
    } else {
        let raw_data = file.data();
        let slice: &[u8] = raw_data.as_ref();
        (slice.to_vec(), None)
    };

    let mut builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, etag_str);

    if let Some(last_mod) = file.last_modified() {
        let last_mod_str: &str = last_mod.as_ref();
        builder = builder.header(header::LAST_MODIFIED, last_mod_str);
    }

    if let Some(mime) = file.mime_type() {
        let mime_str: &str = mime.as_ref();
        builder = builder.header(header::CONTENT_TYPE, mime_str);
    } else {
        builder = builder.header(header::CONTENT_TYPE, "application/octet-stream");
    }

    if let Some(enc) = encoding {
        builder = builder.header(header::CONTENT_ENCODING, enc);
    }

    builder
        .body(axum::body::Body::from(data))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Error building response").into_response())
}

#[derive(Debug, Deserialize)]
pub struct WatchQuery {
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StreamFilterQuery {
    pub search: Option<String>,
    pub category: Option<String>,
}

pub fn create_router(state: Arc<AppState>) -> Router {
    let auth_router = auth::create_auth_router();
    Router::new()
        .route("/", get(render_index))
        .route("/api/partials/streams", get(render_partials_streams))
        .route("/api/partials/stats", get(render_partials_stats))
        .route("/api/streams", get(list_streams).post(register_stream))
        .route("/api/streams/:key/end", post(end_stream))
        .route("/api/stream/live/:key", get(http_flv_stream))
        .route("/api/ws/watch/:key", get(ws_watch_handler))
        .route("/api/ws/chat/:key", get(ws_chat_handler))
        .merge(auth_router)
        .fallback(static_handler)
        .with_state(state)
}

async fn render_index(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut context = Context::new();
    context.insert("title", "Castr — RTMP Live Webcam Streaming Hub");
    context.insert("disable_registration", &state.disable_registration);

    let streams = state.get_stream_list().await;
    context.insert("active_stream_count", &streams.len());
    let total_viewers: usize = streams.iter().map(|s| s.viewer_count).sum();
    context.insert("total_viewers", &total_viewers);

    match TEMPLATES.render("index.html", &context) {
        Ok(rendered) => axum::response::Html(rendered).into_response(),
        Err(e) => {
            tracing::error!("Template render error: {}", e);
            axum::response::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::from(format!("Template error: {}", e)))
                .unwrap()
                .into_response()
        }
    }
}

async fn render_partials_streams(
    State(state): State<Arc<AppState>>,
    Query(filter): Query<StreamFilterQuery>,
) -> impl IntoResponse {
    let mut streams = state.get_stream_list().await;

    if let Some(ref cat) = filter.category {
        if !cat.is_empty() && !cat.eq_ignore_ascii_case("all") {
            streams.retain(|s| s.category.eq_ignore_ascii_case(cat));
        }
    }

    if let Some(ref q) = filter.search {
        let q_lower = q.trim().to_lowercase();
        if !q_lower.is_empty() {
            streams.retain(|s| {
                s.title.to_lowercase().contains(&q_lower)
                    || s.broadcaster.to_lowercase().contains(&q_lower)
                    || s.category.to_lowercase().contains(&q_lower)
            });
        }
    }

    let mut context = Context::new();
    context.insert("streams", &streams);

    match TEMPLATES.render("partials/stream_cards.html", &context) {
        Ok(rendered) => axum::response::Html(rendered).into_response(),
        Err(e) => {
            tracing::error!("Partial render error: {}", e);
            axum::response::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::from(format!("Template error: {}", e)))
                .unwrap()
                .into_response()
        }
    }
}

async fn render_partials_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let streams = state.get_stream_list().await;
    let active_stream_count = streams.len();
    let total_viewers: usize = streams.iter().map(|s| s.viewer_count).sum();

    let mut context = Context::new();
    context.insert("active_stream_count", &active_stream_count);
    context.insert("total_viewers", &total_viewers);

    match TEMPLATES.render("partials/stats_cards.html", &context) {
        Ok(rendered) => axum::response::Html(rendered).into_response(),
        Err(e) => {
            tracing::error!("Partial render error: {}", e);
            axum::response::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::from(format!("Template error: {}", e)))
                .unwrap()
                .into_response()
        }
    }
}

async fn list_streams(State(state): State<Arc<AppState>>) -> Json<Vec<StreamMetadata>> {
    Json(state.get_stream_list().await)
}

async fn register_stream(
    State(state): State<Arc<AppState>>,
    Json(mut meta): Json<StreamMetadata>,
) -> Json<StreamMetadata> {
    if meta.stream_key.is_empty() {
        meta.stream_key = Uuid::new_v4().to_string().replace('-', "")[..12].to_string();
    }
    let updated = state.create_or_update_stream(meta).await;
    Json(updated)
}

async fn end_stream(Path(key): Path<String>, State(state): State<Arc<AppState>>) -> StatusCode {
    state.end_stream(&key).await;
    StatusCode::OK
}

/// HTTP-FLV live stream endpoint restricted to registered users
async fn http_flv_stream(
    Path(key_with_ext): Path<String>,
    Query(query): Query<WatchQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let token = query.token.as_deref().unwrap_or("");
    if state.verify_token(token).await.is_none() {
        warn!(
            "Unauthorized HTTP-FLV watch attempt with token: '{}'",
            token
        );
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(axum::body::Body::from(
                "Authentication Required: Registered users only",
            ))
            .unwrap();
    }

    let key = key_with_ext.trim_end_matches(".flv").to_string();
    let subscription = state.subscribe_media(&key).await;

    let (mut rx, init_tags) = match subscription {
        Some(sub) => sub,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(axum::body::Body::from("Stream offline or does not exist"))
                .unwrap();
        }
    };

    let body_stream = async_stream::stream! {
        let mut init_payload = Vec::new();
        // FLV 9-byte header + 4-byte previous tag size 0
        init_payload.extend_from_slice(&[0x46, 0x4C, 0x56, 0x01, 0x05, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00]);
        for tag in init_tags {
            init_payload.extend_from_slice(&tag);
        }
        yield Ok::<Bytes, std::io::Error>(Bytes::from(init_payload));

        let mut wait_for_keyframe = false;
        loop {
            match rx.recv().await {
                Ok(packet) => {
                    let bytes = match packet {
                        MediaPacket::SequenceHeader(b) => b,
                        MediaPacket::Video { data, .. } => data,
                        MediaPacket::Audio { data, .. } => data,
                        MediaPacket::RawChunk { data, .. } => data,
                    };

                    if bytes.is_empty() {
                        continue;
                    }

                    if wait_for_keyframe {
                        let is_keyframe = bytes[0] == 0x09
                            && bytes.len() >= 13
                            && (bytes[11] >> 4) == 1
                            && bytes[12] == 1;
                        let is_seq_header = (bytes[0] == 0x09 && bytes.len() >= 13 && bytes[12] == 0)
                            || (bytes[0] == 0x08 && bytes.len() >= 13 && bytes[12] == 0);

                        if is_keyframe || is_seq_header {
                            wait_for_keyframe = false;
                        } else {
                            continue;
                        }
                    }

                    yield Ok(bytes);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!("Slow/unstable HTTP-FLV viewer lagged by {} packets. Resynchronizing on next keyframe...", skipped);
                    wait_for_keyframe = true;
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/x-flv")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header("X-Content-Type-Options", "nosniff")
        .header("Transfer-Encoding", "chunked")
        .body(axum::body::Body::from_stream(body_stream))
        .unwrap()
}

/// WebSocket Watch handler restricted to registered users
async fn ws_watch_handler(
    ws: WebSocketUpgrade,
    Path(key): Path<String>,
    Query(query): Query<WatchQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let token = query.token.clone().unwrap_or_default();
    ws.on_upgrade(move |socket| handle_watch_socket(socket, key, token, state))
}

async fn handle_watch_socket(
    mut socket: WebSocket,
    key: String,
    token: String,
    state: Arc<AppState>,
) {
    if state.verify_token(&token).await.is_none() {
        let _ = socket
            .send(Message::Text(
                r#"{"error": "Authentication required. Please log in to watch this live stream."}"#
                    .into(),
            ))
            .await;
        let _ = socket.close().await;
        return;
    }

    info!(
        "📺 Authorized WebSocket viewer connected to stream: {}",
        key
    );
    let subscription = state.subscribe_media(&key).await;

    let (mut rx, init_tags) = match subscription {
        Some(s) => s,
        None => {
            let _ = socket
                .send(Message::Text(r#"{"error": "Stream offline"}"#.into()))
                .await;
            return;
        }
    };

    for tag in init_tags {
        if socket.send(Message::Binary(tag.to_vec())).await.is_err() {
            return;
        }
    }

    let mut wait_for_keyframe = false;
    loop {
        match rx.recv().await {
            Ok(packet) => {
                let bytes = match packet {
                    MediaPacket::SequenceHeader(b) => b,
                    MediaPacket::Video { data, .. } => data,
                    MediaPacket::Audio { data, .. } => data,
                    MediaPacket::RawChunk { data, .. } => data,
                };

                if bytes.is_empty() {
                    continue;
                }

                if wait_for_keyframe {
                    let is_keyframe = bytes[0] == 0x09
                        && bytes.len() >= 13
                        && (bytes[11] >> 4) == 1
                        && bytes[12] == 1;
                    let is_seq_header = (bytes[0] == 0x09 && bytes.len() >= 13 && bytes[12] == 0)
                        || (bytes[0] == 0x08 && bytes.len() >= 13 && bytes[12] == 0);

                    if is_keyframe || is_seq_header {
                        wait_for_keyframe = false;
                    } else {
                        continue;
                    }
                }

                if socket.send(Message::Binary(bytes.to_vec())).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!("Slow/unstable WS viewer lagged by {} packets. Resynchronizing on next keyframe...", skipped);
                wait_for_keyframe = true;
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    info!("Viewer disconnected from {}", key);
}

/// WebSocket Chat handler restricted to registered users
async fn ws_chat_handler(
    ws: WebSocketUpgrade,
    Path(key): Path<String>,
    Query(query): Query<WatchQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let token = query.token.clone().unwrap_or_default();
    ws.on_upgrade(move |socket| handle_chat_socket(socket, key, token, state))
}

async fn handle_chat_socket(
    mut socket: WebSocket,
    key: String,
    token: String,
    state: Arc<AppState>,
) {
    let user = match state.verify_token(&token).await {
        Some(u) => u,
        None => {
            let _ = socket
                .send(Message::Text(
                    r#"{"error": "Authentication required to chat."}"#.into(),
                ))
                .await;
            let _ = socket.close().await;
            return;
        }
    };

    info!(
        "💬 Authorized user '{}' joined chat room: {}",
        user.username, key
    );
    let subscription = state.subscribe_chat(&key).await;

    let (mut rx, recent_chat) = match subscription {
        Some(s) => s,
        None => return,
    };

    for msg in recent_chat {
        if let Ok(json) = serde_json::to_string(&msg) {
            if socket.send(Message::Text(json)).await.is_err() {
                state.unsubscribe_viewer(&key).await;
                return;
            }
        }
    }

    let (mut sender, mut receiver) = futures::stream::StreamExt::split(socket);

    let forward_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if sender.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });

    while let Some(Ok(msg)) = futures::stream::StreamExt::next(&mut receiver).await {
        if let Message::Text(text) = msg {
            if let Ok(mut chat_msg) = serde_json::from_str::<ChatMessage>(&text) {
                chat_msg.id = Uuid::new_v4().to_string();
                chat_msg.stream_key = key.clone();
                chat_msg.sender = user.username.clone();
                chat_msg.badge = Some("Registered User".to_string());
                chat_msg.timestamp = Utc::now();
                state.publish_chat(chat_msg).await;
            }
        }
    }

    forward_task.abort();
    state.unsubscribe_viewer(&key).await;
    info!("Chat client left room: {}", key);
}
