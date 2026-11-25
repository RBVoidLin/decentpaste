use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{debug, warn};

use crate::network::protocol::{ContentType, MediaManifest};

/// Largest text we sync (text travels inline in the gossipsub message).
use crate::network::MAX_CLIPBOARD_CONTENT_BYTES as MAX_TEXT_SIZE;

#[derive(Debug, Clone)]
pub enum ClipboardContent {
    Text {
        content: String,
    },
    /// An image or file, stored on disk at `path`. Bytes never sit in memory.
    Media {
        path: PathBuf,
        manifest: MediaManifest,
    },
}

impl ClipboardContent {
    pub fn content_type(&self) -> ContentType {
        match self {
            ClipboardContent::Text { .. } => ContentType::Text,
            ClipboardContent::Media { manifest, .. } => manifest.kind,
        }
    }

    pub fn preview_text(&self) -> String {
        match self {
            ClipboardContent::Text { content } => content.clone(),
            ClipboardContent::Media { manifest, .. } => crate::media::media_label(manifest),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClipboardChange {
    pub content: ClipboardContent,
    pub content_hash: String,
    pub is_local: bool,
}

/// Identifies a file version cheaply, so unchanged files are not re-hashed.
type FileKey = (PathBuf, u64, SystemTime);

pub struct ClipboardMonitor {
    /// Echo-prevention key of the last clipboard content we saw or wrote:
    /// text hash, file SHA-256, or image pixel fingerprint.
    last_hash: Arc<RwLock<Option<String>>>,
    poll_interval: Duration,
    running: Arc<RwLock<bool>>,
    file_hashes: Arc<Mutex<HashMap<FileKey, String>>>,
    /// Ends the native watcher thread; taken by `stop`.
    #[cfg(desktop)]
    watcher_shutdown: std::sync::Mutex<Option<clipboard_rs::WatcherShutdown>>,
}

fn file_key(path: &Path) -> Option<(FileKey, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    Some(((path.to_path_buf(), meta.len(), modified), meta.len()))
}

impl ClipboardMonitor {
    pub fn new(poll_interval_ms: u64) -> Self {
        Self {
            last_hash: Arc::new(RwLock::new(None)),
            poll_interval: Duration::from_millis(poll_interval_ms),
            running: Arc::new(RwLock::new(false)),
            file_hashes: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(desktop)]
            watcher_shutdown: std::sync::Mutex::new(None),
        }
    }

    pub async fn start(self: &Arc<Self>, app_handle: AppHandle, tx: mpsc::Sender<ClipboardChange>) {
        {
            let mut running = self.running.write().await;
            if *running {
                warn!("Clipboard monitor already running");
                return;
            }
            *running = true;
        }

        // Mobile: outgoing clipboard goes through the share sheet instead.
        #[cfg(mobile)]
        {
            let _ = (app_handle, tx);
        }

        #[cfg(desktop)]
        {
            let (signal_rx, shutdown) = spawn_watcher(self.poll_interval);
            if let Ok(mut slot) = self.watcher_shutdown.lock() {
                *slot = shutdown;
            }
            let monitor = self.clone();
            tokio::spawn(async move {
                // Seed with whatever is on the clipboard now, without broadcasting it.
                monitor.process_change(&app_handle, &tx, false).await;

                let mut signal_rx = signal_rx;
                while signal_rx.recv().await.is_some() {
                    if !*monitor.running.read().await {
                        break;
                    }
                    if !monitor.process_change(&app_handle, &tx, true).await {
                        break;
                    }
                }
                debug!("Clipboard monitor stopped");
                *monitor.running.write().await = false;
            });
        }
    }

    /// Read the clipboard after a change notification. Returns false once the
    /// receiver is gone.
    #[cfg(desktop)]
    async fn process_change(
        &self,
        app_handle: &AppHandle,
        tx: &mpsc::Sender<ClipboardChange>,
        emit: bool,
    ) -> bool {
        use super::file_ops::{image_fingerprint, read_snapshot, ClipboardSnapshot};
        use tauri::Emitter;

        let snapshot = match tokio::task::spawn_blocking(read_snapshot).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(e)) => {
                debug!("Failed to read clipboard: {}", e);
                return true;
            }
            Err(e) => {
                warn!("Clipboard read task failed: {}", e);
                return true;
            }
        };

        let change = match snapshot {
            ClipboardSnapshot::Empty => None,

            ClipboardSnapshot::Text(text) => {
                if text.len() > MAX_TEXT_SIZE {
                    debug!("Clipboard text too large to sync ({} bytes)", text.len());
                    return true;
                }
                let hash = crate::security::hash_content(&text);
                if !self.replace_last_hash(&hash).await || !emit {
                    return true;
                }
                debug!("Clipboard text changed, hash: {}", &hash[..8]);
                Some(ClipboardChange {
                    content: ClipboardContent::Text { content: text },
                    content_hash: hash,
                    is_local: true,
                })
            }

            ClipboardSnapshot::File(path) => {
                let Some((key, size)) = file_key(&path) else {
                    return true;
                };
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());

                if size == 0 {
                    return true;
                }
                if size > crate::media::MAX_MEDIA_SIZE {
                    // Key the notice on the file version so it is shown once.
                    let notice_key = format!("too-large:{:?}", key);
                    if self.replace_last_hash(&notice_key).await && emit {
                        let _ = app_handle.emit(
                            "media-too-large",
                            serde_json::json!({ "name": name, "size": size }),
                        );
                    }
                    return true;
                }

                let Some(hash) = self.hash_file_cached(key).await else {
                    return true;
                };
                if !self.replace_last_hash(&hash).await || !emit {
                    return true;
                }
                debug!("Clipboard file changed: {} ({} bytes)", name, size);
                let manifest =
                    crate::media::build_manifest(ContentType::File, name, size, hash.clone(), None);
                Some(ClipboardChange {
                    content: ClipboardContent::Media { path, manifest },
                    content_hash: hash,
                    is_local: true,
                })
            }

            ClipboardSnapshot::Image(image) => {
                let (image, fingerprint) = match tokio::task::spawn_blocking(move || {
                    let fp = image_fingerprint(&image);
                    (image, fp)
                })
                .await
                {
                    Ok(v) => v,
                    Err(_) => return true,
                };
                if !self.replace_last_hash(&fingerprint).await || !emit {
                    return true;
                }
                match tokio::task::spawn_blocking(move || encode_clipboard_image(image)).await {
                    Ok(Ok(Ok((path, manifest)))) => {
                        debug!("Clipboard image changed: {} bytes", manifest.size);
                        Some(ClipboardChange {
                            content_hash: manifest.sha256.clone(),
                            content: ClipboardContent::Media { path, manifest },
                            is_local: true,
                        })
                    }
                    Ok(Ok(Err(size))) => {
                        let _ = app_handle.emit(
                            "media-too-large",
                            serde_json::json!({ "name": "Image", "size": size }),
                        );
                        None
                    }
                    Ok(Err(e)) => {
                        warn!("Failed to encode clipboard image: {}", e);
                        None
                    }
                    Err(_) => None,
                }
            }
        };

        match change {
            Some(change) => {
                if tx.send(change).await.is_err() {
                    tracing::error!("Failed to send clipboard change - receiver dropped");
                    return false;
                }
                true
            }
            None => true,
        }
    }

    /// Set `last_hash` to `hash`. Returns false if it already was `hash`.
    async fn replace_last_hash(&self, hash: &str) -> bool {
        let mut last = self.last_hash.write().await;
        if last.as_deref() == Some(hash) {
            return false;
        }
        *last = Some(hash.to_string());
        true
    }

    async fn hash_file_cached(&self, key: FileKey) -> Option<String> {
        if let Some(hash) = self.file_hashes.lock().await.get(&key) {
            return Some(hash.clone());
        }
        let path = key.0.clone();
        let hash = match tokio::task::spawn_blocking(move || crate::media::hash_file(&path)).await {
            Ok(Ok(hash)) => hash,
            Ok(Err(e)) => {
                debug!("Failed to hash clipboard file: {}", e);
                return None;
            }
            Err(_) => return None,
        };
        let mut cache = self.file_hashes.lock().await;
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(key, hash.clone());
        Some(hash)
    }

    /// Stop monitoring. Ends the watcher thread, which closes the signal channel and
    /// lets the monitor task exit, so a later `start` on a new monitor does not leak threads.
    pub async fn stop(&self) {
        *self.running.write().await = false;
        #[cfg(desktop)]
        if let Some(shutdown) = self.watcher_shutdown.lock().ok().and_then(|mut s| s.take()) {
            shutdown.stop();
        }
    }

    /// Set the last known hash to prevent the monitor from treating
    /// received clipboard content as a local change (echo prevention).
    pub async fn set_last_hash(&self, hash: String) {
        let mut last = self.last_hash.write().await;
        *last = Some(hash);
    }

    /// Record a file we are about to put on the clipboard, so the monitor can
    /// recognise it without re-hashing (echo prevention for received files).
    pub async fn remember_file(&self, path: &Path, sha256: String) {
        if let Some((key, _)) = file_key(path) {
            self.file_hashes.lock().await.insert(key, sha256.clone());
        }
        self.set_last_hash(sha256).await;
    }

    #[allow(dead_code)]
    pub async fn get_last_hash(&self) -> Option<String> {
        let last = self.last_hash.read().await;
        last.clone()
    }
}

/// Encode a clipboard image as PNG into the cache. `Err(size)` if the PNG is too large.
#[cfg(desktop)]
fn encode_clipboard_image(
    image: image::RgbaImage,
) -> crate::error::Result<std::result::Result<(PathBuf, MediaManifest), u64>> {
    use crate::error::DecentPasteError;

    let (width, height) = image.dimensions();
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| DecentPasteError::Clipboard(format!("PNG encode failed: {}", e)))?;

    if png.len() as u64 > crate::media::MAX_MEDIA_SIZE {
        return Ok(Err(png.len() as u64));
    }

    let name = format!(
        "Image {}.png",
        chrono::Local::now().format("%Y-%m-%d %H.%M.%S")
    );
    let sha256 = crate::security::hash_content(&png);
    let path = crate::storage::save_to_cache(&png, &name)?;
    let _ = crate::storage::enforce_cache_limit();
    let manifest = crate::media::build_manifest(
        ContentType::Image,
        name,
        png.len() as u64,
        sha256,
        Some((width, height)),
    );
    Ok(Ok((path, manifest)))
}

/// Run the native clipboard watcher on its own thread; each change sends a
/// (coalesced) signal.
#[cfg(desktop)]
fn spawn_watcher(
    interval: Duration,
) -> (mpsc::Receiver<()>, Option<clipboard_rs::WatcherShutdown>) {
    use clipboard_rs::{ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext};

    struct Handler(mpsc::Sender<()>);
    impl ClipboardHandler for Handler {
        fn on_clipboard_change(&mut self) {
            // Capacity 1: a burst of changes collapses into one read.
            let _ = self.0.try_send(());
        }
    }

    let (signal_tx, signal_rx) = mpsc::channel(1);
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("clipboard-watcher".into())
        .spawn(
            move || match ClipboardWatcherContext::new_with_interval(interval) {
                Ok(mut watcher) => {
                    watcher.add_handler(Handler(signal_tx));
                    let _ = shutdown_tx.send(Some(watcher.get_shutdown_channel()));
                    watcher.start_watch();
                }
                Err(e) => {
                    tracing::error!("Failed to start clipboard watcher: {}", e);
                    let _ = shutdown_tx.send(None);
                }
            },
        )
        .expect("failed to spawn clipboard watcher thread");
    // The watcher thread replies right after construction, before it starts blocking.
    (signal_rx, shutdown_rx.recv().ok().flatten())
}

pub fn set_clipboard_content(app_handle: &AppHandle, content: &str) -> Result<(), String> {
    app_handle
        .clipboard()
        .write_text(content)
        .map_err(|e| e.to_string())
}
