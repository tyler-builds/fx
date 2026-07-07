use crate::entry::Entry;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, Receiver};
use std::time::{Duration, Instant};

/// Messages streamed from an enumeration worker to the UI.
pub enum Batch {
    /// A chunk of entries, in on-disk order, stamped with the worker-side
    /// elapsed time at send. (The UI drains on its own frame cadence, so a
    /// UI-side stamp would be quantized to vsync and overstate latency.)
    Entries(Vec<Entry>, Duration),
    /// Enumeration finished. `errors` counts directory entries that could
    /// not be read at all (e.g. access denied) — the UI must surface this,
    /// or a permission-locked folder is indistinguishable from an empty one.
    Done {
        total: usize,
        errors: usize,
        elapsed: Duration,
    },
    Error(String),
}

/// How many entries to accumulate before flushing a batch to the UI.
/// Large enough to amortize channel overhead, small enough that the first
/// paint happens within a frame or two of the directory opening.
const BATCH_SIZE: usize = 4096;
/// Flush even a partial batch this often so slow devices (network drives)
/// still show progress immediately.
const BATCH_LATENCY: Duration = Duration::from_millis(8);

/// Enumerate `path` (non-recursive) on a worker thread, streaming results.
///
/// On Windows, `DirEntry::metadata()` is served from the `FindNextFileW`
/// data the directory walk already produced, so taking size + mtime here is
/// nearly free and avoids a per-file `stat` later.
pub fn spawn_enumerate(path: PathBuf) -> Receiver<Batch> {
    // Small bound: if the UI stalls, the worker parks instead of buffering
    // the whole volume in channel memory.
    let (tx, rx) = sync_channel::<Batch>(64);

    std::thread::Builder::new()
        .name("fx-enumerate".into())
        .spawn(move || {
            let start = Instant::now();
            let read = match std::fs::read_dir(&path) {
                Ok(read) => read,
                Err(e) => {
                    let _ = tx.send(Batch::Error(format!("{}: {e}", path.display())));
                    return;
                }
            };

            let mut batch = Vec::with_capacity(BATCH_SIZE);
            let mut total = 0usize;
            let mut errors = 0usize;
            let mut last_flush = Instant::now();

            for dent in read {
                let dent = match dent {
                    Ok(d) => d,
                    Err(_) => {
                        errors += 1;
                        continue;
                    }
                };
                let name = dent.file_name().to_string_lossy().into_owned();
                let entry = match dent.metadata() {
                    Ok(meta) => Entry::from_metadata(name, &meta),
                    // Metadata can fail (permissions, races); still list the name.
                    Err(_) => Entry::new(name, 0, 0, false),
                };
                batch.push(entry);

                if batch.len() >= BATCH_SIZE || last_flush.elapsed() >= BATCH_LATENCY {
                    total += batch.len();
                    let msg = Batch::Entries(std::mem::take(&mut batch), start.elapsed());
                    if tx.send(msg).is_err() {
                        return; // UI dropped the receiver (navigated away)
                    }
                    batch.reserve(BATCH_SIZE);
                    last_flush = Instant::now();
                }
            }

            total += batch.len();
            if !batch.is_empty() && tx.send(Batch::Entries(batch, start.elapsed())).is_err() {
                return;
            }
            let _ = tx.send(Batch::Done {
                total,
                errors,
                elapsed: start.elapsed(),
            });
        })
        .expect("spawn fx-enumerate thread");

    rx
}
