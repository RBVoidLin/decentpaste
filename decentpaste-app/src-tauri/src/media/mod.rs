//! Media (image / file) clipboard transfer.
//!
//! Control plane: a small encrypted `MediaManifest` rides in a normal
//! `ClipboardMessage` over gossipsub (and the offline Sync buffer).
//! Data plane: the receiver pulls encrypted chunks over `/decentpaste/media/1.0.0`.

pub mod download;
pub mod outgoing;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{DecentPasteError, Result};
use crate::network::protocol::{ContentType, MediaManifest};

pub use outgoing::OutgoingMediaRegistry;

pub const MAX_MEDIA_SIZE: u64 = 100 * 1024 * 1024;
pub const AUTO_DOWNLOAD_THRESHOLD: u64 = 20 * 1024 * 1024;
pub const MEDIA_CHUNK_SIZE: u32 = 1024 * 1024;

/// Streaming SHA-256 of a file (hex).
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Expected plaintext length of chunk `index`.
pub fn chunk_len(manifest: &MediaManifest, index: u32) -> usize {
    let start = index as u64 * manifest.chunk_size as u64;
    manifest
        .size
        .saturating_sub(start)
        .min(manifest.chunk_size as u64) as usize
}

pub fn read_chunk(path: &Path, manifest: &MediaManifest, index: u32) -> Result<Vec<u8>> {
    if index >= manifest.chunk_count {
        return Err(DecentPasteError::InvalidInput(format!(
            "chunk {} out of range",
            index
        )));
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(index as u64 * manifest.chunk_size as u64))?;
    let mut buf = vec![0u8; chunk_len(manifest, index)];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Additional authenticated data for a chunk: binds it to its transfer and position.
pub fn chunk_aad(sha256: &str, index: u32) -> Vec<u8> {
    let mut aad = sha256.as_bytes().to_vec();
    aad.extend_from_slice(&index.to_be_bytes());
    aad
}

pub fn build_manifest(
    kind: ContentType,
    name: String,
    size: u64,
    sha256: String,
    dimensions: Option<(u32, u32)>,
) -> MediaManifest {
    let mime_type = mime_guess::from_path(&name)
        .first_or_octet_stream()
        .to_string();
    MediaManifest {
        kind,
        name,
        size,
        mime_type,
        sha256,
        chunk_size: MEDIA_CHUNK_SIZE,
        chunk_count: MediaManifest::chunk_count_for(size, MEDIA_CHUNK_SIZE),
        width: dimensions.map(|d| d.0),
        height: dimensions.map(|d| d.1),
    }
}

pub fn format_size(size: u64) -> String {
    const KB: f64 = 1024.0;
    let size = size as f64;
    if size < KB * KB {
        format!("{:.0} KB", (size / KB).max(1.0))
    } else {
        format!("{:.1} MB", size / (KB * KB))
    }
}

/// History label for a media entry (also what text-only clients would show).
pub fn media_label(manifest: &MediaManifest) -> String {
    match (manifest.kind, manifest.width, manifest.height) {
        (ContentType::Image, Some(w), Some(h)) => {
            format!("[Image] {}×{} ({})", w, h, format_size(manifest.size))
        }
        _ => format!("[File] {} ({})", manifest.name, format_size(manifest.size)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(size: u64) -> MediaManifest {
        build_manifest(
            ContentType::File,
            "a.mp4".into(),
            size,
            "0".repeat(64),
            None,
        )
    }

    #[test]
    fn chunk_layout_boundaries() {
        let exact = manifest(MEDIA_CHUNK_SIZE as u64 * 3);
        assert_eq!(exact.chunk_count, 3);
        assert_eq!(chunk_len(&exact, 2), MEDIA_CHUNK_SIZE as usize);

        let partial = manifest(MEDIA_CHUNK_SIZE as u64 * 2 + 5);
        assert_eq!(partial.chunk_count, 3);
        assert_eq!(chunk_len(&partial, 2), 5);

        let max = manifest(MAX_MEDIA_SIZE);
        assert_eq!(max.chunk_count, 100);
        assert!(max.validate(MAX_MEDIA_SIZE).is_ok());
        assert!(manifest(MAX_MEDIA_SIZE + 1)
            .validate(MAX_MEDIA_SIZE)
            .is_err());
        assert!(manifest(0).validate(MAX_MEDIA_SIZE).is_err());
    }

    #[test]
    fn validate_rejects_tampered_layout() {
        let mut m = manifest(10);
        m.chunk_count = 2;
        assert!(m.validate(MAX_MEDIA_SIZE).is_err());
    }

    #[test]
    fn aad_differs_by_index() {
        assert_ne!(chunk_aad("ab", 0), chunk_aad("ab", 1));
    }
}
