//! Native clipboard access for files and images (desktop only, via `clipboard-rs`).
//! Text writes still go through `tauri-plugin-clipboard-manager` so mobile keeps working.

use std::path::{Path, PathBuf};

use image::RgbaImage;
use sha2::{Digest, Sha256};

use crate::error::{DecentPasteError, Result};

/// What the clipboard currently holds, in the order we prefer to sync it.
#[derive(Debug)]
pub enum ClipboardSnapshot {
    File(PathBuf),
    Text(String),
    Image(RgbaImage),
    Empty,
}

/// Fingerprint used for echo prevention. Hashes decoded pixels instead of the
/// encoded bytes, because writing an image and reading it back can re-encode it
/// (PNG/TIFF on macOS, PNG/DIB on Windows) and produce different bytes.
pub fn image_fingerprint(image: &RgbaImage) -> String {
    let mut hasher = Sha256::new();
    hasher.update(image.width().to_le_bytes());
    hasher.update(image.height().to_le_bytes());
    hasher.update(image.as_raw());
    hex::encode(hasher.finalize())
}

#[cfg(desktop)]
mod desktop {
    use super::*;
    use clipboard_rs::common::RustImage;
    use clipboard_rs::{Clipboard, ClipboardContext, ContentFormat, RustImageData};

    fn context() -> Result<ClipboardContext> {
        ClipboardContext::new().map_err(|e| DecentPasteError::Clipboard(e.to_string()))
    }

    /// Read the clipboard. Priority is file → text → image:
    /// - Finder puts the file name as text next to the file URL, so files come first.
    /// - Office apps put a rendered picture next to copied text, so text beats image.
    pub fn read_snapshot() -> Result<ClipboardSnapshot> {
        let ctx = context()?;

        if ctx.has(ContentFormat::Files) {
            if let Ok(files) = ctx.get_files() {
                // v1 syncs a single file: the first regular file in the selection.
                if let Some(path) = files
                    .iter()
                    .map(|f| PathBuf::from(f.strip_prefix("file://").unwrap_or(f)))
                    .find(|p| p.is_file())
                {
                    return Ok(ClipboardSnapshot::File(path));
                }
            }
        }

        if ctx.has(ContentFormat::Text) {
            if let Ok(text) = ctx.get_text() {
                if !text.is_empty() {
                    return Ok(ClipboardSnapshot::Text(text));
                }
            }
        }

        if ctx.has(ContentFormat::Image) {
            let image = ctx
                .get_image()
                .and_then(|img| img.to_rgba8())
                .map_err(|e| DecentPasteError::Clipboard(format!("Failed to read image: {}", e)))?;
            return Ok(ClipboardSnapshot::Image(image));
        }

        Ok(ClipboardSnapshot::Empty)
    }

    pub fn write_clipboard_file(path: &Path) -> Result<()> {
        context()?
            .set_files(vec![path.to_string_lossy().into_owned()])
            .map_err(|e| DecentPasteError::Clipboard(format!("Failed to set file: {}", e)))
    }

    pub fn write_clipboard_image(image: RgbaImage) -> Result<()> {
        let data = RustImageData::from_dynamic_image(image::DynamicImage::ImageRgba8(image));
        context()?
            .set_image(data)
            .map_err(|e| DecentPasteError::Clipboard(format!("Failed to set image: {}", e)))
    }
}

#[cfg(desktop)]
pub use desktop::{read_snapshot, write_clipboard_file, write_clipboard_image};

#[cfg(mobile)]
pub fn read_snapshot() -> Result<ClipboardSnapshot> {
    Ok(ClipboardSnapshot::Empty)
}

#[cfg(mobile)]
pub fn write_clipboard_file(_path: &Path) -> Result<()> {
    Err(DecentPasteError::NotSupported(
        "File clipboard is not supported on mobile".into(),
    ))
}

#[cfg(mobile)]
pub fn write_clipboard_image(_image: RgbaImage) -> Result<()> {
    Err(DecentPasteError::NotSupported(
        "Image clipboard is not supported on mobile".into(),
    ))
}
