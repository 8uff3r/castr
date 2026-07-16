use crate::state::{AppState, MediaPacket, StreamMetadata};
use bytes::Bytes;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

pub async fn start_rtmp_server(state: Arc<AppState>, port: u16) {
    let addr = format!("0.0.0.0:{}", port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            info!("🚀 RTMP Ingest Server listening on rtmp://{}", addr);
            l
        }
        Err(e) => {
            error!("❌ Failed to bind RTMP port {}: {}", port, e);
            return;
        }
    };

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                info!("⚡ New RTMP connection from {}", peer_addr);
                let state_clone = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = handle_rtmp_client(stream, state_clone).await {
                        warn!("RTMP client error ({}): {}", peer_addr, e);
                    }
                });
            }
            Err(e) => {
                error!("Error accepting RTMP connection: {}", e);
            }
        }
    }
}

async fn handle_rtmp_client(mut stream: TcpStream, state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut handshake = Handshake::new(PeerType::Server);
    let mut buffer = [0u8; 8192];
    let mut handshake_completed = false;
    let mut remaining_after_handshake: Option<Vec<u8>> = None;

    // Phase 1: RTMP Handshake
    while !handshake_completed {
        let bytes_read = stream.read(&mut buffer).await?;
        if bytes_read == 0 {
            info!("RTMP client disconnected during handshake");
            return Ok(());
        }

        match handshake.process_bytes(&buffer[..bytes_read])? {
            HandshakeProcessResult::InProgress { response_bytes } => {
                if !response_bytes.is_empty() {
                    stream.write_all(&response_bytes).await?;
                }
            }
            HandshakeProcessResult::Completed { response_bytes, remaining_bytes } => {
                if !response_bytes.is_empty() {
                    stream.write_all(&response_bytes).await?;
                }
                handshake_completed = true;
                if !remaining_bytes.is_empty() {
                    remaining_after_handshake = Some(remaining_bytes);
                }
            }
        }
    }

    info!("🎉 RTMP Handshake completed successfully!");

    // Phase 2: RTMP Chunk Session
    let config = ServerSessionConfig::new();
    let (mut session, mut results) = ServerSession::new(config)?;
    let mut current_stream_key: Option<String> = None;

    process_results(&mut stream, &mut session, &state, &mut current_stream_key, results).await?;

    if let Some(rem) = remaining_after_handshake {
        results = match session.handle_input(&rem) {
            Ok(res) => res,
            Err(e) => {
                warn!("RTMP session handle_input error on initial remaining bytes: {:?}", e);
                return Ok(());
            }
        };
        process_results(&mut stream, &mut session, &state, &mut current_stream_key, results).await?;
    }

    loop {
        let bytes_read = stream.read(&mut buffer).await?;
        if bytes_read == 0 {
            info!("RTMP client disconnected cleanly");
            if let Some(ref key) = current_stream_key {
                state.end_stream(key).await;
            }
            break;
        }

        let input_data = &buffer[..bytes_read];
        results = match session.handle_input(input_data) {
            Ok(res) => res,
            Err(e) => {
                warn!("RTMP session handle_input error: {:?}", e);
                break;
            }
        };

        process_results(&mut stream, &mut session, &state, &mut current_stream_key, results).await?;
    }

    if let Some(ref key) = current_stream_key {
        state.end_stream(key).await;
    }

    Ok(())
}

async fn process_results(
    stream: &mut TcpStream,
    session: &mut ServerSession,
    state: &Arc<AppState>,
    current_stream_key: &mut Option<String>,
    mut results: Vec<ServerSessionResult>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    while !results.is_empty() {
        let mut next_results = Vec::new();
        for result in results {
            match result {
                ServerSessionResult::OutboundResponse(packet) => {
                    stream.write_all(&packet.bytes).await?;
                }
                ServerSessionResult::RaisedEvent(event) => {
                    let more_res = handle_rtmp_event(session, state, current_stream_key, event).await?;
                    next_results.extend(more_res);
                }
                _ => {}
            }
        }
        results = next_results;
    }
    Ok(())
}

async fn handle_rtmp_event(
    session: &mut ServerSession,
    state: &Arc<AppState>,
    current_stream_key: &mut Option<String>,
    event: ServerSessionEvent,
) -> Result<Vec<ServerSessionResult>, Box<dyn std::error::Error + Send + Sync>> {
    let mut new_results = Vec::new();
    match event {
        ServerSessionEvent::ConnectionRequested { request_id, app_name } => {
            info!("RTMP Connection requested for app: '{}'", app_name);
            if let Ok(res) = session.accept_request(request_id) {
                new_results.extend(res);
            }
        }
        ServerSessionEvent::PublishStreamRequested { request_id, app_name, stream_key, mode: _ } => {
            info!("📡 RTMP Publish requested: app='{}', stream_key='{}'", app_name, stream_key);
            *current_stream_key = Some(stream_key.clone());

            let meta = StreamMetadata {
                stream_key: stream_key.clone(),
                title: format!("Live Stream - {}", stream_key),
                broadcaster: "Broadcaster (RTMP)".to_string(),
                category: "Live Webcam".to_string(),
                description: "Streaming live via RTMP Ingest (OBS/Hardware Client)".to_string(),
                is_live: true,
                source_type: "RTMP".to_string(),
                started_at: Some(chrono::Utc::now()),
                viewer_count: 0,
                bitrate_kbps: 2500,
                fps: 60,
                resolution: "1920x1080".to_string(),
            };

            state.create_or_update_stream(meta).await;
            if state.record_config.enabled {
                let rec_state = Arc::clone(state);
                let rec_key = stream_key.clone();
                tokio::spawn(async move {
                    rec_state.recording_loop(rec_key).await;
                });
            }
            if let Ok(res) = session.accept_request(request_id) {
                new_results.extend(res);
            }
        }
        ServerSessionEvent::VideoDataReceived { app_name: _, stream_key, data, timestamp } => {
            let ts = timestamp.value;
            let bytes = Bytes::from(data.to_vec());
            
            // Format as FLV video tag so FLV players can directly demux without rebuilding tags
            let mut flv_tag = Vec::with_capacity(bytes.len() + 15);
            flv_tag.push(0x09); // Tag type 9 = Video
            let len = bytes.len() as u32;
            flv_tag.push(((len >> 16) & 0xFF) as u8);
            flv_tag.push(((len >> 8) & 0xFF) as u8);
            flv_tag.push((len & 0xFF) as u8);
            flv_tag.push(((ts >> 16) & 0xFF) as u8);
            flv_tag.push(((ts >> 8) & 0xFF) as u8);
            flv_tag.push((ts & 0xFF) as u8);
            flv_tag.push(((ts >> 24) & 0xFF) as u8); // Timestamp extended
            flv_tag.push(0); flv_tag.push(0); flv_tag.push(0); // StreamID = 0
            flv_tag.extend_from_slice(&bytes);
            let prev_size = (bytes.len() + 11) as u32;
            flv_tag.push(((prev_size >> 24) & 0xFF) as u8);
            flv_tag.push(((prev_size >> 16) & 0xFF) as u8);
            flv_tag.push(((prev_size >> 8) & 0xFF) as u8);
            flv_tag.push((prev_size & 0xFF) as u8);

            let flv_bytes = Bytes::from(flv_tag);

            // Check for AVC sequence header (Video tag starting with 0x17 0x00 or 0x27 0x00)
            if bytes.len() >= 2 && bytes[1] == 0x00 {
                state.publish_media(&stream_key, MediaPacket::SequenceHeader(flv_bytes.clone())).await;
            }

            state.publish_media(&stream_key, MediaPacket::RawChunk {
                data: flv_bytes,
                content_type: "video/x-flv".to_string(),
            }).await;
        }
        ServerSessionEvent::AudioDataReceived { app_name: _, stream_key, data, timestamp } => {
            let ts = timestamp.value;
            let bytes = Bytes::from(data.to_vec());
            
            // Format as FLV audio tag
            let mut flv_tag = Vec::with_capacity(bytes.len() + 15);
            flv_tag.push(0x08); // Tag type 8 = Audio
            let len = bytes.len() as u32;
            flv_tag.push(((len >> 16) & 0xFF) as u8);
            flv_tag.push(((len >> 8) & 0xFF) as u8);
            flv_tag.push((len & 0xFF) as u8);
            flv_tag.push(((ts >> 16) & 0xFF) as u8);
            flv_tag.push(((ts >> 8) & 0xFF) as u8);
            flv_tag.push((ts & 0xFF) as u8);
            flv_tag.push(((ts >> 24) & 0xFF) as u8);
            flv_tag.push(0); flv_tag.push(0); flv_tag.push(0);
            flv_tag.extend_from_slice(&bytes);
            let prev_size = (bytes.len() + 11) as u32;
            flv_tag.push(((prev_size >> 24) & 0xFF) as u8);
            flv_tag.push(((prev_size >> 16) & 0xFF) as u8);
            flv_tag.push(((prev_size >> 8) & 0xFF) as u8);
            flv_tag.push((prev_size & 0xFF) as u8);

            let flv_bytes = Bytes::from(flv_tag);
            state.publish_media(&stream_key, MediaPacket::RawChunk {
                data: flv_bytes,
                content_type: "video/x-flv".to_string(),
            }).await;
        }
        ServerSessionEvent::PublishStreamFinished { app_name: _, stream_key } => {
            info!("🛑 RTMP Publish stream finished: '{}'", stream_key);
            state.end_stream(&stream_key).await;
            if *current_stream_key == Some(stream_key.clone()) {
                *current_stream_key = None;
            }
        }
        _ => {}
    }
    Ok(new_results)
}
