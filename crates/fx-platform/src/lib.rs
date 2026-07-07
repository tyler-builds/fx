//! fx-platform — OS integration for the explorer. First resident: shell
//! icon + thumbnail extraction.
//!
//! `IShellItemImageFactory::GetImage` serves both needs through one API:
//! with `SIIGBF_ICONONLY` it returns the file-association icon; without it,
//! the shell's thumbnail (falling back to the icon) — the same images
//! Explorer shows. Extraction runs on a dedicated COM (STA) worker thread;
//! the UI sends requests and receives raw RGBA back over channels, so this
//! crate stays free of any UI dependency.

pub mod shell;

/// A mounted volume for the sidebar: its root, friendly label, and capacity.
pub struct DriveInfo {
    pub path: std::path::PathBuf,
    /// Volume label, e.g. "Windows" ("Local Disk" if unnamed).
    pub label: String,
    pub total: u64,
    pub free: u64,
}

impl DriveInfo {
    /// The drive letter, e.g. 'C'.
    pub fn letter(&self) -> char {
        self.path
            .to_string_lossy()
            .chars()
            .next()
            .unwrap_or('?')
            .to_ascii_uppercase()
    }
}

/// Mounted drives with labels and capacity, for the sidebar.
#[cfg(windows)]
pub fn drive_info() -> Vec<DriveInfo> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW,
    };

    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| {
            let root = format!("{}:\\", (b'A' + bit as u8) as char);
            let wide: Vec<u16> = std::ffi::OsStr::new(&root)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let mut name = [0u16; 64];
            let label = unsafe {
                GetVolumeInformationW(
                    PCWSTR(wide.as_ptr()),
                    Some(&mut name),
                    None,
                    None,
                    None,
                    None,
                )
            }
            .is_ok()
            .then(|| {
                let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                String::from_utf16_lossy(&name[..end])
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Local Disk".into());

            let (mut total, mut free) = (0u64, 0u64);
            unsafe {
                let _ = GetDiskFreeSpaceExW(
                    PCWSTR(wide.as_ptr()),
                    None,
                    Some(&mut total),
                    Some(&mut free),
                );
            }
            DriveInfo {
                path: root.into(),
                label,
                total,
                free,
            }
        })
        .collect()
}

#[cfg(not(windows))]
pub fn drive_info() -> Vec<DriveInfo> {
    vec![DriveInfo {
        path: std::path::PathBuf::from("/"),
        label: "Root".into(),
        total: 0,
        free: 0,
    }]
}

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

/// Cache identity for an icon. Most files share their extension's icon;
/// things with per-file art (thumbnails, .exe, .lnk) are keyed by path.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum IconKey {
    Dir,
    Ext(String),
    Path(PathBuf),
}

pub struct IconRequest {
    pub key: IconKey,
    /// A concrete file to extract from (for `Ext` keys, any file with that
    /// extension).
    pub path: PathBuf,
    /// Requested square size in physical pixels.
    pub size: i32,
    /// Prefer the shell thumbnail (images, video); falls back to the icon.
    pub thumbnail: bool,
}

pub struct IconResult {
    pub key: IconKey,
    /// The request's `size`, echoed back so callers caching multiple sizes
    /// can file the result correctly.
    pub size: i32,
    pub image: Option<RgbaImage>,
}

/// Straight-alpha RGBA8.
#[derive(Clone)]
pub struct RgbaImage {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// Pending requests beyond this are dropped, oldest first. Anything a fast
/// scroll queued and left behind is stale; whatever is actually on screen
/// gets re-requested by the cache.
const QUEUE_CAP: usize = 512;

struct QueueState {
    /// LIFO: the newest request is what the user is looking at right now.
    stack: Vec<IconRequest>,
    /// Dedupe of what's currently queued.
    queued: HashSet<(IconKey, i32)>,
    closed: bool,
}

struct QueueInner {
    state: Mutex<QueueState>,
    cv: Condvar,
}

/// Producer side of the request queue. Dropping it winds the worker down.
pub struct IconQueue(Arc<QueueInner>);

impl IconQueue {
    pub fn push(&self, req: IconRequest) {
        let mut s = self.0.state.lock().unwrap();
        if !s.queued.insert((req.key.clone(), req.size)) {
            return; // identical request already waiting
        }
        s.stack.push(req);
        if s.stack.len() > QUEUE_CAP {
            let dropped = s.stack.remove(0); // oldest = least likely visible
            s.queued.remove(&(dropped.key.clone(), dropped.size));
        }
        drop(s);
        self.0.cv.notify_one();
    }
}

impl Drop for IconQueue {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().closed = true;
        self.0.cv.notify_one();
    }
}

/// Start the extraction worker. `on_result` fires after each result is
/// queued (the UI passes a repaint-request so results appear while idle).
pub fn spawn_icon_worker(
    on_result: impl Fn() + Send + 'static,
) -> (IconQueue, Receiver<IconResult>) {
    let inner = Arc::new(QueueInner {
        state: Mutex::new(QueueState {
            stack: Vec::new(),
            queued: HashSet::new(),
            closed: false,
        }),
        cv: Condvar::new(),
    });
    let (res_tx, res_rx) = channel::<IconResult>();
    let worker_inner = inner.clone();
    std::thread::Builder::new()
        .name("fx-icons".into())
        .spawn(move || icon_worker(worker_inner, res_tx, on_result))
        .expect("spawn fx-icons thread");
    (IconQueue(inner), res_rx)
}

fn next_request(inner: &QueueInner) -> Option<IconRequest> {
    let mut s = inner.state.lock().unwrap();
    loop {
        if let Some(req) = s.stack.pop() {
            s.queued.remove(&(req.key.clone(), req.size));
            return Some(req);
        }
        if s.closed {
            return None;
        }
        s = inner.cv.wait(s).unwrap();
    }
}

#[cfg(windows)]
fn icon_worker(inner: Arc<QueueInner>, tx: Sender<IconResult>, on_result: impl Fn()) {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        // Shell image APIs want an apartment-threaded COM thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    while let Some(req) = next_request(&inner) {
        let image = win::extract(&req.path, req.size, req.thumbnail);
        if tx
            .send(IconResult {
                key: req.key,
                size: req.size,
                image,
            })
            .is_err()
        {
            return;
        }
        on_result();
    }
}

#[cfg(not(windows))]
fn icon_worker(inner: Arc<QueueInner>, tx: Sender<IconResult>, on_result: impl Fn()) {
    while let Some(req) = next_request(&inner) {
        if tx
            .send(IconResult {
                key: req.key,
                size: req.size,
                image: None,
            })
            .is_err()
        {
            return;
        }
        on_result();
    }
}

#[cfg(windows)]
mod win {
    use super::RgbaImage;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT,
    };

    pub fn extract(path: &Path, size: i32, thumbnail: bool) -> Option<RgbaImage> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let factory: IShellItemImageFactory =
            unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }.ok()?;
        let sz = SIZE { cx: size, cy: size };
        let hbmp = if thumbnail {
            // Thumbnail preferred; the shell falls back to the icon itself
            // for most types, but be explicit if it refuses outright.
            unsafe { factory.GetImage(sz, SIIGBF_RESIZETOFIT) }
                .or_else(|_| unsafe { factory.GetImage(sz, SIIGBF_ICONONLY | SIIGBF_RESIZETOFIT) })
        } else {
            unsafe { factory.GetImage(sz, SIIGBF_ICONONLY | SIIGBF_RESIZETOFIT) }
        }
        .ok()?;
        let image = hbitmap_to_rgba(hbmp);
        unsafe {
            let _ = DeleteObject(hbmp.into());
        }
        image
    }

    /// Convert an HBITMAP (32-bit top-down) to straight-alpha RGBA. Shared
    /// with the shell-menu enumeration for menu-item icons.
    pub(crate) fn hbitmap_to_rgba(hbmp: HBITMAP) -> Option<RgbaImage> {
        unsafe {
            let mut bmp = BITMAP::default();
            if GetObjectW(
                hbmp.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bmp as *mut _ as *mut c_void),
            ) == 0
            {
                return None;
            }
            let (w, h) = (bmp.bmWidth, bmp.bmHeight);
            if w <= 0 || h <= 0 || w > 4096 || h > 4096 {
                return None;
            }
            let mut info = BITMAPINFO::default();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // negative = top-down rows
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            };
            let mut pixels = vec![0u8; (w as usize) * (h as usize) * 4];
            let hdc = CreateCompatibleDC(None);
            let lines = GetDIBits(
                hdc,
                hbmp,
                0,
                h as u32,
                Some(pixels.as_mut_ptr() as *mut c_void),
                &mut info,
                DIB_RGB_COLORS,
            );
            let _ = DeleteDC(hdc);
            if lines == 0 {
                return None;
            }
            // BGRA -> RGBA.
            for px in pixels.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            // Some association icons come back with a zeroed alpha channel
            // (legacy 24-bit sources); render those opaque, not invisible.
            if pixels.chunks_exact(4).all(|px| px[3] == 0) {
                for px in pixels.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }
            Some(RgbaImage {
                width: w as usize,
                height: h as usize,
                pixels,
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

        #[test]
        fn extracts_real_shell_icons() {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            // A folder icon...
            let img = extract(Path::new("C:\\Windows"), 32, false).expect("folder icon");
            assert!(img.width > 0 && img.height > 0);
            assert_eq!(img.pixels.len(), img.width * img.height * 4);
            // ...and a per-file icon for an executable that always exists.
            let img = extract(Path::new("C:\\Windows\\notepad.exe"), 32, false).expect("exe icon");
            assert!(
                img.pixels.iter().any(|&b| b != 0),
                "icon should have visible pixels"
            );
            // Nonexistent paths fail cleanly.
            assert!(extract(
                Path::new("C:\\definitely\\not\\a\\real\\file.xyz"),
                32,
                false
            )
            .is_none());
        }
    }
}
