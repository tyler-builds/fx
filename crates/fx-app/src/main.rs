//! fx-app — the speed-spike binary.
//!
//! GUI mode (default): a minimal explorer window proving the milestone-1
//! premise — open a 100k-file directory instantly, scroll at full frame
//! rate, filter-as-you-type in single-digit milliseconds.
//!
//! Headless modes, for measuring without a window:
//!   fx-app --bench <dir>        enumerate + filter + sort timings to stdout
//!   fx-app --gen <dir> <count>  generate a synthetic directory of empty files

// NOTE: deliberately a console-subsystem binary for now — the --bench and
// --gen modes need stdout, and GUI-subsystem exes aren't awaited by shells.
// The product binary will split GUI/CLI entry points properly later.

mod app;
mod bench;
mod fonts;
mod icons;
mod telemetry;
mod theme;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--bench") => {
            bench::run_bench(args.get(2).map(String::as_str));
            return Ok(());
        }
        Some("--gen") => {
            bench::run_gen(
                args.get(2).map(String::as_str),
                args.get(3).map(String::as_str),
            );
            return Ok(());
        }
        Some("--index") => {
            bench::run_index(args.get(2).map(String::as_str));
            return Ok(());
        }
        _ => {}
    }

    let native_options = eframe::NativeOptions {
        centered: true,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 720.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("FX — speed spike"),
        ..Default::default()
    };

    eframe::run_native(
        "fx-speed-spike",
        native_options,
        Box::new(|cc| Ok(Box::new(app::SpikeApp::new(cc)))),
    )
}
