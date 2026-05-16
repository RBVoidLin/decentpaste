use std::path::PathBuf;
use std::sync::OnceLock;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use crate::error::{DecentPasteError, Result};

static FILE_CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Maximum cache size in bytes (100MB)
const MAX_CACHE_SIZE_BYTES: u64 = 100 * 1024 * 1024;

/// Cached file metadata stored alongside files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFileInfo {
    pub name: String,
    pub original_size: u64,
    pub stored_at: DateTime<Utc>,
}

fn get_cache_dir() -> Result<PathBuf> {
    FILE_CACHE_DIR.get().cloned().ok_or_else(|| {
        DecentPasteError::Storage("File cache directory not initialized. Call init_file_cache first.".into())
    })
}

pub fn init_file_cache(data_dir: &PathBuf) -> Result<()> {
    let cache_dir = data_dir.join("file_cache");
    std::fs::create_dir_all(&cache_dir)?;
    FILE_CACHE_DIR.set(cache_dir).map_err(|_| {
        DecentPasteError::Storage("File cache directory already initialized".into())
    })?;
    Ok(())
}

/// Save data to cache and return the cache path
pub fn save_to_cache(data: &[u8], name: &str) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;

    // Generate unique filename with timestamp to avoid collisions
    let unique_id = uuid::Uuid::new_v4().to_string();
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let stored_name = format!("{}_{}.{}", unique_id, sanitize_filename(name), extension);
    let file_path = cache_dir.join(&stored_name);

    std::fs::write(&file_path, data)?;

    Ok(file_path)
}

/// Load file content from a cached path
pub fn load_from_cache(path: &PathBuf) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        DecentPasteError::Storage(format!("Failed to read cached file: {}", e))
    })
}

/// Clean up expired files on startup.
/// Removes all files in the cache directory (simple cleanup strategy).
pub fn cleanup_cache() -> Result<usize> {
    let cache_dir = get_cache_dir()?;

    if !cache_dir.exists() {
        return Ok(0);
    }

    let mut removed_count = 0;

    for entry in std::fs::read_dir(&cache_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("Failed to remove old cache file {:?}: {}", path, e);
            } else {
                removed_count += 1;
            }
        }
    }

    tracing::info!("Cleaned up {} files from file cache", removed_count);
    Ok(removed_count)
}

/// Get cache directory path
pub fn get_cache_path() -> Result<PathBuf> {
    get_cache_dir()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}