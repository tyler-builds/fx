//! UI-side icon cache: maps `IconKey`s to egui textures, feeding requests
//! to the fx-platform COM worker and draining its results each frame.
//! Rendering never blocks — rows draw without an icon until it arrives
//! (usually next frame), the conduit texture-pipeline pattern.

use eframe::egui;
use fx_platform::{spawn_icon_worker, IconKey, IconQueue, IconRequest, IconResult};
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Receiver;

/// Keys kept before evicting the least-recently-drawn quarter.
const CACHE_CAP: usize = 2048;
/// A visible tile still Loading after this many frames re-requests: its
/// original request was likely dropped from the bounded queue during a
/// fast scroll. (The queue dedupes, so re-requests are cheap.)
const RETRY_FRAMES: u64 = 30;

/// Types whose per-file art is worth a per-path cache entry.
const THUMB_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff"];
const PER_FILE_EXTS: &[&str] = &["exe", "lnk", "ico"];

enum IconState {
    Loading {
        since: u64,
    },
    Ready {
        tex: egui::TextureHandle,
        last_used: u64,
    },
    Failed {
        since: u64,
    },
}

impl IconState {
    fn stamp(&self) -> u64 {
        match self {
            IconState::Loading { since } | IconState::Failed { since } => *since,
            IconState::Ready { last_used, .. } => *last_used,
        }
    }
}

pub struct IconCache {
    tx: IconQueue,
    rx: Receiver<IconResult>,
    /// Keyed by (identity, logical pixel size): the same file at list size
    /// and grid size is two textures.
    map: HashMap<(IconKey, u32), IconState>,
    frame: u64,
    tex_seq: u64,
    /// Any visible slot drew without its icon this frame. The app keeps a
    /// slow repaint alive while true, so retries fire even when idle.
    missing: bool,
}

/// Physical extraction size for a logical draw size (2x for hi-dpi).
fn physical(logical_px: u32) -> i32 {
    (logical_px * 2) as i32
}

impl IconCache {
    pub fn new(ctx: &egui::Context) -> Self {
        let repaint = ctx.clone();
        let (tx, rx) = spawn_icon_worker(move || repaint.request_repaint());
        Self {
            tx,
            rx,
            map: HashMap::new(),
            frame: 0,
            tex_seq: 0,
            missing: false,
        }
    }

    /// True when something visible is still waiting on extraction.
    pub fn missing_any(&self) -> bool {
        self.missing
    }

    /// Drain arrived extractions into textures and evict if over cap.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        self.missing = false;
        while let Ok(res) = self.rx.try_recv() {
            let state = match res.image {
                Some(img) => {
                    self.tex_seq += 1;
                    let color = egui::ColorImage::from_rgba_unmultiplied(
                        [img.width, img.height],
                        &img.pixels,
                    );
                    let tex = ctx.load_texture(
                        format!("icon-{}", self.tex_seq),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    IconState::Ready {
                        tex,
                        last_used: self.frame,
                    }
                }
                None => IconState::Failed { since: self.frame },
            };
            let logical = (res.size / 2).max(1) as u32;
            self.map.insert((res.key, logical), state);
        }

        if self.map.len() > CACHE_CAP {
            // Evict the least-recently-touched quarter, whatever its state
            // (stale Loading entries from scrolled-past tiles included) —
            // but never anything touched this frame.
            let mut stale: Vec<((IconKey, u32), u64)> = self
                .map
                .iter()
                .filter(|(_, v)| v.stamp() < self.frame)
                .map(|(k, v)| (k.clone(), v.stamp()))
                .collect();
            stale.sort_unstable_by_key(|&(_, s)| s);
            for (k, _) in stale.into_iter().take(CACHE_CAP / 4) {
                self.map.remove(&k); // dropping the handle frees the texture
            }
        }
    }

    /// Texture for a file's icon at `logical_px`; queues extraction on
    /// first sight and returns None until it lands.
    pub fn get(&mut self, is_dir: bool, path: &Path, logical_px: u32) -> Option<egui::TextureId> {
        let (key, thumbnail) = key_for(is_dir, path);
        let frame = self.frame;
        match self.map.get_mut(&(key.clone(), logical_px)) {
            Some(IconState::Ready { tex, last_used }) => {
                *last_used = frame;
                Some(tex.id())
            }
            Some(IconState::Loading { since }) => {
                self.missing = true;
                // Still visible but nothing arrived: the request probably
                // fell off the bounded queue. Ask again.
                if frame.saturating_sub(*since) >= RETRY_FRAMES {
                    *since = frame;
                    self.tx.push(IconRequest {
                        key,
                        path: path.to_path_buf(),
                        size: physical(logical_px),
                        thumbnail,
                    });
                }
                None
            }
            Some(IconState::Failed { .. }) => None,
            None => {
                self.missing = true;
                self.map.insert(
                    (key.clone(), logical_px),
                    IconState::Loading { since: frame },
                );
                self.tx.push(IconRequest {
                    key,
                    path: path.to_path_buf(),
                    size: physical(logical_px),
                    thumbnail,
                });
                None
            }
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}

fn key_for(is_dir: bool, path: &Path) -> (IconKey, bool) {
    if is_dir {
        return (IconKey::Dir, false);
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if THUMB_EXTS.contains(&ext.as_str()) {
        (IconKey::Path(path.to_path_buf()), true)
    } else if PER_FILE_EXTS.contains(&ext.as_str()) {
        (IconKey::Path(path.to_path_buf()), false)
    } else {
        // Everything else shares its extension's association icon (empty
        // string = the generic file icon).
        (IconKey::Ext(ext), false)
    }
}
