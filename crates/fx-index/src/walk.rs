//! Fallback index builder: a multithreaded recursive directory walk. Works
//! without elevation and on non-NTFS volumes, at the cost of touching every
//! directory (minutes on a large volume vs seconds for the MFT path).
//!
//! Produces the same index shape as the MFT backend, with synthetic ids
//! from a shared counter (files get ids too in v2 — the mutation API is
//! keyed by id). No journal metadata: a walk index can't be tailed.

use crate::{send_progress, FileIndex, IndexBuilder, IndexMsg};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;

const ROOT_ID: u64 = 1;
const PROGRESS_EVERY: usize = 50_000;

type Row = (String, u64, u64, bool); // name, id, parent id, is_dir

struct Shared {
    /// Directories waiting to be scanned: (dir id, path).
    queue: Mutex<Vec<(u64, PathBuf)>>,
    /// Workers currently scanning a directory (for termination detection).
    active: AtomicUsize,
    next_id: AtomicU64,
    counted: AtomicUsize,
}

pub fn build(root: &Path, tx: &Sender<IndexMsg>) -> Result<FileIndex, String> {
    // Fail fast if the root itself is unreadable.
    std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;

    let shared = Shared {
        queue: Mutex::new(vec![(ROOT_ID, root.to_path_buf())]),
        active: AtomicUsize::new(0),
        next_id: AtomicU64::new(ROOT_ID + 1),
        counted: AtomicUsize::new(0),
    };
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);

    let mut results: Vec<Vec<Row>> = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let shared = &shared;
                s.spawn(move || {
                    let mut rows: Vec<Row> = Vec::new();
                    loop {
                        // Claim the job and bump `active` under the same
                        // lock: a worker that later finds the queue empty
                        // AND active == 0 can be certain no scan is still
                        // running that might push more directories.
                        let job = {
                            let mut q = shared.queue.lock().unwrap();
                            let j = q.pop();
                            if j.is_some() {
                                shared.active.fetch_add(1, Ordering::SeqCst);
                            }
                            j
                        };
                        let Some((dir_id, dir_path)) = job else {
                            if shared.active.load(Ordering::SeqCst) == 0 {
                                break;
                            }
                            std::thread::yield_now();
                            continue;
                        };
                        scan_dir(shared, dir_id, &dir_path, &mut rows, tx);
                        shared.active.fetch_sub(1, Ordering::SeqCst);
                    }
                    rows
                })
            })
            .collect();
        results = handles.into_iter().map(|h| h.join().unwrap()).collect();
    });

    let total: usize = results.iter().map(Vec::len).sum();
    let mut builder = IndexBuilder::with_capacity(total);
    for rows in results {
        for (name, id, parent, is_dir) in rows {
            builder.push(&name, id, parent, is_dir);
        }
    }
    Ok(builder.finish(root.to_path_buf(), ROOT_ID))
}

fn scan_dir(
    shared: &Shared,
    dir_id: u64,
    dir_path: &Path,
    rows: &mut Vec<Row>,
    tx: &Sender<IndexMsg>,
) {
    // Unreadable directories are simply skipped: the fallback indexes what
    // it can see. (The browse view is where per-folder errors surface.)
    let Ok(read) = std::fs::read_dir(dir_path) else {
        return;
    };
    let mut local = 0usize;
    for dent in read.flatten() {
        let Ok(ft) = dent.file_type() else { continue };
        // Never descend reparse points (junctions, symlinks): they alias or
        // cycle, and Everything-style indexes list them without following.
        let is_dir = ft.is_dir() && !ft.is_symlink();
        let name = dent.file_name().to_string_lossy().into_owned();
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        rows.push((name, id, dir_id, is_dir));
        if is_dir {
            shared.queue.lock().unwrap().push((id, dent.path()));
        }
        local += 1;
    }
    let counted = shared.counted.fetch_add(local, Ordering::Relaxed) + local;
    if counted % PROGRESS_EVERY < local {
        send_progress(tx, counted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn walk_indexes_a_real_tree() {
        let dir = std::env::temp_dir().join("fx-index-walk-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("top.txt"), b"").unwrap();
        std::fs::write(dir.join("a/mid.txt"), b"").unwrap();
        std::fs::write(dir.join("a/b/deep_report.txt"), b"").unwrap();

        let (tx, _rx) = channel();
        let index = build(&dir, &tx).unwrap();
        assert_eq!(index.len(), 5); // a, b, top.txt, mid.txt, deep_report.txt

        let out = index.search("deep_report", 10);
        assert_eq!(out.total, 1);
        assert_eq!(
            index.resolve_path(out.hits[0]),
            dir.join("a").join("b").join("deep_report.txt")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
