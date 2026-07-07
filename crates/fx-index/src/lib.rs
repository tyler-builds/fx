//! fx-index — whole-volume filename index for instant drive-wide search.
//!
//! The Everything (voidtools) approach: read the NTFS master file table in
//! one pass via `FSCTL_ENUM_USN_DATA`, then keep the index alive by tailing
//! the USN change journal. A parallel directory walk serves as the
//! unprivileged fallback (build only; no journal without a volume handle).
//!
//! Storage (v2) is column-oriented with all names packed into one byte
//! arena: no per-name allocation, cache-friendly scans, and roughly half
//! the memory of the naive struct-per-file layout. Entries are never
//! removed — deletes tombstone a flag bit — so indices held by searches
//! stay valid while the journal mutates the index. Id lookup is a binary
//! search over a build-time sorted array, with a small hash overlay for
//! entries created after the build.

#[cfg(windows)]
mod mft;
pub mod persist;
#[cfg(windows)]
mod usn;
mod walk;

#[cfg(windows)]
pub use usn::{spawn_tail, TailMsg};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

pub(crate) const FLAG_DIR: u8 = 1;
pub(crate) const FLAG_DELETED: u8 = 2;

pub struct FileIndex {
    /// Volume root the ids/paths are relative to, e.g. `C:\`.
    pub root: PathBuf,
    pub root_id: u64,
    /// USN journal identity + position this index is synchronized to
    /// (0 when unknown, e.g. walk backend).
    pub journal_id: u64,
    pub next_usn: i64,

    // Column-oriented entry storage. `names` is the shared UTF-8 arena;
    // renames append the new name and repoint the span (the old bytes leak
    // until the next save compacts — bounded, cheap).
    names: Vec<u8>,
    offs: Vec<u32>,
    lens: Vec<u16>,
    parents: Vec<u64>,
    ids: Vec<u64>,
    flags: Vec<u8>,

    /// Entry indices sorted by id, built once at construction.
    base_sorted: Vec<u32>,
    /// Ids added after construction (journal creates): id -> entry index.
    overlay: HashMap<u64, u32>,
    /// Live (non-tombstoned) entry count.
    live: usize,
}

/// Append-only builder the backends push into; `finish` computes the
/// sorted id lookup.
pub struct IndexBuilder {
    names: Vec<u8>,
    offs: Vec<u32>,
    lens: Vec<u16>,
    parents: Vec<u64>,
    ids: Vec<u64>,
    flags: Vec<u8>,
}

impl IndexBuilder {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            names: Vec::with_capacity(n * 16),
            offs: Vec::with_capacity(n),
            lens: Vec::with_capacity(n),
            parents: Vec::with_capacity(n),
            ids: Vec::with_capacity(n),
            flags: Vec::with_capacity(n),
        }
    }

    pub fn push(&mut self, name: &str, id: u64, parent: u64, is_dir: bool) {
        // NTFS names cap at 255 UTF-16 units; the u16 span length is ample,
        // but clamp defensively rather than corrupt the arena.
        let bytes = &name.as_bytes()[..name.len().min(u16::MAX as usize)];
        self.offs.push(self.names.len() as u32);
        self.lens.push(bytes.len() as u16);
        self.names.extend_from_slice(bytes);
        self.parents.push(parent);
        self.ids.push(id);
        self.flags.push(if is_dir { FLAG_DIR } else { 0 });
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn finish(self, root: PathBuf, root_id: u64) -> FileIndex {
        let mut base_sorted: Vec<u32> = (0..self.ids.len() as u32).collect();
        base_sorted.sort_unstable_by_key(|&i| self.ids[i as usize]);
        let live = self.ids.len();
        FileIndex {
            root,
            root_id,
            journal_id: 0,
            next_usn: 0,
            names: self.names,
            offs: self.offs,
            lens: self.lens,
            parents: self.parents,
            ids: self.ids,
            flags: self.flags,
            base_sorted,
            overlay: HashMap::new(),
            live,
        }
    }
}

pub struct SearchOutput {
    /// Entry indices, in index order, capped at the requested limit.
    pub hits: Vec<u32>,
    /// Total matches before capping.
    pub total: usize,
    pub ms: f32,
}

pub enum IndexMsg {
    Progress(usize),
    /// Non-fatal notes worth surfacing (e.g. "MFT unavailable, walking").
    Note(String),
    Done {
        index: FileIndex,
        elapsed: Duration,
        backend: &'static str,
    },
    Error(String),
}

impl FileIndex {
    pub fn with_journal(mut self, journal_id: u64, next_usn: i64) -> Self {
        self.journal_id = journal_id;
        self.next_usn = next_usn;
        self
    }

    /// Live (non-deleted) entries.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// All entry slots, including tombstones — the valid range for `name`,
    /// `is_dir`, and search hit indices.
    pub fn raw_len(&self) -> usize {
        self.ids.len()
    }

    pub fn name(&self, idx: u32) -> &str {
        let off = self.offs[idx as usize] as usize;
        let len = self.lens[idx as usize] as usize;
        let bytes = &self.names[off..off + len];
        debug_assert!(std::str::from_utf8(bytes).is_ok());
        // SAFETY: arena bytes are only ever written from &str (builder
        // push / upsert), so they are valid UTF-8. Validation here costs a
        // full extra scan per name on the hottest path in the crate.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    pub fn is_dir(&self, idx: u32) -> bool {
        self.flags[idx as usize] & FLAG_DIR != 0
    }

    pub fn is_deleted(&self, idx: u32) -> bool {
        self.flags[idx as usize] & FLAG_DELETED != 0
    }

    pub fn parent_id(&self, idx: u32) -> u64 {
        self.parents[idx as usize]
    }

    pub(crate) fn entry_id(&self, idx: u32) -> u64 {
        self.ids[idx as usize]
    }

    /// Entry index for a file id: binary search over the build-time order,
    /// then the post-build overlay.
    pub fn lookup(&self, id: u64) -> Option<u32> {
        if let Ok(pos) = self
            .base_sorted
            .binary_search_by_key(&id, |&i| self.ids[i as usize])
        {
            return Some(self.base_sorted[pos]);
        }
        self.overlay.get(&id).copied()
    }

    /// Create or update an entry from a journal record. Renames repoint the
    /// name span; moves update the parent; re-creates clear the tombstone.
    pub fn upsert(&mut self, id: u64, parent: u64, name: &str, is_dir: bool) {
        let bytes = &name.as_bytes()[..name.len().min(u16::MAX as usize)];
        match self.lookup(id) {
            Some(idx) => {
                let i = idx as usize;
                if self.flags[i] & FLAG_DELETED != 0 {
                    self.live += 1;
                }
                if self.name(idx).as_bytes() != bytes {
                    self.offs[i] = self.names.len() as u32;
                    self.lens[i] = bytes.len() as u16;
                    self.names.extend_from_slice(bytes);
                }
                self.parents[i] = parent;
                self.flags[i] = if is_dir { FLAG_DIR } else { 0 };
            }
            None => {
                let idx = self.ids.len() as u32;
                self.offs.push(self.names.len() as u32);
                self.lens.push(bytes.len() as u16);
                self.names.extend_from_slice(bytes);
                self.parents.push(parent);
                self.ids.push(id);
                self.flags.push(if is_dir { FLAG_DIR } else { 0 });
                self.overlay.insert(id, idx);
                self.live += 1;
            }
        }
    }

    /// Tombstone an entry. Its slot (and any index into it) stays valid.
    pub fn remove(&mut self, id: u64) {
        if let Some(idx) = self.lookup(id) {
            let i = idx as usize;
            if self.flags[i] & FLAG_DELETED == 0 {
                self.flags[i] |= FLAG_DELETED;
                self.live -= 1;
            }
        }
    }

    /// Case-insensitive substring search across every live name, fanned out
    /// over all cores. Hits come back in index order.
    pub fn search(&self, query: &str, limit: usize) -> SearchOutput {
        let t0 = Instant::now();
        if query.is_empty() {
            return SearchOutput {
                hits: Vec::new(),
                total: 0,
                ms: 0.0,
            };
        }
        let ql = query.to_lowercase();
        let n = self.raw_len();
        let threads = std::thread::available_parallelism()
            .map_or(4, |t| t.get())
            .min(16);
        let chunk = n.div_ceil(threads).max(1);

        let mut per_chunk: Vec<(Vec<u32>, usize)> = Vec::new();
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .step_by(chunk)
                .map(|start| {
                    let ql = ql.as_str();
                    let end = (start + chunk).min(n);
                    s.spawn(move || {
                        let mut hits = Vec::new();
                        let mut total = 0usize;
                        for i in start..end {
                            let idx = i as u32;
                            if self.flags[i] & FLAG_DELETED == 0
                                && name_contains_ci(self.name(idx), ql)
                            {
                                total += 1;
                                if hits.len() < limit {
                                    hits.push(idx);
                                }
                            }
                        }
                        (hits, total)
                    })
                })
                .collect();
            per_chunk = handles.into_iter().map(|h| h.join().unwrap()).collect();
        });

        let total = per_chunk.iter().map(|(_, t)| t).sum();
        let mut hits = Vec::new();
        for (chunk_hits, _) in per_chunk {
            let room = limit - hits.len();
            hits.extend(chunk_hits.into_iter().take(room));
            if hits.len() >= limit {
                break;
            }
        }
        SearchOutput {
            hits,
            total,
            ms: t0.elapsed().as_secs_f32() * 1000.0,
        }
    }

    /// Search only within `candidates` (see the app's incremental ladder).
    /// Correct only when `candidates` is the complete match set of a query
    /// this one extends.
    pub fn search_within(&self, query: &str, limit: usize, candidates: &[u32]) -> SearchOutput {
        let t0 = Instant::now();
        if query.is_empty() {
            return SearchOutput {
                hits: Vec::new(),
                total: 0,
                ms: 0.0,
            };
        }
        let ql = query.to_lowercase();
        let mut hits = Vec::new();
        let mut total = 0usize;
        for &idx in candidates {
            if !self.is_deleted(idx) && name_contains_ci(self.name(idx), &ql) {
                total += 1;
                if hits.len() < limit {
                    hits.push(idx);
                }
            }
        }
        SearchOutput {
            hits,
            total,
            ms: t0.elapsed().as_secs_f32() * 1000.0,
        }
    }

    /// Full path of an entry, resolved by climbing the parent chain.
    pub fn resolve_path(&self, idx: u32) -> PathBuf {
        let mut parts: Vec<u32> = Vec::with_capacity(8);
        parts.push(idx);
        let mut cur = idx;
        let mut guard = 0;
        while self.parents[cur as usize] != self.root_id && guard < 128 {
            match self.lookup(self.parents[cur as usize]) {
                Some(pi) => {
                    parts.push(pi);
                    cur = pi;
                }
                None => break, // orphan; render root-relative
            }
            guard += 1;
        }
        let mut path = self.root.clone();
        for &p in parts.iter().rev() {
            path.push(self.name(p));
        }
        path
    }

    /// Parent directory of an entry, for "open containing folder".
    pub fn resolve_parent(&self, idx: u32) -> PathBuf {
        let mut p = self.resolve_path(idx);
        p.pop();
        p
    }
}

/// ASCII case-insensitive substring test; falls back to full lowercasing
/// only for names that are not pure ASCII (rare on real volumes).
fn name_contains_ci(name: &str, query_lower: &str) -> bool {
    let n = name.as_bytes();
    let q = query_lower.as_bytes();
    if q.len() > n.len() {
        return false;
    }
    if name.is_ascii() {
        n.windows(q.len()).any(|w| w.eq_ignore_ascii_case(q))
    } else {
        name.to_lowercase().contains(query_lower)
    }
}

/// Build an index of the volume containing `root` on a worker thread.
/// Tries the MFT fast path first; falls back to a parallel walk (with a
/// `Note` explaining why) when the volume can't be opened raw.
pub fn spawn_build(root: PathBuf) -> Receiver<IndexMsg> {
    let (tx, rx) = channel::<IndexMsg>();
    std::thread::Builder::new()
        .name("fx-index".into())
        .spawn(move || {
            let start = Instant::now();
            #[cfg(windows)]
            {
                match mft::build(&root, &tx) {
                    Ok(index) => {
                        let _ = tx.send(IndexMsg::Done {
                            index,
                            elapsed: start.elapsed(),
                            backend: "mft",
                        });
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(IndexMsg::Note(format!(
                            "MFT fast path unavailable ({e}); falling back to directory walk"
                        )));
                    }
                }
            }
            match walk::build(&root, &tx) {
                Ok(index) => {
                    let _ = tx.send(IndexMsg::Done {
                        index,
                        elapsed: start.elapsed(),
                        backend: "walk",
                    });
                }
                Err(e) => {
                    let _ = tx.send(IndexMsg::Error(e));
                }
            }
        })
        .expect("spawn fx-index thread");
    rx
}

pub(crate) fn send_progress(tx: &Sender<IndexMsg>, count: usize) {
    let _ = tx.send(IndexMsg::Progress(count));
}

/// True when the MFT fast path can be used (process is elevated). Lets UIs
/// explain the walk fallback and offer elevation instead of silently being
/// 6x slower.
#[cfg(windows)]
pub fn mft_available() -> bool {
    mft::volume_openable('C')
}

#[cfg(not(windows))]
pub fn mft_available() -> bool {
    false
}

/// Current USN journal identity + position for the volume, if this process
/// can read it (elevated). Compare with a loaded index's stored values to
/// judge freshness.
#[cfg(windows)]
pub fn journal_position(letter: char) -> Option<(u64, i64)> {
    mft::journal_position(letter)
}

#[cfg(not(windows))]
pub fn journal_position(_letter: char) -> Option<(u64, i64)> {
    None
}

/// Relaunch the current executable elevated (UAC prompt). Returns true if
/// the new process was started; the caller should then exit this one.
#[cfg(windows)]
pub fn relaunch_elevated() -> bool {
    use std::os::windows::ffi::OsStrExt;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    const SW_SHOWNORMAL: i32 = 1;
    let inst = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Per the ShellExecute contract, values > 32 indicate success.
    inst as usize > 32
}

#[cfg(not(windows))]
pub fn relaunch_elevated() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn test_index() -> FileIndex {
        // C:\ (root id 1)
        //   docs\            id 10
        //     Report.txt     id 11
        //     Übung.txt      id 12
        //   readme.md        id 13
        let mut b = IndexBuilder::with_capacity(4);
        b.push("docs", 10, 1, true);
        b.push("Report.txt", 11, 10, false);
        b.push("\u{dc}bung.txt", 12, 10, false);
        b.push("readme.md", 13, 1, false);
        b.finish(PathBuf::from("C:\\"), 1)
    }

    #[test]
    fn search_is_case_insensitive_substring() {
        let idx = test_index();
        let out = idx.search("report", 100);
        assert_eq!(out.total, 1);
        assert_eq!(idx.name(out.hits[0]), "Report.txt");
        // "re" hits Report.txt and readme.md but not docs.
        assert_eq!(idx.search("RE", 100).total, 2);
        assert_eq!(idx.search("nothing", 100).total, 0);
        assert_eq!(idx.search("", 100).total, 0);
    }

    #[test]
    fn non_ascii_names_match() {
        let idx = test_index();
        assert_eq!(idx.search("\u{fc}bung", 100).total, 1);
    }

    #[test]
    fn limit_caps_hits_but_not_total() {
        let idx = test_index();
        let out = idx.search("t", 1); // matches Report.txt + Übung.txt
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.total, 2);
    }

    #[test]
    fn search_within_narrows_like_a_full_search() {
        let idx = test_index();
        let broad = idx.search("t", 100);
        assert_eq!(broad.total, 2);
        let narrowed = idx.search_within("txt", 100, &broad.hits);
        let full = idx.search("txt", 100);
        assert_eq!(narrowed.hits, full.hits);
        assert_eq!(narrowed.total, full.total);
        assert_eq!(idx.search_within("txt", 100, &[]).total, 0);
    }

    #[test]
    fn paths_resolve_through_parents() {
        let idx = test_index();
        let hit = idx.search("report", 10).hits[0];
        assert_eq!(idx.resolve_path(hit), PathBuf::from("C:\\docs\\Report.txt"));
        assert_eq!(idx.resolve_parent(hit), PathBuf::from("C:\\docs"));
        let readme = idx.search("readme", 10).hits[0];
        assert_eq!(idx.resolve_path(readme), PathBuf::from("C:\\readme.md"));
    }

    #[test]
    fn remove_tombstones_and_upsert_revives() {
        let mut idx = test_index();
        assert_eq!(idx.len(), 4);
        idx.remove(11);
        assert_eq!(idx.len(), 3);
        assert_eq!(
            idx.search("report", 10).total,
            0,
            "deleted entries must not match"
        );
        idx.remove(11); // double delete is a no-op
        assert_eq!(idx.len(), 3);
        idx.upsert(11, 10, "Report.txt", false); // re-created
        assert_eq!(idx.len(), 4);
        assert_eq!(idx.search("report", 10).total, 1);
    }

    #[test]
    fn upsert_renames_and_moves() {
        let mut idx = test_index();
        // Rename readme.md -> CHANGELOG.md and move it into docs\.
        idx.upsert(13, 10, "CHANGELOG.md", false);
        assert_eq!(idx.len(), 4);
        assert_eq!(idx.search("readme", 10).total, 0);
        let hit = idx.search("changelog", 10).hits[0];
        assert_eq!(
            idx.resolve_path(hit),
            PathBuf::from("C:\\docs\\CHANGELOG.md")
        );
    }

    #[test]
    fn upsert_creates_new_entries_via_overlay() {
        let mut idx = test_index();
        idx.upsert(99, 10, "brand_new.rs", false);
        assert_eq!(idx.len(), 5);
        assert_eq!(idx.lookup(99), Some(4));
        let hit = idx.search("brand_new", 10).hits[0];
        assert_eq!(
            idx.resolve_path(hit),
            PathBuf::from("C:\\docs\\brand_new.rs")
        );
        // New directory + child resolving through the overlay.
        idx.upsert(100, 1, "newdir", true);
        idx.upsert(101, 100, "inside.txt", false);
        let hit = idx.search("inside", 10).hits[0];
        assert_eq!(
            idx.resolve_path(hit),
            PathBuf::from("C:\\newdir\\inside.txt")
        );
    }
}
