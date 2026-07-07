//! Headless benchmark + generator so enumeration and filter performance can
//! be measured (and CI-checked later) without opening a window.

use fx_core::{
    filter_sorted, sort_indices, spawn_enumerate, spawn_generate, Batch, Entry, GenMsg, SortKey,
};

pub fn run_bench(dir: Option<&str>) {
    let Some(dir) = dir else {
        eprintln!("usage: fx-app --bench <dir>");
        std::process::exit(2);
    };

    println!("enumerating {dir} ...");
    let rx = spawn_enumerate(dir.into());
    let mut entries: Vec<Entry> = Vec::new();
    let mut first_batch_ms: Option<f32> = None;
    for msg in rx {
        match msg {
            Batch::Entries(mut batch, at) => {
                first_batch_ms.get_or_insert(at.as_secs_f32() * 1000.0);
                entries.append(&mut batch);
            }
            Batch::Done {
                total,
                errors,
                elapsed,
            } => {
                println!(
                    "  {} entries in {:.1} ms (first batch at {:.1} ms, {errors} unreadable)",
                    total,
                    elapsed.as_secs_f32() * 1000.0,
                    first_batch_ms.unwrap_or(0.0),
                );
            }
            Batch::Error(e) => {
                eprintln!("  error: {e}");
                std::process::exit(1);
            }
        }
    }

    let sorted = sort_indices(&entries, SortKey::Name, true);
    println!("  master sort: {:.2} ms", sorted.sort_ms);

    let mut prev: Option<(&str, Vec<u32>)> = None;
    for query in ["", "ker", "repor", "report", "zzzz_no_match"] {
        let prev_ref = prev.as_ref().map(|(q, v)| (*q, v.as_slice()));
        let out = filter_sorted(&entries, &sorted.indices, query, prev_ref);
        println!(
            "  filter {query:?}: {} visible, {:.2} ms{}",
            out.visible.len(),
            out.filter_ms,
            if out.incremental {
                " (incremental)"
            } else {
                ""
            },
        );
        prev = Some((query, out.visible));
    }
}

pub fn run_index(root: Option<&str>) {
    let root = root.unwrap_or("C:\\");
    println!("indexing {root} ...");
    let rx = fx_index::spawn_build(root.into());
    let mut index = None;
    for msg in rx {
        match msg {
            fx_index::IndexMsg::Progress(n) => {
                if n % 500_000 < 50_000 {
                    println!("  {n} ...");
                }
            }
            fx_index::IndexMsg::Note(n) => println!("  note: {n}"),
            fx_index::IndexMsg::Done {
                index: idx,
                elapsed,
                backend,
            } => {
                println!(
                    "  {} entries in {:.2} s ({backend} backend)",
                    idx.len(),
                    elapsed.as_secs_f32(),
                );
                index = Some(idx);
            }
            fx_index::IndexMsg::Error(e) => {
                eprintln!("  error: {e}");
                std::process::exit(1);
            }
        }
    }
    let Some(index) = index else { return };
    for query in ["report", "dll", "windows", "zzzz_no_match", "a"] {
        let out = index.search(query, 100_000);
        println!(
            "  search {query:?}: {} matches in {:.2} ms",
            out.total, out.ms
        );
    }
    // Show a few resolved paths to eyeball correctness.
    let sample = index.search("dll", 3);
    for &hit in &sample.hits {
        println!("  e.g. {}", index.resolve_path(hit).display());
    }

    // Persistence round-trip with timings. Deliberately NOT the app's real
    // index slot: benching a subtree must never clobber the volume index.
    let path = std::env::temp_dir().join("fx-bench-index.bin");
    let t = std::time::Instant::now();
    match fx_index::persist::save(&index, &path) {
        Ok(()) => println!(
            "  saved in {:.2} s -> {}",
            t.elapsed().as_secs_f32(),
            path.display()
        ),
        Err(e) => {
            eprintln!("  save failed: {e}");
            return;
        }
    }
    let size_mb = std::fs::metadata(&path)
        .map(|m| m.len() as f64 / 1e6)
        .unwrap_or(0.0);
    let t = std::time::Instant::now();
    match fx_index::persist::load(&path) {
        Ok(loaded) => {
            println!(
                "  loaded {} entries in {:.2} s ({size_mb:.0} MB on disk)",
                loaded.len(),
                t.elapsed().as_secs_f32(),
            );
            let a = index.search("report", 100_000);
            let b = loaded.search("report", 100_000);
            println!(
                "  reload consistency: {} vs {} matches -> {}",
                a.total,
                b.total,
                if a.hits == b.hits {
                    "identical"
                } else {
                    "MISMATCH"
                },
            );
        }
        Err(e) => eprintln!("  load failed: {e}"),
    }
}

pub fn run_gen(dir: Option<&str>, count: Option<&str>) {
    let (Some(dir), Some(count)) = (dir, count) else {
        eprintln!("usage: fx-app --gen <dir> <count>");
        std::process::exit(2);
    };
    let count: usize = count.parse().unwrap_or(100_000);

    println!("generating {count} files in {dir} ...");
    let rx = spawn_generate(dir.into(), count);
    for msg in rx {
        match msg {
            GenMsg::Progress(n) => {
                if n % 20_000 == 0 {
                    println!("  {n} ...");
                }
            }
            GenMsg::Done { count, elapsed } => {
                println!("  done: {count} files in {:.1} s", elapsed.as_secs_f32());
            }
            GenMsg::Error(e) => {
                eprintln!("  error: {e}");
                std::process::exit(1);
            }
        }
    }
}
