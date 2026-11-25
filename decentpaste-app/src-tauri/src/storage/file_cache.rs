use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use unicode_normalization::UnicodeNormalization;

use crate::error::{DecentPasteError, Result};

static FILE_CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Total size the cache may grow to during a session before the oldest items are evicted.
const MAX_CACHE_SIZE_BYTES: u64 = 500 * 1024 * 1024;

fn get_cache_dir() -> Result<PathBuf> {
    FILE_CACHE_DIR.get().cloned().ok_or_else(|| {
        DecentPasteError::Storage(
            "File cache directory not initialized. Call init_file_cache first.".into(),
        )
    })
}

pub fn init_file_cache(data_dir: &Path) -> Result<()> {
    let cache_dir = data_dir.join("file_cache");
    std::fs::create_dir_all(&cache_dir)?;
    FILE_CACHE_DIR.set(cache_dir).map_err(|_| {
        DecentPasteError::Storage("File cache directory already initialized".into())
    })?;
    Ok(())
}

/// Reserve a fresh slot for `name`. Each item gets its own sub-directory so the
/// file keeps its original name when pasted (Finder / Explorer show that name).
/// Returns `(final_path, part_path)`; write to `part_path`, then rename.
pub fn reserve_cache_path(name: &str) -> Result<(PathBuf, PathBuf)> {
    let dir = get_cache_dir()?.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir)?;
    let final_path = dir.join(sanitize_filename(name));
    let part_path = dir.join(".download.part");
    Ok((final_path, part_path))
}

/// Save data to the cache and return its path.
pub fn save_to_cache(data: &[u8], name: &str) -> Result<PathBuf> {
    let (final_path, _) = reserve_cache_path(name)?;
    std::fs::write(&final_path, data)?;
    Ok(final_path)
}

/// Remove a reserved slot (failed or cancelled download).
pub fn discard_cache_path(path: &Path) {
    if let Some(dir) = path.parent() {
        if let Ok(cache_dir) = get_cache_dir() {
            if dir.starts_with(&cache_dir) && dir != cache_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

/// Clear the whole cache. Run on startup: received media is stored unencrypted,
/// so it is not kept across sessions.
pub fn cleanup_cache() -> Result<usize> {
    let cache_dir = get_cache_dir()?;
    let mut removed_count = 0;

    for entry in std::fs::read_dir(&cache_dir)? {
        let path = entry?.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed_count += 1,
            Err(e) => tracing::warn!("Failed to remove old cache entry {:?}: {}", path, e),
        }
    }

    tracing::info!("Cleaned up {} entries from file cache", removed_count);
    Ok(removed_count)
}

/// Evict the oldest cache items until the cache fits `MAX_CACHE_SIZE_BYTES`.
pub fn enforce_cache_limit() -> Result<()> {
    let cache_dir = get_cache_dir()?;
    let mut items: Vec<(PathBuf, u64, SystemTime)> = Vec::new();

    for entry in std::fs::read_dir(&cache_dir)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let mut size = 0;
        let mut modified = SystemTime::UNIX_EPOCH;
        for file in std::fs::read_dir(&path)?.flatten() {
            if let Ok(meta) = file.metadata() {
                size += meta.len();
                modified = modified.max(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
            }
        }
        items.push((path, size, modified));
    }

    let mut total: u64 = items.iter().map(|i| i.1).sum();
    items.sort_by_key(|i| i.2);
    for (path, size, _) in items {
        if total <= MAX_CACHE_SIZE_BYTES {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(size);
            tracing::debug!("Evicted cache item {:?} ({} bytes)", path, size);
        }
    }
    Ok(())
}

/// Make a peer-supplied name safe on both macOS and Windows.
pub fn sanitize_filename(name: &str) -> String {
    // macOS stores names in NFD; normalize so names compare and display the same everywhere.
    let name: String = name.nfc().collect();
    // Never trust path components from a peer.
    let name = name.rsplit(['/', '\\']).next().unwrap_or("");

    let mut cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    cleaned = cleaned
        .trim_end_matches(['.', ' '])
        .trim_start()
        .to_string();
    if cleaned.chars().count() > 200 {
        cleaned = cleaned.chars().take(200).collect();
    }

    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        cleaned.insert(0, '_');
    }

    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "file".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_filename;

    #[test]
    fn sanitizes_peer_supplied_names() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\Users\\x\\a.png"), "a.png");
        assert_eq!(sanitize_filename("con.txt"), "_con.txt");
        assert_eq!(sanitize_filename("COM1"), "_COM1");
        assert_eq!(sanitize_filename("a:b?.mp4"), "a_b_.mp4");
        assert_eq!(sanitize_filename("name. "), "name");
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename(".."), "file");
        assert_eq!(sanitize_filename("我的 影片.mp4"), "我的 影片.mp4");
        // NFD "é" (e + combining acute) becomes NFC
        assert_eq!(sanitize_filename("e\u{301}.png"), "\u{e9}.png");
    }
}
