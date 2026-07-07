//! Session telemetry for the speed spike: append-only, human-readable lines
//! so a play session can be reviewed afterwards for numbers that stand out.
//!
//! Events land in `%TEMP%\fx-spike.log`, flushed per line so the file can be
//! tailed while the app runs. Deliberately std-only, like everything else.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// UI frames costing more than this get logged (rate-limited). Half a 60fps
/// frame budget: anything above it deserves a look even if nothing dropped.
const SPIKE_MS: f32 = 12.0;
const SPIKE_LOG_INTERVAL_SECS: u64 = 1;
const HEARTBEAT_SECS: u64 = 10;

pub struct Telemetry {
    out: Option<BufWriter<File>>,
    pub path: PathBuf,
    start: Instant,
    last_spike: Option<Instant>,
    last_heartbeat: Instant,
}

impl Telemetry {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join("fx-spike.log");
        let out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(BufWriter::new);
        let mut telem = Self {
            out,
            path,
            start: Instant::now(),
            last_spike: None,
            last_heartbeat: Instant::now(),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        telem.log(
            "session",
            format!(
                "start {} UTC (fx-app {})",
                fx_core::format_unix(now),
                env!("CARGO_PKG_VERSION")
            ),
        );
        telem
    }

    /// Write one event line: `[+12.345s] event | detail`.
    pub fn log(&mut self, event: &str, detail: impl AsRef<str>) {
        if let Some(out) = &mut self.out {
            let t = self.start.elapsed().as_secs_f32();
            let _ = writeln!(out, "[+{t:9.3}s] {event:<10} | {}", detail.as_ref());
            let _ = out.flush();
        }
    }

    /// Per-frame bookkeeping: spike detection + periodic heartbeat.
    pub fn frame(&mut self, update_ms: f32, visible: usize, total: usize, samples: &VecDeque<f32>) {
        if update_ms > SPIKE_MS
            && self
                .last_spike
                .is_none_or(|t| t.elapsed().as_secs() >= SPIKE_LOG_INTERVAL_SECS)
        {
            self.last_spike = Some(Instant::now());
            self.log(
                "spike",
                format!("frame {update_ms:.2} ms ({visible}/{total} entries shown)"),
            );
        }

        if self.last_heartbeat.elapsed().as_secs() >= HEARTBEAT_SECS && !samples.is_empty() {
            self.last_heartbeat = Instant::now();
            let avg: f32 = samples.iter().sum::<f32>() / samples.len() as f32;
            let worst = samples.iter().cloned().fold(0.0, f32::max);
            self.log(
                "heartbeat",
                format!(
                    "frames avg {avg:.2} ms, worst {worst:.2} ms (last {}), {visible}/{total} entries",
                    samples.len()
                ),
            );
        }
    }
}
