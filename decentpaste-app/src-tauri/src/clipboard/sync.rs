use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::network::protocol::{ContentType, FileMeta};
use crate::security::hash_content;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEntry {
    pub id: String,
    pub content: String,
    pub content_hash: String,
    pub timestamp: DateTime<Utc>,
    pub origin_device_id: String,
    pub origin_device_name: String,
    pub is_local: bool,
    #[serde(default)]
    pub content_type: ContentType,
    #[serde(default)]
    pub file_meta: Option<FileMeta>,
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
            file_meta: None,
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
            file_meta: None,
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
