//! Receiving side (offers, chunk pulls, writing to the clipboard) and the
//! sending side's chunk server.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::outgoing::OfferLookup;
use super::{chunk_aad, chunk_len, media_label, read_chunk, AUTO_DOWNLOAD_THRESHOLD};
use crate::clipboard::{image_fingerprint, write_clipboard_file, write_clipboard_image};
use crate::clipboard::{ClipboardEntry, MediaInfo, MediaStatus};
use crate::error::{DecentPasteError, Result};
use crate::network::media_codec::{MediaChunkRequest, MediaChunkResponse};
use crate::network::protocol::{ClipboardMessage, ContentType, MediaManifest};
use crate::network::NetworkCommand;
use crate::security::{decrypt_content_with_aad, encrypt_content_with_aad};
use crate::state::AppState;
use crate::storage::{discard_cache_path, enforce_cache_limit, reserve_cache_path};

const CHUNK_ATTEMPTS: u32 = 3;
/// Slightly above the request-response timeout so the network layer reports first.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(35);

enum TransferError {
    /// The sender no longer offers this item.
    Expired(String),
    Failed(String),
}

impl From<DecentPasteError> for TransferError {
    fn from(e: DecentPasteError) -> Self {
        TransferError::Failed(e.to_string())
    }
}

impl From<std::io::Error> for TransferError {
    fn from(e: std::io::Error) -> Self {
        TransferError::Failed(e.to_string())
    }
}

/// Handle a decrypted, validated media offer from `peer_id`.
pub async fn handle_incoming_offer(
    app: &AppHandle,
    peer_id: &str,
    msg: &ClipboardMessage,
    manifest: MediaManifest,
) {
    let state = app.state::<AppState>();

    // Already have these bytes (e.g. the peer copied the same thing again): reuse them.
    let existing = {
        let history = state.clipboard_history.read().await;
        history
            .iter()
            .find(|e| e.content_hash == manifest.sha256)
            .and_then(|e| e.media.clone())
            .filter(|m| {
                m.status == MediaStatus::Ready && m.local_path.as_ref().is_some_and(|p| p.is_file())
            })
    };

    let media = existing.clone().unwrap_or(MediaInfo {
        manifest: manifest.clone(),
        source_peer_id: Some(peer_id.to_string()),
        local_path: None,
        status: MediaStatus::Pending,
    });
    let entry = ClipboardEntry {
        id: Uuid::new_v4().to_string(),
        content: media_label(&manifest),
        content_hash: manifest.sha256.clone(),
        timestamp: msg.timestamp,
        origin_device_id: msg.origin_device_id.clone(),
        origin_device_name: msg.origin_device_name.clone(),
        is_local: false,
        content_type: manifest.kind,
        media: Some(media),
    };
    state.add_clipboard_entry(entry.clone()).await;
    let _ = app.emit("clipboard-received", &entry);

    info!(
        "Media offer from {}: {} ({} bytes)",
        msg.origin_device_name, manifest.name, manifest.size
    );

    if let Some(media) = existing {
        if let Err(e) = apply_to_clipboard(app, &media, true).await {
            warn!("Failed to put cached media on clipboard: {}", e);
        }
    } else if manifest.size <= AUTO_DOWNLOAD_THRESHOLD {
        let app = app.clone();
        let entry_id = entry.id.clone();
        tokio::spawn(async move {
            let _ = download(&app, &entry_id).await;
        });
    }
}

/// Download a remote media entry, then put it on the clipboard.
pub async fn download(app: &AppHandle, entry_id: &str) -> Result<ClipboardEntry> {
    let state = app.state::<AppState>();
    let _guard = state.download_lock.lock().await;

    let entry = state
        .find_clipboard_entry(entry_id)
        .await
        .ok_or_else(|| DecentPasteError::InvalidInput("Clipboard entry not found".into()))?;
    let media = entry
        .media
        .clone()
        .ok_or_else(|| DecentPasteError::InvalidInput("Entry is not media".into()))?;

    if media.status == MediaStatus::Ready && media.local_path.as_ref().is_some_and(|p| p.is_file())
    {
        apply_to_clipboard(app, &media, !entry.is_local).await?;
        return Ok(entry);
    }

    let peer_id = media
        .source_peer_id
        .clone()
        .ok_or_else(|| DecentPasteError::InvalidInput("Entry has no source device".into()))?;
    let shared_secret = {
        let peers = state.paired_peers.read().await;
        peers
            .iter()
            .find(|p| p.peer_id == peer_id)
            .map(|p| p.shared_secret.clone())
            .ok_or_else(|| DecentPasteError::PeerNotFound(peer_id.clone()))?
    };

    set_status(app, entry_id, MediaStatus::Downloading, None).await;

    match transfer(app, entry_id, &peer_id, &shared_secret, &media.manifest).await {
        Ok(path) => {
            let updated = set_status(app, entry_id, MediaStatus::Ready, Some(path)).await;
            let _ = enforce_cache_limit();
            info!(
                "Downloaded {} from {}",
                media.manifest.name, entry.origin_device_name
            );
            if let Some(media) = updated.as_ref().and_then(|e| e.media.as_ref()) {
                apply_to_clipboard(app, media, true).await?;
            }
            updated.ok_or_else(|| DecentPasteError::InvalidInput("Clipboard entry removed".into()))
        }
        Err(err) => {
            let (status, message) = match err {
                TransferError::Expired(m) => (MediaStatus::Expired, m),
                TransferError::Failed(m) => (MediaStatus::Failed, m),
            };
            warn!(
                "Media download failed ({}): {}",
                media.manifest.name, message
            );
            set_status(app, entry_id, status, None).await;
            let _ = app.emit(
                "media-error",
                serde_json::json!({ "entryId": entry_id, "name": media.manifest.name, "error": message }),
            );
            Err(DecentPasteError::Network(message))
        }
    }
}

async fn set_status(
    app: &AppHandle,
    entry_id: &str,
    status: MediaStatus,
    local_path: Option<PathBuf>,
) -> Option<ClipboardEntry> {
    let state = app.state::<AppState>();
    let updated = state
        .update_clipboard_entry(entry_id, |entry| {
            if let Some(media) = entry.media.as_mut() {
                media.status = status;
                if local_path.is_some() {
                    media.local_path = local_path;
                }
            }
        })
        .await;
    if let Some(entry) = &updated {
        let _ = app.emit("clipboard-entry-updated", entry);
    }
    updated
}

async fn transfer(
    app: &AppHandle,
    entry_id: &str,
    peer_id: &str,
    shared_secret: &[u8],
    manifest: &MediaManifest,
) -> std::result::Result<PathBuf, TransferError> {
    let (final_path, part_path) = reserve_cache_path(&manifest.name)?;
    let result = pull_chunks(app, entry_id, peer_id, shared_secret, manifest, &part_path).await;
    match result {
        Ok(()) => {
            tokio::fs::rename(&part_path, &final_path).await?;
            Ok(final_path)
        }
        Err(e) => {
            discard_cache_path(&final_path);
            Err(e)
        }
    }
}

async fn pull_chunks(
    app: &AppHandle,
    entry_id: &str,
    peer_id: &str,
    shared_secret: &[u8],
    manifest: &MediaManifest,
    part_path: &Path,
) -> std::result::Result<(), TransferError> {
    let mut file = tokio::fs::File::create(part_path).await?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;

    for index in 0..manifest.chunk_count {
        let encrypted = fetch_chunk(app, peer_id, &manifest.sha256, index).await?;
        let plain = decrypt_content_with_aad(
            &encrypted,
            shared_secret,
            &chunk_aad(&manifest.sha256, index),
        )?;
        if plain.len() != chunk_len(manifest, index) {
            return Err(TransferError::Failed(format!(
                "chunk {} has unexpected length",
                index
            )));
        }
        hasher.update(&plain);
        file.write_all(&plain).await?;
        received += plain.len() as u64;

        let _ = app.emit(
            "media-progress",
            serde_json::json!({ "entryId": entry_id, "received": received, "total": manifest.size }),
        );
    }
    file.flush().await?;
    drop(file);

    if hex::encode(hasher.finalize()) != manifest.sha256 {
        return Err(TransferError::Failed("content hash mismatch".into()));
    }
    Ok(())
}

async fn fetch_chunk(
    app: &AppHandle,
    peer_id: &str,
    sha256: &str,
    index: u32,
) -> std::result::Result<Vec<u8>, TransferError> {
    let state = app.state::<AppState>();
    let mut last_error = String::new();

    for attempt in 1..=CHUNK_ATTEMPTS {
        let tx = state
            .network_command_tx
            .read()
            .await
            .clone()
            .ok_or_else(|| TransferError::Failed("network not running".into()))?;
        let (reply, response) = oneshot::channel();
        tx.send(NetworkCommand::RequestMediaChunk {
            peer_id: peer_id.to_string(),
            request: MediaChunkRequest {
                sha256: sha256.to_string(),
                index,
            },
            reply,
        })
        .await
        .map_err(|_| TransferError::Failed("network not running".into()))?;

        match tokio::time::timeout(CHUNK_TIMEOUT, response).await {
            Ok(Ok(Ok(MediaChunkResponse::Chunk(data)))) => return Ok(data),
            Ok(Ok(Ok(MediaChunkResponse::NotFound))) => {
                return Err(TransferError::Expired(
                    "the sender no longer offers this item".into(),
                ))
            }
            Ok(Ok(Ok(MediaChunkResponse::Denied))) => {
                return Err(TransferError::Failed(
                    "the sender refused the transfer".into(),
                ))
            }
            Ok(Ok(Err(e))) => last_error = e,
            Ok(Err(_)) => last_error = "network task dropped the request".into(),
            Err(_) => last_error = "timed out".into(),
        }
        debug!("Chunk {} attempt {} failed: {}", index, attempt, last_error);
        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
    }
    Err(TransferError::Failed(format!(
        "chunk {} failed: {}",
        index, last_error
    )))
}

/// Put a downloaded (or local) media item on the system clipboard.
/// `suppress_echo` marks it as already seen so the monitor does not re-broadcast it;
/// pass false when the user re-copies their own item and expects it to be sent again.
pub async fn apply_to_clipboard(
    app: &AppHandle,
    media: &MediaInfo,
    suppress_echo: bool,
) -> Result<()> {
    let path = media
        .local_path
        .clone()
        .filter(|p| p.is_file())
        .ok_or_else(|| DecentPasteError::Storage("Media file is no longer available".into()))?;
    let monitor = if suppress_echo {
        app.state::<AppState>()
            .clipboard_monitor
            .read()
            .await
            .clone()
    } else {
        None
    };
    let join_err = |e: tokio::task::JoinError| DecentPasteError::Clipboard(e.to_string());

    match media.manifest.kind {
        ContentType::Image => {
            let (image, fingerprint) = tokio::task::spawn_blocking(move || {
                let image = image::open(&path)
                    .map_err(|e| {
                        DecentPasteError::Clipboard(format!("Failed to decode image: {}", e))
                    })?
                    .to_rgba8();
                let fingerprint = image_fingerprint(&image);
                Ok::<_, DecentPasteError>((image, fingerprint))
            })
            .await
            .map_err(join_err)??;
            if let Some(monitor) = monitor {
                monitor.set_last_hash(fingerprint).await;
            }
            tokio::task::spawn_blocking(move || write_clipboard_image(image))
                .await
                .map_err(join_err)?
        }
        _ => {
            if let Some(monitor) = monitor {
                monitor
                    .remember_file(&path, media.manifest.sha256.clone())
                    .await;
            }
            tokio::task::spawn_blocking(move || write_clipboard_file(&path))
                .await
                .map_err(join_err)?
        }
    }
}

/// Serve one chunk of media we offered. Only peers the item was offered to may pull it.
pub async fn serve_chunk(app: AppHandle, key: u64, peer_id: String, sha256: String, index: u32) {
    let state = app.state::<AppState>();

    let shared_secret = {
        let peers = state.paired_peers.read().await;
        peers
            .iter()
            .find(|p| p.peer_id == peer_id)
            .map(|p| p.shared_secret.clone())
    };

    let response = match shared_secret {
        None => {
            warn!("Media chunk request from unpaired peer {}", peer_id);
            MediaChunkResponse::Denied
        }
        Some(secret) => match state.outgoing_media.lookup(&sha256, &peer_id).await {
            OfferLookup::Found(offer) => {
                let aad = chunk_aad(&sha256, index);
                let encrypted = tokio::task::spawn_blocking(move || {
                    let chunk = read_chunk(&offer.path, &offer.manifest, index)?;
                    encrypt_content_with_aad(&chunk, &secret, &aad)
                })
                .await;
                match encrypted {
                    Ok(Ok(data)) => MediaChunkResponse::Chunk(data),
                    Ok(Err(e)) => {
                        warn!("Failed to read media chunk {}: {}", index, e);
                        MediaChunkResponse::NotFound
                    }
                    Err(_) => MediaChunkResponse::NotFound,
                }
            }
            OfferLookup::NotFound => MediaChunkResponse::NotFound,
            OfferLookup::Denied => {
                warn!("Peer {} requested media it was not offered", peer_id);
                MediaChunkResponse::Denied
            }
        },
    };

    let tx = state.network_command_tx.read().await.clone();
    if let Some(tx) = tx {
        let _ = tx
            .send(NetworkCommand::SendMediaChunk { key, response })
            .await;
    }
}
