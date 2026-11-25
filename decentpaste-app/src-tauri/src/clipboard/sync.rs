use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::network::protocol::{ContentType, MediaManifest};
use crate::security::hash_content;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MediaStatus {
    /// Offered by a peer, not downloaded yet (above the auto-download threshold,
    /// or waiting to start).
    Pending,
    Downloading,
    /// Bytes are on disk at `local_path`.
    Ready,
    Failed,
    /// The local file is gone (cache cleared, original moved) or the sender no
    /// longer offers it.
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub manifest: MediaManifest,
    /// Remote entries: the peer to pull chunks from.
    #[serde(default)]
    pub source_peer_id: Option<String>,
    #[serde(default)]
    pub local_path: Option<PathBuf>,
    pub status: MediaStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEntry {
    pub id: String,
    /// Text content, or a human-readable label for media entries.
    pub content: String,
    pub content_hash: String,
    pub timestamp: DateTime<Utc>,
    pub origin_device_id: String,
    pub origin_device_name: String,
    pub is_local: bool,
    #[serde(default)]
    pub content_type: ContentType,
    #[serde(default)]
    pub media: Option<MediaInfo>,
}

impl ClipboardEntry {
    pub fn new_local(content: String, device_id: &str, device_name: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            content_hash: hash_content(&content),
            content,
            timestamp: Utc::now(),
            origin_device_id: device_id.to_string(),
            origin_device_name: device_name.to_string(),
            is_local: true,
            content_type: ContentType::Text,
            media: None,
        }
    }

    pub fn new_remote(
        content: String,
        content_hash: String,
        timestamp: DateTime<Utc>,
        device_id: &str,
        device_name: &str,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            content,
            content_hash,
            timestamp,
            origin_device_id: device_id.to_string(),
            origin_device_name: device_name.to_string(),
            is_local: false,
            content_type: ContentType::Text,
            media: None,
        }
    }

    pub fn preview(&self, max_length: usize) -> String {
        if self.content.len() <= max_length {
            self.content.clone()
        } else {
            format!("{}...", &self.content[..max_length])
        }
    }
}
