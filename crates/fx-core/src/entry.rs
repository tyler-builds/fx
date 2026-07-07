use std::time::{SystemTime, UNIX_EPOCH};

/// One row in a directory listing. Kept deliberately flat and compact:
/// at 500k entries this struct's size is what decides whether we fit in a
/// few tens of MB or blow past it.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    /// Cached lowercase name so filtering and name-sorting never re-lowercase
    /// per keystroke. Costs memory; buys per-keystroke milliseconds.
    pub name_lower: String,
    /// File size in bytes. 0 for directories.
    pub size: u64,
    /// Modified time as unix seconds, 0 if unavailable.
    pub modified: i64,
    pub is_dir: bool,
}

impl Entry {
    pub fn new(name: String, size: u64, modified: i64, is_dir: bool) -> Self {
        let name_lower = name.to_lowercase();
        Self {
            name,
            name_lower,
            size,
            modified,
            is_dir,
        }
    }

    pub fn from_metadata(name: String, meta: &std::fs::Metadata) -> Self {
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self::new(name, size, modified, is_dir)
    }
}

/// Unix seconds for "now"; used by the synthetic generators.
pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
