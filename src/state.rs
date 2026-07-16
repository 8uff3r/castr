use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, RwLock};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RecordOverflowAction {
    Delete,
    Archive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordConfig {
    pub enabled: bool,
    pub max_size_bytes: u64,
    pub overflow_action: RecordOverflowAction,
    pub max_archived_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password_hash: String,
    pub token: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub stream_key: String,
    pub sender: String,
    pub badge: Option<String>, // e.g. "Broadcaster", "Moderator", "VIP"
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum MediaPacket {
    /// FLV Sequence Header / Initialization metadata required to decode
    SequenceHeader(Bytes),
    /// Video tag payload (codec, timestamp_ms, raw bytes)
    Video {
        data: Bytes,
        timestamp_ms: u32,
        is_keyframe: bool,
    },
    /// Audio tag payload (codec, timestamp_ms, raw bytes)
    Audio {
        data: Bytes,
        timestamp_ms: u32,
    },
    /// Raw chunk (FLV tag or WebM chunk)
    RawChunk {
        data: Bytes,
        content_type: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamMetadata {
    pub stream_key: String,
    pub title: String,
    pub broadcaster: String,
    pub category: String,
    pub description: String,
    pub is_live: bool,
    pub source_type: String, // "RTMP"
    pub started_at: Option<DateTime<Utc>>,
    pub viewer_count: usize,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub resolution: String, // e.g. "1920x1080"
}

pub struct LiveStream {
    pub meta: StreamMetadata,
    pub sender: broadcast::Sender<MediaPacket>,
    pub chat_sender: broadcast::Sender<ChatMessage>,
    pub recent_chat: Vec<ChatMessage>,
    pub video_sequence_header: Option<Bytes>,
    pub audio_sequence_header: Option<Bytes>,
    pub gop_cache: Vec<Bytes>,
}

impl LiveStream {
    pub fn new(meta: StreamMetadata) -> Self {
        let (sender, _) = broadcast::channel(8192);
        let (chat_sender, _) = broadcast::channel(256);
        Self {
            meta,
            sender,
            chat_sender,
            recent_chat: Vec::new(),
            video_sequence_header: None,
            audio_sequence_header: None,
            gop_cache: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct AuthDump {
    users: HashMap<String, User>,
    tokens: HashMap<String, String>,
}

pub struct AppState {
    pub streams: RwLock<HashMap<String, LiveStream>>,
    pub users: RwLock<HashMap<String, User>>,       // username -> User
    pub tokens: RwLock<HashMap<String, String>>,    // token -> username
    pub disable_registration: bool,
    pub record_config: RecordConfig,
}

impl AppState {
    pub fn new(disable_registration: bool, record_config: RecordConfig) -> Arc<Self> {
        let mut users = HashMap::new();
        let mut tokens = HashMap::new();

        if let Ok(data) = std::fs::read_to_string("users.json") {
            if let Ok(dump) = serde_json::from_str::<AuthDump>(&data) {
                users = dump.users;
                tokens = dump.tokens;
                tracing::info!("Loaded {} users and {} tokens from users.json", users.len(), tokens.len());
            }
        }

        Arc::new(Self {
            streams: RwLock::new(HashMap::new()),
            users: RwLock::new(users),
            tokens: RwLock::new(tokens),
            disable_registration,
            record_config,
        })
    }

    pub async fn save_auth(&self) {
        let users = self.users.read().await.clone();
        let tokens = self.tokens.read().await.clone();
        let dump = AuthDump { users, tokens };
        if let Ok(json) = serde_json::to_string_pretty(&dump) {
            let _ = std::fs::write("users.json", json);
        }
    }

    pub async fn verify_token(&self, token: &str) -> Option<User> {
        if token.is_empty() {
            return None;
        }
        let tokens = self.tokens.read().await;
        if let Some(username) = tokens.get(token) {
            let users = self.users.read().await;
            users.get(username).cloned()
        } else {
            None
        }
    }

    pub async fn get_stream_list(&self) -> Vec<StreamMetadata> {
        let streams = self.streams.read().await;
        streams.values().map(|s| s.meta.clone()).collect()
    }

    pub async fn create_or_update_stream(&self, mut meta: StreamMetadata) -> StreamMetadata {
        let mut streams = self.streams.write().await;
        if let Some(existing) = streams.get_mut(&meta.stream_key) {
            existing.meta.title = meta.title.clone();
            existing.meta.broadcaster = meta.broadcaster.clone();
            existing.meta.category = meta.category.clone();
            existing.meta.description = meta.description.clone();
            existing.meta.is_live = meta.is_live;
            existing.meta.source_type = meta.source_type.clone();
            if meta.is_live && existing.meta.started_at.is_none() {
                existing.meta.started_at = Some(Utc::now());
            }
            existing.meta.clone()
        } else {
            if meta.is_live && meta.started_at.is_none() {
                meta.started_at = Some(Utc::now());
            }
            let stream = LiveStream::new(meta.clone());
            streams.insert(meta.stream_key.clone(), stream);
            meta
        }
    }

    pub async fn end_stream(&self, stream_key: &str) {
        let mut streams = self.streams.write().await;
        if let Some(stream) = streams.get_mut(stream_key) {
            stream.meta.is_live = false;
            stream.meta.bitrate_kbps = 0;
            stream.meta.fps = 0;
        }
    }

    pub async fn update_stats(&self, stream_key: &str, bitrate: u32, fps: u32, resolution: &str) {
        let mut streams = self.streams.write().await;
        if let Some(stream) = streams.get_mut(stream_key) {
            stream.meta.bitrate_kbps = bitrate;
            stream.meta.fps = fps;
            if !resolution.is_empty() {
                stream.meta.resolution = resolution.to_string();
            }
        }
    }

    pub async fn subscribe_media(&self, stream_key: &str) -> Option<(broadcast::Receiver<MediaPacket>, Vec<Bytes>)> {
        let streams = self.streams.read().await;
        if let Some(stream) = streams.get(stream_key) {
            let mut init_tags = Vec::new();
            if let Some(ref vseq) = stream.video_sequence_header {
                init_tags.push(vseq.clone());
            }
            if let Some(ref aseq) = stream.audio_sequence_header {
                init_tags.push(aseq.clone());
            }
            init_tags.extend_from_slice(&stream.gop_cache);
            Some((stream.sender.subscribe(), init_tags))
        } else {
            None
        }
    }

    pub async fn publish_media(&self, stream_key: &str, packet: MediaPacket) {
        let mut streams = self.streams.write().await;
        if let Some(stream) = streams.get_mut(stream_key) {
            match packet {
                MediaPacket::SequenceHeader(ref bytes) => {
                    if !bytes.is_empty() {
                        if bytes[0] == 0x09 {
                            stream.video_sequence_header = Some(bytes.clone());
                        } else if bytes[0] == 0x08 {
                            stream.audio_sequence_header = Some(bytes.clone());
                        }
                    }
                }
                MediaPacket::Video { ref data, .. } | MediaPacket::RawChunk { ref data, .. } => {
                    if !data.is_empty() && data[0] == 0x09 {
                        if data.len() >= 13 {
                            let frame_type = data[11] >> 4;
                            let codec_id = data[11] & 0x0F;
                            let avc_packet_type = data[12];
                            if codec_id == 7 && avc_packet_type == 0 {
                                stream.video_sequence_header = Some(data.clone());
                            } else if frame_type == 1 && avc_packet_type == 1 {
                                stream.gop_cache.clear();
                                stream.gop_cache.push(data.clone());
                            } else if !stream.gop_cache.is_empty() {
                                stream.gop_cache.push(data.clone());
                                if stream.gop_cache.len() > 1200 {
                                    stream.gop_cache.remove(0);
                                }
                            }
                        }
                    } else if !data.is_empty() && data[0] == 0x08 {
                        if data.len() >= 13 {
                            let sound_format = data[11] >> 4;
                            let aac_packet_type = data[12];
                            if sound_format == 10 && aac_packet_type == 0 {
                                stream.audio_sequence_header = Some(data.clone());
                            } else if !stream.gop_cache.is_empty() {
                                stream.gop_cache.push(data.clone());
                                if stream.gop_cache.len() > 1200 {
                                    stream.gop_cache.remove(0);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            let _ = stream.sender.send(packet);
        }
    }

    pub async fn subscribe_chat(&self, stream_key: &str) -> Option<(broadcast::Receiver<ChatMessage>, Vec<ChatMessage>)> {
        let mut streams = self.streams.write().await;
        if let Some(stream) = streams.get_mut(stream_key) {
            stream.meta.viewer_count += 1;
            Some((stream.chat_sender.subscribe(), stream.recent_chat.clone()))
        } else {
            None
        }
    }

    pub async fn unsubscribe_viewer(&self, stream_key: &str) {
        let mut streams = self.streams.write().await;
        if let Some(stream) = streams.get_mut(stream_key) {
            if stream.meta.viewer_count > 0 {
                stream.meta.viewer_count -= 1;
            }
        }
    }

    pub async fn publish_chat(&self, msg: ChatMessage) {
        let mut streams = self.streams.write().await;
        if let Some(stream) = streams.get_mut(&msg.stream_key) {
            if stream.recent_chat.len() >= 100 {
                stream.recent_chat.remove(0);
            }
            stream.recent_chat.push(msg.clone());
            let _ = stream.chat_sender.send(msg);
        }
    }

    pub async fn recording_loop(self: Arc<Self>, stream_key: String) {
        let _ = tokio::fs::create_dir_all("recordings").await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let (mut rx, init_tags) = match self.subscribe_media(&stream_key).await {
            Some(s) => s,
            None => return,
        };

        tracing::info!(
            "📼 Stream recorder started for '{}' (max_size={} bytes, action={:?})",
            stream_key,
            self.record_config.max_size_bytes,
            self.record_config.overflow_action
        );

        let mut part_num = 1;
        let mut file_path = format!(
            "recordings/{}_{}.flv",
            stream_key,
            chrono::Utc::now().format("%Y%m%d_%H%M%S")
        );
        let mut current_file = match tokio::fs::File::create(&file_path).await {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to create recording file {}: {}", file_path, e);
                return;
            }
        };

        let flv_header = [
            0x46, 0x4C, 0x56, 0x01, 0x05, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00,
        ];
        let _ = current_file.write_all(&flv_header).await;
        let mut current_size = flv_header.len() as u64;

        for tag in &init_tags {
            let _ = current_file.write_all(tag).await;
            current_size += tag.len() as u64;
        }

        let mut video_seq = None;
        let mut audio_seq = None;
        for tag in &init_tags {
            if !tag.is_empty() && tag[0] == 0x09 {
                video_seq = Some(tag.clone());
                break;
            }
        }
        for tag in &init_tags {
            if !tag.is_empty() && tag[0] == 0x08 {
                audio_seq = Some(tag.clone());
                break;
            }
        }

        loop {
            match rx.recv().await {
                Ok(packet) => {
                    let bytes = match packet {
                        MediaPacket::SequenceHeader(b) => {
                            if !b.is_empty() && b[0] == 0x09 {
                                video_seq = Some(b.clone());
                            } else if !b.is_empty() && b[0] == 0x08 {
                                audio_seq = Some(b.clone());
                            }
                            b
                        }
                        MediaPacket::Video { data, .. } | MediaPacket::RawChunk { data, .. } => data,
                        MediaPacket::Audio { data, .. } => data,
                    };

                    if bytes.is_empty() {
                        continue;
                    }

                    let is_keyframe = bytes[0] == 0x09
                        && bytes.len() >= 13
                        && (bytes[11] >> 4) == 1
                        && bytes[12] == 1;

                    if current_size >= self.record_config.max_size_bytes && is_keyframe {
                        let _ = current_file.flush().await;
                        drop(current_file);

                        match self.record_config.overflow_action {
                            RecordOverflowAction::Delete => {
                                tracing::info!(
                                    "♻️ Recording file reached max size ({} bytes). Deleting {} and restarting...",
                                    current_size,
                                    file_path
                                );
                                let _ = tokio::fs::remove_file(&file_path).await;
                                file_path = format!(
                                    "recordings/{}_{}.flv",
                                    stream_key,
                                    chrono::Utc::now().format("%Y%m%d_%H%M%S")
                                );
                                current_file = match tokio::fs::File::create(&file_path).await {
                                    Ok(f) => f,
                                    Err(_) => break,
                                };
                                current_size = 0;
                            }
                            RecordOverflowAction::Archive => {
                                if self.record_config.max_archived_files > 0 {
                                    if let Ok(mut entries) = tokio::fs::read_dir("recordings").await {
                                        let mut archived_files = Vec::new();
                                        let prefix = format!("{}_", stream_key);
                                        while let Ok(Some(entry)) = entries.next_entry().await {
                                            let path = entry.path();
                                            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                                if name.starts_with(&prefix) && name.ends_with(".flv") {
                                                    if let Ok(meta) = entry.metadata().await {
                                                        if let Ok(modified) = meta.modified() {
                                                            archived_files.push((modified, path));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if archived_files.len() >= self.record_config.max_archived_files {
                                            archived_files.sort_by_key(|(m, _)| *m);
                                            let num_to_remove = archived_files.len() - self.record_config.max_archived_files + 1;
                                            for (_, old_path) in archived_files.into_iter().take(num_to_remove) {
                                                tracing::info!(
                                                    "🗑️ Pruning oldest archive file {} (max_archived limit: {})",
                                                    old_path.display(),
                                                    self.record_config.max_archived_files
                                                );
                                                let _ = tokio::fs::remove_file(&old_path).await;
                                            }
                                        }
                                    }
                                }

                                part_num += 1;
                                tracing::info!(
                                    "📦 Recording file reached max size ({} bytes). Archiving and starting part {}...",
                                    current_size,
                                    part_num
                                );
                                file_path = format!(
                                    "recordings/{}_{}_part{}.flv",
                                    stream_key,
                                    chrono::Utc::now().format("%Y%m%d_%H%M%S"),
                                    part_num
                                );
                                current_file = match tokio::fs::File::create(&file_path).await {
                                    Ok(f) => f,
                                    Err(_) => break,
                                };
                                current_size = 0;
                            }
                        }

                        let _ = current_file.write_all(&flv_header).await;
                        current_size += flv_header.len() as u64;
                        if let Some(ref vs) = video_seq {
                            let _ = current_file.write_all(vs).await;
                            current_size += vs.len() as u64;
                        }
                        if let Some(ref as_tag) = audio_seq {
                            let _ = current_file.write_all(as_tag).await;
                            current_size += as_tag.len() as u64;
                        }
                    }

                    let _ = current_file.write_all(&bytes).await;
                    current_size += bytes.len() as u64;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Recorder lagged by {} frames on {}, continuing...", n, stream_key);
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("Stream {} closed, stopping recorder.", stream_key);
                    let _ = current_file.flush().await;
                    break;
                }
            }
        }
    }
}
