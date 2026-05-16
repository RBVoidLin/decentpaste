use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, warn};

use crate::network::protocol::{ContentType, FileMeta};

/// Maximum file size for clipboard transfer (100MB)
const MAX_CLIPBOARD_SIZE: usize = 100 * 1024 * 1024;

/// Represents clipboard content - either text or file data
#[derive(Debug, Clone)]
pub enum ClipboardContent {
    Text { content: String },
    File {
        name: String,
        data: Vec<u8>,
        size: u64,
    },
}

impl ClipboardContent {
    pub fn content_type(&self) -> ContentType {
        match self {
            ClipboardContent::Text { .. } => ContentType::Text,
            ClipboardContent::File { .. } => ContentType::File,
        }
    }

    pub fn as_bytes(&self) -> Vec<u8> {
        match self {
            ClipboardContent::Text { content } => content.as_bytes().to_vec(),
            ClipboardContent::File { data, .. } => data.clone(),
        }
    }

    pub fn hash_content(&self) -> String {
        crate::security::hash_content(&self.as_bytes())
    }

    pub fn preview_text(&self) -> String {
        match self {
            ClipboardContent::Text { content } => content.clone(),
            ClipboardContent::File { name, size, .. } => {
                format!("[File] {} ({} bytes)", name, size)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClipboardChange {
    pub content: ClipboardContent,
    pub content_hash: String,
    pub is_local: bool,
    pub file_meta: Option<FileMeta>,
}

pub struct ClipboardMonitor {
    last_hash: Arc<RwLock<Option<String>>>,
    poll_interval: Duration,
    running: Arc<RwLock<bool>>,
}

impl ClipboardMonitor {
    pub fn new(poll_interval_ms: u64) -> Self {
        Self {
            last_hash: Arc::new(RwLock::new(None)),
            poll_interval: Duration::from_millis(poll_interval_ms),
            running: Arc::new(RwLock::new(false)),
        }
    }

    pub async fn start(&self, app_handle: AppHandle, tx: mpsc::Sender<ClipboardChange>) {
        // Mark as running
        {
            let mut running = self.running.write().await;
            if *running {
                warn!("Clipboard monitor already running");
                return;
            }
            *running = true;
        }

        debug!(
            "Starting clipboard monitor with {:?} poll interval",
            self.poll_interval
        );

        // Clone for the async task
        let last_hash = self.last_hash.clone();
        let poll_interval = self.poll_interval;
        let running = self.running.clone();

        tokio::spawn(async move {
            loop {
                // Check if we should stop
                if !*running.read().await {
                    debug!("Clipboard monitor stopping");
                    break;
                }

                // Try to read clipboard using Tauri plugin
                // Note: On Android/iOS, the Rust clipboard API may not work for reading.
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                {
                    // Try text first
                    match app_handle.clipboard().read_text() {
                        Ok(text) if !text.is_empty() && text.len() <= MAX_CLIPBOARD_SIZE => {
                            let hash = crate::security::hash_content(&text);
                            let mut last = last_hash.write().await;

                            if last.as_ref() != Some(&hash) {
                                debug!("Clipboard text changed, hash: {}", &hash[..8]);
                                *last = Some(hash.clone());

                                let change = ClipboardChange {
                                    content: ClipboardContent::Text { content: text },
                                    content_hash: hash,
                                    is_local: true,
                                    file_meta: None,
                                };

                                if tx.send(change).await.is_err() {
                                    tracing::error!(
                                        "Failed to send clipboard change - receiver dropped"
                                    );
                                    break;
                                }
                            }
                        }
                        _ => {
                            // No text content or text too large - clipboard may have other content
                            // This can happen if clipboard is empty or contains non-text
                            debug!("Could not read clipboard text or text too large");
                        }
                    }
                }

                // On mobile, clipboard monitoring from Rust is not supported
                #[cfg(any(target_os = "android", target_os = "ios"))]
                {
                    // Mobile platforms: clipboard monitoring disabled
                    let _ = (&app_handle, &last_hash, &tx);
                }

                tokio::time::sleep(poll_interval).await;
            }

            *running.write().await = false;
        });
    }

    #[allow(dead_code)]
    pub async fn stop(&self) {
        let mut running = self.running.write().await;
        *running = false;
    }

    /// Set the last known hash to prevent the monitor from treating
    /// received clipboard content as a local change (echo prevention).
    pub async fn set_last_hash(&self, hash: String) {
        let mut last = self.last_hash.write().await;
        *last = Some(hash);
    }

    #[allow(dead_code)]
    pub async fn get_last_hash(&self) -> Option<String> {
        let last = self.last_hash.read().await;
        last.clone()
    }
}

pub fn set_clipboard_content(app_handle: &AppHandle, content: &str) -> Result<(), String> {
    app_handle
        .clipboard()
        .write_text(content)
        .map_err(|e| e.to_string())
}

#[allow(dead_code)]
pub fn set_clipboard_file(_app_handle: &AppHandle, file_path: &PathBuf) -> Result<(), String> {
    // File clipboard writing requires platform-specific implementation:
    // - Windows: CF_HDROP format
    // - macOS: NSPasteboardTypeFileURL
    // - Linux: x-special/gnome-copied-files or similar URI list
    // For now, log a warning
    tracing::warn!("set_clipboard_file called with path: {:?} - file clipboard writing not yet implemented", file_path);
    Ok(())
}