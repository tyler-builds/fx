//! Synthetic workloads for the speed spike: an on-disk generator (to test
//! real enumeration) and an in-memory generator (to stress rendering and
//! filtering past what we'd wait for disk to produce).

use crate::entry::{now_unix, Entry};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

pub enum GenMsg {
    Progress(usize),
    Done { count: usize, elapsed: Duration },
    Error(String),
}

const EXTS: &[&str] = &["txt", "log", "rs", "toml", "png", "dll", "json", "md"];
const WORDS: &[&str] = &[
    "report", "backup", "invoice", "photo", "config", "kernel", "session", "archive", "render",
    "index", "draft", "export", "cache", "notes",
];

fn synth_name(i: usize) -> String {
    // Deterministic but varied names so fuzzy filtering has something real
    // to chew on. (No RNG dep needed for a spike.)
    let w1 = WORDS[i % WORDS.len()];
    let w2 = WORDS[(i / WORDS.len() + 3) % WORDS.len()];
    let ext = EXTS[i % EXTS.len()];
    format!("{w1}_{w2}_{i:06}.{ext}")
}

/// Create `count` empty files inside `dir` on a worker thread.
pub fn spawn_generate(dir: PathBuf, count: usize) -> Receiver<GenMsg> {
    let (tx, rx) = channel::<GenMsg>();
    std::thread::Builder::new()
        .name("fx-generate".into())
        .spawn(move || {
            let start = Instant::now();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                let _ = tx.send(GenMsg::Error(format!("{}: {e}", dir.display())));
                return;
            }
            for i in 0..count {
                let path = dir.join(synth_name(i));
                // Skip files that already exist so re-runs are fast no-ops.
                if !path.exists() {
                    if let Err(e) = std::fs::File::create(&path) {
                        let _ = tx.send(GenMsg::Error(format!("{}: {e}", path.display())));
                        return;
                    }
                }
                if i % 2000 == 0 && tx.send(GenMsg::Progress(i)).is_err() {
                    return;
                }
            }
            let _ = tx.send(GenMsg::Done {
                count,
                elapsed: start.elapsed(),
            });
        })
        .expect("spawn fx-generate thread");
    rx
}

/// Build `count` entries directly in memory: pure UI/filter stress, no disk.
pub fn synthetic_entries(count: usize) -> Vec<Entry> {
    let now = now_unix();
    (0..count)
        .map(|i| {
            // Cheap deterministic "hash" for size/date variety.
            let h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let is_dir = i % 23 == 0;
            let size = if is_dir { 0 } else { h % 50_000_000 };
            let modified = now - (h % 63_072_000) as i64; // within ~2 years
            Entry::new(synth_name(i), size, modified, is_dir)
        })
        .collect()
}
